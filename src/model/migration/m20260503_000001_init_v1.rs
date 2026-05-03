//! Initial schema for `waywallen-v1.db`.
//!
//! The database filename was bumped to `-v1`, which lets us collapse
//! the previous chain of incremental migrations (init, item media meta,
//! item timestamps, item probed_at, playlist, library metadata) into a
//! single create-from-scratch step. There is no upgrade path from the
//! pre-v1 file — old DBs are simply ignored.
//!
//! `tag.name` and `playlist.name` need case-insensitive uniqueness, and
//! `playlist` carries `CHECK` constraints on enum-like text columns;
//! SeaORM's portable builder doesn't surface either, so those two
//! tables are emitted as raw SQLite DDL.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(SourcePlugin::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(SourcePlugin::Id)
                            .big_integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(SourcePlugin::Name)
                            .text()
                            .not_null()
                            .unique_key(),
                    )
                    .col(
                        ColumnDef::new(SourcePlugin::Version)
                            .text()
                            .not_null()
                            .default(""),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table(Library::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(Library::Id)
                            .big_integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(Library::PluginId).big_integer().not_null())
                    .col(ColumnDef::new(Library::Path).text().not_null())
                    .col(
                        ColumnDef::new(Library::Metadata)
                            .text()
                            .not_null()
                            .default("{}"),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_library_plugin")
                            .from(Library::Table, Library::PluginId)
                            .to(SourcePlugin::Table, SourcePlugin::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx_library_plugin_path")
                    .table(Library::Table)
                    .col(Library::PluginId)
                    .col(Library::Path)
                    .unique()
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx_library_plugin")
                    .table(Library::Table)
                    .col(Library::PluginId)
                    .to_owned(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table(Item::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(Item::Id)
                            .big_integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(Item::PluginId).big_integer().not_null())
                    .col(ColumnDef::new(Item::LibraryId).big_integer().not_null())
                    .col(ColumnDef::new(Item::Path).text().not_null())
                    .col(ColumnDef::new(Item::Type).text().not_null())
                    .col(
                        ColumnDef::new(Item::DisplayName)
                            .text()
                            .not_null()
                            .default(""),
                    )
                    .col(ColumnDef::new(Item::PreviewPath).text().null())
                    .col(ColumnDef::new(Item::Description).text().null())
                    .col(ColumnDef::new(Item::ExternalId).text().null())
                    .col(ColumnDef::new(Item::Size).big_integer().null())
                    .col(ColumnDef::new(Item::Width).integer().null())
                    .col(ColumnDef::new(Item::Height).integer().null())
                    .col(ColumnDef::new(Item::Format).text().null())
                    .col(
                        ColumnDef::new(Item::CreateAt)
                            .big_integer()
                            .not_null()
                            .default(0i64),
                    )
                    .col(
                        ColumnDef::new(Item::UpdateAt)
                            .big_integer()
                            .not_null()
                            .default(0i64),
                    )
                    .col(
                        ColumnDef::new(Item::SyncAt)
                            .big_integer()
                            .not_null()
                            .default(0i64),
                    )
                    .col(ColumnDef::new(Item::ProbedAt).big_integer().null())
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_item_plugin")
                            .from(Item::Table, Item::PluginId)
                            .to(SourcePlugin::Table, SourcePlugin::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_item_library")
                            .from(Item::Table, Item::LibraryId)
                            .to(Library::Table, Library::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx_item_library_path")
                    .table(Item::Table)
                    .col(Item::LibraryId)
                    .col(Item::Path)
                    .unique()
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx_item_plugin")
                    .table(Item::Table)
                    .col(Item::PluginId)
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx_item_external_id")
                    .table(Item::Table)
                    .col(Item::ExternalId)
                    .to_owned(),
            )
            .await?;

        manager
            .get_connection()
            .execute_unprepared(
                "CREATE TABLE IF NOT EXISTS tag (\
                   id INTEGER PRIMARY KEY AUTOINCREMENT,\
                   name TEXT NOT NULL UNIQUE COLLATE NOCASE\
                 )",
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table(ItemTag::Table)
                    .if_not_exists()
                    .col(ColumnDef::new(ItemTag::ItemId).big_integer().not_null())
                    .col(ColumnDef::new(ItemTag::TagId).big_integer().not_null())
                    .primary_key(
                        Index::create()
                            .col(ItemTag::ItemId)
                            .col(ItemTag::TagId),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_item_tag_item")
                            .from(ItemTag::Table, ItemTag::ItemId)
                            .to(Item::Table, Item::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_item_tag_tag")
                            .from(ItemTag::Table, ItemTag::TagId)
                            .to(Tag::Table, Tag::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx_item_tag_tag")
                    .table(ItemTag::Table)
                    .col(ItemTag::TagId)
                    .to_owned(),
            )
            .await?;

        manager
            .get_connection()
            .execute_unprepared(
                "CREATE TABLE IF NOT EXISTS playlist (\
                   id INTEGER PRIMARY KEY AUTOINCREMENT,\
                   name TEXT NOT NULL UNIQUE COLLATE NOCASE,\
                   source_kind TEXT NOT NULL DEFAULT 'curated' \
                     CHECK(source_kind IN ('curated','smart')),\
                   filter_json TEXT NULL,\
                   mode TEXT NOT NULL DEFAULT 'sequential' \
                     CHECK(mode IN ('sequential','shuffle','random')),\
                   interval_secs INTEGER NOT NULL DEFAULT 0,\
                   shuffle_seed BIGINT NOT NULL DEFAULT 0,\
                   create_at BIGINT NOT NULL,\
                   update_at BIGINT NOT NULL\
                 )",
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table(PlaylistItem::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(PlaylistItem::PlaylistId)
                            .big_integer()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(PlaylistItem::ItemId)
                            .big_integer()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(PlaylistItem::Position)
                            .integer()
                            .not_null(),
                    )
                    .primary_key(
                        Index::create()
                            .col(PlaylistItem::PlaylistId)
                            .col(PlaylistItem::Position),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_playlist_item_playlist")
                            .from(PlaylistItem::Table, PlaylistItem::PlaylistId)
                            .to(Playlist::Table, Playlist::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_playlist_item_item")
                            .from(PlaylistItem::Table, PlaylistItem::ItemId)
                            .to(Item::Table, Item::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx_playlist_item_unique")
                    .table(PlaylistItem::Table)
                    .col(PlaylistItem::PlaylistId)
                    .col(PlaylistItem::ItemId)
                    .unique()
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx_playlist_item_item")
                    .table(PlaylistItem::Table)
                    .col(PlaylistItem::ItemId)
                    .to_owned(),
            )
            .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(PlaylistItem::Table).to_owned())
            .await?;
        manager
            .get_connection()
            .execute_unprepared("DROP TABLE IF EXISTS playlist")
            .await?;
        manager
            .drop_table(Table::drop().table(ItemTag::Table).to_owned())
            .await?;
        manager
            .get_connection()
            .execute_unprepared("DROP TABLE IF EXISTS tag")
            .await?;
        manager
            .drop_table(Table::drop().table(Item::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(Library::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(SourcePlugin::Table).to_owned())
            .await?;
        Ok(())
    }
}

#[derive(DeriveIden)]
enum SourcePlugin {
    Table,
    Id,
    Name,
    Version,
}

#[derive(DeriveIden)]
enum Library {
    Table,
    Id,
    PluginId,
    Path,
    Metadata,
}

#[derive(DeriveIden)]
enum Item {
    Table,
    Id,
    PluginId,
    LibraryId,
    Path,
    Type,
    DisplayName,
    PreviewPath,
    Description,
    ExternalId,
    Size,
    Width,
    Height,
    Format,
    CreateAt,
    UpdateAt,
    SyncAt,
    ProbedAt,
}

#[derive(DeriveIden)]
enum Tag {
    Table,
    Id,
}

#[derive(DeriveIden)]
enum ItemTag {
    Table,
    ItemId,
    TagId,
}

#[derive(DeriveIden)]
enum Playlist {
    Table,
    Id,
}

#[derive(DeriveIden)]
enum PlaylistItem {
    Table,
    PlaylistId,
    ItemId,
    Position,
}
