//! 制度が返す事実（主たる URL のいまの資源・関連リンク）を、資源を入れた Postgres で確かめる。
//!
//! `TEST_DATABASE_URL` のデータベースはテストごとに作り直す（本番の接続先を入れないこと）。未設定ならスキップする。
//! フィクスチャは `api.rs` と同じ（渋谷区の UM24 児童手当・UM3 出生届、東京都の UM58）。資源は SQL で直に入れる。

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use migration::{Migrator, MigratorTrait};
use sea_orm::{ConnectionTrait, Database, DatabaseConnection};
use serde_json::Value;
use tokio::sync::{Mutex, MutexGuard};
use tower::ServiceExt;

const FIXTURE: &str = include_str!("../../domain/tests/fixtures/registry_sample.json");

/// テストは1つのデータベースを共有し、それぞれが作り直すので直列にする
static DB: Mutex<()> = Mutex::const_new(());

async fn fresh_db() -> Option<(DatabaseConnection, MutexGuard<'static, ()>)> {
    let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!("TEST_DATABASE_URL が無いのでスキップ");
        return None;
    };
    let guard = DB.lock().await;
    let db = Database::connect(&url).await.expect("接続できる");
    Migrator::fresh(&db).await.expect("migration が流れる");
    let imported = domain::registry::parse(FIXTURE).unwrap();
    pipeline::import_registry::import(&db, imported)
        .await
        .expect("取り込める");
    Some((db, guard))
}

async fn get(db: &DatabaseConnection, uri: &str) -> (StatusCode, Value) {
    let res = api::router(db.clone())
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).expect("JSON が返る"))
}

async fn sql(db: &DatabaseConnection, statement: &str) {
    db.execute_unprepared(statement).await.expect(statement);
}

/// 渋谷区の一覧から UM を引く
async fn program<'a>(list: &'a Value, um: &str) -> &'a Value {
    list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["um"] == um)
        .unwrap()
}

/// UM24 の主たる URL の id を返す SQL の式
const UM24_MAIN: &str = "(select pu.url_id from program_urls pu join programs p on p.id = pu.program_id \
     where p.um = 'UM24' and pu.role = 'main')";

/// 資源を作り、`url_expr` の URL を `observed_at` に結ぶ
async fn attach(
    db: &DatabaseConnection,
    url_expr: &str,
    canonical: &str,
    state: &str,
    observed_at: &str,
) {
    sql(
        db,
        &format!(
            "insert into resources (canonical_url, final_url, canonical_source, state, last_crawled_at) \
             values ('{canonical}', '{canonical}', 'normalized', '{state}', '2026-10-12T03:17:00+09:00') \
             on conflict (canonical_url) do nothing;
             insert into url_resources (url_id, resource_id, relation, observed_at) \
             select {url_expr}, id, 'direct', '{observed_at}' from resources where canonical_url = '{canonical}';"
        ),
    )
    .await;
}

/// UM24 に関連 URL を結ぶ。`resource` があれば、その状態の資源に結ぶ
async fn relate(db: &DatabaseConnection, rank: i32, resource: Option<&str>) {
    let raw = format!("https://www.city.example.jp/related{rank}.html");
    sql(
        db,
        &format!(
            "insert into urls (raw_url, normalized_url, dedup_key, host_key) \
             values ('{raw}', '{raw}', '{raw}', 'https://www.city.example.jp');
             insert into program_urls (program_id, url_id, role, rank) \
             select p.id, u.id, 'related', {rank} from programs p, urls u \
             where p.um = 'UM24' and u.dedup_key = '{raw}';"
        ),
    )
    .await;
    if let Some(state) = resource {
        let url_expr = format!("(select id from urls where dedup_key = '{raw}')");
        attach(db, &url_expr, &raw, state, "2026-10-12T03:17:00+09:00").await;
    }
}

#[tokio::test]
async fn unchecked_programs_have_facts_but_no_status() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let (status, body) = get(&db, "/v1/areas/131130/programs").await;

    assert_eq!(status, StatusCode::OK);
    let programs = body["data"].as_array().unwrap();
    assert_eq!(programs.len(), 3);
    for p in programs {
        assert_eq!(p["url"], p["source_url"], "{p}");
        for key in ["state", "checked_at", "changed_at", "moved_to"] {
            assert!(p[key].is_null(), "{key}: {p}");
        }
        assert_eq!(p["auto_update"], true);
        assert_eq!(p["related_links"], serde_json::json!([]));
        assert!(p.get("status").is_none(), "status は返さない: {p}");
    }
}

#[tokio::test]
async fn deleted_program_shows_related_links_in_list_and_detail() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    attach(
        &db,
        UM24_MAIN,
        "https://www.city.example.jp/dead.html",
        "deleted",
        "2026-10-12T03:17:00+09:00",
    )
    .await;
    relate(&db, 2, None).await;
    relate(&db, 1, Some("active")).await;

    let (_, list) = get(&db, "/v1/areas/131130/programs").await;
    let um24 = program(&list, "UM24").await;
    assert_eq!(um24["state"], "deleted");
    assert_eq!(um24["url"], "https://www.city.example.jp/dead.html");
    let links = um24["related_links"].as_array().unwrap();
    assert_eq!(links.len(), 2);
    assert_eq!(links[0]["url"], "https://www.city.example.jp/related1.html");
    assert_eq!(links[0]["state"], "active");
    assert!(links[0]["checked_at"].is_string());
    assert_eq!(links[1]["url"], "https://www.city.example.jp/related2.html");
    assert!(links[1]["state"].is_null());
    assert_eq!(
        program(&list, "UM3").await["related_links"],
        serde_json::json!([])
    );

    let id = um24["id"].as_str().unwrap();
    let (status, detail) = get(&db, &format!("/v1/programs/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["data"]["state"], "deleted");
    assert_eq!(detail["data"]["related_links"], um24["related_links"]);
    assert!(detail["data"].get("status").is_none());
}

#[tokio::test]
async fn latest_observed_resource_wins() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    attach(
        &db,
        UM24_MAIN,
        "https://www.city.example.jp/old.html",
        "active",
        "2026-10-05T03:17:00+09:00",
    )
    .await;
    attach(
        &db,
        UM24_MAIN,
        "https://www.city.example.jp/new.html",
        "deleted",
        "2026-10-12T03:17:00+09:00",
    )
    .await;

    let (_, list) = get(&db, "/v1/areas/131130/programs").await;
    let um24 = program(&list, "UM24").await;
    assert_eq!(um24["state"], "deleted");
    assert_eq!(um24["url"], "https://www.city.example.jp/new.html");
}

#[tokio::test]
async fn blocked_url_turns_off_auto_update() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    sql(
        &db,
        &format!("update urls set status = 'blocked' where id = {UM24_MAIN}"),
    )
    .await;

    let (_, list) = get(&db, "/v1/areas/131130/programs").await;
    for p in list["data"].as_array().unwrap() {
        assert_eq!(p["auto_update"], p["um"] != "UM24", "{p}");
    }
}
