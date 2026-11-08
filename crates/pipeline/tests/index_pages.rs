//! 起点（主たる URL の親ディレクトリ）の登録を実際の Postgres に流す。
//!
//! `TEST_DATABASE_URL` のデータベースは毎回作り直す（本番の接続先を入れないこと）。未設定ならスキップする。

mod common;

use pipeline::import_registry::import;
use pipeline::index_pages::{Counts, INDEX_PRIORITY, register};
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

/// fixture の index 番目の行の主たる URL に、その状態の資源を加える
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

async fn count(db: &DatabaseConnection, sql: &str) -> i64 {
    let row = db
        .query_one_raw(Statement::from_string(db.get_database_backend(), sql))
        .await
        .unwrap()
        .unwrap();
    row.try_get_by_index(0).unwrap()
}

const JIDOTEATE: &str =
    "https://www.city.example.jp/smph/kosodatekyoiku/N84/kakusyuteate/jidoteate/";
const SYUSSYOU: &str = "https://www.city.example.jp/smph/kurashi/410/zyumininkankoseki/412/";
const FUKUSHI: &str = "https://www.fukushi.metro.tokyo.lg.jp/kodomo/kosodate/018/";

/// role = 'index' の行を normalized_url 順に（normalized_url, depth, priority, status）で返す
async fn index_rows(db: &DatabaseConnection) -> Vec<(String, i32, i32, String)> {
    db.query_all_raw(Statement::from_string(
        db.get_database_backend(),
        "select normalized_url, depth, priority, status from urls where role = 'index' order by 1",
    ))
    .await
    .unwrap()
    .into_iter()
    .map(|row| {
        (
            row.try_get_by_index(0).unwrap(),
            row.try_get_by_index(1).unwrap(),
            row.try_get_by_index(2).unwrap(),
            row.try_get_by_index(3).unwrap(),
        )
    })
    .collect()
}

#[tokio::test]
async fn starts_are_parent_dirs_of_main_urls() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };
    import_fixture(&db).await;
    let counts = register(&db).await.unwrap();
    assert_eq!(
        counts,
        Counts {
            starts: 3,
            new_urls: 3,
            existing_urls: 0,
            parked: 0,
            restored: 0
        }
    );
    let queued = |url: &str| (url.to_string(), 0, INDEX_PRIORITY, "queued".to_string());
    assert_eq!(
        index_rows(&db).await,
        [queued(JIDOTEATE), queued(SYUSSYOU), queued(FUKUSHI)]
    );
    assert_eq!(INDEX_PRIORITY, 90);
    assert_eq!(
        count(
            &db,
            "select count(*) from urls where role = 'index' and next_crawl_at <= now()"
        )
        .await,
        3
    );
}

#[tokio::test]
async fn register_is_idempotent() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };
    import_fixture(&db).await;
    register(&db).await.unwrap();
    let rows = count(&db, "select count(*) from urls").await;
    let second = register(&db).await.unwrap();
    assert_eq!(
        second,
        Counts {
            starts: 3,
            new_urls: 0,
            existing_urls: 3,
            parked: 0,
            restored: 0
        }
    );
    assert_eq!(count(&db, "select count(*) from urls").await, rows);
}

#[tokio::test]
async fn existing_rows_are_not_touched() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };
    import_fixture(&db).await;
    // 起点と同じ dedup_key の seed の行を先に作る
    let prepared = domain::urls::prepare(FUKUSHI).unwrap();
    db.execute_raw(Statement::from_sql_and_values(
        db.get_database_backend(),
        "insert into urls (raw_url, normalized_url, dedup_key, host_key, role, priority, status) \
         values ($1, $2, $3, $4, 'seed', 50, 'blocked')",
        [
            prepared.raw_url.into(),
            prepared.normalized_url.into(),
            prepared.dedup_key.into(),
            prepared.host_key.into(),
        ],
    ))
    .await
    .unwrap();
    let counts = register(&db).await.unwrap();
    assert_eq!((counts.new_urls, counts.existing_urls), (2, 1));
    assert_eq!(
        count(
            &db,
            "select count(*) from urls where normalized_url = 'https://www.fukushi.metro.tokyo.lg.jp/kodomo/kosodate/018/' \
             and role = 'seed' and priority = 50 and status = 'blocked'"
        )
        .await,
        1
    );
    assert_eq!(index_rows(&db).await.len(), 2);
}

#[tokio::test]
async fn dropped_starts_are_parked_and_restored() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };
    import_fixture(&db).await;
    register(&db).await.unwrap();

    // 児童手当の主たる URL を別のディレクトリの URL に差し替える
    let original = main_url_id(&db, 0).await;
    let other = domain::urls::prepare("https://www.city.example.jp/other/page.html").unwrap();
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            db.get_database_backend(),
            "insert into urls (raw_url, normalized_url, dedup_key, host_key) \
             values ($1, $2, $3, $4) returning id",
            [
                other.raw_url.into(),
                other.normalized_url.into(),
                other.dedup_key.into(),
                other.host_key.into(),
            ],
        ))
        .await
        .unwrap()
        .unwrap();
    let other_id: Uuid = row.try_get_by_index(0).unwrap();
    swap_main(&db, 0, other_id).await;

    let counts = register(&db).await.unwrap();
    assert_eq!((counts.parked, counts.restored), (1, 0));
    assert_eq!(counts.starts, 3);
    assert_eq!(
        count(
            &db,
            &format!(
                "select count(*) from urls where normalized_url = '{JIDOTEATE}' and status = 'retired'"
            )
        )
        .await,
        1
    );
    // 続けて流しても、もう動かすものは無い
    let again = register(&db).await.unwrap();
    assert_eq!((again.parked, again.restored), (0, 0));

    // 主たる URL を戻す → queued・now に戻る
    swap_main(&db, 0, original).await;
    let counts = register(&db).await.unwrap();
    assert_eq!((counts.parked, counts.restored), (1, 1));
    assert_eq!(
        count(
            &db,
            &format!(
                "select count(*) from urls where normalized_url = '{JIDOTEATE}' \
                 and status = 'queued' and next_crawl_at <= now()"
            )
        )
        .await,
        1
    );
}

#[tokio::test]
async fn seed_rows_are_never_parked() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };
    import_fixture(&db).await;
    // seed は起点に無くても触らない（ホスト移行で retired にした行も戻さない）
    let counts = register(&db).await.unwrap();
    assert_eq!(counts.parked, 0);
    assert_eq!(
        count(
            &db,
            "select count(*) from urls where role = 'seed' and status = 'queued'"
        )
        .await,
        3
    );
    db.execute_unprepared("update urls set status = 'retired' where role = 'seed'")
        .await
        .unwrap();
    let counts = register(&db).await.unwrap();
    assert_eq!((counts.parked, counts.restored), (0, 0));
    assert_eq!(
        count(
            &db,
            "select count(*) from urls where role = 'seed' and status = 'retired'"
        )
        .await,
        3
    );
}

#[tokio::test]
async fn dead_main_urls_still_give_starts() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };
    import_fixture(&db).await;
    main_resource(&db, 0, "deleted").await;
    let counts = register(&db).await.unwrap();
    assert_eq!(counts.starts, 3);
    assert!(
        index_rows(&db)
            .await
            .iter()
            .any(|(url, ..)| url == JIDOTEATE)
    );
}

/// fixture の index 番目の行の主たる URL を別の URL に差し替える
async fn swap_main(db: &DatabaseConnection, index: usize, url_id: Uuid) {
    db.execute_raw(Statement::from_sql_and_values(
        db.get_database_backend(),
        "update program_urls set url_id = $1 where role = 'main' \
         and program_id = (select id from programs where psid = $2)",
        [url_id.into(), psid_of(index).into()],
    ))
    .await
    .unwrap();
}

#[tokio::test]
async fn no_main_urls_is_an_error() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };
    // 制度が無いのに一覧ページの行だけがある（取り込みの失敗や接続先の取り違え）
    let prepared = domain::urls::prepare(FUKUSHI).unwrap();
    db.execute_raw(Statement::from_sql_and_values(
        db.get_database_backend(),
        "insert into urls (raw_url, normalized_url, dedup_key, host_key, role, priority) \
         values ($1, $2, $3, $4, 'index', 90)",
        [
            prepared.raw_url.into(),
            prepared.normalized_url.into(),
            prepared.dedup_key.into(),
            prepared.host_key.into(),
        ],
    ))
    .await
    .unwrap();
    assert!(register(&db).await.is_err());
    assert_eq!(
        index_rows(&db).await,
        [(FUKUSHI.to_string(), 0, 90, "queued".to_string())]
    );
}
