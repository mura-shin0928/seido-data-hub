use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// 巡回で使う列は、いま値が入らないものも最初から作る（後から migration を足さずに済ませるため）。
// スキーマは既存と同じく SQL で書く。
const UP: &str = r#"
create table urls (
    id              uuid primary key default gen_random_uuid(),
    raw_url         text not null,
    normalized_url  text not null,
    dedup_key       text not null unique,
    host_key        text not null,
    role            text not null default 'seed'
                    check (role in ('seed', 'index', 'discovered')),
    depth           integer not null default 0 check (depth >= 0),
    priority        integer not null default 80,
    status          text not null default 'queued'
                    check (status in ('queued', 'processing', 'succeeded', 'retry_wait', 'failed_final', 'blocked')),
    next_crawl_at   timestamptz not null default now(),
    last_crawled_at timestamptz,
    attempt_count   integer not null default 0,
    retry_count     integer not null default 0,
    worker_id       text,
    lease_until     timestamptz,
    claim_token     bigint not null default 0,
    last_error_type text,
    created_at      timestamptz not null default now(),
    updated_at      timestamptz not null default now()
);

-- スケジューラは「取れる状態で、時刻が来たもの」をホストごとに選ぶ
create index urls_claim_idx on urls (status, next_crawl_at);
create index urls_host_key_idx on urls (host_key);

create table program_urls (
    program_id uuid not null references programs (id) on delete cascade,
    url_id     uuid not null references urls (id),
    role       text not null check (role in ('main', 'related')),
    rank       integer not null default 1 check (rank >= 1),
    source     text not null default 'registry' check (source in ('registry', 'discovered')),
    primary key (program_id, url_id)
);

-- 主たる URL は1制度に1つ。関連 URL は複数持てる
create unique index program_urls_main_idx on program_urls (program_id) where role = 'main';
create index program_urls_url_id_idx on program_urls (url_id);
"#;

const DOWN: &str = r#"
drop table program_urls;
drop table urls;
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
