pub use sea_orm_migration::prelude::*;

mod m20260910_000001_create_areas_programs;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![Box::new(m20260910_000001_create_areas_programs::Migration)]
    }
}
