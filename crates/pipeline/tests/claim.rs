//! 候補・claim・lease を実際の Postgres に流す。
//!
//! `TEST_DATABASE_URL` のデータベースは毎回作り直す（本番の接続先を入れないこと）。未設定ならスキップする。

mod common;

use std::collections::BTreeSet;
use std::time::Duration;

use common::*;
use domain::schedule::Policy;
use entity::{resources, url_resources, urls};
use pipeline::claim::{self, Exclude};
use pipeline::lifecycle::Context;
use pipeline::persist::{Attempt, Recorded, record};
use sea_orm::{
    ActiveValue::Set,
    ConnectionTrait, DatabaseConnection, EntityTrait, Statement,
    prelude::{DateTimeWithTimeZone, Uuid},
};

const LEASE: Duration = Duration::from_secs(60);

async fn candidate_ids(db: &DatabaseConnection, exclude: &Exclude) -> BTreeSet<Uuid> {
    claim::candidates(db, exclude)
        .await
        .unwrap()
        .into_iter()
        .map(|candidate| candidate.url_id)
        .collect()
}

async fn url_row(db: &DatabaseConnection, id: Uuid) -> urls::Model {
    urls::Entity::find_by_id(id).one(db).await.unwrap().unwrap()
}

async fn lease_until(db: &DatabaseConnection, id: Uuid) -> DateTimeWithTimeZone {
    url_row(db, id).await.lease_until.unwrap()
}

async fn expire_lease(db: &DatabaseConnection, id: Uuid) {
    db.execute_raw(Statement::from_sql_and_values(
        db.get_database_backend(),
        "update urls set lease_until = now() - interval '1 second' where id = $1",
        [id.into()],
    ))
    .await
    .unwrap();
}

/// URL を新しい資源に結び、その資源の id を返す
async fn link_to_new_resource(db: &DatabaseConnection, url_id: Uuid, canonical: &str) -> Uuid {
    let resource_id = resources::Entity::insert(resources::ActiveModel {
        canonical_url: Set(canonical.to_string()),
        final_url: Set(canonical.to_string()),
        canonical_source: Set("normalized".to_string()),
        ..Default::default()
    })
    .exec(db)
    .await
    .unwrap()
    .last_insert_id;
    url_resources::Entity::insert(url_resources::ActiveModel {
        url_id: Set(url_id),
        resource_id: Set(resource_id),
        relation: Set("direct".to_string()),
        ..Default::default()
    })
    .exec_without_returning(db)
    .await
    .unwrap();
    resource_id
}

#[tokio::test]
async fn the_highest_priority_url_of_a_host_comes_first() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let low = register(&db, &url("/low.html")).await;
    let high = register(&db, &url("/high.html")).await;
    let middle = register(&db, &url("/middle.html")).await;
    set_priority(&db, low, 30).await;
    set_priority(&db, high, 90).await;
    set_priority(&db, middle, 80).await;

    let candidates = claim::candidates(&db, &Exclude::default()).await.unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].url_id, high);
    assert_eq!(candidates[0].priority, 90);
    assert_eq!(candidates[0].host_key, url_row(&db, high).await.host_key);
}

#[tokio::test]
async fn a_host_with_a_live_lease_has_no_candidate() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let a = register(&db, &url("/a.html")).await;
    register(&db, &url("/b.html")).await;

    let claims = claim::claim(&db, &[a], "worker-1", LEASE).await.unwrap();
    assert_eq!(claims.len(), 1);
    assert!(
        claim::candidates(&db, &Exclude::default())
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn rows_whose_time_has_not_come_are_left_alone() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let succeeded = register(&db, &url("/a.html")).await;
    set_status(&db, succeeded, "succeeded", "now() + interval '1 day'").await;
    assert!(candidate_ids(&db, &Exclude::default()).await.is_empty());

    let blocked = register(&db, "https://www.blocked.example.jp/a.html").await;
    let failed = register(&db, "https://www.failed.example.jp/a.html").await;
    let retry = register(&db, "https://www.retry.example.jp/a.html").await;
    set_status(&db, blocked, "blocked", "now() - interval '1 second'").await;
    set_status(&db, failed, "failed_final", "now() - interval '1 second'").await;
    set_status(&db, retry, "retry_wait", "now() - interval '1 second'").await;

    assert_eq!(
        candidate_ids(&db, &Exclude::default()).await,
        BTreeSet::from([blocked, failed, retry])
    );
}

#[tokio::test]
async fn an_expired_lease_is_reclaimed_and_the_old_worker_is_refused() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let fetcher = serve(|_| page(None)).await;
    let id = register(&db, &url("/a.html")).await;

    let first = claim::claim(&db, &[id], "worker-1", LEASE).await.unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].claim_token, 1);
    assert_eq!(first[0].url, url_row(&db, id).await.normalized_url);
    assert_eq!(first[0].resource_id, None);

    expire_lease(&db, id).await;
    assert_eq!(
        candidate_ids(&db, &Exclude::default()).await,
        BTreeSet::from([id])
    );
    let second = claim::claim(&db, &[id], "worker-2", LEASE).await.unwrap();
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].claim_token, 2);
    let row = url_row(&db, id).await;
    assert_eq!(row.status, "processing");
    assert_eq!(row.worker_id.as_deref(), Some("worker-2"));

    let fetch = fetcher.fetch(&url("/a.html"), &Default::default()).await;
    let extracted = extracted_of(&fetch);
    let ctx = Context {
        host_trusted: true,
        known_not_found_titles: &[],
    };
    let run = start_run(&db).await;
    let policy = Policy::default();
    let attempt = |claim_token| Attempt {
        run_id: run,
        url_id: id,
        claim_token,
        fetch: &fetch,
        extracted: extracted.as_ref(),
        ctx: &ctx,
        policy: &policy,
    };
    assert!(matches!(
        record(&db, &attempt(1)).await.unwrap(),
        Recorded::StaleClaim
    ));
    assert!(matches!(
        record(&db, &attempt(2)).await.unwrap(),
        Recorded::Saved { .. }
    ));
}

#[tokio::test]
async fn two_claims_at_once_never_share_a_url() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let id = register(&db, &url("/a.html")).await;

    let ids = [id];
    let (one, two) = tokio::join!(
        claim::claim(&db, &ids, "worker-1", LEASE),
        claim::claim(&db, &ids, "worker-2", LEASE),
    );
    assert_eq!(one.unwrap().len() + two.unwrap().len(), 1);
    assert_eq!(url_row(&db, id).await.claim_token, 1);
}

#[tokio::test]
async fn extend_needs_the_current_token() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let id = register(&db, &url("/a.html")).await;
    let claims = claim::claim(&db, &[id], "worker-1", LEASE).await.unwrap();
    let token = claims[0].claim_token;
    let before = lease_until(&db, id).await;

    let longer = Duration::from_secs(600);
    assert_eq!(
        claim::extend(&db, &[(id, token - 1)], longer)
            .await
            .unwrap(),
        0
    );
    assert_eq!(lease_until(&db, id).await, before);

    assert_eq!(claim::extend(&db, &[(id, token)], longer).await.unwrap(), 1);
    assert!(lease_until(&db, id).await > before);
}

#[tokio::test]
async fn urls_of_an_excluded_resource_give_way_to_the_next_url_of_the_host() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let a = register(&db, &url("/a.html")).await;
    let b = register(&db, &url("/b.html")).await;
    set_priority(&db, a, 90).await;
    set_priority(&db, b, 80).await;
    let resource = link_to_new_resource(&db, a, &url("/a.html")).await;
    let exclude = Exclude {
        resources: BTreeSet::from([resource]),
        ..Default::default()
    };

    assert_eq!(
        candidate_ids(&db, &Exclude::default()).await,
        BTreeSet::from([a])
    );
    assert_eq!(candidate_ids(&db, &exclude).await, BTreeSet::from([b]));

    // 資源を取った URL は claim したときに分かる
    let claims = claim::claim(&db, &[a], "worker-1", LEASE).await.unwrap();
    assert_eq!(claims[0].resource_id, Some(resource));
    expire_lease(&db, a).await;

    set_status(&db, a, "retry_wait", "now() + interval '10 seconds'").await;
    let wait = claim::next_retry_in(&db, &Exclude::default())
        .await
        .unwrap()
        .expect("再試行を待つ行がある");
    assert!(wait > Duration::from_secs(8) && wait <= Duration::from_secs(10));
    assert_eq!(claim::next_retry_in(&db, &exclude).await.unwrap(), None);
}

#[tokio::test]
async fn an_excluded_host_has_no_candidate_and_no_retry() {
    let Some((db, _guard)) = fresh_db().await else {
        return;
    };
    let a = register(&db, &url("/a.html")).await;
    let other = register(&db, "https://www.other.example.jp/a.html").await;
    set_status(&db, other, "retry_wait", "now() + interval '10 seconds'").await;
    let exclude = Exclude {
        hosts: BTreeSet::from([url_row(&db, other).await.host_key]),
        ..Default::default()
    };

    assert_eq!(candidate_ids(&db, &exclude).await, BTreeSet::from([a]));
    assert!(
        claim::next_retry_in(&db, &Exclude::default())
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(claim::next_retry_in(&db, &exclude).await.unwrap(), None);
}
