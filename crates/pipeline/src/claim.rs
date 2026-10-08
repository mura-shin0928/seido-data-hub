//! 時刻の来た URL を選び、lease を付けて claim する（§9 §13 §14）。
//!
//! 候補の条件（取れる状態で時刻が来たこと、そのホストに有効な lease が無いこと）は `claimable` の1か所に置き、
//! 候補・claim・再試行までの残りが同じ条件を使う。時刻の比較と書き込みは DB の時計（`now()`）で行う。
//!
//! 候補と claim は、時刻の来る少し前の行も取れる（`ahead`）。週1の起動は毎回数分ずれるので、
//! 余裕が無いと7日間隔の行が「まだ」になって1週飛ぶ。再試行を待つ行には当てない
//! （バックオフと `Retry-After` を守る）。

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use anyhow::Context as _;
use sea_orm::{
    ConnectionTrait, DbBackend, FromQueryResult, Statement, Value,
    prelude::{DateTimeWithTimeZone, Uuid},
};
use tokio::time::Instant;

#[derive(Debug, Clone, PartialEq, Eq, FromQueryResult)]
pub struct Candidate {
    pub url_id: Uuid,
    pub host_key: String,
    pub priority: i32,
    pub next_crawl_at: DateTimeWithTimeZone,
    /// いま結ばれている資源（url_resources の observed_at が最新）。無ければ None。
    /// 別のホストの URL が同じ資源に結ばれていることがあるので、呼び出し側が1回の claim の中で重ならないようにする
    pub resource_id: Option<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Eq, FromQueryResult)]
pub struct Claim {
    pub url_id: Uuid,
    pub claim_token: i64,
    /// `urls.normalized_url`
    pub url: String,
    pub host_key: String,
    /// いま結ばれている資源（url_resources の observed_at が最新）。無ければ None
    pub resource_id: Option<Uuid>,
    /// claim する前の行が `processing`（持ち主の lease が切れていた）だった
    pub lease_expired: bool,
}

/// 候補から外すもの
#[derive(Debug, Default, Clone)]
pub struct Exclude {
    pub hosts: BTreeSet<String>,
    /// 資源 → それを取った URL。資源にいま結ばれている URL は、取った URL 自身を除いて外す
    /// （取った URL の再試行は同じ実行の中で続ける）
    pub resources: BTreeMap<Uuid, Uuid>,
}

/// 候補の条件（`u` は urls）。`due` が None なら時刻の条件を外す。Some なら、その幅だけ先の時刻まで取る。
///
/// - 時刻: `next_crawl_at` が now + 幅 までに来る行、または lease の切れた `processing`。
///   `retry_wait` は幅を当てず、時刻どおり（先取りでバックオフと `Retry-After` を縮めない）
/// - ホスト: 同じホストに有効な lease の `processing` が無いこと
fn claimable(due: Option<Duration>) -> String {
    let time = match due {
        // 秒は整数なので、そのまま SQL に埋める
        Some(ahead) => format!(
            "((u.status NOT IN ('processing', 'retry_wait') \
                AND u.next_crawl_at <= now() + make_interval(secs => {})) \
              OR (u.status = 'retry_wait' AND u.next_crawl_at <= now()) \
              OR (u.status = 'processing' AND u.lease_until < now()))",
            ahead.as_secs()
        ),
        None => "TRUE".to_string(),
    };
    format!(
        "{time} AND NOT EXISTS (\
            SELECT 1 FROM urls live \
            WHERE live.host_key = u.host_key \
              AND live.status = 'processing' AND live.lease_until >= now())"
    )
}

/// URL のいまの資源（`current.resource_id`。無ければ NULL）を横に付ける
const CURRENT_RESOURCE: &str = "LEFT JOIN LATERAL (\
    SELECT ur.resource_id FROM url_resources ur \
    WHERE ur.url_id = u.id ORDER BY ur.observed_at DESC LIMIT 1) current ON TRUE";

/// 除外の条件。`$1` にホスト、`$2` と `$3` に資源とそれを取った URL を同じ順で渡す
const NOT_EXCLUDED: &str = "u.host_key <> ALL($1) \
    AND NOT EXISTS (\
        SELECT 1 FROM unnest($2::uuid[], $3::uuid[]) AS taken(resource_id, url_id) \
        WHERE taken.resource_id = current.resource_id AND taken.url_id <> u.id)";

fn exclude_values(exclude: &Exclude) -> [Value; 3] {
    let hosts: Vec<String> = exclude.hosts.iter().cloned().collect();
    let (resources, takers): (Vec<Uuid>, Vec<Uuid>) = exclude
        .resources
        .iter()
        .map(|(resource, taker)| (*resource, *taker))
        .unzip();
    [hosts.into(), resources.into(), takers.into()]
}

/// 時刻が来た（`ahead` だけ先まで含む）URL を、ホストごとに先頭1件（priority 降順 → next_crawl_at → id）
pub async fn candidates(
    db: &impl ConnectionTrait,
    exclude: &Exclude,
    ahead: Duration,
) -> anyhow::Result<Vec<Candidate>> {
    let sql = format!(
        "SELECT DISTINCT ON (u.host_key) u.id AS url_id, u.host_key, u.priority, u.next_crawl_at, \
             current.resource_id \
         FROM urls u {CURRENT_RESOURCE} \
         WHERE {} AND {NOT_EXCLUDED} \
         ORDER BY u.host_key, u.priority DESC, u.next_crawl_at, u.id",
        claimable(Some(ahead))
    );
    Candidate::find_by_statement(Statement::from_sql_and_values(
        DbBackend::Postgres,
        sql,
        exclude_values(exclude),
    ))
    .all(db)
    .await
    .context("候補を選べない")
}

/// ホストの次に送ってよい時刻の早い順（過去・不明は now と同じ）、同点は priority → next_crawl_at → id で `limit` 件
pub fn pick(
    mut candidates: Vec<Candidate>,
    ready_at: impl Fn(&str) -> Option<Instant>,
    now: Instant,
    limit: usize,
) -> Vec<Candidate> {
    candidates.sort_by_cached_key(|candidate| {
        let ready = ready_at(&candidate.host_key).map_or(now, |at| at.max(now));
        (
            ready,
            Reverse(candidate.priority),
            candidate.next_crawl_at,
            candidate.url_id,
        )
    });
    candidates.truncate(limit);
    candidates
}

/// 渡した URL のうち、まだ候補の条件（`ahead` は `candidates` と同じ値）を満たすものだけを claim する（FOR UPDATE SKIP LOCKED）。
///
/// 1つの文で選んで書くので、選んだ行と書いた行はずれない。他の claim がロック中の行は飛ばす。
/// 同じホストの URL を一度に渡すと両方 claim する（ホストに1件は呼び出し側が `candidates` で守る）
pub async fn claim(
    db: &impl ConnectionTrait,
    url_ids: &[Uuid],
    worker_id: &str,
    lease: Duration,
    ahead: Duration,
) -> anyhow::Result<Vec<Claim>> {
    if url_ids.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "WITH picked AS (\
            SELECT u.id, (u.status = 'processing') AS lease_expired FROM urls u \
            WHERE u.id = ANY($1) AND {} \
            FOR UPDATE OF u SKIP LOCKED) \
         UPDATE urls SET status = 'processing', worker_id = $2, \
             lease_until = now() + make_interval(secs => $3), \
             claim_token = urls.claim_token + 1, updated_at = now() \
         FROM picked WHERE urls.id = picked.id \
         RETURNING urls.id AS url_id, urls.claim_token, urls.normalized_url AS url, urls.host_key, \
             (SELECT ur.resource_id FROM url_resources ur \
              WHERE ur.url_id = urls.id ORDER BY ur.observed_at DESC LIMIT 1) AS resource_id, \
             picked.lease_expired",
        claimable(Some(ahead))
    );
    let mut claims = Claim::find_by_statement(Statement::from_sql_and_values(
        DbBackend::Postgres,
        sql,
        [
            url_ids.to_vec().into(),
            worker_id.into(),
            lease.as_secs_f64().into(),
        ],
    ))
    .all(db)
    .await
    .context("claim できない")?;
    // RETURNING の順は決まらないので、渡した順に揃える
    claims.sort_by_key(|claim| url_ids.iter().position(|id| *id == claim.url_id));
    Ok(claims)
}

/// claim_token が合う processing の行の lease を now() + lease に延ばす。延ばした件数を返す
pub async fn extend(
    db: &impl ConnectionTrait,
    claims: &[(Uuid, i64)],
    lease: Duration,
) -> anyhow::Result<u64> {
    if claims.is_empty() {
        return Ok(0);
    }
    let (ids, tokens): (Vec<Uuid>, Vec<i64>) = claims.iter().copied().unzip();
    let result = db
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE urls SET lease_until = now() + make_interval(secs => $3), updated_at = now() \
             FROM unnest($1::uuid[], $2::bigint[]) AS held(id, claim_token) \
             WHERE urls.id = held.id AND urls.claim_token = held.claim_token \
               AND urls.status = 'processing'",
            [ids.into(), tokens.into(), lease.as_secs_f64().into()],
        ))
        .await
        .context("lease を延ばせない")?;
    Ok(result.rows_affected())
}

/// 除外を通った retry_wait のうち、最も早い next_crawl_at までの残り（過去なら ZERO）。無ければ None
pub async fn next_retry_in(
    db: &impl ConnectionTrait,
    exclude: &Exclude,
) -> anyhow::Result<Option<Duration>> {
    let sql = format!(
        "SELECT extract(epoch from min(u.next_crawl_at) - now())::float8 AS secs \
         FROM urls u {CURRENT_RESOURCE} \
         WHERE u.status = 'retry_wait' AND {} AND {NOT_EXCLUDED}",
        claimable(None)
    );
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            sql,
            exclude_values(exclude),
        ))
        .await
        .context("再試行までの残りを読めない")?
        .context("集計の行が無い")?;
    let secs: Option<f64> = row.try_get("", "secs")?;
    Ok(secs.map(|secs| Duration::from_secs_f64(secs.max(0.0))))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(id: u128, host: &str, priority: i32, next_crawl_at: &str) -> Candidate {
        Candidate {
            url_id: Uuid::from_u128(id),
            host_key: host.to_string(),
            priority,
            next_crawl_at: DateTimeWithTimeZone::parse_from_rfc3339(next_crawl_at).unwrap(),
            resource_id: None,
        }
    }

    #[test]
    fn pick_prefers_the_host_that_is_ready_sooner() {
        let now = Instant::now() + Duration::from_secs(60);
        let a = candidate(1, "a", 90, "2026-10-01T09:00:00+09:00");
        let b = candidate(2, "b", 80, "2026-10-01T09:30:00+09:00");
        let c = candidate(3, "c", 80, "2026-10-01T09:10:00+09:00");
        let ready_at = |host: &str| match host {
            "a" => Some(now + Duration::from_secs(3)),
            "c" => Some(now - Duration::from_secs(1)),
            _ => None,
        };

        let picked = pick(vec![a, b.clone(), c.clone()], ready_at, now, 2);
        assert_eq!(picked, vec![c, b]);
    }
}
