//! 許可リストの外へ動いたホストの記録と承認（§6 §17）。
//!
//! 自動では許可リストに入れない。人が確かめて `approve`・`approve_by_hand` したものだけが `approved_hosts` に出る。

use std::collections::BTreeSet;

use anyhow::Context;
use domain::liveness::HostMove;
use entity::host_moves;
use sea_orm::sea_query::{Expr, OnConflict};
use sea_orm::{ActiveValue::Set, ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter};

/// 見つけた移行を残す。同じ組は日時と例の URL だけ更新し、承認済みの状態は変えない
pub async fn record(
    db: &impl ConnectionTrait,
    mv: &HostMove,
    sample_url: &str,
) -> anyhow::Result<()> {
    host_moves::Entity::insert(host_moves::ActiveModel {
        from_host_key: Set(mv.from_host_key.clone()),
        to_host_key: Set(mv.to_host_key.clone()),
        sample_url: Set(sample_url.to_string()),
        ..Default::default()
    })
    .on_conflict(
        OnConflict::columns([
            host_moves::Column::FromHostKey,
            host_moves::Column::ToHostKey,
        ])
        .update_column(host_moves::Column::SampleUrl)
        .value(host_moves::Column::LastSeenAt, Expr::current_timestamp())
        .to_owned(),
    )
    .exec_without_returning(db)
    .await
    .context("host_moves を書けない")?;
    Ok(())
}

/// 観測済みの移行を承認する。観測していない組なら false（打ち間違いを許可リストに入れない）
pub async fn approve(
    db: &impl ConnectionTrait,
    from_host_key: &str,
    to_host_key: &str,
) -> anyhow::Result<bool> {
    let result = host_moves::Entity::update_many()
        .col_expr(host_moves::Column::Status, Expr::value("approved"))
        .filter(host_moves::Column::FromHostKey.eq(from_host_key))
        .filter(host_moves::Column::ToHostKey.eq(to_host_key))
        .exec(db)
        .await
        .context("host_moves を承認できない")?;
    Ok(result.rows_affected > 0)
}

/// 人が確かめた移行を承認する。観測していなくてもよい（旧ホストが消えた後は、転送はもう観測できない）。
/// 行が無ければ承認済みで入れ、あれば承認にするだけ（最初に見た日時と例の URL は残す）
pub async fn approve_by_hand(
    db: &impl ConnectionTrait,
    from_host_key: &str,
    to_host_key: &str,
    sample_url: &str,
) -> anyhow::Result<()> {
    host_moves::Entity::insert(host_moves::ActiveModel {
        from_host_key: Set(from_host_key.to_string()),
        to_host_key: Set(to_host_key.to_string()),
        status: Set("approved".to_string()),
        sample_url: Set(sample_url.to_string()),
        ..Default::default()
    })
    .on_conflict(
        OnConflict::columns([
            host_moves::Column::FromHostKey,
            host_moves::Column::ToHostKey,
        ])
        .value(host_moves::Column::Status, Expr::value("approved"))
        .to_owned(),
    )
    .exec_without_returning(db)
    .await
    .context("host_moves を承認できない")?;
    Ok(())
}

/// 許可リストに加えてよい、移行先のホスト
pub async fn approved_hosts(db: &impl ConnectionTrait) -> anyhow::Result<BTreeSet<String>> {
    let rows = host_moves::Entity::find()
        .filter(host_moves::Column::Status.eq("approved"))
        .all(db)
        .await
        .context("host_moves を読めない")?;
    Ok(rows.into_iter().map(|row| row.to_host_key).collect())
}
