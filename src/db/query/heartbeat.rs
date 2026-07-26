//! Heartbeat read/write against `event_<id>_time_id`
//! (Go: `WriteHeartbeat`, `FetchLatestHeartbeat`).
//!
//! When the upstream Sekai API fails or the diff produced no changed rows,
//! the tracker still inserts a `time_id` row with `status >= 1` so the
//! `/status` endpoint can report freshness without having to read any
//! ranking table.

use sea_orm::sea_query::{Alias, Expr, OnConflict, Order, Query};
use sea_orm::{ConnectionTrait, DatabaseBackend, DbErr, ExprTrait, FromQueryResult};

use crate::db::engine::DatabaseEngine;
use crate::db::entity::time_id;
use crate::db::table_name::{TableKind, intern};

#[derive(FromQueryResult)]
struct LatestHeartbeatRow {
    timestamp: i64,
    status: i16,
}

fn heartbeat_status_expr(backend: DatabaseBackend) -> Expr {
    match backend {
        DatabaseBackend::Postgres => Expr::cust(
            r#"CASE
                WHEN pg_typeof("status")::text = '"char"' THEN
                    CASE
                        WHEN ascii("status"::text) BETWEEN 48 AND 57
                            THEN (ascii("status"::text) - 48)::smallint
                        ELSE ascii("status"::text)::smallint
                    END
                ELSE "status"::smallint
            END"#,
        ),
        _ => Expr::col(time_id::Column::Status),
    }
}

#[tracing::instrument(skip(engine), fields(event_id, timestamp, status))]
pub async fn write_heartbeat(
    engine: &DatabaseEngine,
    event_id: i64,
    timestamp: i64,
    status: i16,
) -> Result<(), DbErr> {
    let table = intern(TableKind::TimeId, event_id);
    // The caller never reads the generated `time_id` back, so a single
    // conflict-ignoring insert replaces the SELECT/INSERT/re-SELECT
    // transaction this used to share with the batch writer.
    let ins = Query::insert()
        .into_table(Alias::new(table))
        .columns([time_id::Column::Timestamp, time_id::Column::Status])
        .values_panic([timestamp.into(), status.into()])
        .on_conflict(
            OnConflict::column(time_id::Column::Timestamp)
                .do_nothing_on([time_id::Column::Timestamp])
                .to_owned(),
        )
        .to_owned();
    engine.conn().execute(&ins).await?;
    Ok(())
}

#[tracing::instrument(skip(engine), fields(event_id))]
pub async fn fetch_latest_heartbeat(
    engine: &DatabaseEngine,
    event_id: i64,
) -> Result<Option<(i64, i16)>, DbErr> {
    fetch_latest_heartbeat_before(engine, event_id, None).await
}

#[tracing::instrument(skip(engine), fields(event_id, timestamp))]
pub async fn fetch_latest_heartbeat_before(
    engine: &DatabaseEngine,
    event_id: i64,
    timestamp: Option<i64>,
) -> Result<Option<(i64, i16)>, DbErr> {
    let backend = engine.backend();
    let table = intern(TableKind::TimeId, event_id);
    let mut stmt = Query::select()
        .expr_as(
            Expr::col(time_id::Column::Timestamp),
            Alias::new("timestamp"),
        )
        .expr_as(heartbeat_status_expr(backend), Alias::new("status"))
        .from(Alias::new(table))
        .to_owned();
    if let Some(timestamp) = timestamp {
        stmt.and_where(Expr::col(time_id::Column::Timestamp).lte(timestamp));
    }
    stmt.order_by(time_id::Column::Timestamp, Order::Desc)
        .limit(1);

    let row = LatestHeartbeatRow::find_by_statement(backend.build(&stmt))
        .one(engine.conn())
        .await?;
    Ok(row.map(|r| (r.timestamp, r.status)))
}
