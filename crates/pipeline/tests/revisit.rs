//! 取得のたびの変化の判定と、資源の last_changed_at・change_count の進み方

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use common::*;
use domain::change::Change;
use domain::extract;
use domain::schedule::Policy;
use entity::{resources, urls};
use pipeline::crawl::validators;
use pipeline::fetch::{Fetch, Fetcher};
use pipeline::lifecycle::Context;
use pipeline::persist::{Attempt, Recorded, record};
use sea_orm::{ConnectionTrait, DatabaseConnection, EntityTrait, Statement, prelude::Uuid};

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

const DAY: f64 = 86_400.0;

/// `urls.recrawl_interval_secs` を日で返す
async fn interval_days(db: &DatabaseConnection, id: Uuid) -> Option<f64> {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            db.get_database_backend(),
            "SELECT recrawl_interval_secs FROM urls WHERE id = $1",
            [id.into()],
        ))
        .await
        .unwrap()
        .unwrap();
    row.try_get::<Option<i32>>("", "recrawl_interval_secs")
        .unwrap()
        .map(|secs| f64::from(secs) / DAY)
}

/// 間隔が `days` 日で、次の時刻も同じだけ先（±60秒）
async fn assert_interval(db: &DatabaseConnection, id: Uuid, days: f64) {
    assert_eq!(interval_days(db, id).await, Some(days));
    let until = seconds_until_next(db, id).await as f64;
    assert!((until - days * DAY).abs() <= 60.0, "{until} 秒先");
}

async fn status_of(db: &DatabaseConnection, id: Uuid) -> String {
    urls::Entity::find_by_id(id)
        .one(db)
        .await
        .unwrap()
        .unwrap()
        .status
}

#[tokio::test]
async fn an_unchanged_page_is_revisited_less_often() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let fetcher = serve(|_| body_page("本文")).await;
    let id = register(&db, &url("/a.html")).await;

    for days in [7.0, 10.5, 14.0] {
        record_fetch(&db, id, &get(&fetcher).await).await;
        assert_interval(&db, id, days).await;
    }
}

#[tokio::test]
async fn a_changed_page_is_revisited_sooner() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let (mode, fetcher) = switchable();
    let fetcher = fetcher.await;
    let id = register(&db, &url("/a.html")).await;

    record_fetch(&db, id, &get(&fetcher).await).await;
    assert_interval(&db, id, 7.0).await;
    mode.store(1, Ordering::SeqCst);
    record_fetch(&db, id, &get(&fetcher).await).await;
    assert_interval(&db, id, 3.5).await;
}

#[tokio::test]
async fn a_retry_keeps_the_interval_and_the_next_success_adapts_from_it() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let failing = Arc::new(AtomicU8::new(0));
    let fetcher = {
        let failing = failing.clone();
        serve(move |_| {
            if failing.load(Ordering::SeqCst) == 1 {
                reply(503).body(Default::default()).unwrap()
            } else {
                body_page("本文")
            }
        })
        .await
    };
    let id = register(&db, &url("/a.html")).await;

    record_fetch(&db, id, &get(&fetcher).await).await;
    assert_interval(&db, id, 7.0).await;

    failing.store(1, Ordering::SeqCst);
    record_fetch(&db, id, &get(&fetcher).await).await;
    assert_eq!(status_of(&db, id).await, "retry_wait");
    assert_eq!(interval_days(&db, id).await, Some(7.0));
    assert!((seconds_until_next(&db, id).await - 10).abs() <= 6);

    failing.store(0, Ordering::SeqCst);
    record_fetch(&db, id, &get(&fetcher).await).await;
    assert_interval(&db, id, 10.5).await;
}

#[tokio::test]
async fn a_not_found_page_switches_to_the_deletion_interval_and_back() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let (mode, fetcher) = switchable();
    let fetcher = fetcher.await;
    let id = register(&db, &url("/a.html")).await;

    record_fetch(&db, id, &get(&fetcher).await).await;
    db.execute_raw(Statement::from_sql_and_values(
        db.get_database_backend(),
        "UPDATE urls SET recrawl_interval_secs = $1 WHERE id = $2",
        [(14 * 86_400).into(), id.into()],
    ))
    .await
    .unwrap();

    mode.store(2, Ordering::SeqCst);
    record_fetch(&db, id, &get(&fetcher).await).await;
    assert_eq!(only_resource(&db).await.state, "deletion_candidate");
    assert_interval(&db, id, 7.0).await;

    mode.store(0, Ordering::SeqCst);
    record_fetch(&db, id, &get(&fetcher).await).await;
    assert_eq!(only_resource(&db).await.state, "active");
    assert_interval(&db, id, 10.5).await;
}

#[tokio::test]
async fn a_blocked_url_waits_longer_each_time() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let fetcher = serve(|_| reply(403).body(Default::default()).unwrap()).await;
    let id = register(&db, &url("/a.html")).await;

    for days in [7.0, 10.5] {
        record_fetch(&db, id, &get(&fetcher).await).await;
        assert_eq!(status_of(&db, id).await, "blocked");
        assert_interval(&db, id, days).await;
    }
}

fn days(n: u64) -> Duration {
    Duration::from_secs(n * 86_400)
}

/// ETag "v1" 付きの HTML を返すサーバー
async fn etagged() -> Fetcher {
    serve(|_| {
        let mut response = body_page("本文");
        response
            .headers_mut()
            .insert("etag", "\"v1\"".parse().unwrap());
        response
    })
    .await
}

async fn age_history(db: &DatabaseConnection, url_id: Uuid, days: i32) {
    db.execute_raw(Statement::from_sql_and_values(
        db.get_database_backend(),
        "UPDATE fetch_history SET fetched_at = now() - make_interval(days => $2) WHERE url_id = $1",
        [url_id.into(), days.into()],
    ))
    .await
    .unwrap();
}

#[tokio::test]
async fn validators_are_dropped_once_the_last_full_read_is_four_weeks_old() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let fetcher = etagged().await;
    let id = register(&db, &url("/a.html")).await;

    change_of(&db, id, &get(&fetcher).await).await;
    let resource_id = only_resource(&db).await.id;
    let found = validators(&db, id, Some(resource_id), days(28))
        .await
        .unwrap();
    assert_eq!(found.etag.as_deref(), Some("\"v1\""));

    age_history(&db, id, 29).await;
    let found = validators(&db, id, Some(resource_id), days(28))
        .await
        .unwrap();
    assert!(found.etag.is_none() && found.last_modified.is_none());

    // 本文を読まない 304 は「最後に読んだ時刻」を新しくしない
    change_of(&db, id, &not_modified("/a.html")).await;
    let found = validators(&db, id, Some(resource_id), days(28))
        .await
        .unwrap();
    assert!(found.etag.is_none() && found.last_modified.is_none());

    // 本文を読んだら、また validator を送る
    change_of(&db, id, &get(&fetcher).await).await;
    let found = validators(&db, id, Some(resource_id), days(28))
        .await
        .unwrap();
    assert_eq!(found.etag.as_deref(), Some("\"v1\""));
}

#[tokio::test]
async fn a_url_that_never_read_a_body_is_fetched_without_validators() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let fetcher = etagged().await;
    let a = register(&db, &url("/a.html")).await;
    let b = register(&db, &url("/b.html")).await;

    change_of(&db, a, &get(&fetcher).await).await;
    let resource_id = links_of(&db, a).await.first().unwrap().resource_id;
    db.execute_raw(Statement::from_sql_and_values(
        db.get_database_backend(),
        "INSERT INTO url_resources (url_id, resource_id, relation) VALUES ($1, $2, 'content_match')",
        [b.into(), resource_id.into()],
    ))
    .await
    .unwrap();

    let for_b = validators(&db, b, Some(resource_id), days(28))
        .await
        .unwrap();
    assert!(for_b.etag.is_none() && for_b.last_modified.is_none());
    let for_a = validators(&db, a, Some(resource_id), days(28))
        .await
        .unwrap();
    assert_eq!(for_a.etag.as_deref(), Some("\"v1\""));
}
