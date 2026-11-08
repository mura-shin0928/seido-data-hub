//! 主たる URL が死んだ制度の移転先の候補を、一覧ページのリンクから作る。
//!
//! DB の `page_links`（生きた一覧ページから取ったリンクの URL・リンク文字列）を、制度名に近い順に並べるだけで、
//! DB には何も書かない。人が確かめ済みの（制度・`dedup_key`）の組は、リポジトリの判定のファイル
//! （`data/reviews/*.json`）から渡され、候補から外す。結果は `--out` の JSON（成果物）と実行サマリーに出す。

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Context;
use domain::moves::{Candidate, LinkRef, rank};
use sea_orm::{
    ConnectionTrait, DbBackend, QueryResult, Statement, Value, prelude::DateTimeWithTimeZone,
    prelude::Uuid,
};
use serde::Serialize;

/// 起点がこの数以上あるのに生きた一覧ページが無いホストは、起点の規則が使えないとみなす
pub const UNUSABLE_MIN_STARTS: usize = 5;

/// 1制度の候補
#[derive(Debug, Serialize)]
pub struct Suggestion {
    pub program_id: Uuid,
    pub area_code: String,
    pub canonical_name: String,
    pub short_name: Option<String>,
    pub dead_url: String,
    pub candidates: Vec<Candidate>,
    /// 上位に入りきらなかった候補の数
    pub more: usize,
}

/// ホストごとの一覧ページの起点と、そのうち生きているものの数
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct HostCoverage {
    pub host_key: String,
    pub starts: usize,
    pub live_pages: usize,
}

#[derive(Debug, Serialize)]
pub struct Report {
    /// 主たる URL のいまの資源が deleted・deletion_candidate の制度の数
    pub targets: usize,
    pub suggestions: Vec<Suggestion>,
    /// 確かめ済みとして外した候補の数（名前の規則で近いと出たものだけ）
    pub skipped_judged: usize,
    pub unusable_hosts: Vec<HostCoverage>,
}

/// 主たる URL のいまの資源（`observed_at` が最新）が死んでいる制度。最新の資源の取り方は `link_related.rs` と同じ
const TARGETS: &str = "SELECT p.id, p.area_code, p.canonical_name, p.short_name, \
    u.normalized_url, u.host_key \
    FROM programs p \
    JOIN program_urls pu ON pu.program_id = p.id AND pu.role = 'main' \
    JOIN urls u ON u.id = pu.url_id \
    JOIN LATERAL (\
        SELECT ur.resource_id FROM url_resources ur \
        WHERE ur.url_id = u.id ORDER BY ur.observed_at DESC LIMIT 1) current ON TRUE \
    JOIN resources r ON r.id = current.resource_id \
    WHERE r.state IN ('deleted', 'deletion_candidate') \
    ORDER BY p.area_code, p.canonical_name, p.id";

/// 制度が既に持つ URL（主たる・関連のすべて）
const PROGRAM_KEYS: &str = "SELECT pu.program_id, u.dedup_key FROM program_urls pu \
    JOIN urls u ON u.id = pu.url_id WHERE pu.program_id = ANY($1)";

/// そのホストの、生きた一覧ページから取ったリンク
const HOST_LINKS: &str = "SELECT pl.url, pl.dedup_key, pl.text, r.canonical_url AS page \
    FROM page_links pl JOIN resources r ON r.id = pl.resource_id \
    WHERE r.state = 'active' AND pl.host_key = $1";

/// ホストごとの、取る対象の起点（`retired` 以外の `index`）の数と、そのうちいまの資源が active のものの数
const COVERAGE: &str = "SELECT u.host_key, count(*) AS starts, \
    count(*) FILTER (WHERE r.state = 'active') AS live_pages \
    FROM urls u \
    LEFT JOIN LATERAL (\
        SELECT ur.resource_id FROM url_resources ur \
        WHERE ur.url_id = u.id ORDER BY ur.observed_at DESC LIMIT 1) current ON TRUE \
    LEFT JOIN resources r ON r.id = current.resource_id \
    WHERE u.role = 'index' AND u.status <> 'retired' \
    GROUP BY u.host_key ORDER BY u.host_key";

struct Link {
    url: String,
    dedup_key: String,
    text: String,
    page: String,
}

impl Link {
    fn as_ref(&self) -> LinkRef<'_> {
        LinkRef {
            url: &self.url,
            dedup_key: &self.dedup_key,
            text: &self.text,
            page: &self.page,
        }
    }
}

fn select(sql: &str, values: impl IntoIterator<Item = Value>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, sql, values)
}

/// 移転先の候補を作る（読むだけ）。`judged` は人が確かめ済みの（制度, `dedup_key`）
pub async fn suggest(
    db: &impl ConnectionTrait,
    judged: &BTreeSet<(Uuid, String)>,
) -> anyhow::Result<Report> {
    let targets = db
        .query_all_raw(Statement::from_string(DbBackend::Postgres, TARGETS))
        .await
        .context("死んだ制度を読めない")?;
    let ids: Vec<Uuid> = targets
        .iter()
        .map(|row| row.try_get("", "id"))
        .collect::<Result<_, _>>()?;

    let mut own: BTreeMap<Uuid, BTreeSet<String>> = BTreeMap::new();
    if !ids.is_empty() {
        let rows = db
            .query_all_raw(select(PROGRAM_KEYS, [ids.into()]))
            .await
            .context("制度の URL を読めない")?;
        for row in rows {
            own.entry(row.try_get("", "program_id")?)
                .or_default()
                .insert(row.try_get("", "dedup_key")?);
        }
    }
    let mut judged_of: BTreeMap<Uuid, BTreeSet<String>> = BTreeMap::new();
    for (program_id, key) in judged {
        judged_of
            .entry(*program_id)
            .or_default()
            .insert(key.clone());
    }

    // リンクはホストごとに1回だけ読む
    let mut links_of: BTreeMap<String, Vec<Link>> = BTreeMap::new();
    let mut report = Report {
        targets: targets.len(),
        suggestions: Vec::new(),
        skipped_judged: 0,
        unusable_hosts: Vec::new(),
    };
    for row in &targets {
        let program_id: Uuid = row.try_get("", "id")?;
        let host: String = row.try_get("", "host_key")?;
        let dead_url: String = row.try_get("", "normalized_url")?;
        let canonical_name: String = row.try_get("", "canonical_name")?;
        let short_name: Option<String> = row.try_get("", "short_name")?;
        if !links_of.contains_key(&host) {
            let links = read_links(db, &host).await?;
            links_of.insert(host.clone(), links);
        }
        let links = &links_of[&host];

        let names = [canonical_name.as_str(), short_name.as_deref().unwrap_or("")];
        let mut exclude = own.remove(&program_id).unwrap_or_default();
        let program_judged = judged_of.remove(&program_id).unwrap_or_default();
        if !program_judged.is_empty() {
            // 確かめ済みのうち、外さなければ候補になっていたものの数
            let would_be = rank(
                &names,
                links
                    .iter()
                    .filter(|link| program_judged.contains(&link.dedup_key))
                    .map(Link::as_ref),
                &BTreeSet::new(),
            );
            report.skipped_judged += would_be.top.len() + would_be.more;
        }
        exclude.extend(program_judged);
        // 死んだ主たる URL も `own` に入っているので、候補には出ない
        let ranked = rank(&names, links.iter().map(Link::as_ref), &exclude);
        if ranked.top.is_empty() {
            continue;
        }
        report.suggestions.push(Suggestion {
            program_id,
            area_code: row.try_get("", "area_code")?,
            canonical_name,
            short_name,
            dead_url,
            candidates: ranked.top,
            more: ranked.more,
        });
    }

    let rows = db
        .query_all_raw(Statement::from_string(DbBackend::Postgres, COVERAGE))
        .await
        .context("一覧ページの起点を読めない")?;
    for row in rows {
        let coverage = host_coverage(&row)?;
        if coverage.starts >= UNUSABLE_MIN_STARTS && coverage.live_pages == 0 {
            report.unusable_hosts.push(coverage);
        }
    }
    Ok(report)
}

async fn read_links(db: &impl ConnectionTrait, host: &str) -> anyhow::Result<Vec<Link>> {
    db.query_all_raw(select(HOST_LINKS, [host.into()]))
        .await
        .with_context(|| format!("{host} の一覧ページのリンクを読めない"))?
        .iter()
        .map(|row| {
            Ok(Link {
                url: row.try_get("", "url")?,
                dedup_key: row.try_get("", "dedup_key")?,
                text: row.try_get("", "text")?,
                page: row.try_get("", "page")?,
            })
        })
        .collect()
}

fn host_coverage(row: &QueryResult) -> anyhow::Result<HostCoverage> {
    let starts: i64 = row.try_get("", "starts")?;
    let live_pages: i64 = row.try_get("", "live_pages")?;
    Ok(HostCoverage {
        host_key: row.try_get("", "host_key")?,
        starts: starts as usize,
        live_pages: live_pages as usize,
    })
}

/// DB の時計の、いまの時刻（RFC 3339）
pub async fn generated_at(db: &impl ConnectionTrait) -> anyhow::Result<String> {
    let row = db
        .query_one_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT now() AS now",
        ))
        .await?
        .context("時刻を読めない")?;
    let now: DateTimeWithTimeZone = row.try_get("", "now")?;
    Ok(now.to_rfc3339())
}

/// `--out` に書く JSON（整形済み）
pub fn candidates_json(report: &Report, generated_at: &str) -> anyhow::Result<String> {
    Ok(serde_json::to_string_pretty(&serde_json::json!({
        "generated_at": generated_at,
        "suggestions": report.suggestions,
    }))?)
}

/// 実行サマリー。候補そのもの（制度名・URL）は出さず、成果物で見る
pub fn markdown(report: &Report) -> String {
    let candidates: usize = report.suggestions.iter().map(|s| s.candidates.len()).sum();
    let cut = report.suggestions.iter().filter(|s| s.more > 0).count();
    let mut out = format!(
        "## 移転先の候補\n\n\
         | 項目 | 件数 |\n| --- | ---: |\n\
         | 対象の制度 | {} |\n\
         | まだ確かめていない候補がある制度 | {} |\n\
         | 候補の延べ | {} |\n\
         | 4件目以降を切った制度 | {} |\n\
         | 確かめ済みとして外した候補 | {} |\n\
         | 起点の規則が使えないホスト | {} |\n",
        report.targets,
        report.suggestions.len(),
        candidates,
        cut,
        report.skipped_judged,
        report.unusable_hosts.len()
    );
    if !report.unusable_hosts.is_empty() {
        out.push_str("\n起点の規則が使えないホスト（ホスト・起点の数）:\n\n");
        for host in &report.unusable_hosts {
            out.push_str(&format!("- {} ({})\n", host.host_key, host.starts));
        }
    }
    out.push_str("\n候補のファイルは `gh run download <run id> -n move-candidates` で取る。\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_lists_counts_and_hosts() {
        let report = Report {
            targets: 7,
            suggestions: vec![Suggestion {
                program_id: Uuid::nil(),
                area_code: "130001".into(),
                canonical_name: "児童手当".into(),
                short_name: None,
                dead_url: "https://a.example.jp/x.html".into(),
                candidates: vec![],
                more: 2,
            }],
            skipped_judged: 3,
            unusable_hosts: vec![HostCoverage {
                host_key: "www.town.example.jp".into(),
                starts: 6,
                live_pages: 0,
            }],
        };
        let md = markdown(&report);
        assert!(md.starts_with("## 移転先の候補\n"));
        assert!(md.contains("| 対象の制度 | 7 |"));
        assert!(md.contains("| まだ確かめていない候補がある制度 | 1 |"));
        assert!(md.contains("| 4件目以降を切った制度 | 1 |"));
        assert!(md.contains("| 確かめ済みとして外した候補 | 3 |"));
        assert!(md.contains("| 起点の規則が使えないホスト | 1 |"));
        assert!(md.contains("- www.town.example.jp (6)"));
        assert!(md.contains("gh run download <run id> -n move-candidates"));
        assert!(!md.contains("児童手当"));
    }
}
