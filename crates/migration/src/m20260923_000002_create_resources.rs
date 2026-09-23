use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// 資源（同じ内容を指す URL をまとめたもの）と、URL から資源への結び付き。
// 巡回で使う列は、いま値が入らないものも最初から作る（urls と同じ方針）。
const UP: &str = r#"
create table resources (
    id                     uuid primary key default gen_random_uuid(),
    canonical_url          text not null unique,
    final_url              text not null,
    declared_canonical_url text,
    canonical_source       text not null
                           check (canonical_source in ('redirect', 'declared', 'normalized')),
    state                  text not null default 'active'
                           check (state in ('active', 'deletion_candidate', 'deleted', 'moved')),
    etag                   text,
    last_modified          text,
    raw_hash               text,
    page_hash              text,
    title_hash             text,
    body_hash              text,
    links_hash             text,
    page_updated_on        date,
    extractor_version      integer,
    extractor_rule         text,
    robots_meta            text,
    last_crawled_at        timestamptz,
    last_changed_at        timestamptz,
    next_crawl_at          timestamptz,
    consecutive_not_found  integer not null default 0 check (consecutive_not_found >= 0),
    moved_to_resource_id   uuid references resources (id),
    change_count           integer not null default 0 check (change_count >= 0),
    created_at             timestamptz not null default now(),
    updated_at             timestamptz not null default now()
);

-- 結び付きは消さずに残す（URL の履歴）。URL のいまの資源は observed_at が最新の行
create table url_resources (
    url_id      uuid not null references urls (id),
    resource_id uuid not null references resources (id),
    relation    text not null check (relation in ('direct', 'redirect', 'declared', 'content_match')),
    observed_at timestamptz not null default now(),
    primary key (url_id, resource_id)
);

create index url_resources_resource_id_idx on url_resources (resource_id);
create index url_resources_latest_idx on url_resources (url_id, observed_at desc);
"#;

const DOWN: &str = r#"
drop table url_resources;
drop table resources;
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
