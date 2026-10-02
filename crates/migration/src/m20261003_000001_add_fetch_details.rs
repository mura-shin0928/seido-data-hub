use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// 取得の履歴に、実データで数えるための判断の材料を足す。
// 応答の Content-Type、HTML の文字コード（名前・判定元・置き換えが起きたか）、資源に当てた観測。
const UP: &str = r#"
alter table fetch_history
    add column content_type     text,
    add column charset          text,
    add column charset_source   text check (charset_source in ('bom', 'header', 'meta', 'default')),
    add column charset_replaced boolean,
    add column observation      text check (observation in ('alive', 'not_found', 'soft_404_title',
                                                            'soft_404_known_title', 'top_redirect', 'gone'));
"#;

const DOWN: &str = r#"
alter table fetch_history
    drop column observation,
    drop column charset_replaced,
    drop column charset_source,
    drop column charset,
    drop column content_type;
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
