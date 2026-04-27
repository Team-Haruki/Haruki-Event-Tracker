//! Top-100 ranking queries for non-World-Bloom events
//! (Go: `FetchLatestRanking`, `FetchAllRankings`,
//! `FetchLatestRankingByRank`, `FetchAllRankingsByRank`).
//!
//! All four select the same shape — `(timestamp, user_id, score, rank)` —
//! joining `event_<id>` ↔ `event_<id>_time_id` ↔ `event_<id>_users`.

use sea_orm::sea_query::{Alias, Expr, Order, Query, SelectStatement};
use sea_orm::{DbErr, FromQueryResult};

use crate::db::engine::DatabaseEngine;
use crate::db::entity::{event, event_users, time_id};
use crate::db::table_name::{TableKind, intern};
use crate::model::api::RecordedRankingSchema;

/// Build the shared `SELECT t.timestamp, u.user_id, e.score, e.rank FROM event_<id> e
/// INNER JOIN event_<id>_time_id t ... INNER JOIN event_<id>_users u ...` query.
fn ranking_select(event_id: i64) -> SelectStatement {
    let event_tbl = Alias::new(intern(TableKind::Event, event_id));
    let time_tbl = Alias::new(intern(TableKind::TimeId, event_id));
    let users_tbl = Alias::new(intern(TableKind::EventUsers, event_id));

    Query::select()
        .expr_as(
            Expr::col((time_tbl.clone(), time_id::Column::Timestamp)),
            Alias::new("timestamp"),
        )
        .expr_as(
            Expr::col((users_tbl.clone(), event_users::Column::UserId)),
            Alias::new("user_id"),
        )
        .expr_as(
            Expr::col((event_tbl.clone(), event::Column::Score)),
            Alias::new("score"),
        )
        .expr_as(
            Expr::col((event_tbl.clone(), event::Column::Rank)),
            Alias::new("rank"),
        )
        .from(event_tbl.clone())
        .inner_join(
            time_tbl.clone(),
            Expr::col((event_tbl.clone(), event::Column::TimeId))
                .equals((time_tbl.clone(), time_id::Column::TimeId)),
        )
        .inner_join(
            users_tbl.clone(),
            Expr::col((event_tbl.clone(), event::Column::UserIdKey))
                .equals((users_tbl.clone(), event_users::Column::UserIdKey)),
        )
        .to_owned()
}

#[tracing::instrument(skip(engine), fields(event_id, user_id = %user_id))]
pub async fn fetch_latest_ranking(
    engine: &DatabaseEngine,
    event_id: i64,
    user_id: &str,
) -> Result<Option<RecordedRankingSchema>, DbErr> {
    let users_tbl = Alias::new(intern(TableKind::EventUsers, event_id));
    let time_tbl = Alias::new(intern(TableKind::TimeId, event_id));
    let stmt = ranking_select(event_id)
        .and_where(Expr::col((users_tbl, event_users::Column::UserId)).eq(user_id))
        .order_by(
            (time_tbl, time_id::Column::Timestamp),
            Order::Desc,
        )
        .limit(1)
        .to_owned();

    let backend = engine.backend();
    RecordedRankingSchema::find_by_statement(backend.build(&stmt))
        .one(engine.conn())
        .await
}

#[tracing::instrument(skip(engine), fields(event_id, user_id = %user_id))]
pub async fn fetch_all_rankings(
    engine: &DatabaseEngine,
    event_id: i64,
    user_id: &str,
) -> Result<Vec<RecordedRankingSchema>, DbErr> {
    let users_tbl = Alias::new(intern(TableKind::EventUsers, event_id));
    let time_tbl = Alias::new(intern(TableKind::TimeId, event_id));
    let stmt = ranking_select(event_id)
        .and_where(Expr::col((users_tbl, event_users::Column::UserId)).eq(user_id))
        .order_by((time_tbl, time_id::Column::Timestamp), Order::Asc)
        .to_owned();

    let backend = engine.backend();
    RecordedRankingSchema::find_by_statement(backend.build(&stmt))
        .all(engine.conn())
        .await
}

#[tracing::instrument(skip(engine), fields(event_id, rank))]
pub async fn fetch_latest_ranking_by_rank(
    engine: &DatabaseEngine,
    event_id: i64,
    rank: i64,
) -> Result<Option<RecordedRankingSchema>, DbErr> {
    let event_tbl = Alias::new(intern(TableKind::Event, event_id));
    let time_tbl = Alias::new(intern(TableKind::TimeId, event_id));
    let stmt = ranking_select(event_id)
        .and_where(Expr::col((event_tbl, event::Column::Rank)).eq(rank))
        .order_by((time_tbl, time_id::Column::Timestamp), Order::Desc)
        .limit(1)
        .to_owned();

    let backend = engine.backend();
    RecordedRankingSchema::find_by_statement(backend.build(&stmt))
        .one(engine.conn())
        .await
}

#[tracing::instrument(skip(engine), fields(event_id, rank))]
pub async fn fetch_all_rankings_by_rank(
    engine: &DatabaseEngine,
    event_id: i64,
    rank: i64,
) -> Result<Vec<RecordedRankingSchema>, DbErr> {
    let event_tbl = Alias::new(intern(TableKind::Event, event_id));
    let time_tbl = Alias::new(intern(TableKind::TimeId, event_id));
    let stmt = ranking_select(event_id)
        .and_where(Expr::col((event_tbl, event::Column::Rank)).eq(rank))
        .order_by((time_tbl, time_id::Column::Timestamp), Order::Asc)
        .to_owned();

    let backend = engine.backend();
    RecordedRankingSchema::find_by_statement(backend.build(&stmt))
        .all(engine.conn())
        .await
}
