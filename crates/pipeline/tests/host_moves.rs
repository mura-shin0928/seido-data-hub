//! ホスト移行の記録と承認を、実際の Postgres に流す。
//!
//! `TEST_DATABASE_URL` のデータベースは毎回作り直す（本番の接続先を入れないこと）。未設定ならスキップする。

use domain::liveness::HostMove;
use migration::{Migrator, MigratorTrait};
use pipeline::host_moves::{approve, approved_hosts, record};
use sea_orm::Database;

fn fukushi() -> HostMove {
    HostMove {
        from_host_key: "https://www.old.example.jp".to_string(),
        to_host_key: "https://www.new.example.jp".to_string(),
    }
}

#[tokio::test]
async fn a_recorded_move_joins_the_allow_list_only_after_approval() {
    let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!("TEST_DATABASE_URL が無いのでスキップ");
        return;
    };
    let db = Database::connect(&url).await.unwrap();
    Migrator::fresh(&db).await.unwrap();

    record(&db, &fukushi(), "https://www.old.example.jp/a.html")
        .await
        .unwrap();
    // 同じ移行を何度見ても行は増えない
    record(&db, &fukushi(), "https://www.old.example.jp/b.html")
        .await
        .unwrap();
    assert!(approved_hosts(&db).await.unwrap().is_empty());

    // 観測していない組は承認できない
    assert!(
        !approve(&db, "https://x.example.jp", "https://y.example.jp")
            .await
            .unwrap()
    );
    assert!(approved_hosts(&db).await.unwrap().is_empty());

    assert!(
        approve(
            &db,
            "https://www.old.example.jp",
            "https://www.new.example.jp"
        )
        .await
        .unwrap()
    );
    let hosts = approved_hosts(&db).await.unwrap();
    assert_eq!(
        hosts.into_iter().collect::<Vec<_>>(),
        vec!["https://www.new.example.jp".to_string()]
    );

    // 承認済みの移行を、また見つけても承認は取り消されない
    record(&db, &fukushi(), "https://www.old.example.jp/c.html")
        .await
        .unwrap();
    assert_eq!(approved_hosts(&db).await.unwrap().len(), 1);
}
