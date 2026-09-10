use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// スキーマは SQL で書く。PostgreSQL 固有の型（text[]・jsonb）と check 制約をそのまま読めるようにするため。
const UP: &str = r#"
create table areas (
    code        text primary key check (code ~ '^[0-9]{6}$'),
    name        text not null,
    parent_code text references areas (code)
);

create table programs (
    id             uuid primary key default gen_random_uuid(),
    psid           text not null unique,
    um             text not null check (um ~ '^UM[0-9]+$'),
    area_code      text not null references areas (code),
    canonical_name text not null,
    short_name     text,
    source_url     text not null,
    category_codes text[] not null default '{}',
    target_codes   text[] not null default '{}',
    content_codes  text[] not null default '{}',
    age_min_months integer check (age_min_months >= 0),
    age_max_months integer check (age_max_months > 0),
    registry       jsonb not null,
    status         text not null default 'registry_only'
                   check (status in ('registry_only', 'fresh', 'changed', 'needs_review', 'gone')),
    checked_at     timestamptz,
    imported_at    timestamptz not null default now()
);

create index programs_area_code_idx on programs (area_code);
create index programs_um_idx on programs (um);
"#;

const DOWN: &str = r#"
drop table programs;
drop table areas;
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
