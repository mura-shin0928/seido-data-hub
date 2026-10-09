//! 主たる URL が死んだ制度に、関連 URL を結ぶ。
//!
//! 巡回の前に流す。前回の巡回で主たる URL が消えた（`deleted`・`deletion_candidate`）制度について、
//! レジストリの行から取り出した関連 URL を `urls` に登録して結ぶ。続く巡回がそれを取る。
//!
//! 死んだ制度の `role = 'related'` は毎回作り直す。生き返った制度（主たる URL がまた `active`）の結び付きは
//! 消さない（触るのは死んでいる制度だけ。関連 URL は一度結んだら案内として残す）。

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Context;
use domain::related::{Reason, related};
use domain::urls::PreparedUrl;
use entity::{program_urls, urls as urls_table};
use sea_orm::sea_query::OnConflict;
use sea_orm::{
    ActiveValue::Set, ColumnTrait, ConnectionTrait, DatabaseConnection, DbBackend, EntityTrait,
    PaginatorTrait, QueryFilter, QuerySelect, Statement, TransactionTrait, prelude::Uuid,
};

use crate::crawl::allowed_hosts;

/// Postgres のバインド変数の上限（65535）に収まるよう、まとめて書くときは分ける。
const CHUNK: usize = 500;

/// 新しく入れる関連 URL の優先度。主たる URL（80）より後に取る
pub const RELATED_PRIORITY: i32 = 70;

/// 1制度と1つの関連 URL の結び付き
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub program_id: Uuid,
    pub rank: i32,
    pub url: PreparedUrl,
}

/// 書かずに調べた結果
#[derive(Debug, Default)]
pub struct Survey {
    /// 主たる URL のいまの資源が deleted・deletion_candidate の制度
    pub dead_programs: Vec<Uuid>,
    pub links: Vec<Link>,
    /// 落とした理由ごとの件数
    pub dropped: BTreeMap<Reason, usize>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Counts {
    pub dead_programs: usize,
    /// 候補が1件以上あった制度
    pub programs_with_related: usize,
    /// 結び付き（program_urls の related）
    pub links: usize,
    /// 重複を畳んだ URL のうち、今回 urls に入れたもの・既にあったもの
    pub new_urls: usize,
    pub existing_urls: usize,
    pub dropped: BTreeMap<Reason, usize>,
}

/// 主たる URL のいまの資源（`observed_at` が最新の結び付き）が死んでいる制度。
/// 最新の資源の取り方は `claim.rs` と同じ
const DEAD_PROGRAMS: &str = "SELECT p.id, p.registry, u.raw_url, u.normalized_url, u.dedup_key, u.host_key \
    FROM programs p \
    JOIN program_urls pu ON pu.program_id = p.id AND pu.role = 'main' \
    JOIN urls u ON u.id = pu.url_id \
    JOIN LATERAL (\
        SELECT ur.resource_id FROM url_resources ur \
        WHERE ur.url_id = u.id ORDER BY ur.observed_at DESC LIMIT 1) current ON TRUE \
    JOIN resources r ON r.id = current.resource_id \
    WHERE r.state IN ('deleted', 'deletion_candidate') \
    ORDER BY p.psid";

/// 死んだ制度と、その関連 URL の候補を調べる（書かない）
pub async fn survey(db: &impl ConnectionTrait) -> anyhow::Result<Survey> {
    let allowed = allowed_hosts(db).await?;
    let rows = db
        .query_all_raw(Statement::from_string(DbBackend::Postgres, DEAD_PROGRAMS))
        .await
        .context("死んだ制度を読めない")?;

    let mut survey = Survey::default();
    for row in rows {
        let program_id: Uuid = row.try_get("", "id")?;
        let registry: serde_json::Value = row.try_get("", "registry")?;
        let main = PreparedUrl {
            raw_url: row.try_get("", "raw_url")?,
            normalized_url: row.try_get("", "normalized_url")?,
            dedup_key: row.try_get("", "dedup_key")?,
            host_key: row.try_get("", "host_key")?,
        };
        let found = related(&registry, &main, &allowed);
        survey.dead_programs.push(program_id);
        for (_, reason) in found.dropped {
            *survey.dropped.entry(reason).or_default() += 1;
        }
        survey
            .links
            .extend(found.candidates.into_iter().map(|candidate| Link {
                program_id,
                rank: candidate.rank,
                url: candidate.url,
            }));
    }
    Ok(survey)
}

/// 死んだ制度の関連 URL を作り直す。1つのトランザクションで、何度流しても同じ結果になる
pub async fn link(db: &DatabaseConnection) -> anyhow::Result<Counts> {
    let txn = db.begin().await?;
    let survey = survey(&txn).await?;
    let mut counts = Counts {
        dead_programs: survey.dead_programs.len(),
        programs_with_related: survey
            .links
            .iter()
            .map(|link| link.program_id)
            .collect::<BTreeSet<_>>()
            .len(),
        links: survey.links.len(),
        new_urls: 0,
        existing_urls: 0,
        dropped: survey.dropped,
    };
    if survey.dead_programs.is_empty() {
        return Ok(counts);
    }

    // 表記違いは最初に見たものを残す
    let mut by_key: BTreeMap<&str, &PreparedUrl> = BTreeMap::new();
    for link in &survey.links {
        by_key.entry(&link.url.dedup_key).or_insert(&link.url);
    }

    // 既にある行は触らない。巡回で進んだ状態（status・next_crawl_at・lease・priority）を戻さないため
    let before = urls_table::Entity::find().count(&txn).await? as usize;
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
    let after = urls_table::Entity::find().count(&txn).await? as usize;
    counts.new_urls = after - before;
    counts.existing_urls = by_key.len() - counts.new_urls;

    for chunk in survey.dead_programs.chunks(CHUNK) {
        program_urls::Entity::delete_many()
            .filter(program_urls::Column::Role.eq("related"))
            .filter(program_urls::Column::ProgramId.is_in(chunk.iter().copied()))
            .exec(&txn)
            .await
            .context("program_urls の related を消せない")?;
    }

    let mut url_ids: BTreeMap<String, Uuid> = BTreeMap::new();
    let keys: Vec<&str> = by_key.keys().copied().collect();
    for chunk in keys.chunks(CHUNK) {
        let found = urls_table::Entity::find()
            .select_only()
            .columns([urls_table::Column::DedupKey, urls_table::Column::Id])
            .filter(urls_table::Column::DedupKey.is_in(chunk.iter().copied()))
            .into_tuple::<(String, Uuid)>()
            .all(&txn)
            .await
            .context("urls の id を読めない")?;
        url_ids.extend(found);
    }
    let rows: Vec<_> = survey
        .links
        .iter()
        .map(|link| {
            let url_id = url_ids
                .get(&link.url.dedup_key)
                .with_context(|| format!("{} の URL が無い", link.url.dedup_key))?;
            Ok(program_urls::ActiveModel {
                program_id: Set(link.program_id),
                url_id: Set(*url_id),
                role: Set("related".to_string()),
                rank: Set(link.rank),
                source: Set("registry".to_string()),
            })
        })
        .collect::<anyhow::Result<_>>()?;
    for chunk in rows.chunks(CHUNK) {
        program_urls::Entity::insert_many(chunk.to_vec())
            .exec_without_returning(&txn)
            .await
            .context("program_urls を書けない")?;
    }

    txn.commit().await?;
    Ok(counts)
}

// role・depth・status・next_crawl_at は DB の既定値に任せる。巡回の状態は結び付けの持ち物ではない
fn url_model(url: &PreparedUrl) -> urls_table::ActiveModel {
    urls_table::ActiveModel {
        raw_url: Set(url.raw_url.clone()),
        normalized_url: Set(url.normalized_url.clone()),
        dedup_key: Set(url.dedup_key.clone()),
        host_key: Set(url.host_key.clone()),
        priority: Set(RELATED_PRIORITY),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 巡回済みの DB での件数確認。読むだけで、書かない。
    ///
    /// ```sh
    /// SWEEP_DATABASE_URL=postgres://postgres:postgres@localhost:55432/seido_data_hub_sweep \
    ///   cargo test -p pipeline -- --ignored --nocapture real_sweep_counts
    /// ```
    #[tokio::test]
    #[ignore = "巡回済みの DB を SWEEP_DATABASE_URL で渡したときだけ動く"]
    async fn real_sweep_counts() {
        let Ok(url) = std::env::var("SWEEP_DATABASE_URL") else {
            panic!("SWEEP_DATABASE_URL に巡回済みの DB を渡す");
        };
        let db = sea_orm::Database::connect(&url).await.expect("接続できる");
        let survey = survey(&db).await.expect("調べられる");
        let with_related = survey
            .links
            .iter()
            .map(|link| link.program_id)
            .collect::<BTreeSet<_>>()
            .len();
        let urls = survey
            .links
            .iter()
            .map(|link| link.url.dedup_key.as_str())
            .collect::<BTreeSet<_>>()
            .len();
        println!(
            "死んだ制度 {} / 候補のある制度 {with_related} / 結び付き {} / 畳んだ URL {urls}",
            survey.dead_programs.len(),
            survey.links.len()
        );
        for (reason, count) in &survey.dropped {
            println!("  落とした: {} {count}", reason.label());
        }

        // 2026-10-09 に数えた値（簡易な正規化での見積もりは 1,130 / 489 / 559）
        assert_eq!(survey.dead_programs.len(), 1_130);
        assert_eq!(with_related, 490);
        assert_eq!(urls, 566);
        assert_eq!(survey.links.len(), 983);

        // 小金井市の児童手当に teatekaisei.html が出る
        let koganei = db
            .query_one_raw(Statement::from_string(
                DbBackend::Postgres,
                "select id from programs where psid = 'psid3.0+3000020132101+1+UM24'",
            ))
            .await
            .unwrap()
            .expect("小金井市の児童手当がある");
        let koganei: Uuid = koganei.try_get_by_index(0).unwrap();
        assert!(
            survey.links.iter().any(|link| link.program_id == koganei
                && link.url.normalized_url.ends_with("/teatekaisei.html")),
            "teatekaisei.html が候補に無い"
        );
    }
}
