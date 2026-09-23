//! レジストリに書かれた URL を、巡回に使える形（`normalized_url` と `dedup_key`）にそろえる。
//!
//! ```text
//! 生の値 → カンマ結合の分割 → 前後の空白除去 → 解析 → 正規化 → scope 判定 → dedup_key
//! ```

use std::collections::BTreeSet;

use url::Url;

/// 取得してよい scheme。`http` は `https` に書き換えず、そのまま取りに行く
const ALLOWED_SCHEMES: [&str; 2] = ["https", "http"];

/// 消してよい追跡パラメータ。内容を変えるクエリ（`page`・`id` など）は消さない
const TRACKING_PARAMS: [&str; 6] = [
    "utm_source",
    "utm_medium",
    "utm_campaign",
    "utm_term",
    "utm_content",
    "gclid",
];

/// 接頭辞付きの URL が同じパスの PC 版へ 301 するホストを足す（いま成り立つのは1件だけ）。
/// 畳むのは鍵の上だけで、`normalized_url` は書き換えない。
const SITE_RULES: [(&str, &str); 1] = [("www.city.koganei.lg.jp", "/smph/")];

/// Frontier（`urls`）に入れる1件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedUrl {
    /// 発見したときの表記。カンマ結合を分けた後の、前後の空白を落としただけの値
    pub raw_url: String,
    /// 取得前に機械的に正規化した URL。実際に取りに行くのはこれ
    pub normalized_url: String,
    /// 投入時の一意キー。`normalized_url` ＋ サイト固有の規則
    pub dedup_key: String,
    /// `scheme + hostname + 実効ポート`。ホストごとのレート制御に使う
    pub host_key: String,
}

/// `urls` に入れなかった理由。読めない値があっても取り込みは止めず、理由を警告に出す。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Rejection {
    #[error("URL として読めない")]
    Unparsable,
    #[error("scheme が対象外: {scheme}")]
    Scheme { scheme: String },
    #[error("ホストが無い")]
    NoHost,
    #[error("標準ポートではない: {port}")]
    Port { port: u16 },
    #[error("ホストが許可リストに無い: {host_key}")]
    HostNotAllowed { host_key: String },
}

/// 1つの項目に複数の URL を入れた値を分ける。
///
/// 切るのは `,http://` `,https://` の直前だけ。パスにカンマを含む URL があるため。
pub fn split_joined(value: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut rest = value;
    loop {
        let Some(cut) = find_join(rest) else {
            parts.push(rest);
            return parts;
        };
        parts.push(&rest[..cut]);
        rest = &rest[cut + 1..];
    }
}

/// 次の `,http://` / `,https://` のカンマの位置
fn find_join(value: &str) -> Option<usize> {
    value
        .match_indices(',')
        .find(|(i, _)| {
            let after = &value[i + 1..];
            after.starts_with("http://") || after.starts_with("https://")
        })
        .map(|(i, _)| i)
}

/// 生の値1件を、Frontier に入れる形にする。
///
/// 分割は呼び出し側で先に行う（1つの値から複数の URL が出るため）。
pub fn prepare(raw: &str) -> Result<PreparedUrl, Rejection> {
    prepare_with(raw, &SITE_RULES)
}

fn prepare_with(raw: &str, rules: &[(&str, &str)]) -> Result<PreparedUrl, Rejection> {
    let raw = raw.trim();
    let parsed = Url::parse(raw).map_err(|_| Rejection::Unparsable)?;

    let scheme = parsed.scheme().to_string();
    if !ALLOWED_SCHEMES.contains(&scheme.as_str()) {
        return Err(Rejection::Scheme { scheme });
    }
    // 既定ポート（http=80・https=443）は Url が落とすので、残っていれば非標準
    if let Some(port) = parsed.port() {
        return Err(Rejection::Port { port });
    }
    let host = parsed.host_str().ok_or(Rejection::NoHost)?.to_string();

    let normalized = normalize(parsed);
    let host_key = format!("{scheme}://{host}");
    let dedup_key = dedup_key(&normalized, &host, rules);
    Ok(PreparedUrl {
        raw_url: raw.to_string(),
        normalized_url: normalized.into(),
        dedup_key,
        host_key,
    })
}

/// ホストの許可リストによる scope 判定。発見した URL が他自治体や国のサイトへ渡らないようにする。
pub fn in_scope(url: &PreparedUrl, allowed_hosts: &BTreeSet<String>) -> Result<(), Rejection> {
    allowed_hosts
        .contains(&url.host_key)
        .then_some(())
        .ok_or_else(|| Rejection::HostNotAllowed {
            host_key: url.host_key.clone(),
        })
}

/// 取得前の正規化。小文字化・IDN・既定ポートの除去・`.`／`..` の解決・非 ASCII の
/// エンコードは `Url` が済ませているので、ここでは残りを行う。
fn normalize(mut parsed: Url) -> Url {
    parsed.set_fragment(None);

    let query = parsed.query().map(normalize_query);
    match query.as_deref() {
        Some("") | None => parsed.set_query(None),
        Some(q) => parsed.set_query(Some(q)),
    }

    let path = normalize_percent(parsed.path());
    parsed.set_path(&path);
    parsed
}

/// クエリの順番を安定させ、追跡パラメータを落とす。値は分解せず、そのまま並べ替える。
fn normalize_query(query: &str) -> String {
    let mut pairs: Vec<String> = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .filter(|pair| {
            let key = pair.split('=').next().unwrap_or(pair);
            !TRACKING_PARAMS.contains(&key)
        })
        .map(normalize_percent)
        .collect();
    pairs.sort();
    pairs.join("&")
}

/// パーセントエンコーディングの表記をそろえる（RFC 3986 §6.2.2）。
/// 予約されていない文字はデコードし、残りは16進を大文字にする。
fn normalize_percent(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = String::with_capacity(value.len());
    let mut i = 0;
    while i < bytes.len() {
        let Some(hex) = bytes
            .get(i + 1..i + 3)
            .filter(|_| bytes[i] == b'%')
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok().map(|b| (h, b)))
        else {
            out.push(bytes[i] as char);
            i += 1;
            continue;
        };
        let (text, byte) = hex;
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push_str(&text.to_ascii_uppercase());
        }
        i += 3;
    }
    out
}

/// `normalized_url` にサイト固有の規則を当てて、投入時の一意キーを作る。
fn dedup_key(normalized: &Url, host: &str, rules: &[(&str, &str)]) -> String {
    let Some((_, prefix)) = rules.iter().find(|(rule_host, _)| *rule_host == host) else {
        return normalized.as_str().to_string();
    };
    let path = normalized.path();
    let Some(rest) = path.strip_prefix(prefix) else {
        return normalized.as_str().to_string();
    };
    let mut key = normalized.clone();
    key.set_path(&format!("/{rest}"));
    key.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prepared(raw: &str) -> PreparedUrl {
        prepare(raw).unwrap_or_else(|e| panic!("{raw} を読めない: {e}"))
    }

    #[test]
    fn trailing_whitespace_and_newlines_are_dropped() {
        assert_eq!(
            prepared("https://www.city.example.jp/oyako/syussanhi.html ").normalized_url,
            "https://www.city.example.jp/oyako/syussanhi.html"
        );
        assert_eq!(
            prepared("https://www.city.example.lg.jp/b023/p001766.html\n\n").normalized_url,
            "https://www.city.example.lg.jp/b023/p001766.html"
        );
    }

    #[test]
    fn fragments_are_dropped() {
        assert_eq!(
            prepared("https://www.city.example.jp/s058/1075.html#p1").normalized_url,
            "https://www.city.example.jp/s058/1075.html"
        );
        assert_eq!(
            prepared("https://www.city.example.jp/faq/1463551358720.html#:~:text=%E5%9B%BD")
                .dedup_key,
            "https://www.city.example.jp/faq/1463551358720.html"
        );
    }

    #[test]
    fn japanese_paths_are_percent_encoded() {
        let url = prepared("https://csw.example.jp/子ども家庭事業/");
        assert!(
            url.normalized_url
                .starts_with("https://csw.example.jp/%E5%AD%90"),
            "{}",
            url.normalized_url
        );
    }

    #[test]
    fn percent_escapes_are_normalized() {
        // 小文字の16進は大文字に、予約されていない文字はデコードする
        assert_eq!(
            prepared("https://example.jp/%e5%ad%90/%41%2Db.html").normalized_url,
            "https://example.jp/%E5%AD%90/A-b.html"
        );
    }

    #[test]
    fn query_is_kept_sorted_and_stripped_of_tracking() {
        // `?id=` は内容を決めるので残す
        assert_eq!(
            prepared("https://lib.example.jp/viewer/info.html?id=8").normalized_url,
            "https://lib.example.jp/viewer/info.html?id=8"
        );
        assert_eq!(
            prepared("https://example.jp/a?b=2&utm_source=mail&a=1&gclid=x").normalized_url,
            "https://example.jp/a?a=1&b=2"
        );
        assert_eq!(
            prepared("https://example.jp/a?utm_campaign=x").normalized_url,
            "https://example.jp/a"
        );
    }

    const TEST_RULES: [(&str, &str); 1] = [("www.city.example.jp", "/smph/")];

    #[test]
    fn site_rules_fold_only_the_dedup_key() {
        let rule = |raw: &str| prepare_with(raw, &TEST_RULES).unwrap();
        let smph = rule("https://www.city.example.jp/smph/kosodate/gaiyou.html");
        let pc = rule("https://www.city.example.jp/kosodate/gaiyou.html");
        assert_eq!(smph.dedup_key, pc.dedup_key);
        // 取りに行く URL は書き換えない
        assert_ne!(smph.normalized_url, pc.normalized_url);
        assert!(smph.normalized_url.contains("/smph/"));

        // 規則に無いホストでは畳まない
        let other = rule("https://www.town.example.jp/smph/a.html");
        assert!(other.dedup_key.contains("/smph/"));
    }

    #[test]
    fn prepare_uses_the_site_rules() {
        let (host, prefix) = SITE_RULES[0];
        let with_prefix = prepared(&format!("https://{host}{prefix}a.html"));
        assert_eq!(with_prefix.dedup_key, format!("https://{host}/a.html"));
    }

    #[test]
    fn joined_values_are_split_before_the_scheme() {
        assert_eq!(
            split_joined("https://a.jp/1.html,https://a.jp/2.html"),
            ["https://a.jp/1.html", "https://a.jp/2.html"]
        );
        assert_eq!(
            split_joined("http://a.jp/1.html,http://a.jp/2.html,http://a.jp/3.html").len(),
            3
        );
        // パスに含まれるカンマでは切らない
        assert_eq!(
            split_joined("https://www.city.example.jp/index.cfm/43,4560,586,3431,html").len(),
            1
        );
    }

    #[test]
    fn http_is_kept_as_it_is() {
        // https に書き換えない
        let url = prepared("http://www.town.example.jp/p001763.html");
        assert_eq!(
            url.normalized_url,
            "http://www.town.example.jp/p001763.html"
        );
        assert_eq!(url.host_key, "http://www.town.example.jp");
    }

    #[test]
    fn host_and_scheme_are_lowercased_and_default_ports_dropped() {
        let url = prepared("HTTPS://WWW.City.Example.JP:443/A.html");
        assert_eq!(url.normalized_url, "https://www.city.example.jp/A.html");
        assert_eq!(url.host_key, "https://www.city.example.jp");
    }

    #[test]
    fn out_of_scope_values_are_rejected_with_a_reason() {
        assert_eq!(prepare("ちらし"), Err(Rejection::Unparsable));
        assert_eq!(prepare(""), Err(Rejection::Unparsable));
        assert_eq!(
            prepare("mailto:kosodate@example.jp"),
            Err(Rejection::Scheme {
                scheme: "mailto".to_string()
            })
        );
        assert_eq!(
            prepare("https://example.jp:8443/a.html"),
            Err(Rejection::Port { port: 8443 })
        );
    }

    #[test]
    fn host_allow_list_is_enforced() {
        let url = prepared("https://www.city.example.jp/a.html");
        let allowed = BTreeSet::from(["https://www.city.example.jp".to_string()]);
        assert_eq!(in_scope(&url, &allowed), Ok(()));

        let other = prepared("https://www.pref.example.jp/a.html");
        assert_eq!(
            in_scope(&other, &allowed),
            Err(Rejection::HostNotAllowed {
                host_key: "https://www.pref.example.jp".to_string()
            })
        );
    }

    #[test]
    fn dot_segments_are_resolved() {
        assert_eq!(
            prepared("https://example.jp/a/../b/./c.html").normalized_url,
            "https://example.jp/b/c.html"
        );
    }
}
