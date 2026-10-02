//! 巡回の実行: 時刻の来た URL を claim し、取得・抽出・記録を同時に回す（§11 §13 §14）。
//! 取得に渡すもの（許可リスト・validator・ホストの canonical の信用・本文の取り出し）もここで DB から組み立てる。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use domain::canonical::{self, Declaration};
use domain::extract::{self, Extracted};
use domain::schedule::Policy;
use domain::urls;
use entity::{resources, urls as urls_table};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseConnection, DbBackend, EntityTrait, FromQueryResult,
    QueryFilter, QuerySelect, Statement, prelude::Uuid,
};
use tokio::task::JoinSet;
use tokio::time::{Instant, MissedTickBehavior};

use crate::claim::{self, Claim, Exclude};
use crate::fetch::{Body, Fetch, Fetcher, Outcome, Validators};
use crate::host_moves;
use crate::lifecycle::Context;
use crate::persist::{self, Attempt, Recorded};

/// 実行の設定
#[derive(Debug, Clone)]
pub struct Config {
    /// lease を持つ行がどのプロセスのものか（`urls.worker_id`）
    pub worker_id: String,
    /// 同時に取得する件数の上限（ホストごとは1件）
    pub concurrency: usize,
    /// claim した行の lease の長さ
    pub lease: Duration,
    /// 取得中の行の lease を延ばす間隔
    pub heartbeat: Duration,
    /// 再試行と次の時刻
    pub policy: Policy,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            worker_id: default_worker_id(),
            concurrency: 16,
            lease: Duration::from_secs(600),
            heartbeat: Duration::from_secs(120),
            policy: Policy::default(),
        }
    }
}

/// `{ホスト名}-{pid}`。ホスト名が取れなければ `worker-{pid}`。
/// 依存を足さないので、ホスト名は環境変数 `HOSTNAME` から読む
fn default_worker_id() -> String {
    let pid = std::process::id();
    match std::env::var("HOSTNAME") {
        Ok(host) if !host.trim().is_empty() => format!("{}-{pid}", host.trim()),
        _ => format!("worker-{pid}"),
    }
}

#[derive(Debug, Default)]
pub struct Summary {
    /// 記録した取得（Recorded::Saved）
    pub saved: usize,
    /// claim を取られていた（Recorded::StaleClaim）
    pub stale: usize,
    /// robots.txt が読めず、この実行の残りで見送ったホスト
    pub skipped_hosts: BTreeSet<String>,
}

/// 取得中の1件
struct InFlight {
    claim_token: i64,
    host_key: String,
}

/// 1件の処理の結果
struct Done {
    url_id: Uuid,
    recorded: Recorded,
    /// robots.txt が読めなかったホスト
    robots_unavailable: Option<String>,
}

/// 候補が尽きるまで claim → 取得 → 抽出 → 記録を回す。`run_id` の crawl_runs は呼び出し側が作る。
///
/// - 同時に `concurrency` 件まで、ホストごとに1件
/// - この実行で取った資源・いま取得中の資源に結ばれている URL は候補から外す（資源は実行あたり1回）
/// - robots.txt が読めないホストは、この実行の残りでは claim しない
/// - 取得中が無く候補も無いとき、近い再試行（`longest_retry_wait` 以内）があればそこまで待ち、無ければ終わる
/// - 記録が失敗したら残りを止めてエラーを返す。取得中だった行は lease が切れて次の実行で回収される
pub async fn run(
    db: &DatabaseConnection,
    fetcher: Arc<Fetcher>,
    run_id: Uuid,
    config: &Config,
) -> anyhow::Result<Summary> {
    let mut summary = Summary::default();
    let mut in_flight: BTreeMap<Uuid, InFlight> = BTreeMap::new();
    let mut taken_resources: BTreeSet<Uuid> = BTreeSet::new();
    let mut trusted: HashMap<String, bool> = HashMap::new();
    // 戻るとき（エラーを含む）に JoinSet を落とせば、残りのタスクは止まる
    let mut tasks: JoinSet<anyhow::Result<Done>> = JoinSet::new();
    let mut heartbeat =
        tokio::time::interval_at(Instant::now() + config.heartbeat, config.heartbeat);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        let exclude = Exclude {
            hosts: in_flight
                .values()
                .map(|flight| flight.host_key.clone())
                .chain(summary.skipped_hosts.iter().cloned())
                .collect(),
            resources: taken_resources.clone(),
        };

        let free = config.concurrency.saturating_sub(tasks.len());
        if free > 0 {
            let candidates = claim::candidates(db, &exclude).await?;
            let picked = claim::pick(
                candidates,
                |host| fetcher.ready_at(host),
                Instant::now(),
                free,
            );
            let ids: Vec<Uuid> = picked.iter().map(|candidate| candidate.url_id).collect();
            for claim in claim::claim(db, &ids, &config.worker_id, config.lease).await? {
                let host_trusted = match trusted.get(&claim.host_key) {
                    Some(trusted) => *trusted,
                    None => {
                        let value = host_trusted(db, &claim.host_key).await?;
                        trusted.insert(claim.host_key.clone(), value);
                        value
                    }
                };
                in_flight.insert(
                    claim.url_id,
                    InFlight {
                        claim_token: claim.claim_token,
                        host_key: claim.host_key.clone(),
                    },
                );
                taken_resources.extend(claim.resource_id);
                tasks.spawn(attempt(
                    db.clone(),
                    fetcher.clone(),
                    run_id,
                    claim,
                    host_trusted,
                    config.policy.clone(),
                ));
            }
        }

        if tasks.is_empty() {
            // 取得中が無いので、除外は見送ったホストとこの実行で取った資源だけ
            let exclude = Exclude {
                hosts: summary.skipped_hosts.clone(),
                resources: taken_resources.clone(),
            };
            match claim::next_retry_in(db, &exclude).await? {
                Some(wait) if wait <= config.policy.longest_retry_wait() => {
                    tokio::time::sleep(wait).await;
                    continue;
                }
                _ => break,
            }
        }

        tokio::select! {
            joined = tasks.join_next() => {
                let Some(joined) = joined else { continue };
                let done = joined.context("取得のタスクが止まった")??;
                in_flight.remove(&done.url_id);
                if let Some(host_key) = done.robots_unavailable {
                    summary.skipped_hosts.insert(host_key);
                }
                match done.recorded {
                    Recorded::Saved { processed, .. } => {
                        summary.saved += 1;
                        taken_resources.extend(processed.resource_id);
                    }
                    Recorded::StaleClaim => summary.stale += 1,
                    Recorded::AlreadyRecorded => {}
                }
            }
            _ = heartbeat.tick() => {
                let held: Vec<(Uuid, i64)> = in_flight
                    .iter()
                    .map(|(url_id, flight)| (*url_id, flight.claim_token))
                    .collect();
                claim::extend(db, &held, config.lease).await?;
            }
        }
    }
    Ok(summary)
}

/// 1件: validator を引く → 取得 → 抽出 → 記録
async fn attempt(
    db: DatabaseConnection,
    fetcher: Arc<Fetcher>,
    run_id: Uuid,
    claim: Claim,
    host_trusted: bool,
    policy: Policy,
) -> anyhow::Result<Done> {
    let validators = validators(&db, claim.resource_id).await?;
    let fetch = fetcher.fetch(&claim.url, &validators).await;
    let extracted = extract_of(&fetch);
    let ctx = Context {
        host_trusted,
        known_not_found_titles: &[],
    };
    let recorded = persist::record(
        &db,
        &Attempt {
            run_id,
            url_id: claim.url_id,
            claim_token: claim.claim_token,
            fetch: &fetch,
            extracted: extracted.as_ref(),
            ctx: &ctx,
            policy: &policy,
        },
    )
    .await
    .with_context(|| format!("取得の結果を記録できない: {}", claim.url))?;
    let robots_unavailable = match fetch.outcome {
        Outcome::RobotsUnavailable { host_key, .. } => Some(host_key),
        _ => None,
    };
    Ok(Done {
        url_id: claim.url_id,
        recorded,
        robots_unavailable,
    })
}

/// 許可リスト: urls.host_key の集合 ∪ 承認したホスト移行の移行先
pub async fn allowed_hosts(db: &impl ConnectionTrait) -> anyhow::Result<BTreeSet<String>> {
    let registered: Vec<String> = urls_table::Entity::find()
        .select_only()
        .column(urls_table::Column::HostKey)
        .distinct()
        .into_tuple()
        .all(db)
        .await
        .context("登録済みのホストを読めない")?;
    let mut hosts: BTreeSet<String> = registered.into_iter().collect();
    hosts.extend(host_moves::approved_hosts(db).await?);
    Ok(hosts)
}

/// いまの資源に保存した ETag・Last-Modified。資源が無ければ空
pub async fn validators(
    db: &impl ConnectionTrait,
    resource_id: Option<Uuid>,
) -> anyhow::Result<Validators> {
    let Some(resource_id) = resource_id else {
        return Ok(Validators::default());
    };
    let resource = resources::Entity::find()
        .filter(resources::Column::Id.eq(resource_id))
        .one(db)
        .await
        .context("資源の validator を読めない")?;
    Ok(resource
        .map(|resource| Validators {
            etag: resource.etag,
            last_modified: resource.last_modified,
        })
        .unwrap_or_default())
}

#[derive(FromQueryResult)]
struct DeclaredRow {
    page: String,
    declared: String,
}

/// そのホストの canonical の申告を信用するか。各 URL の最新の申告（fetch_history）を judge_host に掛ける
pub async fn host_trusted(db: &impl ConnectionTrait, host_key: &str) -> anyhow::Result<bool> {
    let rows = DeclaredRow::find_by_statement(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "SELECT u.normalized_url AS page, h.declared_canonical_url AS declared \
         FROM urls u \
         JOIN ( \
             SELECT DISTINCT ON (url_id) url_id, declared_canonical_url \
             FROM fetch_history \
             WHERE declared_canonical_url IS NOT NULL \
             ORDER BY url_id, fetched_at DESC \
         ) h ON h.url_id = u.id \
         WHERE u.host_key = $1",
        [host_key.into()],
    ))
    .all(db)
    .await
    .context("canonical の申告を読めない")?;

    // 読めない申告は捨てる
    let pairs: Vec<(String, String)> = rows
        .into_iter()
        .filter_map(|row| {
            let declared = urls::prepare(&row.declared).ok()?;
            Some((row.page, declared.normalized_url))
        })
        .collect();
    let declarations: Vec<Declaration<'_>> = pairs
        .iter()
        .map(|(page, declared)| Declaration { page, declared })
        .collect();
    Ok(canonical::judge_host(&declarations).is_none())
}

/// HTML を読みきった応答から本文を取り出す
pub fn extract_of(fetch: &Fetch) -> Option<Extracted> {
    match &fetch.outcome {
        Outcome::Response(response) => match &response.body {
            Body::Html(html) => Some(extract::extract(&html.text, &response.url)),
            _ => None,
        },
        _ => None,
    }
}
