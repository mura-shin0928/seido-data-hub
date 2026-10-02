//! 取得の結果を1つのトランザクションで保存する（§13 §15）。
//!
//! 履歴・代表 URL と資源の状態（`lifecycle::process`）・ジョブの完了と次に取る時刻を同じトランザクションに入れる。
//! 途中で失敗したら何も残らず、その URL はまた対象になる。

use anyhow::Context as _;
use domain::extract::{self, Extracted};
use domain::liveness::Observation;
use domain::schedule::{self, Policy};
use entity::{content_versions, fetch_history, resources, urls};
use sea_orm::sea_query::{Expr, OnConflict};
use sea_orm::{
    ActiveValue::Set,
    ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, QuerySelect, TransactionSession,
    TransactionTrait,
    prelude::{DateTimeWithTimeZone, Uuid},
};

use crate::fetch::{Body, Fetch, Outcome};
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
    /// 再試行の上限・待ち時間・次に取るまでの間隔
    pub policy: &'a Policy,
}

#[derive(Debug)]
pub enum Recorded {
    Saved {
        processed: Box<Processed>,
        /// 内容を上書きしたときの、上書き前の `body_hash`（初回・上書きなしは無い）。変化の判定に使う
        previous_body_hash: Option<String>,
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
    // claim し直された URL（processing）は、同じ実行の中の再試行。前の試行の行は最新の試行で置き換える。
    // 完了済み（processing でない）の流し直しは、記録済みとして何も書かない
    let retrying = url.status == "processing";
    if !insert_history(&txn, attempt, retrying).await? {
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
    let previous_body_hash = match (processed.resource_id, processed.verdict.observation) {
        (Some(resource_id), Some(observation)) => {
            save_resource(&txn, resource_id, observation, attempt).await?
        }
        _ => None,
    };
    let pdf = is_pdf(&txn, attempt.fetch, processed.resource_id).await?;
    complete_job(
        &txn,
        &url,
        processed.verdict.job,
        processed.verdict.error_type,
        pdf,
        attempt.policy,
    )
    .await?;

    txn.commit().await?;
    Ok(Recorded::Saved {
        processed: Box::new(processed),
        previous_body_hash,
    })
}

/// 次の間隔を PDF のものにするか。本文を読んだ応答は本文の種類で決める。
/// 本文を読まない 304 は、観測した資源の保存のされ方で決める（PDF は raw_hash だけ、HTML は body_hash も持つ）
async fn is_pdf(
    txn: &impl ConnectionTrait,
    fetch: &Fetch,
    resource_id: Option<Uuid>,
) -> anyhow::Result<bool> {
    let Outcome::Response(response) = &fetch.outcome else {
        return Ok(false);
    };
    match (&response.body, response.status, resource_id) {
        (Body::Pdf(_), _, _) => Ok(true),
        (Body::NotRead, 304, Some(resource_id)) => {
            let resource = resources::Entity::find_by_id(resource_id)
                .one(txn)
                .await
                .context("資源を読めない")?;
            Ok(resource.is_some_and(|resource| {
                resource.raw_hash.is_some() && resource.body_hash.is_none()
            }))
        }
        _ => Ok(false),
    }
}

/// 履歴を1行追記する。`replace` でなく、この実行でこの URL の行が既にあれば書かずに false
async fn insert_history(
    txn: &impl ConnectionTrait,
    attempt: &Attempt<'_>,
    replace: bool,
) -> anyhow::Result<bool> {
    if replace {
        fetch_history::Entity::delete_many()
            .filter(fetch_history::Column::RunId.eq(attempt.run_id))
            .filter(fetch_history::Column::UrlId.eq(attempt.url_id))
            .exec(txn)
            .await
            .context("前の試行の履歴を置き換えられない")?;
    }
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
    let verdict = lifecycle::verdict(fetch, attempt.extracted, attempt.ctx.known_not_found_titles);
    let html = response.and_then(|response| match &response.body {
        Body::Html(html) => Some(html),
        _ => None,
    });

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
        error_type: Set(verdict.error_type.map(str::to_string)),
        error_detail: Set(error_detail),
        content_type: Set(response.and_then(|response| response.content_type.clone())),
        charset: Set(html.map(|html| html.encoding.to_string())),
        charset_source: Set(html.map(|html| html.source.as_str().to_string())),
        charset_replaced: Set(html.map(|html| html.had_errors)),
        observation: Set(verdict
            .observation
            .map(|observation| observation.as_str().to_string())),
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
        // 種類（timeout・dns など）を先頭に置き、種類ごとに数えられるようにする
        Outcome::Network { error, detail, .. } => {
            ("network", Some(format!("{}: {detail}", error.as_str())))
        }
    }
}

/// 観測を当てた資源に、取得の時刻と、読みきった生きている200の内容を書く。
/// 内容を書いたときは上書き前の `body_hash` を返す。
///
/// 内容はソフト404・304・404 などでは書かない（生きていたときの内容を、復活の比較のために残す）。
/// 応答に無い値は NULL で上書きする（消えた validator を送り続けない）。
/// `last_changed_at`・`change_count`・`next_crawl_at` は動かさない。
async fn save_resource(
    txn: &impl ConnectionTrait,
    resource_id: Uuid,
    observation: Observation,
    attempt: &Attempt<'_>,
) -> anyhow::Result<Option<String>> {
    let readable = match &attempt.fetch.outcome {
        Outcome::Response(response) if observation == Observation::Alive => response
            .raw_hash
            .as_ref()
            .map(|raw_hash| (response, raw_hash)),
        _ => None,
    };
    let Some((response, raw_hash)) = readable else {
        resources::Entity::update_many()
            .col_expr(resources::Column::LastCrawledAt, Expr::current_timestamp())
            .col_expr(resources::Column::UpdatedAt, Expr::current_timestamp())
            .filter(resources::Column::Id.eq(resource_id))
            .exec(txn)
            .await
            .context("資源の取得時刻を書けない")?;
        return Ok(None);
    };

    let before = resources::Entity::find_by_id(resource_id)
        .lock_exclusive()
        .one(txn)
        .await
        .context("資源を読めない")?
        .context("資源が無い")?;
    let extracted = attempt.extracted;
    resources::Entity::update_many()
        .col_expr(resources::Column::RawHash, Expr::value(raw_hash.clone()))
        .col_expr(resources::Column::Etag, Expr::value(response.etag.clone()))
        .col_expr(
            resources::Column::LastModified,
            Expr::value(response.last_modified.clone()),
        )
        .col_expr(
            resources::Column::PageHash,
            Expr::value(extracted.map(|e| e.hashes.page.clone())),
        )
        .col_expr(
            resources::Column::TitleHash,
            Expr::value(extracted.and_then(|e| e.hashes.title.clone())),
        )
        .col_expr(
            resources::Column::BodyHash,
            Expr::value(extracted.map(|e| e.hashes.body.clone())),
        )
        .col_expr(
            resources::Column::LinksHash,
            Expr::value(extracted.map(|e| e.hashes.links.clone())),
        )
        .col_expr(
            resources::Column::PageUpdatedOn,
            Expr::value(extracted.and_then(|e| e.page_updated_on)),
        )
        .col_expr(
            resources::Column::ExtractorVersion,
            Expr::value(extracted.map(|_| extract::EXTRACTOR_VERSION)),
        )
        .col_expr(
            resources::Column::ExtractorRule,
            Expr::value(extracted.map(|e| e.rule.as_str().to_string())),
        )
        .col_expr(
            resources::Column::RobotsMeta,
            Expr::value(extracted.and_then(|e| e.robots_meta.clone())),
        )
        .col_expr(resources::Column::LastCrawledAt, Expr::current_timestamp())
        .col_expr(resources::Column::UpdatedAt, Expr::current_timestamp())
        .filter(resources::Column::Id.eq(resource_id))
        .exec(txn)
        .await
        .context("資源の内容を書けない")?;

    if let Some(extracted) = extracted {
        content_versions::Entity::insert(content_versions::ActiveModel {
            resource_id: Set(resource_id),
            body_hash: Set(extracted.hashes.body.clone()),
            title: Set(extracted.title.clone()),
            page_updated_on: Set(extracted.page_updated_on),
            extractor_version: Set(Some(extract::EXTRACTOR_VERSION)),
            ..Default::default()
        })
        .on_conflict(
            OnConflict::columns([
                content_versions::Column::ResourceId,
                content_versions::Column::BodyHash,
            ])
            .do_nothing()
            .to_owned(),
        )
        .exec_without_returning(txn)
        .await
        .context("content_versions を書けない")?;
    }
    Ok(before.body_hash)
}

/// ジョブを完了にし、次の状態と次に取る時刻を書く。回数と時刻は取得の記録と同じトランザクションで動かす。
/// 状態・回数・間隔は `schedule::next` が決める（再試行の上限を超えたら `failed_final`）。
/// 時刻は DB の時計（`now()`）からの間隔で書く
async fn complete_job(
    txn: &impl ConnectionTrait,
    url: &urls::Model,
    job: domain::liveness::Job,
    error_type: Option<&str>,
    pdf: bool,
    policy: &Policy,
) -> anyhow::Result<()> {
    let next = schedule::next(policy, job, url.retry_count, pdf, url.id.as_u128());
    let (status, retry_count) = (next.status.as_str(), next.retry_count);
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
        .col_expr(
            urls::Column::NextCrawlAt,
            Expr::cust_with_values(
                "now() + make_interval(secs => $1)",
                [next.after.as_secs_f64()],
            ),
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
