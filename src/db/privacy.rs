use sea_orm::sea_query::{Alias, Expr, Index, Query};
use sea_orm::{ConnectionTrait, DatabaseBackend, DbErr, ExprTrait, FromQueryResult, Statement};

use crate::db::engine::DatabaseEngine;
use crate::db::entity::event_users;
use crate::db::table_name::{TableKind, intern};
use crate::model::enums::SekaiServerRegion;
use crate::privacy::UidAnonymizer;

#[derive(Debug, FromQueryResult)]
struct UserUniqueRow {
    user_id_key: i64,
    user_id: String,
    unique_id: Option<String>,
}

pub async fn ensure_user_unique_ids(
    engine: &DatabaseEngine,
    server: SekaiServerRegion,
    event_id: i64,
    anonymizer: &UidAnonymizer,
) -> Result<(), DbErr> {
    ensure_user_table_extensions(engine, server, event_id, anonymizer).await
}

pub async fn ensure_user_table_extensions(
    engine: &DatabaseEngine,
    server: SekaiServerRegion,
    event_id: i64,
    anonymizer: &UidAnonymizer,
) -> Result<(), DbErr> {
    let table = intern(TableKind::EventUsers, event_id);
    ensure_profile_columns(engine, table).await?;

    if !anonymizer.is_enabled() {
        return Ok(());
    }

    ensure_unique_id_column(engine, table).await?;
    backfill_unique_ids(engine, server, event_id, table, anonymizer).await?;
    ensure_unique_id_index(engine, event_id, table).await?;
    Ok(())
}

async fn ensure_profile_columns(engine: &DatabaseEngine, table: &'static str) -> Result<(), DbErr> {
    for (column, ty) in [
        ("card_id", "BIGINT"),
        ("card_level", "BIGINT"),
        ("card_master_rank", "BIGINT"),
        ("card_special_training_status", "VARCHAR(64)"),
        ("card_default_image", "VARCHAR(64)"),
        ("profile_word", "VARCHAR(300)"),
        ("profile_honors_json", "TEXT"),
        ("player_frames_json", "TEXT"),
    ] {
        ensure_column(engine, table, column, ty).await?;
    }
    Ok(())
}

async fn ensure_unique_id_column(
    engine: &DatabaseEngine,
    table: &'static str,
) -> Result<(), DbErr> {
    ensure_column(engine, table, "unique_id", "VARCHAR(64)").await
}

async fn ensure_column(
    engine: &DatabaseEngine,
    table: &'static str,
    column: &str,
    ty: &str,
) -> Result<(), DbErr> {
    let backend = engine.backend();
    let stmt = Statement::from_string(
        backend,
        format!(
            "ALTER TABLE {} ADD COLUMN {} {}",
            quote_ident(backend, table),
            quote_ident(backend, column),
            ty
        ),
    );

    if let Err(err) = engine.conn().execute_raw(stmt).await
        && !is_duplicate_column_error(&err)
    {
        return Err(err);
    }
    Ok(())
}

async fn backfill_unique_ids(
    engine: &DatabaseEngine,
    server: SekaiServerRegion,
    event_id: i64,
    table: &'static str,
    anonymizer: &UidAnonymizer,
) -> Result<(), DbErr> {
    let backend = engine.backend();
    let sel = Query::select()
        .expr_as(
            Expr::col(event_users::Column::UserIdKey),
            Alias::new("user_id_key"),
        )
        .expr_as(
            Expr::col(event_users::Column::UserId),
            Alias::new("user_id"),
        )
        .expr_as(
            Expr::col(event_users::Column::UniqueId),
            Alias::new("unique_id"),
        )
        .from(Alias::new(table))
        .to_owned();

    let rows = UserUniqueRow::find_by_statement(backend.build(&sel))
        .all(engine.conn())
        .await?;
    for row in rows {
        let unique_id = anonymizer.public_user_id(server, event_id, &row.user_id);
        if row.unique_id.as_deref() == Some(unique_id.as_str()) {
            continue;
        }
        let upd = Query::update()
            .table(Alias::new(table))
            .value(event_users::Column::UniqueId, unique_id)
            .and_where(Expr::col(event_users::Column::UserIdKey).eq(row.user_id_key))
            .to_owned();
        engine.conn().execute(&upd).await?;
    }
    Ok(())
}

async fn ensure_unique_id_index(
    engine: &DatabaseEngine,
    event_id: i64,
    table: &'static str,
) -> Result<(), DbErr> {
    let backend = engine.backend();
    let mut idx = Index::create();
    idx.name(format!("idx_{event_id}_users_unique_id"))
        .table(Alias::new(table))
        .col(event_users::Column::UniqueId)
        .unique();
    if !matches!(backend, DatabaseBackend::MySql) {
        idx.if_not_exists();
    }
    if let Err(err) = engine.conn().execute(&idx.to_owned()).await
        && !is_duplicate_index_error(&err)
    {
        return Err(err);
    }
    Ok(())
}

fn is_duplicate_column_error(err: &DbErr) -> bool {
    let msg = err.to_string().to_ascii_lowercase();
    msg.contains("duplicate column")
        || msg.contains("duplicate column name")
        || msg.contains("already exists")
        || msg.contains("column \"unique_id\" of relation")
}

fn is_duplicate_index_error(err: &DbErr) -> bool {
    let msg = err.to_string().to_ascii_lowercase();
    msg.contains("duplicate key name")
        || msg.contains("already exists")
        || (msg.contains("duplicate") && msg.contains("index"))
}

fn quote_ident(backend: DatabaseBackend, ident: &str) -> String {
    let escaped = match backend {
        DatabaseBackend::MySql => ident.replace('`', "``"),
        DatabaseBackend::Postgres | DatabaseBackend::Sqlite => ident.replace('"', "\"\""),
        _ => ident.replace('"', "\"\""),
    };
    match backend {
        DatabaseBackend::MySql => format!("`{escaped}`"),
        DatabaseBackend::Postgres | DatabaseBackend::Sqlite => format!("\"{escaped}\""),
        _ => format!("\"{escaped}\""),
    }
}

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait, Database, DatabaseBackend};

    use super::*;
    use crate::db::engine::DatabaseEngine;
    use crate::db::query::ranking::fetch_latest_ranking_by_rank;
    use crate::db::query::user::{PublicUserIdMode, get_user_data};

    #[tokio::test]
    async fn lazy_migration_backfills_unique_ids_and_queries_by_public_id() {
        let conn = Database::connect("sqlite::memory:").await.unwrap();
        let engine = DatabaseEngine::from_connection(conn, DatabaseBackend::Sqlite);
        let event_id = 4242;
        let users_tbl = intern(TableKind::EventUsers, event_id);
        let time_tbl = intern(TableKind::TimeId, event_id);
        let event_tbl = intern(TableKind::Event, event_id);

        engine
            .conn()
            .execute_unprepared(&format!(
                "CREATE TABLE {users_tbl} (
                    user_id_key INTEGER PRIMARY KEY AUTOINCREMENT,
                    user_id VARCHAR(30) UNIQUE,
                    name VARCHAR(300),
                    cheerful_team_id BIGINT
                )"
            ))
            .await
            .unwrap();
        engine
            .conn()
            .execute_unprepared(&format!(
                "CREATE TABLE {time_tbl} (
                    time_id INTEGER PRIMARY KEY AUTOINCREMENT,
                    timestamp BIGINT UNIQUE,
                    status TINYINT
                )"
            ))
            .await
            .unwrap();
        engine
            .conn()
            .execute_unprepared(&format!(
                "CREATE TABLE {event_tbl} (
                    time_id BIGINT,
                    user_id_key BIGINT,
                    score BIGINT,
                    rank BIGINT,
                    PRIMARY KEY (time_id, user_id_key)
                )"
            ))
            .await
            .unwrap();
        engine
            .conn()
            .execute_unprepared(&format!(
                "INSERT INTO {users_tbl} (user_id, name, cheerful_team_id)
                 VALUES ('100', 'Miku', 3)"
            ))
            .await
            .unwrap();
        engine
            .conn()
            .execute_unprepared(&format!(
                "INSERT INTO {time_tbl} (timestamp, status) VALUES (1710000000, 0)"
            ))
            .await
            .unwrap();
        engine
            .conn()
            .execute_unprepared(&format!(
                "INSERT INTO {event_tbl} (time_id, user_id_key, score, rank)
                 VALUES (1, 1, 3900000, 5)"
            ))
            .await
            .unwrap();

        let anonymizer = UidAnonymizer::enabled("pepper");
        ensure_user_unique_ids(&engine, SekaiServerRegion::Jp, event_id, &anonymizer)
            .await
            .unwrap();
        let unique_id = anonymizer.public_user_id(SekaiServerRegion::Jp, event_id, "100");

        let raw_lookup = get_user_data(&engine, event_id, "100", PublicUserIdMode::Unique)
            .await
            .unwrap();
        assert!(raw_lookup.is_none());

        let user = get_user_data(&engine, event_id, &unique_id, PublicUserIdMode::Unique)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(user.user_id, unique_id);
        assert_eq!(user.name, "Miku");
        assert_eq!(user.cheerful_team_id, Some(3));

        let ranking = fetch_latest_ranking_by_rank(&engine, event_id, 5, PublicUserIdMode::Unique)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ranking.user_id, user.user_id);
        assert_eq!(ranking.score, 3_900_000);
    }
}
