//! 巡回の1回の集計（§22）: 型・比率・警告の判定・実行サマリーの Markdown。

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

/// 1ホスト分の集計
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HostStats {
    /// outcome = response の行の http_status ごとの件数
    pub statuses: BTreeMap<u16, u64>,
    /// outcome <> response の行。network は "network:{種類}"、ほかは outcome の名前
    pub stopped: BTreeMap<String, u64>,
    /// observation ごとの件数
    pub observations: BTreeMap<String, u64>,
    pub compared: u64,
    pub changed: u64,
    pub bytes: u64,
    /// 文字の置き換えが起きたページ（charset_replaced）
    pub replaced: u64,
}

fn ratio(numerator: u64, denominator: u64) -> Option<f64> {
    (denominator > 0).then(|| numerator as f64 / denominator as f64)
}

fn merge_counts<K: Ord + Clone>(into: &mut BTreeMap<K, u64>, from: &BTreeMap<K, u64>) {
    for (k, n) in from {
        *into.entry(k.clone()).or_default() += n;
    }
}

impl HostStats {
    /// 取りに行った回数（応答があったもの＋応答なし・止めたもの）
    pub fn fetched(&self) -> u64 {
        self.responses() + self.stopped.values().sum::<u64>()
    }

    /// 応答があった回数
    pub fn responses(&self) -> u64 {
        self.statuses.values().sum()
    }

    /// 成功（200 と 304）
    pub fn ok(&self) -> u64 {
        self.status(200) + self.status(304)
    }

    fn status(&self, code: u16) -> u64 {
        self.statuses.get(&code).copied().unwrap_or(0)
    }

    fn observation(&self, name: &str) -> u64 {
        self.observations.get(name).copied().unwrap_or(0)
    }

    /// 304 の割合（200 と 304 に対して）。どちらも無ければ None
    pub fn not_modified_ratio(&self) -> Option<f64> {
        ratio(self.status(304), self.status(200) + self.status(304))
    }

    /// 内容の変更率。比べたものが無ければ None
    pub fn change_rate(&self) -> Option<f64> {
        ratio(self.changed, self.compared)
    }

    /// 429 と 5xx の合計
    pub fn throttled_or_server_error(&self) -> u64 {
        self.status(429) + self.statuses.range(500..=599).map(|(_, n)| n).sum::<u64>()
    }

    /// 別の集計を足し合わせる（マップは件数ごとに足す）
    pub fn add(&mut self, other: &HostStats) {
        merge_counts(&mut self.statuses, &other.statuses);
        merge_counts(&mut self.stopped, &other.stopped);
        merge_counts(&mut self.observations, &other.observations);
        self.compared += other.compared;
        self.changed += other.changed;
        self.bytes += other.bytes;
        self.replaced += other.replaced;
    }
}

/// 実行のループが数えたもの
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Counters {
    pub retries: u64,
    pub lease_expired: u64,
}

/// 実行1回分の集計
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RunStats {
    /// host_key → 集計
    pub hosts: BTreeMap<String, HostStats>,
    pub response_ms_p50: Option<u64>,
    pub response_ms_p95: Option<u64>,
    /// この実行で取って、いま failed_final の URL
    pub failed_final: u64,
    /// 実行の終わりに残った、時刻の来た行（processing 以外で next_crawl_at <= now()）
    pub queue_due: u64,
    pub queue_oldest_wait_secs: Option<u64>,
    /// 実行のループが数えたもの。履歴から作り直したときは None
    pub counters: Option<Counters>,
}

impl RunStats {
    /// 全ホストを足し合わせた集計
    pub fn totals(&self) -> HostStats {
        let mut total = HostStats::default();
        for h in self.hosts.values() {
            total.add(h);
        }
        total
    }
}

/// 実行時の設定の控え
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConfigSnapshot {
    pub worker_id: String,
    pub concurrency: usize,
    pub min_interval_ms: u64,
    pub lease_secs: u64,
    pub heartbeat_secs: u64,
    pub connect_timeout_ms: u64,
    pub timeout_ms: u64,
    pub max_retries: i32,
}

/// 実行の結果から立つ警告
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Alert {
    HostErrorsSurged {
        host: String,
        count: u64,
        rate: f64,
        previous_rate: Option<f64>,
    },
    AllFailed {
        fetched: u64,
    },
    ContentChangeRateHigh {
        changed: u64,
        compared: u64,
    },
    LeaseExpiredOften {
        lease_expired: u64,
        fetched: u64,
    },
}

/// 率を「100倍して小数1桁＋%」にする
fn pct(x: f64) -> String {
    format!("{:.1}%", x * 100.0)
}

fn pct_or_dash(x: Option<f64>) -> String {
    x.map_or_else(|| "—".to_string(), pct)
}

fn or_dash<T: fmt::Display>(x: Option<T>) -> String {
    x.map_or_else(|| "—".to_string(), |v| v.to_string())
}

impl fmt::Display for Alert {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Alert::HostErrorsSurged {
                host,
                count,
                rate,
                previous_rate,
            } => {
                let previous = previous_rate.map_or_else(|| "なし".to_string(), pct);
                write!(
                    f,
                    "{host} で 429・5xx が {count} 件（{}、前回 {previous}）",
                    pct(*rate)
                )
            }
            Alert::AllFailed { fetched } => {
                write!(f, "{fetched} 件すべてで応答が無い（ネットワーク断を疑う）")
            }
            Alert::ContentChangeRateHigh { changed, compared } => {
                let rate = ratio(*changed, *compared).unwrap_or(0.0);
                write!(
                    f,
                    "内容の変更率が {changed}/{compared}（{}）で50%を超えた（抽出規則の壊れを疑う）",
                    pct(rate)
                )
            }
            Alert::LeaseExpiredOften {
                lease_expired,
                fetched,
            } => write!(
                f,
                "lease 切れの再 claim が {lease_expired} 件（取得 {fetched} 件）"
            ),
        }
    }
}

/// 警告の閾値。値は仮置きで、運用しながら見直す
#[derive(Debug, Clone, PartialEq)]
pub struct Thresholds {
    /// ホストの 429・5xx がこの件数以上
    pub surge_min_count: u64,
    /// ホストの取得に対する 429・5xx の割合がこれ以上
    pub surge_min_rate: f64,
    /// 前回の割合のこの倍を超える
    pub surge_over_previous: f64,
    /// 変更率の警告に必要な、比べた件数の下限
    pub change_min_compared: u64,
    /// 変更率がこれを超えたら警告
    pub change_max_rate: f64,
    /// lease 切れの再 claim がこの件数以上
    pub lease_min_count: u64,
    /// 取得に対する lease 切れの割合がこれ以上
    pub lease_min_rate: f64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            surge_min_count: 5,
            surge_min_rate: 0.2,
            surge_over_previous: 2.0,
            change_min_compared: 20,
            change_max_rate: 0.5,
            lease_min_count: 10,
            lease_min_rate: 0.05,
        }
    }
}

/// 今回の集計（と、あれば前回）から立つ警告を返す
pub fn alerts(
    current: &RunStats,
    previous: Option<&RunStats>,
    thresholds: &Thresholds,
) -> Vec<Alert> {
    let mut out = Vec::new();

    for (host, h) in &current.hosts {
        let count = h.throttled_or_server_error();
        let Some(rate) = ratio(count, h.fetched()) else {
            continue;
        };
        if count < thresholds.surge_min_count || rate < thresholds.surge_min_rate {
            continue;
        }
        // 前回にそのホストが無い（または取得0件）ときは、前回との比較を外す
        let previous_rate = previous
            .and_then(|p| p.hosts.get(host))
            .and_then(|p| ratio(p.throttled_or_server_error(), p.fetched()));
        if let Some(prev) = previous_rate
            && rate <= prev * thresholds.surge_over_previous
        {
            continue;
        }
        out.push(Alert::HostErrorsSurged {
            host: host.clone(),
            count,
            rate,
            previous_rate,
        });
    }

    let total = current.totals();
    let fetched = total.fetched();
    // 全件失敗は通信まわり（robots の取得不能とネットワークエラー）だけで応答が0のときに限る。
    // 範囲外・robots 拒否・リダイレクト異常はサーバー側の結果で、通信の障害ではない
    let only_network_failures = current
        .hosts
        .values()
        .flat_map(|h| h.stopped.keys())
        .all(|key| key == "robots_unavailable" || key.starts_with("network:"));
    if fetched >= 1 && total.responses() == 0 && only_network_failures {
        out.push(Alert::AllFailed { fetched });
    }

    if total.compared >= thresholds.change_min_compared
        && total
            .change_rate()
            .is_some_and(|r| r > thresholds.change_max_rate)
    {
        out.push(Alert::ContentChangeRateHigh {
            changed: total.changed,
            compared: total.compared,
        });
    }

    if let Some(c) = &current.counters
        && c.lease_expired >= thresholds.lease_min_count
        && ratio(c.lease_expired, fetched).is_some_and(|r| r >= thresholds.lease_min_rate)
    {
        out.push(Alert::LeaseExpiredOften {
            lease_expired: c.lease_expired,
            fetched,
        });
    }

    out
}

/// サマリーの見出しに出す、実行の情報
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RunMeta {
    pub run_id: String,
    pub kind: String,
    /// 表示用（JST の "2026-10-03 21:04"）
    pub started_at: String,
    /// finished_at - started_at。終わっていなければ None
    pub duration_secs: Option<u64>,
    pub config: Option<ConfigSnapshot>,
}

/// 「{key}{sep}{n}」を " / " でつなぐ。1件も無ければ「—」
fn join_counts<K: fmt::Display>(items: impl Iterator<Item = (K, u64)>, sep: &str) -> String {
    let parts: Vec<String> = items.map(|(k, n)| format!("{k}{sep}{n}")).collect();
    if parts.is_empty() {
        "—".to_string()
    } else {
        parts.join(" / ")
    }
}

/// 実行サマリーを Markdown にする。値の無いところは「—」
pub fn render_markdown(meta: &RunMeta, stats: &RunStats, alerts: &[Alert]) -> String {
    let total = stats.totals();
    let fetched = total.fetched();
    let mut md = String::new();

    md.push_str(&format!("## 巡回 {}（{}）\n\n", meta.kind, meta.run_id));

    let duration = match meta.duration_secs {
        Some(s) => format!("{}分{}秒", s / 60, s % 60),
        None => "終わっていない".to_string(),
    };
    md.push_str(&format!("- 開始 {} / 所要 {duration}\n", meta.started_at));

    let per_minute = meta.duration_secs.filter(|s| *s > 0).map_or_else(
        || "—".to_string(),
        |s| format!("{:.1}", fetched as f64 * 60.0 / s as f64),
    );
    md.push_str(&format!(
        "- 取得 {fetched} 件（1分あたり {per_minute} 件）/ {:.1} MB / 応答時間 中央 {} ms・95% {} ms\n",
        total.bytes as f64 / 1_000_000.0,
        or_dash(stats.response_ms_p50),
        or_dash(stats.response_ms_p95),
    ));

    if let Some(c) = &meta.config {
        md.push_str(&format!(
            "- 設定: 同時 {} 件・同一ホスト 1 件・間隔 {:.1} 秒・lease {} 秒・再試行 {} 回（worker {}）\n",
            c.concurrency,
            c.min_interval_ms as f64 / 1000.0,
            c.lease_secs,
            c.max_retries,
            c.worker_id,
        ));
    }

    let wait = stats
        .queue_oldest_wait_secs
        .map_or_else(|| "—".to_string(), |s| format!("{:.1}", s as f64 / 3600.0));
    md.push_str(&format!(
        "- 残った待ち行列 {} 件（最古の待ち {wait} 時間）\n",
        stats.queue_due
    ));

    let robots: Vec<&str> = stats
        .hosts
        .iter()
        .filter(|(_, h)| h.stopped.contains_key("robots_unavailable"))
        .map(|(k, _)| k.as_str())
        .collect();
    if !robots.is_empty() {
        md.push_str(&format!(
            "- robots.txt が読めず見送ったホスト: {}\n",
            robots.join(", ")
        ));
    }

    md.push_str("\n### 警告\n\n");
    if alerts.is_empty() {
        md.push_str("- なし\n");
    } else {
        for a in alerts {
            md.push_str(&format!("- {a}\n"));
        }
    }

    let not_found = join_counts(
        total
            .observations
            .iter()
            .filter(|(k, _)| k.as_str() != "alive")
            .map(|(k, n)| (k.as_str(), *n)),
        " ",
    );
    let stopped = join_counts(total.stopped.iter().map(|(k, n)| (k.as_str(), *n)), " ");
    let statuses = join_counts(total.statuses.iter().map(|(k, n)| (*k, *n)), ": ");
    let (retries, lease_expired) = match &stats.counters {
        Some(c) => (c.retries.to_string(), c.lease_expired.to_string()),
        None => ("—".to_string(), "—".to_string()),
    };

    md.push_str("\n### 全体\n\n| 項目 | 値 |\n|---|---|\n");
    md.push_str(&format!(
        "| 成功（200・304） | {} / {fetched}（{}） |\n",
        total.ok(),
        pct_or_dash(ratio(total.ok(), fetched)),
    ));
    md.push_str(&format!("| status | {statuses} |\n"));
    md.push_str(&format!("| 応答なし・止めた | {stopped} |\n"));
    md.push_str(&format!(
        "| 304 の割合（200 に対して） | {} |\n",
        pct_or_dash(total.not_modified_ratio())
    ));
    md.push_str(&format!(
        "| 内容の変更率 | {}（比べた {} 件） |\n",
        pct_or_dash(total.change_rate()),
        total.compared
    ));
    md.push_str(&format!("| 404・410・ソフト404 | {not_found} |\n"));
    md.push_str(&format!(
        "| 429・5xx | {} |\n",
        total.throttled_or_server_error()
    ));
    md.push_str(&format!("| 文字の置き換え | {} |\n", total.replaced));
    md.push_str(&format!(
        "| 再試行 / failed_final / lease 切れ | {retries} / {} / {lease_expired} |\n",
        stats.failed_final
    ));

    md.push_str(&format!(
        "\n<details><summary>ホスト別（{}）</summary>\n\n",
        stats.hosts.len()
    ));
    md.push_str("| ホスト | 取得 | 200 | 304 | 404・410 | 403 | 429・5xx | 応答なし・止めた | 304 の割合 | 変更率 | ソフト404・トップ転送 |\n");
    md.push_str("|---|---|---|---|---|---|---|---|---|---|---|\n");
    for (key, h) in &stats.hosts {
        md.push_str(&format!(
            "| {key} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
            h.fetched(),
            h.status(200),
            h.status(304),
            h.status(404) + h.status(410),
            h.status(403),
            h.throttled_or_server_error(),
            h.stopped.values().sum::<u64>(),
            pct_or_dash(h.not_modified_ratio()),
            pct_or_dash(h.change_rate()),
            h.observation("soft_404_title")
                + h.observation("soft_404_known_title")
                + h.observation("top_redirect"),
        ));
    }
    md.push_str("\n</details>\n");
    md
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(statuses: &[(u16, u64)]) -> HostStats {
        HostStats {
            statuses: statuses.iter().copied().collect(),
            ..HostStats::default()
        }
    }

    fn stats(hosts: Vec<(&str, HostStats)>) -> RunStats {
        RunStats {
            hosts: hosts.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
            ..RunStats::default()
        }
    }

    fn meta() -> RunMeta {
        RunMeta {
            run_id: "r1".into(),
            kind: "crawl".into(),
            started_at: "2026-10-03 21:04".into(),
            duration_secs: None,
            config: None,
        }
    }

    const CITY: &str = "http://www.city.example.jp";

    #[test]
    fn ratios_are_counted_against_200_and_304() {
        let mut h = host(&[(200, 6), (304, 2), (404, 1)]);
        h.stopped.insert("network:timeout".into(), 1);
        h.compared = 4;
        h.changed = 1;
        assert_eq!(h.fetched(), 10);
        assert_eq!(h.ok(), 8);
        assert_eq!(h.not_modified_ratio(), Some(0.25));
        assert_eq!(h.change_rate(), Some(0.25));
    }

    #[test]
    fn an_empty_host_has_no_ratios() {
        let h = HostStats::default();
        assert_eq!(h.fetched(), 0);
        assert_eq!(h.not_modified_ratio(), None);
        assert_eq!(h.change_rate(), None);
    }

    #[test]
    fn totals_add_up_every_host() {
        let mut b = host(&[(200, 2)]);
        b.stopped.insert("robots_unavailable".into(), 1);
        let s = stats(vec![("a", host(&[(200, 1), (404, 1)])), ("b", b)]);
        let t = s.totals();
        assert_eq!(t.statuses, [(200, 3), (404, 1)].into_iter().collect());
        assert_eq!(t.fetched(), 5);
    }

    fn surge_host(count_503: u64, fetched: u64) -> RunStats {
        stats(vec![(
            CITY,
            host(&[(200, fetched - count_503), (503, count_503)]),
        )])
    }

    #[test]
    fn a_host_surge_needs_count_rate_and_a_jump_from_the_previous_run() {
        let t = Thresholds::default();
        let cur = surge_host(5, 20);
        assert_eq!(
            alerts(&cur, None, &t),
            vec![Alert::HostErrorsSurged {
                host: CITY.into(),
                count: 5,
                rate: 0.25,
                previous_rate: None
            }]
        );
        assert!(alerts(&cur, Some(&surge_host(3, 20)), &t).is_empty());
        assert_eq!(
            alerts(&cur, Some(&surge_host(1, 20)), &t),
            vec![Alert::HostErrorsSurged {
                host: CITY.into(),
                count: 5,
                rate: 0.25,
                previous_rate: Some(0.05)
            }]
        );
        let other = stats(vec![("other", host(&[(200, 20)]))]);
        assert_eq!(
            alerts(&cur, Some(&other), &t),
            vec![Alert::HostErrorsSurged {
                host: CITY.into(),
                count: 5,
                rate: 0.25,
                previous_rate: None
            }]
        );
        assert!(alerts(&surge_host(4, 20), None, &t).is_empty());
        assert!(alerts(&surge_host(5, 30), None, &t).is_empty());
    }

    #[test]
    fn a_run_where_nothing_answered_is_flagged() {
        let t = Thresholds::default();
        let mut h = HostStats::default();
        h.stopped.insert("robots_unavailable".into(), 2);
        h.stopped.insert("network:dns".into(), 1);
        assert_eq!(
            alerts(&stats(vec![(CITY, h)]), None, &t),
            vec![Alert::AllFailed { fetched: 3 }]
        );
        assert!(alerts(&RunStats::default(), None, &t).is_empty());
        assert!(alerts(&stats(vec![(CITY, host(&[(403, 1)]))]), None, &t).is_empty());
    }

    #[test]
    fn server_side_stops_alone_do_not_flag_an_outage() {
        let t = Thresholds::default();
        let mut h = HostStats::default();
        h.stopped.insert("out_of_scope".into(), 2);
        assert!(alerts(&stats(vec![(CITY, h)]), None, &t).is_empty());
        let mut h = HostStats::default();
        h.stopped.insert("out_of_scope".into(), 1);
        h.stopped.insert("network:dns".into(), 1);
        assert!(alerts(&stats(vec![(CITY, h)]), None, &t).is_empty());
    }

    #[test]
    fn a_change_rate_over_half_is_flagged_only_with_enough_comparisons() {
        let t = Thresholds::default();
        let with = |compared, changed| {
            let mut h = host(&[(200, 20)]);
            h.compared = compared;
            h.changed = changed;
            stats(vec![(CITY, h)])
        };
        assert_eq!(
            alerts(&with(20, 11), None, &t),
            vec![Alert::ContentChangeRateHigh {
                changed: 11,
                compared: 20
            }]
        );
        assert!(alerts(&with(20, 10), None, &t).is_empty());
        assert!(alerts(&with(19, 19), None, &t).is_empty());
    }

    #[test]
    fn expired_leases_are_flagged_when_many_and_frequent() {
        let t = Thresholds::default();
        let with = |fetched, lease_expired| RunStats {
            counters: Some(Counters {
                retries: 0,
                lease_expired,
            }),
            ..stats(vec![(CITY, host(&[(200, fetched)]))])
        };
        assert_eq!(
            alerts(&with(200, 10), None, &t),
            vec![Alert::LeaseExpiredOften {
                lease_expired: 10,
                fetched: 200
            }]
        );
        assert!(alerts(&with(200, 9), None, &t).is_empty());
        assert!(alerts(&with(201, 10), None, &t).is_empty());
        assert!(alerts(&stats(vec![(CITY, host(&[(200, 200)]))]), None, &t).is_empty());
    }

    fn city_stats() -> RunStats {
        let mut h = host(&[(200, 6), (304, 2), (404, 1)]);
        h.observations.insert("alive".into(), 8);
        h.observations.insert("not_found".into(), 1);
        h.compared = 4;
        h.changed = 1;
        stats(vec![(CITY, h)])
    }

    #[test]
    fn the_summary_shows_host_statuses_ratios_and_not_found_counts() {
        let md = render_markdown(&meta(), &city_stats(), &[]);
        assert!(
            md.contains("| 304 の割合（200 に対して） | 25.0% |"),
            "{md}"
        );
        assert!(md.contains("内容の変更率 | 25.0%（比べた 4 件）"), "{md}");
        assert!(md.contains("not_found 1"), "{md}");
        assert!(
            md.contains("| http://www.city.example.jp | 9 | 6 | 2 | 1 |"),
            "{md}"
        );
        assert!(md.contains("### 警告\n\n- なし\n"), "{md}");

        let md = render_markdown(&meta(), &city_stats(), &[Alert::AllFailed { fetched: 9 }]);
        assert!(
            md.contains("- 9 件すべてで応答が無い（ネットワーク断を疑う）\n"),
            "{md}"
        );
        assert!(!md.contains("- なし"), "{md}");
    }

    #[test]
    fn an_empty_run_renders_dashes_and_no_rates() {
        let md = render_markdown(&meta(), &RunStats::default(), &[]);
        assert!(md.contains("終わっていない"), "{md}");
        assert!(md.contains("| 304 の割合（200 に対して） | — |"), "{md}");
        assert!(
            md.contains("| 再試行 / failed_final / lease 切れ | — / 0 / — |"),
            "{md}"
        );
        assert!(!md.contains("NaN"), "{md}");
    }

    #[test]
    fn a_full_run_renders_config_duration_and_robots_hosts() {
        let mut s = city_stats();
        s.hosts
            .get_mut(CITY)
            .unwrap()
            .stopped
            .insert("robots_unavailable".into(), 1);
        s.counters = Some(Counters {
            retries: 2,
            lease_expired: 1,
        });
        let m = RunMeta {
            duration_secs: Some(150),
            config: Some(ConfigSnapshot {
                worker_id: "w1".into(),
                concurrency: 4,
                min_interval_ms: 1500,
                lease_secs: 300,
                heartbeat_secs: 60,
                connect_timeout_ms: 5000,
                timeout_ms: 30000,
                max_retries: 3,
            }),
            ..meta()
        };
        let md = render_markdown(&m, &s, &[]);
        assert!(md.contains("所要 2分30秒"), "{md}");
        assert!(md.contains("1分あたり 4.0 件"), "{md}");
        assert!(
            md.contains(
                "同時 4 件・同一ホスト 1 件・間隔 1.5 秒・lease 300 秒・再試行 3 回（worker w1）"
            ),
            "{md}"
        );
        assert!(
            md.contains("robots.txt が読めず見送ったホスト: http://www.city.example.jp"),
            "{md}"
        );
        assert!(
            md.contains("| 再試行 / failed_final / lease 切れ | 2 / 0 / 1 |"),
            "{md}"
        );
    }

    #[test]
    fn stats_round_trip_through_json() {
        let mut s = city_stats();
        s.counters = Some(Counters {
            retries: 1,
            lease_expired: 0,
        });
        let json = serde_json::to_value(&s).unwrap();
        assert_eq!(serde_json::from_value::<RunStats>(json).unwrap(), s);
        let a = Alert::AllFailed { fetched: 3 };
        let json = serde_json::to_value(&a).unwrap();
        assert_eq!(json["kind"], "all_failed");
        assert_eq!(serde_json::from_value::<Alert>(json).unwrap(), a);
    }
}
