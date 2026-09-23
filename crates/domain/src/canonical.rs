//! 取得の結果から代表 URL（`canonical_url`）を決める（§8）。
//!
//! ```text
//! 妥当な恒久転送の先 → 検証を通った rel=canonical の申告 → 取りに行った URL（normalized_url）
//! ```
//!
//! 比べる URL はどれも `urls::prepare` を通した `normalized_url` の形にそろえる。
//! サイト固有の畳み込み（`/smph/`）は `dedup_key` の上だけなので、代表 URL には当てない。

use std::collections::HashMap;

use url::Url;

use crate::fetch::{self, Redirect};
use crate::urls::{self, Rejection};

/// ホスト全体が同じ先を指すとみなす最小のページ数（**仮置き**）。
/// 2ページまでは `/foo` と `/foo/index.html` をまとめる正しい使い方と区別できない
pub const MIN_SAME_TARGET_PAGES: usize = 3;

/// 転送の1段。先頭が取りに行った URL、最後が最終応答
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hop<'a> {
    pub url: &'a str,
    pub status: u16,
}

/// 代表 URL を決める材料
#[derive(Debug, Clone, Copy)]
pub struct Observed<'a> {
    /// 1段以上。各段の URL は取得のときに正規化済み
    pub hops: &'a [Hop<'a>],
    /// 最終応答の HTML にあった `rel=canonical`（絶対 URL）
    pub declared: Option<&'a str>,
    /// ホストの申告を信用するか（`judge_host` の結果）
    pub host_trusted: bool,
}

/// 代表 URL の根拠（`resources.canonical_source`）
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    Normalized,
    Declared,
    Redirect,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Normalized => "normalized",
            Self::Declared => "declared",
            Self::Redirect => "redirect",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "normalized" => Some(Self::Normalized),
            "declared" => Some(Self::Declared),
            "redirect" => Some(Self::Redirect),
            _ => None,
        }
    }
}

/// URL から資源への結び方（`url_resources.relation`）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relation {
    /// 取りに行った URL がそのまま代表 URL
    Direct,
    /// 恒久転送の先が代表 URL
    Redirect,
    /// 申告された canonical が代表 URL
    Declared,
}

impl Relation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Redirect => "redirect",
            Self::Declared => "declared",
        }
    }
}

/// 候補にしなかった恒久転送
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IgnoredRedirect {
    /// 消えたページの受け皿としてのトップへの転送。同じ内容の移動ではない
    #[error("トップページへの転送")]
    ToTopPage,
}

/// 採らなかった申告
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IgnoredDeclared {
    #[error("URL として読めない: {0}")]
    Unreadable(Rejection),
    #[error("別のホストを指す")]
    OtherHost,
    /// 一時転送やトップへの転送の先にあったページの申告。取りに行った URL の申告ではない
    #[error("採らなかった転送の先のページの申告")]
    RedirectedAway,
    #[error("ホストの申告を信用していない")]
    HostUntrusted,
    #[error("トップ以外のページがトップを指す")]
    PointsToTopPage,
    /// 恒久転送の先を採った。申告は記録だけ
    #[error("恒久転送の先と食い違う")]
    ConflictsWithRedirect,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub canonical_url: String,
    pub source: Source,
    pub relation: Relation,
    /// 転送後に実際に到達した URL
    pub final_url: String,
    /// 申告（読めたものは正規化した形、読めないものはそのまま）
    pub declared_canonical_url: Option<String>,
    pub ignored_redirect: Option<IgnoredRedirect>,
    pub ignored_declared: Option<IgnoredDeclared>,
}

/// 最終応答の status から、資源をどう扱うか（**仮置き**）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// 代表 URL を決めて資源に結ぶ（2xx・404・410。404・410 は削除候補として資源が要る）
    Link,
    /// 前回の結び付きをそのまま使う（304。本文が無く申告が読めない）
    KeepPrevious,
    /// 資源に触らない（403・429・5xx など。取れなかったのはジョブの事実で、資源の性質ではない）
    Skip,
}

pub fn action(status: u16) -> Action {
    match status {
        200..=299 | 404 | 410 => Action::Link,
        304 => Action::KeepPrevious,
        _ => Action::Skip,
    }
}

/// 代表 URL を決める。`hops` が空なら決めない
pub fn decide(observed: &Observed<'_>) -> Option<Decision> {
    let (start, last) = (observed.hops.first()?, observed.hops.last()?);

    // 先頭から続く恒久転送だけを辿る。一時転送の先は代表にしない
    let mut candidate = None;
    for pair in observed.hops.windows(2) {
        if fetch::redirect_kind(pair[0].status) != Some(Redirect::Permanent) {
            break;
        }
        candidate = Some(pair[1].url);
    }
    let mut ignored_redirect = None;
    if candidate.is_some_and(|url| is_top_page(url) && !is_top_page(start.url)) {
        candidate = None;
        ignored_redirect = Some(IgnoredRedirect::ToTopPage);
    }

    let declared = observed
        .declared
        .map(|raw| urls::prepare(raw).map_err(|e| (raw, e)));
    let declared_canonical_url = declared.as_ref().map(|d| match d {
        Ok(url) => url.normalized_url.clone(),
        Err((raw, _)) => raw.to_string(),
    });

    let mut adopted = None;
    let ignored_declared = match declared {
        None => None,
        Some(Err((_, rejection))) => Some(IgnoredDeclared::Unreadable(rejection)),
        Some(Ok(url)) => {
            let reached_by_redirect = candidate == Some(last.url);
            if last.url != start.url && !reached_by_redirect {
                Some(IgnoredDeclared::RedirectedAway)
            } else if urls::prepare(last.url).map(|u| u.host_key).ok() != Some(url.host_key) {
                Some(IgnoredDeclared::OtherHost)
            } else if !observed.host_trusted {
                Some(IgnoredDeclared::HostUntrusted)
            } else if is_top_page(&url.normalized_url) && !is_top_page(last.url) {
                Some(IgnoredDeclared::PointsToTopPage)
            } else if let Some(redirected) = candidate {
                (redirected != url.normalized_url).then_some(IgnoredDeclared::ConflictsWithRedirect)
            } else {
                adopted = Some(url.normalized_url);
                None
            }
        }
    };

    let (canonical_url, source) = match (candidate, adopted) {
        (Some(url), _) => (url.to_string(), Source::Redirect),
        (None, Some(url)) => (url, Source::Declared),
        (None, None) => (start.url.to_string(), Source::Normalized),
    };
    let relation = if canonical_url == start.url {
        Relation::Direct
    } else if source == Source::Redirect {
        Relation::Redirect
    } else {
        Relation::Declared
    };
    Some(Decision {
        canonical_url,
        source,
        relation,
        final_url: last.url.to_string(),
        declared_canonical_url,
        ignored_redirect,
        ignored_declared,
    })
}

/// ホストのトップ（パスが `/` か `/index.*`、クエリなし）
fn is_top_page(url: &str) -> bool {
    let Ok(url) = Url::parse(url) else {
        return false;
    };
    let path = url.path();
    url.query().is_none()
        && (path == "/"
            || path
                .strip_prefix("/index.")
                .is_some_and(|ext| !ext.contains('/')))
}

/// あるページが申告していた canonical。どちらも正規化した形
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Declaration<'a> {
    pub page: &'a str,
    pub declared: &'a str,
}

/// ホストの申告を信用しない理由
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Untrusted {
    #[error("{pages} ページがすべて {target} を指す")]
    SameTarget { target: String, pages: usize },
    /// 申告の先がさらに別の URL を指す（ループを含む）。正しい canonical は1段で着く
    #[error("{page} → {target} の先がさらに別の URL を指す")]
    Chain { page: String, target: String },
}

/// 同じホストの各ページの申告から、そのホストの申告を信用するかを決める（§8「不自然でないか」）
pub fn judge_host(declarations: &[Declaration<'_>]) -> Option<Untrusted> {
    let by_page: HashMap<&str, &str> = declarations.iter().map(|d| (d.page, d.declared)).collect();

    let mut pointing: Vec<&Declaration<'_>> = declarations
        .iter()
        .filter(|d| d.page != d.declared)
        .collect();
    pointing.sort_by_key(|d| d.page);
    for d in &pointing {
        if by_page
            .get(d.declared)
            .is_some_and(|next| *next != d.declared)
        {
            return Some(Untrusted::Chain {
                page: d.page.to_string(),
                target: d.declared.to_string(),
            });
        }
    }

    let target = pointing.first()?.declared;
    let all_same = by_page
        .iter()
        .filter(|(page, _)| **page != target)
        .all(|(_, declared)| *declared == target);
    (all_same && pointing.len() >= MIN_SAME_TARGET_PAGES).then(|| Untrusted::SameTarget {
        target: target.to_string(),
        pages: pointing.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CITY: &str = "https://www.city.example.jp";

    fn url(path: &str) -> String {
        format!("{CITY}{path}")
    }

    fn decide_with(hops: &[(&str, u16)], declared: Option<&str>) -> Decision {
        let hops: Vec<Hop<'_>> = hops
            .iter()
            .map(|&(url, status)| Hop { url, status })
            .collect();
        decide(&Observed {
            hops: &hops,
            declared,
            host_trusted: true,
        })
        .unwrap()
    }

    #[test]
    fn without_redirect_or_declaration_the_url_itself_is_canonical() {
        let a = url("/a.html");
        let d = decide_with(&[(&a, 200)], None);
        assert_eq!(d.canonical_url, a);
        assert_eq!(d.source, Source::Normalized);
        assert_eq!(d.relation, Relation::Direct);
        assert_eq!(d.final_url, a);
    }

    #[test]
    fn permanent_redirects_decide_the_canonical() {
        let (a, b) = (url("/a.html"), url("/b.html"));
        for status in [301, 308] {
            let d = decide_with(&[(&a, status), (&b, 200)], None);
            assert_eq!(d.canonical_url, b);
            assert_eq!(d.source, Source::Redirect);
            assert_eq!(d.relation, Relation::Redirect);
        }
    }

    #[test]
    fn temporary_redirects_do_not_change_the_canonical() {
        let (a, b) = (url("/a.html"), url("/b.html"));
        for status in [302, 303, 307] {
            let d = decide_with(&[(&a, status), (&b, 200)], None);
            assert_eq!(d.canonical_url, a);
            assert_eq!(d.source, Source::Normalized);
            assert_eq!(d.final_url, b, "転送先は記録する");
        }
    }

    #[test]
    fn permanent_redirects_are_followed_only_until_a_temporary_one() {
        let (a, b, c) = (url("/a.html"), url("/b.html"), url("/c.html"));
        let d = decide_with(&[(&a, 301), (&b, 302), (&c, 200)], None);
        assert_eq!(d.canonical_url, b);
        assert_eq!(d.final_url, c);
    }

    #[test]
    fn redirects_to_the_top_page_are_not_candidates() {
        let a = url("/kosodate/a.html");
        for top in [url("/"), url("/index.html"), url("/index.php")] {
            let d = decide_with(&[(&a, 301), (&top, 200)], Some(&top));
            assert_eq!(d.canonical_url, a, "{top}");
            assert_eq!(d.ignored_redirect, Some(IgnoredRedirect::ToTopPage));
            // トップの申告は取りに行った URL の申告ではない
            assert_eq!(d.ignored_declared, Some(IgnoredDeclared::RedirectedAway));
        }
        // トップの下のディレクトリの index はトップではない
        let section = url("/kosodate/index.html");
        assert_eq!(
            decide_with(&[(&a, 301), (&section, 200)], None).canonical_url,
            section
        );
        // 元がトップなら、トップへの転送も候補にする
        let (http_top, top) = (url("/index.htm"), url("/"));
        assert_eq!(
            decide_with(&[(&http_top, 301), (&top, 200)], None).canonical_url,
            top
        );
    }

    #[test]
    fn a_valid_declaration_is_adopted() {
        let (a, b) = (url("/a/"), url("/a/index.html"));
        let d = decide_with(&[(&b, 200)], Some(&a));
        assert_eq!(d.canonical_url, a);
        assert_eq!(d.source, Source::Declared);
        assert_eq!(d.relation, Relation::Declared);
        assert_eq!(d.ignored_declared, None);

        // 自分を指す申告も採る（結び方は direct）
        let d = decide_with(&[(&a, 200)], Some(&a));
        assert_eq!(d.source, Source::Declared);
        assert_eq!(d.relation, Relation::Direct);
    }

    #[test]
    fn declarations_are_compared_after_normalization() {
        let a = url("/a.html");
        let d = decide_with(&[(&a, 200)], Some("HTTPS://WWW.City.Example.JP/a.html#top"));
        assert_eq!(d.canonical_url, a);
        assert_eq!(d.declared_canonical_url.as_deref(), Some(a.as_str()));
    }

    #[test]
    fn invalid_declarations_are_ignored_with_a_reason() {
        let a = url("/a.html");
        let d = decide_with(&[(&a, 200)], Some("https://www.pref.example.jp/a.html"));
        assert_eq!(d.canonical_url, a);
        assert_eq!(d.ignored_declared, Some(IgnoredDeclared::OtherHost));

        let d = decide_with(&[(&a, 200)], Some("ftp://www.city.example.jp/a.html"));
        assert!(matches!(
            d.ignored_declared,
            Some(IgnoredDeclared::Unreadable(Rejection::Scheme { .. }))
        ));
        assert_eq!(
            d.declared_canonical_url.as_deref(),
            Some("ftp://www.city.example.jp/a.html")
        );

        // http と https は別のホスト（host_key が違う）
        let d = decide_with(&[(&a, 200)], Some("http://www.city.example.jp/a.html"));
        assert_eq!(d.ignored_declared, Some(IgnoredDeclared::OtherHost));
    }

    #[test]
    fn declarations_pointing_to_the_top_page_are_ignored() {
        let a = url("/kosodate/a.html");
        let d = decide_with(&[(&a, 200)], Some(&url("/")));
        assert_eq!(d.canonical_url, a);
        assert_eq!(d.ignored_declared, Some(IgnoredDeclared::PointsToTopPage));

        // トップ自身がトップを指すのは正しい
        let top = url("/index.html");
        let d = decide_with(&[(&top, 200)], Some(&url("/")));
        assert_eq!(d.canonical_url, url("/"));
    }

    #[test]
    fn permanent_redirects_win_over_declarations() {
        let (a, b, c) = (url("/a.html"), url("/b.html"), url("/c.html"));
        let d = decide_with(&[(&a, 301), (&b, 200)], Some(&c));
        assert_eq!(d.canonical_url, b);
        assert_eq!(d.source, Source::Redirect);
        assert_eq!(
            d.ignored_declared,
            Some(IgnoredDeclared::ConflictsWithRedirect)
        );
        assert_eq!(d.declared_canonical_url.as_deref(), Some(c.as_str()));

        // 転送先と同じ申告は食い違いではない
        let d = decide_with(&[(&a, 301), (&b, 200)], Some(&b));
        assert_eq!(d.ignored_declared, None);
    }

    #[test]
    fn declarations_after_a_temporary_redirect_are_ignored() {
        let (a, b) = (url("/a.html"), url("/b.html"));
        let d = decide_with(&[(&a, 302), (&b, 200)], Some(&b));
        assert_eq!(d.canonical_url, a);
        assert_eq!(d.ignored_declared, Some(IgnoredDeclared::RedirectedAway));
    }

    #[test]
    fn untrusted_hosts_have_their_declarations_ignored() {
        let (a, b) = (url("/a/index.html"), url("/a/"));
        let hops = [Hop {
            url: &a,
            status: 200,
        }];
        let d = decide(&Observed {
            hops: &hops,
            declared: Some(&b),
            host_trusted: false,
        })
        .unwrap();
        assert_eq!(d.canonical_url, a);
        assert_eq!(d.ignored_declared, Some(IgnoredDeclared::HostUntrusted));
    }

    #[test]
    fn site_prefixes_stay_in_the_canonical() {
        // `/smph/` を畳むのは dedup_key だけ。代表 URL は実際に開ける URL
        let smph = "https://www.city.koganei.lg.jp/smph/kosodate/a.html";
        let d = decide_with(&[(smph, 200)], None);
        assert_eq!(d.canonical_url, smph);
    }

    #[test]
    fn statuses_decide_whether_to_touch_the_resource() {
        for status in [200, 204, 404, 410] {
            assert_eq!(action(status), Action::Link, "{status}");
        }
        assert_eq!(action(304), Action::KeepPrevious);
        for status in [401, 403, 429, 500, 503] {
            assert_eq!(action(status), Action::Skip, "{status}");
        }
    }

    fn judge(pairs: &[(&str, &str)]) -> Option<Untrusted> {
        let pairs: Vec<(String, String)> = pairs.iter().map(|(p, d)| (url(p), url(d))).collect();
        let declarations: Vec<Declaration<'_>> = pairs
            .iter()
            .map(|(page, declared)| Declaration { page, declared })
            .collect();
        judge_host(&declarations)
    }

    #[test]
    fn hosts_where_every_page_points_to_one_url_are_untrusted() {
        assert_eq!(
            judge(&[
                ("/a.html", "/kosodate/"),
                ("/b.html", "/kosodate/"),
                ("/c.html", "/kosodate/"),
                // 指されている先が自分を指すのは数えない
                ("/kosodate/", "/kosodate/"),
            ]),
            Some(Untrusted::SameTarget {
                target: url("/kosodate/"),
                pages: 3
            })
        );
    }

    #[test]
    fn two_pages_pointing_to_one_url_are_trusted() {
        assert_eq!(
            judge(&[("/a/index.html", "/a/"), ("/a/default.html", "/a/")]),
            None
        );
    }

    #[test]
    fn self_declaring_pages_break_the_same_target_rule() {
        assert_eq!(
            judge(&[
                ("/a.html", "/x/"),
                ("/b.html", "/x/"),
                ("/c.html", "/x/"),
                ("/d.html", "/d.html"),
            ]),
            None
        );
        assert_eq!(
            judge(&[("/a.html", "/a.html"), ("/b.html", "/b.html")]),
            None
        );
        assert_eq!(judge(&[]), None);
    }

    #[test]
    fn loops_and_chains_make_the_host_untrusted() {
        assert!(matches!(
            judge(&[("/a.html", "/b.html"), ("/b.html", "/a.html")]),
            Some(Untrusted::Chain { .. })
        ));
        assert_eq!(
            judge(&[("/a.html", "/b.html"), ("/b.html", "/c.html")]),
            Some(Untrusted::Chain {
                page: url("/a.html"),
                target: url("/b.html")
            })
        );
        // 先が自分を指していれば連鎖ではない
        assert_eq!(
            judge(&[("/a.html", "/b.html"), ("/b.html", "/b.html")]),
            None
        );
    }
}
