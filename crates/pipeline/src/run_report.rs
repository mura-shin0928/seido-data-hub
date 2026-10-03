//! 実行1回分の集計を取得の履歴から数え、警告を判定して `crawl_runs` に残す。

use std::io::Write as _;
use std::path::Path;

use anyhow::Context as _;
use domain::run_report::{
    Alert, ConfigSnapshot, Counters, HostStats, RunMeta, RunStats, Thresholds, alerts,
};
use sea_orm::{ConnectionTrait, DbBackend, FromQueryResult, Statement, prelude::Uuid};

/// 履歴の行を、取得した URL のホストとつないだもの
const FROM_RUN: &str = "from fetch_history h join urls u on u.id = h.url_id where h.run_id = $1";

/// ホストの集計を取り出す（無ければ作る）。取得が1件も無いホストは入らない
fn host_of<'a>(stats: &'a mut RunStats, host_key: &str) -> &'a mut HostStats {
    stats.hosts.entry(host_key.to_string()).or_default()
}

#[derive(FromQueryResult)]
struct CountRow {
    host_key: String,
    key: Option<String>,
    n: i64,
}

async fn count_rows(
    db: &impl ConnectionTrait,
    run_id: Uuid,
    sql: &str,
) -> anyhow::Result<Vec<CountRow>> {
    CountRow::find_by_statement(Statement::from_sql_and_values(
        DbBackend::Postgres,
        sql,
        [run_id.into()],
    ))
    .all(db)
    .await
    .context("実行の集計を読めない")
}

#[derive(FromQueryResult)]
struct TotalsRow {
    host_key: String,
    bytes: i64,
    replaced: i64,
}

#[derive(FromQueryResult)]
struct ComparedRow {
    host_key: String,
    compared: i64,
    changed: i64,
}

#[derive(FromQueryResult)]
struct SpreadRow {
    p50: Option<i64>,
    p95: Option<i64>,
}

#[derive(FromQueryResult)]
struct QueueRow {
    due: i64,
    oldest_wait_secs: Option<i64>,
}

#[derive(FromQueryResult)]
struct OneCount {
    n: i64,
}

fn to_u64(n: i64) -> u64 {
    u64::try_from(n).unwrap_or(0)
}

/// `run_id` の実行の集計を履歴（`fetch_history`）と `urls` から数える。`counters` は渡したものをそのまま入れる
pub async fn collect(
    db: &impl ConnectionTrait,
    run_id: Uuid,
    counters: Option<Counters>,
) -> anyhow::Result<RunStats> {
    let mut stats = RunStats {
        counters,
        ..RunStats::default()
    };

    let statuses = count_rows(
        db,
        run_id,
        &format!(
            "select u.host_key, h.http_status::text as key, count(*) as n {FROM_RUN} \
             and h.outcome = 'response' group by 1, 2"
        ),
    )
    .await?;
    for row in statuses {
        if let Some(status) = row.key.and_then(|k| k.parse::<u16>().ok()) {
            host_of(&mut stats, &row.host_key)
                .statuses
                .insert(status, to_u64(row.n));
        }
    }

    let stopped = count_rows(
        db,
        run_id,
        &format!(
            "select u.host_key, \
                    case when h.outcome = 'network' then 'network:' || split_part(h.error_detail, ':', 1) \
                         else h.outcome end as key, \
                    count(*) as n {FROM_RUN} and h.outcome <> 'response' group by 1, 2"
        ),
    )
    .await?;
    for row in stopped {
        host_of(&mut stats, &row.host_key)
            .stopped
            .insert(row.key.unwrap_or_default(), to_u64(row.n));
    }

    let observations = count_rows(
        db,
        run_id,
        &format!(
            "select u.host_key, h.observation as key, count(*) as n {FROM_RUN} \
             and h.observation is not null group by 1, 2"
        ),
    )
    .await?;
    for row in observations {
        host_of(&mut stats, &row.host_key)
            .observations
            .insert(row.key.unwrap_or_default(), to_u64(row.n));
    }

    let totals = TotalsRow::find_by_statement(Statement::from_sql_and_values(
        DbBackend::Postgres,
        format!(
            "select u.host_key, coalesce(sum(h.bytes), 0)::bigint as bytes, \
                    count(*) filter (where h.charset_replaced) as replaced {FROM_RUN} group by 1"
        ),
        [run_id.into()],
    ))
    .all(db)
    .await
    .context("実行のバイト数を読めない")?;
    for row in totals {
        let host = host_of(&mut stats, &row.host_key);
        host.bytes = to_u64(row.bytes);
        host.replaced = to_u64(row.replaced);
    }

    // 比べる相手は、別の実行で最後にハッシュを持った取得（304 はハッシュを持たないので飛ばされる）
    let compared = ComparedRow::find_by_statement(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "select u.host_key, \
                count(*) filter (where prev.hash is not null) as compared, \
                count(*) filter (where prev.hash is not null and h.http_status = 200 \
                                   and coalesce(h.body_hash, h.raw_hash) <> prev.hash) as changed \
           from fetch_history h join urls u on u.id = h.url_id \
           left join lateral ( \
                select coalesce(p.body_hash, p.raw_hash) as hash from fetch_history p \
                 where p.url_id = h.url_id and p.run_id <> h.run_id and p.fetched_at < h.fetched_at \
                   and coalesce(p.body_hash, p.raw_hash) is not null \
                 order by p.fetched_at desc limit 1) prev on true \
          where h.run_id = $1 and h.observation = 'alive' \
            and (h.http_status = 304 or coalesce(h.body_hash, h.raw_hash) is not null) \
          group by 1",
        [run_id.into()],
    ))
    .all(db)
    .await
    .context("内容の変更を読めない")?;
    for row in compared {
        let host = host_of(&mut stats, &row.host_key);
        host.compared = to_u64(row.compared);
        host.changed = to_u64(row.changed);
    }

    let spread = SpreadRow::find_by_statement(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "select percentile_disc(0.5) within group (order by h.response_time_ms) as p50, \
                percentile_disc(0.95) within group (order by h.response_time_ms) as p95 \
           from fetch_history h where h.run_id = $1 and h.outcome = 'response'",
        [run_id.into()],
    ))
    .one(db)
    .await
    .context("応答時間を読めない")?;
    if let Some(spread) = spread {
        stats.response_ms_p50 = spread.p50.map(to_u64);
        stats.response_ms_p95 = spread.p95.map(to_u64);
    }

    let failed_final = OneCount::find_by_statement(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "select count(distinct u.id) as n from fetch_history h join urls u on u.id = h.url_id \
          where h.run_id = $1 and u.status = 'failed_final'",
        [run_id.into()],
    ))
    .one(db)
    .await
    .context("failed_final を数えられない")?;
    stats.failed_final = failed_final.map_or(0, |row| to_u64(row.n));

    let queue = QueueRow::find_by_statement(Statement::from_string(
        DbBackend::Postgres,
        "select count(*) as due, \
                extract(epoch from now() - min(next_crawl_at))::bigint as oldest_wait_secs \
           from urls where status <> 'processing' and next_crawl_at <= now()",
    ))
    .one(db)
    .await
    .context("待ち行列を読めない")?;
    if let Some(queue) = queue {
        stats.queue_due = to_u64(queue.due);
        stats.queue_oldest_wait_secs = queue.oldest_wait_secs.map(to_u64);
    }

    Ok(stats)
}

#[derive(FromQueryResult)]
struct IdRow {
    id: Uuid,
}

/// `run_id` より前に始まり `finished_at` のある直近の実行（種類は問わない）
pub async fn previous_run(db: &impl ConnectionTrait, run_id: Uuid) -> anyhow::Result<Option<Uuid>> {
    let row = IdRow::find_by_statement(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "select p.id from crawl_runs p join crawl_runs c on c.id = $1 \
          where p.id <> c.id and p.started_at < c.started_at and p.finished_at is not null \
          order by p.started_at desc limit 1",
        [run_id.into()],
    ))
    .one(db)
    .await
    .context("前回の実行を探せない")?;
    Ok(row.map(|row| row.id))
}

#[derive(FromQueryResult)]
struct RunRow {
    id: Uuid,
    kind: String,
    started_at_jst: String,
    duration_secs: Option<i64>,
    config: Option<serde_json::Value>,
    stats: Option<serde_json::Value>,
    alerts: Option<serde_json::Value>,
}

async fn run_row(db: &impl ConnectionTrait, run_id: Option<Uuid>) -> anyhow::Result<RunRow> {
    let select = "select id, kind, \
                         to_char(started_at at time zone 'Asia/Tokyo', 'YYYY-MM-DD HH24:MI') as started_at_jst, \
                         extract(epoch from finished_at - started_at)::bigint as duration_secs, \
                         config, stats, alerts from crawl_runs";
    let statement = match run_id {
        Some(id) => Statement::from_sql_and_values(
            DbBackend::Postgres,
            format!("{select} where id = $1"),
            [id.into()],
        ),
        None => Statement::from_string(
            DbBackend::Postgres,
            format!("{select} order by started_at desc limit 1"),
        ),
    };
    RunRow::find_by_statement(statement)
        .one(db)
        .await
        .context("実行を読めない")?
        .context("該当する実行が無い")
}

/// 前回と比べた警告（前回が無ければ前回なしで判定する）
pub async fn alerts_for(
    db: &impl ConnectionTrait,
    run_id: Uuid,
    stats: &RunStats,
) -> anyhow::Result<Vec<Alert>> {
    // 前回の集計は、保存した値ではなく履歴から数え直す（数え方を変えても同じ定義で比べるため）
    let previous = match previous_run(db, run_id).await? {
        Some(previous) => Some(collect(db, previous, None).await?),
        None => None,
    };
    Ok(alerts(stats, previous.as_ref(), &Thresholds::default()))
}

/// `finished_at` を今にし、集計と警告を1つの UPDATE で書く
pub async fn close(
    db: &impl ConnectionTrait,
    run_id: Uuid,
    stats: &RunStats,
    alerts: &[Alert],
) -> anyhow::Result<()> {
    db.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "update crawl_runs set finished_at = now(), stats = $2, alerts = $3 where id = $1",
        [
            run_id.into(),
            serde_json::to_value(stats)?.into(),
            serde_json::to_value(alerts)?.into(),
        ],
    ))
    .await
    .with_context(|| format!("実行 {run_id} の集計を書けない"))?;
    Ok(())
}

pub struct Report {
    pub meta: RunMeta,
    pub stats: RunStats,
    pub alerts: Vec<Alert>,
}

/// 保存した集計（stats・alerts）があればそれを、無ければ履歴から数え直して返す
/// （counters は None、警告は `alerts_for` で数え直す）。
/// `run_id` が None なら `started_at` が最新の実行。実行が無ければエラー
pub async fn load(db: &impl ConnectionTrait, run_id: Option<Uuid>) -> anyhow::Result<Report> {
    let row = run_row(db, run_id).await?;
    let config: Option<ConfigSnapshot> = row
        .config
        .map(serde_json::from_value)
        .transpose()
        .context("保存した設定を読めない")?;
    let stats: RunStats = match row.stats {
        Some(value) => serde_json::from_value(value).context("保存した集計を読めない")?,
        None => collect(db, row.id, None).await?,
    };
    let alerts: Vec<Alert> = match row.alerts {
        Some(value) => serde_json::from_value(value).context("保存した警告を読めない")?,
        None => alerts_for(db, row.id, &stats).await?,
    };
    Ok(Report {
        meta: RunMeta {
            run_id: row.id.to_string(),
            kind: row.kind,
            started_at: row.started_at_jst,
            duration_secs: row.duration_secs.map(to_u64),
            config,
        },
        stats,
        alerts,
    })
}

/// `GITHUB_STEP_SUMMARY` のファイルに追記する
pub fn append_step_summary(path: &Path, markdown: &str) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{markdown}")
}
