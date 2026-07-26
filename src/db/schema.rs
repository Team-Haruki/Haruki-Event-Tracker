use sea_orm::sea_query::{Alias, Index, IndexCreateStatement};
use sea_orm::{ConnectionTrait, DatabaseBackend, DbErr, Schema, Statement};

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
    // Fresh tables get every column from `create_table_from_entity`;
    // migrating older tables is `ensure_user_table_extensions`' job (run
    // from tracker init), so no per-column ALTER probing here.
    create_query_indexes(engine, event_id, is_world_bloom).await?;
    Ok(())
}

pub async fn create_query_indexes(
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
        event_index(event_id, event_tbl, "time_rank", |idx| {
            idx.col(event::Column::TimeId).col(event::Column::Rank);
        }),
        event_index(event_id, event_tbl, "time_score", |idx| {
            idx.col(event::Column::TimeId).col(event::Column::Score);
        }),
    ];

    let users_tbl = intern(TableKind::EventUsers, event_id);
    // Equality column first, `user_id_key` second: `/users` filters paginate
    // with `ORDER BY user_id_key` + keyset cursor, so these serve filter and
    // order in one scan.
    indexes.extend([
        event_index(event_id, users_tbl, "users_card_user", |idx| {
            idx.col(event_users::Column::CardId)
                .col(event_users::Column::UserIdKey);
        }),
        event_index(event_id, users_tbl, "users_team_user", |idx| {
            idx.col(event_users::Column::CheerfulTeamId)
                .col(event_users::Column::UserIdKey);
        }),
    ]);

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
            event_index(event_id, wl_tbl, "wl_char_time_rank", |idx| {
                idx.col(world_bloom::Column::CharacterId)
                    .col(world_bloom::Column::TimeId)
                    .col(world_bloom::Column::Rank);
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
    drop_legacy_user_indexes(engine, event_id, users_tbl).await?;
    Ok(())
}

/// Indexes retired from the bootstrap set, dropped so existing tables shed
/// their write amplification: `users_name` can never serve the
/// leading-wildcard LIKE that queries names, and the single-column card/team
/// indexes are superseded by the composite keyset-pagination ones.
async fn drop_legacy_user_indexes(
    engine: &DatabaseEngine,
    event_id: i64,
    users_tbl: &'static str,
) -> Result<(), DbErr> {
    let backend = engine.backend();
    for suffix in ["users_name", "users_card_id", "users_cheerful_team"] {
        let name = format!("idx_{event_id}_{suffix}");
        let sql = match backend {
            DatabaseBackend::MySql => format!("DROP INDEX `{name}` ON `{users_tbl}`"),
            _ => format!("DROP INDEX IF EXISTS \"{name}\""),
        };
        if let Err(err) = engine
            .conn()
            .execute_raw(Statement::from_string(backend, sql))
            .await
            && !is_missing_index_error(&err)
        {
            return Err(err);
        }
    }
    Ok(())
}

fn is_missing_index_error(err: &DbErr) -> bool {
    let msg = err.to_string().to_ascii_lowercase();
    msg.contains("check that column/key exists")
        || msg.contains("does not exist")
        || msg.contains("no such index")
        || msg.contains("1091")
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

#[cfg(test)]
mod tests {
    use super::is_missing_index_error;
    use sea_orm::DbErr;

    #[test]
    fn missing_index_errors_are_ignorable() {
        for msg in [
            // MySQL 1091
            "Can't DROP 'idx_1_users_name'; check that column/key exists",
            "error 1091 (42000)",
            // Postgres
            "index \"idx_1_users_name\" does not exist",
            // SQLite
            "no such index: idx_1_users_name",
        ] {
            assert!(
                is_missing_index_error(&DbErr::Custom(msg.to_owned())),
                "should be ignorable: {msg}"
            );
        }
    }

    #[test]
    fn other_errors_are_not_ignorable() {
        for msg in [
            "connection refused",
            "syntax error at or near \"DROP\"",
            "permission denied for table event_1_users",
        ] {
            assert!(
                !is_missing_index_error(&DbErr::Custom(msg.to_owned())),
                "should not be ignorable: {msg}"
            );
        }
    }
}
