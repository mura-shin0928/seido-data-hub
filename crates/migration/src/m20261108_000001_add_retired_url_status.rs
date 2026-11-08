use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// ホスト移行で主たる URL を差し替えて、どの制度からも使われなくなった URL を巡回から外す（retired）。
// 行と取得の履歴は残す。巡回の候補にも待ち行列の数にも入れない。
const UP: &str = r#"
alter table urls drop constraint urls_status_check;
alter table urls add constraint urls_status_check
    check (status in ('queued', 'processing', 'succeeded', 'retry_wait', 'failed_final', 'blocked', 'retired'));
"#;

const DOWN: &str = r#"
update urls set status = 'failed_final' where status = 'retired';
alter table urls drop constraint urls_status_check;
alter table urls add constraint urls_status_check
    check (status in ('queued', 'processing', 'succeeded', 'retry_wait', 'failed_final', 'blocked'));
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
