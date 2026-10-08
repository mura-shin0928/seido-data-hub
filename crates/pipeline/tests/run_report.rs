//! 実行の集計を、実際の Postgres と取得の履歴から数える部分を流す。
//!
//! `TEST_DATABASE_URL` のデータベースは毎回作り直す（本番の接続先を入れないこと）。未設定ならスキップする。

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::*;
use domain::run_report::{Alert, Counters, Thresholds, render_markdown};
use domain::schedule::Policy;
use entity::crawl_runs;
use pipeline::crawl::{Config, run};
use pipeline::fetch::Fetcher;
use pipeline::run_report;
use sea_orm::{
    ConnectionTrait, DatabaseConnection, DbBackend, EntityTrait, Statement, prelude::Uuid,
};
use tokio::time::timeout;

const CITY_KEY: &str = "http://www.city.example.jp";
const TOWN_KEY: &str = "http://www.town.example.jp";

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

/// 実行を作って回し、run_id を返す。ループが終わらない不具合はハングではなく失敗にする
async fn run_once(db: &DatabaseConnection, fetcher: &Arc<Fetcher>) -> Uuid {
    let run_id = start_run(db).await;
    timeout(
        Duration::from_secs(10),
        run(db, fetcher.clone(), run_id, &run_config()),
    )
    .await
    .expect("実行が終わる")
    .expect("実行が失敗しない");
    run_id
}

async fn exec(db: &DatabaseConnection, sql: &str, values: Vec<sea_orm::Value>) {
    db.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        sql,
        values,
    ))
    .await
    .unwrap();
}

#[tokio::test]
async fn statuses_observations_and_failures_are_counted_per_host() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let fetcher = Arc::new(
        serve_hosts(
            &[CITY, TOWN],
            |_| reply(404).body(Default::default()).unwrap(),
            |path| match path {
                "/b.html" => reply(404).body(Default::default()).unwrap(),
                "/c.html" => reply(503).body(Default::default()).unwrap(),
                _ => page(None),
            },
        )
        .await,
    );
    register(&db, &format!("http://{CITY}/a.html")).await;
    register(&db, &format!("http://{CITY}/b.html")).await;
    register(&db, &format!("http://{CITY}/c.html")).await;
    register(&db, &format!("http://{TOWN}/d.html")).await;

    let run_id = run_once(&db, &fetcher).await;
    let stats = run_report::collect(&db, run_id, None).await.unwrap();

    let city = &stats.hosts[CITY_KEY];
    assert_eq!(
        city.statuses,
        BTreeMap::from([(200, 1), (404, 1), (503, 1)])
    );
    assert_eq!(
        city.observations,
        BTreeMap::from([("alive".to_string(), 1), ("not_found".to_string(), 1)])
    );
    assert_eq!(stats.hosts[TOWN_KEY].statuses, BTreeMap::from([(200, 1)]));
    assert_eq!(stats.failed_final, 1);
    assert_eq!(stats.counters, None);
    assert!(stats.response_ms_p50.is_some());
    assert_eq!(stats.queue_due, 0);
}

#[tokio::test]
async fn a_later_run_counts_304s_and_changed_pages_against_the_last_hashed_fetch() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    // 0: 1回目、1: 2回目、2: 3回目
    let phase = Arc::new(AtomicUsize::new(0));
    let fetcher = Arc::new(
        serve({
            let phase = phase.clone();
            move |path| match (phase.load(Ordering::SeqCst), path) {
                (0, "/a.html") => reply(200)
                    .header("etag", "\"v1\"")
                    .header("content-type", "text/html; charset=utf-8")
                    .body("<html><body><main>甲</main></body></html>".into())
                    .unwrap(),
                (0, "/b.html") => body_page("一"),
                (0, _) => body_page("同じ"),
                (_, "/a.html") => reply(304)
                    .header("etag", "\"v1\"")
                    .body(Default::default())
                    .unwrap(),
                (_, "/b.html") => body_page("二"),
                (_, _) => body_page("同じ"),
            }
        })
        .await,
    );
    let ids = [
        register(&db, &url("/a.html")).await,
        register(&db, &url("/b.html")).await,
        register(&db, &url("/c.html")).await,
    ];

    run_once(&db, &fetcher).await;
    for id in ids {
        set_status(&db, id, "succeeded", "now() - interval '1 second'").await;
    }
    phase.store(1, Ordering::SeqCst);
    let second = run_once(&db, &fetcher).await;

    let city = &run_report::collect(&db, second, None).await.unwrap().hosts[CITY_KEY];
    assert_eq!(city.statuses, BTreeMap::from([(200, 2), (304, 1)]));
    assert_eq!(city.not_modified_ratio(), Some(1.0 / 3.0));
    assert_eq!((city.compared, city.changed), (3, 1));

    set_status(&db, ids[0], "succeeded", "now() - interval '1 second'").await;
    phase.store(2, Ordering::SeqCst);
    let third = run_once(&db, &fetcher).await;

    // 2回目の 304 はハッシュを持たないので、1回目の 200 のハッシュと比べる
    let city = &run_report::collect(&db, third, None).await.unwrap().hosts[CITY_KEY];
    assert_eq!(city.statuses, BTreeMap::from([(304, 1)]));
    assert_eq!((city.compared, city.changed), (1, 0));
}

#[tokio::test]
async fn robots_failures_and_network_errors_are_kept_by_kind() {
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
    let run_id = run_once(&db, &fetcher).await;

    // 本物のタイムアウトは待つのが長いので、同じ形の行を直接入れる
    let town = register(&db, &format!("http://{TOWN}/x.html")).await;
    exec(
        &db,
        "insert into fetch_history (run_id, url_id, outcome, error_type, error_detail) \
         values ($1, $2, 'network', 'timeout', 'timeout: operation timed out')",
        vec![run_id.into(), town.into()],
    )
    .await;

    let stats = run_report::collect(&db, run_id, None).await.unwrap();
    assert_eq!(
        stats.hosts[CITY_KEY].stopped,
        BTreeMap::from([("robots_unavailable".to_string(), 1)])
    );
    assert_eq!(
        stats.hosts[TOWN_KEY].stopped,
        BTreeMap::from([("network:timeout".to_string(), 1)])
    );
}

#[tokio::test]
async fn the_previous_run_is_the_latest_finished_one_before_this() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let urls = [register(&db, &url("/p.html")).await];
    let a = start_run(&db).await;
    let b = start_run(&db).await;
    let c = start_run(&db).await;
    let d = start_run(&db).await;
    // a: 取得あり・終了、b: 未終了、c: 終了だが取得なし（比べる相手にしない）、d: 今回
    for (id, ago, finished) in [(a, 40, true), (b, 30, false), (c, 20, true), (d, 10, false)] {
        exec(
            &db,
            "update crawl_runs set started_at = now() - make_interval(secs => $1), \
             finished_at = case when $2 then now() else null end where id = $3",
            vec![(ago as f64).into(), finished.into(), id.into()],
        )
        .await;
    }
    insert_responses(&db, a, &urls, 0).await;
    insert_responses(&db, b, &urls, 0).await;

    // b は未終了、c は取得が無いので、a が前回
    assert_eq!(run_report::previous_run(&db, d).await.unwrap(), Some(a));
    // 後に始まった実行は、終わって取得があっても前回にならない
    exec(
        &db,
        "update crawl_runs set finished_at = now() where id = $1",
        vec![b.into()],
    )
    .await;
    insert_responses(&db, d, &urls, 0).await;
    exec(
        &db,
        "update crawl_runs set finished_at = now() where id = $1",
        vec![d.into()],
    )
    .await;
    assert_eq!(run_report::previous_run(&db, a).await.unwrap(), None);
    assert_eq!(run_report::previous_run(&db, d).await.unwrap(), Some(b));
}

#[tokio::test]
async fn closing_a_run_keeps_its_stats_and_alerts_and_load_reads_them_back() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let fetcher = Arc::new(serve(|_| page(None)).await);
    register(&db, &url("/a.html")).await;
    let run_id = run_once(&db, &fetcher).await;

    let counters = Counters {
        retries: 1,
        lease_expired: 0,
    };
    let stats = run_report::collect(&db, run_id, Some(counters))
        .await
        .unwrap();
    let alerts = vec![Alert::AllFailed { fetched: 1 }];
    run_report::close(&db, run_id, &stats, &alerts)
        .await
        .unwrap();

    let row = crawl_runs::Entity::find_by_id(run_id)
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert!(row.finished_at.is_some());
    assert_eq!(
        serde_json::from_value::<domain::run_report::RunStats>(row.stats.unwrap()).unwrap(),
        stats
    );
    assert_eq!(
        serde_json::from_value::<Vec<Alert>>(row.alerts.unwrap()).unwrap(),
        alerts
    );

    let report = run_report::load(&db, Some(run_id)).await.unwrap();
    assert_eq!(report.stats, stats);
    assert_eq!(report.stats.counters.as_ref().map(|c| c.retries), Some(1));
    assert_eq!(report.alerts, alerts);
    assert!(report.meta.duration_secs.is_some());
}

#[tokio::test]
async fn a_run_that_stopped_halfway_is_rebuilt_from_history() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let fetcher = Arc::new(serve(|_| page(None)).await);
    register(&db, &url("/a.html")).await;
    let run_id = run_once(&db, &fetcher).await;

    let report = run_report::load(&db, None).await.unwrap();
    assert_eq!(report.meta.run_id, run_id.to_string());
    assert_eq!(report.stats.counters, None);
    assert_eq!(report.meta.duration_secs, None);
    assert_eq!(
        report.stats.hosts[CITY_KEY].statuses,
        BTreeMap::from([(200, 1)])
    );
    let md = render_markdown(&report.meta, &report.stats, &report.alerts);
    assert!(md.contains("終わっていない"), "{md}");
}

/// 実行 `run_id` に、`urls` の各 URL の応答を1行ずつ入れる（`n_503` 件だけ 503、残りは 200）
async fn insert_responses(db: &DatabaseConnection, run_id: Uuid, urls: &[Uuid], n_503: usize) {
    for (i, url_id) in urls.iter().enumerate() {
        let status = if i < n_503 { 503 } else { 200 };
        exec(
            db,
            "insert into fetch_history (run_id, url_id, outcome, http_status) \
             values ($1, $2, 'response', $3)",
            vec![run_id.into(), (*url_id).into(), status.into()],
        )
        .await;
    }
}

#[tokio::test]
async fn alerts_are_judged_against_the_previous_run_recounted_from_history() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let mut urls = Vec::new();
    for n in 0..20 {
        urls.push(register(&db, &url(&format!("/p{n}.html"))).await);
    }

    // 前回: 履歴では 503 が 1/20（5%）。保存した集計は 20/20 だったと言っている
    let previous = start_run(&db).await;
    insert_responses(&db, previous, &urls, 1).await;
    let mut tampered = run_report::collect(&db, previous, None).await.unwrap();
    tampered.hosts.get_mut(CITY_KEY).unwrap().statuses = BTreeMap::from([(503, 20)]);
    run_report::close(&db, previous, &tampered, &[])
        .await
        .unwrap();
    exec(
        &db,
        "update crawl_runs set started_at = now() - interval '1 minute' where id = $1",
        vec![previous.into()],
    )
    .await;

    // 今回: 503 が 5/20（25%）
    let current = start_run(&db).await;
    insert_responses(&db, current, &urls, 5).await;
    let stats = run_report::collect(&db, current, None).await.unwrap();

    // 保存値（100%）と比べるなら抑えられるが、履歴から数え直した 5% の2倍（10%）は超える
    let alerts = run_report::alerts_for(&db, current, &stats, &Thresholds::default())
        .await
        .unwrap();
    assert_eq!(
        alerts,
        vec![Alert::HostErrorsSurged {
            host: CITY_KEY.to_string(),
            count: 5,
            rate: 0.25,
            previous_rate: Some(0.05),
        }]
    );
}

#[tokio::test]
async fn without_a_previous_run_the_comparison_with_it_is_dropped() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let mut urls = Vec::new();
    for n in 0..20 {
        urls.push(register(&db, &url(&format!("/p{n}.html"))).await);
    }
    let current = start_run(&db).await;
    insert_responses(&db, current, &urls, 5).await;
    let stats = run_report::collect(&db, current, None).await.unwrap();

    let alerts = run_report::alerts_for(&db, current, &stats, &Thresholds::default())
        .await
        .unwrap();
    assert_eq!(
        alerts,
        vec![Alert::HostErrorsSurged {
            host: CITY_KEY.to_string(),
            count: 5,
            rate: 0.25,
            previous_rate: None,
        }]
    );
}
