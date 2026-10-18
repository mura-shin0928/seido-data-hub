//! 制度が返す事実。制度は状態を持たず、主たる URL のいまの資源から導く。

use std::collections::HashMap;

use chrono::{DateTime, FixedOffset};
use sea_orm::FromQueryResult;
use serde::Serialize;
use uuid::Uuid;

/// いまの資源の行（resources から必要な列だけ）
#[derive(Debug, Clone, FromQueryResult)]
pub struct ResourceRow {
    pub id: Uuid,
    pub canonical_url: String,
    pub state: String,
    pub last_crawled_at: Option<DateTime<FixedOffset>>,
    pub last_changed_at: Option<DateTime<FixedOffset>>,
    pub moved_to_resource_id: Option<Uuid>,
}

/// 制度に結ばれた URL 1つ（program_urls × urls × いまの資源の id）
#[derive(Debug, Clone, FromQueryResult)]
pub struct UrlRow {
    pub program_id: Uuid,
    /// `main` か `related`
    pub role: String,
    pub rank: i32,
    pub raw_url: String,
    /// `urls.status`
    pub url_status: String,
    pub resource_id: Option<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LinkFacts {
    pub url: String,
    pub state: Option<String>,
    pub checked_at: Option<DateTime<FixedOffset>>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProgramFacts {
    /// いま開ける URL
    pub url: String,
    pub state: Option<String>,
    pub checked_at: Option<DateTime<FixedOffset>>,
    pub changed_at: Option<DateTime<FixedOffset>>,
    pub auto_update: bool,
    pub moved_to: Option<String>,
    pub related_links: Vec<LinkFacts>,
}

/// moved をたどる上限（仮置き）。移動を記録する側（`pipeline::lifecycle`）は循環しないように書いてあるので、念のための止め。
/// 2026-10-18 の巡回済みの写しで moved は0件で、続く段数は測っていない
pub const MAX_MOVED_HOPS: usize = 10;

/// `urls` はこの制度の行だけ（順不同）。`resources` は、いまの資源と、moved の先を引いたもの
pub fn assemble(
    source_url: &str,
    urls: &[UrlRow],
    resources: &HashMap<Uuid, ResourceRow>,
) -> ProgramFacts {
    let main = urls.iter().find(|u| u.role == "main");
    let auto_update = main.is_none_or(|u| u.url_status != "blocked");
    let Some((main_resource, main_url)) = main.and_then(|u| current(u, resources)) else {
        return ProgramFacts {
            url: source_url.to_string(),
            state: None,
            checked_at: None,
            changed_at: None,
            auto_update,
            moved_to: None,
            related_links: vec![],
        };
    };

    let related_links = if matches!(
        main_resource.state.as_str(),
        "deleted" | "deletion_candidate"
    ) {
        let mut related: Vec<&UrlRow> = urls
            .iter()
            .filter(|u| u.role == "related" && u.resource_id != Some(main_resource.id))
            .collect();
        related.sort_by_key(|u| u.rank);
        related
            .into_iter()
            .map(|u| match current(u, resources) {
                Some((resource, url)) => LinkFacts {
                    url,
                    state: Some(resource.state.clone()),
                    checked_at: resource.last_crawled_at,
                },
                None => LinkFacts {
                    url: u.raw_url.clone(),
                    state: None,
                    checked_at: None,
                },
            })
            .collect()
    } else {
        vec![]
    };

    ProgramFacts {
        moved_to: (main_resource.state == "moved").then(|| main_url.clone()),
        url: main_url,
        state: Some(main_resource.state.clone()),
        checked_at: main_resource.last_crawled_at,
        changed_at: main_resource.last_changed_at,
        auto_update,
        related_links,
    }
}

/// URL のいまの資源と、いま開ける URL（moved ならたどった先の `canonical_url`）。資源が無ければ None
fn current<'a>(
    url: &UrlRow,
    resources: &'a HashMap<Uuid, ResourceRow>,
) -> Option<(&'a ResourceRow, String)> {
    let resource = resources.get(&url.resource_id?)?;
    let mut last = resource;
    for _ in 0..MAX_MOVED_HOPS {
        if last.state != "moved" {
            break;
        }
        match last.moved_to_resource_id.and_then(|id| resources.get(&id)) {
            Some(next) => last = next,
            None => break,
        }
    }
    Some((resource, last.canonical_url.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    const SOURCE: &str = "https://www.city.example.jp/old.html";

    fn at(day: u32) -> DateTime<FixedOffset> {
        FixedOffset::east_opt(9 * 3600)
            .unwrap()
            .with_ymd_and_hms(2026, 10, day, 3, 0, 0)
            .unwrap()
    }

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn resource(n: u128, state: &str, moved_to: Option<u128>) -> ResourceRow {
        ResourceRow {
            id: id(n),
            canonical_url: format!("https://www.city.example.jp/r{n}.html"),
            state: state.to_string(),
            last_crawled_at: Some(at(n as u32)),
            last_changed_at: Some(at(n as u32 + 10)),
            moved_to_resource_id: moved_to.map(id),
        }
    }

    fn row(role: &str, rank: i32, status: &str, resource: Option<u128>) -> UrlRow {
        UrlRow {
            program_id: id(999),
            role: role.to_string(),
            rank,
            raw_url: format!("https://www.city.example.jp/raw{rank}.html"),
            url_status: status.to_string(),
            resource_id: resource.map(id),
        }
    }

    fn map(rows: Vec<ResourceRow>) -> HashMap<Uuid, ResourceRow> {
        rows.into_iter().map(|r| (r.id, r)).collect()
    }

    fn unchecked() -> ProgramFacts {
        ProgramFacts {
            url: SOURCE.to_string(),
            state: None,
            checked_at: None,
            changed_at: None,
            auto_update: true,
            moved_to: None,
            related_links: vec![],
        }
    }

    #[test]
    fn without_main_url_falls_back_to_source_url() {
        assert_eq!(assemble(SOURCE, &[], &HashMap::new()), unchecked());

        let resources = map(vec![resource(1, "deleted", None)]);
        let urls = [row("related", 1, "succeeded", Some(1))];
        assert_eq!(assemble(SOURCE, &urls, &resources), unchecked());
    }

    #[test]
    fn main_without_resource_is_unchecked() {
        let urls = [row("main", 1, "queued", None)];
        assert_eq!(assemble(SOURCE, &urls, &HashMap::new()), unchecked());
    }

    #[test]
    fn active_uses_canonical_url() {
        let resources = map(vec![
            resource(1, "active", None),
            resource(2, "active", None),
        ]);
        let urls = [
            row("main", 1, "succeeded", Some(1)),
            row("related", 1, "succeeded", Some(2)),
        ];
        let facts = assemble(SOURCE, &urls, &resources);
        assert_eq!(
            facts,
            ProgramFacts {
                url: "https://www.city.example.jp/r1.html".to_string(),
                state: Some("active".to_string()),
                checked_at: Some(at(1)),
                changed_at: Some(at(11)),
                auto_update: true,
                moved_to: None,
                related_links: vec![],
            }
        );
    }

    #[test]
    fn moved_follows_to_the_last_resource() {
        let resources = map(vec![
            resource(1, "moved", Some(2)),
            resource(2, "moved", Some(3)),
            resource(3, "active", None),
        ]);
        let urls = [row("main", 1, "succeeded", Some(1))];
        let facts = assemble(SOURCE, &urls, &resources);
        let last = "https://www.city.example.jp/r3.html".to_string();
        assert_eq!(facts.state.as_deref(), Some("moved"));
        assert_eq!(facts.url, last);
        assert_eq!(facts.moved_to, Some(last));
        assert_eq!(facts.checked_at, Some(at(1)));
        assert_eq!(facts.changed_at, Some(at(11)));
    }

    #[test]
    fn moved_cycle_stops() {
        let resources = map(vec![
            resource(1, "moved", Some(2)),
            resource(2, "moved", Some(1)),
        ]);
        let urls = [row("main", 1, "succeeded", Some(1))];
        let facts = assemble(SOURCE, &urls, &resources);
        // 10段（偶数）たどると A に戻って止まる
        assert_eq!(facts.url, "https://www.city.example.jp/r1.html");
        assert_eq!(facts.moved_to.as_deref(), Some(facts.url.as_str()));
    }

    #[test]
    fn deleted_lists_related_links_in_rank_order() {
        let resources = map(vec![
            resource(1, "deleted", None),
            resource(3, "active", None),
        ]);
        let urls = [
            row("related", 3, "succeeded", Some(3)),
            row("main", 1, "succeeded", Some(1)),
            row("related", 2, "succeeded", Some(1)),
            row("related", 1, "queued", None),
        ];
        let facts = assemble(SOURCE, &urls, &resources);
        assert_eq!(facts.state.as_deref(), Some("deleted"));
        assert_eq!(
            facts.related_links,
            vec![
                LinkFacts {
                    url: "https://www.city.example.jp/raw1.html".to_string(),
                    state: None,
                    checked_at: None,
                },
                LinkFacts {
                    url: "https://www.city.example.jp/r3.html".to_string(),
                    state: Some("active".to_string()),
                    checked_at: Some(at(3)),
                },
            ]
        );
    }

    #[test]
    fn deletion_candidate_also_lists_related_links() {
        let resources = map(vec![
            resource(1, "deletion_candidate", None),
            resource(2, "deleted", None),
        ]);
        let urls = [
            row("main", 1, "succeeded", Some(1)),
            row("related", 1, "succeeded", Some(2)),
        ];
        let facts = assemble(SOURCE, &urls, &resources);
        assert_eq!(facts.related_links.len(), 1);
        assert_eq!(facts.related_links[0].state.as_deref(), Some("deleted"));
    }

    #[test]
    fn blocked_main_url_turns_off_auto_update() {
        let resources = map(vec![resource(1, "active", None)]);
        let urls = [row("main", 1, "blocked", Some(1))];
        assert!(!assemble(SOURCE, &urls, &resources).auto_update);
    }
}
