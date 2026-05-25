use sea_orm::sea_query::{Alias, Index, IndexCreateStatement};
use sea_orm::{ConnectionTrait, DatabaseBackend, DbErr, Schema};

use crate::db::engine::DatabaseEngine;
use crate::db::entity::{event, event_users, time_id, world_bloom};
use crate::db::table_name::{TableKind, intern};
use crate::model::enums::SekaiServerRegion;

/// Idempotent: creates `event_<id>_time_id`, `event_<id>_users`, `event_<id>`
/// (and `wl_<id>` for World Bloom events) if they don't already exist. Mirrors
/// `DatabaseEngine.CreateEventTables` in `utils/gorm/engine.go:125`.
#[tracing::instrument(skip(engine), fields(server = %server, event_id))]
pub async fn create_event_tables(
    engine: &DatabaseEngine,
    server: SekaiServerRegion,
    event_id: i64,
    is_world_bloom: bool,
) -> Result<(), DbErr> {
    let _ = server;
    let backend = engine.backend();
    let schema = Schema::new(backend);

    let time_id_ent = time_id::Entity {
        table_name: intern(TableKind::TimeId, event_id),
    };
    let users_ent = event_users::Entity {
        table_name: intern(TableKind::EventUsers, event_id),
    };
    let event_ent = event::Entity {
        table_name: intern(TableKind::Event, event_id),
    };

    let mut creates = vec![
        schema.create_table_from_entity(time_id_ent),
        schema.create_table_from_entity(users_ent),
        schema.create_table_from_entity(event_ent),
    ];
    if is_world_bloom {
        let wl_ent = world_bloom::Entity {
            table_name: intern(TableKind::WorldBloom, event_id),
        };
        creates.push(schema.create_table_from_entity(wl_ent));
    }

    let conn = engine.conn();
    for mut stmt in creates {
        stmt.if_not_exists();
        conn.execute(&stmt).await?;
    }
    create_query_indexes(engine, event_id, is_world_bloom).await?;
    Ok(())
}

async fn create_query_indexes(
    engine: &DatabaseEngine,
    event_id: i64,
    is_world_bloom: bool,
) -> Result<(), DbErr> {
    let backend = engine.backend();
    let conn = engine.conn();
    let event_tbl = intern(TableKind::Event, event_id);

    let mut indexes = vec![
        event_index(event_id, event_tbl, "rank_time", |idx| {
            idx.col(event::Column::Rank).col(event::Column::TimeId);
        }),
        event_index(event_id, event_tbl, "user_time", |idx| {
            idx.col(event::Column::UserIdKey).col(event::Column::TimeId);
        }),
    ];

    if is_world_bloom {
        let wl_tbl = intern(TableKind::WorldBloom, event_id);
        indexes.extend([
            event_index(event_id, wl_tbl, "wl_char_rank_time", |idx| {
                idx.col(world_bloom::Column::CharacterId)
                    .col(world_bloom::Column::Rank)
                    .col(world_bloom::Column::TimeId);
            }),
            event_index(event_id, wl_tbl, "wl_char_user_time", |idx| {
                idx.col(world_bloom::Column::CharacterId)
                    .col(world_bloom::Column::UserIdKey)
                    .col(world_bloom::Column::TimeId);
            }),
        ]);
    }

    for mut stmt in indexes {
        if supports_index_if_not_exists(backend) {
            stmt.if_not_exists();
        }
        if let Err(err) = conn.execute(&stmt).await
            && !is_duplicate_index_error(&err)
        {
            return Err(err);
        }
    }
    Ok(())
}

fn event_index(
    event_id: i64,
    table: &'static str,
    suffix: &str,
    columns: impl FnOnce(&mut IndexCreateStatement),
) -> IndexCreateStatement {
    let mut idx = Index::create();
    idx.name(format!("idx_{event_id}_{suffix}"))
        .table(Alias::new(table));
    columns(&mut idx);
    idx.to_owned()
}

fn supports_index_if_not_exists(backend: DatabaseBackend) -> bool {
    !matches!(backend, DatabaseBackend::MySql)
}

fn is_duplicate_index_error(err: &DbErr) -> bool {
    let msg = err.to_string().to_ascii_lowercase();
    msg.contains("duplicate key name")
        || msg.contains("already exists")
        || (msg.contains("duplicate") && msg.contains("index"))
}
