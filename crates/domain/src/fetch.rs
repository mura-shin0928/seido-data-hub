//! 取得の判定規則。通信はしない（robots・レート・Content-Type・文字コード・転送の分類）。
//!
//! 通信する側（`pipeline`）は、応答の値をここに渡して扱いを決める。

use std::time::{Duration, SystemTime};

use encoding_rs::{Encoding, UTF_8};

/// 名乗り。ブラウザを名乗らない（403 を返すサイトがあっても偽らない）
pub const USER_AGENT: &str =
    "seido-data-hub/0.1 (+https://github.com/mura-shin0928/seido-data-hub)";

/// robots.txt の `User-agent` と照らす名前
pub const ROBOTS_AGENT: &str = "seido-data-hub";

/// 同じホストへの最短の間隔（前の応答を受け取り終えてから次を送るまで）
pub const MIN_INTERVAL: Duration = Duration::from_secs(2);

/// 429・5xx で間隔を倍にしていく上限。`Crawl-delay` がこれより長ければそちらを守る
pub const MAX_SLOWED_INTERVAL: Duration = Duration::from_secs(30);

/// `Retry-After` を待つ上限（仮置き。§12 のバックオフの上限600秒に合わせた）。
/// 1日後などを指定されても、1回の実行の中では待ちきれないため
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(600);

/// 本文の上限（§20）
pub const MAX_BODY_BYTES: u64 = 10 * 1024 * 1024;

/// robots.txt の上限（RFC 9309 §2.5）。超えた分は読まずに、ここまでで解釈する
pub const MAX_ROBOTS_BYTES: u64 = 500 * 1024;

/// 転送の段数の上限（仮置き。RFC 9309 が robots.txt に求める値に合わせた）
pub const MAX_REDIRECTS: usize = 5;

/// `<meta charset>` を探す範囲（HTML の仕様の prescan と同じ）
const META_PRESCAN_BYTES: usize = 1024;

/// robots.txt の応答から決まる、そのホストの扱い（§19）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RobotsPolicy {
    /// 2xx: 中身を解釈する
    Parse,
    /// 4xx（429 を含む）: 制限なし。429 の減速はレートの側で掛かる
    AllowAll,
    /// 5xx・通信エラー: そのホストを今回見送る
    Skip,
}

pub fn robots_policy(status: u16) -> RobotsPolicy {
    match status {
        200..=299 => RobotsPolicy::Parse,
        400..=499 => RobotsPolicy::AllowAll,
        // 3xx は転送を追いきれなかったときだけここに来る。RFC 9309 §2.3.1.2 に従い「無い」と同じ扱いにする
        300..=399 => RobotsPolicy::AllowAll,
        _ => RobotsPolicy::Skip,
    }
}

/// 転送の種類。代表 URL を決めるときに、恒久だけを強い候補にする。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Redirect {
    /// 301・308
    Permanent,
    /// 302・303・307
    Temporary,
}

pub fn redirect_kind(status: u16) -> Option<Redirect> {
    match status {
        301 | 308 => Some(Redirect::Permanent),
        302 | 303 | 307 => Some(Redirect::Temporary),
        _ => None,
    }
}

/// 取得する対象の種別（§6・§20）。拡張子ではなく Content-Type と先頭バイトで決める。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Html,
    Pdf,
    /// 対象外。値は記録用（ヘッダが無ければ `None`）
    Other(Option<String>),
}

/// ヘッダだけで判断できるか。対象外と分かれば本文を読まずに済む。
///
/// `None` は「本文の先頭を見ないと決まらない」（HTML・PDF の候補）。
pub fn kind_from_header(content_type: Option<&str>) -> Option<Kind> {
    match media_type(content_type).as_deref() {
        None | Some("application/octet-stream") => None,
        Some("text/html" | "application/xhtml+xml" | "application/pdf") => None,
        Some(_) => Some(Kind::Other(content_type.map(str::to_string))),
    }
}

/// ヘッダと本文の先頭から種別を決める。先頭が `%PDF-` ならヘッダより優先する。
pub fn classify(content_type: Option<&str>, head: &[u8]) -> Kind {
    if head.starts_with(b"%PDF-") {
        return Kind::Pdf;
    }
    if let Some(kind) = kind_from_header(content_type) {
        return kind;
    }
    match media_type(content_type).as_deref() {
        Some("text/html" | "application/xhtml+xml") => Kind::Html,
        // PDF と言いながら先頭が `%PDF-` でないものは、中身を信じて HTML かどうかだけ見る
        _ if looks_like_html(head) => Kind::Html,
        _ => Kind::Other(content_type.map(str::to_string)),
    }
}

fn looks_like_html(head: &[u8]) -> bool {
    let head = head.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(head);
    head.iter()
        .find(|b| !b.is_ascii_whitespace())
        .is_some_and(|b| *b == b'<')
}

/// `text/html; charset=...` の `text/html` の部分（小文字）
fn media_type(content_type: Option<&str>) -> Option<String> {
    let value = content_type?.split(';').next()?.trim();
    (!value.is_empty()).then(|| value.to_ascii_lowercase())
}

/// 文字コードがどこで決まったか。UTF-8 以外がどれだけあるかを実データで数えるために残す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CharsetSource {
    Bom,
    Header,
    Meta,
    /// どこにも無かったので UTF-8 とした
    Default,
}

impl CharsetSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bom => "bom",
            Self::Header => "header",
            Self::Meta => "meta",
            Self::Default => "default",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedHtml {
    pub text: String,
    /// WHATWG の名前（`UTF-8`・`Shift_JIS` など）
    pub encoding: &'static str,
    pub source: CharsetSource,
    /// 復号できないバイトを置き換えたか（判定を誤った疑い）
    pub had_errors: bool,
}

/// HTML を復号する。判定順は BOM → `Content-Type` の charset → `<meta charset>` → UTF-8。
///
/// BOM を最初にするのは設計（§20）の順に足したもの。Encoding Standard ではヘッダより BOM が優先する。
pub fn decode_html(content_type: Option<&str>, body: &[u8]) -> DecodedHtml {
    let (encoding, source) = detect_charset(content_type, body);
    // decode は BOM があればそれを外す（BOM と判定が食い違うときは BOM に従う）
    let (text, used, had_errors) = encoding.decode(body);
    DecodedHtml {
        text: text.into_owned(),
        encoding: used.name(),
        source,
        had_errors,
    }
}

fn detect_charset(content_type: Option<&str>, body: &[u8]) -> (&'static Encoding, CharsetSource) {
    if let Some((encoding, _)) = Encoding::for_bom(body) {
        return (encoding, CharsetSource::Bom);
    }
    if let Some(encoding) = header_charset(content_type).and_then(label) {
        return (encoding, CharsetSource::Header);
    }
    let head = &body[..body.len().min(META_PRESCAN_BYTES)];
    if let Some(encoding) = meta_charset(head).and_then(label) {
        // HTML の仕様: meta で UTF-16 と書かれていても、ASCII で読めている以上 UTF-8 とみなす
        return (encoding.output_encoding(), CharsetSource::Meta);
    }
    (UTF_8, CharsetSource::Default)
}

fn label(value: &[u8]) -> Option<&'static Encoding> {
    Encoding::for_label(value)
}

fn header_charset(content_type: Option<&str>) -> Option<&[u8]> {
    content_type?.split(';').skip(1).find_map(|param| {
        let (key, value) = param.split_once('=')?;
        key.trim()
            .eq_ignore_ascii_case("charset")
            .then(|| value.trim().trim_matches(['"', '\'']).as_bytes())
    })
}

/// `<meta charset="x">` と `<meta http-equiv="Content-Type" content="text/html; charset=x">` の両方を拾う。
fn meta_charset(head: &[u8]) -> Option<&[u8]> {
    let lower = head.to_ascii_lowercase();
    let mut from = 0;
    while let Some(at) = find(&lower[from..], b"<meta") {
        let start = from + at;
        let end = find(&lower[start..], b">").map_or(lower.len(), |e| start + e);
        if let Some(value) =
            charset_in(&lower[start..end]).map(|(s, e)| &head[start + s..start + e])
        {
            return Some(value);
        }
        from = end;
    }
    None
}

/// タグの中の `charset` の値の位置（タグ内の相対位置）
fn charset_in(tag: &[u8]) -> Option<(usize, usize)> {
    let at = find(tag, b"charset")? + b"charset".len();
    let mut i = at;
    let skip_space = |i: &mut usize| {
        while tag.get(*i).is_some_and(u8::is_ascii_whitespace) {
            *i += 1;
        }
    };
    skip_space(&mut i);
    if tag.get(i) != Some(&b'=') {
        return None;
    }
    i += 1;
    skip_space(&mut i);
    if matches!(tag.get(i), Some(b'"' | b'\'')) {
        i += 1;
    }
    let start = i;
    while tag.get(i).is_some_and(|b| {
        !matches!(b, b'"' | b'\'' | b';' | b'/' | b'>') && !b.is_ascii_whitespace()
    }) {
        i += 1;
    }
    (i > start).then_some((start, i))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// ホストごとの間隔の状態（§11）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pace {
    /// 最短の間隔（本番は `MIN_INTERVAL`。テストで縮める）
    pub min_interval: Duration,
    /// robots.txt の `Crawl-delay`
    pub crawl_delay: Option<Duration>,
    /// 429・5xx で倍にした回数
    pub slowdowns: u32,
}

impl Pace {
    pub fn new(min_interval: Duration) -> Self {
        Self {
            min_interval,
            crawl_delay: None,
            slowdowns: 0,
        }
    }

    /// 前の応答を受け取り終えてから、次を送るまでの間隔。
    ///
    /// `max(最短の間隔, Crawl-delay)` を減速の回数だけ倍にする。倍にした値は30秒で止めるが、
    /// `Crawl-delay` がそれより長ければ `Crawl-delay` を守る。
    pub fn interval(&self) -> Duration {
        let base = self
            .crawl_delay
            .map_or(self.min_interval, |d| d.max(self.min_interval));
        let slowed = base.saturating_mul(2u32.saturating_pow(self.slowdowns));
        slowed.min(MAX_SLOWED_INTERVAL.max(base))
    }

    /// 応答を受けて間隔を直し、次に送ってよいまでの待ち時間を返す。
    ///
    /// 429・5xx は間隔を倍にし、`Retry-After` があればそちらまで待つ（上限 `MAX_RETRY_AFTER`）。
    /// それ以外の応答では倍にした回数を1つ戻す（仮置き。§11 に戻し方の定めは無い）。
    pub fn after_response(&mut self, status: u16, retry_after: Option<Duration>) -> Duration {
        if status == 429 || (500..=599).contains(&status) {
            if self.interval() < MAX_SLOWED_INTERVAL {
                self.slowdowns += 1;
            }
        } else {
            self.slowdowns = self.slowdowns.saturating_sub(1);
        }
        let wait = self.interval();
        match retry_after {
            Some(after) if status == 429 || status == 503 => wait.max(after.min(MAX_RETRY_AFTER)),
            _ => wait,
        }
    }
}

/// `Retry-After` を待ち時間にする。秒数と HTTP 日付の両方を受ける。過去の日付は0秒。
pub fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let at = httpdate::parse_http_date(value).ok()?;
    Some(at.duration_since(now).unwrap_or(Duration::ZERO))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn robots_status_decides_the_policy() {
        assert_eq!(robots_policy(200), RobotsPolicy::Parse);
        assert_eq!(robots_policy(404), RobotsPolicy::AllowAll);
        // 429 も 4xx として制限なし。減速は Pace の側で掛かる
        assert_eq!(robots_policy(429), RobotsPolicy::AllowAll);
        assert_eq!(robots_policy(503), RobotsPolicy::Skip);
    }

    #[test]
    fn redirects_are_split_into_permanent_and_temporary() {
        assert_eq!(redirect_kind(301), Some(Redirect::Permanent));
        assert_eq!(redirect_kind(308), Some(Redirect::Permanent));
        assert_eq!(redirect_kind(302), Some(Redirect::Temporary));
        assert_eq!(redirect_kind(307), Some(Redirect::Temporary));
        assert_eq!(redirect_kind(304), None);
    }

    #[test]
    fn kind_is_decided_by_header_and_leading_bytes() {
        assert_eq!(
            classify(Some("text/html; charset=Shift_JIS"), b"<html>"),
            Kind::Html
        );
        assert_eq!(classify(None, b"\n  <!DOCTYPE html>"), Kind::Html);
        assert_eq!(classify(Some("text/html"), b"%PDF-1.7\n"), Kind::Pdf);
        assert_eq!(classify(Some("application/pdf"), b"%PDF-1.4"), Kind::Pdf);
        assert_eq!(
            classify(Some("application/octet-stream"), b"%PDF-1.4"),
            Kind::Pdf
        );
        assert_eq!(
            classify(Some("image/png"), b"\x89PNG"),
            Kind::Other(Some("image/png".to_string()))
        );
        assert_eq!(classify(None, b"\x89PNG"), Kind::Other(None));
    }

    #[test]
    fn body_is_not_needed_when_the_header_says_out_of_scope() {
        assert_eq!(
            kind_from_header(Some("image/png")),
            Some(Kind::Other(Some("image/png".to_string())))
        );
        assert_eq!(kind_from_header(Some("text/html")), None);
        assert_eq!(kind_from_header(Some("application/pdf")), None);
        assert_eq!(kind_from_header(None), None);
    }

    /// 「子育て」を Shift_JIS にしたもの
    const KOSODATE_SJIS: &[u8] = b"\x8e\x71\x88\xe7\x82\xc4";

    fn sjis_page(head: &str) -> Vec<u8> {
        [
            format!("<html><head>{head}</head><body>").as_bytes(),
            KOSODATE_SJIS,
            b"</body></html>",
        ]
        .concat()
    }

    #[test]
    fn utf8_bom_wins() {
        let body = [b"\xEF\xBB\xBF".as_slice(), "<p>子育て</p>".as_bytes()].concat();
        let decoded = decode_html(Some("text/html; charset=Shift_JIS"), &body);
        assert_eq!(decoded.source, CharsetSource::Bom);
        assert_eq!(decoded.encoding, "UTF-8");
        assert_eq!(decoded.text, "<p>子育て</p>");
    }

    #[test]
    fn header_charset_is_used() {
        let decoded = decode_html(Some("text/html; charset=\"Shift_JIS\""), &sjis_page(""));
        assert_eq!(decoded.source, CharsetSource::Header);
        assert_eq!(decoded.encoding, "Shift_JIS");
        assert!(decoded.text.contains("子育て"));
        assert!(!decoded.had_errors);
    }

    #[test]
    fn meta_charset_is_used_when_the_header_has_none() {
        let decoded = decode_html(
            Some("text/html"),
            &sjis_page(r#"<meta charset="shift_jis">"#),
        );
        assert_eq!(decoded.source, CharsetSource::Meta);
        assert!(decoded.text.contains("子育て"));

        let http_equiv =
            r#"<META http-equiv="Content-Type" content="text/html; charset=Shift_JIS" />"#;
        let decoded = decode_html(None, &sjis_page(http_equiv));
        assert_eq!(decoded.source, CharsetSource::Meta);
        assert_eq!(decoded.encoding, "Shift_JIS");
        assert!(decoded.text.contains("子育て"));
    }

    #[test]
    fn utf8_is_the_default() {
        let decoded = decode_html(Some("text/html"), "<p>子育て</p>".as_bytes());
        assert_eq!(decoded.source, CharsetSource::Default);
        assert_eq!(decoded.encoding, "UTF-8");
        assert_eq!(decoded.text, "<p>子育て</p>");

        // 判定を誤ると置き換えが起きたことが分かる
        let decoded = decode_html(Some("text/html"), &sjis_page(""));
        assert!(decoded.had_errors);
    }

    #[test]
    fn header_wins_over_meta() {
        let body = sjis_page(r#"<meta charset="utf-8">"#);
        let decoded = decode_html(Some("text/html; charset=Shift_JIS"), &body);
        assert_eq!(decoded.source, CharsetSource::Header);
        assert!(decoded.text.contains("子育て"));
    }

    #[test]
    fn meta_after_the_prescan_range_is_not_seen() {
        let padding = "<!-- ".to_string() + &"x".repeat(META_PRESCAN_BYTES) + " -->";
        let body = sjis_page(&format!(r#"{padding}<meta charset="shift_jis">"#));
        assert_eq!(decode_html(None, &body).source, CharsetSource::Default);
    }

    const TWO: Duration = Duration::from_secs(2);

    #[test]
    fn interval_is_at_least_two_seconds() {
        assert_eq!(Pace::new(MIN_INTERVAL).interval(), TWO);

        let mut pace = Pace::new(MIN_INTERVAL);
        pace.crawl_delay = Some(Duration::from_secs(5));
        assert_eq!(pace.interval(), Duration::from_secs(5));
        // Crawl-delay が短くても2秒は空ける
        pace.crawl_delay = Some(Duration::from_secs(1));
        assert_eq!(pace.interval(), TWO);
    }

    #[test]
    fn errors_slow_the_host_down_up_to_thirty_seconds() {
        let mut pace = Pace::new(MIN_INTERVAL);
        assert_eq!(pace.after_response(503, None), Duration::from_secs(4));
        assert_eq!(pace.after_response(429, None), Duration::from_secs(8));
        for _ in 0..10 {
            pace.after_response(500, None);
        }
        assert_eq!(pace.interval(), MAX_SLOWED_INTERVAL);

        // 成功で1段ずつ戻る
        let slowed = pace.interval();
        assert!(pace.after_response(200, None) < slowed);

        // Crawl-delay が30秒より長ければそれを守る
        let mut long = Pace::new(MIN_INTERVAL);
        long.crawl_delay = Some(Duration::from_secs(60));
        assert_eq!(long.after_response(503, None), Duration::from_secs(60));
    }

    #[test]
    fn retry_after_is_respected_up_to_the_limit() {
        let mut pace = Pace::new(MIN_INTERVAL);
        assert_eq!(
            pace.after_response(429, Some(Duration::from_secs(120))),
            Duration::from_secs(120)
        );
        let mut pace = Pace::new(MIN_INTERVAL);
        assert_eq!(
            pace.after_response(503, Some(Duration::from_secs(86_400))),
            MAX_RETRY_AFTER
        );
        // 429・503 以外の Retry-After は見ない
        let mut pace = Pace::new(MIN_INTERVAL);
        assert_eq!(
            pace.after_response(200, Some(Duration::from_secs(120))),
            TWO
        );
    }

    #[test]
    fn retry_after_accepts_seconds_and_dates() {
        let now = httpdate::parse_http_date("Wed, 23 Sep 2026 10:00:00 GMT").unwrap();
        assert_eq!(
            parse_retry_after("120", now),
            Some(Duration::from_secs(120))
        );
        assert_eq!(
            parse_retry_after("Wed, 23 Sep 2026 10:01:30 GMT", now),
            Some(Duration::from_secs(90))
        );
        assert_eq!(
            parse_retry_after("Wed, 23 Sep 2026 09:00:00 GMT", now),
            Some(Duration::ZERO)
        );
        assert_eq!(parse_retry_after("soon", now), None);
    }
}
