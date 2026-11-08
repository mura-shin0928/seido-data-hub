//! 移転先の候補づくりを実際の Postgres に流す。
//!
//! `TEST_DATABASE_URL` のデータベースは毎回作り直す（本番の接続先を入れないこと）。未設定ならスキップする。

mod common;

use std::collections::BTreeSet;

use pipeline::import_registry::import;
use pipeline::suggest_moves::{HostCoverage, suggest};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement, prelude::Uuid};

const FIXTURE: &str = include_str!("../../domain/tests/fixtures/registry_sample.json");

/// fixture を取り込む（児童手当・出生届・都の金銭的支援の3制度、主たる URL 3件）
async fn import_fixture(db: &DatabaseConnection) {
    let imported = domain::registry::parse(FIXTURE).unwrap();
    import(db, imported).await.expect("取り込める");
}

async fn one<T: sea_orm::TryGetable>(
    db: &DatabaseConnection,
    sql: &str,
    v: Vec<sea_orm::Value>,
) -> T {
    db.query_one_raw(Statement::from_sql_and_values(
        db.get_database_backend(),
        sql,
        v,
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get_by_index(0)
    .unwrap()
}

/// 名前の制度の id
async fn program_id(db: &DatabaseConnection, name: &str) -> Uuid {
    one(
        db,
        "select id from programs where canonical_name = $1",
        vec![name.into()],
    )
    .await
}

/// 名前の制度の主たる URL の host_key
async fn main_host(db: &DatabaseConnection, name: &str) -> String {
    one(
        db,
        "select u.host_key from program_urls pu join programs p on p.id = pu.program_id \
         join urls u on u.id = pu.url_id where p.canonical_name = $1 and pu.role = 'main'",
        vec![name.into()],
    )
    .await
}

/// 資源を作って url_resources でその URL に加える。資源の id を返す
async fn resource_for(db: &DatabaseConnection, url_id: Uuid, canonical: &str, state: &str) -> Uuid {
    let resource_id: Uuid = one(
        db,
        "insert into resources (canonical_url, final_url, canonical_source, state) \
         values ($1, $1, 'normalized', $2) returning id",
        vec![canonical.into(), state.into()],
    )
    .await;
    db.execute_raw(Statement::from_sql_and_values(
        db.get_database_backend(),
        "insert into url_resources (url_id, resource_id, relation, observed_at) \
         values ($1, $2, 'direct', now())",
        [url_id.into(), resource_id.into()],
    ))
    .await
    .unwrap();
    resource_id
}

/// 名前の制度の主たる URL に、その状態の資源を加える
async fn main_resource(db: &DatabaseConnection, name: &str, state: &str) {
    let url_id: Uuid = one(
        db,
        "select pu.url_id from program_urls pu join programs p on p.id = pu.program_id \
         where p.canonical_name = $1 and pu.role = 'main'",
        vec![name.into()],
    )
    .await;
    let canonical = format!("https://main.example.jp/{url_id}/{state}");
    resource_for(db, url_id, &canonical, state).await;
}

/// 一覧ページ（role = 'index'）の行を作り、その状態の資源を加える。資源の id を返す
async fn index_page(db: &DatabaseConnection, url: &str, status: &str, state: &str) -> Uuid {
    let url_id = common::register(db, url).await;
    db.execute_raw(Statement::from_sql_and_values(
        db.get_database_backend(),
        "update urls set role = 'index', status = $2 where id = $1",
        [url_id.into(), status.into()],
    ))
    .await
    .unwrap();
    resource_for(db, url_id, url, state).await
}

async fn add_link(db: &DatabaseConnection, resource_id: Uuid, url: &str, text: &str) {
    let prepared = domain::urls::prepare(url).unwrap();
    db.execute_raw(Statement::from_sql_and_values(
        db.get_database_backend(),
        "insert into page_links (resource_id, url, dedup_key, host_key, text) \
         values ($1, $2, $3, $4, $5)",
        [
            resource_id.into(),
            prepared.normalized_url.into(),
            prepared.dedup_key.into(),
            prepared.host_key.into(),
            text.into(),
        ],
    ))
    .await
    .unwrap();
}

/// 児童手当を死なせ、同じホストの一覧ページ（資源の状態を指定）に「児童手当」のリンクを置く。
/// 一覧ページの URL・候補の URL・一覧ページの資源の id を返す
async fn dead_with_index(db: &DatabaseConnection, index_state: &str) -> (String, String, Uuid) {
    import_fixture(db).await;
    main_resource(db, "児童手当", "deleted").await;
    let host = main_host(db, "児童手当").await;
    let page = format!("{host}/list/index.html");
    let target = format!("{host}/list/jidoteate-new.html");
    let resource = index_page(db, &page, "queued", index_state).await;
    add_link(db, resource, &target, "児童手当").await;
    (page, target, resource)
}

#[tokio::test]
async fn dead_program_gets_named_candidate() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };
    let (page, target, _) = dead_with_index(&db, "active").await;
    let report = suggest(&db, &BTreeSet::new()).await.unwrap();
    assert_eq!(report.targets, 1);
    assert_eq!(report.suggestions.len(), 1);
    let suggestion = &report.suggestions[0];
    assert_eq!(suggestion.program_id, program_id(&db, "児童手当").await);
    assert_eq!(suggestion.canonical_name, "児童手当");
    assert_eq!(suggestion.more, 0);
    assert_eq!(suggestion.candidates.len(), 1);
    let candidate = &suggestion.candidates[0];
    assert_eq!(candidate.url, target);
    assert_eq!(candidate.score, 1.0);
    assert_eq!(candidate.page, page);
    assert_eq!(report.skipped_judged, 0);
}

#[tokio::test]
async fn live_programs_are_not_targets() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };
    import_fixture(&db).await;
    main_resource(&db, "児童手当", "active").await;
    let host = main_host(&db, "児童手当").await;
    let resource = index_page(&db, &format!("{host}/list/"), "queued", "active").await;
    add_link(&db, resource, &format!("{host}/a/new.html"), "児童手当").await;
    let report = suggest(&db, &BTreeSet::new()).await.unwrap();
    assert_eq!(report.targets, 0);
    assert!(report.suggestions.is_empty());
}

#[tokio::test]
async fn judged_pairs_are_skipped() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };
    let (_, target, _) = dead_with_index(&db, "active").await;
    let key = domain::urls::prepare(&target).unwrap().dedup_key;
    let judged = BTreeSet::from([(program_id(&db, "児童手当").await, key.clone())]);
    let report = suggest(&db, &judged).await.unwrap();
    assert!(report.suggestions.is_empty());
    assert_eq!(report.skipped_judged, 1);
    // 別の制度についての判定では外れない
    let other = BTreeSet::from([(program_id(&db, "出生届").await, key)]);
    let report = suggest(&db, &other).await.unwrap();
    assert_eq!(report.suggestions.len(), 1);
    assert_eq!(report.skipped_judged, 0);
}

#[tokio::test]
async fn dead_index_pages_are_not_used() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };
    dead_with_index(&db, "deleted").await;
    let report = suggest(&db, &BTreeSet::new()).await.unwrap();
    assert_eq!(report.targets, 1);
    assert!(report.suggestions.is_empty());
}

#[tokio::test]
async fn other_host_pages_are_not_used() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };
    import_fixture(&db).await;
    main_resource(&db, "児童手当", "deleted").await;
    let resource = index_page(&db, "https://other.example.org/list/", "queued", "active").await;
    add_link(
        &db,
        resource,
        "https://other.example.org/a/new.html",
        "児童手当",
    )
    .await;
    let report = suggest(&db, &BTreeSet::new()).await.unwrap();
    assert!(report.suggestions.is_empty());
}

#[tokio::test]
async fn own_urls_are_not_candidates() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };
    let (_, target, _) = dead_with_index(&db, "active").await;
    // 候補の URL が、その制度の関連 URL として既にある
    let url_id = common::register(&db, &target).await;
    db.execute_raw(Statement::from_sql_and_values(
        db.get_database_backend(),
        "insert into program_urls (program_id, url_id, role, rank, source) \
         values ($1, $2, 'related', 1, 'discovered')",
        [program_id(&db, "児童手当").await.into(), url_id.into()],
    ))
    .await
    .unwrap();
    let report = suggest(&db, &BTreeSet::new()).await.unwrap();
    assert!(report.suggestions.is_empty());
}

#[tokio::test]
async fn unusable_hosts_are_listed() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };
    // a: 5件すべて deleted（retired が1件あっても数えない）、b: 4件すべて deleted、c: 5件のうち1件が active
    for i in 0..5 {
        index_page(
            &db,
            &format!("https://a.example.jp/d{i}/"),
            "queued",
            "deleted",
        )
        .await;
        index_page(
            &db,
            &format!("https://c.example.jp/d{i}/"),
            "queued",
            if i == 0 { "active" } else { "deleted" },
        )
        .await;
    }
    for i in 0..4 {
        index_page(
            &db,
            &format!("https://b.example.jp/d{i}/"),
            "queued",
            "deleted",
        )
        .await;
    }
    index_page(&db, "https://b.example.jp/gone/", "retired", "deleted").await;
    index_page(&db, "https://a.example.jp/gone/", "retired", "active").await;
    let report = suggest(&db, &BTreeSet::new()).await.unwrap();
    assert_eq!(
        report.unusable_hosts,
        [HostCoverage {
            host_key: "https://a.example.jp".into(),
            starts: 5,
            live_pages: 0
        }]
    );
}
