use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// 再訪の間隔（秒）。成功と blocked のたびに伸び縮みし、失敗では変えない。NULL はまだ決めていない。
const UP: &str = r#"
alter table urls add column recrawl_interval_secs integer check (recrawl_interval_secs > 0);
"#;

const DOWN: &str = r#"
alter table urls drop column recrawl_interval_secs;
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
