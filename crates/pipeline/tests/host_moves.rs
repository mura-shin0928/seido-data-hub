//! ホスト移行の記録と承認を、実際の Postgres に流す。
//!
//! `TEST_DATABASE_URL` のデータベースは毎回作り直す（本番の接続先を入れないこと）。未設定ならスキップする。

mod common;

use domain::liveness::HostMove;
use pipeline::host_moves::{approve, approve_by_hand, approved_hosts, record};
use sea_orm::{ConnectionTrait, Statement};

fn fukushi() -> HostMove {
    HostMove {
        from_host_key: "https://www.old.example.jp".to_string(),
        to_host_key: "https://www.new.example.jp".to_string(),
    }
}

#[tokio::test]
async fn a_recorded_move_joins_the_allow_list_only_after_approval() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };

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

#[tokio::test]
async fn a_move_confirmed_by_hand_joins_the_allow_list_without_being_observed() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };

    // 観測していない移行を、人が確かめて承認する
    let (old, new) = (
        "https://www.gone.example.jp",
        "https://www.moved.example.jp",
    );
    approve_by_hand(&db, old, new, "https://www.gone.example.jp/a.html")
        .await
        .unwrap();
    approve_by_hand(&db, old, new, "https://www.gone.example.jp/b.html")
        .await
        .unwrap();
    assert_eq!(
        approved_hosts(&db)
            .await
            .unwrap()
            .into_iter()
            .collect::<Vec<_>>(),
        vec![new.to_string()]
    );
    let rows = db
        .query_all_raw(Statement::from_string(
            db.get_database_backend(),
            "select sample_url from host_moves",
        ))
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    let sample: String = rows[0].try_get_by_index(0).unwrap();
    assert_eq!(sample, "https://www.gone.example.jp/a.html");

    // 観測して proposed になっていた組も承認になり、最初に見た日時は残る
    record(&db, &fukushi(), "https://www.old.example.jp/a.html")
        .await
        .unwrap();
    let first_seen = "select first_seen_at::text from host_moves where from_host_key = 'https://www.old.example.jp'";
    let before: String = db
        .query_one_raw(Statement::from_string(
            db.get_database_backend(),
            first_seen,
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get_by_index(0)
        .unwrap();
    approve_by_hand(
        &db,
        "https://www.old.example.jp",
        "https://www.new.example.jp",
        "https://www.old.example.jp/z.html",
    )
    .await
    .unwrap();
    let after: String = db
        .query_one_raw(Statement::from_string(
            db.get_database_backend(),
            first_seen,
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get_by_index(0)
        .unwrap();
    assert_eq!(before, after);
    assert_eq!(approved_hosts(&db).await.unwrap().len(), 2);
}
