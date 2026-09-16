//! 東京都「子育て支援制度レジストリ」JSON の読み取りと、取り込み用の値への変換。
//!
//! 項目定義はレジストリ README §6。取り込みで使う項目だけを型にし、行全体は
//! `serde_json::Value` のまま `programs.registry` に保存する。

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

use crate::tags;

/// 取り込み元の既定URL（0〜6歳、2025-08-20時点で更新停止）。
pub const DEFAULT_SOURCE_URL: &str = "https://data.storage.data.metro.tokyo.lg.jp/digitalservice/130001_kosodateshienseido_tokyo.json";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    institution_name: InstitutionName,
    target: Target,
    local_government_link: Link,
    area: Area,
    basic_information: BasicInformation,
    tag: Tag,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InstitutionName {
    canonical_name: String,
    short_name: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Target {
    less_than: AgeBound,
    less_than_or_equal_to: AgeBound,
    greater_than_or_equal_to: AgeBound,
    greater_than: AgeBound,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgeBound {
    target_age: Option<i32>,
    target_age_of_months: Option<i32>,
}

#[derive(Debug, Deserialize)]
struct Link {
    uri: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Area {
    area_code: String,
}

#[derive(Debug, Deserialize)]
struct BasicInformation {
    psid: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Tag {
    category_code: Option<Vec<String>>,
    target_code: Option<Vec<String>>,
    contents_code: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedArea {
    pub code: String,
    pub name: String,
    pub parent_code: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImportedProgram {
    pub psid: String,
    pub um: String,
    pub area_code: String,
    pub canonical_name: String,
    pub short_name: Option<String>,
    pub source_url: String,
    pub category_codes: Vec<String>,
    pub target_codes: Vec<String>,
    pub content_codes: Vec<String>,
    pub age_min_months: Option<i32>,
    pub age_max_months: Option<i32>,
    pub registry: Value,
}

#[derive(Debug)]
pub struct Imported {
    pub areas: Vec<ImportedArea>,
    pub programs: Vec<ImportedProgram>,
    /// タグの一覧に無いコード。取り込みは止めず、呼び出し側でログに出す
    pub unknown_tags: Vec<UnknownTag>,
}

/// 正規化してもタグの一覧（README §3）に無いコード。別のタグのコードが入っている等、元データの誤り。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownTag {
    pub psid: String,
    /// `category_codes` / `target_codes` / `content_codes`
    pub column: &'static str,
    pub value: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("JSON として解釈できない: {source}")]
    InvalidJson { source: serde_json::Error },
    #[error("JSON のトップレベルが配列ではない")]
    NotAnArray,
    #[error("{index} 行目を読めない: {source}")]
    Record {
        index: usize,
        source: serde_json::Error,
    },
    #[error("{index} 行目の psid から UM ナンバーを取れない: {psid}")]
    Psid { index: usize, psid: String },
    #[error("{index} 行目の対象地域を読めない: {area}")]
    Area { index: usize, area: String },
}

/// レジストリ JSON 全体を読み、自治体と制度に分ける。
///
/// 同じ自治体・同じUM・同じURLの行も psid は別で、中身も違う（予防接種の種類ごと等）ので畳まない。
pub fn parse(json: &str) -> Result<Imported, ParseError> {
    let Value::Array(rows) =
        serde_json::from_str::<Value>(json).map_err(|source| ParseError::InvalidJson { source })?
    else {
        return Err(ParseError::NotAnArray);
    };

    let mut area_names: BTreeMap<String, String> = BTreeMap::new();
    let mut programs = Vec::with_capacity(rows.len());
    let mut unknown_tags = Vec::new();

    for (index, row) in rows.into_iter().enumerate() {
        let record =
            Record::deserialize(&row).map_err(|source| ParseError::Record { index, source })?;

        let um = um_from_psid(&record.basic_information.psid).ok_or_else(|| ParseError::Psid {
            index,
            psid: record.basic_information.psid.clone(),
        })?;
        let (area_code, area_name) =
            split_area(&record.area.area_code).ok_or_else(|| ParseError::Area {
                index,
                area: record.area.area_code.clone(),
            })?;
        area_names.entry(area_code.clone()).or_insert(area_name);

        let psid = record.basic_information.psid;
        let tag = record.tag;
        let category_codes = normalize_codes(tag.category_code.unwrap_or_default());
        let target_codes = normalize_codes(tag.target_code.unwrap_or_default());
        let content_codes = normalize_codes(tag.contents_code);
        for (column, known, codes) in [
            ("category_codes", tags::CATEGORIES, &category_codes),
            ("target_codes", tags::TARGETS, &target_codes),
            ("content_codes", tags::CONTENTS, &content_codes),
        ] {
            unknown_tags.extend(unknown_codes(known, codes).map(|value| UnknownTag {
                psid: psid.clone(),
                column,
                value: value.clone(),
            }));
        }

        let target = &record.target;
        programs.push(ImportedProgram {
            psid,
            um,
            area_code,
            canonical_name: record.institution_name.canonical_name,
            short_name: record.institution_name.short_name,
            source_url: record.local_government_link.uri,
            category_codes,
            target_codes,
            content_codes,
            age_min_months: age_min_months(&target.greater_than_or_equal_to, &target.greater_than),
            age_max_months: age_max_months(&target.less_than, &target.less_than_or_equal_to),
            registry: row,
        });
    }

    Ok(Imported {
        areas: build_areas(area_names),
        programs,
        unknown_tags,
    })
}

/// タグの値を3桁のコードの並びにそろえる。
///
/// 元データには `"002，003"`（全角読点で1要素）・`"027 "`（末尾空白）・`"86"`（2桁）がある。
/// 読点で分け、前後の空白（全角含む）を除き、数字だけで3桁未満なら0埋めする。空は捨て、重複は最初の1つを残す。
fn normalize_codes(values: Vec<String>) -> Vec<String> {
    let mut codes: Vec<String> = Vec::with_capacity(values.len());
    for value in &values {
        for part in value.split([',', '，', '、']) {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let code = if part.len() < 3 && part.bytes().all(|b| b.is_ascii_digit()) {
                format!("{part:0>3}")
            } else {
                part.to_string()
            };
            if !codes.contains(&code) {
                codes.push(code);
            }
        }
    }
    codes
}

fn unknown_codes<'a>(
    known: &'static [tags::Tag],
    codes: &'a [String],
) -> impl Iterator<Item = &'a String> {
    codes
        .iter()
        .filter(move |code| !tags::contains(known, code))
}

/// `psid3.0+3000020131130+1+UM24` → `UM24`
fn um_from_psid(psid: &str) -> Option<String> {
    let um = psid.rsplit('+').next()?;
    let digits = um.strip_prefix("UM")?;
    (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())).then(|| um.to_string())
}

/// `131130;渋谷区` → (`131130`, `渋谷区`)。名前の前後の空白・タブは落とす（`檜原村\t` がある）。
fn split_area(value: &str) -> Option<(String, String)> {
    let (code, name) = value.split_once(';')?;
    let code = code.trim();
    let name = name.trim();
    (code.len() == 6 && code.bytes().all(|b| b.is_ascii_digit()) && !name.is_empty())
        .then(|| (code.to_string(), name.to_string()))
}

/// 団体コードの3〜5桁目が `000` なら都道府県。市区町村の親は、同じ上2桁の都道府県。
/// 末尾はチェックディジットなので、親のコードは計算せずデータの中から探す。
fn build_areas(names: BTreeMap<String, String>) -> Vec<ImportedArea> {
    let is_prefecture = |code: &str| &code[2..5] == "000";
    let prefectures: BTreeMap<&str, &str> = names
        .keys()
        .filter(|code| is_prefecture(code))
        .map(|code| (&code[..2], code.as_str()))
        .collect();

    names
        .iter()
        .map(|(code, name)| ImportedArea {
            code: code.clone(),
            name: name.clone(),
            parent_code: (!is_prefecture(code))
                .then(|| prefectures.get(&code[..2]).map(|p| p.to_string()))
                .flatten(),
        })
        .collect()
}

fn total_months(bound: &AgeBound) -> Option<i32> {
    match (bound.target_age, bound.target_age_of_months) {
        (None, None) => None,
        (years, months) => Some(years.unwrap_or(0) * 12 + months.unwrap_or(0)),
    }
}

/// 対象になる最小の月齢（含む）。
///
/// 「超過」も「以上」と同じ月数にする。「1歳超過」を2歳以上と読むか1歳0か月超と読むかは曖昧で、
/// 絞り込みでは迷ったら表示する側（小さい方）に倒す。
fn age_min_months(at_least: &AgeBound, over: &AgeBound) -> Option<i32> {
    [total_months(at_least), total_months(over)]
        .into_iter()
        .flatten()
        .min()
}

/// 対象から外れる月齢（含まない）。
///
/// 「未満」はそのまま。「以下」は、年だけなら次の誕生日の前まで（1歳以下 → 24か月未満）、
/// 月まであればその月まで（1歳6か月以下 → 19か月未満）。両方あれば表示する側（大きい方）に倒す。
///
/// 0か月未満のような上限は、生まれた子の月齢では誰も当てはまらない。入力の誤りか妊娠中を指したもので
/// （羽村市の相談窓口に、対象が「妊娠中の方…就学前のお子さんの保護者」なのに「0か月未満」の行がある）、
/// 絞り込みに使うと表示すべきものを隠すので捨てる。
fn age_max_months(less_than: &AgeBound, at_most: &AgeBound) -> Option<i32> {
    let at_most = match (at_most.target_age, at_most.target_age_of_months) {
        (None, None) => None,
        (Some(years), None) => Some((years + 1) * 12),
        (years, Some(months)) => Some(years.unwrap_or(0) * 12 + months + 1),
    };
    [total_months(less_than), at_most]
        .into_iter()
        .flatten()
        .max()
        .filter(|&months| months > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bound(years: Option<i32>, months: Option<i32>) -> AgeBound {
        AgeBound {
            target_age: years,
            target_age_of_months: months,
        }
    }
    const NONE: AgeBound = AgeBound {
        target_age: None,
        target_age_of_months: None,
    };

    #[test]
    fn um_is_the_last_psid_segment() {
        assert_eq!(
            um_from_psid("psid3.0+3000020131130+1+UM24").as_deref(),
            Some("UM24")
        );
        assert_eq!(
            um_from_psid("psid3.0+8000020131016+12+UM5013").as_deref(),
            Some("UM5013")
        );
        assert_eq!(um_from_psid("psid3.0+3000020131130+1+24"), None);
        assert_eq!(um_from_psid("psid3.0+3000020131130+1+UM"), None);
    }

    #[test]
    fn area_code_and_name_are_trimmed() {
        assert_eq!(
            split_area("133078;檜原村\t"),
            Some(("133078".to_string(), "檜原村".to_string()))
        );
        assert_eq!(split_area("13999;渋谷区"), None);
        assert_eq!(split_area("131130"), None);
    }

    #[test]
    fn municipalities_point_to_their_prefecture() {
        let names = BTreeMap::from([
            ("131130".to_string(), "渋谷区".to_string()),
            ("130001".to_string(), "東京都".to_string()),
            ("131016".to_string(), "千代田区".to_string()),
        ]);
        let areas = build_areas(names);
        assert_eq!(areas.len(), 3);

        // 並び順には頼らず、コードで探す
        let parent_of = |code: &str| {
            areas
                .iter()
                .find(|a| a.code == code)
                .unwrap_or_else(|| panic!("{code} が無い"))
                .parent_code
                .as_deref()
        };
        assert_eq!(parent_of("130001"), None);
        assert_eq!(parent_of("131130"), Some("130001"));
        assert_eq!(parent_of("131016"), Some("130001"));
    }

    #[test]
    fn less_than_is_exclusive_months() {
        // 1歳6か月未満 / 1歳未満 / 6か月未満（README §1 の例）
        assert_eq!(age_max_months(&bound(Some(1), Some(6)), &NONE), Some(18));
        assert_eq!(age_max_months(&bound(Some(1), None), &NONE), Some(12));
        assert_eq!(age_max_months(&bound(None, Some(6)), &NONE), Some(6));
    }

    #[test]
    fn at_most_includes_the_stated_age() {
        // 1歳以下は2歳の誕生日の前まで、1歳6か月以下は1歳7か月の前まで
        assert_eq!(age_max_months(&NONE, &bound(Some(1), None)), Some(24));
        assert_eq!(age_max_months(&NONE, &bound(Some(1), Some(6))), Some(19));
        assert_eq!(age_max_months(&NONE, &bound(None, Some(6))), Some(7));
    }

    #[test]
    fn upper_bound_of_zero_months_is_dropped() {
        assert_eq!(age_max_months(&bound(None, Some(0)), &NONE), None);
        // 0歳以下は「1歳の誕生日の前まで」なので残る
        assert_eq!(age_max_months(&NONE, &bound(Some(0), None)), Some(12));
    }

    #[test]
    fn min_age_leans_toward_showing() {
        assert_eq!(age_min_months(&bound(Some(1), Some(6)), &NONE), Some(18));
        assert_eq!(age_min_months(&NONE, &bound(Some(1), None)), Some(12));
        assert_eq!(age_min_months(&NONE, &NONE), None);
    }

    #[test]
    fn broken_json_is_not_blamed_on_a_row() {
        // 途中で切れた JSON は、行の読み取り失敗（Record）ではなく全体の失敗として返す
        let err = parse(r#"[{"basicInformation": "#).unwrap_err();
        assert!(matches!(err, ParseError::InvalidJson { .. }), "{err:?}");
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| v.to_string()).collect()
    }

    #[test]
    fn codes_are_split_trimmed_and_padded() {
        assert_eq!(normalize_codes(strings(&["002，003"])), ["002", "003"]);
        assert_eq!(normalize_codes(strings(&["027 "])), ["027"]);
        assert_eq!(normalize_codes(strings(&["86"])), ["086"]);
        assert_eq!(
            normalize_codes(strings(&["\u{3000}001、 002,", "002", ""])),
            ["001", "002"]
        );
        // 数字でないものは埋めずに残し、一覧との照合（警告）に回す
        assert_eq!(normalize_codes(strings(&["x"])), ["x"]);
    }

    /// 本番で見つかった誤記（小平市・武蔵野市・青梅市・北区）を、サンプルの行に入れ直して読む。
    #[test]
    fn registry_typos_are_normalized_and_misplaced_codes_are_reported() {
        let json = include_str!("../tests/fixtures/registry_sample.json");
        let mut rows: Vec<Value> = serde_json::from_str(json).unwrap();
        let base = rows[0].clone();
        let cases = [
            ("小平市", Some(vec!["002，003"]), Some(vec!["086"])),
            ("武蔵野市", Some(vec!["027 "]), Some(vec!["087"])),
            ("青梅市", Some(vec!["002"]), Some(vec!["86"])),
            ("北区", Some(vec!["087"]), Some(vec!["079"])),
            ("タグ無し", None, None),
        ];
        rows = cases
            .iter()
            .enumerate()
            .map(|(i, (_, category, target))| {
                let mut row = base.clone();
                row["basicInformation"]["psid"] =
                    format!("psid3.0+3000020131130+{}+UM{}", i + 1, i + 1).into();
                row["tag"]["categoryCode"] = serde_json::to_value(category).unwrap();
                row["tag"]["targetCode"] = serde_json::to_value(target).unwrap();
                row
            })
            .collect();

        let imported = parse(&serde_json::to_string(&rows).unwrap()).unwrap();
        let codes = |i: usize| {
            let p = &imported.programs[i];
            (p.category_codes.clone(), p.target_codes.clone())
        };
        assert_eq!(codes(0), (strings(&["002", "003"]), strings(&["086"])));
        assert_eq!(codes(1), (strings(&["027"]), strings(&["087"])));
        assert_eq!(codes(2), (strings(&["002"]), strings(&["086"])));
        assert_eq!(codes(4), (vec![], vec![]));

        let north = &imported.programs[3].psid;
        assert_eq!(
            imported.unknown_tags,
            [
                UnknownTag {
                    psid: north.clone(),
                    column: "category_codes",
                    value: "087".into(),
                },
                UnknownTag {
                    psid: north.clone(),
                    column: "target_codes",
                    value: "079".into(),
                },
            ]
        );
    }

    #[test]
    fn parses_a_registry_row() {
        let json = include_str!("../tests/fixtures/registry_sample.json");
        let imported = parse(json).expect("fixture parses");

        assert_eq!(imported.areas.len(), 2);
        let test_city = imported.areas.iter().find(|a| a.code == "131130").unwrap();
        assert_eq!(test_city.name, "渋谷区");
        assert_eq!(test_city.parent_code.as_deref(), Some("130001"));

        let birth = imported
            .programs
            .iter()
            .find(|p| p.um == "UM3")
            .expect("出生届");
        assert_eq!(birth.area_code, "131130");
        assert_eq!(birth.canonical_name, "出生届");
        assert_eq!(birth.content_codes, vec!["077"]);
        assert!(imported.unknown_tags.is_empty());
        assert_eq!(
            birth.registry["basicInformation"]["psid"],
            birth.psid.as_str()
        );
    }
}
