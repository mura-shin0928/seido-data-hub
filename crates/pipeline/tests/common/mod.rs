//! 統合テストの共通部品: テスト用サーバーと Postgres の準備

#![allow(dead_code)]

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::Request;
use axum::http::{StatusCode, header};
use axum::response::Response as HttpResponse;
use entity::{crawl_runs, resources, url_resources, urls as urls_table};
use migration::{Migrator, MigratorTrait};
use pipeline::fetch::{Body, Config, Fetch, Fetcher, Hop, Outcome, Response};
use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveValue::Set, ColumnTrait, ConnectionTrait, Database, DatabaseConnection, DbBackend,
    EntityTrait, QueryFilter, Statement, prelude::Uuid,
};
use tokio::sync::{Mutex, MutexGuard};

pub const CITY: &str = "www.city.example.jp";
/// 2つ目のホスト（ホストをまたぐテスト用）
pub const TOWN: &str = "www.town.example.jp";

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
    // robots.txt が無いホスト（制限なし）
    serve_with_robots(|| reply(404).body(Default::default()).unwrap(), respond).await
}

/// `serve` の robots.txt の応答を `robots` にしたもの
pub async fn serve_with_robots(
    robots: impl Fn() -> HttpResponse + Send + Sync + 'static,
    respond: impl Fn(&str) -> HttpResponse + Send + Sync + 'static,
) -> Fetcher {
    serve_hosts(&[CITY], move |_| robots(), respond).await
}

/// `hosts` のどれも同じテスト用サーバーへ向け、すべて許可リストに入れる。
/// robots.txt はホスト名（Host ヘッダーからポートを除いたもの）ごとに `robots` が返す
pub async fn serve_hosts(
    hosts: &[&str],
    robots: impl Fn(&str) -> HttpResponse + Send + Sync + 'static,
    respond: impl Fn(&str) -> HttpResponse + Send + Sync + 'static,
) -> Fetcher {
    let robots = Arc::new(robots);
    let respond: Arc<Respond> = Arc::new(respond);
    let app = Router::new().fallback(move |request: Request| {
        let robots = robots.clone();
        let respond = respond.clone();
        async move {
            match request.uri().path() {
                "/robots.txt" => {
                    let host = request
                        .headers()
                        .get(header::HOST)
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or_default();
                    robots(host.split(':').next().unwrap_or_default())
                }
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
    let mut builder = Fetcher::client_builder(&config);
    for host in hosts {
        builder = builder.resolve(host, addr);
    }
    let client = builder.build().unwrap();
    let allowed: BTreeSet<String> = hosts.iter().map(|host| format!("http://{host}")).collect();
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

/// `crawl_runs` に1行入れて id を返す
pub async fn start_run(db: &DatabaseConnection) -> Uuid {
    crawl_runs::Entity::insert(crawl_runs::ActiveModel {
        kind: Set("test".to_string()),
        ..Default::default()
    })
    .exec(db)
    .await
    .unwrap()
    .last_insert_id
}

/// 304 の応答（本文を読んでいない）
pub fn not_modified(path: &str) -> Fetch {
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

/// HTML を読んだ応答から本文を取り出す。取り出せない応答は `None`
pub fn extracted_of(fetch: &Fetch) -> Option<domain::extract::Extracted> {
    pipeline::crawl::extract_of(fetch)
}

/// 本文だけが違う 200 のページ
pub fn body_page(text: &str) -> HttpResponse {
    reply(200)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(format!("<html><head></head><body><main>{text}</main></body></html>").into())
        .unwrap()
}

/// URL の優先度を変える
pub async fn set_priority(db: &DatabaseConnection, id: Uuid, priority: i32) {
    urls_table::Entity::update_many()
        .col_expr(urls_table::Column::Priority, Expr::value(priority))
        .filter(urls_table::Column::Id.eq(id))
        .exec(db)
        .await
        .unwrap();
}

/// URL の状態と次に取る時刻を変える。`next_crawl_at_sql` は `"now() - interval '1 second'"` のような SQL 片
pub async fn set_status(db: &DatabaseConnection, id: Uuid, status: &str, next_crawl_at_sql: &str) {
    db.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        format!("update urls set status = $1, next_crawl_at = {next_crawl_at_sql} where id = $2"),
        [status.into(), id.into()],
    ))
    .await
    .unwrap();
}
