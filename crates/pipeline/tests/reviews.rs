//! 人が判定した移転先を、実際の Postgres に反映する。
//!
//! `TEST_DATABASE_URL` のデータベースは毎回作り直す（本番の接続先を入れないこと）。未設定ならスキップする。
//! 候補の URL の資源は巡回を流さずに SQL で作る。

mod common;

use pipeline::claim::{self, Exclude};
use pipeline::import_registry::import;
use pipeline::reviews::{
    LinkReport, RefuseReason, RegisterCounts, ReviewFile, Verdict, link, parse, register,
};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement, prelude::Uuid};

const FIXTURE: &str = include_str!("../../domain/tests/fixtures/registry_sample.json");

/// fixture の制度（取り込み順）の id と主たる URL
struct Program {
    id: Uuid,
    main_url_id: Uuid,
    main_url: String,
}

async fn setup(db: &DatabaseConnection) -> Vec<Program> {
    import(db, domain::registry::parse(FIXTURE).unwrap())
        .await
        .expect("取り込める");
    db.query_all_raw(Statement::from_string(
        db.get_database_backend(),
        "select p.id, u.id, u.normalized_url from programs p \
         join program_urls pu on pu.program_id = p.id and pu.role = 'main' \
         join urls u on u.id = pu.url_id order by p.psid",
    ))
    .await
    .unwrap()
    .into_iter()
    .map(|row| Program {
        id: row.try_get_by_index(0).unwrap(),
        main_url_id: row.try_get_by_index(1).unwrap(),
        main_url: row.try_get_by_index(2).unwrap(),
    })
    .collect()
}

async fn query_uuid(db: &DatabaseConnection, sql: &str, values: Vec<sea_orm::Value>) -> Uuid {
    db.query_one_raw(Statement::from_sql_and_values(
        db.get_database_backend(),
        sql,
        values,
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get_by_index(0)
    .unwrap()
}

/// その状態の資源を1つ作る
async fn resource(db: &DatabaseConnection, state: &str) -> Uuid {
    query_uuid(
        db,
        "insert into resources (canonical_url, final_url, canonical_source, state) \
         values ('https://www.city.example.jp/resource/' || gen_random_uuid(), \
                 'https://www.city.example.jp/', 'normalized', $1) returning id",
        vec![state.into()],
    )
    .await
}

/// URL のいまの資源にする（observed_at = now()）
async fn attach(db: &DatabaseConnection, url_id: Uuid, resource_id: Uuid) {
    db.execute_raw(Statement::from_sql_and_values(
        db.get_database_backend(),
        "insert into url_resources (url_id, resource_id, relation, observed_at) \
         values ($1, $2, 'direct', now())",
        [url_id.into(), resource_id.into()],
    ))
    .await
    .unwrap();
}

/// 候補の URL を urls に入れ、その状態の資源を結ぶ（巡回で取った後の形）
async fn candidate(db: &DatabaseConnection, raw: &str, state: &str) -> Uuid {
    let url_id = common::register(db, raw).await;
    let resource_id = resource(db, state).await;
    attach(db, url_id, resource_id).await;
    resource_id
}

async fn dead_main(db: &DatabaseConnection, program: &Program, state: &str) -> Uuid {
    let resource_id = resource(db, state).await;
    attach(db, program.main_url_id, resource_id).await;
    resource_id
}

async fn state_of(db: &DatabaseConnection, id: Uuid) -> (String, Option<Uuid>) {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            db.get_database_backend(),
            "select state, moved_to_resource_id from resources where id = $1",
            [id.into()],
        ))
        .await
        .unwrap()
        .unwrap();
    (
        row.try_get_by_index(0).unwrap(),
        row.try_get_by_index(1).unwrap(),
    )
}

/// その制度の主たる URL（normalized_url, source）
async fn main_of(db: &DatabaseConnection, program_id: Uuid) -> (String, String) {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            db.get_database_backend(),
            "select u.normalized_url, pu.source from program_urls pu join urls u on u.id = pu.url_id \
             where pu.program_id = $1 and pu.role = 'main'",
            [program_id.into()],
        ))
        .await
        .unwrap()
        .unwrap();
    (
        row.try_get_by_index(0).unwrap(),
        row.try_get_by_index(1).unwrap(),
    )
}

async fn count(db: &DatabaseConnection, sql: &str) -> i64 {
    db.query_one_raw(Statement::from_string(db.get_database_backend(), sql))
        .await
        .unwrap()
        .unwrap()
        .try_get_by_index(0)
        .unwrap()
}

fn review(program: &Program, verdict: &str, url: Option<&str>) -> serde_json::Value {
    let mut v = serde_json::json!({
        "program_id": program.id,
        "dead_url": program.main_url,
        "verdict": verdict,
    });
    if let Some(url) = url {
        v["url"] = url.into();
    }
    v
}

fn file(reviews: Vec<serde_json::Value>) -> ReviewFile {
    parse(&serde_json::json!({ "reviewed_at": "2026-10-28", "reviews": reviews }).to_string())
        .expect("読める")
}

const HIT: &str = "https://www.city.example.jp/kosodate/new.html";
const LIST: &str = "https://www.city.example.jp/kosodate/";

const OLD_LIB: &str = "https://www.library.city.kita.tokyo.jp/viewer/info.html?id=8";
const NEW_LIB: &str = "https://www.library.city.kita.lg.jp/viewer/info.html?id=8";

#[test]
fn parse_accepts_a_host_move_with_the_same_path() {
    let json = serde_json::json!({
        "reviewed_at": "2026-11-08",
        "reviews": [
            { "program_id": "00000000-0000-0000-0000-000000000001", "dead_url": OLD_LIB,
              "verdict": "hit", "url": NEW_LIB, "host_move": true },
        ]
    });
    let file = parse(&json.to_string()).unwrap();
    assert!(file.reviews[0].host_move);
}

#[test]
fn parse_rejects_bad_host_moves() {
    let ids = [
        "00000000-0000-0000-0000-000000000011",
        "00000000-0000-0000-0000-000000000012",
        "00000000-0000-0000-0000-000000000013",
        "00000000-0000-0000-0000-000000000014",
    ];
    let json = serde_json::json!({
        "reviewed_at": "2026-11-08",
        "reviews": [
            // クエリが違う
            { "program_id": ids[0], "dead_url": OLD_LIB, "verdict": "hit",
              "url": "https://www.library.city.kita.lg.jp/viewer/info.html?id=9", "host_move": true },
            // パスが違う
            { "program_id": ids[1], "dead_url": OLD_LIB, "verdict": "hit",
              "url": "https://www.library.city.kita.lg.jp/0003/info-0000000008-0.html", "host_move": true },
            // hit でない
            { "program_id": ids[2], "dead_url": OLD_LIB, "verdict": "index",
              "url": NEW_LIB, "host_move": true },
            // ホストが同じ
            { "program_id": ids[3], "dead_url": OLD_LIB, "verdict": "hit",
              "url": "https://www.library.city.kita.tokyo.jp/viewer/info.html?id=56", "host_move": true },
        ]
    });
    let err = parse(&json.to_string()).unwrap_err().to_string();
    for id in ids {
        assert!(err.contains(id), "{id} が無い: {err}");
    }
    assert!(err.contains("ホスト移行"), "{err}");
}

#[test]
fn parse_rejects_malformed() {
    let ids = [
        "00000000-0000-0000-0000-000000000001",
        "00000000-0000-0000-0000-000000000002",
        "00000000-0000-0000-0000-000000000003",
        "00000000-0000-0000-0000-000000000004",
    ];
    let json = serde_json::json!({
        "reviewed_at": "2026-10-28",
        "reviews": [
            { "program_id": ids[0], "dead_url": LIST, "verdict": "hit" },
            { "program_id": ids[1], "dead_url": LIST, "verdict": "none", "url": HIT },
            { "program_id": ids[2], "dead_url": LIST, "verdict": "index", "url": "ftp://example.jp/" },
            { "program_id": ids[3], "dead_url": LIST, "verdict": "none" },
            { "program_id": ids[3], "dead_url": LIST, "verdict": "none" },
        ]
    });
    let err = parse(&json.to_string()).unwrap_err().to_string();
    for id in ids {
        assert!(err.contains(id), "{id} が無い: {err}");
    }
}

#[test]
fn parse_accepts_spec_example() {
    let json = r#"{
      "reviewed_at": "2026-10-28",
      "reviews": [
        { "program_id": "00000000-0000-0000-0000-000000000001",
          "dead_url": "https://www.city.chofu.lg.jp/050010/p027009.html",
          "verdict": "hit", "url": "https://www.city.chofu.lg.jp/050060/p027009.html" },
        { "program_id": "00000000-0000-0000-0000-000000000002",
          "dead_url": "https://www.city.chofu.lg.jp/a.html", "verdict": "index",
          "url": "https://www.city.chofu.lg.jp/list/" },
        { "program_id": "00000000-0000-0000-0000-000000000003",
          "dead_url": "https://www.city.chofu.lg.jp/b.html", "verdict": "none" }
      ]
    }"#;
    let file = parse(json).unwrap();
    let verdicts: Vec<Verdict> = file.reviews.iter().map(|r| r.verdict).collect();
    assert_eq!(verdicts, [Verdict::Hit, Verdict::Index, Verdict::None]);
}

/// 制度の主たる URL を北区立図書館の旧ホストの URL に付け替え、その url_id を返す（資源は無い）
async fn main_on_old_library(db: &DatabaseConnection, program: &Program) -> Uuid {
    let url_id = common::register(db, OLD_LIB).await;
    db.execute_raw(Statement::from_sql_and_values(
        db.get_database_backend(),
        "update program_urls set url_id = $1 where program_id = $2 and role = 'main'",
        [url_id.into(), program.id.into()],
    ))
    .await
    .unwrap();
    url_id
}

fn host_move_review(program: &Program) -> serde_json::Value {
    serde_json::json!({
        "program_id": program.id,
        "dead_url": OLD_LIB,
        "verdict": "hit",
        "url": NEW_LIB,
        "host_move": true,
    })
}

#[tokio::test]
async fn register_approves_the_host_move() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };
    let programs = setup(&db).await;
    main_on_old_library(&db, &programs[0]).await;
    let counts = register(&db, &file(vec![host_move_review(&programs[0])]), false)
        .await
        .unwrap();
    assert_eq!((counts.new_urls, counts.approved_moves), (1, 1));
    assert_eq!(
        count(
            &db,
            &format!(
                "select count(*) from host_moves where from_host_key = 'https://www.library.city.kita.tokyo.jp' \
                 and to_host_key = 'https://www.library.city.kita.lg.jp' and status = 'approved' \
                 and sample_url = '{OLD_LIB}'"
            )
        )
        .await,
        1
    );
    assert!(
        pipeline::crawl::allowed_hosts(&db)
            .await
            .unwrap()
            .contains("https://www.library.city.kita.lg.jp")
    );
}

#[tokio::test]
async fn register_dry_run_does_not_approve() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };
    let programs = setup(&db).await;
    main_on_old_library(&db, &programs[0]).await;
    let counts = register(&db, &file(vec![host_move_review(&programs[0])]), true)
        .await
        .unwrap();
    assert_eq!(counts.approved_moves, 1);
    assert_eq!(count(&db, "select count(*) from host_moves").await, 0);
    assert_eq!(
        count(
            &db,
            &format!("select count(*) from urls where normalized_url = '{NEW_LIB}'")
        )
        .await,
        0
    );
}

#[tokio::test]
async fn register_inserts_candidates_once() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };
    let programs = setup(&db).await;
    let f = file(vec![
        review(&programs[0], "hit", Some(HIT)),
        review(&programs[1], "index", Some(LIST)),
    ]);
    let first = register(&db, &f, false).await.unwrap();
    assert_eq!(
        first,
        RegisterCounts {
            new_urls: 2,
            existing_urls: 0,
            none: 0,
            approved_moves: 0
        }
    );
    let second = register(&db, &f, false).await.unwrap();
    assert_eq!((second.new_urls, second.existing_urls), (0, 2));
    assert_eq!(
        count(
            &db,
            &format!("select priority::bigint from urls where normalized_url = '{HIT}'")
        )
        .await,
        80
    );
    assert_eq!(
        count(
            &db,
            &format!("select priority::bigint from urls where normalized_url = '{LIST}'")
        )
        .await,
        70
    );
}

#[tokio::test]
async fn register_reuses_url_by_dedup_key() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };
    let programs = setup(&db).await;
    common::register(&db, LIST).await;
    let before = count(&db, "select count(*) from urls").await;
    let f = file(vec![review(
        &programs[0],
        "index",
        Some(" https://www.city.example.jp/kosodate/#top "),
    )]);
    let counts = register(&db, &f, false).await.unwrap();
    assert_eq!((counts.new_urls, counts.existing_urls), (0, 1));
    assert_eq!(count(&db, "select count(*) from urls").await, before);
}

#[tokio::test]
async fn register_dry_run_writes_nothing() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };
    let programs = setup(&db).await;
    let before = count(&db, "select count(*) from urls").await;
    let f = file(vec![review(&programs[0], "hit", Some(HIT))]);
    let counts = register(&db, &f, true).await.unwrap();
    assert_eq!(counts.new_urls, 1);
    assert_eq!(count(&db, "select count(*) from urls").await, before);
}

#[tokio::test]
async fn hit_replaces_main_url() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };
    let programs = setup(&db).await;
    let old = dead_main(&db, &programs[0], "deletion_candidate").await;
    candidate(&db, HIT, "active").await;
    let report = link(
        &db,
        &file(vec![review(&programs[0], "hit", Some(HIT))]),
        false,
    )
    .await
    .unwrap();
    assert_eq!(report.replaced, 1);
    assert!(report.refused.is_empty() && report.failed.is_empty());
    assert_eq!(
        main_of(&db, programs[0].id).await,
        (HIT.to_string(), "discovered".to_string())
    );
    // 資源の状態は変えない（元の URL を共有する制度を巻き込まないため）
    assert_eq!(
        state_of(&db, old).await,
        ("deletion_candidate".to_string(), None)
    );
}

#[tokio::test]
async fn hit_twice_is_already() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };
    let programs = setup(&db).await;
    dead_main(&db, &programs[0], "deletion_candidate").await;
    candidate(&db, HIT, "active").await;
    let f = file(vec![review(&programs[0], "hit", Some(HIT))]);
    link(&db, &f, false).await.unwrap();
    let second = link(&db, &f, false).await.unwrap();
    assert_eq!((second.replaced, second.already), (0, 1));
    assert!(second.refused.is_empty());
}

#[tokio::test]
async fn hit_on_a_related_url_of_the_same_program() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };
    let programs = setup(&db).await;
    dead_main(&db, &programs[0], "deleted").await;
    candidate(&db, HIT, "active").await;
    db.execute_raw(Statement::from_sql_and_values(
        db.get_database_backend(),
        "insert into program_urls (program_id, url_id, role, rank) \
         select $1, id, 'related', 1 from urls where normalized_url = $2",
        [programs[0].id.into(), HIT.into()],
    ))
    .await
    .unwrap();
    let report = link(
        &db,
        &file(vec![review(&programs[0], "hit", Some(HIT))]),
        false,
    )
    .await
    .unwrap();
    assert_eq!(report.replaced, 1, "{report:?}");
    assert_eq!(main_of(&db, programs[0].id).await.0, HIT);
    assert_eq!(
        count(
            &db,
            &format!(
                "select count(*) from program_urls where program_id = '{}' and role = 'related'",
                programs[0].id
            )
        )
        .await,
        0
    );
}

#[tokio::test]
async fn index_adds_discovered_related() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };
    let programs = setup(&db).await;
    dead_main(&db, &programs[0], "deleted").await;
    // 既にある関連リンク（rank 1）
    let existing = common::register(&db, "https://www.city.example.jp/other.html").await;
    db.execute_raw(Statement::from_sql_and_values(
        db.get_database_backend(),
        "insert into program_urls (program_id, url_id, role, rank) values ($1, $2, 'related', 1)",
        [programs[0].id.into(), existing.into()],
    ))
    .await
    .unwrap();
    candidate(&db, LIST, "active").await;
    let report = link(
        &db,
        &file(vec![review(&programs[0], "index", Some(LIST))]),
        false,
    )
    .await
    .unwrap();
    assert_eq!(report.related, 1);
    assert_eq!(
        count(
            &db,
            &format!(
                "select count(*) from program_urls pu join urls u on u.id = pu.url_id \
                 where pu.program_id = '{}' and u.normalized_url = '{LIST}' \
                 and pu.role = 'related' and pu.rank = 2 and pu.source = 'discovered'",
                programs[0].id
            )
        )
        .await,
        1
    );
}

#[tokio::test]
async fn none_changes_nothing() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };
    let programs = setup(&db).await;
    let old = dead_main(&db, &programs[0], "deleted").await;
    let before = count(&db, "select count(*) from program_urls").await;
    let report = link(&db, &file(vec![review(&programs[0], "none", None)]), false)
        .await
        .unwrap();
    assert_eq!(
        report,
        LinkReport {
            none: 1,
            ..LinkReport::default()
        }
    );
    assert_eq!(
        count(&db, "select count(*) from program_urls").await,
        before
    );
    assert_eq!(state_of(&db, old).await, ("deleted".to_string(), None));
}

/// 1件だけの判定を流し、結ばなかった理由を返す
async fn refusal(db: &DatabaseConnection, f: &ReviewFile) -> RefuseReason {
    let report = link(db, f, false).await.unwrap();
    assert_eq!(report.refused.len(), 1, "{report:?}");
    report.refused[0].reason.clone()
}

#[tokio::test]
async fn candidate_not_active_is_refused() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };
    let programs = setup(&db).await;
    dead_main(&db, &programs[0], "deleted").await;
    let f = file(vec![review(&programs[0], "hit", Some(HIT))]);
    assert_eq!(refusal(&db, &f).await, RefuseReason::NoResource);
    candidate(&db, HIT, "deletion_candidate").await;
    assert_eq!(
        refusal(&db, &f).await,
        RefuseReason::NotActive("deletion_candidate".to_string())
    );
}

#[tokio::test]
async fn main_changed_is_refused() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };
    let programs = setup(&db).await;
    dead_main(&db, &programs[0], "deleted").await;
    candidate(&db, HIT, "active").await;
    let mut r = review(&programs[0], "hit", Some(HIT));
    r["dead_url"] = "https://www.city.example.jp/someone-else.html".into();
    assert_eq!(
        refusal(&db, &file(vec![r])).await,
        RefuseReason::MainChanged
    );
}

#[tokio::test]
async fn main_revived_is_refused() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };
    let programs = setup(&db).await;
    let old = dead_main(&db, &programs[0], "active").await;
    candidate(&db, HIT, "active").await;
    let f = file(vec![review(&programs[0], "hit", Some(HIT))]);
    assert_eq!(
        refusal(&db, &f).await,
        RefuseReason::MainNotDead("active".to_string())
    );
    assert_eq!(state_of(&db, old).await, ("active".to_string(), None));
}

/// 3つの制度の主たる URL を、1つ目と同じ URL にする（レジストリで URL を共有する制度）
async fn share_main_url(db: &DatabaseConnection, programs: &[Program]) {
    for program in &programs[1..] {
        db.execute_raw(Statement::from_sql_and_values(
            db.get_database_backend(),
            "update program_urls set url_id = $1 where program_id = $2 and role = 'main'",
            [programs[0].main_url_id.into(), program.id.into()],
        ))
        .await
        .unwrap();
    }
}

fn shared_review(
    program: &Program,
    first: &Program,
    verdict: &str,
    url: Option<&str>,
) -> serde_json::Value {
    let mut r = review(program, verdict, url);
    r["dead_url"] = first.main_url.clone().into();
    r
}

#[tokio::test]
async fn hits_on_a_shared_url_change_only_each_program() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };
    let programs = setup(&db).await;
    share_main_url(&db, &programs).await;
    let old = dead_main(&db, &programs[0], "deleted").await;
    candidate(&db, HIT, "active").await;
    candidate(&db, LIST, "active").await;
    // 1つの古いページに載っていた制度の移転先が分かれる。3つ目は「どれも違う」
    let report = link(
        &db,
        &file(vec![
            shared_review(&programs[0], &programs[0], "hit", Some(HIT)),
            shared_review(&programs[1], &programs[0], "hit", Some(LIST)),
            shared_review(&programs[2], &programs[0], "none", None),
        ]),
        false,
    )
    .await
    .unwrap();
    assert_eq!((report.replaced, report.none), (2, 1), "{report:?}");
    assert_eq!(main_of(&db, programs[0].id).await.0, HIT);
    assert_eq!(main_of(&db, programs[1].id).await.0, LIST);
    assert_eq!(main_of(&db, programs[2].id).await.0, programs[0].main_url);
    assert_eq!(state_of(&db, old).await, ("deleted".to_string(), None));
}

#[tokio::test]
async fn target_moved_is_refused() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };
    let programs = setup(&db).await;
    dead_main(&db, &programs[0], "deleted").await;
    candidate(&db, HIT, "moved").await;
    let f = file(vec![review(&programs[0], "hit", Some(HIT))]);
    assert_eq!(refusal(&db, &f).await, RefuseReason::TargetMoved);
}

#[tokio::test]
async fn same_resource_is_refused() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };
    let programs = setup(&db).await;
    let old = dead_main(&db, &programs[0], "deleted").await;
    let url_id = common::register(&db, HIT).await;
    attach(&db, url_id, old).await;
    let f = file(vec![review(&programs[0], "hit", Some(HIT))]);
    // 候補の資源が元の資源と同じ（いまは deleted）なので、active でないより先に同じ資源で止める
    assert_eq!(refusal(&db, &f).await, RefuseReason::SameResource);
}

#[tokio::test]
async fn link_dry_run_writes_nothing() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };
    let programs = setup(&db).await;
    dead_main(&db, &programs[0], "deletion_candidate").await;
    candidate(&db, HIT, "active").await;
    let report = link(
        &db,
        &file(vec![review(&programs[0], "hit", Some(HIT))]),
        true,
    )
    .await
    .unwrap();
    assert_eq!(report.replaced, 1);
    assert_eq!(
        main_of(&db, programs[0].id).await,
        (programs[0].main_url.clone(), "registry".to_string())
    );
}

#[test]
fn parse_rejects_candidate_on_another_host() {
    let id = "00000000-0000-0000-0000-000000000009";
    let json = serde_json::json!({
        "reviewed_at": "2026-10-28",
        "reviews": [
            { "program_id": id, "dead_url": "https://www.city.example.jp/old.html",
              "verdict": "hit", "url": "https://www.town.example.jp/new.html" },
        ]
    });
    let err = parse(&json.to_string()).unwrap_err().to_string();
    assert!(err.contains(id) && err.contains("ホスト"), "{err}");
}

#[tokio::test]
async fn dry_run_counts_match_the_real_run_on_a_shared_url() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };
    let programs = setup(&db).await;
    share_main_url(&db, &programs).await;
    dead_main(&db, &programs[0], "deleted").await;
    candidate(&db, HIT, "active").await;
    candidate(&db, LIST, "active").await;
    let f = file(vec![
        shared_review(&programs[0], &programs[0], "hit", Some(HIT)),
        shared_review(&programs[1], &programs[0], "hit", Some(LIST)),
    ]);
    let report = link(&db, &f, true).await.unwrap();
    assert_eq!(report.replaced, 2, "{report:?}");
    assert_eq!(main_of(&db, programs[0].id).await.0, programs[0].main_url);
    assert_eq!(main_of(&db, programs[1].id).await.0, programs[0].main_url);
}

#[tokio::test]
async fn hit_on_the_dead_url_itself_is_refused() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };
    let programs = setup(&db).await;
    dead_main(&db, &programs[0], "deleted").await;
    let dead = programs[0].main_url.clone();
    let f = file(vec![review(&programs[0], "hit", Some(&dead))]);
    assert_eq!(refusal(&db, &f).await, RefuseReason::SameResource);
}

#[tokio::test]
async fn rerun_with_a_different_candidate_is_refused() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };
    let programs = setup(&db).await;
    dead_main(&db, &programs[0], "deleted").await;
    candidate(&db, HIT, "active").await;
    candidate(&db, LIST, "active").await;
    link(
        &db,
        &file(vec![review(&programs[0], "hit", Some(HIT))]),
        false,
    )
    .await
    .unwrap();
    let f = file(vec![review(&programs[0], "hit", Some(LIST))]);
    assert_eq!(refusal(&db, &f).await, RefuseReason::MainChanged);
    assert_eq!(main_of(&db, programs[0].id).await.0, HIT);
}

#[tokio::test]
async fn replaced_main_survives_import_and_can_be_undone() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };
    let programs = setup(&db).await;
    dead_main(&db, &programs[0], "deleted").await;
    candidate(&db, HIT, "active").await;
    link(
        &db,
        &file(vec![review(&programs[0], "hit", Some(HIT))]),
        false,
    )
    .await
    .unwrap();
    import(&db, domain::registry::parse(FIXTURE).unwrap())
        .await
        .expect("取り込める");
    assert_eq!(
        main_of(&db, programs[0].id).await,
        (HIT.to_string(), "discovered".to_string())
    );
    // 誤った判定の戻し方（docs/apply-reviews.md）
    db.execute_raw(Statement::from_sql_and_values(
        db.get_database_backend(),
        "update program_urls set source = 'registry' where program_id = $1 and role = 'main'",
        [programs[0].id.into()],
    ))
    .await
    .unwrap();
    import(&db, domain::registry::parse(FIXTURE).unwrap())
        .await
        .expect("取り込める");
    assert_eq!(
        main_of(&db, programs[0].id).await,
        (programs[0].main_url.clone(), "registry".to_string())
    );
}

/// ホスト移行の判定の前提: 主たる URL を旧ホストに付け替え、register で承認し、新ホストの URL に active の資源を付ける。
/// 元の URL の url_id を返す
async fn host_move_ready(db: &DatabaseConnection, programs: &[Program]) -> Uuid {
    let old = main_on_old_library(db, &programs[0]).await;
    register(db, &file(vec![host_move_review(&programs[0])]), false)
        .await
        .unwrap();
    let new = query_uuid(
        db,
        "select id from urls where normalized_url = $1",
        vec![NEW_LIB.into()],
    )
    .await;
    let resource_id = resource(db, "active").await;
    attach(db, new, resource_id).await;
    old
}

async fn status_of(db: &DatabaseConnection, url_id: Uuid) -> String {
    db.query_one_raw(Statement::from_sql_and_values(
        db.get_database_backend(),
        "select status from urls where id = $1",
        [url_id.into()],
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get_by_index(0)
    .unwrap()
}

#[tokio::test]
async fn host_move_replaces_a_main_url_that_never_had_a_resource() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };
    let programs = setup(&db).await;
    let old = host_move_ready(&db, &programs).await;
    let report = link(&db, &file(vec![host_move_review(&programs[0])]), false)
        .await
        .unwrap();
    assert_eq!((report.replaced, report.retired), (1, 1));
    assert!(report.refused.is_empty() && report.failed.is_empty());
    assert_eq!(
        main_of(&db, programs[0].id).await,
        (NEW_LIB.to_string(), "discovered".to_string())
    );
    assert_eq!(status_of(&db, old).await, "retired");
}

#[tokio::test]
async fn host_move_without_approval_is_refused() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };
    let programs = setup(&db).await;
    main_on_old_library(&db, &programs[0]).await;
    // register を流さずに、候補だけ取れている
    candidate(&db, NEW_LIB, "active").await;
    let f = file(vec![host_move_review(&programs[0])]);
    assert_eq!(refusal(&db, &f).await, RefuseReason::HostMoveNotApproved);
    assert_eq!(
        main_of(&db, programs[0].id).await,
        (OLD_LIB.to_string(), "registry".to_string())
    );
}

#[tokio::test]
async fn host_move_refused_when_old_main_came_back() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };
    let programs = setup(&db).await;
    let old = host_move_ready(&db, &programs).await;
    let revived = resource(&db, "active").await;
    attach(&db, old, revived).await;
    let report = link(&db, &file(vec![host_move_review(&programs[0])]), false)
        .await
        .unwrap();
    assert_eq!(report.retired, 0);
    assert_eq!(
        report
            .refused
            .iter()
            .map(|r| r.reason.clone())
            .collect::<Vec<_>>(),
        vec![RefuseReason::MainNotDead("active".to_string())]
    );
    assert_ne!(status_of(&db, old).await, "retired");
}

#[tokio::test]
async fn host_move_keeps_crawling_a_url_still_used_elsewhere() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };
    let programs = setup(&db).await;
    let old = host_move_ready(&db, &programs).await;
    db.execute_raw(Statement::from_sql_and_values(
        db.get_database_backend(),
        "insert into program_urls (program_id, url_id, role, rank, source) \
         values ($1, $2, 'related', 1, 'registry')",
        [programs[1].id.into(), old.into()],
    ))
    .await
    .unwrap();
    let report = link(&db, &file(vec![host_move_review(&programs[0])]), false)
        .await
        .unwrap();
    assert_eq!((report.replaced, report.retired), (1, 0));
    assert_eq!(main_of(&db, programs[0].id).await.0, NEW_LIB);
    assert_ne!(status_of(&db, old).await, "retired");
}

#[tokio::test]
async fn host_move_twice_is_already() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };
    let programs = setup(&db).await;
    let old = host_move_ready(&db, &programs).await;
    let f = file(vec![host_move_review(&programs[0])]);
    link(&db, &f, false).await.unwrap();
    let second = link(&db, &f, false).await.unwrap();
    assert_eq!((second.already, second.replaced, second.retired), (1, 0, 0));
    assert!(second.refused.is_empty() && second.failed.is_empty());
    assert_eq!(status_of(&db, old).await, "retired");
}

#[tokio::test]
async fn host_move_link_dry_run_writes_nothing() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };
    let programs = setup(&db).await;
    let old = host_move_ready(&db, &programs).await;
    let before = status_of(&db, old).await;
    let report = link(&db, &file(vec![host_move_review(&programs[0])]), true)
        .await
        .unwrap();
    assert_eq!((report.replaced, report.retired), (1, 1));
    assert_eq!(
        main_of(&db, programs[0].id).await,
        (OLD_LIB.to_string(), "registry".to_string())
    );
    assert_eq!(status_of(&db, old).await, before);
}

#[tokio::test]
async fn retired_old_url_is_not_crawled() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };
    let programs = setup(&db).await;
    let old = host_move_ready(&db, &programs).await;
    link(&db, &file(vec![host_move_review(&programs[0])]), false)
        .await
        .unwrap();
    db.execute_raw(Statement::from_sql_and_values(
        db.get_database_backend(),
        "update urls set next_crawl_at = now() - interval '1 day' where id = $1",
        [old.into()],
    ))
    .await
    .unwrap();
    let candidates = claim::candidates(&db, &Exclude::default(), std::time::Duration::ZERO)
        .await
        .unwrap();
    assert!(candidates.iter().all(|c| c.url_id != old));
}

#[test]
fn the_kita_library_review_file_parses() {
    let file = parse(include_str!(
        "../../../data/reviews/2026-11-kita-library.json"
    ))
    .unwrap();
    assert_eq!(file.reviews.len(), 1);
    let review = &file.reviews[0];
    assert_eq!(review.verdict, Verdict::Hit);
    assert!(review.host_move);
    assert_eq!(
        review.program_id.to_string(),
        "80b693e7-42f4-439f-a6bc-c4c8dde26208"
    );
}
