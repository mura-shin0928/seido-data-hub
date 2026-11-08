//! 移転先の候補を、制度名とリンク文字列の近さで選ぶ。
//!
//! 規則の出どころは発見の調査（正規化・bigram の Dice・含む規則）。
//! 2026-11 の再現では、届いた hit 193件のうち188件が上位3件に入った。
//!
//! ```text
//! 制度名・リンク文字列を正規化 → ページらしい URL だけ → 除外を外す
//!   → 近さ（含む / Dice）→ 同じ dedup_key をまとめる → 上位3件
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use regex::Regex;
use serde::Serialize;
use unicode_normalization::UnicodeNormalization;

/// 候補にする近さの下限
pub const MIN_SCORE: f64 = 0.6;

/// 正規化した名前がこの文字数に満たなければ比べない（短い名前は何にでも当たる）
pub const MIN_NAME_CHARS: usize = 4;

/// 1制度に出す候補の上限
pub const MAX_CANDIDATES: usize = 3;

/// ページらしい拡張子（小文字）。拡張子が無い URL とディレクトリは別に通す。
const PAGE_EXTENSIONS: [&str; 6] = ["html", "htm", "shtml", "php", "aspx", "jsp"];

static PARENT_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(https?://[^/]+/(?:.*?/)?)[^/]*/?$").expect("親ディレクトリの正規表現")
});

/// 起点にするディレクトリ。クエリとフラグメントを落とし、パスの最後の `/` まで。
/// `…/b/` で終わるときはその上。トップはトップ。読めなければ `None`。
pub fn parent_dir(normalized_url: &str) -> Option<String> {
    let end = normalized_url
        .find(['?', '#'])
        .unwrap_or(normalized_url.len());
    PARENT_PATTERN
        .captures(&normalized_url[..end])
        .map(|c| c[1].to_string())
}

/// NFKC・小文字にして、文字と数字だけを残す。
pub fn normalize_name(s: &str) -> String {
    s.nfkc()
        .flat_map(char::to_lowercase)
        .filter(|c| c.is_alphanumeric())
        .collect()
}

fn bigrams(chars: &[char]) -> BTreeMap<(char, char), usize> {
    let mut map = BTreeMap::new();
    for w in chars.windows(2) {
        *map.entry((w[0], w[1])).or_insert(0) += 1;
    }
    map
}

/// 正規化済みの2つの近さ（0.0〜1.0）。どちらかが `MIN_NAME_CHARS` 未満なら 0.0。
///
/// 短い方が長い方に含まれ、長さが半分以上なら 1.0。そうでなければ bigram の Dice 係数。
pub fn similarity(a: &str, b: &str) -> f64 {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.len() < MIN_NAME_CHARS || b.len() < MIN_NAME_CHARS {
        return 0.0;
    }
    let (short, long) = if a.len() <= b.len() {
        (&a, &b)
    } else {
        (&b, &a)
    };
    if short.len() * 2 >= long.len() && long.windows(short.len()).any(|w| w == short.as_slice()) {
        return 1.0;
    }
    let (ba, bb) = (bigrams(&a), bigrams(&b));
    let common: usize = ba
        .iter()
        .map(|(k, n)| (*n).min(bb.get(k).copied().unwrap_or(0)))
        .sum();
    2.0 * common as f64 / ((a.len() - 1) + (b.len() - 1)) as f64
}

/// ページとして読める URL か。パスの最後の区切りが、拡張子なし・`/` 終わり・ページの拡張子のどれか。
pub fn page_like(url: &str) -> bool {
    let end = url.find(['?', '#']).unwrap_or(url.len());
    let Some((_, rest)) = url[..end].split_once("://") else {
        return false;
    };
    let Some((_, path)) = rest.split_once('/') else {
        return true;
    };
    let last = path.rsplit('/').next().unwrap_or("");
    match last.rsplit_once('.') {
        None => true,
        Some((_, ext)) => PAGE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()),
    }
}

/// 一覧ページから取れたリンク1件
#[derive(Debug, Clone, Copy)]
pub struct LinkRef<'a> {
    pub url: &'a str,
    pub dedup_key: &'a str,
    pub text: &'a str,
    /// このリンクがあった一覧ページの URL
    pub page: &'a str,
}

/// 移転先の候補1件
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Candidate {
    pub url: String,
    pub dedup_key: String,
    pub text: String,
    pub score: f64,
    pub page: String,
}

/// 近さの降順に並べた上位と、入りきらなかった数
#[derive(Debug, Default, PartialEq)]
pub struct Ranked {
    pub top: Vec<Candidate>,
    pub more: usize,
}

/// 制度名（正規化前の `canonical_name`・`short_name`）に近いリンクを選ぶ。
///
/// `exclude` は `dedup_key` の集合。同じ `dedup_key` は近さが最大の1件にまとめる。
/// 近さの降順、同じなら `url` の昇順。
pub fn rank<'a>(
    names: &[&str],
    links: impl IntoIterator<Item = LinkRef<'a>>,
    exclude: &BTreeSet<String>,
) -> Ranked {
    let names: Vec<String> = names
        .iter()
        .map(|n| normalize_name(n))
        .filter(|n| !n.is_empty())
        .collect();
    let mut best: BTreeMap<&str, Candidate> = BTreeMap::new();
    for link in links {
        if !page_like(link.url) || exclude.contains(link.dedup_key) {
            continue;
        }
        let text = normalize_name(link.text);
        let score = names
            .iter()
            .map(|n| similarity(n, &text))
            .fold(0.0, f64::max);
        if score < MIN_SCORE {
            continue;
        }
        let better = best
            .get(link.dedup_key)
            .is_none_or(|c| score > c.score || (score == c.score && link.url < c.url.as_str()));
        if better {
            best.insert(
                link.dedup_key,
                Candidate {
                    url: link.url.to_string(),
                    dedup_key: link.dedup_key.to_string(),
                    text: link.text.to_string(),
                    score,
                    page: link.page.to_string(),
                },
            );
        }
    }
    let mut all: Vec<Candidate> = best.into_values().collect();
    all.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.url.cmp(&b.url)));
    let more = all.len().saturating_sub(MAX_CANDIDATES);
    all.truncate(MAX_CANDIDATES);
    Ranked { top: all, more }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link<'a>(url: &'a str, key: &'a str, text: &'a str, page: &'a str) -> LinkRef<'a> {
        LinkRef {
            url,
            dedup_key: key,
            text,
            page,
        }
    }

    #[test]
    fn parent_dirs() {
        assert_eq!(
            parent_dir("https://h.jp/a/b.html").as_deref(),
            Some("https://h.jp/a/")
        );
        assert_eq!(
            parent_dir("https://h.jp/a/b/").as_deref(),
            Some("https://h.jp/a/")
        );
        assert_eq!(
            parent_dir("https://h.jp/a/b.html?x=1#f").as_deref(),
            Some("https://h.jp/a/")
        );
        assert_eq!(
            parent_dir("https://h.jp/").as_deref(),
            Some("https://h.jp/")
        );
        assert_eq!(
            parent_dir("https://h.jp/b.html").as_deref(),
            Some("https://h.jp/")
        );
        assert_eq!(parent_dir("not a url"), None);
    }

    #[test]
    fn names_are_normalized() {
        assert_eq!(
            normalize_name("児童 手当（令和７年度）"),
            "児童手当令和7年度"
        );
        assert_eq!(normalize_name("ＡＢＣ・ｄ"), "abcd");
    }

    #[test]
    fn similarity_rules() {
        assert!((similarity("abcd", "abce") - 2.0 / 3.0).abs() < 1e-9);
        assert_eq!(similarity("abcd", "wxyz"), 0.0);
        assert_eq!(similarity("児童育成手当", "児童育成手当の申請"), 1.0);
        assert!(similarity("児童手当", "児童手当制度についてのお知らせ") < 1.0);
        assert_eq!(similarity("保育園", "保育園"), 0.0);
    }

    #[test]
    fn page_like_extensions() {
        for url in [
            "https://h.jp/a/b.html",
            "https://h.jp/a/b.htm",
            "https://h.jp/a/",
            "https://h.jp/a/b",
            "https://h.jp/a/b.php?x=1",
            "https://h.jp/a/b.HTML",
        ] {
            assert!(page_like(url), "{url}");
        }
        for url in [
            "https://h.jp/a/b.pdf",
            "https://h.jp/a/b.docx",
            "https://h.jp/a/b.jpg",
            "https://h.jp/a/b.pdf#p=1",
        ] {
            assert!(!page_like(url), "{url}");
        }
    }

    #[test]
    fn rank_orders_and_cuts() {
        let links = [
            link(
                "https://h.jp/e.html",
                "e",
                "児童育成手当の申請",
                "https://h.jp/",
            ),
            link("https://h.jp/d.html", "d", "児童育成手当", "https://h.jp/"),
            link("https://h.jp/c.html", "c", "児童育成手当", "https://h.jp/"),
            link(
                "https://h.jp/b.html",
                "b",
                "児童育成手当について",
                "https://h.jp/",
            ),
            link(
                "https://h.jp/a.html",
                "a",
                "児童育成手当の制度",
                "https://h.jp/",
            ),
        ];
        let r = rank(&["児童育成手当"], links, &BTreeSet::new());
        let urls: Vec<&str> = r.top.iter().map(|c| c.url.as_str()).collect();
        assert_eq!(
            urls,
            [
                "https://h.jp/a.html",
                "https://h.jp/b.html",
                "https://h.jp/c.html"
            ]
        );
        assert!(r.top.iter().all(|c| c.score == 1.0));
        assert_eq!(r.more, 2);
    }

    #[test]
    fn rank_merges_same_url_from_two_pages() {
        // 近さ 8/11（含まないが 0.6 以上）と 1.0 の2件。どの順に並べても近い方が残る
        let far = link(
            "https://h.jp/x.html",
            "x",
            "児童育成の手当",
            "https://h.jp/p1/",
        );
        let near = link(
            "https://h.jp/x.html",
            "x",
            "児童育成手当の案内",
            "https://h.jp/p2/",
        );
        for links in [[far, near], [near, far]] {
            let r = rank(&["児童育成手当"], links, &BTreeSet::new());
            assert_eq!(r.top.len(), 1);
            assert_eq!(r.more, 0);
            assert_eq!(r.top[0].text, "児童育成手当の案内");
            assert_eq!(r.top[0].page, "https://h.jp/p2/");
            assert_eq!(r.top[0].score, 1.0);
        }
        let only_far = rank(&["児童育成手当"], [far], &BTreeSet::new());
        assert!((only_far.top[0].score - 8.0 / 11.0).abs() < 1e-9);
    }

    #[test]
    fn rank_merge_tie_prefers_smaller_url() {
        let a = link(
            "https://h.jp/a.html",
            "k",
            "児童育成手当",
            "https://h.jp/p1/",
        );
        let b = link(
            "https://h.jp/b.html",
            "k",
            "児童育成手当",
            "https://h.jp/p2/",
        );
        for links in [[a, b], [b, a]] {
            let r = rank(&["児童育成手当"], links, &BTreeSet::new());
            assert_eq!(r.top.len(), 1);
            assert_eq!(r.top[0].url, "https://h.jp/a.html");
            assert_eq!(r.top[0].page, "https://h.jp/p1/");
        }
    }

    #[test]
    fn rank_skips_excluded_and_non_pages() {
        let links = [
            link(
                "https://h.jp/ok.html",
                "ok",
                "児童育成手当",
                "https://h.jp/",
            ),
            link(
                "https://h.jp/ex.html",
                "ex",
                "児童育成手当",
                "https://h.jp/",
            ),
            link("https://h.jp/f.pdf", "f", "児童育成手当", "https://h.jp/"),
            link(
                "https://h.jp/far.html",
                "far",
                "保育所入所案内",
                "https://h.jp/",
            ),
        ];
        let exclude = BTreeSet::from(["ex".to_string()]);
        let r = rank(&["児童育成手当"], links, &exclude);
        assert_eq!(r.top.len(), 1);
        assert_eq!(r.top[0].url, "https://h.jp/ok.html");
        assert_eq!(r.more, 0);
    }

    #[test]
    fn short_names_give_no_candidates() {
        let links = [link("https://h.jp/a.html", "a", "保育", "https://h.jp/")];
        assert_eq!(rank(&["保育"], links, &BTreeSet::new()), Ranked::default());
        assert_eq!(
            rank(&["保育", ""], links, &BTreeSet::new()),
            Ranked::default()
        );
    }
}
