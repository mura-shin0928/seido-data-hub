pub use sea_orm_migration::prelude::*;

mod m20260910_000001_create_areas_programs;
mod m20260923_000001_create_urls;
mod m20260923_000002_create_resources;
mod m20260929_000001_create_host_moves;
mod m20260929_000002_create_crawl_history;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20260910_000001_create_areas_programs::Migration),
            Box::new(m20260923_000001_create_urls::Migration),
            Box::new(m20260923_000002_create_resources::Migration),
            Box::new(m20260929_000001_create_host_moves::Migration),
            Box::new(m20260929_000002_create_crawl_history::Migration),
        ]
    }
}
