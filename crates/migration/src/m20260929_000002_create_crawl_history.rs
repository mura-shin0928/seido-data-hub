use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// 実行の単位と、取得の履歴（追記のみ）、内容が変わった記録。
// 実行の集計列は、何を出すかが決まってから足す。
const UP: &str = r#"
create table crawl_runs (
    id          uuid primary key default gen_random_uuid(),
    kind        text not null,
    started_at  timestamptz not null default now(),
    finished_at timestamptz
);

create table fetch_history (
    id                     uuid primary key default gen_random_uuid(),
    run_id                 uuid not null references crawl_runs (id),
    url_id                 uuid not null references urls (id),
    fetched_at             timestamptz not null default now(),
    http_status            integer,
    response_time_ms       bigint not null default 0 check (response_time_ms >= 0),
    bytes                  bigint not null default 0 check (bytes >= 0),
    outcome                text not null
                           check (outcome in ('response', 'robots_denied', 'robots_unavailable',
                                              'out_of_scope', 'redirect_anomaly', 'network')),
    etag                   text,
    last_modified          text,
    raw_hash               text,
    body_hash              text,
    extractor_version      integer,
    declared_canonical_url text,
    error_type             text,
    error_detail           text,
    -- 同じ実行を流し直しても、1つの URL の記録は1行
    unique (run_id, url_id)
);

create index fetch_history_url_id_idx on fetch_history (url_id, fetched_at desc);

create table content_versions (
    id                uuid primary key default gen_random_uuid(),
    resource_id       uuid not null references resources (id),
    body_hash         text not null,
    created_at        timestamptz not null default now(),
    title             text,
    page_updated_on   date,
    extractor_version integer,
    unique (resource_id, body_hash)
);
"#;

const DOWN: &str = r#"
drop table content_versions;
drop table fetch_history;
drop table crawl_runs;
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
