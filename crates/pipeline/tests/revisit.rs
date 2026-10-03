//! 取得のたびの変化の判定と、資源の last_changed_at・change_count の進み方

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use common::*;
use domain::change::Change;
use domain::extract;
use domain::schedule::Policy;
use entity::resources;
use pipeline::fetch::{Fetch, Fetcher};
use pipeline::lifecycle::Context;
use pipeline::persist::{Attempt, Recorded, record};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement, prelude::Uuid};

async fn record_fetch(db: &DatabaseConnection, url_id: Uuid, fetch: &Fetch) -> Recorded {
    let run_id = start_run(db).await;
    let extracted = extracted_of(fetch);
    let ctx = Context {
        host_trusted: true,
        known_not_found_titles: &[],
    };
    record(
        db,
        &Attempt {
            run_id,
            url_id,
            claim_token: 0,
            fetch,
            extracted: extracted.as_ref(),
            ctx: &ctx,
            policy: &Policy::default(),
        },
    )
    .await
    .unwrap()
}

/// 記録して、比べた結果を返す
async fn change_of(db: &DatabaseConnection, url_id: Uuid, fetch: &Fetch) -> Change {
    let Recorded::Saved { change, .. } = record_fetch(db, url_id, fetch).await else {
        panic!("保存される");
    };
    change
}

async fn only_resource(db: &DatabaseConnection) -> resources::Model {
    let all = all_resources(db).await;
    assert_eq!(all.len(), 1);
    all.into_iter().next().unwrap()
}

/// 取るたびに本文が切り替わるサーバー。0 = 初版、1 = 改訂、2 = 404
fn switchable() -> (Arc<AtomicU8>, impl std::future::Future<Output = Fetcher>) {
    let mode = Arc::new(AtomicU8::new(0));
    let served = {
        let mode = mode.clone();
        serve(move |_| match mode.load(Ordering::SeqCst) {
            0 => body_page("初版"),
            1 => body_page("改訂"),
            _ => reply(404).body(Default::default()).unwrap(),
        })
    };
    (mode, served)
}

async fn get(fetcher: &Fetcher) -> Fetch {
    fetcher.fetch(&url("/a.html"), &Default::default()).await
}

#[tokio::test]
async fn an_unchanged_second_fetch_does_not_touch_changed_at() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let fetcher = serve(|_| body_page("本文")).await;
    let id = register(&db, &url("/a.html")).await;

    assert_eq!(
        change_of(&db, id, &get(&fetcher).await).await,
        Change::Unknown
    );
    assert_eq!(
        change_of(&db, id, &get(&fetcher).await).await,
        Change::Unchanged
    );

    let resource = only_resource(&db).await;
    assert!(resource.last_changed_at.is_none());
    assert_eq!(resource.change_count, 0);
}

#[tokio::test]
async fn a_changed_body_sets_changed_at_and_counts_it() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let (mode, fetcher) = switchable();
    let fetcher = fetcher.await;
    let id = register(&db, &url("/a.html")).await;

    change_of(&db, id, &get(&fetcher).await).await;
    mode.store(1, Ordering::SeqCst);
    assert_eq!(
        change_of(&db, id, &get(&fetcher).await).await,
        Change::Changed
    );
    let resource = only_resource(&db).await;
    assert!(resource.last_changed_at.is_some());
    assert_eq!(resource.change_count, 1);

    mode.store(0, Ordering::SeqCst);
    assert_eq!(
        change_of(&db, id, &get(&fetcher).await).await,
        Change::Changed
    );
    assert_eq!(only_resource(&db).await.change_count, 2);
}

#[tokio::test]
async fn the_first_fetch_and_an_extractor_change_are_not_changes() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let fetcher = serve(|_| body_page("本文")).await;
    let id = register(&db, &url("/a.html")).await;

    assert_eq!(
        change_of(&db, id, &get(&fetcher).await).await,
        Change::Unknown
    );
    assert!(only_resource(&db).await.last_changed_at.is_none());

    db.execute_raw(Statement::from_sql_and_values(
        db.get_database_backend(),
        "UPDATE resources SET extractor_version = $1, body_hash = 'old'",
        [(extract::EXTRACTOR_VERSION - 1).into()],
    ))
    .await
    .unwrap();
    assert_eq!(
        change_of(&db, id, &get(&fetcher).await).await,
        Change::Unknown
    );

    let resource = only_resource(&db).await;
    assert!(resource.last_changed_at.is_none());
    assert_eq!(resource.change_count, 0);
    assert_eq!(resource.extractor_version, Some(extract::EXTRACTOR_VERSION));
}

#[tokio::test]
async fn a_304_is_unchanged() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let fetcher = serve(|_| body_page("本文")).await;
    let id = register(&db, &url("/a.html")).await;

    change_of(&db, id, &get(&fetcher).await).await;
    assert_eq!(
        change_of(&db, id, &not_modified("/a.html")).await,
        Change::Unchanged
    );
    assert!(only_resource(&db).await.last_changed_at.is_none());
}

#[tokio::test]
async fn a_restored_page_is_compared_with_its_last_alive_content() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let (mode, fetcher) = switchable();
    let fetcher = fetcher.await;
    let id = register(&db, &url("/a.html")).await;

    change_of(&db, id, &get(&fetcher).await).await;
    mode.store(2, Ordering::SeqCst);
    record_fetch(&db, id, &get(&fetcher).await).await;
    mode.store(0, Ordering::SeqCst);
    assert_eq!(
        change_of(&db, id, &get(&fetcher).await).await,
        Change::Unchanged
    );
    let resource = only_resource(&db).await;
    assert_eq!(resource.state, "active");
    assert!(resource.last_changed_at.is_none());

    mode.store(2, Ordering::SeqCst);
    record_fetch(&db, id, &get(&fetcher).await).await;
    mode.store(1, Ordering::SeqCst);
    assert_eq!(
        change_of(&db, id, &get(&fetcher).await).await,
        Change::Changed
    );
    assert_eq!(only_resource(&db).await.change_count, 1);
}

#[tokio::test]
async fn a_pdf_is_compared_by_its_raw_hash() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let id = register(&db, &url("/a.pdf")).await;

    change_of(&db, id, &pdf_fetch(200, b"%PDF-1.7 a")).await;
    assert_eq!(
        change_of(&db, id, &pdf_fetch(200, b"%PDF-1.7 a")).await,
        Change::Unchanged
    );
    assert_eq!(
        change_of(&db, id, &pdf_fetch(200, b"%PDF-1.7 b")).await,
        Change::Changed
    );
    assert_eq!(only_resource(&db).await.change_count, 1);
}
