use sea_orm_migration::prelude::*;

use crate::model::fts::{
    create_fts_table_and_triggers, drop_fts_table_and_triggers, rebuild_fts_table,
};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        create_fts_table_and_triggers(db, "item", &["display_name"]).await?;
        rebuild_fts_table(db, "item").await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        drop_fts_table_and_triggers(db, "item").await?;
        Ok(())
    }
}
