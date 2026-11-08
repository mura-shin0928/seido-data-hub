//! 人が判定した移転先を DB に反映する（決定58・59）。
//!
//! 判定のファイル（`data/reviews/*.json`）を読み、`register` で候補の URL を `urls` に入れる。続く巡回が取って資源を作る。
//! `link` で、候補の資源が `active` なら反映する: `hit` はその制度の主たる URL を候補に差し替え、
//! `index` は一覧ページを関連リンク（`source = 'discovered'`）として加える。`none` は記録だけで何もしない。
//!
//! `rejected` は、人に見せた候補のうち選ばなかった URL。`judged` が選んだ URL と合わせて「確かめ済み」の組にし、移転先の提案から外す。
//!
//! 差し替えは制度ごとの処理で、資源の状態には触らない。1つの古いページに複数の制度が載り、移転先が制度ごとに
//! 分かれていることがあるので、元の URL を共有する制度を巻き込まない。
//!
//! ホスト移行の `hit`（`host_move`）は、旧ホストが消えて同じパスのページが別のホストにあると人が確かめたもの。
//! `register` がその移行を `host_moves` に承認として残し、`link` は承認があるときだけ、元の資源が無くても差し替える。
//! 差し替えでどの制度からも使われなくなった元の URL は `retired` にして、巡回から外す。
//!
//! 1件ずつのトランザクションで書く。`dry_run` は同じ処理を流して最後に戻す（数える処理を別に持たない）。
//! 判定の後に状況が変わった件（主たる URL が変わった・生き返った・候補が取れない）は反映せずに `Refusal` で返す。

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, bail};
use domain::urls::{PreparedUrl, prepare};
use entity::{program_urls, urls as urls_table};
use sea_orm::sea_query::OnConflict;
use sea_orm::{
    ActiveValue::Set, ConnectionTrait, DatabaseConnection, DbBackend, DbErr, EntityTrait,
    Statement, TransactionSession, TransactionTrait, prelude::Uuid,
};
use serde::Deserialize;

use crate::host_moves;
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
    /// 旧ホストが消え、同じパス・同じクエリのページが別のホストにあると人が確かめた `hit`。
    /// このときだけ候補のホストが `dead_url` と違ってよく、`register` がその移行を承認する
    #[serde(default)]
    pub host_move: bool,
    /// 人に見せた候補のうち選ばなかった URL（どの判定でも書ける）。ホスト移行の `hit` でも `dead_url` と同じホストに限る
    #[serde(default)]
    pub rejected: Vec<String>,
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
        if review.host_move && review.verdict != Verdict::Hit {
            errors.push(format!("{id}: ホスト移行は hit だけ"));
        }
        match (review.verdict, &review.url) {
            (Verdict::Hit | Verdict::Index, None) => {
                errors.push(format!("{id}: {:?} に url が無い", review.verdict))
            }
            (Verdict::None, Some(_)) => errors.push(format!("{id}: none に url がある")),
            (_, Some(url)) => match (prepare(url), prepare(&review.dead_url)) {
                (Err(e), _) => errors.push(format!("{id}: url を読めない（{e}）: {url:?}")),
                // hit でないホスト移行は上で拒んでいる
                (Ok(_), Ok(_)) if review.host_move && review.verdict != Verdict::Hit => {}
                (Ok(candidate), Ok(dead)) if review.host_move => {
                    if candidate.host_key == dead.host_key {
                        errors.push(format!("{id}: ホスト移行なのにホストが同じ"));
                    } else if !same_path_and_query(&candidate, &dead) {
                        errors.push(format!("{id}: ホスト移行なのにパスかクエリが違う: {url:?}"));
                    }
                }
                // 候補は元の URL と同じホストに限る。別のホストを入れると、そのホストが巡回の許可リストに加わる
                (Ok(candidate), Ok(dead)) if candidate.host_key != dead.host_key => {
                    errors.push(format!("{id}: url のホストが dead_url と違う: {url:?}"))
                }
                _ => {}
            },
            (Verdict::None, None) => {}
        }
        let dead = prepare(&review.dead_url);
        if let Err(e) = &dead {
            errors.push(format!("{id}: dead_url を読めない（{e}）"));
        }
        for url in &review.rejected {
            match (prepare(url), &dead) {
                (Err(e), _) => errors.push(format!("{id}: rejected を読めない（{e}）: {url:?}")),
                (Ok(candidate), Ok(dead)) if candidate.host_key != dead.host_key => errors.push(
                    format!("{id}: rejected のホストが dead_url と違う: {url:?}"),
                ),
                _ => {}
            }
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

/// `dir` 直下の `*.json` を名前の順に読む（下のディレクトリは読まない）。誤りにはファイル名を添える
pub fn load_dir(dir: &Path) -> anyhow::Result<Vec<ReviewFile>> {
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("{} を読めない", dir.display()))?
    {
        let path = entry?.path();
        if path.is_file() && path.extension().is_some_and(|ext| ext == "json") {
            paths.push(path);
        }
    }
    paths.sort();
    paths
        .iter()
        .map(|path| {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            let json =
                std::fs::read_to_string(path).with_context(|| format!("{name} を読めない"))?;
            parse(&json).with_context(|| name.to_string())
        })
        .collect()
}

/// 人が確かめ済みの（制度, `dedup_key`）の組。選んだ `url`（hit・index）と、選ばなかった `rejected` のすべて。
/// `none` は `url` を持たないので、`rejected` が無ければ何も確かめ済みにしない。
/// `parse` を通したファイルを渡す前提で、読めない URL は飛ばす
pub fn judged(files: &[ReviewFile]) -> BTreeSet<(Uuid, String)> {
    let mut set = BTreeSet::new();
    for review in files.iter().flat_map(|file| &file.reviews) {
        for url in review.url.iter().chain(&review.rejected) {
            if let Ok(prepared) = prepare(url) {
                set.insert((review.program_id, prepared.dedup_key));
            }
        }
    }
    set
}

/// ホストだけが変わったか（パスとクエリが同じ）。`domain::liveness` の移行の検出と同じ見方
fn same_path_and_query(a: &PreparedUrl, b: &PreparedUrl) -> bool {
    match (
        url::Url::parse(&a.normalized_url),
        url::Url::parse(&b.normalized_url),
    ) {
        (Ok(a), Ok(b)) => a.path() == b.path() && a.query() == b.query(),
        _ => false,
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct RegisterCounts {
    pub new_urls: usize,
    pub existing_urls: usize,
    pub none: usize,
    /// 承認したホスト移行（`host_move` の判定ごとに1）
    pub approved_moves: usize,
}

/// `hit`・`index` の候補の URL を `urls` に入れる。既にある行は触らない（巡回で進んだ状態を戻さないため）。
/// `host_move` の判定は、その移行も承認する
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
        if review.host_move {
            // 新しいホストが許可リストに加わる根拠として、人の承認を host_moves に残す（parse で同じパスと確かめてある）
            let dead = prepare(&review.dead_url)?;
            host_moves::approve_by_hand(&txn, &dead.host_key, &url.host_key, &review.dead_url)
                .await?;
            counts.approved_moves += 1;
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
    /// 主たる URL の normalized_url が dead_url と違う（制度が無いときも）
    MainChanged,
    /// 元の資源が deleted・deletion_candidate でない（状態を持つ。moved を含む。資源が無ければ "none"）
    MainNotDead(String),
    /// hit で、候補の資源が moved
    TargetMoved,
    /// hit で、元の資源と候補の資源が同じ
    SameResource,
    /// ホスト移行の判定で、その移行が host_moves で承認されていない（register を流していない）
    HostMoveNotApproved,
}

impl RefuseReason {
    pub fn label(&self) -> String {
        match self {
            Self::NoResource => "候補をまだ取っていない".to_string(),
            Self::NotActive(state) => format!("候補が active でない（{state}）"),
            Self::MainChanged => "主たる URL が判定のときと違う".to_string(),
            Self::MainNotDead(state) => format!("元の資源が死んでいない（{state}）"),
            Self::TargetMoved => "候補の資源が moved".to_string(),
            Self::SameResource => "候補の資源が元の資源と同じ".to_string(),
            Self::HostMoveNotApproved => "ホスト移行が承認されていない".to_string(),
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
    /// 主たる URL を候補に差し替えた
    pub replaced: usize,
    pub related: usize,
    /// 既に反映してあった（もう一度流したとき）
    pub already: usize,
    pub none: usize,
    /// ホスト移行で差し替え、どの制度からも使われなくなって巡回から外した元の URL
    pub retired: usize,
    pub refused: Vec<Refusal>,
    /// DB のエラー（program_id と内容）
    pub failed: Vec<(Uuid, String)>,
}

enum Linked {
    /// `retired`: 元の URL を巡回から外した（ホスト移行のときだけ）
    Replaced {
        retired: bool,
    },
    Related,
    Already {
        retired: bool,
    },
    Refused(RefuseReason),
}

/// 候補の資源が active なら反映する。1件ずつのトランザクション
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
        // 始める・確定するところの失敗も、その件の失敗として積んで続ける（それまでに反映した件の報告を残すため）
        match link_in_txn(db, review, &url).await {
            Ok(Linked::Replaced { retired }) => {
                report.replaced += 1;
                report.retired += usize::from(retired);
            }
            Ok(Linked::Related) => report.related += 1,
            Ok(Linked::Already { retired }) => {
                report.already += 1;
                report.retired += usize::from(retired);
            }
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

/// いまの資源（`observed_at` が最新の結び付き。API と `link-related` と同じ見方）の id と状態
struct Current {
    id: Uuid,
    state: String,
}

async fn current(txn: &impl ConnectionTrait, url_id: Uuid) -> Result<Option<Current>, DbErr> {
    let row = txn
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT r.id, r.state FROM url_resources ur \
             JOIN resources r ON r.id = ur.resource_id \
             WHERE ur.url_id = $1 ORDER BY ur.observed_at DESC LIMIT 1",
            [url_id.into()],
        ))
        .await?;
    row.map(|row| {
        Ok(Current {
            id: row.try_get_by_index(0)?,
            state: row.try_get_by_index(1)?,
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

    let candidate_url_id: Option<Uuid> = txn
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT id FROM urls WHERE dedup_key = $1",
            [url.dedup_key.clone().into()],
        ))
        .await?
        .map(|row| row.try_get_by_index(0))
        .transpose()?;

    let main = txn
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT u.id, u.normalized_url, pu.source FROM program_urls pu JOIN urls u ON u.id = pu.url_id \
             WHERE pu.program_id = $1 AND pu.role = 'main'",
            [review.program_id.into()],
        ))
        .await?;
    let Some(main) = main else {
        return refused(RefuseReason::MainChanged);
    };
    let main_url_id: Uuid = main.try_get_by_index(0)?;
    if review.host_move {
        return link_host_move(
            txn,
            review,
            url,
            candidate_url_id,
            MainUrl {
                id: main_url_id,
                normalized_url: main.try_get_by_index(1)?,
                source: main.try_get_by_index(2)?,
            },
        )
        .await;
    }
    // 候補が元の URL そのものの判定は、差し替え済みではない（下の SameResource で止まる）
    if review.verdict == Verdict::Hit
        && candidate_url_id == Some(main_url_id)
        && main.try_get_by_index::<String>(2)? == "discovered"
    {
        return Ok(Linked::Already { retired: false });
    }
    if main.try_get_by_index::<String>(1)? != prepare(&review.dead_url)?.normalized_url {
        return refused(RefuseReason::MainChanged);
    }

    let Some(old) = current(txn, main_url_id).await? else {
        return refused(RefuseReason::MainNotDead("none".to_string()));
    };
    let Some(candidate_url_id) = candidate_url_id else {
        return refused(RefuseReason::NoResource);
    };
    let Some(target) = current(txn, candidate_url_id).await? else {
        return refused(RefuseReason::NoResource);
    };
    if review.verdict == Verdict::Hit {
        if target.id == old.id {
            return refused(RefuseReason::SameResource);
        }
        if target.state == "moved" {
            return refused(RefuseReason::TargetMoved);
        }
    }
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
    if review.verdict == Verdict::Index && exists {
        return Ok(Linked::Already { retired: false });
    }
    if !matches!(old.state.as_str(), "deleted" | "deletion_candidate") {
        return refused(RefuseReason::MainNotDead(old.state));
    }

    match review.verdict {
        Verdict::Hit => {
            // 候補がその制度の関連 URL にもあれば、主キーが重なるので関連の行を消す
            if exists {
                txn.execute_raw(Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    "DELETE FROM program_urls WHERE program_id = $1 AND url_id = $2",
                    [review.program_id.into(), candidate_url_id.into()],
                ))
                .await
                .context("候補の関連リンクを消せない")?;
            }
            // 読んでから書くまでに巡回が生き返らせていたら書かない（同じ実行の外で流れた巡回との競合）
            let updated = txn
                .execute_raw(Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    "UPDATE program_urls SET url_id = $1, source = 'discovered' \
                     WHERE program_id = $2 AND role = 'main' AND url_id = $3 \
                     AND EXISTS (SELECT 1 FROM resources \
                                 WHERE id = $4 AND state IN ('deleted', 'deletion_candidate'))",
                    [
                        candidate_url_id.into(),
                        review.program_id.into(),
                        main_url_id.into(),
                        old.id.into(),
                    ],
                ))
                .await
                .context("主たる URL を差し替えられない")?;
            if updated.rows_affected() == 0 {
                bail!("主たる URL を差し替える直前に元の資源の状態が変わった");
            }
            Ok(Linked::Replaced { retired: false })
        }
        Verdict::Index => {
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

/// 制度のいまの主たる URL
struct MainUrl {
    id: Uuid,
    normalized_url: String,
    source: String,
}

/// ホスト移行の `hit`。承認済みの移行に限り、別のホストの候補で差し替える。
/// 元の資源が無くても（旧ホストが消えてから一度も取れていない）差し替え、使われなくなった元の URL を巡回から外す
async fn link_host_move(
    txn: &impl ConnectionTrait,
    review: &Review,
    url: &PreparedUrl,
    candidate_url_id: Option<Uuid>,
    main: MainUrl,
) -> anyhow::Result<Linked> {
    let refused = |reason| Ok(Linked::Refused(reason));
    let dead = prepare(&review.dead_url)?;

    let approved = txn
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT 1 FROM host_moves WHERE from_host_key = $1 AND to_host_key = $2 AND status = 'approved'",
            [dead.host_key.clone().into(), url.host_key.clone().into()],
        ))
        .await?
        .is_some();
    if !approved {
        return refused(RefuseReason::HostMoveNotApproved);
    }

    // もう一度流したとき: 差し替えは済んでいる。元の URL を外しそびれていれば外す
    if candidate_url_id == Some(main.id) && main.source == "discovered" {
        let old_url_id: Option<Uuid> = txn
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT id FROM urls WHERE dedup_key = $1",
                [dead.dedup_key.clone().into()],
            ))
            .await?
            .map(|row| row.try_get_by_index(0))
            .transpose()?;
        let retired = match old_url_id {
            Some(id) => retire_if_unused(txn, id).await?,
            None => false,
        };
        return Ok(Linked::Already { retired });
    }
    if main.normalized_url != dead.normalized_url {
        return refused(RefuseReason::MainChanged);
    }

    let Some(candidate_url_id) = candidate_url_id else {
        return refused(RefuseReason::NoResource);
    };
    let Some(target) = current(txn, candidate_url_id).await? else {
        return refused(RefuseReason::NoResource);
    };
    if target.state == "moved" {
        return refused(RefuseReason::TargetMoved);
    }
    if target.state != "active" {
        return refused(RefuseReason::NotActive(target.state));
    }
    let old = current(txn, main.id).await?;
    if let Some(old) = &old {
        if old.id == target.id {
            return refused(RefuseReason::SameResource);
        }
        if !matches!(old.state.as_str(), "deleted" | "deletion_candidate") {
            return refused(RefuseReason::MainNotDead(old.state.clone()));
        }
    }

    // 候補がその制度の関連 URL にもあれば、主キーが重なるので関連の行を消す
    txn.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "DELETE FROM program_urls WHERE program_id = $1 AND url_id = $2 AND role = 'related'",
        [review.program_id.into(), candidate_url_id.into()],
    ))
    .await
    .context("候補の関連リンクを消せない")?;
    // 読んでから書くまでに元の URL の資源が変わっていたら書かない（無いまま、または同じ資源が死んだまま）
    let guard = match &old {
        None => "NOT EXISTS (SELECT 1 FROM url_resources WHERE url_id = $3)",
        Some(_) => {
            "EXISTS (SELECT 1 FROM resources WHERE id = $4 AND state IN ('deleted', 'deletion_candidate'))"
        }
    };
    let old_id = old.as_ref().map(|old| old.id);
    let updated = txn
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            format!(
                "UPDATE program_urls SET url_id = $1, source = 'discovered' \
                 WHERE program_id = $2 AND role = 'main' AND url_id = $3 AND {guard}"
            ),
            [
                candidate_url_id.into(),
                review.program_id.into(),
                main.id.into(),
                old_id.into(),
            ],
        ))
        .await
        .context("主たる URL を差し替えられない")?;
    if updated.rows_affected() == 0 {
        bail!("主たる URL を差し替える直前に元の資源の状態が変わった");
    }
    let retired = retire_if_unused(txn, main.id).await?;
    Ok(Linked::Replaced { retired })
}

/// どの制度にも使われていない URL を巡回から外す（`retired`）。外したら true。
/// 巡回が取っている最中（`processing`）なら外さず、次に流したときに外す
async fn retire_if_unused(txn: &impl ConnectionTrait, url_id: Uuid) -> anyhow::Result<bool> {
    let result = txn
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE urls SET status = 'retired', worker_id = NULL, lease_until = NULL, updated_at = now() \
             WHERE id = $1 AND status NOT IN ('processing', 'retired') \
               AND NOT EXISTS (SELECT 1 FROM program_urls WHERE url_id = $1)",
            [url_id.into()],
        ))
        .await
        .context("元の URL を巡回から外せない")?;
    Ok(result.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOVES: &str = include_str!("../../../data/reviews/2026-10-moves.json");
    const ID1: &str = "00000000-0000-0000-0000-000000000001";
    const ID2: &str = "00000000-0000-0000-0000-000000000002";

    fn file(reviews: serde_json::Value) -> String {
        serde_json::json!({ "reviewed_at": "2026-11-01", "reviews": reviews }).to_string()
    }

    #[test]
    fn rejected_is_optional() {
        let file = parse(MOVES).unwrap();
        assert!(!file.reviews.is_empty());
        assert!(file.reviews.iter().all(|r| r.rejected.is_empty()));
    }

    #[test]
    fn rejected_is_validated() {
        let json = file(serde_json::json!([{
            "program_id": ID1, "dead_url": "https://a.example.jp/old.html",
            "verdict": "none",
            "rejected": ["not a url", "https://b.example.jp/x.html"]
        }]));
        let err = parse(&json).unwrap_err().to_string();
        assert!(err.contains("誤りが 2 件"), "{err}");
        assert!(
            err.contains(&format!("{ID1}: rejected を読めない（")),
            "{err}"
        );
        assert!(err.contains("\"not a url\""), "{err}");
        assert!(
            err.contains(&format!(
                "{ID1}: rejected のホストが dead_url と違う: \"https://b.example.jp/x.html\""
            )),
            "{err}"
        );
    }

    #[test]
    fn host_move_rejected_must_match_dead_host() {
        let json = file(serde_json::json!([{
            "program_id": ID1, "dead_url": "https://a.example.jp/old.html",
            "verdict": "hit", "host_move": true, "url": "https://b.example.jp/old.html",
            "rejected": ["https://b.example.jp/other.html"]
        }]));
        let err = parse(&json).unwrap_err().to_string();
        assert!(err.contains("rejected のホストが dead_url と違う"), "{err}");
    }

    fn judged_sample() -> Vec<ReviewFile> {
        let json = file(serde_json::json!([
            {
                "program_id": ID1, "dead_url": "https://a.example.jp/old.html",
                "verdict": "hit", "url": "https://a.example.jp/new.html",
                "rejected": ["https://a.example.jp/x.html", "https://a.example.jp/y.html"]
            },
            {
                "program_id": ID2, "dead_url": "https://a.example.jp/old2.html",
                "verdict": "none", "rejected": ["https://a.example.jp/z.html"]
            }
        ]));
        vec![parse(&json).unwrap()]
    }

    #[test]
    fn judged_collects_url_and_rejected() {
        let set = judged(&judged_sample());
        let id1: Uuid = ID1.parse().unwrap();
        let id2: Uuid = ID2.parse().unwrap();
        let expected: BTreeSet<(Uuid, String)> = [
            (id1, "https://a.example.jp/new.html"),
            (id1, "https://a.example.jp/x.html"),
            (id1, "https://a.example.jp/y.html"),
            (id2, "https://a.example.jp/z.html"),
        ]
        .into_iter()
        .map(|(id, url)| (id, prepare(url).unwrap().dedup_key))
        .collect();
        assert_eq!(set, expected);
        assert_eq!(set.len(), 4);
    }

    #[test]
    fn judged_uses_dedup_key() {
        let json = file(serde_json::json!([
            {
                "program_id": ID1, "dead_url": "https://a.example.jp/old.html",
                "verdict": "none", "rejected": ["https://a.example.jp/a.html#x"]
            },
            {
                "program_id": ID2, "dead_url": "https://www.city.koganei.lg.jp/old.html",
                "verdict": "none", "rejected": ["https://www.city.koganei.lg.jp/smph/a.html"]
            }
        ]));
        let set = judged(&[parse(&json).unwrap()]);
        let plain = prepare("https://a.example.jp/a.html").unwrap().dedup_key;
        assert!(set.contains(&(ID1.parse().unwrap(), plain)));
        // /smph/ はサイトの規則で畳まれ、PC 版の URL と同じ組になる
        let pc = prepare("https://www.city.koganei.lg.jp/a.html")
            .unwrap()
            .dedup_key;
        assert!(set.contains(&(ID2.parse().unwrap(), pc)));
    }

    #[test]
    fn none_without_rejected_judges_nothing() {
        let file = parse(MOVES).unwrap();
        let none: Vec<&Review> = file
            .reviews
            .iter()
            .filter(|r| r.verdict == Verdict::None)
            .collect();
        assert!(!none.is_empty());
        let set = judged(std::slice::from_ref(&file));
        for review in none {
            assert!(set.iter().all(|(id, _)| *id != review.program_id));
        }
    }

    #[test]
    fn load_dir_reads_all_json() {
        let dir = std::env::temp_dir().join(format!("reviews-load-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = file(serde_json::json!([]));
        let b = serde_json::json!({ "reviewed_at": "2026-11-02", "reviews": [] }).to_string();
        std::fs::write(dir.join("b.json"), b).unwrap();
        std::fs::write(dir.join("a.json"), a).unwrap();
        std::fs::write(dir.join("notes.txt"), "json ではない").unwrap();
        let files = load_dir(&dir).unwrap();
        assert_eq!(
            files
                .iter()
                .map(|f| f.reviewed_at.as_str())
                .collect::<Vec<_>>(),
            ["2026-11-01", "2026-11-02"]
        );
        // 誤りにはファイル名を添える
        std::fs::write(dir.join("c.json"), "{").unwrap();
        let err = format!("{:#}", load_dir(&dir).unwrap_err());
        assert!(err.contains("c.json"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_dir_reads_repo_reviews() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/reviews");
        let json_count = std::fs::read_dir(&dir)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|ext| ext == "json")
            })
            .count();
        let files = load_dir(&dir).unwrap();
        // 判定のファイルは増えていくので、総数は決め打ちせず json の数と合わせる
        assert_eq!(files.len(), json_count);
        assert!(files.iter().any(|f| f.reviews.len() == 290));
        assert!(files.iter().any(|f| f.reviews.iter().any(|r| r.host_move)));
    }
}
