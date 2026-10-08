//! 取得した内容が前回から変わったかの判定。
//!
//! 抽出規則の版が違うときは比べない（`Unknown`）。ハッシュの違いは規則の変更で生じたもので、サイトの変更とは限らない。
//! 前回の内容が無い初回も変化とは数えない（`Unknown`）。

/// 判定の結果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Changed,
    Unchanged,
    /// 比べられない（初回・抽出規則の版の違い・本文を読まなかった）
    Unknown,
}

/// 比べる内容。HTML は本文のハッシュと抽出規則の版、PDF などは生のハッシュと版なし
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Content<'a> {
    pub hash: &'a str,
    pub extractor_version: Option<i32>,
}

pub fn compare(previous: Option<Content<'_>>, current: Content<'_>) -> Change {
    match previous {
        Some(previous) if previous.extractor_version == current.extractor_version => {
            if previous.hash == current.hash {
                Change::Unchanged
            } else {
                Change::Changed
            }
        }
        _ => Change::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn html(hash: &str, version: i32) -> Content<'_> {
        Content {
            hash,
            extractor_version: Some(version),
        }
    }
    fn pdf(hash: &str) -> Content<'_> {
        Content {
            hash,
            extractor_version: None,
        }
    }

    #[test]
    fn nothing_to_compare_with_is_unknown() {
        assert_eq!(compare(None, html("a", 2)), Change::Unknown);
    }

    #[test]
    fn the_same_hash_is_unchanged_and_a_different_one_is_changed() {
        assert_eq!(compare(Some(html("a", 2)), html("a", 2)), Change::Unchanged);
        assert_eq!(compare(Some(html("a", 2)), html("b", 2)), Change::Changed);
        assert_eq!(compare(Some(pdf("a")), pdf("b")), Change::Changed);
    }

    #[test]
    fn a_different_extractor_version_is_unknown_even_if_the_hash_differs() {
        assert_eq!(compare(Some(html("a", 1)), html("b", 2)), Change::Unknown);
        assert_eq!(compare(Some(html("a", 1)), html("a", 2)), Change::Unknown);
    }
}
