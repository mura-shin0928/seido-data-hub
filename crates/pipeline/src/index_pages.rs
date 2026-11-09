//! 一覧ページの起点を登録する。
//!
//! 起点は、すべての制度の主たる URL（`program_urls.role = 'main'`）の親ディレクトリ。主たる URL の資源が
//! 死んでいても、親ディレクトリは起点に入れる。主たる URL が1件も無いときは、何も書かずにエラーにする（取り込みの失敗や接続先の取り違えで、一覧ページを全部止めないため）。`index.html` を足して試さず、深さも0のままにする
//! （測定: 960件を試して HTML が返ったのは53件。深さ1まで広げると +5 ポイントのために11ホストへ +550 件の要求）。
//!
//! 起点から外れた一覧ページは `retired` にして止める。ただし制度の URL（`program_urls`）に使われている行は止めない。

use std::collections::BTreeMap;

use anyhow::Context;
use domain::moves::parent_dir;
use domain::urls::{PreparedUrl, prepare};
use entity::urls as urls_table;
use sea_orm::sea_query::{Expr, OnConflict};
use sea_orm::{
    ActiveValue::Set, ColumnTrait, ConnectionTrait, DatabaseConnection, DbBackend, EntityTrait,
    PaginatorTrait, QueryFilter, Statement, TransactionTrait,
};

/// Postgres のバインド変数の上限（65535）に収まるよう、まとめて書くときは分ける。
const CHUNK: usize = 500;

/// 起点の優先度。主たる URL（80）より先に取る
pub const INDEX_PRIORITY: i32 = 90;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// 重複を畳んだ起点の数
    pub starts: usize,
    /// 起点のうち、今回 urls に入れたもの・既にあったもの
    pub new_urls: usize,
    pub existing_urls: usize,
    /// 起点から外れて取らないようにした一覧ページ
    pub parked: u64,
    /// 起点に戻って取るようにした一覧ページ
    pub restored: u64,
}

const MAIN_URLS: &str = "SELECT DISTINCT u.normalized_url FROM program_urls pu \
    JOIN urls u ON u.id = pu.url_id WHERE pu.role = 'main'";

/// 起点を登録し、起点から外れた一覧ページを止め、戻ったものを再開する。
/// 1つのトランザクションで、何度流しても同じ結果になる
pub async fn register(db: &DatabaseConnection) -> anyhow::Result<Counts> {
    let txn = db.begin().await?;

    let rows = txn
        .query_all_raw(Statement::from_string(DbBackend::Postgres, MAIN_URLS))
        .await
        .context("主たる URL を読めない")?;
    if rows.is_empty() {
        anyhow::bail!(
            "主たる URL が1件も無いので、起点の登録も一覧ページの停止もしていない（DATABASE_URL と取り込みを確かめる）"
        );
    }
    // 親ディレクトリを読めない・登録できないものは起点にしない。表記違いは最初に見たものを残す
    let mut starts: BTreeMap<String, PreparedUrl> = BTreeMap::new();
    for row in rows {
        let normalized: String = row.try_get("", "normalized_url")?;
        let Some(dir) = parent_dir(&normalized) else {
            continue;
        };
        let Ok(prepared) = prepare(&dir) else {
            continue;
        };
        starts.entry(prepared.dedup_key.clone()).or_insert(prepared);
    }
    let mut counts = Counts {
        starts: starts.len(),
        ..Counts::default()
    };

    // 既にある行は触らない。巡回で進んだ状態（status・next_crawl_at・lease）や seed の role を戻さないため
    let before = urls_table::Entity::find().count(&txn).await? as usize;
    let models: Vec<_> = starts.values().map(url_model).collect();
    for chunk in models.chunks(CHUNK) {
        urls_table::Entity::insert_many(chunk.to_vec())
            .on_conflict(
                OnConflict::column(urls_table::Column::DedupKey)
                    .do_nothing()
                    .to_owned(),
            )
            .exec_without_returning(&txn)
            .await
            .context("urls を書けない")?;
    }
    let after = urls_table::Entity::find().count(&txn).await? as usize;
    counts.new_urls = after - before;
    counts.existing_urls = starts.len() - counts.new_urls;

    // 一覧ページ（role = 'index'）だけを動かす。seed は host_moves が決めた状態を残す
    let keys: Vec<&str> = starts.keys().map(String::as_str).collect();
    let index = urls_table::Column::Role.eq("index");
    counts.parked = park(&txn, &keys).await?;
    for chunk in keys.chunks(CHUNK) {
        counts.restored += urls_table::Entity::update_many()
            .col_expr(urls_table::Column::Status, Expr::value("queued"))
            .col_expr(urls_table::Column::NextCrawlAt, Expr::current_timestamp())
            .col_expr(urls_table::Column::UpdatedAt, Expr::current_timestamp())
            .filter(index.clone())
            .filter(urls_table::Column::Status.eq("retired"))
            .filter(urls_table::Column::DedupKey.is_in(chunk.iter().copied()))
            .exec(&txn)
            .await
            .context("一覧ページを戻せない")?
            .rows_affected;
    }

    txn.commit().await?;
    Ok(counts)
}

/// 起点に無い一覧ページを止める。起点を配列1つで渡すので、バインド変数の上限に当たらない
/// （起点が無ければ、一覧ページはすべて外れる）。
/// 制度の URL（`program_urls`）に使われている行は止めない。人の判定で制度の URL になった一覧ページを、
/// 巡回から外さないため（`apply-reviews link` は `seed` にするが、それより前に反映した行も守る）
async fn park(txn: &impl ConnectionTrait, keys: &[&str]) -> anyhow::Result<u64> {
    let keys: Vec<String> = keys.iter().map(|key| key.to_string()).collect();
    let result = txn
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE urls SET status = 'retired', updated_at = now() \
             WHERE role = 'index' AND status <> 'retired' AND dedup_key <> ALL($1) \
               AND NOT EXISTS (SELECT 1 FROM program_urls pu WHERE pu.url_id = urls.id)",
            [keys.into()],
        ))
        .await
        .context("一覧ページを止められない")?;
    Ok(result.rows_affected())
}

// status・next_crawl_at は DB の既定値（queued・now()）に任せる
fn url_model(url: &PreparedUrl) -> urls_table::ActiveModel {
    urls_table::ActiveModel {
        raw_url: Set(url.raw_url.clone()),
        normalized_url: Set(url.normalized_url.clone()),
        dedup_key: Set(url.dedup_key.clone()),
        host_key: Set(url.host_key.clone()),
        role: Set("index".to_string()),
        depth: Set(0),
        priority: Set(INDEX_PRIORITY),
        ..Default::default()
    }
}
