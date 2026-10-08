//! 取得に渡すものを DB から組み立てる部分を、実際の Postgres に流す。
//!
//! `TEST_DATABASE_URL` のデータベースは毎回作り直す（本番の接続先を入れないこと）。未設定ならスキップする。

mod common;

use std::collections::BTreeSet;
use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use axum::body::Bytes;
use common::*;
use domain::liveness::HostMove;
use domain::run_report::{Alert, Counters};
use domain::schedule::Policy;
use entity::crawl_runs;
use entity::{fetch_history, resources, urls as urls_table};
use futures_util::stream;
use pipeline::claim::{self, Exclude};
use pipeline::crawl::{
    Config, Summary, allowed_hosts, host_trusted, run, run_recorded, validators,
};
use pipeline::fetch::Fetcher;
use pipeline::host_moves;
use pipeline::lifecycle::Context;
use pipeline::persist::{Attempt, record};
use pipeline::run_report::Report;
use sea_orm::{
    ActiveValue::Set, ColumnTrait, ConnectionTrait, DatabaseConnection, DbBackend, EntityTrait,
    QueryFilter, Statement, prelude::Uuid,
};
use tokio::time::timeout;

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
    let found = validators(&db, id, Some(resource_id), Duration::from_secs(28 * 86_400))
        .await
        .unwrap();
    assert_eq!(found.etag.as_deref(), Some("\"v1\""));
    assert_eq!(
        found.last_modified.as_deref(),
        Some("Mon, 01 Jan 2026 00:00:00 GMT")
    );
    let none = validators(&db, id, None, Duration::from_secs(28 * 86_400))
        .await
        .unwrap();
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

// ---- 実行のループ ----

/// 実行のテストに共通の設定。待ちを短くし、jitter を消す
fn run_config() -> Config {
    Config {
        concurrency: 4,
        lease: Duration::from_secs(1),
        heartbeat: Duration::from_millis(200),
        policy: Policy {
            retry_base: Duration::from_millis(50),
            retry_cap: Duration::from_millis(200),
            jitter_max: Duration::ZERO,
            ..Policy::default()
        },
        ..Config::default()
    }
}

/// 実行を回す。ループが終わらない不具合はハングではなく失敗にする
async fn run_once(db: &DatabaseConnection, fetcher: &Arc<Fetcher>) -> Summary {
    let run_id = start_run(db).await;
    timeout(
        Duration::from_secs(10),
        run(db, fetcher.clone(), run_id, &run_config()),
    )
    .await
    .expect("実行が終わる")
    .expect("実行が失敗しない")
}

async fn url_row(db: &DatabaseConnection, id: Uuid) -> urls_table::Model {
    urls_table::Entity::find_by_id(id)
        .one(db)
        .await
        .unwrap()
        .unwrap()
}

/// next_crawl_at までの残り（日。過去なら負）
async fn days_until_next(db: &DatabaseConnection, id: Uuid) -> f64 {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "select (extract(epoch from next_crawl_at - now()) / 86400)::float8 as days \
             from urls where id = $1",
            [id.into()],
        ))
        .await
        .unwrap()
        .unwrap();
    row.try_get("", "days").unwrap()
}

async fn history_count(db: &DatabaseConnection, id: Uuid) -> usize {
    fetch_history::Entity::find()
        .filter(fetch_history::Column::UrlId.eq(id))
        .all(db)
        .await
        .unwrap()
        .len()
}

#[tokio::test]
async fn urls_of_a_host_are_fetched_in_priority_order() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let seen = Arc::new(StdMutex::new(Vec::new()));
    let fetcher = Arc::new(
        serve({
            let seen = seen.clone();
            move |path| {
                seen.lock().unwrap().push(path.to_string());
                body_page(path)
            }
        })
        .await,
    );
    let mut ids = Vec::new();
    for priority in [30, 90, 80] {
        let id = register(&db, &url(&format!("/p{priority}.html"))).await;
        set_priority(&db, id, priority).await;
        ids.push(id);
    }

    let summary = run_once(&db, &fetcher).await;

    assert_eq!(
        *seen.lock().unwrap(),
        vec!["/p90.html", "/p80.html", "/p30.html"]
    );
    assert_eq!(summary.saved, 3);
    for id in ids {
        assert_eq!(url_row(&db, id).await.status, "succeeded");
        let days = days_until_next(&db, id).await;
        assert!((6.99..=7.01).contains(&days), "7日後: {days}");
    }
}

#[tokio::test]
async fn an_expired_lease_left_by_a_dead_worker_is_picked_up_by_the_next_run() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let fetcher = Arc::new(serve(|_| page(None)).await);
    let id = register(&db, &url("/a.html")).await;
    claim::claim(&db, &[id], "dead", Duration::from_secs(600), Duration::ZERO)
        .await
        .unwrap();
    db.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "update urls set lease_until = now() - interval '1 second' where id = $1",
        [id.into()],
    ))
    .await
    .unwrap();

    run_once(&db, &fetcher).await;

    let row = url_row(&db, id).await;
    assert_eq!(row.status, "succeeded");
    assert_eq!(row.claim_token, 2);
    assert_eq!(history_count(&db, id).await, 1);
}

#[tokio::test]
async fn a_retry_in_the_same_run_succeeds_after_backoff() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let fetcher = Arc::new(
        serve({
            let calls = calls.clone();
            move |_| match calls.fetch_add(1, Ordering::SeqCst) {
                0 => reply(503).body(Default::default()).unwrap(),
                _ => page(None),
            }
        })
        .await,
    );
    let id = register(&db, &url("/flaky.html")).await;

    run_once(&db, &fetcher).await;

    let row = url_row(&db, id).await;
    assert_eq!(row.status, "succeeded");
    assert_eq!(row.retry_count, 0);
    assert_eq!(row.attempt_count, 2);
    // 同じ実行の中の再試行は、最新の試行の1行に置き換わる
    assert_eq!(history_count(&db, id).await, 1);
}

#[tokio::test]
async fn a_page_that_keeps_failing_ends_as_failed_final_and_the_run_ends() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let fetcher = Arc::new(serve(|_| reply(503).body(Default::default()).unwrap()).await);
    let id = register(&db, &url("/down.html")).await;

    run_once(&db, &fetcher).await;

    let row = url_row(&db, id).await;
    assert_eq!(row.status, "failed_final");
    assert_eq!(row.attempt_count, 3);
}

#[tokio::test]
async fn a_host_whose_robots_txt_cannot_be_read_is_left_for_the_next_run() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let pages = Arc::new(AtomicUsize::new(0));
    let fetcher = Arc::new(
        serve_with_robots(|| reply(503).body(Default::default()).unwrap(), {
            let pages = pages.clone();
            move |_| {
                pages.fetch_add(1, Ordering::SeqCst);
                page(None)
            }
        })
        .await,
    );
    let mut ids = Vec::new();
    for n in 1..=3 {
        ids.push(register(&db, &url(&format!("/p{n}.html"))).await);
    }

    let summary = run_once(&db, &fetcher).await;

    assert_eq!(pages.load(Ordering::SeqCst), 0);
    let mut rows = Vec::new();
    for id in ids {
        rows.push(url_row(&db, id).await);
    }
    let waiting: Vec<_> = rows.iter().filter(|r| r.status == "retry_wait").collect();
    assert_eq!(waiting.len(), 1);
    assert_eq!(waiting[0].attempt_count, 1);
    assert_eq!(rows.iter().filter(|r| r.status == "queued").count(), 2);
    assert_eq!(
        summary.skipped_hosts,
        BTreeSet::from([format!("http://{CITY}")])
    );
}

#[tokio::test]
async fn a_resource_shared_by_two_urls_is_fetched_once_per_run() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let alive = Arc::new(AtomicBool::new(true));
    let fetcher = Arc::new(
        serve({
            let alive = alive.clone();
            move |path| match path {
                "/c.html" if alive.load(Ordering::SeqCst) => page(None),
                "/c.html" => reply(404).body(Default::default()).unwrap(),
                _ => redirect(301, &url("/c.html")),
            }
        })
        .await,
    );
    let a = register(&db, &url("/a.html")).await;
    let b = register(&db, &url("/b.html")).await;

    // 1回目: まだどの資源にも結ばれていないので両方取る
    run_once(&db, &fetcher).await;
    let resource = links_of(&db, a).await[0].resource_id;
    assert_eq!(links_of(&db, b).await[0].resource_id, resource);

    alive.store(false, Ordering::SeqCst);
    for id in [a, b] {
        set_status(&db, id, "succeeded", "now() - interval '1 second'").await;
    }

    // 2回目: 片方だけ
    run_once(&db, &fetcher).await;
    let not_found = |db: DatabaseConnection| async move {
        resources::Entity::find_by_id(resource)
            .one(&db)
            .await
            .unwrap()
            .unwrap()
            .consecutive_not_found
    };
    assert_eq!(not_found(db.clone()).await, 1);
    let (taken, left) = if days_until_next(&db, a).await > 0.0 {
        (a, b)
    } else {
        (b, a)
    };
    assert!(days_until_next(&db, taken).await > 6.0);
    assert_eq!(url_row(&db, left).await.status, "succeeded");
    assert!(days_until_next(&db, left).await < 0.0);

    // 3回目: 残った方
    run_once(&db, &fetcher).await;
    assert_eq!(not_found(db.clone()).await, 2);
    assert!(days_until_next(&db, left).await > 6.0);
}

#[tokio::test]
async fn heartbeat_keeps_a_slow_fetch_from_being_reclaimed() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let fetcher = Arc::new(
        serve(|_| {
            // 見出しはすぐ返し、本文を 1.5 秒遅らせる
            let body = stream::once(async {
                tokio::time::sleep(Duration::from_millis(1500)).await;
                Ok::<_, Infallible>(Bytes::from(
                    "<html><head></head><body><main>本文</main></body></html>",
                ))
            });
            reply(200)
                .header("content-type", "text/html; charset=utf-8")
                .body(axum::body::Body::from_stream(body))
                .unwrap()
        })
        .await,
    );
    let id = register(&db, &url("/slow.html")).await;
    let run_id = start_run(&db).await;

    let handle = tokio::spawn({
        let db = db.clone();
        let fetcher = fetcher.clone();
        async move { run(&db, fetcher, run_id, &run_config()).await }
    });
    tokio::time::sleep(Duration::from_millis(1200)).await;
    // lease（1秒）は切れているはずの時刻だが、heartbeat が延ばしている
    assert!(
        claim::candidates(&db, &Exclude::default(), Duration::ZERO)
            .await
            .unwrap()
            .is_empty()
    );

    let summary = timeout(Duration::from_secs(10), handle)
        .await
        .expect("実行が終わる")
        .unwrap()
        .unwrap();
    assert_eq!(summary.stale, 0);
    assert_eq!(url_row(&db, id).await.status, "succeeded");
}

#[tokio::test]
async fn a_linked_url_is_retried_in_the_same_run() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let fetcher = Arc::new(
        serve({
            let calls = calls.clone();
            // 1回目の実行は 200、2回目の実行は 503 を1回返してから 200
            move |_| match calls.fetch_add(1, Ordering::SeqCst) {
                1 => reply(503).body(Default::default()).unwrap(),
                _ => page(None),
            }
        })
        .await,
    );
    let id = register(&db, &url("/a.html")).await;
    run_once(&db, &fetcher).await;
    assert!(
        !links_of(&db, id).await.is_empty(),
        "前の実行で資源に結ばれている"
    );
    let before = url_row(&db, id).await.attempt_count;
    set_status(&db, id, "succeeded", "now() - interval '1 second'").await;

    // 資源を取ったのはこの URL 自身なので、同じ実行の中で再試行される
    run_once(&db, &fetcher).await;

    let row = url_row(&db, id).await;
    assert_eq!(row.status, "succeeded");
    assert_eq!(row.attempt_count, before + 2);
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn robots_txt_unreadable_at_the_redirect_target_skips_both_hosts() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let fetcher = Arc::new(
        serve_hosts(
            &[CITY, TOWN],
            |host| match host {
                TOWN => reply(503).body(Default::default()).unwrap(),
                _ => reply(404).body(Default::default()).unwrap(),
            },
            |_| redirect(301, &format!("http://{TOWN}/a.html")),
        )
        .await,
    );
    let id = register(&db, &url("/a.html")).await;

    let summary = run_once(&db, &fetcher).await;

    let row = url_row(&db, id).await;
    assert_eq!(row.status, "retry_wait");
    assert_eq!(row.attempt_count, 1);
    assert_eq!(
        summary.skipped_hosts,
        BTreeSet::from([format!("http://{CITY}"), format!("http://{TOWN}")])
    );
}

#[tokio::test]
async fn urls_of_two_hosts_linked_to_one_resource_are_not_claimed_together() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let alive = Arc::new(AtomicBool::new(true));
    let entries = Arc::new(AtomicUsize::new(0));
    let fetcher = Arc::new(
        serve_hosts(
            &[CITY, TOWN],
            |_| reply(404).body(Default::default()).unwrap(),
            {
                let alive = alive.clone();
                let entries = entries.clone();
                // どちらのホストの /a.html も CITY の /c.html へ転送する
                move |path| match path {
                    "/c.html" if alive.load(Ordering::SeqCst) => page(None),
                    "/c.html" => reply(404).body(Default::default()).unwrap(),
                    _ => {
                        entries.fetch_add(1, Ordering::SeqCst);
                        redirect(301, &url("/c.html"))
                    }
                }
            },
        )
        .await,
    );
    let city = register(&db, &url("/a.html")).await;
    let town = register(&db, &format!("http://{TOWN}/a.html")).await;

    // 1回目: まだ結ばれていないので両方取り、同じ資源に結ぶ
    run_once(&db, &fetcher).await;
    let resource = links_of(&db, city).await[0].resource_id;
    assert_eq!(links_of(&db, town).await[0].resource_id, resource);

    alive.store(false, Ordering::SeqCst);
    entries.store(0, Ordering::SeqCst);
    for id in [city, town] {
        set_status(&db, id, "succeeded", "now() - interval '1 second'").await;
    }

    // 2回目: ホストは別でも、同じ資源に結ばれた2件を一緒に claim しない
    run_once(&db, &fetcher).await;
    assert_eq!(entries.load(Ordering::SeqCst), 1);
    let not_found = resources::Entity::find_by_id(resource)
        .one(&db)
        .await
        .unwrap()
        .unwrap()
        .consecutive_not_found;
    assert_eq!(not_found, 1);
}

/// 実行を記録して回す。ループが終わらない不具合はハングではなく失敗にする
async fn run_recorded_once(
    db: &DatabaseConnection,
    fetcher: &Arc<Fetcher>,
) -> (Uuid, Summary, Report) {
    timeout(
        Duration::from_secs(10),
        run_recorded(db, fetcher.clone(), "sweep", &run_config()),
    )
    .await
    .expect("実行が終わる")
    .expect("実行が失敗しない")
}

#[tokio::test]
async fn a_recorded_run_is_closed_and_a_second_run_right_after_fetches_nothing() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let fetcher = Arc::new(serve(|_| page(None)).await);
    register(&db, &url("/a.html")).await;
    register(&db, &url("/b.html")).await;

    let (first, summary, _) = run_recorded_once(&db, &fetcher).await;
    assert_eq!(summary.saved, 2);
    let row = crawl_runs::Entity::find_by_id(first)
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.kind, "sweep");
    assert!(row.finished_at.is_some());

    // 成功した行は7日後まで候補に出ない
    let (second, summary, _) = run_recorded_once(&db, &fetcher).await;
    assert_ne!(second, first);
    assert_eq!(summary.saved, 0);
    assert_eq!(
        fetch_history::Entity::find().all(&db).await.unwrap().len(),
        2
    );
    let runs = crawl_runs::Entity::find().all(&db).await.unwrap();
    assert_eq!(runs.len(), 2);
    assert!(runs.iter().all(|run| run.finished_at.is_some()));
}

#[tokio::test]
async fn two_new_urls_on_different_hosts_redirecting_to_one_page_share_a_resource() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let target = format!("http://{TOWN}/c.html");
    let fetcher = Arc::new(
        serve_hosts(
            &[CITY, TOWN],
            |_| reply(404).body(Default::default()).unwrap(),
            {
                let target = target.clone();
                // CITY の /a.html と TOWN の /b.html が、どちらも TOWN の /c.html へ転送する
                move |path| match path {
                    "/c.html" => page(None),
                    _ => redirect(301, &target),
                }
            },
        )
        .await,
    );
    let a = register(&db, &url("/a.html")).await;
    let b = register(&db, &format!("http://{TOWN}/b.html")).await;

    let (_, summary, _) = run_recorded_once(&db, &fetcher).await;

    assert_eq!(summary.saved, 2);
    let all = all_resources(&db).await;
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].canonical_url, target);
    assert_eq!(links_of(&db, a).await[0].resource_id, all[0].id);
    assert_eq!(links_of(&db, b).await[0].resource_id, all[0].id);
    assert_eq!(url_row(&db, a).await.status, "succeeded");
    assert_eq!(url_row(&db, b).await.status, "succeeded");
}

#[tokio::test]
async fn a_recorded_run_keeps_its_stats_config_and_a_summary() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let fetcher = Arc::new(serve(|_| page(None)).await);
    register(&db, &url("/a.html")).await;
    register(&db, &url("/b.html")).await;

    let (first, _, report) = run_recorded_once(&db, &fetcher).await;
    assert_eq!(
        report.stats.totals().statuses,
        std::collections::BTreeMap::from([(200u16, 2u64)])
    );
    assert!(report.alerts.is_empty());
    let row = crawl_runs::Entity::find_by_id(first)
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.config.unwrap()["concurrency"], 4);
    let saved: domain::run_report::RunStats =
        serde_json::from_value(row.stats.expect("stats を残す")).unwrap();
    assert_eq!(saved, report.stats);

    // 何も取らなかった実行は警告にしない
    let (_, _, second) = run_recorded_once(&db, &fetcher).await;
    assert!(second.stats.hosts.is_empty());
    assert!(second.alerts.is_empty());
}

#[tokio::test]
async fn a_retry_and_a_reclaimed_lease_are_counted_by_the_loop() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let fetcher = Arc::new(
        serve({
            let calls = calls.clone();
            move |path| match path {
                "/flaky.html" if calls.fetch_add(1, Ordering::SeqCst) == 0 => {
                    reply(503).body(Default::default()).unwrap()
                }
                _ => page(None),
            }
        })
        .await,
    );
    register(&db, &url("/flaky.html")).await;
    let left = register(&db, &url("/left.html")).await;
    claim::claim(
        &db,
        &[left],
        "dead",
        Duration::from_secs(600),
        Duration::ZERO,
    )
    .await
    .unwrap();
    db.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "update urls set lease_until = now() - interval '1 second' where id = $1",
        [left.into()],
    ))
    .await
    .unwrap();

    let (_, summary, report) = run_recorded_once(&db, &fetcher).await;

    assert_eq!(summary.retries, 1);
    assert_eq!(summary.lease_expired, 1);
    assert_eq!(
        report.stats.counters,
        Some(Counters {
            retries: 1,
            lease_expired: 1
        })
    );
    // 再試行の 503 は 200 に置き換わって残らない
    assert_eq!(
        report.stats.hosts[&format!("http://{CITY}")].statuses,
        std::collections::BTreeMap::from([(200u16, 2u64)])
    );
}

#[tokio::test]
async fn a_run_where_robots_txt_cannot_be_read_anywhere_is_recorded_with_an_alert() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let fetcher = Arc::new(
        serve_with_robots(
            || reply(503).body(Default::default()).unwrap(),
            |_| page(None),
        )
        .await,
    );
    register(&db, &url("/a.html")).await;

    let (run_id, _, report) = run_recorded_once(&db, &fetcher).await;

    assert_eq!(report.alerts, vec![Alert::AllFailed { fetched: 1 }]);
    let row = crawl_runs::Entity::find_by_id(run_id)
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert!(row.finished_at.is_some());
    assert!(
        row.alerts
            .is_some_and(|alerts| alerts != serde_json::json!([]))
    );
}

#[tokio::test]
async fn a_run_with_a_window_fetches_a_url_due_soon_exactly_once() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let fetcher = Arc::new(serve(|_| page(None)).await);
    let id = register(&db, &url("/a.html")).await;
    run_once(&db, &fetcher).await;
    set_status(&db, id, "succeeded", "now() + interval '1 day'").await;

    run_once(&db, &fetcher).await;
    assert_eq!(history_count(&db, id).await, 1);

    let config = Config {
        due_within: Duration::from_secs(2 * 86_400),
        ..run_config()
    };
    let summary = timeout(
        Duration::from_secs(10),
        run(&db, fetcher.clone(), start_run(&db).await, &config),
    )
    .await
    .expect("実行が終わる")
    .unwrap();
    assert_eq!(summary.saved, 1);
    assert_eq!(history_count(&db, id).await, 2);
}

#[tokio::test]
async fn a_window_as_long_as_the_shortest_interval_is_refused() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let fetcher = Arc::new(serve(|_| page(None)).await);
    let id = register(&db, &url("/a.html")).await;
    let config = run_config();
    let config = Config {
        due_within: config.policy.shortest_interval(),
        ..config
    };

    let error = run(&db, fetcher, start_run(&db).await, &config)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("先取り"), "{error}");
    assert_eq!(history_count(&db, id).await, 0);
}
