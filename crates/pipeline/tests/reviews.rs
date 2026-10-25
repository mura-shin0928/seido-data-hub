//! 人が判定した移転先を、実際の Postgres に結ぶ。
//!
//! `TEST_DATABASE_URL` のデータベースは毎回作り直す（本番の接続先を入れないこと）。未設定ならスキップする。
//! 候補の URL の資源は巡回を流さずに SQL で作る。

mod common;

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
            none: 0
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
async fn hit_moves_dead_resource() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };
    let programs = setup(&db).await;
    let old = dead_main(&db, &programs[0], "deletion_candidate").await;
    let new = candidate(&db, HIT, "active").await;
    let report = link(
        &db,
        &file(vec![review(&programs[0], "hit", Some(HIT))]),
        false,
    )
    .await
    .unwrap();
    assert_eq!(report.moved, 1);
    assert!(report.refused.is_empty() && report.failed.is_empty());
    assert_eq!(state_of(&db, old).await, ("moved".to_string(), Some(new)));
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
    assert_eq!((second.moved, second.already), (0, 1));
    assert!(second.refused.is_empty());
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

#[tokio::test]
async fn second_hit_on_shared_resource_is_refused() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };
    let programs = setup(&db).await;
    // 2つ目の制度の主たる URL を、1つ目と同じ URL にする（レジストリで URL を共有する制度）
    db.execute_raw(Statement::from_sql_and_values(
        db.get_database_backend(),
        "update program_urls set url_id = $1 where program_id = $2 and role = 'main'",
        [programs[0].main_url_id.into(), programs[1].id.into()],
    ))
    .await
    .unwrap();
    let old = dead_main(&db, &programs[0], "deleted").await;
    let first = candidate(&db, HIT, "active").await;
    candidate(&db, LIST, "active").await;
    let mut second = review(&programs[1], "hit", Some(LIST));
    second["dead_url"] = programs[0].main_url.clone().into();
    let report = link(
        &db,
        &file(vec![review(&programs[0], "hit", Some(HIT)), second]),
        false,
    )
    .await
    .unwrap();
    assert_eq!(report.moved, 1);
    assert_eq!(
        report.refused[0].reason,
        RefuseReason::MainNotDead("moved".to_string())
    );
    assert_eq!(state_of(&db, old).await, ("moved".to_string(), Some(first)));
}

#[tokio::test]
async fn target_moved_or_cycle_is_refused() {
    let Some((db, _guard)) = common::fresh_db().await else {
        return;
    };
    let programs = setup(&db).await;
    let old = dead_main(&db, &programs[0], "deleted").await;
    let target = candidate(&db, HIT, "moved").await;
    let f = file(vec![review(&programs[0], "hit", Some(HIT))]);
    assert_eq!(refusal(&db, &f).await, RefuseReason::TargetMoved);
    db.execute_raw(Statement::from_sql_and_values(
        db.get_database_backend(),
        "update resources set state = 'active', moved_to_resource_id = $1 where id = $2",
        [old.into(), target.into()],
    ))
    .await
    .unwrap();
    assert_eq!(refusal(&db, &f).await, RefuseReason::Cycle);
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
    let old = dead_main(&db, &programs[0], "deletion_candidate").await;
    candidate(&db, HIT, "active").await;
    let report = link(
        &db,
        &file(vec![review(&programs[0], "hit", Some(HIT))]),
        true,
    )
    .await
    .unwrap();
    assert_eq!(report.moved, 1);
    assert_eq!(
        state_of(&db, old).await,
        ("deletion_candidate".to_string(), None)
    );
}
