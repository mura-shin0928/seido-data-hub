//! 取得に渡すものを DB から組み立てる（許可リスト・validator・ホストの canonical の信用・本文の取り出し）。

use std::collections::BTreeSet;

use anyhow::Context as _;
use domain::canonical::{self, Declaration};
use domain::extract::{self, Extracted};
use domain::urls;
use entity::{resources, urls as urls_table};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DbBackend, EntityTrait, FromQueryResult, QueryFilter,
    QuerySelect, Statement, prelude::Uuid,
};

use crate::fetch::{Body, Fetch, Outcome, Validators};
use crate::host_moves;

/// 許可リスト: urls.host_key の集合 ∪ 承認したホスト移行の移行先
pub async fn allowed_hosts(db: &impl ConnectionTrait) -> anyhow::Result<BTreeSet<String>> {
    let registered: Vec<String> = urls_table::Entity::find()
        .select_only()
        .column(urls_table::Column::HostKey)
        .distinct()
        .into_tuple()
        .all(db)
        .await
        .context("登録済みのホストを読めない")?;
    let mut hosts: BTreeSet<String> = registered.into_iter().collect();
    hosts.extend(host_moves::approved_hosts(db).await?);
    Ok(hosts)
}

/// いまの資源に保存した ETag・Last-Modified。資源が無ければ空
pub async fn validators(
    db: &impl ConnectionTrait,
    resource_id: Option<Uuid>,
) -> anyhow::Result<Validators> {
    let Some(resource_id) = resource_id else {
        return Ok(Validators::default());
    };
    let resource = resources::Entity::find()
        .filter(resources::Column::Id.eq(resource_id))
        .one(db)
        .await
        .context("資源の validator を読めない")?;
    Ok(resource
        .map(|resource| Validators {
            etag: resource.etag,
            last_modified: resource.last_modified,
        })
        .unwrap_or_default())
}

#[derive(FromQueryResult)]
struct DeclaredRow {
    page: String,
    declared: String,
}

/// そのホストの canonical の申告を信用するか。各 URL の最新の申告（fetch_history）を judge_host に掛ける
pub async fn host_trusted(db: &impl ConnectionTrait, host_key: &str) -> anyhow::Result<bool> {
    let rows = DeclaredRow::find_by_statement(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "SELECT u.normalized_url AS page, h.declared_canonical_url AS declared \
         FROM urls u \
         JOIN ( \
             SELECT DISTINCT ON (url_id) url_id, declared_canonical_url \
             FROM fetch_history \
             WHERE declared_canonical_url IS NOT NULL \
             ORDER BY url_id, fetched_at DESC \
         ) h ON h.url_id = u.id \
         WHERE u.host_key = $1",
        [host_key.into()],
    ))
    .all(db)
    .await
    .context("canonical の申告を読めない")?;

    // 読めない申告は捨てる
    let pairs: Vec<(String, String)> = rows
        .into_iter()
        .filter_map(|row| {
            let declared = urls::prepare(&row.declared).ok()?;
            Some((row.page, declared.normalized_url))
        })
        .collect();
    let declarations: Vec<Declaration<'_>> = pairs
        .iter()
        .map(|(page, declared)| Declaration { page, declared })
        .collect();
    Ok(canonical::judge_host(&declarations).is_none())
}

/// HTML を読みきった応答から本文を取り出す
pub fn extract_of(fetch: &Fetch) -> Option<Extracted> {
    match &fetch.outcome {
        Outcome::Response(response) => match &response.body {
            Body::Html(html) => Some(extract::extract(&html.text, &response.url)),
            _ => None,
        },
        _ => None,
    }
}
