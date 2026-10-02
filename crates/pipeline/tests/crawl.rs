//! 取得に渡すものを DB から組み立てる部分を、実際の Postgres に流す。
//!
//! `TEST_DATABASE_URL` のデータベースは毎回作り直す（本番の接続先を入れないこと）。未設定ならスキップする。

mod common;

use std::collections::BTreeSet;

use common::*;
use domain::liveness::HostMove;
use domain::schedule::Policy;
use entity::fetch_history;
use pipeline::crawl::{allowed_hosts, host_trusted, validators};
use pipeline::host_moves;
use pipeline::lifecycle::Context;
use pipeline::persist::{Attempt, record};
use sea_orm::{ActiveValue::Set, DatabaseConnection, EntityTrait, prelude::Uuid};

fn moved(from: &str, to: &str) -> HostMove {
    HostMove {
        from_host_key: from.to_string(),
        to_host_key: to.to_string(),
    }
}

#[tokio::test]
async fn the_allow_list_adds_approved_host_moves_to_registered_hosts() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    register(&db, "https://www.a.example.jp/x.html").await;
    register(&db, "https://www.b.example.jp/y.html").await;
    host_moves::record(
        &db,
        &moved(
            "https://www.old1.example.jp",
            "https://www.proposed.example.jp",
        ),
        "https://www.old1.example.jp/a",
    )
    .await
    .unwrap();
    host_moves::record(
        &db,
        &moved("https://www.old2.example.jp", "https://www.new.example.jp"),
        "https://www.old2.example.jp/a",
    )
    .await
    .unwrap();
    host_moves::approve(
        &db,
        "https://www.old2.example.jp",
        "https://www.new.example.jp",
    )
    .await
    .unwrap();

    assert_eq!(
        allowed_hosts(&db).await.unwrap(),
        BTreeSet::from([
            "https://www.a.example.jp".to_string(),
            "https://www.b.example.jp".to_string(),
            "https://www.new.example.jp".to_string(),
        ])
    );
}

#[tokio::test]
async fn validators_come_from_the_current_resource() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let fetcher = serve(|_| {
        let mut response = body_page("本文");
        let headers = response.headers_mut();
        headers.insert("etag", "\"v1\"".parse().unwrap());
        headers.insert(
            "last-modified",
            "Mon, 01 Jan 2026 00:00:00 GMT".parse().unwrap(),
        );
        response
    })
    .await;
    let id = register(&db, &url("/a.html")).await;
    let run = start_run(&db).await;
    let fetch = fetcher.fetch(&url("/a.html"), &Default::default()).await;
    let extracted = pipeline::crawl::extract_of(&fetch);
    let ctx = Context {
        host_trusted: true,
        known_not_found_titles: &[],
    };
    record(
        &db,
        &Attempt {
            run_id: run,
            url_id: id,
            claim_token: 0,
            fetch: &fetch,
            extracted: extracted.as_ref(),
            ctx: &ctx,
            policy: &Policy::default(),
        },
    )
    .await
    .unwrap();

    let resource_id = all_resources(&db).await[0].id;
    let found = validators(&db, Some(resource_id)).await.unwrap();
    assert_eq!(found.etag.as_deref(), Some("\"v1\""));
    assert_eq!(
        found.last_modified.as_deref(),
        Some("Mon, 01 Jan 2026 00:00:00 GMT")
    );
    let none = validators(&db, None).await.unwrap();
    assert!(none.etag.is_none() && none.last_modified.is_none());
}

async fn declare(db: &DatabaseConnection, run_id: Uuid, url_id: Uuid, declared: &str) {
    fetch_history::Entity::insert(fetch_history::ActiveModel {
        run_id: Set(run_id),
        url_id: Set(url_id),
        outcome: Set("response".to_string()),
        declared_canonical_url: Set(Some(declared.to_string())),
        ..Default::default()
    })
    .exec_without_returning(db)
    .await
    .unwrap();
}

#[tokio::test]
async fn a_host_whose_pages_all_point_to_one_page_is_not_trusted() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let run = start_run(&db).await;
    for n in 1..=4 {
        let id = register(&db, &url(&format!("/p{n}.html"))).await;
        declare(&db, run, id, &url("/top.html")).await;
    }
    assert!(!host_trusted(&db, &format!("http://{CITY}")).await.unwrap());
    // 申告が無いホストは信用する
    assert!(
        host_trusted(&db, "https://www.other.example.jp")
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn ready_at_is_known_after_a_request() {
    let fetcher = serve(|_| page(None)).await;
    let host = format!("http://{CITY}");
    assert!(fetcher.ready_at(&host).is_none());
    fetcher.fetch(&url("/a.html"), &Default::default()).await;
    let at = fetcher.ready_at(&host).expect("送ったあとは分かる");
    assert!(at + std::time::Duration::from_secs(1) > tokio::time::Instant::now());
}
