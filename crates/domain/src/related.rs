//! レジストリの行から、関連 URL の候補を取り出す。
//!
//! 主たる URL が死んだ制度に、押せるリンクを出すための材料にする。後継のページは当てない。
//!
//! ```text
//! 値を集める → URL を切り出す → 連結を分ける → 末尾の記号を落とす → 解析（prepare）
//!   → 重複を飛ばす → 除外の判定 → 3件まで
//! ```

use std::collections::BTreeSet;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use crate::urls::{PreparedUrl, in_scope, prepare, split_joined};

/// 1制度あたりの関連 URL の上限
pub const MAX_RELATED: usize = 3;

/// 候補に残す拡張子（小文字）。拡張子が無い URL とディレクトリは別に通す。
///
/// 仮置き: 主たる URL と同じホストの URL のべ4,794件のうち `html` が2,211件・`pdf` が2,155件
/// （2026-10-09 の実測）。ファイルは HTML のようにリンク先として読めないので外す。
const HTML_EXTENSIONS: [&str; 2] = ["html", "htm"];

/// 問い合わせフォームなどの CGI を置くパス。
///
/// 仮置き: 同じホストの URL のうち該当は2件だけ（2026-10-09 の実測）。見つけたら増やす。
const FORM_PATH: &str = "/cgi-bin/";

/// 切り出した値の末尾から落とす記号。文章中の句読点や括弧が URL に付いてくる分
const TRAILING_MARKS: [char; 7] = ['.', ',', ';', ':', '(', ')', '\''];

/// URL の始まりから、URL に使える ASCII 文字が続くところまで。
/// 全角文字・空白・引用符で終わる。
static URL_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"https?://[A-Za-z0-9\-._~:/?#\[\]@!$&'()*+,;=%]+").expect("URL の正規表現")
});

/// 候補にした関連 URL 1件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// 1 から。カンマ連結の後続 → relatedLink → description の出現順
    pub rank: i32,
    pub url: PreparedUrl,
}

/// 候補にしなかった理由
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Reason {
    /// `urls::prepare` が通らない
    Unreadable,
    /// 主たる URL とホスト名が違う（scheme の違いは見ない）
    OtherHost,
    /// ホスト名は同じだが `host_key` が許可リストに無い
    NotAllowed,
    /// 主たる URL と同じ `dedup_key`
    SameAsMain,
    /// 拡張子が `html`・`htm`・無し・ディレクトリ以外
    NotHtml,
    /// パスに `/cgi-bin/` を含む
    Form,
    /// 4件目以降
    OverLimit,
}

impl Reason {
    /// 件数の表示に使う名前
    pub fn label(self) -> &'static str {
        match self {
            Reason::Unreadable => "読めない",
            Reason::OtherHost => "別ホスト",
            Reason::NotAllowed => "許可リスト外",
            Reason::SameAsMain => "主たる URL と同じ",
            Reason::NotHtml => "HTML 以外",
            Reason::Form => "フォーム",
            Reason::OverLimit => "上限超え",
        }
    }
}

/// 1行から取り出した結果。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Related {
    pub candidates: Vec<Candidate>,
    /// 落とした値（切り出した後の表記）と理由
    pub dropped: Vec<(String, Reason)>,
}

/// 行から関連 URL の候補を最大 `MAX_RELATED` 件取り出す。
///
/// 値の順は、主たる URL のカンマ連結の後続 → `relatedLink[].uri` → `description`。
/// 同じ `dedup_key` は最初の1件だけを見て、あとは理由も付けずに飛ばす
/// （`relatedLink` と `description` に同じ URL があるのは普通のため）。
pub fn related(registry: &Value, main: &PreparedUrl, allowed_hosts: &BTreeSet<String>) -> Related {
    let main_host = hostname(&main.host_key);
    let mut out = Related::default();
    let mut seen: BTreeSet<String> = BTreeSet::new();

    for text in source_values(registry) {
        for raw in extract_urls(&text) {
            let Ok(url) = prepare(&raw) else {
                out.dropped.push((raw, Reason::Unreadable));
                continue;
            };
            if !seen.insert(url.dedup_key.clone()) {
                continue;
            }
            let reason = if hostname(&url.host_key) != main_host {
                Some(Reason::OtherHost)
            } else if in_scope(&url, allowed_hosts).is_err() {
                Some(Reason::NotAllowed)
            } else if url.dedup_key == main.dedup_key {
                Some(Reason::SameAsMain)
            } else if !is_html_like(&url.normalized_url) {
                Some(Reason::NotHtml)
            } else if is_form(&url.normalized_url) {
                Some(Reason::Form)
            } else if out.candidates.len() >= MAX_RELATED {
                Some(Reason::OverLimit)
            } else {
                None
            };
            match reason {
                Some(reason) => out.dropped.push((raw, reason)),
                None => out.candidates.push(Candidate {
                    rank: out.candidates.len() as i32 + 1,
                    url,
                }),
            }
        }
    }
    out
}

/// URL を探す文字列を、出現順に集める。`relatedLink[].title` は見ない。
fn source_values(registry: &Value) -> Vec<String> {
    let mut values = Vec::new();
    if let Some(joined) = registry
        .pointer("/localGovernmentLink/uri")
        .and_then(Value::as_str)
    {
        values.extend(split_joined(joined).into_iter().skip(1).map(str::to_string));
    }
    if let Some(links) = registry.get("relatedLink").and_then(Value::as_array) {
        values.extend(
            links
                .iter()
                .filter_map(|link| link.get("uri").and_then(Value::as_str))
                .map(str::to_string),
        );
    }
    if let Some(description) = registry.get("description").and_then(Value::as_str) {
        values.push(description.to_string());
    }
    values
}

/// 文字列から URL を切り出す。連結された分は分け、末尾の記号は落とす。
fn extract_urls(text: &str) -> Vec<String> {
    URL_PATTERN
        .find_iter(text)
        .flat_map(|found| split_at_joins(found.as_str()))
        .map(|part| part.trim_end_matches(TRAILING_MARKS).to_string())
        .collect()
}

/// `,http://` `,https://` `;http://` `;https://` の直前で分ける。
fn split_at_joins(value: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut rest = value;
    while let Some(cut) = rest.match_indices([',', ';']).find_map(|(i, _)| {
        let after = &rest[i + 1..];
        (after.starts_with("http://") || after.starts_with("https://")).then_some(i)
    }) {
        parts.push(&rest[..cut]);
        rest = &rest[cut + 1..];
    }
    parts.push(rest);
    parts
}

/// `host_key`（`scheme://host`）のホスト名の部分
fn hostname(host_key: &str) -> &str {
    host_key
        .split_once("://")
        .map_or(host_key, |(_, host)| host)
}

/// 正規化後のパスが、HTML・拡張子なし・ディレクトリのどれかか。
fn is_html_like(normalized_url: &str) -> bool {
    let path = path_of(normalized_url);
    let last = path.rsplit('/').next().unwrap_or("");
    // 末尾が `/` なら最後の区切りの後ろは空で、ディレクトリになる
    match last.rsplit_once('.') {
        None => true,
        Some((_, extension)) => HTML_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str()),
    }
}

fn is_form(normalized_url: &str) -> bool {
    path_of(normalized_url).contains(FORM_PATH)
}

/// 正規化後の URL のパス。`prepare` を通っているので解析できる。
fn path_of(normalized_url: &str) -> &str {
    let after_scheme = normalized_url
        .split_once("://")
        .map_or(normalized_url, |(_, rest)| rest);
    let from_path = after_scheme.find('/').map_or("", |i| &after_scheme[i..]);
    from_path.split(['?', '#']).next().unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const CITY: &str = "https://www.city.example.jp";

    fn main_url(raw: &str) -> PreparedUrl {
        prepare(raw).unwrap()
    }

    fn allowed(hosts: &[&str]) -> BTreeSet<String> {
        hosts.iter().map(|h| h.to_string()).collect()
    }

    fn urls(r: &Related) -> Vec<&str> {
        r.candidates
            .iter()
            .map(|c| c.url.normalized_url.as_str())
            .collect()
    }

    fn fixture_row(index: usize) -> Value {
        let rows: Value =
            serde_json::from_str(include_str!("../tests/fixtures/registry_sample.json")).unwrap();
        rows[index].clone()
    }

    #[test]
    fn child_allowance_row_yields_the_page_in_description() {
        // 設計の完了条件: 児童手当の行で teatekaisei.html が候補に出る
        let row = fixture_row(0);
        let main = main_url(row["localGovernmentLink"]["uri"].as_str().unwrap());
        let got = related(&row, &main, &allowed(&[CITY]));
        assert_eq!(
            urls(&got),
            [
                "https://www.city.example.jp/smph/kosodatekyoiku/N84/kakusyuteate/jidoteate/teatekaisei.html",
            ]
        );
        assert_eq!(got.candidates[0].rank, 1);
    }

    #[test]
    fn birth_registration_row_keeps_three_and_explains_the_rest() {
        let row = fixture_row(1);
        let main = main_url(row["localGovernmentLink"]["uri"].as_str().unwrap());
        let got = related(&row, &main, &allowed(&[CITY]));
        assert_eq!(
            urls(&got),
            [
                "https://www.city.example.jp/smph/kurashi/410/zyumininkankoseki/413/shimin29.html",
                "https://www.city.example.jp/smph/kosodatekyoiku/N84/iryohi/yoji_iryo/kosodateshien08.html",
                "https://www.city.example.jp/smph/kosodatekyoiku/N84/kakusyuteate/jidoteate/jidouteateseidogaiyo.html",
            ]
        );
        assert_eq!(
            got.candidates.iter().map(|c| c.rank).collect::<Vec<_>>(),
            [1, 2, 3]
        );
        let reasons: Vec<Reason> = got.dropped.iter().map(|(_, r)| *r).collect();
        // 法務省（relatedLink と description に同じ URL。1回だけ数える）・4件目以降の3件・PDF
        assert_eq!(
            reasons.iter().filter(|r| **r == Reason::OtherHost).count(),
            1
        );
        assert_eq!(
            reasons.iter().filter(|r| **r == Reason::OverLimit).count(),
            3
        );
        assert_eq!(reasons.iter().filter(|r| **r == Reason::NotHtml).count(), 1);
        assert_eq!(got.dropped.len(), 5);
    }

    #[test]
    fn row_with_only_files_and_other_hosts_has_no_candidates() {
        let row = fixture_row(2);
        let main = main_url(row["localGovernmentLink"]["uri"].as_str().unwrap());
        let got = related(
            &row,
            &main,
            &allowed(&["https://www.fukushi.metro.tokyo.lg.jp"]),
        );
        assert!(got.candidates.is_empty(), "{:?}", got.candidates);
    }

    #[test]
    fn order_is_joined_rest_then_related_link_then_description() {
        let row = json!({
            "localGovernmentLink": {"uri": "https://www.city.example.jp/main.html,https://www.city.example.jp/second.html"},
            "relatedLink": [{"uri": "https://www.city.example.jp/link.html", "title": "https://www.city.example.jp/title.html"}],
            "description": "詳しくは https://www.city.example.jp/desc.html をご覧ください",
        });
        let got = related(
            &row,
            &main_url("https://www.city.example.jp/main.html"),
            &allowed(&[CITY]),
        );
        assert_eq!(
            urls(&got),
            [
                "https://www.city.example.jp/second.html",
                "https://www.city.example.jp/link.html",
                "https://www.city.example.jp/desc.html",
            ]
        );
    }

    #[test]
    fn joined_values_and_trailing_marks_are_cut() {
        let row = json!({"description":
            "https://www.city.example.jp/a.html;https://www.city.example.jp/b.html, \
             https://www.city.example.jp/c.htmlをご覧ください。（https://www.city.example.jp/d.html）"});
        let got = related(
            &row,
            &main_url("https://www.city.example.jp/main.html"),
            &allowed(&[CITY]),
        );
        assert_eq!(
            urls(&got),
            [
                "https://www.city.example.jp/a.html",
                "https://www.city.example.jp/b.html",
                "https://www.city.example.jp/c.html",
            ]
        );
        assert_eq!(
            got.dropped,
            [(
                "https://www.city.example.jp/d.html".to_string(),
                Reason::OverLimit
            )]
        );
    }

    #[test]
    fn variants_of_the_main_url_are_not_related() {
        // フラグメント違いと、小金井市の /smph/ 違い（dedup_key が同じ）
        let main = main_url("https://www.city.koganei.lg.jp/smph/kosodate/teate.html");
        let row = json!({"description":
            "https://www.city.koganei.lg.jp/smph/kosodate/teate.html#p2 https://www.city.koganei.lg.jp/kosodate/teate.html"});
        let got = related(&row, &main, &allowed(&["https://www.city.koganei.lg.jp"]));
        assert!(got.candidates.is_empty());
        // 1件目で SameAsMain、2件目は同じ dedup_key なので黙って飛ばす
        assert_eq!(
            got.dropped.iter().map(|(_, r)| *r).collect::<Vec<_>>(),
            [Reason::SameAsMain]
        );
    }

    #[test]
    fn extensions_and_forms() {
        let row = json!({"description": "\
            https://www.city.example.jp/a.pdf https://www.city.example.jp/b.XLSX https://www.city.example.jp/c.jpg \
            https://www.city.example.jp/cgi-bin/form_enq/formmail.cgi?d=x https://www.city.example.jp/cgi-bin/enquetes/10d7 \
            https://www.city.example.jp/dir/ https://www.city.example.jp/index.cfm/41,128249,310,1987,html https://www.city.example.jp/page.htm"});
        let got = related(
            &row,
            &main_url("https://www.city.example.jp/main.html"),
            &allowed(&[CITY]),
        );
        assert_eq!(
            urls(&got),
            [
                "https://www.city.example.jp/dir/",
                "https://www.city.example.jp/index.cfm/41,128249,310,1987,html",
                "https://www.city.example.jp/page.htm",
            ]
        );
        assert_eq!(
            got.dropped.iter().map(|(_, r)| *r).collect::<Vec<_>>(),
            [
                Reason::NotHtml,
                Reason::NotHtml,
                Reason::NotHtml,
                Reason::NotHtml,
                Reason::Form
            ]
        );
    }

    #[test]
    fn same_hostname_on_a_host_key_outside_the_allow_list() {
        // 主たる URL は https。http の側は許可リストに無い
        let row = json!({"description": "http://www.city.example.jp/a.html https://kosodate.city.example.jp/b.html"});
        let got = related(
            &row,
            &main_url("https://www.city.example.jp/main.html"),
            &allowed(&[CITY]),
        );
        assert!(got.candidates.is_empty());
        assert_eq!(
            got.dropped.iter().map(|(_, r)| *r).collect::<Vec<_>>(),
            [Reason::NotAllowed, Reason::OtherHost]
        );
    }

    #[test]
    fn broken_rows_do_not_panic() {
        let main = main_url("https://www.city.example.jp/main.html");
        for row in [
            json!({}),
            json!({"relatedLink": null, "description": null, "localGovernmentLink": null}),
            json!({"relatedLink": "https://www.city.example.jp/a.html", "description": 3}),
            json!({"relatedLink": [null, {"uri": null}, {"title": "x"}, 1]}),
        ] {
            assert_eq!(related(&row, &main, &allowed(&[CITY])), Related::default());
        }
        let row =
            json!({"description": "https://www.city.example.jp:8443/a.html https:// http://[::1"});
        let got = related(&row, &main, &allowed(&[CITY]));
        assert!(got.candidates.is_empty());
        assert!(
            got.dropped.iter().all(|(_, r)| *r == Reason::Unreadable),
            "{:?}",
            got.dropped
        );
    }
}
