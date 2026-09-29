//! 取得の結果の保存を、テスト用サーバーと実際の Postgres に流す。
//!
//! `TEST_DATABASE_URL` のデータベースは毎回作り直す（本番の接続先を入れないこと）。未設定ならスキップする。

mod common;

use common::*;
use entity::{content_versions, fetch_history};
use sea_orm::{ActiveValue::Set, DatabaseConnection, DbErr, EntityTrait, prelude::Uuid};

async fn insert_history(
    db: &DatabaseConnection,
    run_id: Uuid,
    url_id: Uuid,
) -> Result<u64, DbErr> {
    fetch_history::Entity::insert(fetch_history::ActiveModel {
        run_id: Set(run_id),
        url_id: Set(url_id),
        outcome: Set("network".to_string()),
        ..Default::default()
    })
    .exec_without_returning(db)
    .await
}

#[tokio::test]
async fn history_is_unique_per_run_and_url() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let url_id = register(&db, &url("/a.html")).await;
    let run = start_run(&db).await;

    insert_history(&db, run, url_id).await.unwrap();
    assert!(insert_history(&db, run, url_id).await.is_err());
    // 別の実行なら追記できる
    let other = start_run(&db).await;
    insert_history(&db, other, url_id).await.unwrap();
}

#[tokio::test]
async fn content_versions_are_unique_per_resource_and_body_hash() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let fetcher = serve(|_| page(None)).await;
    let url_id = register(&db, &url("/a.html")).await;
    let fetch = fetcher.fetch(&url("/a.html"), &Default::default()).await;
    let decision = pipeline::resources::decide(&fetch, None, true).unwrap();
    let linked = pipeline::resources::link(&db, url_id, &decision)
        .await
        .unwrap();
    let pipeline::resources::Linked::Linked { resource_id, .. } = linked else {
        panic!("結べる");
    };

    let insert = || {
        content_versions::Entity::insert(content_versions::ActiveModel {
            resource_id: Set(resource_id),
            body_hash: Set("h1".to_string()),
            ..Default::default()
        })
        .exec_without_returning(&db)
    };
    insert().await.unwrap();
    assert!(insert().await.is_err());
}
