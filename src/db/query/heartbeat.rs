//! Heartbeat read/write against `event_<id>_time_id`
//! (Go: `WriteHeartbeat`, `FetchLatestHeartbeat`).
//!
//! When the upstream Sekai API fails or the diff produced no changed rows,
//! the tracker still inserts a `time_id` row with `status >= 1` so the
//! `/status` endpoint can report freshness without having to read any
//! ranking table.

use sea_orm::sea_query::{Alias, Expr, Order, Query};
use sea_orm::{DbErr, FromQueryResult, TransactionTrait};
use std::collections::HashSet;

use crate::db::engine::DatabaseEngine;
use crate::db::entity::time_id;
use crate::db::query::batch::batch_get_or_create_time_ids;
use crate::db::table_name::{TableKind, intern};

#[derive(FromQueryResult)]
struct LatestHeartbeatRow {
    timestamp: i64,
    status: i8,
}

#[tracing::instrument(skip(engine), fields(event_id, timestamp, status))]
pub async fn write_heartbeat(
    engine: &DatabaseEngine,
    event_id: i64,
    timestamp: i64,
    status: i8,
) -> Result<(), DbErr> {
    let backend = engine.backend();
    let table = intern(TableKind::TimeId, event_id);
    engine
        .conn()
        .transaction::<_, (), DbErr>(|tx| {
            Box::pin(async move {
                let mut set = HashSet::with_capacity(1);
                set.insert(timestamp);
                batch_get_or_create_time_ids(tx, backend, table, &set, status).await?;
                Ok(())
            })
        })
        .await
        .map_err(|e| match e {
            sea_orm::TransactionError::Connection(err) => err,
            sea_orm::TransactionError::Transaction(err) => err,
        })
}

#[tracing::instrument(skip(engine), fields(event_id))]
pub async fn fetch_latest_heartbeat(
    engine: &DatabaseEngine,
    event_id: i64,
) -> Result<Option<(i64, i8)>, DbErr> {
    let backend = engine.backend();
    let table = intern(TableKind::TimeId, event_id);
    let stmt = Query::select()
        .expr_as(Expr::col(time_id::Column::Timestamp), Alias::new("timestamp"))
        .expr_as(Expr::col(time_id::Column::Status), Alias::new("status"))
        .from(Alias::new(table))
        .order_by(time_id::Column::Timestamp, Order::Desc)
        .limit(1)
        .to_owned();

    let row = LatestHeartbeatRow::find_by_statement(backend.build(&stmt))
        .one(engine.conn())
        .await?;
    Ok(row.map(|r| (r.timestamp, r.status)))
}

