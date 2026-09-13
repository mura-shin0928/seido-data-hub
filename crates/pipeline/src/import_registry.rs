//! レジストリの取り込み。psid で上書きするので、何度流しても同じ結果になる。

use anyhow::Context;
use domain::registry::{Imported, ImportedArea, ImportedProgram};
use entity::{areas, programs};
use sea_orm::sea_query::{Expr, OnConflict};
use sea_orm::{ActiveValue::Set, DatabaseConnection, EntityTrait, TransactionTrait};

/// Postgres のバインド変数の上限（65535）に収まるよう、制度は分けて書く。
const PROGRAM_CHUNK: usize = 500;

#[derive(Debug, PartialEq, Eq)]
pub struct Counts {
    pub areas: usize,
    pub programs: usize,
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

pub async fn import(db: &DatabaseConnection, imported: Imported) -> anyhow::Result<Counts> {
    let counts = Counts {
        areas: imported.areas.len(),
        programs: imported.programs.len(),
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

    let mut rows = imported.programs.into_iter().peekable();
    while rows.peek().is_some() {
        let chunk: Vec<_> = rows
            .by_ref()
            .take(PROGRAM_CHUNK)
            .map(program_model)
            .collect();
        programs::Entity::insert_many(chunk)
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

    txn.commit().await?;
    Ok(counts)
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
