//! 取り込みを実際の Postgres に流す。upsert の SQL と外部キーは単体テストでは確かめられないため。
//!
//! `TEST_DATABASE_URL` のデータベースは毎回作り直す（本番の接続先を入れないこと）。未設定ならスキップする。

use migration::{Migrator, MigratorTrait};
use pipeline::import_registry::{Counts, import};
use sea_orm::{ConnectionTrait, Database, DatabaseConnection, Statement};

const FIXTURE: &str = include_str!("../../domain/tests/fixtures/registry_sample.json");

async fn fresh_db() -> Option<DatabaseConnection> {
    let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!("TEST_DATABASE_URL が無いのでスキップ");
        return None;
    };
    let db = Database::connect(&url).await.expect("接続できる");
    Migrator::fresh(&db).await.expect("migration が流れる");
    Some(db)
}

async fn count(db: &DatabaseConnection, sql: &str) -> i64 {
    let row = db
        .query_one_raw(Statement::from_string(db.get_database_backend(), sql))
        .await
        .unwrap()
        .unwrap();
    row.try_get_by_index(0).unwrap()
}

#[tokio::test]
async fn importing_twice_gives_the_same_rows() {
    let Some(db) = fresh_db().await else { return };

    for _ in 0..2 {
        let imported = domain::registry::parse(FIXTURE).unwrap();
        let counts = import(&db, imported).await.expect("取り込める");
        assert_eq!(
            counts,
            Counts {
                areas: 2,
                programs: 3
            }
        );
    }

    assert_eq!(count(&db, "select count(*) from areas").await, 2);
    assert_eq!(count(&db, "select count(*) from programs").await, 3);
    assert_eq!(
        count(
            &db,
            "select count(*) from programs where status = 'registry_only'"
        )
        .await,
        3
    );
    assert_eq!(
        count(
            &db,
            "select count(*) from areas where code = '131130' and parent_code = '130001'"
        )
        .await,
        1
    );
}
