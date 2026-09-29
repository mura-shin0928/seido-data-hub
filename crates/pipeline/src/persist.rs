//! 取得の結果を1つのトランザクションで保存する（§13 §15）。
//!
//! 履歴・代表 URL と資源の状態（`lifecycle::process`）・ジョブの完了を同じトランザクションに入れる。
//! 途中で失敗したら何も残らず、その URL はまた対象になる。

use anyhow::Context as _;
use domain::extract::{self, Extracted};
use domain::liveness::Job;
use entity::{fetch_history, urls};
use sea_orm::sea_query::{Expr, OnConflict};
use sea_orm::{
    ActiveValue::Set,
    ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, QuerySelect, TransactionSession,
    TransactionTrait,
    prelude::{DateTimeWithTimeZone, Uuid},
};

use crate::fetch::{Fetch, Outcome};
use crate::lifecycle::{self, Context, Processed};

/// 1回の取得の結果と、それを記録する実行・URL・claim
pub struct Attempt<'a> {
    pub run_id: Uuid,
    pub url_id: Uuid,
    /// claim したときの値。完了更新で照合する（古い worker の結果を書かせない）
    pub claim_token: i64,
    pub fetch: &'a Fetch,
    pub extracted: Option<&'a Extracted>,
    pub ctx: &'a Context<'a>,
}

#[derive(Debug)]
pub enum Recorded {
    Saved {
        processed: Processed,
    },
    /// この実行でこの URL は記録済み。何も書いていない（同じ実行の流し直し）
    AlreadyRecorded,
    /// `claim_token` が合わない。何も書いていない
    StaleClaim,
}

/// 取得の結果を記録する。呼び出し側のトランザクションを渡せば、その中で書く（中ではセーブポイントを使う）
pub async fn record<C: ConnectionTrait + TransactionTrait>(
    db: &C,
    attempt: &Attempt<'_>,
) -> anyhow::Result<Recorded> {
    let txn = db.begin().await?;

    let url = urls::Entity::find_by_id(attempt.url_id)
        .lock_exclusive()
        .one(&txn)
        .await
        .context("URL を読めない")?
        .context("URL が無い")?;
    if url.claim_token != attempt.claim_token {
        return Ok(Recorded::StaleClaim);
    }
    if !insert_history(&txn, attempt).await? {
        return Ok(Recorded::AlreadyRecorded);
    }

    let processed = lifecycle::process(
        &txn,
        attempt.url_id,
        attempt.fetch,
        attempt.extracted,
        attempt.ctx,
    )
    .await?;
    complete_job(
        &txn,
        &url,
        processed.verdict.job,
        processed.verdict.error_type,
    )
    .await?;

    txn.commit().await?;
    Ok(Recorded::Saved { processed })
}

/// 履歴を1行追記する。この実行でこの URL の行が既にあれば書かずに false
async fn insert_history(txn: &impl ConnectionTrait, attempt: &Attempt<'_>) -> anyhow::Result<bool> {
    let fetch = attempt.fetch;
    let response = match &fetch.outcome {
        Outcome::Response(response) => Some(response),
        _ => None,
    };
    let http_status = response
        .map(|response| response.status)
        .or_else(|| fetch.hops.last().map(|hop| hop.status))
        .map(i32::from);
    let elapsed: u128 = fetch.hops.iter().map(|hop| hop.elapsed.as_millis()).sum();
    let (outcome, error_detail) = describe(&fetch.outcome);
    let error_type =
        lifecycle::verdict(fetch, attempt.extracted, attempt.ctx.known_not_found_titles).error_type;

    let inserted = fetch_history::Entity::insert(fetch_history::ActiveModel {
        run_id: Set(attempt.run_id),
        url_id: Set(attempt.url_id),
        http_status: Set(http_status),
        response_time_ms: Set(i64::try_from(elapsed).unwrap_or(i64::MAX)),
        bytes: Set(response.map_or(0, |response| {
            i64::try_from(response.bytes).unwrap_or(i64::MAX)
        })),
        outcome: Set(outcome.to_string()),
        etag: Set(response.and_then(|response| response.etag.clone())),
        last_modified: Set(response.and_then(|response| response.last_modified.clone())),
        raw_hash: Set(response.and_then(|response| response.raw_hash.clone())),
        body_hash: Set(attempt
            .extracted
            .map(|extracted| extracted.hashes.body.clone())),
        extractor_version: Set(attempt.extracted.map(|_| extract::EXTRACTOR_VERSION)),
        declared_canonical_url: Set(attempt
            .extracted
            .and_then(|extracted| extracted.declared_canonical_url.clone())),
        error_type: Set(error_type.map(str::to_string)),
        error_detail: Set(error_detail),
        ..Default::default()
    })
    .on_conflict(
        OnConflict::columns([fetch_history::Column::RunId, fetch_history::Column::UrlId])
            .do_nothing()
            .to_owned(),
    )
    .exec_without_returning(txn)
    .await
    .context("fetch_history を書けない")?;
    Ok(inserted == 1)
}

/// `fetch_history.outcome` に入れる種類と、応答が無いときの詳細
fn describe(outcome: &Outcome) -> (&'static str, Option<String>) {
    match outcome {
        Outcome::Response(_) => ("response", None),
        Outcome::RobotsDenied { url } => ("robots_denied", Some(url.clone())),
        Outcome::RobotsUnavailable { reason, .. } => ("robots_unavailable", Some(reason.clone())),
        Outcome::OutOfScope { location, reason } => {
            ("out_of_scope", Some(format!("{location}（{reason:?}）")))
        }
        Outcome::MissingLocation => ("redirect_anomaly", None),
        Outcome::TooManyRedirects { location } | Outcome::RedirectLoop { location } => {
            ("redirect_anomaly", Some(location.clone()))
        }
        Outcome::Network { detail, .. } => ("network", Some(detail.clone())),
    }
}

/// ジョブを完了にする。回数は取得の記録と同じトランザクションで動かす。
/// `failed_final` への移行と次の時刻は書かない（再試行の回数を見て決めるのはスケジューラ）
async fn complete_job(
    txn: &impl ConnectionTrait,
    url: &urls::Model,
    job: Job,
    error_type: Option<&str>,
) -> anyhow::Result<()> {
    let (status, retry_count) = match job {
        Job::Succeeded => ("succeeded", 0),
        Job::Blocked => ("blocked", url.retry_count),
        Job::Retry => ("retry_wait", url.retry_count + 1),
    };
    urls::Entity::update_many()
        .col_expr(urls::Column::Status, Expr::value(status))
        .col_expr(
            urls::Column::AttemptCount,
            Expr::value(url.attempt_count + 1),
        )
        .col_expr(urls::Column::RetryCount, Expr::value(retry_count))
        .col_expr(
            urls::Column::LastErrorType,
            Expr::value(error_type.map(str::to_string)),
        )
        .col_expr(urls::Column::LastCrawledAt, Expr::current_timestamp())
        .col_expr(urls::Column::WorkerId, Expr::value(Option::<String>::None))
        .col_expr(
            urls::Column::LeaseUntil,
            Expr::value(Option::<DateTimeWithTimeZone>::None),
        )
        .col_expr(urls::Column::UpdatedAt, Expr::current_timestamp())
        .filter(urls::Column::Id.eq(url.id))
        .filter(urls::Column::ClaimToken.eq(url.claim_token))
        .exec(txn)
        .await
        .context("ジョブを完了にできない")?;
    Ok(())
}
