//! 応答の分類と資源の状態を、テスト用サーバーと実際の Postgres に流す。
//!
//! `TEST_DATABASE_URL` のデータベースは毎回作り直す（本番の接続先を入れないこと）。未設定ならスキップする。

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use common::*;
use domain::extract;
use domain::liveness::{Job, Observation, Why};
use entity::resources;
use pipeline::fetch::{Body, Fetch, Fetcher, Hop, Outcome, Response};
use pipeline::host_moves;
use pipeline::lifecycle::{Context, Processed, Resolved, process};
use pipeline::resources::Linked;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, prelude::Uuid};

const CANDIDATE: &str = "deletion_candidate";

/// 取得して、抽出して、決めて、結んで、状態を動かす
async fn run(db: &DatabaseConnection, fetcher: &Fetcher, url_id: Uuid, path: &str) -> Processed {
    let fetch = fetcher.fetch(&url(path), &Default::default()).await;
    process_fetch(db, url_id, &fetch).await
}

async fn process_fetch(db: &DatabaseConnection, url_id: Uuid, fetch: &Fetch) -> Processed {
    let extracted = match &fetch.outcome {
        Outcome::Response(response) => match &response.body {
            Body::Html(html) => Some(extract::extract(&html.text, &response.url)),
            _ => None,
        },
        _ => None,
    };
    let ctx = Context {
        host_trusted: true,
        known_not_found_titles: &[],
    };
    process(db, url_id, fetch, extracted.as_ref(), &ctx)
        .await
        .unwrap()
}

async fn resource(db: &DatabaseConnection, canonical: &str) -> resources::Model {
    resources::Entity::find()
        .filter(resources::Column::CanonicalUrl.eq(canonical))
        .one(db)
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("{canonical} の資源が無い"))
}

fn empty(status: u16) -> axum::response::Response {
    reply(status).body(Default::default()).unwrap()
}

/// 304 の応答（本文を読んでいない）
fn not_modified(path: &str) -> Fetch {
    Fetch {
        hops: vec![Hop {
            url: url(path),
            status: 304,
            elapsed: Duration::ZERO,
        }],
        outcome: Outcome::Response(Response {
            url: url(path),
            status: 304,
            etag: None,
            last_modified: None,
            content_type: None,
            x_robots_tag: None,
            body: Body::NotRead,
            bytes: 0,
            raw_hash: None,
        }),
    }
}

#[tokio::test]
async fn repeated_404s_go_from_candidate_to_deleted() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let fetcher = serve(|_| empty(404)).await;
    let id = register(&db, &url("/gone.html")).await;

    let mut seen = Vec::new();
    for _ in 0..3 {
        let done = run(&db, &fetcher, id, "/gone.html").await;
        assert_eq!(done.verdict.job, Job::Succeeded);
        let resource = resource(&db, &url("/gone.html")).await;
        seen.push((resource.state, resource.consecutive_not_found));
    }
    assert_eq!(
        seen,
        vec![
            (CANDIDATE.to_string(), 1),
            (CANDIDATE.to_string(), 2),
            ("deleted".to_string(), 3),
        ]
    );
}

#[tokio::test]
async fn a_410_deletes_at_once() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let fetcher = serve(|_| empty(410)).await;
    let id = register(&db, &url("/gone.html")).await;
    run(&db, &fetcher, id, "/gone.html").await;
    assert_eq!(resource(&db, &url("/gone.html")).await.state, "deleted");
}

#[tokio::test]
async fn a_page_that_comes_back_is_active_again() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let up = Arc::new(AtomicBool::new(false));
    let fetcher = {
        let up = up.clone();
        serve(move |_| {
            if up.load(Ordering::SeqCst) {
                page(None)
            } else {
                empty(404)
            }
        })
        .await
    };
    let id = register(&db, &url("/a.html")).await;
    run(&db, &fetcher, id, "/a.html").await;
    assert_eq!(resource(&db, &url("/a.html")).await.state, CANDIDATE);

    up.store(true, Ordering::SeqCst);
    let done = run(&db, &fetcher, id, "/a.html").await;
    let transition = done.transition.expect("観測が当たる");
    assert!(transition.restored);
    let resource = resource(&db, &url("/a.html")).await;
    assert_eq!(
        (resource.state.as_str(), resource.consecutive_not_found),
        ("active", 0)
    );
    // 状態が動いただけで、内容の変化ではない
    assert!(resource.last_changed_at.is_none());
}

#[tokio::test]
async fn a_304_also_brings_a_candidate_back() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let fetcher = serve(|_| empty(404)).await;
    let id = register(&db, &url("/a.html")).await;
    run(&db, &fetcher, id, "/a.html").await;
    assert_eq!(resource(&db, &url("/a.html")).await.state, CANDIDATE);

    // 304 は代表 URL を決め直さない（前回の結び付きを使う）
    let done = process_fetch(&db, id, &not_modified("/a.html")).await;
    assert!(done.linked.is_none());
    assert_eq!(done.transition.map(|t| t.restored), Some(true));
    assert_eq!(resource(&db, &url("/a.html")).await.state, "active");
}

#[tokio::test]
async fn a_redirect_to_the_top_page_makes_a_candidate() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let fetcher = serve(|path| match path {
        "/old-301.html" => redirect(301, "/"),
        "/old-302.html" => redirect(302, "/"),
        _ => page(None),
    })
    .await;
    for path in ["/old-301.html", "/old-302.html"] {
        let id = register(&db, &url(path)).await;
        let done = run(&db, &fetcher, id, path).await;
        assert_eq!(
            done.verdict.observation,
            Some(Observation::NotFound(Why::TopRedirect)),
            "{path}"
        );
        assert_eq!(resource(&db, &url(path)).await.state, CANDIDATE, "{path}");
    }
    // トップの資源に寄っていない
    assert_eq!(all_resources(&db).await.len(), 2);
}

#[tokio::test]
async fn a_soft_404_title_makes_a_candidate() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let fetcher = serve(|path| match path {
        "/soft.html" => titled("ページが見つかりません｜○○市"),
        _ => titled("児童手当のご案内"),
    })
    .await;
    let soft = register(&db, &url("/soft.html")).await;
    let ok = register(&db, &url("/ok.html")).await;
    run(&db, &fetcher, soft, "/soft.html").await;
    run(&db, &fetcher, ok, "/ok.html").await;
    assert_eq!(resource(&db, &url("/soft.html")).await.state, CANDIDATE);
    assert_eq!(resource(&db, &url("/ok.html")).await.state, "active");
}

#[tokio::test]
async fn a_permanent_redirect_to_a_404_makes_the_target_a_candidate() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let fetcher = serve(|path| match path {
        "/smph/a.html" => redirect(301, "/a.html"),
        _ => empty(404),
    })
    .await;
    let id = register(&db, &url("/smph/a.html")).await;
    run(&db, &fetcher, id, "/smph/a.html").await;
    // 資源は転送先の1つに寄り、それが削除候補になる
    let resources = all_resources(&db).await;
    assert_eq!(resources.len(), 1);
    assert_eq!(resources[0].canonical_url, url("/a.html"));
    assert_eq!(resources[0].state, CANDIDATE);
}

#[tokio::test]
async fn transient_and_blocked_responses_leave_a_live_resource_alone() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let status = Arc::new(std::sync::atomic::AtomicU16::new(200));
    let fetcher = {
        let status = status.clone();
        serve(move |_| match status.load(Ordering::SeqCst) {
            200 => page(None),
            other => empty(other),
        })
        .await
    };
    let id = register(&db, &url("/a.html")).await;
    run(&db, &fetcher, id, "/a.html").await;

    for (code, job) in [(403, Job::Blocked), (503, Job::Retry), (500, Job::Retry)] {
        status.store(code, Ordering::SeqCst);
        let done = run(&db, &fetcher, id, "/a.html").await;
        assert_eq!(done.verdict.job, job, "{code}");
        assert!(done.transition.is_none(), "{code}");
        let resource = resource(&db, &url("/a.html")).await;
        assert_eq!(
            (resource.state.as_str(), resource.consecutive_not_found),
            ("active", 0)
        );
    }
}

#[tokio::test]
async fn a_content_move_renames_the_resource_and_keeps_it_active() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let moved = Arc::new(AtomicBool::new(false));
    let fetcher = {
        let moved = moved.clone();
        serve(move |path| match path {
            "/a.html" if moved.load(Ordering::SeqCst) => redirect(301, "/b.html"),
            _ => page(None),
        })
        .await
    };
    let id = register(&db, &url("/a.html")).await;
    run(&db, &fetcher, id, "/a.html").await;
    let before = resource(&db, &url("/a.html")).await;

    moved.store(true, Ordering::SeqCst);
    let done = run(&db, &fetcher, id, "/a.html").await;
    assert_eq!(
        done.resolved,
        Some(Resolved::Renamed {
            resource_id: before.id,
            from: url("/a.html"),
            to: url("/b.html"),
        })
    );
    let all = all_resources(&db).await;
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].id, before.id);
    assert_eq!(all[0].canonical_url, url("/b.html"));
    assert_eq!(all[0].state, "active");
    assert!(all[0].last_changed_at.is_none());
    let links = links_of(&db, id).await;
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].relation, "redirect");

    // もう一度流しても同じ（今度は代表 URL が変わらない）
    let again = run(&db, &fetcher, id, "/a.html").await;
    assert!(matches!(
        again.linked,
        Some(Linked::Linked { created: false, .. })
    ));
    assert!(again.resolved.is_none());
}

#[tokio::test]
async fn absorption_marks_the_old_resource_moved_and_never_forms_a_loop() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let a_moves = Arc::new(AtomicBool::new(false));
    let b_moves = Arc::new(AtomicBool::new(false));
    let fetcher = {
        let (a_moves, b_moves) = (a_moves.clone(), b_moves.clone());
        serve(move |path| match path {
            "/a.html" if a_moves.load(Ordering::SeqCst) => redirect(301, "/b.html"),
            "/b.html" if b_moves.load(Ordering::SeqCst) => redirect(301, "/a.html"),
            _ => page(None),
        })
        .await
    };
    let a = register(&db, &url("/a.html")).await;
    let b = register(&db, &url("/b.html")).await;
    run(&db, &fetcher, a, "/a.html").await;
    run(&db, &fetcher, b, "/b.html").await;
    let (old, into) = (
        resource(&db, &url("/a.html")).await,
        resource(&db, &url("/b.html")).await,
    );

    // a が、既にある b に吸収される
    a_moves.store(true, Ordering::SeqCst);
    let done = run(&db, &fetcher, a, "/a.html").await;
    assert_eq!(
        done.resolved,
        Some(Resolved::Absorbed {
            from_resource_id: old.id,
            into_resource_id: into.id,
        })
    );
    let old_after = resource(&db, &url("/a.html")).await;
    assert_eq!(old_after.state, "moved");
    assert_eq!(old_after.moved_to_resource_id, Some(into.id));
    assert!(old_after.last_changed_at.is_none());
    assert_eq!(links_of(&db, a).await.len(), 2);
    // 吸収先は生きている
    assert_eq!(resource(&db, &url("/b.html")).await.state, "active");

    // 吸収済みのまま流し直しても、行も状態も増えない
    let again = run(&db, &fetcher, a, "/a.html").await;
    assert!(again.resolved.is_none());
    assert_eq!(resource(&db, &url("/a.html")).await.state, "moved");
    assert_eq!(links_of(&db, a).await.len(), 2);

    // b が a へ転送されても、moved の輪を作らない
    a_moves.store(false, Ordering::SeqCst);
    b_moves.store(true, Ordering::SeqCst);
    let done = run(&db, &fetcher, b, "/b.html").await;
    assert_eq!(
        done.resolved,
        Some(Resolved::Refused {
            from_resource_id: into.id,
            into_resource_id: old.id,
        })
    );
    assert!(done.transition.is_none());
    assert_eq!(resource(&db, &url("/b.html")).await.state, "active");
    assert_eq!(resource(&db, &url("/a.html")).await.state, "moved");
    assert_eq!(links_of(&db, b).await.len(), 1);
}

#[tokio::test]
async fn a_move_to_another_host_is_recorded_and_touches_no_resource() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let fetcher = serve(|path| match path {
        "/a.html" => redirect(301, "http://www.other.example.jp/a.html"),
        "/b.html" => redirect(301, "http://www.other.example.jp/elsewhere.html"),
        _ => page(None),
    })
    .await;

    let a = register(&db, &url("/a.html")).await;
    let done = run(&db, &fetcher, a, "/a.html").await;
    assert_eq!(done.verdict.job, Job::Blocked);
    assert_eq!(done.verdict.error_type, Some("host_moved"));
    assert!(all_resources(&db).await.is_empty());
    let mv = done.verdict.host_move.expect("移行の候補");
    assert!(host_moves::approved_hosts(&db).await.unwrap().is_empty());
    assert!(
        host_moves::approve(&db, &mv.from_host_key, &mv.to_host_key)
            .await
            .unwrap()
    );
    assert!(
        host_moves::approved_hosts(&db)
            .await
            .unwrap()
            .contains(&mv.to_host_key)
    );

    // パスが変わる転送は移行の候補にしない
    let b = register(&db, &url("/b.html")).await;
    let done = run(&db, &fetcher, b, "/b.html").await;
    assert_eq!(done.verdict.error_type, Some("out_of_scope"));
    assert!(done.verdict.host_move.is_none());
}
