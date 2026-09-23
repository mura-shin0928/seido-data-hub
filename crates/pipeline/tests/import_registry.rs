//! 取り込みを実際の Postgres に流す。upsert の SQL と外部キーは単体テストでは確かめられないため。
//!
//! `TEST_DATABASE_URL` のデータベースは毎回作り直す（本番の接続先を入れないこと）。未設定ならスキップする。

use migration::{Migrator, MigratorTrait};
use pipeline::import_registry::{Counts, import};
use sea_orm::{ConnectionTrait, Database, DatabaseConnection, Statement};
use serde_json::Value;
use tokio::sync::{Mutex, MutexGuard};

const FIXTURE: &str = include_str!("../../domain/tests/fixtures/registry_sample.json");

/// テストは1つのデータベースを共有し、それぞれが `Migrator::fresh` で作り直す。
/// 同時に走ると互いのスキーマを消してしまうので、ここで直列にする
static DB: Mutex<()> = Mutex::const_new(());

async fn fresh_db() -> Option<(DatabaseConnection, MutexGuard<'static, ()>)> {
    let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!("TEST_DATABASE_URL が無いのでスキップ");
        return None;
    };
    // 取り込みの中で panic したテストがあっても、次のテストは作り直すので影響しない
    let guard = DB.lock().await;
    let db = Database::connect(&url).await.expect("接続できる");
    Migrator::fresh(&db).await.expect("migration が流れる");
    Some((db, guard))
}

async fn count(db: &DatabaseConnection, sql: &str) -> i64 {
    let row = db
        .query_one_raw(Statement::from_string(db.get_database_backend(), sql))
        .await
        .unwrap()
        .unwrap();
    row.try_get_by_index(0).unwrap()
}

async fn text(db: &DatabaseConnection, sql: &str) -> String {
    let row = db
        .query_one_raw(Statement::from_string(db.get_database_backend(), sql))
        .await
        .unwrap()
        .unwrap();
    row.try_get_by_index(0).unwrap()
}

async fn run(db: &DatabaseConnection, json: &str) -> Counts {
    let imported = domain::registry::parse(json).unwrap();
    let outcome = import(db, imported).await.expect("取り込める");
    assert!(outcome.rejected.is_empty(), "{:?}", outcome.rejected);
    outcome.counts
}

/// 指定した行の主たる URL を差し替えた JSON を作る
fn with_source_url(index: usize, uri: &str) -> String {
    let mut rows: Vec<Value> = serde_json::from_str(FIXTURE).unwrap();
    rows[index]["localGovernmentLink"]["uri"] = uri.into();
    serde_json::to_string(&rows).unwrap()
}

#[tokio::test]
async fn importing_twice_gives_the_same_rows() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };

    for _ in 0..2 {
        assert_eq!(
            run(&db, FIXTURE).await,
            Counts {
                areas: 2,
                programs: 3,
                urls: 3,
                program_urls: 3,
            }
        );
    }

    assert_eq!(count(&db, "select count(*) from areas").await, 2);
    assert_eq!(count(&db, "select count(*) from programs").await, 3);
    assert_eq!(count(&db, "select count(*) from urls").await, 3);
    assert_eq!(count(&db, "select count(*) from program_urls").await, 3);
    assert_eq!(
        count(
            &db,
            "select count(*) from areas where code = '131130' and parent_code = '130001'"
        )
        .await,
        1
    );
    // 1制度の main はちょうど1つ
    assert_eq!(
        count(
            &db,
            "select count(*) from (select program_id from program_urls where role = 'main'
             group by program_id having count(*) <> 1) as broken"
        )
        .await,
        0
    );
    // 投入時に決まるのは dedup_key まで。巡回の状態は既定値から始まる
    assert_eq!(
        count(
            &db,
            "select count(*) from urls where status = 'queued' and role = 'seed'
             and depth = 0 and priority = 80"
        )
        .await,
        3
    );
}

#[tokio::test]
async fn reimport_does_not_reset_crawl_state() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    run(&db, FIXTURE).await;

    db.execute_unprepared(
        "update urls set status = 'succeeded', attempt_count = 1,
         next_crawl_at = now() + interval '7 days'",
    )
    .await
    .unwrap();

    run(&db, FIXTURE).await;

    assert_eq!(
        count(&db, "select count(*) from urls where status = 'succeeded'").await,
        3
    );
    assert_eq!(
        count(&db, "select count(*) from urls where attempt_count = 1").await,
        3
    );
    assert_eq!(
        count(
            &db,
            "select count(*) from urls where next_crawl_at > now() + interval '6 days'"
        )
        .await,
        3
    );
}

#[tokio::test]
async fn spelling_variants_share_one_url_row() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };

    // 2行目を1行目と同じページ（フラグメント違い）にする
    let rows: Vec<Value> = serde_json::from_str(FIXTURE).unwrap();
    let first = rows[0]["localGovernmentLink"]["uri"].as_str().unwrap();
    let json = with_source_url(1, &format!("{first}#anchor"));

    let counts = run(&db, &json).await;
    assert_eq!(counts.programs, 3);
    assert_eq!(counts.urls, 2, "URL は畳まれる");
    assert_eq!(counts.program_urls, 3, "制度の数だけ結び付きは残る");

    assert_eq!(count(&db, "select count(*) from urls").await, 2);
    // 畳まれた側の raw_url は、最初に見た表記が残る（フラグメントは付かない）
    assert_eq!(
        count(&db, "select count(*) from urls where raw_url like '%#%'").await,
        0
    );
    assert_eq!(
        count(
            &db,
            "select count(*) from program_urls pu join urls u on u.id = pu.url_id
             where u.normalized_url not like '%#%'"
        )
        .await,
        3
    );
}

#[tokio::test]
async fn changing_the_registry_url_moves_the_main_link() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    run(&db, FIXTURE).await;

    let json = with_source_url(0, "https://www.city.example.jp/kosodate/atarashii.html");
    run(&db, &json).await;

    // 新しい URL が main になり、古い URL の行は履歴として残る
    assert_eq!(count(&db, "select count(*) from urls").await, 4);
    assert_eq!(
        count(&db, "select count(*) from program_urls where role = 'main'").await,
        3
    );
    assert_eq!(
        text(
            &db,
            "select u.normalized_url from program_urls pu
             join urls u on u.id = pu.url_id
             join programs p on p.id = pu.program_id
             where p.um = 'UM24' and pu.role = 'main'"
        )
        .await,
        "https://www.city.example.jp/kosodate/atarashii.html"
    );
}
