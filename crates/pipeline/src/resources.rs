//! 取得の結果から代表 URL を決め、URL を資源に結ぶ（§8 §21）。
//!
//! 資源の状態（`state`）・ハッシュ・validator は書かない。代表 URL が前回と変わったときも書き換えず、
//! 変わったことを返す（同じ内容の移動か、別の資源への統合かの判断は呼び出し側）。

use anyhow::Context;
use domain::canonical::{self, Action, Decision, Hop, Observed, Source};
use entity::{resources, url_resources};
use sea_orm::sea_query::{Expr, OnConflict};
use sea_orm::{
    ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter, QueryOrder, TransactionSession,
    TransactionTrait, prelude::Uuid,
};

use crate::fetch::{Fetch, Outcome};

/// 資源に結ぶ取得なら、代表 URL を決める。
///
/// 決めないのは、応答が無い（robots・通信エラー・scope 外への転送など）か、最終応答が資源に触らない
/// status のとき。304 もここに入る（前回の結び付きをそのまま使う）。
/// `declared` は最終応答の HTML にあった `rel=canonical`、`host_trusted` はそのホストの申告を信用するか。
pub fn decide(fetch: &Fetch, declared: Option<&str>, host_trusted: bool) -> Option<Decision> {
    let Outcome::Response(response) = &fetch.outcome else {
        return None;
    };
    if canonical::action(response.status) != Action::Link {
        return None;
    }
    let hops: Vec<Hop<'_>> = fetch
        .hops
        .iter()
        .map(|hop| Hop {
            url: &hop.url,
            status: hop.status,
        })
        .collect();
    canonical::decide(&Observed {
        hops: &hops,
        declared,
        host_trusted,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Linked {
    /// 資源に結んだ（`created` は資源をこのとき作ったか）
    Linked { resource_id: Uuid, created: bool },
    /// URL はいま別の代表 URL の資源に結ばれている。書き換えていない
    Changed {
        resource_id: Uuid,
        previous_canonical_url: String,
        canonical_url: String,
    },
}

/// URL を代表 URL の資源に結ぶ。資源が無ければ作る。同じ結果で何度呼んでも行は増えない。
///
/// 呼び出し側のトランザクションを渡せば、その中で書く（中ではセーブポイントを使う）。
pub async fn link<C: TransactionTrait>(
    db: &C,
    url_id: Uuid,
    decision: &Decision,
) -> anyhow::Result<Linked> {
    let txn = db.begin().await?;

    let current = url_resources::Entity::find()
        .filter(url_resources::Column::UrlId.eq(url_id))
        .order_by_desc(url_resources::Column::ObservedAt)
        .find_also_related(resources::Entity)
        .one(&txn)
        .await
        .context("URL のいまの資源を読めない")?;
    if let Some((_, Some(resource))) = current
        && resource.canonical_url != decision.canonical_url
    {
        return Ok(Linked::Changed {
            resource_id: resource.id,
            previous_canonical_url: resource.canonical_url,
            canonical_url: decision.canonical_url.clone(),
        });
    }

    let inserted = resources::Entity::insert(resources::ActiveModel {
        canonical_url: Set(decision.canonical_url.clone()),
        final_url: Set(decision.final_url.clone()),
        declared_canonical_url: Set(decision.declared_canonical_url.clone()),
        canonical_source: Set(decision.source.as_str().to_string()),
        ..Default::default()
    })
    .on_conflict(
        OnConflict::column(resources::Column::CanonicalUrl)
            .do_nothing()
            .to_owned(),
    )
    .exec_without_returning(&txn)
    .await
    .context("resources を書けない")?;
    let created = inserted == 1;

    let resource = resources::Entity::find()
        .filter(resources::Column::CanonicalUrl.eq(&decision.canonical_url))
        .one(&txn)
        .await
        .context("resources を読めない")?
        .context("作ったはずの資源が無い")?;
    let resource_id = resource.id;

    if !created {
        // 根拠は強い方を残す（転送 > 申告 > 正規化）。同じ資源に別の結び方の URL が来るため
        let source = Source::parse(&resource.canonical_source)
            .map_or(decision.source, |current| current.max(decision.source));
        resources::Entity::update_many()
            .col_expr(
                resources::Column::FinalUrl,
                Expr::value(&decision.final_url),
            )
            .col_expr(
                resources::Column::DeclaredCanonicalUrl,
                Expr::value(decision.declared_canonical_url.clone()),
            )
            .col_expr(
                resources::Column::CanonicalSource,
                Expr::value(source.as_str()),
            )
            .col_expr(resources::Column::UpdatedAt, Expr::current_timestamp())
            .filter(resources::Column::Id.eq(resource_id))
            .exec(&txn)
            .await
            .context("resources を更新できない")?;
    }

    url_resources::Entity::insert(url_resources::ActiveModel {
        url_id: Set(url_id),
        resource_id: Set(resource_id),
        relation: Set(decision.relation.as_str().to_string()),
        ..Default::default()
    })
    .on_conflict(
        OnConflict::columns([
            url_resources::Column::UrlId,
            url_resources::Column::ResourceId,
        ])
        .update_column(url_resources::Column::Relation)
        .value(url_resources::Column::ObservedAt, Expr::current_timestamp())
        .to_owned(),
    )
    .exec_without_returning(&txn)
    .await
    .context("url_resources を書けない")?;

    txn.commit().await?;
    Ok(Linked::Linked {
        resource_id,
        created,
    })
}
