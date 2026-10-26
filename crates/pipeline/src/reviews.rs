//! 人が判定した移転先を結ぶ（決定58）。
//!
//! 判定のファイル（`data/reviews/*.json`）を読み、`register` で候補の URL を `urls` に入れる。続く巡回が取って資源を作る。
//! `link` で、候補の資源が `active` なら結ぶ: `hit` は元の資源を `moved` にして移転先に向け、
//! `index` は一覧ページを関連リンク（`source = 'discovered'`）として結ぶ。`none` は記録だけで何もしない。
//!
//! 1件ずつのトランザクションで書く。`dry_run` は同じ処理を流してロールバックする（数える処理を別に持たない）。
//! 判定の後に状況が変わった件（主たる URL が変わった・生き返った・候補が取れない）は結ばずに `Refusal` で返す。

use std::collections::BTreeSet;

use anyhow::{Context, bail};
use domain::urls::{PreparedUrl, prepare};
use entity::{program_urls, urls as urls_table};
use sea_orm::sea_query::OnConflict;
use sea_orm::{
    ActiveValue::Set, ConnectionTrait, DatabaseConnection, DbBackend, DbErr, EntityTrait,
    Statement, TransactionSession, TransactionTrait, prelude::Uuid,
};
use serde::Deserialize;

use crate::link_related::RELATED_PRIORITY;

#[derive(Debug, Deserialize)]
pub struct ReviewFile {
    pub reviewed_at: String,
    pub reviews: Vec<Review>,
}

#[derive(Debug, Deserialize)]
pub struct Review {
    pub program_id: Uuid,
    /// 判定したときの主たる URL（`normalized_url`）
    pub dead_url: String,
    pub verdict: Verdict,
    /// `hit`・`index` で選んだ候補の URL
    pub url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// 同じ制度の今のページ
    Hit,
    /// 制度へのリンクがある一覧ページ
    Index,
    /// どの候補も違う
    None,
}

/// 全件を確かめてから返す。誤りは全部まとめて1つのエラーにする（行番号ではなく program_id で）
pub fn parse(json: &str) -> anyhow::Result<ReviewFile> {
    let file: ReviewFile = serde_json::from_str(json).context("判定のファイルを読めない")?;
    let mut errors = Vec::new();
    let mut seen = BTreeSet::new();
    for review in &file.reviews {
        let id = review.program_id;
        if !seen.insert(id) {
            errors.push(format!("{id}: 同じ制度が2回ある"));
        }
        match (review.verdict, &review.url) {
            (Verdict::Hit | Verdict::Index, None) => {
                errors.push(format!("{id}: {:?} に url が無い", review.verdict))
            }
            (Verdict::None, Some(_)) => errors.push(format!("{id}: none に url がある")),
            (_, Some(url)) => match (prepare(url), prepare(&review.dead_url)) {
                (Err(e), _) => errors.push(format!("{id}: url を読めない（{e}）: {url:?}")),
                // 候補は元の URL と同じホストに限る。別のホストを入れると、そのホストが巡回の許可リストに加わる
                (Ok(candidate), Ok(dead)) if candidate.host_key != dead.host_key => {
                    errors.push(format!("{id}: url のホストが dead_url と違う: {url:?}"))
                }
                _ => {}
            },
            (Verdict::None, None) => {}
        }
        if let Err(e) = prepare(&review.dead_url) {
            errors.push(format!("{id}: dead_url を読めない（{e}）"));
        }
    }
    if !errors.is_empty() {
        bail!(
            "判定のファイルに誤りが {} 件ある:\n{}",
            errors.len(),
            errors.join("\n")
        );
    }
    Ok(file)
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct RegisterCounts {
    pub new_urls: usize,
    pub existing_urls: usize,
    pub none: usize,
}

/// `hit`・`index` の候補の URL を `urls` に入れる。既にある行は触らない（巡回で進んだ状態を戻さないため）
pub async fn register(
    db: &DatabaseConnection,
    file: &ReviewFile,
    dry_run: bool,
) -> anyhow::Result<RegisterCounts> {
    let mut counts = RegisterCounts::default();
    let txn = db.begin().await?;
    for review in &file.reviews {
        let Some(url) = candidate_of(review)? else {
            counts.none += 1;
            continue;
        };
        let mut model = urls_table::ActiveModel {
            raw_url: Set(url.raw_url.clone()),
            normalized_url: Set(url.normalized_url.clone()),
            dedup_key: Set(url.dedup_key.clone()),
            host_key: Set(url.host_key.clone()),
            ..Default::default()
        };
        // hit は主たる URL と同じ扱い（DB の既定値 80）、index は関連 URL と同じ扱い
        if review.verdict == Verdict::Index {
            model.priority = Set(RELATED_PRIORITY);
        }
        let inserted = urls_table::Entity::insert(model)
            .on_conflict(
                OnConflict::column(urls_table::Column::DedupKey)
                    .do_nothing()
                    .to_owned(),
            )
            .exec_without_returning(&txn)
            .await
            .context("urls を書けない")?;
        if inserted == 0 {
            counts.existing_urls += 1;
        } else {
            counts.new_urls += 1;
        }
    }
    if dry_run {
        txn.rollback().await?;
    } else {
        txn.commit().await?;
    }
    Ok(counts)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefuseReason {
    /// 候補の URL が urls に無い、または資源が無い（まだ取っていない）
    NoResource,
    /// 候補の資源が active でない（状態を持つ）
    NotActive(String),
    /// 主たる URL の normalized_url が dead_url と違う
    MainChanged,
    /// 元の資源が deleted・deletion_candidate でない（状態を持つ。moved を含む。資源が無ければ "none"）
    MainNotDead(String),
    /// hit で、候補の資源が moved
    TargetMoved,
    /// hit で、候補の資源の moved_to_resource_id が元の資源
    Cycle,
    /// hit で、元の資源と候補の資源が同じ
    SameResource,
}

impl RefuseReason {
    pub fn label(&self) -> String {
        match self {
            Self::NoResource => "候補をまだ取っていない".to_string(),
            Self::NotActive(state) => format!("候補が active でない（{state}）"),
            Self::MainChanged => "主たる URL が判定のときと違う".to_string(),
            Self::MainNotDead(state) => format!("元の資源が死んでいない（{state}）"),
            Self::TargetMoved => "候補の資源が moved".to_string(),
            Self::Cycle => "候補の資源が元の資源へ移っている（循環）".to_string(),
            Self::SameResource => "候補の資源が元の資源と同じ".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub program_id: Uuid,
    pub url: String,
    pub reason: RefuseReason,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct LinkReport {
    pub moved: usize,
    pub related: usize,
    /// 既に結んであった（もう一度流したとき）
    pub already: usize,
    pub none: usize,
    pub refused: Vec<Refusal>,
    /// DB のエラー（program_id と内容）
    pub failed: Vec<(Uuid, String)>,
}

enum Linked {
    Moved,
    Related,
    Already,
    Refused(RefuseReason),
}

/// 候補の資源が active なら結ぶ。1件ずつのトランザクション
pub async fn link(
    db: &DatabaseConnection,
    file: &ReviewFile,
    dry_run: bool,
) -> anyhow::Result<LinkReport> {
    let mut report = LinkReport::default();
    if dry_run {
        // 試し流しは全体を1つのトランザクションにし、1件ごとはセーブポイントにする。
        // 前の判定の結果を見て次を決めるので、本番と同じ件数になる。最後にすべて戻す
        let outer = db.begin().await?;
        link_all(&outer, file, &mut report).await?;
        outer.rollback().await?;
    } else {
        link_all(db, file, &mut report).await?;
    }
    Ok(report)
}

async fn link_all<C: TransactionTrait>(
    db: &C,
    file: &ReviewFile,
    report: &mut LinkReport,
) -> anyhow::Result<()> {
    for review in &file.reviews {
        let Some(url) = candidate_of(review)? else {
            report.none += 1;
            continue;
        };
        // 始める・確定するところの失敗も、その件の失敗として積んで続ける（それまでに結んだ件の報告を残すため）
        match link_in_txn(db, review, &url).await {
            Ok(Linked::Moved) => report.moved += 1,
            Ok(Linked::Related) => report.related += 1,
            Ok(Linked::Already) => report.already += 1,
            Ok(Linked::Refused(reason)) => report.refused.push(Refusal {
                program_id: review.program_id,
                url: url.normalized_url.clone(),
                reason,
            }),
            Err(e) => report.failed.push((review.program_id, format!("{e:#}"))),
        }
    }
    Ok(())
}

async fn link_in_txn<C: TransactionTrait>(
    db: &C,
    review: &Review,
    url: &PreparedUrl,
) -> anyhow::Result<Linked> {
    let txn = db.begin().await?;
    match link_one(&txn, review, url).await {
        Ok(linked) => {
            txn.commit().await?;
            Ok(linked)
        }
        Err(e) => {
            // 戻せなかったときも、元の失敗の方を報告する（トランザクションは捨てれば戻る）
            let _ = txn.rollback().await;
            Err(e)
        }
    }
}

fn candidate_of(review: &Review) -> anyhow::Result<Option<PreparedUrl>> {
    match (review.verdict, &review.url) {
        (Verdict::None, _) => Ok(None),
        (_, Some(url)) => Ok(Some(prepare(url)?)),
        (_, None) => bail!("{}: url が無い", review.program_id),
    }
}

/// いまの資源（`observed_at` が最新の結び付き。API と `link-related` と同じ見方）の id・状態・移り先
struct Current {
    id: Uuid,
    state: String,
    moved_to: Option<Uuid>,
}

async fn current(txn: &impl ConnectionTrait, url_id: Uuid) -> Result<Option<Current>, DbErr> {
    let row = txn
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT r.id, r.state, r.moved_to_resource_id FROM url_resources ur \
             JOIN resources r ON r.id = ur.resource_id \
             WHERE ur.url_id = $1 ORDER BY ur.observed_at DESC LIMIT 1",
            [url_id.into()],
        ))
        .await?;
    row.map(|row| {
        Ok(Current {
            id: row.try_get_by_index(0)?,
            state: row.try_get_by_index(1)?,
            moved_to: row.try_get_by_index(2)?,
        })
    })
    .transpose()
}

async fn link_one(
    txn: &impl ConnectionTrait,
    review: &Review,
    url: &PreparedUrl,
) -> anyhow::Result<Linked> {
    let refused = |reason| Ok(Linked::Refused(reason));

    let main = txn
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT u.id, u.normalized_url FROM program_urls pu JOIN urls u ON u.id = pu.url_id \
             WHERE pu.program_id = $1 AND pu.role = 'main'",
            [review.program_id.into()],
        ))
        .await?;
    let dead_url = prepare(&review.dead_url)?.normalized_url;
    let main_url_id: Uuid = match main {
        Some(row) if row.try_get_by_index::<String>(1)? == dead_url => row.try_get_by_index(0)?,
        _ => return refused(RefuseReason::MainChanged),
    };
    let Some(main) = current(txn, main_url_id).await? else {
        return refused(RefuseReason::MainNotDead("none".to_string()));
    };

    let candidate_url_id: Option<Uuid> = txn
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT id FROM urls WHERE dedup_key = $1",
            [url.dedup_key.clone().into()],
        ))
        .await?
        .map(|row| row.try_get_by_index(0))
        .transpose()?;
    let Some(candidate_url_id) = candidate_url_id else {
        return refused(RefuseReason::NoResource);
    };
    let Some(target) = current(txn, candidate_url_id).await? else {
        return refused(RefuseReason::NoResource);
    };
    let main_is_dead = matches!(main.state.as_str(), "deleted" | "deletion_candidate");

    match review.verdict {
        Verdict::Hit => {
            // 巡回が転送で moved にするときの規則（lifecycle の Refused）に合わせる
            if target.id == main.id {
                return refused(RefuseReason::SameResource);
            }
            if target.state == "moved" {
                return refused(RefuseReason::TargetMoved);
            }
            if target.state != "active" {
                return refused(RefuseReason::NotActive(target.state));
            }
            if main.state == "moved" && main.moved_to == Some(target.id) {
                return Ok(Linked::Already);
            }
            if !main_is_dead {
                return refused(RefuseReason::MainNotDead(main.state));
            }
            if target.moved_to == Some(main.id) {
                return refused(RefuseReason::Cycle);
            }
            // 読んでから書くまでに巡回が生き返らせていたら書かない（同じ実行の外で流れた巡回との競合）
            let updated = txn
                .execute_raw(Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    "UPDATE resources SET state = 'moved', moved_to_resource_id = $1, updated_at = now() \
                     WHERE id = $2 AND state IN ('deleted', 'deletion_candidate')",
                    [target.id.into(), main.id.into()],
                ))
                .await
                .context("資源を moved にできない")?;
            if updated.rows_affected() == 0 {
                return refused(RefuseReason::MainNotDead("changed".to_string()));
            }
            Ok(Linked::Moved)
        }
        Verdict::Index => {
            if target.state != "active" {
                return refused(RefuseReason::NotActive(target.state));
            }
            let exists = txn
                .query_one_raw(Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    "SELECT 1 FROM program_urls WHERE program_id = $1 AND url_id = $2",
                    [review.program_id.into(), candidate_url_id.into()],
                ))
                .await?
                .is_some();
            if exists {
                return Ok(Linked::Already);
            }
            if !main_is_dead {
                return refused(RefuseReason::MainNotDead(main.state));
            }
            let rank: i32 = txn
                .query_one_raw(Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    "SELECT COALESCE(MAX(rank), 0) + 1 FROM program_urls \
                     WHERE program_id = $1 AND role = 'related'",
                    [review.program_id.into()],
                ))
                .await?
                .context("rank を読めない")?
                .try_get_by_index(0)?;
            program_urls::Entity::insert(program_urls::ActiveModel {
                program_id: Set(review.program_id),
                url_id: Set(candidate_url_id),
                role: Set("related".to_string()),
                rank: Set(rank),
                source: Set("discovered".to_string()),
            })
            .exec_without_returning(txn)
            .await
            .context("program_urls を書けない")?;
            Ok(Linked::Related)
        }
        Verdict::None => unreachable!("none は candidate_of で除いている"),
    }
}
