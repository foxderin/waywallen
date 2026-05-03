use sea_orm_migration::prelude::*;

mod m20260503_000001_init_v1;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![Box::new(m20260503_000001_init_v1::Migration)]
    }
}
