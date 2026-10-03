use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// 実行の集計を残す列。config は実行を作るとき、stats・alerts は正常に終えたときに書く。
const UP: &str = r#"
alter table crawl_runs
    add column config jsonb,  -- 実行の設定値のスナップショット（実行を作るとき）
    add column stats  jsonb,  -- 集計（正常に終えたとき）
    add column alerts jsonb;  -- 警告の一覧（正常に終えたとき）
"#;

const DOWN: &str = r#"
alter table crawl_runs
    drop column alerts,
    drop column stats,
    drop column config;
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
