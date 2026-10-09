//! 死んだ制度への関連 URL の紐付けを実際の Postgres に流す。
//!
//! `TEST_DATABASE_URL` のデータベースは毎回作り直す（本番の接続先を入れないこと）。未設定ならスキップする。

mod common;

use std::collections::BTreeMap;

use domain::related::Reason;
use pipeline::import_registry::import;
use pipeline::link_related::{Counts, link};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement, prelude::Uuid};
use serde_json::Value;

const FIXTURE: &str = include_str!("../../domain/tests/fixtures/registry_sample.json");

/// fixture を取り込む（児童手当・出生届・都の金銭的支援の3制度、主たる URL 3件）
async fn import_fixture(db: &DatabaseConnection) {
    import_json(db, FIXTURE).await;
}

async fn import_json(db: &DatabaseConnection, json: &str) {
    let imported = domain::registry::parse(json).unwrap();
    import(db, imported).await.expect("取り込める");
}

/// fixture の index 番目の行の psid
fn psid_of(index: usize) -> String {
    let rows: Vec<Value> = serde_json::from_str(FIXTURE).unwrap();
    domain::registry::parse(&serde_json::to_string(&rows).unwrap())
        .unwrap()
        .programs[index]
        .psid
        .clone()
}

/// fixture の index 番目の行の主たる URL の id
async fn main_url_id(db: &DatabaseConnection, index: usize) -> Uuid {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            db.get_database_backend(),
            "select pu.url_id from program_urls pu join programs p on p.id = pu.program_id \
             where p.psid = $1 and pu.role = 'main'",
            [psid_of(index).into()],
        ))
        .await
        .unwrap()
        .unwrap();
    row.try_get_by_index(0).unwrap()
}

/// fixture の index 番目の行の主たる URL に、その状態の資源を結ぶ
/// （resources と url_resources に1行ずつ。同じ URL に2回呼べるよう、canonical_url は呼ぶたびに別の値にする）
async fn main_resource(db: &DatabaseConnection, index: usize, state: &str) {
    let url_id = main_url_id(db, index).await;
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            db.get_database_backend(),
            "insert into resources (canonical_url, final_url, canonical_source, state) \
             values ('https://www.city.example.jp/resource/' || gen_random_uuid(), \
                     'https://www.city.example.jp/', 'normalized', $1) returning id",
            [state.into()],
        ))
        .await
        .unwrap()
        .unwrap();
    let resource_id: Uuid = row.try_get_by_index(0).unwrap();
    db.execute_raw(Statement::from_sql_and_values(
        db.get_database_backend(),
        "insert into url_resources (url_id, resource_id, relation, observed_at) \
         values ($1, $2, 'direct', now())",
        [url_id.into(), resource_id.into()],
    ))
    .await
    .unwrap();
}

/// その制度の related を rank 順に（normalized_url, rank）で返す
async fn related_of(db: &DatabaseConnection, index: usize) -> Vec<(String, i32)> {
    db.query_all_raw(Statement::from_sql_and_values(
        db.get_database_backend(),
        "select u.normalized_url, pu.rank from program_urls pu \
         join programs p on p.id = pu.program_id join urls u on u.id = pu.url_id \
         where p.psid = $1 and pu.role = 'related' order by pu.rank",
        [psid_of(index).into()],
    ))
    .await
    .unwrap()
    .into_iter()
    .map(|row| {
        (
            row.try_get_by_index(0).unwrap(),
            row.try_get_by_index(1).unwrap(),
        )
    })
    .collect()
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
async fn only_dead_programs_get_related_urls() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };
    // 児童手当 = deleted、出生届 = deletion_candidate、都の制度 = active
    import_fixture(&db).await;
    main_resource(&db, 0, "deleted").await;
    main_resource(&db, 1, "deletion_candidate").await;
    main_resource(&db, 2, "active").await;
    let counts = link(&db).await.unwrap();
    assert_eq!(
        related_of(&db, 0).await,
        [(
            "https://www.city.example.jp/smph/kosodatekyoiku/N84/kakusyuteate/jidoteate/teatekaisei.html"
                .to_string(),
            1
        )]
    );
    assert_eq!(related_of(&db, 1).await.len(), 3);
    assert_eq!(
        related_of(&db, 1)
            .await
            .iter()
            .map(|(_, rank)| *rank)
            .collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert!(related_of(&db, 2).await.is_empty());
    assert_eq!(counts.dead_programs, 2);
    assert_eq!(counts.programs_with_related, 2);
    assert_eq!(counts.links, 4);
    // 出生届の3件目は児童手当の主たる URL（既にある）。残り3件が新しい
    assert_eq!((counts.new_urls, counts.existing_urls), (3, 1));
    assert_eq!(counts.dropped[&Reason::OverLimit], 3);
    assert_eq!(
        count(&db, "select count(*) from urls where priority = 70").await,
        3
    );
    assert_eq!(
        count(
            &db,
            "select count(*) from urls where priority = 70 and (role <> 'seed' or status <> 'queued')"
        )
        .await,
        0
    );
    assert_eq!(
        count(
            &db,
            "select count(*) from program_urls where role = 'related' and source <> 'registry'"
        )
        .await,
        0
    );
}

#[tokio::test]
async fn programs_whose_main_url_is_not_dead_are_skipped() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };
    // 資源が無い（未取得）・blocked・moved は対象にしない
    import_fixture(&db).await;
    main_resource(&db, 1, "moved").await;
    // 児童手当の主たる URL を blocked にする（資源は無いまま）
    let blocked = main_url_id(&db, 0).await;
    common::set_status(&db, blocked, "blocked", "now() + interval '7 days'").await;
    let counts = link(&db).await.unwrap();
    assert_eq!(
        counts,
        Counts {
            dead_programs: 0,
            programs_with_related: 0,
            links: 0,
            new_urls: 0,
            existing_urls: 0,
            dropped: BTreeMap::new()
        }
    );
    assert_eq!(count(&db, "select count(*) from urls").await, 3);
}

#[tokio::test]
async fn the_latest_resource_decides() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };
    // 古い結び付きは deleted、最新は active → 対象にしない
    import_fixture(&db).await;
    main_resource(&db, 0, "deleted").await;
    // observed_at を過去にずらしてから、active の資源を結ぶ
    db.execute_unprepared("update url_resources set observed_at = now() - interval '1 day'")
        .await
        .unwrap();
    main_resource(&db, 0, "active").await;
    assert_eq!(link(&db).await.unwrap().dead_programs, 0);
}

#[tokio::test]
async fn running_twice_changes_nothing() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };
    import_fixture(&db).await;
    main_resource(&db, 0, "deleted").await;
    main_resource(&db, 1, "deleted").await;
    let first = link(&db).await.unwrap();
    let before = count(&db, "select count(*) from program_urls").await;
    let second = link(&db).await.unwrap();
    assert_eq!(
        count(&db, "select count(*) from program_urls").await,
        before
    );
    assert_eq!(second.links, first.links);
    assert_eq!((second.new_urls, second.existing_urls), (0, 4));
}

#[tokio::test]
async fn existing_urls_keep_their_crawl_state() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };
    // 出生届の候補の1つは児童手当の主たる URL。巡回の途中の状態を持たせておく
    import_fixture(&db).await;
    main_resource(&db, 1, "deleted").await;
    let shared = main_url_id(&db, 0).await;
    common::set_status(&db, shared, "retry_wait", "now() + interval '5 minutes'").await;
    link(&db).await.unwrap();
    assert_eq!(
        count(
            &db,
            &format!(
                "select count(*) from urls where id = '{shared}' and status = 'retry_wait' \
                 and priority = 80 and next_crawl_at > now()"
            )
        )
        .await,
        1
    );
}

#[tokio::test]
async fn stale_links_of_dead_programs_are_replaced_and_revived_programs_keep_theirs() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };
    import_fixture(&db).await;
    main_resource(&db, 0, "deleted").await;
    main_resource(&db, 1, "deleted").await;
    link(&db).await.unwrap();
    // 出生届の description から URL を無くして取り込み直す → 流し直すと related が消える
    let mut rows: Vec<Value> = serde_json::from_str(FIXTURE).unwrap();
    rows[1]["description"] = "".into();
    import_json(&db, &serde_json::to_string(&rows).unwrap()).await;
    // 児童手当は生き返らせる（url_resources を過去にずらして active を結ぶ）→ related は残る
    db.execute_unprepared("update url_resources set observed_at = now() - interval '1 day'")
        .await
        .unwrap();
    main_resource(&db, 0, "active").await;
    let counts = link(&db).await.unwrap();
    assert!(related_of(&db, 1).await.is_empty());
    assert_eq!(related_of(&db, 0).await.len(), 1);
    assert_eq!(counts.dead_programs, 1);
}
