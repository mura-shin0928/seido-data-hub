//! レジストリの取り込み。psid と `dedup_key` で上書きするので、何度流しても同じ結果になる。

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Context;
use domain::registry::{Imported, ImportedArea, ImportedProgram};
use domain::urls::{self, PreparedUrl, Rejection};
use entity::{areas, program_urls, programs, urls as urls_table};
use sea_orm::sea_query::{Expr, OnConflict};
use sea_orm::{
    ActiveValue::Set, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QuerySelect,
    TransactionTrait, prelude::Uuid,
};

/// Postgres のバインド変数の上限（65535）に収まるよう、まとめて書くときは分ける。
const CHUNK: usize = 500;

#[derive(Debug, PartialEq, Eq)]
pub struct Counts {
    pub areas: usize,
    pub programs: usize,
    /// 重複を畳んだ後の URL（`dedup_key` の数）
    pub urls: usize,
    /// 制度と主たる URL の結び付き
    pub program_urls: usize,
}

/// `urls` に入れなかった値。元データの誤りなので取り込みは止めず、呼び出し側でログに出す。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectedUrl {
    pub psid: String,
    pub raw_url: String,
    pub reason: Rejection,
}

#[derive(Debug)]
pub struct Outcome {
    pub counts: Counts,
    pub rejected: Vec<RejectedUrl>,
}

/// URL なら取得し、それ以外はファイルとして読む。
pub async fn load_source(source: &str) -> anyhow::Result<String> {
    if source.starts_with("https://") || source.starts_with("http://") {
        reqwest::get(source)
            .await
            .and_then(|res| res.error_for_status())
            .with_context(|| format!("{source} を取得できない"))?
            .text()
            .await
            .context("本文を読めない")
    } else {
        std::fs::read_to_string(source).with_context(|| format!("{source} を読めない"))
    }
}

/// 制度ごとの主たる URL。
struct MainUrl {
    psid: String,
    url: PreparedUrl,
}

/// レジストリの `localGovernmentLink.uri` から、制度ごとの主たる URL を作る。
///
/// カンマで連結された値は分け、先頭だけを使う。残りは推測で組み替えず捨てる。
fn prepare_main_urls(programs: &[ImportedProgram]) -> (Vec<MainUrl>, Vec<RejectedUrl>) {
    let mut prepared = Vec::with_capacity(programs.len());
    let mut rejected = Vec::new();
    for program in programs {
        let first = urls::split_joined(&program.source_url)
            .first()
            .copied()
            .unwrap_or_default();
        match urls::prepare(first) {
            Ok(url) => prepared.push(MainUrl {
                psid: program.psid.clone(),
                url,
            }),
            Err(reason) => rejected.push(RejectedUrl {
                psid: program.psid.clone(),
                raw_url: program.source_url.clone(),
                reason,
            }),
        }
    }

    // 許可リストはレジストリから作る。ここでは全部通るが、後から足す URL と同じ関門を通しておく
    let allowed: BTreeSet<String> = prepared
        .iter()
        .map(|main| main.url.host_key.clone())
        .collect();
    let (in_scope, out_of_scope): (Vec<_>, Vec<_>) = prepared
        .into_iter()
        .partition(|main| urls::in_scope(&main.url, &allowed).is_ok());
    rejected.extend(out_of_scope.into_iter().map(|main| RejectedUrl {
        psid: main.psid,
        raw_url: main.url.raw_url,
        reason: Rejection::HostNotAllowed {
            host_key: main.url.host_key,
        },
    }));

    (in_scope, rejected)
}

pub async fn import(db: &DatabaseConnection, imported: Imported) -> anyhow::Result<Outcome> {
    let (main_urls, rejected) = prepare_main_urls(&imported.programs);

    // 表記違いは最初に見たものを残す。制度ごとの元の表記は programs.registry にある
    let mut by_key: BTreeMap<&str, &PreparedUrl> = BTreeMap::new();
    for main in &main_urls {
        by_key.entry(&main.url.dedup_key).or_insert(&main.url);
    }

    let counts = Counts {
        areas: imported.areas.len(),
        programs: imported.programs.len(),
        urls: by_key.len(),
        program_urls: main_urls.len(),
    };
    let txn = db.begin().await?;

    // parent_code の外部キーは文の終わりに確かめられるので、1文で入れる限り都道府県と市区町村の並び順は問わない。
    // 文を分けて入れるようにするなら、都道府県を先に入れる必要がある
    areas::Entity::insert_many(imported.areas.into_iter().map(area_model))
        .on_conflict(
            OnConflict::column(areas::Column::Code)
                .update_columns([areas::Column::Name, areas::Column::ParentCode])
                .to_owned(),
        )
        .exec_without_returning(&txn)
        .await
        .context("areas を書けない")?;

    for chunk in imported.programs.chunks(CHUNK) {
        programs::Entity::insert_many(chunk.iter().cloned().map(program_model))
            .on_conflict(
                OnConflict::column(programs::Column::Psid)
                    .update_columns([
                        programs::Column::Um,
                        programs::Column::AreaCode,
                        programs::Column::CanonicalName,
                        programs::Column::ShortName,
                        programs::Column::SourceUrl,
                        programs::Column::CategoryCodes,
                        programs::Column::TargetCodes,
                        programs::Column::ContentCodes,
                        programs::Column::AgeMinMonths,
                        programs::Column::AgeMaxMonths,
                        programs::Column::Registry,
                    ])
                    .value(programs::Column::ImportedAt, Expr::current_timestamp())
                    .to_owned(),
            )
            .exec_without_returning(&txn)
            .await
            .context("programs を書けない")?;
    }

    // 巡回で進んだ状態（status・next_crawl_at・lease）を取り込み直しで戻さないため、既にある行は触らない
    let rows: Vec<_> = by_key.values().map(|url| url_model(url)).collect();
    for chunk in rows.chunks(CHUNK) {
        urls_table::Entity::insert_many(chunk.to_vec())
            .on_conflict(
                OnConflict::column(urls_table::Column::DedupKey)
                    .do_nothing()
                    .to_owned(),
            )
            .exec_without_returning(&txn)
            .await
            .context("urls を書けない")?;
    }

    let program_ids: BTreeMap<String, Uuid> = programs::Entity::find()
        .select_only()
        .columns([programs::Column::Psid, programs::Column::Id])
        .into_tuple::<(String, Uuid)>()
        .all(&txn)
        .await
        .context("programs の id を読めない")?
        .into_iter()
        .collect();
    let url_ids: BTreeMap<String, Uuid> = urls_table::Entity::find()
        .select_only()
        .columns([urls_table::Column::DedupKey, urls_table::Column::Id])
        .into_tuple::<(String, Uuid)>()
        .all(&txn)
        .await
        .context("urls の id を読めない")?
        .into_iter()
        .collect();

    // main は毎回レジストリから作り直す。URL が変わった制度の古い行が残らないようにするため。
    // 関連 URL（role = related）は消さない
    program_urls::Entity::delete_many()
        .filter(program_urls::Column::Role.eq("main"))
        .exec(&txn)
        .await
        .context("program_urls の main を消せない")?;

    let links: Vec<_> = main_urls
        .iter()
        .map(|main| {
            let program_id = program_ids
                .get(&main.psid)
                .with_context(|| format!("psid {} の制度が無い", main.psid))?;
            let url_id = url_ids
                .get(&main.url.dedup_key)
                .with_context(|| format!("{} の URL が無い", main.url.dedup_key))?;
            Ok(program_urls::ActiveModel {
                program_id: Set(*program_id),
                url_id: Set(*url_id),
                role: Set("main".to_string()),
                rank: Set(1),
                source: Set("registry".to_string()),
            })
        })
        .collect::<anyhow::Result<_>>()?;
    for chunk in links.chunks(CHUNK) {
        program_urls::Entity::insert_many(chunk.to_vec())
            .exec_without_returning(&txn)
            .await
            .context("program_urls を書けない")?;
    }

    txn.commit().await?;
    Ok(Outcome { counts, rejected })
}

fn area_model(area: ImportedArea) -> areas::ActiveModel {
    areas::ActiveModel {
        code: Set(area.code),
        name: Set(area.name),
        parent_code: Set(area.parent_code),
    }
}

// id・status・checked_at・imported_at は DB の既定値に任せる。status と checked_at は更新パイプラインの持ち物なので上書きもしない
fn program_model(p: ImportedProgram) -> programs::ActiveModel {
    programs::ActiveModel {
        psid: Set(p.psid),
        um: Set(p.um),
        area_code: Set(p.area_code),
        canonical_name: Set(p.canonical_name),
        short_name: Set(p.short_name),
        source_url: Set(p.source_url),
        category_codes: Set(p.category_codes),
        target_codes: Set(p.target_codes),
        content_codes: Set(p.content_codes),
        age_min_months: Set(p.age_min_months),
        age_max_months: Set(p.age_max_months),
        registry: Set(p.registry),
        ..Default::default()
    }
}

// role・depth・priority・status・next_crawl_at は DB の既定値に任せる。
// 巡回の状態は取り込みの持ち物ではない
fn url_model(url: &PreparedUrl) -> urls_table::ActiveModel {
    urls_table::ActiveModel {
        raw_url: Set(url.raw_url.clone()),
        normalized_url: Set(url.normalized_url.clone()),
        dedup_key: Set(url.dedup_key.clone()),
        host_key: Set(url.host_key.clone()),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn program(psid: &str, source_url: &str) -> ImportedProgram {
        ImportedProgram {
            psid: psid.to_string(),
            um: "UM1".to_string(),
            area_code: "131130".to_string(),
            canonical_name: "児童手当".to_string(),
            short_name: None,
            source_url: source_url.to_string(),
            category_codes: vec![],
            target_codes: vec![],
            content_codes: vec![],
            age_min_months: None,
            age_max_months: None,
            registry: json!({}),
        }
    }

    #[test]
    fn joined_values_keep_only_the_first_url() {
        let programs = [program(
            "a",
            "http://www.city.example.jp/PC/kodomo/20200715151457.html,http://www.city.example.jp/PC/kenkou/hpg000000397.html",
        )];
        let (main, rejected) = prepare_main_urls(&programs);
        assert!(rejected.is_empty(), "{rejected:?}");
        assert_eq!(
            main[0].url.normalized_url,
            "http://www.city.example.jp/PC/kodomo/20200715151457.html"
        );
    }

    #[test]
    fn shared_urls_are_folded_into_one_row() {
        // 同じ URL を複数の制度が指すときも、取得は1回で済ませる
        let programs = [
            program("a", "https://www.city.example.jp/a.html"),
            program("b", "https://www.city.example.jp/a.html#section"),
            program("c", "https://www.city.example.jp/b.html"),
        ];
        let (main, _) = prepare_main_urls(&programs);
        let keys: BTreeSet<_> = main.iter().map(|m| m.url.dedup_key.as_str()).collect();
        assert_eq!(main.len(), 3, "制度の数だけ結び付きは残る");
        assert_eq!(keys.len(), 2, "URL は畳まれる");
    }

    /// 実データでの件数確認。JSON はリポジトリに入れないので、パスを渡したときだけ動かす。
    ///
    /// ```sh
    /// REGISTRY_JSON=~/registry.json cargo test -p pipeline -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "レジストリ JSON のパスを REGISTRY_JSON で渡したときだけ動く"]
    fn real_registry_counts() {
        let Ok(path) = std::env::var("REGISTRY_JSON") else {
            panic!("REGISTRY_JSON にレジストリ JSON のパスを渡す");
        };
        let json = std::fs::read_to_string(path).expect("読める");
        let imported = domain::registry::parse(&json).expect("読める");
        let (main, rejected) = prepare_main_urls(&imported.programs);
        let keys: BTreeSet<_> = main.iter().map(|m| m.url.dedup_key.as_str()).collect();
        let raw: BTreeSet<_> = imported
            .programs
            .iter()
            .map(|p| p.source_url.as_str())
            .collect();
        println!(
            "行 {} / 重複を除いた raw_url {} / dedup_key {} / 登録できない URL {}",
            imported.programs.len(),
            raw.len(),
            keys.len(),
            rejected.len()
        );
        for reject in &rejected {
            println!("  登録できない: psid={} {:?}", reject.psid, reject.reason);
        }

        // 2026-09-21 に数えた値
        assert_eq!(imported.programs.len(), 7_812, "レジストリの行");
        assert_eq!(raw.len(), 5_315, "重複を除いた主たる URL");
        assert_eq!(keys.len(), 5_284, "dedup_key");
        assert_eq!(main.len(), 7_812, "全行が main の URL を持つ");
        assert!(rejected.is_empty(), "{rejected:?}");
    }

    #[test]
    fn unreadable_values_are_reported_with_the_original_text() {
        let programs = [program("a", "ちらしを参照")];
        let (main, rejected) = prepare_main_urls(&programs);
        assert!(main.is_empty());
        assert_eq!(
            rejected,
            [RejectedUrl {
                psid: "a".to_string(),
                raw_url: "ちらしを参照".to_string(),
                reason: Rejection::Unparsable,
            }]
        );
    }
}
