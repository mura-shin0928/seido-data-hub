use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// 一覧ページ（role = index の URL で取った資源）の本文コンテナの中の、同じホストへのリンク。
// 移転先の候補を作るのに使う。links_hash が変わったときだけ、資源ごとに入れ替える。
// ページの本文は残さず、URL・dedup_key・host_key・リンク文字列だけを持つ。
const UP: &str = r#"
create table page_links (
    resource_id uuid not null references resources (id) on delete cascade,
    url         text not null,
    dedup_key   text not null,
    host_key    text not null,
    text        text not null default '',
    primary key (resource_id, url, text)
);
create index page_links_host_key_idx on page_links (host_key);
"#;

const DOWN: &str = r#"
drop table page_links;
"#;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.get_connection().execute_unprepared(UP).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.get_connection().execute_unprepared(DOWN).await?;
        Ok(())
    }
}
