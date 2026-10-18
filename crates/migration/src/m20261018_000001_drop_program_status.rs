use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// 制度は状態を持たず、API は主たる URL の資源の事実を返す。2列は誰も書いておらず、中身は既定値だけ。
const UP: &str = r#"
alter table programs drop column status, drop column checked_at;
"#;

const DOWN: &str = r#"
alter table programs
    add column status text not null default 'registry_only'
        check (status in ('registry_only', 'fresh', 'changed', 'needs_review', 'gone')),
    add column checked_at timestamptz;
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
