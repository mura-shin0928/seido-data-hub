//! 取得した結果から代表 URL を決め、URL を資源に結ぶところを、テスト用サーバーと実際の Postgres に流す。
//!
//! `TEST_DATABASE_URL` のデータベースは毎回作り直す（本番の接続先を入れないこと）。未設定ならスキップする。

mod common;

use std::sync::Arc;

use common::*;
use domain::canonical::{Relation, Source};
use domain::extract;
use entity::{resources, url_resources};
use pipeline::fetch::{Body, Fetcher, Outcome};
use pipeline::resources::{Linked, decide, link};
use sea_orm::{DatabaseConnection, EntityTrait, PaginatorTrait, prelude::Uuid};

/// 取得して、決めて、結ぶ。資源に結ばない取得なら None
async fn crawl(
    db: &DatabaseConnection,
    fetcher: &Fetcher,
    url_id: Uuid,
    raw: &str,
    host_trusted: bool,
) -> Option<Linked> {
    let fetch = fetcher.fetch(raw, &Default::default()).await;
    let declared = match &fetch.outcome {
        Outcome::Response(response) => match &response.body {
            Body::Html(html) => extract::extract(&html.text, &response.url).declared_canonical_url,
            _ => None,
        },
        _ => None,
    };
    let decision = decide(&fetch, declared.as_deref(), host_trusted)?;
    Some(link(db, url_id, &decision).await.unwrap())
}

#[tokio::test]
async fn urls_redirected_to_the_same_page_share_one_resource() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let fetcher = serve(|path| match path {
        "/old/a.html" => redirect(301, "/new.html"),
        "/old/b.html" => redirect(308, &url("/new.html")),
        "/new.html" => page(None),
        _ => reply(404).body(Default::default()).unwrap(),
    })
    .await;

    let a = register(&db, &url("/old/a.html")).await;
    let b = register(&db, &url("/old/b.html")).await;
    let new = register(&db, &url("/new.html")).await;
    for (id, path) in [(a, "/old/a.html"), (b, "/old/b.html"), (new, "/new.html")] {
        crawl(&db, &fetcher, id, &url(path), true).await.unwrap();
    }

    let resources = all_resources(&db).await;
    assert_eq!(resources.len(), 1, "{resources:?}");
    let resource = &resources[0];
    assert_eq!(resource.canonical_url, url("/new.html"));
    assert_eq!(resource.final_url, url("/new.html"));
    // 転送先そのものを後から結んでも、根拠は強い方（転送）が残る
    assert_eq!(resource.canonical_source, Source::Redirect.as_str());
    assert_eq!(resource.state, "active");

    for (id, relation) in [
        (a, Relation::Redirect),
        (b, Relation::Redirect),
        (new, Relation::Direct),
    ] {
        let links = links_of(&db, id).await;
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].resource_id, resource.id);
        assert_eq!(links[0].relation, relation.as_str());
    }
}

#[tokio::test]
async fn misconfigured_canonicals_are_ignored() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let fetcher = serve(|path| match path {
        // テンプレートが全ページにトップを入れている
        "/a.html" | "/b.html" => page(Some("/")),
        "/c/index.html" => page(Some("/c/")),
        _ => reply(404).body(Default::default()).unwrap(),
    })
    .await;

    for path in ["/a.html", "/b.html"] {
        let id = register(&db, &url(path)).await;
        crawl(&db, &fetcher, id, &url(path), true).await.unwrap();
    }
    // ホストを信用しないと決めたときは、それ自体は妥当な申告も採らない
    let c = register(&db, &url("/c/index.html")).await;
    crawl(&db, &fetcher, c, &url("/c/index.html"), false)
        .await
        .unwrap();

    let mut resources = all_resources(&db).await;
    resources.sort_by(|x, y| x.canonical_url.cmp(&y.canonical_url));
    let canonicals: Vec<_> = resources.iter().map(|r| r.canonical_url.as_str()).collect();
    assert_eq!(
        canonicals,
        [url("/a.html"), url("/b.html"), url("/c/index.html")]
    );
    for resource in &resources {
        assert_eq!(resource.canonical_source, Source::Normalized.as_str());
    }
    // 申告は採らなくても記録する
    assert_eq!(resources[0].declared_canonical_url, Some(url("/")));
    assert_eq!(resources[2].declared_canonical_url, Some(url("/c/")));
}

#[tokio::test]
async fn trusted_declarations_fold_urls() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let fetcher = serve(|path| match path {
        "/c/index.html" | "/c/" => page(Some("/c/")),
        _ => reply(404).body(Default::default()).unwrap(),
    })
    .await;

    let index = register(&db, &url("/c/index.html")).await;
    let dir = register(&db, &url("/c/")).await;
    crawl(&db, &fetcher, index, &url("/c/index.html"), true)
        .await
        .unwrap();
    crawl(&db, &fetcher, dir, &url("/c/"), true).await.unwrap();

    let resources = all_resources(&db).await;
    assert_eq!(resources.len(), 1);
    assert_eq!(resources[0].canonical_url, url("/c/"));
    assert_eq!(resources[0].canonical_source, Source::Declared.as_str());
    assert_eq!(links_of(&db, index).await[0].relation, "declared");
    assert_eq!(links_of(&db, dir).await[0].relation, "direct");
}

#[tokio::test]
async fn linking_the_same_result_again_adds_no_rows() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let fetcher = serve(|path| match path {
        "/old.html" => redirect(301, "/new.html"),
        _ => page(None),
    })
    .await;
    let id = register(&db, &url("/old.html")).await;

    let first = crawl(&db, &fetcher, id, &url("/old.html"), true).await;
    let second = crawl(&db, &fetcher, id, &url("/old.html"), true).await;
    let Some(Linked::Linked {
        resource_id,
        created: true,
    }) = first
    else {
        panic!("{first:?}");
    };
    assert_eq!(
        second,
        Some(Linked::Linked {
            resource_id,
            created: false
        })
    );
    assert_eq!(resources::Entity::find().count(&db).await.unwrap(), 1);
    assert_eq!(url_resources::Entity::find().count(&db).await.unwrap(), 1);
}

#[tokio::test]
async fn a_changed_canonical_is_reported_without_rewriting() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let moved = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let fetcher = {
        let moved = moved.clone();
        serve(move |path| match path {
            "/a.html" if moved.load(std::sync::atomic::Ordering::SeqCst) => {
                redirect(301, "/b.html")
            }
            _ => page(None),
        })
        .await
    };
    let id = register(&db, &url("/a.html")).await;
    let Some(Linked::Linked { resource_id, .. }) =
        crawl(&db, &fetcher, id, &url("/a.html"), true).await
    else {
        panic!("結べる");
    };

    moved.store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        crawl(&db, &fetcher, id, &url("/a.html"), true).await,
        Some(Linked::Changed {
            resource_id,
            previous_canonical_url: url("/a.html"),
            canonical_url: url("/b.html"),
        })
    );
    // 資源も結び付きも増えていない
    assert_eq!(all_resources(&db).await.len(), 1);
    assert_eq!(links_of(&db, id).await.len(), 1);
}

#[tokio::test]
async fn failures_do_not_touch_resources() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let fetcher = serve(|path| match path {
        "/forbidden.html" => reply(403).body(Default::default()).unwrap(),
        "/down.html" => reply(503).body(Default::default()).unwrap(),
        "/gone.html" => reply(404).body(Default::default()).unwrap(),
        _ => page(None),
    })
    .await;
    for path in ["/forbidden.html", "/down.html"] {
        let id = register(&db, &url(path)).await;
        assert_eq!(crawl(&db, &fetcher, id, &url(path), true).await, None);
    }
    assert!(all_resources(&db).await.is_empty());

    // 404 は削除候補として資源が要る
    let gone = register(&db, &url("/gone.html")).await;
    assert!(
        crawl(&db, &fetcher, gone, &url("/gone.html"), true)
            .await
            .is_some()
    );
    assert_eq!(all_resources(&db).await.len(), 1);
}
