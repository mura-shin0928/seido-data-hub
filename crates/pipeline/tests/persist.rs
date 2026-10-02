//! 取得の結果の保存を、テスト用サーバーと実際の Postgres に流す。
//!
//! `TEST_DATABASE_URL` のデータベースは毎回作り直す（本番の接続先を入れないこと）。未設定ならスキップする。

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::time::Duration;

use common::*;
use domain::extract;
use domain::schedule::Policy;
use entity::{content_versions, fetch_history, resources, urls};
use pipeline::fetch::{Body, Fetch, Fetcher, Hop, NetworkError, Outcome, Response};
use pipeline::lifecycle::Context;
use pipeline::persist::{Attempt, Recorded, record};
use sea_orm::{
    ActiveValue::Set, ColumnTrait, ConnectionTrait, DatabaseConnection, DbErr, EntityTrait,
    PaginatorTrait, QueryFilter, Statement, TransactionTrait, prelude::Uuid,
};

async fn insert_history(db: &DatabaseConnection, run_id: Uuid, url_id: Uuid) -> Result<u64, DbErr> {
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

fn empty(status: u16) -> axum::response::Response {
    reply(status).body(Default::default()).unwrap()
}

async fn record_fetch<C: ConnectionTrait + TransactionTrait>(
    db: &C,
    run_id: Uuid,
    url_id: Uuid,
    claim_token: i64,
    fetch: &Fetch,
) -> Recorded {
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
            claim_token,
            fetch,
            extracted: extracted.as_ref(),
            ctx: &ctx,
            policy: &Policy::default(),
        },
    )
    .await
    .unwrap()
}

async fn get(fetcher: &Fetcher, path: &str) -> Fetch {
    fetcher.fetch(&url(path), &Default::default()).await
}

async fn url_row(db: &DatabaseConnection, id: Uuid) -> urls::Model {
    urls::Entity::find_by_id(id).one(db).await.unwrap().unwrap()
}

async fn history_count(db: &DatabaseConnection) -> u64 {
    fetch_history::Entity::find().count(db).await.unwrap()
}

#[tokio::test]
async fn the_same_run_does_not_count_a_404_twice() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let fetcher = serve(|_| empty(404)).await;
    let id = register(&db, &url("/gone.html")).await;
    let fetch = get(&fetcher, "/gone.html").await;
    let run = start_run(&db).await;

    assert!(matches!(
        record_fetch(&db, run, id, 0, &fetch).await,
        Recorded::Saved { .. }
    ));
    assert!(matches!(
        record_fetch(&db, run, id, 0, &fetch).await,
        Recorded::AlreadyRecorded
    ));

    let resource = &all_resources(&db).await[0];
    assert_eq!(resource.consecutive_not_found, 1);
    assert_eq!(history_count(&db).await, 1);
    assert_eq!(url_row(&db, id).await.attempt_count, 1);

    // 別の実行なら、もう1回として数える
    let next = start_run(&db).await;
    record_fetch(&db, next, id, 0, &fetch).await;
    assert_eq!(all_resources(&db).await[0].consecutive_not_found, 2);
    assert_eq!(history_count(&db).await, 2);
}

#[tokio::test]
async fn a_stale_claim_writes_nothing() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let fetcher = serve(|_| page(None)).await;
    let id = register(&db, &url("/a.html")).await;
    urls::Entity::update_many()
        .col_expr(
            urls::Column::ClaimToken,
            sea_orm::sea_query::Expr::value(5_i64),
        )
        .filter(urls::Column::Id.eq(id))
        .exec(&db)
        .await
        .unwrap();
    let fetch = get(&fetcher, "/a.html").await;
    let run = start_run(&db).await;

    assert!(matches!(
        record_fetch(&db, run, id, 4, &fetch).await,
        Recorded::StaleClaim
    ));
    assert_eq!(history_count(&db).await, 0);
    assert!(all_resources(&db).await.is_empty());
    let row = url_row(&db, id).await;
    assert_eq!((row.status.as_str(), row.attempt_count), ("queued", 0));

    // 正しい token なら書ける
    assert!(matches!(
        record_fetch(&db, run, id, 5, &fetch).await,
        Recorded::Saved { .. }
    ));
}

#[tokio::test]
async fn a_crash_before_commit_leaves_nothing_and_the_next_try_goes_through() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let fetcher = serve(|_| page(None)).await;
    let id = register(&db, &url("/a.html")).await;
    let fetch = get(&fetcher, "/a.html").await;
    let run = start_run(&db).await;

    // すべて書き終えたところでコミットせずに捨てる（落ちたのと同じ）
    let txn = db.begin().await.unwrap();
    assert!(matches!(
        record_fetch(&txn, run, id, 0, &fetch).await,
        Recorded::Saved { .. }
    ));
    txn.rollback().await.unwrap();

    assert_eq!(history_count(&db).await, 0);
    assert!(all_resources(&db).await.is_empty());
    assert!(links_of(&db, id).await.is_empty());
    let row = url_row(&db, id).await;
    assert_eq!((row.status.as_str(), row.attempt_count), ("queued", 0));

    // 同じ実行の続きとして、そのまま通る
    assert!(matches!(
        record_fetch(&db, run, id, 0, &fetch).await,
        Recorded::Saved { .. }
    ));
    assert_eq!(url_row(&db, id).await.status, "succeeded");
    assert_eq!(history_count(&db).await, 1);
}

#[tokio::test]
async fn the_job_follows_the_verdict_and_retries_are_counted() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let status = Arc::new(AtomicU16::new(503));
    let fetcher = {
        let status = status.clone();
        serve(move |_| match status.load(Ordering::SeqCst) {
            200 => titled("制度"),
            other => empty(other),
        })
        .await
    };
    let id = register(&db, &url("/a.html")).await;

    let mut seen = Vec::new();
    for code in [503, 503, 200, 403] {
        status.store(code, Ordering::SeqCst);
        let run = start_run(&db).await;
        let fetch = get(&fetcher, "/a.html").await;
        record_fetch(&db, run, id, 0, &fetch).await;
        let row = url_row(&db, id).await;
        seen.push((
            row.status,
            row.retry_count,
            row.last_error_type,
            row.attempt_count,
        ));
    }
    assert_eq!(
        seen,
        vec![
            (
                "retry_wait".to_string(),
                1,
                Some("server_error".to_string()),
                1
            ),
            (
                "retry_wait".to_string(),
                2,
                Some("server_error".to_string()),
                2
            ),
            ("succeeded".to_string(), 0, None, 3),
            ("blocked".to_string(), 0, Some("forbidden".to_string()), 4),
        ]
    );
    // 503 と 403 は資源に触れていない: 200 のときの1つだけが `active` のまま
    let all = all_resources(&db).await;
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].state, "active");
}

#[tokio::test]
async fn an_attempt_without_a_response_is_kept_in_history() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let id = register(&db, &url("/a.html")).await;
    let run = start_run(&db).await;
    let fetch = Fetch {
        hops: vec![],
        outcome: Outcome::Network {
            url: url("/a.html"),
            error: NetworkError::Timeout,
            detail: "timed out".to_string(),
        },
    };

    record_fetch(&db, run, id, 0, &fetch).await;

    let rows = fetch_history::Entity::find().all(&db).await.unwrap();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.outcome, "network");
    assert_eq!(row.http_status, None);
    assert_eq!(row.error_detail.as_deref(), Some("timeout: timed out"));
    assert_eq!(row.observation, None);
    let job = url_row(&db, id).await;
    assert_eq!(row.error_type, job.last_error_type);
    assert!(row.error_type.is_some());
    assert_eq!((job.status.as_str(), job.retry_count), ("retry_wait", 1));
    assert!(all_resources(&db).await.is_empty());
}

async fn only_resource(db: &DatabaseConnection) -> resources::Model {
    let all = all_resources(db).await;
    assert_eq!(all.len(), 1);
    all.into_iter().next().unwrap()
}

async fn versions(db: &DatabaseConnection) -> Vec<String> {
    let mut rows: Vec<_> = content_versions::Entity::find()
        .all(db)
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.body_hash)
        .collect();
    rows.sort();
    rows
}

fn with_validators(text: &str) -> axum::response::Response {
    let mut response = body_page(text);
    let headers = response.headers_mut();
    headers.insert("etag", "\"v1\"".parse().unwrap());
    headers.insert(
        "last-modified",
        "Mon, 01 Jan 2026 00:00:00 GMT".parse().unwrap(),
    );
    response
}

#[tokio::test]
async fn an_alive_page_saves_its_hashes_validators_and_a_version() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let fetcher = serve(|_| with_validators("本文")).await;
    let id = register(&db, &url("/a.html")).await;
    let fetch = get(&fetcher, "/a.html").await;
    let run = start_run(&db).await;

    let Recorded::Saved {
        previous_body_hash, ..
    } = record_fetch(&db, run, id, 0, &fetch).await
    else {
        panic!("保存される");
    };
    assert_eq!(previous_body_hash, None);

    let extracted = extracted_of(&fetch).unwrap();
    let resource = only_resource(&db).await;
    assert_eq!(
        resource.body_hash.as_deref(),
        Some(extracted.hashes.body.as_str())
    );
    assert_eq!(
        resource.page_hash.as_deref(),
        Some(extracted.hashes.page.as_str())
    );
    assert_eq!(
        resource.links_hash.as_deref(),
        Some(extracted.hashes.links.as_str())
    );
    assert!(resource.raw_hash.is_some());
    assert_eq!(resource.etag.as_deref(), Some("\"v1\""));
    assert_eq!(
        resource.last_modified.as_deref(),
        Some("Mon, 01 Jan 2026 00:00:00 GMT")
    );
    assert_eq!(resource.extractor_version, Some(extract::EXTRACTOR_VERSION));
    assert_eq!(
        resource.extractor_rule.as_deref(),
        Some(extracted.rule.as_str())
    );
    assert!(resource.last_crawled_at.is_some());
    // 変化の判定は別の層。ここでは動かさない
    assert_eq!((resource.last_changed_at, resource.change_count), (None, 0));
    assert_eq!(resource.next_crawl_at, None);
    assert_eq!(versions(&db).await, vec![extracted.hashes.body]);

    let history = fetch_history::Entity::find()
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(history.body_hash.as_deref(), resource.body_hash.as_deref());
    assert_eq!(history.etag.as_deref(), Some("\"v1\""));
}

#[tokio::test]
async fn a_changed_body_adds_a_version_and_hands_back_the_previous_hash() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let second = Arc::new(AtomicBool::new(false));
    let fetcher = {
        let second = second.clone();
        serve(move |_| {
            body_page(if second.load(Ordering::SeqCst) {
                "改訂"
            } else {
                "初版"
            })
        })
        .await
    };
    let id = register(&db, &url("/a.html")).await;

    let first = get(&fetcher, "/a.html").await;
    record_fetch(&db, start_run(&db).await, id, 0, &first).await;
    let first_hash = only_resource(&db).await.body_hash.unwrap();

    second.store(true, Ordering::SeqCst);
    let changed = get(&fetcher, "/a.html").await;
    let Recorded::Saved {
        previous_body_hash, ..
    } = record_fetch(&db, start_run(&db).await, id, 0, &changed).await
    else {
        panic!("保存される");
    };
    assert_eq!(previous_body_hash.as_deref(), Some(first_hash.as_str()));
    let second_hash = only_resource(&db).await.body_hash.unwrap();
    assert_ne!(first_hash, second_hash);
    assert_eq!(versions(&db).await.len(), 2);

    // 元に戻っても、版は増えない。最新は資源が持つ
    record_fetch(&db, start_run(&db).await, id, 0, &first).await;
    assert_eq!(versions(&db).await.len(), 2);
    assert_eq!(only_resource(&db).await.body_hash.unwrap(), first_hash);
}

#[tokio::test]
async fn a_soft_404_does_not_overwrite_the_last_alive_content() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let gone = Arc::new(AtomicBool::new(false));
    let fetcher = {
        let gone = gone.clone();
        serve(move |_| {
            if gone.load(Ordering::SeqCst) {
                titled("ページが見つかりません")
            } else {
                with_validators("本文")
            }
        })
        .await
    };
    let id = register(&db, &url("/a.html")).await;
    let alive = get(&fetcher, "/a.html").await;
    record_fetch(&db, start_run(&db).await, id, 0, &alive).await;
    let before = only_resource(&db).await;

    gone.store(true, Ordering::SeqCst);
    let soft = get(&fetcher, "/a.html").await;
    record_fetch(&db, start_run(&db).await, id, 0, &soft).await;

    let after = only_resource(&db).await;
    assert_eq!(after.state, "deletion_candidate");
    assert_eq!(after.body_hash, before.body_hash);
    assert_eq!(after.raw_hash, before.raw_hash);
    assert_eq!(after.etag, before.etag);
    assert_eq!(versions(&db).await.len(), 1);
    // 履歴には、そのとき見たものがそのまま残る
    let history = fetch_history::Entity::find().all(&db).await.unwrap();
    assert_eq!(history.len(), 2);
    assert!(history.iter().any(|row| row.raw_hash != before.raw_hash));
}

#[tokio::test]
async fn a_304_keeps_the_content_and_only_moves_last_crawled_at() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let fetcher = serve(|_| with_validators("本文")).await;
    let id = register(&db, &url("/a.html")).await;
    let alive = get(&fetcher, "/a.html").await;
    record_fetch(&db, start_run(&db).await, id, 0, &alive).await;
    let before = only_resource(&db).await;

    let Recorded::Saved {
        previous_body_hash, ..
    } = record_fetch(&db, start_run(&db).await, id, 0, &not_modified("/a.html")).await
    else {
        panic!("保存される");
    };
    assert_eq!(previous_body_hash, None);

    let after = only_resource(&db).await;
    assert_eq!(after.body_hash, before.body_hash);
    assert_eq!(after.raw_hash, before.raw_hash);
    assert_eq!(after.etag, before.etag);
    assert_eq!(after.last_modified, before.last_modified);
    assert!(after.last_crawled_at > before.last_crawled_at);
}

#[tokio::test]
async fn a_pdf_saves_only_its_raw_hash() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let id = register(&db, &url("/a.pdf")).await;
    let fetch = Fetch {
        hops: vec![Hop {
            url: url("/a.pdf"),
            status: 200,
            elapsed: Duration::ZERO,
        }],
        outcome: Outcome::Response(Response {
            url: url("/a.pdf"),
            status: 200,
            etag: None,
            last_modified: None,
            content_type: Some("application/pdf".to_string()),
            x_robots_tag: None,
            body: Body::Pdf(b"%PDF-1.7".to_vec()),
            bytes: 8,
            raw_hash: Some(extract::digest(b"%PDF-1.7")),
        }),
    };

    record_fetch(&db, start_run(&db).await, id, 0, &fetch).await;

    let resource = only_resource(&db).await;
    assert!(resource.raw_hash.is_some());
    assert_eq!(resource.body_hash, None);
    assert_eq!(resource.extractor_version, None);
    assert!(versions(&db).await.is_empty());
}

#[tokio::test]
async fn a_retry_in_the_same_run_replaces_the_earlier_attempt() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let status = Arc::new(AtomicU16::new(503));
    let fetcher = {
        let status = status.clone();
        serve(move |_| match status.load(Ordering::SeqCst) {
            200 => titled("制度"),
            other => empty(other),
        })
        .await
    };
    let id = register(&db, &url("/a.html")).await;
    let run = start_run(&db).await;

    let failed = get(&fetcher, "/a.html").await;
    record_fetch(&db, run, id, 0, &failed).await;
    assert_eq!(url_row(&db, id).await.status, "retry_wait");

    // 同じ実行の中でスケジューラが claim し直した（processing）
    urls::Entity::update_many()
        .col_expr(
            urls::Column::Status,
            sea_orm::sea_query::Expr::value("processing"),
        )
        .filter(urls::Column::Id.eq(id))
        .exec(&db)
        .await
        .unwrap();
    status.store(200, Ordering::SeqCst);
    let ok = get(&fetcher, "/a.html").await;
    assert!(matches!(
        record_fetch(&db, run, id, 0, &ok).await,
        Recorded::Saved { .. }
    ));

    let row = url_row(&db, id).await;
    assert_eq!(
        (row.status.as_str(), row.retry_count, row.attempt_count),
        ("succeeded", 0, 2)
    );
    let history = fetch_history::Entity::find().all(&db).await.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].http_status, Some(200));

    // 完了した後の流し直しは、これまでどおり何も書かない
    assert!(matches!(
        record_fetch(&db, run, id, 0, &ok).await,
        Recorded::AlreadyRecorded
    ));
}

/// `next_crawl_at - now()` を秒で返す
async fn seconds_until_next(db: &DatabaseConnection, id: Uuid) -> i64 {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            db.get_database_backend(),
            "SELECT extract(epoch from next_crawl_at - now())::float8 AS secs FROM urls WHERE id = $1",
            [id.into()],
        ))
        .await
        .unwrap()
        .unwrap();
    row.try_get::<f64>("", "secs").unwrap().round() as i64
}

const DAY: i64 = 24 * 60 * 60;

fn pdf_fetch(status: u16) -> Fetch {
    Fetch {
        hops: vec![Hop {
            url: url("/a.pdf"),
            status,
            elapsed: Duration::ZERO,
        }],
        outcome: Outcome::Response(Response {
            url: url("/a.pdf"),
            status,
            etag: None,
            last_modified: None,
            content_type: Some("application/pdf".to_string()),
            x_robots_tag: None,
            body: Body::Pdf(b"%PDF-1.7".to_vec()),
            bytes: 8,
            raw_hash: Some(extract::digest(b"%PDF-1.7")),
        }),
    }
}

#[tokio::test]
async fn a_success_schedules_the_next_crawl_a_week_later() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let fetcher = serve(|_| titled("制度")).await;
    let id = register(&db, &url("/a.html")).await;

    let fetch = get(&fetcher, "/a.html").await;
    record_fetch(&db, start_run(&db).await, id, 0, &fetch).await;

    assert_eq!(url_row(&db, id).await.status, "succeeded");
    assert!((seconds_until_next(&db, id).await - 7 * DAY).abs() <= 60);
}

#[tokio::test]
async fn the_third_failure_becomes_failed_final() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let fetcher = serve(|_| empty(503)).await;
    let id = register(&db, &url("/a.html")).await;

    let mut seen = Vec::new();
    for _ in 0..3 {
        let fetch = get(&fetcher, "/a.html").await;
        record_fetch(&db, start_run(&db).await, id, 0, &fetch).await;
        let row = url_row(&db, id).await;
        seen.push((
            row.status,
            row.retry_count,
            seconds_until_next(&db, id).await,
        ));
    }

    assert_eq!(seen[0].0, "retry_wait");
    assert_eq!(seen[0].1, 1);
    assert!((9..=15).contains(&seen[0].2), "{}", seen[0].2);
    assert_eq!(seen[1].0, "retry_wait");
    assert_eq!(seen[1].1, 2);
    assert!((19..=25).contains(&seen[1].2), "{}", seen[1].2);
    assert_eq!(seen[2].0, "failed_final");
    assert_eq!(seen[2].1, 3);
    assert!((seen[2].2 - 7 * DAY).abs() <= 60);
}

#[tokio::test]
async fn a_pdf_waits_thirty_days_and_blocked_waits_a_week() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let pdf = register(&db, &url("/a.pdf")).await;
    record_fetch(&db, start_run(&db).await, pdf, 0, &pdf_fetch(200)).await;
    assert!((seconds_until_next(&db, pdf).await - 30 * DAY).abs() <= 60);

    let fetcher = serve(|_| empty(403)).await;
    let blocked = register(&db, &url("/b.html")).await;
    let fetch = get(&fetcher, "/b.html").await;
    record_fetch(&db, start_run(&db).await, blocked, 0, &fetch).await;
    assert_eq!(url_row(&db, blocked).await.status, "blocked");
    assert!((seconds_until_next(&db, blocked).await - 7 * DAY).abs() <= 60);
}

#[tokio::test]
async fn a_304_for_a_pdf_waits_thirty_days() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let pdf = register(&db, &url("/a.pdf")).await;
    record_fetch(&db, start_run(&db).await, pdf, 0, &pdf_fetch(200)).await;
    // 本文を読まない 304 でも、保存した資源が PDF なら PDF の間隔
    record_fetch(&db, start_run(&db).await, pdf, 0, &not_modified("/a.pdf")).await;
    assert_eq!(url_row(&db, pdf).await.status, "succeeded");
    assert!((seconds_until_next(&db, pdf).await - 30 * DAY).abs() <= 60);

    // HTML の 304 は HTML の間隔のまま
    let fetcher = serve(|_| titled("制度")).await;
    let page = register(&db, &url("/b.html")).await;
    let fetch = get(&fetcher, "/b.html").await;
    record_fetch(&db, start_run(&db).await, page, 0, &fetch).await;
    record_fetch(&db, start_run(&db).await, page, 0, &not_modified("/b.html")).await;
    assert!((seconds_until_next(&db, page).await - 7 * DAY).abs() <= 60);
}

/// `path` を取得して記録し、その URL の履歴の1行を返す
async fn record_path(
    db: &DatabaseConnection,
    fetcher: &Fetcher,
    path: &str,
) -> fetch_history::Model {
    let id = register(db, &url(path)).await;
    let run = start_run(db).await;
    let fetch = fetcher.fetch(&url(path), &Default::default()).await;
    record_fetch(db, run, id, 0, &fetch).await;
    fetch_history::Entity::find()
        .filter(fetch_history::Column::UrlId.eq(id))
        .one(db)
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn history_keeps_the_content_type_charset_and_observation() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    // 「子育て」を Shift_JIS で
    let fetcher = serve(|_| {
        reply(200)
            .header(
                axum::http::header::CONTENT_TYPE,
                "text/html; charset=Shift_JIS",
            )
            .body(
                b"<html><body><main>\x8e\x71\x88\xe7\x82\xc4</main></body></html>"
                    .to_vec()
                    .into(),
            )
            .unwrap()
    })
    .await;

    let row = record_path(&db, &fetcher, "/a.html").await;

    assert_eq!(
        row.content_type.as_deref(),
        Some("text/html; charset=Shift_JIS")
    );
    assert_eq!(row.charset.as_deref(), Some("Shift_JIS"));
    assert_eq!(row.charset_source.as_deref(), Some("header"));
    assert_eq!(row.charset_replaced, Some(false));
    assert_eq!(row.observation.as_deref(), Some("alive"));
}

#[tokio::test]
async fn a_pdf_served_as_octet_stream_keeps_its_header_and_is_read_as_pdf() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let fetcher = serve(|_| {
        reply(200)
            .header(axum::http::header::CONTENT_TYPE, "application/octet-stream")
            .body("%PDF-1.7\n".into())
            .unwrap()
    })
    .await;

    let row = record_path(&db, &fetcher, "/a.pdf").await;

    assert_eq!(
        row.content_type.as_deref(),
        Some("application/octet-stream")
    );
    assert_eq!(row.charset, None);
    assert_eq!(row.charset_source, None);
    assert_eq!(row.charset_replaced, None);
    // 先頭が %PDF- なので PDF として読む（ヘッダの値は残る）
    assert!(row.raw_hash.is_some());
    assert_eq!(row.body_hash, None);
    assert_eq!(row.observation.as_deref(), Some("alive"));
}

#[tokio::test]
async fn soft_404s_and_top_redirects_are_told_apart_from_live_pages() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let fetcher = serve(|path| match path {
        "/gone.html" => titled("ページが見つかりません"),
        "/old.html" => redirect(301, "/"),
        "/" => page(None),
        "/missing.html" => empty(404),
        "/removed.html" => empty(410),
        _ => empty(403),
    })
    .await;

    let soft = record_path(&db, &fetcher, "/gone.html").await;
    assert_eq!(soft.observation.as_deref(), Some("soft_404_title"));
    assert_eq!(soft.error_type, None);
    let top = record_path(&db, &fetcher, "/old.html").await;
    assert_eq!(top.observation.as_deref(), Some("top_redirect"));
    let missing = record_path(&db, &fetcher, "/missing.html").await;
    assert_eq!(missing.observation.as_deref(), Some("not_found"));
    let removed = record_path(&db, &fetcher, "/removed.html").await;
    assert_eq!(removed.observation.as_deref(), Some("gone"));
    let forbidden = record_path(&db, &fetcher, "/forbidden.html").await;
    assert_eq!(forbidden.observation, None);
    assert_eq!(forbidden.error_type.as_deref(), Some("forbidden"));
}
