//! Router を実際の Postgres に向けて叩く。自治体の解決・絞り込み・並び順は SQL で決まるため。
//!
//! `TEST_DATABASE_URL` のデータベースは作り直す（本番の接続先を入れないこと）。未設定ならスキップする。
//! フィクスチャ: 小金井市の UM24 児童手当（003・0〜36か月未満）と UM3 出生届（002・月齢なし）、東京都の UM58（004・0か月以上）。

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use migration::{Migrator, MigratorTrait};
use sea_orm::{Database, DatabaseConnection};
use serde_json::Value;
use tokio::sync::OnceCell;
use tower::ServiceExt;

const FIXTURE: &str = include_str!("../../domain/tests/fixtures/registry_sample.json");

static SEEDED: OnceCell<()> = OnceCell::const_new();

/// テストごとに runtime が別なので、接続もテストごとに張る。作り直しと取り込みは最初の1回だけ。
async fn db() -> Option<DatabaseConnection> {
    let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!("TEST_DATABASE_URL が無いのでスキップ");
        return None;
    };
    SEEDED
        .get_or_init(|| async {
            let db = Database::connect(&url).await.expect("接続できる");
            Migrator::fresh(&db).await.expect("migration が流れる");
            let imported = domain::registry::parse(FIXTURE).unwrap();
            pipeline::import_registry::import(&db, imported)
                .await
                .expect("取り込める");
            db.close().await.unwrap();
        })
        .await;
    Some(Database::connect(&url).await.expect("接続できる"))
}

async fn get(db: DatabaseConnection, uri: &str) -> (StatusCode, Value) {
    let res = api::router(db)
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).expect("JSON が返る"))
}

fn ums(body: &Value) -> Vec<(&str, &str)> {
    body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (p["area_code"].as_str().unwrap(), p["um"].as_str().unwrap()))
        .collect()
}

#[tokio::test]
async fn areas_are_listed_with_attribution() {
    let Some(db) = db().await else { return };
    let (status, body) = get(db, "/v1/areas").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"][0]["code"], "130001");
    assert_eq!(body["data"][1]["parent_code"], "130001");
    assert_eq!(body["attribution"]["license"], "CC BY 4.0");
}

#[tokio::test]
async fn municipality_includes_its_prefecture() {
    let Some(db) = db().await else { return };
    let (status, body) = get(db, "/v1/areas/132101/programs").await;

    assert_eq!(status, StatusCode::OK);
    // 市区町村が先、UM は数値順（UM3 が UM24 より前）
    assert_eq!(
        ums(&body),
        [("132101", "UM3"), ("132101", "UM24"), ("130001", "UM58")]
    );
    assert!(
        body["data"][0].get("registry").is_none(),
        "一覧には registry を載せない"
    );
    assert!(body["attribution"]["source"].is_string());
}

#[tokio::test]
async fn prefecture_has_only_its_own_programs() {
    let Some(db) = db().await else { return };
    let (_, body) = get(db, "/v1/areas/130001/programs").await;
    assert_eq!(ums(&body), [("130001", "UM58")]);
}

#[tokio::test]
async fn age_filter_keeps_programs_without_bounds() {
    let Some(db) = db().await else { return };
    // 36か月は児童手当（36か月未満）の外。月齢の無い出生届は残る
    let (_, body) = get(db.clone(), "/v1/areas/132101/programs?age_months=36").await;
    assert_eq!(ums(&body), [("132101", "UM3"), ("130001", "UM58")]);

    let (_, body) = get(db, "/v1/areas/132101/programs?age_months=35").await;
    assert_eq!(ums(&body).len(), 3);
}

#[tokio::test]
async fn category_filter() {
    let Some(db) = db().await else { return };
    let (_, body) = get(db, "/v1/areas/132101/programs?category=003").await;
    assert_eq!(ums(&body), [("132101", "UM24")]);
}

#[tokio::test]
async fn program_detail_has_the_registry_row() {
    let Some(db) = db().await else { return };
    let (_, list) = get(db.clone(), "/v1/areas/132101/programs").await;
    let id = list["data"][0]["id"].as_str().unwrap();

    let (status, body) = get(db, &format!("/v1/programs/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["canonical_name"], "出生届");
    assert_eq!(
        body["data"]["registry"]["basicInformation"]["psid"],
        body["data"]["psid"]
    );
    assert_eq!(body["attribution"]["license"], "CC BY 4.0");
}

#[tokio::test]
async fn errors_are_json() {
    let Some(db) = db().await else { return };
    for (uri, expected) in [
        ("/v1/areas/999999/programs", StatusCode::NOT_FOUND),
        ("/v1/programs/not-a-uuid", StatusCode::NOT_FOUND),
        (
            "/v1/programs/00000000-0000-0000-0000-000000000000",
            StatusCode::NOT_FOUND,
        ),
        (
            "/v1/areas/132101/programs?age_months=abc",
            StatusCode::BAD_REQUEST,
        ),
        (
            "/v1/areas/132101/programs?age_months=-1",
            StatusCode::BAD_REQUEST,
        ),
        ("/nope", StatusCode::NOT_FOUND),
    ] {
        let (status, body) = get(db.clone(), uri).await;
        assert_eq!(status, expected, "{uri}");
        assert!(body["error"]["message"].is_string(), "{uri}");
    }
}
