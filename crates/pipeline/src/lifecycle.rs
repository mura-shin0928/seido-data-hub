//! 取得の結果を資源の状態に反映する（§12 §17）。
//!
//! 分類は `domain::liveness`、遷移は `domain::state`。ここは DB に当てるところ。
//! ハッシュ・validator・履歴の保存はしない。`changed_at` も動かさない（URL や状態が動いても内容は同じことが多い）。
//! 各関数は内部でトランザクション（渡された中ならセーブポイント）を使うので、呼び出し側が1つにまとめられる。

use anyhow::Context as _;
use domain::canonical::{Decision, Hop};
use domain::extract::Extracted;
use domain::liveness::{self, Ending, Input, Observation, Page, Verdict};
use domain::state::{self, State, Transition};
use entity::{resources, url_resources};
use sea_orm::sea_query::Expr;
use sea_orm::{
    ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect,
    TransactionSession, TransactionTrait, prelude::Uuid,
};

use crate::fetch::{Fetch, Outcome};
use crate::host_moves;
use crate::resources::{self as links, Linked, upsert_url_resource};

/// 取得の結果を分類する。`known_not_found_titles` はそのホストで既知の Not Found の `title_hash`
pub fn verdict(
    fetch: &Fetch,
    extracted: Option<&Extracted>,
    known_not_found_titles: &[&str],
) -> Verdict {
    let hops: Vec<Hop<'_>> = fetch
        .hops
        .iter()
        .map(|hop| Hop {
            url: &hop.url,
            status: hop.status,
        })
        .collect();
    let ending = match &fetch.outcome {
        Outcome::Response(response) => Ending::Response {
            status: response.status,
            page: extracted.map(|extracted| Page {
                title: extracted.title.as_deref(),
                title_hash: extracted.hashes.title.as_deref(),
            }),
        },
        Outcome::RobotsDenied { .. } => Ending::RobotsDenied,
        Outcome::RobotsUnavailable { .. } => Ending::RobotsUnavailable,
        Outcome::OutOfScope { location, .. } => Ending::OutOfScope { location },
        Outcome::MissingLocation
        | Outcome::TooManyRedirects { .. }
        | Outcome::RedirectLoop { .. } => Ending::RedirectAnomaly,
        Outcome::Network { .. } => Ending::Network,
    };
    liveness::classify(&Input {
        hops: &hops,
        ending,
        known_not_found_titles,
    })
}

/// 観測を資源に当てて、状態と見つからなかった回数を書く。日時は動かさない
pub async fn observe<C: TransactionTrait>(
    db: &C,
    resource_id: Uuid,
    observation: Observation,
) -> anyhow::Result<Transition> {
    let txn = db.begin().await?;
    let resource = resources::Entity::find_by_id(resource_id)
        .lock_exclusive()
        .one(&txn)
        .await
        .context("資源を読めない")?
        .context("資源が無い")?;
    let current = State::parse(&resource.state).context("知らない資源の状態")?;
    let transition = state::next(current, resource.consecutive_not_found, observation);
    resources::Entity::update_many()
        .col_expr(
            resources::Column::State,
            Expr::value(transition.state.as_str()),
        )
        .col_expr(
            resources::Column::ConsecutiveNotFound,
            Expr::value(transition.consecutive_not_found),
        )
        .col_expr(resources::Column::UpdatedAt, Expr::current_timestamp())
        .filter(resources::Column::Id.eq(resource_id))
        .exec(&txn)
        .await
        .context("資源の状態を書けない")?;
    txn.commit().await?;
    Ok(transition)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// 同じ内容が新しい URL へ動いた。資源は `active` のまま代表 URL を差し替えた
    Renamed {
        resource_id: Uuid,
        from: String,
        to: String,
    },
    /// 既にある別の資源に統合された。元の資源を `moved` にした
    Absorbed {
        from_resource_id: Uuid,
        into_resource_id: Uuid,
    },
    /// 吸収先が `moved`、または元の資源へ `moved` している。輪や鎖を作らないよう何も書かなかった
    Refused {
        from_resource_id: Uuid,
        into_resource_id: Uuid,
    },
}

/// URL の代表 URL が前回と変わった（`Linked::Changed`）ときの扱いを決めて書く。
///
/// 新しい代表 URL の資源がまだ無ければ、同じ内容が動いただけなので元の資源の代表 URL を書き換える。
/// 既にあれば、元の資源は吸収された（`moved`）ので、URL を吸収先に結ぶ。
pub async fn resolve_change<C: TransactionTrait>(
    db: &C,
    url_id: Uuid,
    from_resource_id: Uuid,
    decision: &Decision,
) -> anyhow::Result<Resolved> {
    let txn = db.begin().await?;
    let from = resources::Entity::find_by_id(from_resource_id)
        .lock_exclusive()
        .one(&txn)
        .await
        .context("資源を読めない")?
        .context("資源が無い")?;
    let target = resources::Entity::find()
        .filter(resources::Column::CanonicalUrl.eq(&decision.canonical_url))
        .lock_exclusive()
        .one(&txn)
        .await
        .context("移動先の資源を読めない")?;

    let resolved = match target {
        None => {
            resources::Entity::update_many()
                .col_expr(
                    resources::Column::CanonicalUrl,
                    Expr::value(&decision.canonical_url),
                )
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
                    Expr::value(decision.source.as_str()),
                )
                .col_expr(resources::Column::UpdatedAt, Expr::current_timestamp())
                .filter(resources::Column::Id.eq(from_resource_id))
                .exec(&txn)
                .await
                .context("代表 URL を書き換えられない")?;
            upsert_url_resource(&txn, url_id, from_resource_id, decision.relation).await?;
            Resolved::Renamed {
                resource_id: from_resource_id,
                from: from.canonical_url,
                to: decision.canonical_url.clone(),
            }
        }
        Some(target)
            if target.state == State::Moved.as_str()
                || target.moved_to_resource_id == Some(from_resource_id) =>
        {
            Resolved::Refused {
                from_resource_id,
                into_resource_id: target.id,
            }
        }
        Some(target) => {
            resources::Entity::update_many()
                .col_expr(resources::Column::State, Expr::value(State::Moved.as_str()))
                .col_expr(resources::Column::MovedToResourceId, Expr::value(target.id))
                .col_expr(resources::Column::UpdatedAt, Expr::current_timestamp())
                .filter(resources::Column::Id.eq(from_resource_id))
                .exec(&txn)
                .await
                .context("資源を moved にできない")?;
            upsert_url_resource(&txn, url_id, target.id, decision.relation).await?;
            Resolved::Absorbed {
                from_resource_id,
                into_resource_id: target.id,
            }
        }
    };
    txn.commit().await?;
    Ok(resolved)
}

/// URL のいまの資源（結び付きの観測が最も新しい行）
async fn current_resource<C: ConnectionTrait>(
    db: &C,
    url_id: Uuid,
) -> anyhow::Result<Option<Uuid>> {
    let row = url_resources::Entity::find()
        .filter(url_resources::Column::UrlId.eq(url_id))
        .order_by_desc(url_resources::Column::ObservedAt)
        .one(db)
        .await
        .context("URL のいまの資源を読めない")?;
    Ok(row.map(|row| row.resource_id))
}

pub struct Context<'a> {
    /// そのホストの canonical の申告を信用するか
    pub host_trusted: bool,
    /// そのホストで既知の Not Found の `title_hash`
    pub known_not_found_titles: &'a [&'a str],
}

#[derive(Debug)]
pub struct Processed {
    pub verdict: Verdict,
    /// 代表 URL を決めて結んだときだけ
    pub linked: Option<Linked>,
    /// 代表 URL が前回と変わったときだけ
    pub resolved: Option<Resolved>,
    /// 資源に観測を当てたときだけ
    pub transition: Option<Transition>,
}

/// 1回の取得の結果を反映する: 分類 → 代表 URL を決めて結ぶ → 変化を解決 → 状態を動かす。
///
/// 許可リストの外へのホスト移行が見つかったら `host_moves` に残す。
pub async fn process<C: ConnectionTrait + TransactionTrait>(
    db: &C,
    url_id: Uuid,
    fetch: &Fetch,
    extracted: Option<&Extracted>,
    ctx: &Context<'_>,
) -> anyhow::Result<Processed> {
    let verdict = verdict(fetch, extracted, ctx.known_not_found_titles);

    if let Some(host_move) = &verdict.host_move {
        let sample = fetch.hops.last().map_or("", |hop| hop.url.as_str());
        host_moves::record(db, host_move, sample).await?;
    }

    let declared = extracted.and_then(|extracted| extracted.declared_canonical_url.as_deref());
    let (linked, resolved, resource_id) = match links::decide(fetch, declared, ctx.host_trusted) {
        Some(decision) => {
            let linked = links::link(db, url_id, &decision).await?;
            match &linked {
                Linked::Linked { resource_id, .. } => {
                    let id = Some(*resource_id);
                    (Some(linked), None, id)
                }
                Linked::Changed { resource_id, .. } => {
                    let resolved = resolve_change(db, url_id, *resource_id, &decision).await?;
                    let id = match &resolved {
                        Resolved::Renamed { resource_id, .. } => Some(*resource_id),
                        Resolved::Absorbed {
                            into_resource_id, ..
                        } => Some(*into_resource_id),
                        Resolved::Refused { .. } => None,
                    };
                    (Some(linked), Some(resolved), id)
                }
            }
        }
        // 304 など、代表 URL を決め直さない応答は、前回の結び付きの資源に当てる
        None if verdict.observation.is_some() => (None, None, current_resource(db, url_id).await?),
        None => (None, None, None),
    };

    let transition = match (verdict.observation, resource_id) {
        (Some(observation), Some(resource_id)) => {
            Some(observe(db, resource_id, observation).await?)
        }
        _ => None,
    };
    Ok(Processed {
        verdict,
        linked,
        resolved,
        transition,
    })
}
