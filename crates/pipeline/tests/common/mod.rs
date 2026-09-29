//! 統合テストの共通部品: テスト用サーバーと Postgres の準備

#![allow(dead_code)]

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::Request;
use axum::http::{StatusCode, header};
use axum::response::Response as HttpResponse;
use entity::{resources, url_resources, urls as urls_table};
use migration::{Migrator, MigratorTrait};
use pipeline::fetch::{Config, Fetcher};
use sea_orm::{
    ActiveValue::Set, ColumnTrait, Database, DatabaseConnection, EntityTrait, QueryFilter,
    prelude::Uuid,
};
use tokio::sync::{Mutex, MutexGuard};

pub const CITY: &str = "www.city.example.jp";

/// テストは1つのデータベースを共有し、それぞれが作り直す。同時に走ると互いのスキーマを消すので直列にする
static DB: Mutex<()> = Mutex::const_new(());

pub async fn fresh_db() -> Option<(DatabaseConnection, MutexGuard<'static, ()>)> {
    let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!("TEST_DATABASE_URL が無いのでスキップ");
        return None;
    };
    let guard = DB.lock().await;
    let db = Database::connect(&url).await.expect("接続できる");
    Migrator::fresh(&db).await.expect("migration が流れる");
    Some((db, guard))
}

type Respond = dyn Fn(&str) -> HttpResponse + Send + Sync;

/// CITY だけを持つテスト用サーバー（robots.txt は無し）を立て、そこへ向けた `Fetcher` を作る
pub async fn serve(respond: impl Fn(&str) -> HttpResponse + Send + Sync + 'static) -> Fetcher {
    let respond: Arc<Respond> = Arc::new(respond);
    let app = Router::new().fallback(move |request: Request| {
        let respond = respond.clone();
        async move {
            match request.uri().path() {
                // robots.txt が無いホスト（制限なし）
                "/robots.txt" => reply(404).body(Default::default()).unwrap(),
                path => respond(path),
            }
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let config = Config {
        min_interval: Duration::from_millis(20),
        ..Config::default()
    };
    let client = Fetcher::client_builder(&config)
        .resolve(CITY, addr)
        .build()
        .unwrap();
    let allowed = BTreeSet::from([format!("http://{CITY}")]);
    Fetcher::with_client(client, config, allowed)
}

pub fn url(path: &str) -> String {
    format!("http://{CITY}{path}")
}

pub fn reply(status: u16) -> axum::http::response::Builder {
    HttpResponse::builder().status(StatusCode::from_u16(status).unwrap())
}

pub fn page(canonical: Option<&str>) -> HttpResponse {
    let link = canonical
        .map(|href| format!(r#"<link rel="canonical" href="{href}">"#))
        .unwrap_or_default();
    reply(200)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(format!("<html><head>{link}</head><body><main>本文</main></body></html>").into())
        .unwrap()
}

/// `<title>` を持つ 200 のページ
pub fn titled(title: &str) -> HttpResponse {
    reply(200)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(
            format!(
                "<html><head><title>{title}</title></head><body><main>本文</main></body></html>"
            )
            .into(),
        )
        .unwrap()
}

pub fn redirect(status: u16, location: &str) -> HttpResponse {
    reply(status)
        .header(header::LOCATION, location)
        .body(Default::default())
        .unwrap()
}

/// `urls` に1行入れて id を返す
pub async fn register(db: &DatabaseConnection, raw: &str) -> Uuid {
    let prepared = domain::urls::prepare(raw).unwrap();
    urls_table::Entity::insert(urls_table::ActiveModel {
        raw_url: Set(prepared.raw_url),
        normalized_url: Set(prepared.normalized_url),
        dedup_key: Set(prepared.dedup_key),
        host_key: Set(prepared.host_key),
        ..Default::default()
    })
    .exec(db)
    .await
    .unwrap()
    .last_insert_id
}

pub async fn all_resources(db: &DatabaseConnection) -> Vec<resources::Model> {
    resources::Entity::find().all(db).await.unwrap()
}

pub async fn links_of(db: &DatabaseConnection, url_id: Uuid) -> Vec<url_resources::Model> {
    url_resources::Entity::find()
        .filter(url_resources::Column::UrlId.eq(url_id))
        .all(db)
        .await
        .unwrap()
}
