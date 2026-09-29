use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// 許可リストの外へ、ホストだけが変わって動いた URL の記録。承認されたものだけが許可リストに入る。
const UP: &str = r#"
create table host_moves (
    from_host_key text not null,
    to_host_key   text not null,
    status        text not null default 'proposed' check (status in ('proposed', 'approved')),
    sample_url    text not null,
    first_seen_at timestamptz not null default now(),
    last_seen_at  timestamptz not null default now(),
    primary key (from_host_key, to_host_key),
    check (from_host_key <> to_host_key)
);
"#;

const DOWN: &str = "drop table host_moves;";

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
