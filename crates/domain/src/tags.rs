//! レジストリのタグ（コードと名前）。レジストリ README §3 の一覧をコード順に並べたもの。

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Tag {
    pub code: &'static str,
    pub name: &'static str,
}

const fn tag(code: &'static str, name: &'static str) -> Tag {
    Tag { code, name }
}

/// CSC_個人向けカテゴリー（`programs.category_codes`）
pub const CATEGORIES: &[Tag] = &[
    tag("000", "未分類"),
    tag("001", "住民向け情報"),
    tag("002", "妊娠・出産"),
    tag("003", "子育て"),
    tag("004", "保育"),
    tag("005", "学校教育"),
    tag("006", "結婚・離婚"),
    tag("007", "引越し・住まい"),
    tag("008", "就職・退職"),
    tag("009", "高齢者支援"),
    tag("010", "在宅介護"),
    tag("011", "施設介護"),
    tag("012", "ご不幸"),
    tag("013", "戸籍・住民票・印鑑登録等"),
    tag("014", "税"),
    tag("015", "国民健康保険"),
    tag("016", "国民年金"),
    tag("017", "水道・ガス・電気"),
    tag("018", "交通"),
    tag("019", "駐輪・駐車"),
    tag("020", "都市計画"),
    tag("021", "ごみ・環境保全"),
    tag("022", "食品・衛生"),
    tag("023", "ペット・動物"),
    tag("024", "生活困窮者支援"),
    tag("025", "障がい者支援"),
    tag("026", "消費生活"),
    tag("027", "健康・医療"),
    tag("028", "文化・スポーツ・生涯学習"),
    tag("029", "市民活動・コミュニティ"),
    tag("030", "防災・災害"),
    tag("031", "防犯・犯罪"),
    tag("032", "救急・消防"),
];

/// CBF_対象者（個人）（`programs.target_codes`）
pub const TARGETS: &[Tag] = &[
    tag("000", "未分類"),
    tag("086", "妊産婦"),
    tag("087", "子育て中"),
    tag("088", "ひとり親"),
    tag("089", "未熟児"),
    tag("090", "障害児"),
    tag("091", "遺児"),
    tag("092", "学生"),
    tag("093", "独身者"),
    tag("094", "求職者"),
    tag("095", "就業者"),
    tag("096", "高齢者"),
    tag("097", "介護中"),
    tag("098", "障がい者"),
    tag("099", "遺族"),
    tag("100", "ペット"),
    tag("101", "被災者"),
    tag("102", "犯罪被害者"),
];

/// CCT_コンテンツタイプ（個人）（`programs.content_codes`）
pub const CONTENTS: &[Tag] = &[
    tag("000", "未分類"),
    tag("077", "届出"),
    tag("078", "申請"),
    tag("079", "支給・支援"),
    tag("080", "イベント"),
    tag("081", "施設"),
    tag("082", "情報啓発"),
    tag("083", "地図"),
];

pub fn contains(tags: &[Tag], code: &str) -> bool {
    tags.iter().any(|t| t.code == code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_match_the_registry_readme() {
        assert_eq!(CATEGORIES.len(), 33);
        assert_eq!(TARGETS.len(), 18);
        assert_eq!(CONTENTS.len(), 8);
    }

    #[test]
    fn codes_are_three_digits_in_ascending_order() {
        for tags in [CATEGORIES, TARGETS, CONTENTS] {
            assert!(
                tags.iter()
                    .all(|t| t.code.len() == 3 && t.code.bytes().all(|b| b.is_ascii_digit()))
            );
            assert!(tags.windows(2).all(|w| w[0].code < w[1].code));
        }
    }
}
