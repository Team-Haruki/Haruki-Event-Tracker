use sea_orm::{ConnectionTrait, DbErr, Schema};

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

    let time_id_ent = time_id::Entity { table_name: intern(TableKind::TimeId, event_id) };
    let users_ent = event_users::Entity { table_name: intern(TableKind::EventUsers, event_id) };
    let event_ent = event::Entity { table_name: intern(TableKind::Event, event_id) };

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
        conn.execute(backend.build(&stmt)).await?;
    }
    Ok(())
}
