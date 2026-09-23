//! 取得した HTML から本文を取り出し、変化を比べるためのハッシュを作る（§16 §19 §20）。
//!
//! ```text
//! HTML → 本文コンテナの抽出 → script/style/nav/header/footer/コメント除去
//!      → 空白・Unicode（NFKC）正規化 → ハッシュ
//! ```

use std::collections::BTreeSet;
use std::sync::LazyLock;

use chrono::NaiveDate;
use regex::Regex;
use scraper::node::{Element, Node};
use scraper::{ElementRef, Html};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;
use url::Url;

use crate::urls;

/// 抽出の規則・除去する要素・正規化のどれかを変えたら上げる。
/// 規則の変更とサイトの変更を区別するため、ハッシュと一緒に残す
pub const EXTRACTOR_VERSION: i32 = 1;

/// 画面に出ない要素。本文からもページ全体からも除く
const HIDDEN: [&str; 4] = ["script", "style", "noscript", "template"];

/// 本文から除く、ページの枠の要素（§16）
const CHROME: [&str; 3] = ["nav", "header", "footer"];

/// 前後で行を分ける要素。インライン要素（`<b>` など）では分けない（「子<b>育</b>て」を割らないため）
const BLOCKS: [&str; 39] = [
    "address",
    "article",
    "aside",
    "blockquote",
    "br",
    "caption",
    "dd",
    "details",
    "dialog",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hr",
    "li",
    "main",
    "nav",
    "ol",
    "p",
    "pre",
    "section",
    "summary",
    "table",
    "td",
    "th",
    "tr",
    "ul",
    "option",
];

/// 本文コンテナを決めた規則（§16 の優先順）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    Main,
    RoleMain,
    IdMain,
    Content,
    Honbun,
    Article,
    /// どれにも当たらないときの最後の手段。`<body>` から aside も除く
    Body,
}

impl Rule {
    /// 上から順に試す
    const ORDER: [Rule; 6] = [
        Rule::Main,
        Rule::RoleMain,
        Rule::IdMain,
        Rule::Content,
        Rule::Honbun,
        Rule::Article,
    ];

    /// `resources.extractor_rule` に残す値
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::RoleMain => "role_main",
            Self::IdMain => "id_main",
            Self::Content => "content",
            Self::Honbun => "honbun",
            Self::Article => "article",
            Self::Body => "body",
        }
    }

    fn matches(self, element: &Element) -> bool {
        let contains = |attr: &str, word: &str| {
            element
                .attr(attr)
                .is_some_and(|value| value.to_ascii_lowercase().contains(word))
        };
        match self {
            Self::Main => element.name() == "main",
            Self::RoleMain => element
                .attr("role")
                .is_some_and(|role| role.trim().eq_ignore_ascii_case("main")),
            Self::IdMain => contains("id", "main"),
            Self::Content => contains("id", "content") || contains("class", "content"),
            Self::Honbun => contains("id", "honbun") || contains("class", "honbun"),
            Self::Article => element.name() == "article",
            Self::Body => element.name() == "body",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extracted {
    pub rule: Rule,
    /// `<title>`（正規化済み・1行）
    pub title: Option<String>,
    /// 本文コンテナのテキスト（正規化済み・ブロックごとに1行）。保存はしない（§28-4）
    pub body_text: String,
    /// ページに書かれた更新日（`Last-Modified` ではない）
    pub page_updated_on: Option<NaiveDate>,
    /// `<link rel="canonical">` を絶対 URL にしたもの。採用するかは検証の後（§8）
    pub declared_canonical_url: Option<String>,
    /// `<meta name="robots">` の値（小文字）
    pub robots_meta: Option<String>,
    /// 本文コンテナの中のリンク（正規化済み・重複なし・並べ替え済み）。scope では絞っていない
    pub links: Vec<String>,
    pub hashes: Hashes,
}

/// 5種のうち、HTML から作る4種。`raw_hash` は受信したバイト列から取得の側で作る
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hashes {
    /// ノイズ（script・style・コメントなど）を除いたページ全体
    pub page: String,
    pub title: Option<String>,
    /// 本文。制度の鮮度判定に使う主ハッシュ
    pub body: String,
    pub links: String,
}

/// SHA-256 の16進（小文字）
pub fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `url` は最終 URL（相対のリンクと canonical の基準）
pub fn extract(html: &str, url: &str) -> Extracted {
    let document = Html::parse_document(html);
    let root = document.root_element();
    let body = first(root, |e| e.name() == "body").unwrap_or(root);
    let base = base_url(root, url);

    let (rule, container) = container(body);
    let chrome: &[&str] = if rule == Rule::Body {
        &["nav", "header", "footer", "aside"]
    } else {
        &CHROME
    };
    let mut links = BTreeSet::new();
    let body_text = normalize(&text(
        container,
        &|e| is(e, &HIDDEN) || is(e, chrome),
        &mut |a| {
            if let Some(link) = resolve_link(a, base.as_ref()) {
                links.insert(link);
            }
        },
    ));
    let page_text = normalize(&text(body, &|e| is(e, &HIDDEN), &mut |_| {}));

    let title = first(root, |e| e.name() == "title")
        .map(|title| one_line(&normalize(&title.text().collect::<String>())))
        .filter(|title| !title.is_empty());
    let links: Vec<String> = links.into_iter().collect();

    Extracted {
        rule,
        hashes: Hashes {
            page: digest(page_text.as_bytes()),
            title: title.as_deref().map(|t| digest(t.as_bytes())),
            body: digest(body_text.as_bytes()),
            links: digest(links.join("\n").as_bytes()),
        },
        title,
        page_updated_on: updated_on(&page_text),
        declared_canonical_url: canonical(root, base.as_ref()),
        robots_meta: robots_meta(root),
        links,
        body_text,
    }
}

/// 規則を上から試し、最初に見つかった要素を本文コンテナにする。
///
/// 同じ規則に当たる要素が複数あれば文書順で最初（入れ子なら外側）。除去する要素（nav など）と
/// その中は候補にしない（取り出した後で除くと空になるため）
fn container(body: ElementRef<'_>) -> (Rule, ElementRef<'_>) {
    let mut candidates = Vec::new();
    collect(body, &mut candidates);
    Rule::ORDER
        .into_iter()
        .find_map(|rule| {
            candidates
                .iter()
                .find(|e| rule.matches(e.value()))
                .map(|e| (rule, *e))
        })
        .unwrap_or((Rule::Body, body))
}

fn collect<'a>(parent: ElementRef<'a>, out: &mut Vec<ElementRef<'a>>) {
    for child in parent.children().filter_map(ElementRef::wrap) {
        if is(child.value(), &HIDDEN) || is(child.value(), &CHROME) {
            continue;
        }
        out.push(child);
        collect(child, out);
    }
}

fn is(element: &Element, names: &[&str]) -> bool {
    names.contains(&element.name())
}

fn first<'a>(root: ElementRef<'a>, pred: impl Fn(&Element) -> bool) -> Option<ElementRef<'a>> {
    root.descendants()
        .filter_map(ElementRef::wrap)
        .find(|e| pred(e.value()))
}

/// テキストを集める。`skip` に当たる要素の中とコメントは読まない。`<a>` は `on_link` にも渡す
fn text(
    root: ElementRef<'_>,
    skip: &dyn Fn(&Element) -> bool,
    on_link: &mut dyn FnMut(&Element),
) -> String {
    fn walk(
        node: ego_tree::NodeRef<'_, Node>,
        skip: &dyn Fn(&Element) -> bool,
        on_link: &mut dyn FnMut(&Element),
        out: &mut String,
    ) {
        for child in node.children() {
            match child.value() {
                Node::Text(text) => out.push_str(text),
                Node::Element(element) => {
                    if skip(element) {
                        continue;
                    }
                    if element.name() == "a" {
                        on_link(element);
                    }
                    let block = is(element, &BLOCKS);
                    if block {
                        out.push('\n');
                    }
                    walk(child, skip, on_link, out);
                    if block {
                        out.push('\n');
                    }
                }
                _ => {}
            }
        }
    }
    let mut out = String::new();
    walk(*root, skip, on_link, &mut out);
    out
}

/// NFKC にし、行ごとに空白を1つに畳んで、空行を捨てる
fn normalize(text: &str) -> String {
    let text: String = text.nfkc().collect();
    text.lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 相対 URL の基準。`<base href>` があればそれ、無ければ最終 URL
fn base_url(root: ElementRef<'_>, url: &str) -> Option<Url> {
    let page = Url::parse(url).ok()?;
    let base = first(root, |e| e.name() == "base" && e.attr("href").is_some())
        .and_then(|e| page.join(e.value().attr("href")?.trim()).ok());
    Some(base.unwrap_or(page))
}

fn resolve_link(a: &Element, base: Option<&Url>) -> Option<String> {
    let href = a.attr("href")?.trim();
    let joined = base?.join(href).ok()?;
    // mailto・javascript などは prepare が scheme で落とす
    urls::prepare(joined.as_str())
        .ok()
        .map(|url| url.normalized_url)
}

/// 記録だけなので正規化はしない
fn canonical(root: ElementRef<'_>, base: Option<&Url>) -> Option<String> {
    let link = first(root, |e| {
        e.name() == "link"
            && e.attr("href").is_some()
            && e.attr("rel").is_some_and(|rel| {
                rel.split_ascii_whitespace()
                    .any(|token| token.eq_ignore_ascii_case("canonical"))
            })
    })?;
    let href = link.value().attr("href")?.trim();
    if href.is_empty() {
        return None;
    }
    // 解決できない値も捨てずに残す（検証で「URL として正常でない」と分かるように）
    Some(
        base.and_then(|base| base.join(href).ok())
            .map_or_else(|| href.to_string(), String::from),
    )
}

fn robots_meta(root: ElementRef<'_>) -> Option<String> {
    let meta = first(root, |e| {
        e.name() == "meta"
            && e.attr("name")
                .is_some_and(|name| name.trim().eq_ignore_ascii_case("robots"))
    })?;
    let content = meta.value().attr("content")?.trim().to_ascii_lowercase();
    (!content.is_empty()).then_some(content)
}

/// 日付の書式（捕まえない形）。NFKC と空白の正規化の後なので、数字・記号は半角で、改行はまたがない
const DATE: &str = r"(?:\d{4} *年 *\d{1,2} *月 *\d{1,2} *日|\d{4} *[/.\-] *\d{1,2} *[/.\-] *\d{1,2}|(?:令和|平成) *(?:\d{1,2}|元) *年 *\d{1,2} *月 *\d{1,2} *日)";

/// 見出しと日付の間に挟まる記号（`更新日:` `【更新日】` `(更新日)` など）。
/// 見出しと日付が別の行（`<dt>` と `<dd>` など）に分かれていてもよい
const GAP: &str = r"[\s:\]】)>]*";

/// 更新を指す書き方。「更新」だけの語は日付に隣り合うときだけ見る（「免許の更新」などを拾わない）
static UPDATED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?:最終更新日|更新日)時?{GAP}{DATE}|{DATE} *(?:最終)?更新"
    ))
    .expect("正しい正規表現")
});

/// 更新日が無いときに使う書き方
static PUBLISHED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?:掲載日|公開日|作成日|登録日)時?{GAP}{DATE}|{DATE} *(?:掲載|公開|作成)"
    ))
    .expect("正しい正規表現")
});

static DATE_PARTS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?:(?P<era>令和|平成) *(?P<ey>\d{1,2}|元)|(?P<y>\d{4})) *[年/.\-] *(?P<m>\d{1,2}) *[月/.\-] *(?P<d>\d{1,2})",
    )
    .expect("正しい正規表現")
});

/// 日付として成り立たない値は飛ばして次を見る
fn updated_on(page_text: &str) -> Option<NaiveDate> {
    [&*UPDATED, &*PUBLISHED].into_iter().find_map(|pattern| {
        pattern
            .find_iter(page_text)
            .find_map(|found| parse_date(found.as_str()))
    })
}

fn parse_date(text: &str) -> Option<NaiveDate> {
    let parts = DATE_PARTS.captures(text)?;
    let number = |name: &str| parts.name(name)?.as_str().parse::<u32>().ok();
    let year = match parts.name("era") {
        Some(era) => {
            let offset = match era.as_str() {
                "令和" => 2018,
                _ => 1988,
            };
            let year = match parts.name("ey")?.as_str() {
                "元" => 1,
                other => other.parse::<i32>().ok()?,
            };
            offset + year
        }
        None => parts.name("y")?.as_str().parse().ok()?,
    };
    NaiveDate::from_ymd_opt(year, number("m")?, number("d")?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "https://www.city.example.lg.jp/kosodate/teate/index.html";

    fn page(head: &str, body: &str) -> String {
        format!("<!DOCTYPE html><html><head>{head}</head><body>{body}</body></html>")
    }

    fn rule_of(body: &str) -> (Rule, String) {
        let extracted = extract(&page("", body), URL);
        (extracted.rule, extracted.body_text)
    }

    /// 自治体サイトによくある枠。本文コンテナの規則に当たる要素は含まない
    fn chrome(inner: &str) -> String {
        format!(
            "<header><p>○○市</p><nav><ul><li>くらし</li><li>子育て</li></ul></nav></header>\
             {inner}\
             <aside><p>よく見られるページ</p></aside>\
             <footer><p>このページに関するお問い合わせ</p></footer>"
        )
    }

    #[test]
    fn rules_are_tried_in_order() {
        let (rule, text) = rule_of(&chrome("<main><p>児童手当</p></main>"));
        assert_eq!((rule, text.as_str()), (Rule::Main, "児童手当"));

        let (rule, text) = rule_of(&chrome(r#"<div role="main"><p>児童手当</p></div>"#));
        assert_eq!((rule, text.as_str()), (Rule::RoleMain, "児童手当"));

        let (rule, text) = rule_of(&chrome(r#"<div id="main_contents"><p>児童手当</p></div>"#));
        assert_eq!((rule, text.as_str()), (Rule::IdMain, "児童手当"));

        let (rule, text) = rule_of(&chrome(
            r#"<div class="page Content"><p>児童手当</p></div>"#,
        ));
        assert_eq!((rule, text.as_str()), (Rule::Content, "児童手当"));

        let (rule, text) = rule_of(&chrome(r#"<div id="honbun"><p>児童手当</p></div>"#));
        assert_eq!((rule, text.as_str()), (Rule::Honbun, "児童手当"));

        let (rule, text) = rule_of(&chrome("<article><p>児童手当</p></article>"));
        assert_eq!((rule, text.as_str()), (Rule::Article, "児童手当"));

        // どれも無ければ body から枠（aside を含む）を除く
        let (rule, text) = rule_of(&chrome("<div><p>児童手当</p></div>"));
        assert_eq!((rule, text.as_str()), (Rule::Body, "児童手当"));
    }

    #[test]
    fn earlier_rules_win_over_later_ones() {
        // 文書では content が先に出ても、main が優先する
        let body = r#"<div class="content"><p>お知らせ</p><main><p>児童手当</p></main></div>"#;
        assert_eq!(rule_of(body), (Rule::Main, "児童手当".to_string()));

        // 同じ規則なら文書順で最初（入れ子なら外側）
        let body = r#"<div id="contents"><div class="content-body"><p>児童手当</p></div><p>関連</p></div>"#;
        assert_eq!(rule_of(body), (Rule::Content, "児童手当\n関連".to_string()));
    }

    #[test]
    fn removed_elements_are_not_candidates() {
        // nav の id が main を含んでも本文コンテナにしない
        let body = chrome(
            r#"<nav id="mainmenu"><p>メニュー</p></nav><div class="content"><p>児童手当</p></div>"#,
        );
        assert_eq!(rule_of(&body), (Rule::Content, "児童手当".to_string()));
    }

    #[test]
    fn noise_is_removed_from_the_body() {
        let body = r#"<main>
            <header><h1>児童手当</h1></header>
            <p>支給額は<b>月額</b>1万円です。</p>
            <script>var token = "abc";</script>
            <style>p { color: red }</style>
            <noscript><img src="/track?t=1"></noscript>
            <!-- 更新: 2026-09-23 10:00 -->
            <footer>このページのトップへ</footer>
        </main>"#;
        assert_eq!(rule_of(body).1, "支給額は月額1万円です。");
    }

    #[test]
    fn body_hash_ignores_changes_outside_the_body() {
        let at = |time: &str, token: &str| {
            page(
                &format!(r#"<meta name="csrf" content="{token}"><title>児童手当</title>"#),
                &chrome(&format!(
                    "<p>現在時刻 {time}</p><main><p>児童手当を支給します。</p>\
                     <script>window.t = '{token}'</script><!-- {time} --></main>"
                )),
            )
        };
        let first = extract(&at("10:00", "a1"), URL);
        let second = extract(&at("10:05", "b2"), URL);
        assert_eq!(first.hashes.body, second.hashes.body);
        assert_eq!(first.hashes.title, second.hashes.title);
        // ページ全体は本文の外の時刻で変わる
        assert_ne!(first.hashes.page, second.hashes.page);

        let changed = extract(
            &page("", "<main><p>児童手当を支給しません。</p></main>"),
            URL,
        );
        assert_ne!(first.hashes.body, changed.hashes.body);
    }

    #[test]
    fn width_and_whitespace_do_not_change_the_hash() {
        let a = extract(&page("", "<main><p>月額　１０，０００円</p></main>"), URL);
        let b = extract(
            &page("", "<main>\n  <p>\n月額 10,000円\n  </p>\n</main>"),
            URL,
        );
        assert_eq!(a.body_text, "月額 10,000円");
        assert_eq!(a.hashes.body, b.hashes.body);
    }

    #[test]
    fn blocks_are_split_into_lines_but_inline_elements_are_not() {
        let body = "<main><h2>対象</h2><ul><li>0歳から</li><li>中学生まで</li></ul>\
                    <p>子<b>育</b>て<br>支援</p></main>";
        assert_eq!(rule_of(body).1, "対象\n0歳から\n中学生まで\n子育て\n支援");
    }

    #[test]
    fn title_is_one_normalized_line() {
        let html = page(
            "<title>\n  児童手当｜○○市　\n</title>",
            "<main><p>x</p></main>",
        );
        let extracted = extract(&html, URL);
        assert_eq!(extracted.title.as_deref(), Some("児童手当|○○市"));
        assert_eq!(
            extracted.hashes.title,
            Some(digest("児童手当|○○市".as_bytes()))
        );

        assert_eq!(
            extract(&page("<title> </title>", ""), URL).hashes.title,
            None
        );
    }

    #[test]
    fn declared_canonical_is_made_absolute() {
        let html = page(r#"<link rel="Canonical" href="../teate/">"#, "");
        assert_eq!(
            extract(&html, URL).declared_canonical_url.as_deref(),
            Some("https://www.city.example.lg.jp/kosodate/teate/")
        );
        // rel に複数の語があっても拾う。別ホストでも記録する（採るかどうかは検証で決める）
        let html = page(
            r#"<link rel="alternate canonical" href="https://www.other.example.jp/x.html">"#,
            "",
        );
        assert_eq!(
            extract(&html, URL).declared_canonical_url.as_deref(),
            Some("https://www.other.example.jp/x.html")
        );
        assert_eq!(extract(&page("", ""), URL).declared_canonical_url, None);
    }

    #[test]
    fn robots_meta_is_recorded() {
        let html = page(r#"<meta name="ROBOTS" content=" NoIndex, NoFollow ">"#, "");
        assert_eq!(
            extract(&html, URL).robots_meta.as_deref(),
            Some("noindex, nofollow")
        );
        assert_eq!(extract(&page("", ""), URL).robots_meta, None);
    }

    #[test]
    fn links_are_resolved_normalized_and_sorted() {
        let body = chrome(
            r##"<main>
                <a href="shinsei.html#form">申請</a>
                <a href="/kosodate/teate/shinsei.html">申請（同じ）</a>
                <a href="https://WWW.City.Example.lg.jp/kosodate/?utm_source=x">一覧</a>
                <a href="https://www.pref.example.jp/">県</a>
                <a href="mailto:kosodate@city.example.lg.jp">メール</a>
                <a href="javascript:void(0)">印刷</a>
                <a href="#top">ページの先頭</a>
                <a>リンクなし</a>
                <nav><a href="/sitemap.html">サイトマップ</a></nav>
            </main>"##,
        );
        let extracted = extract(&page("", &body), URL);
        assert_eq!(
            extracted.links,
            [
                "https://www.city.example.lg.jp/kosodate/",
                "https://www.city.example.lg.jp/kosodate/teate/index.html",
                "https://www.city.example.lg.jp/kosodate/teate/shinsei.html",
                "https://www.pref.example.jp/",
            ]
        );
        assert_eq!(
            extracted.hashes.links,
            digest(extracted.links.join("\n").as_bytes())
        );
    }

    #[test]
    fn base_href_is_used_for_links() {
        let html = page(
            r#"<base href="https://www.city.example.lg.jp/other/">"#,
            r#"<main><a href="a.html">a</a></main>"#,
        );
        assert_eq!(
            extract(&html, URL).links,
            ["https://www.city.example.lg.jp/other/a.html"]
        );
    }

    #[test]
    fn empty_link_set_still_has_a_hash() {
        let extracted = extract(&page("", "<main><p>x</p></main>"), URL);
        assert!(extracted.links.is_empty());
        assert_eq!(extracted.hashes.links, digest(b""));
    }

    fn date(y: i32, m: u32, d: u32) -> Option<NaiveDate> {
        NaiveDate::from_ymd_opt(y, m, d)
    }

    fn updated(body: &str) -> Option<NaiveDate> {
        extract(&page("", body), URL).page_updated_on
    }

    #[test]
    fn updated_on_reads_western_dates() {
        assert_eq!(
            updated("<main><p>x</p></main><p>更新日：2024年3月1日</p>"),
            date(2024, 3, 1)
        );
        assert_eq!(updated("<p>最終更新日 2024/03/01</p>"), date(2024, 3, 1));
        assert_eq!(
            updated("<p>【更新日】２０２４年１２月２５日</p>"),
            date(2024, 12, 25)
        );
        assert_eq!(updated("<p>更新日時: 2024-3-1 10:00</p>"), date(2024, 3, 1));
        assert_eq!(updated("<p>2024.3.1 更新</p>"), date(2024, 3, 1));
        assert_eq!(
            updated("<dl><dt>更新日</dt><dd>2024年3月1日</dd></dl>"),
            date(2024, 3, 1)
        );
    }

    #[test]
    fn updated_on_reads_japanese_eras() {
        assert_eq!(updated("<p>更新日：令和6年3月1日</p>"), date(2024, 3, 1));
        assert_eq!(updated("<p>令和元年5月1日更新</p>"), date(2019, 5, 1));
        assert_eq!(updated("<p>更新日：平成31年4月30日</p>"), date(2019, 4, 30));
        // 合字の元号も NFKC で読める
        assert_eq!(updated("<p>更新日：㋿6年3月1日</p>"), date(2024, 3, 1));
    }

    #[test]
    fn updated_is_preferred_over_published() {
        assert_eq!(
            updated("<p>掲載日：2020年4月1日</p><p>更新日：2024年3月1日</p>"),
            date(2024, 3, 1)
        );
        assert_eq!(updated("<p>掲載日：2020年4月1日</p>"), date(2020, 4, 1));
    }

    #[test]
    fn unrelated_dates_are_not_taken() {
        // 「更新」は日付と隣り合わないと見ない
        assert_eq!(
            updated("<p>2025年4月1日から受付。免許の更新手続き</p>"),
            None
        );
        // 日付として成り立たなければ次を探す
        assert_eq!(
            updated("<p>更新日：2024年2月30日</p><p>更新日：2024年3月1日</p>"),
            date(2024, 3, 1)
        );
        // script の中は読まない
        assert_eq!(updated("<script>// 更新日：2024年3月1日</script>"), None);
    }

    #[test]
    fn digest_is_lowercase_sha256() {
        assert_eq!(
            digest(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
