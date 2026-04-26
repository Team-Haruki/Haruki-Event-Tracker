//! World Bloom per-character ranking queries (Go:
//! `FetchLatestWorldBloomRanking`, `FetchAllWorldBloomRankings`,
//! `FetchLatestWorldBloomRankingByRank`, `FetchAllWorldBloomRankingsByRank`).
//!
//! Same join shape as `ranking.rs` but against `wl_<id>` and selecting an
//! extra `character_id` column.

use sea_orm::sea_query::{Alias, Expr, Order, Query, SelectStatement};
use sea_orm::{DbErr, FromQueryResult};

use crate::db::engine::DatabaseEngine;
use crate::db::entity::{event_users, time_id, world_bloom};
use crate::db::table_name::{TableKind, intern};
use crate::model::api::RecordedWorldBloomRankingSchema;

fn wl_select(event_id: i64) -> SelectStatement {
    let wl_tbl = Alias::new(intern(TableKind::WorldBloom, event_id));
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
            Expr::col((wl_tbl.clone(), world_bloom::Column::Score)),
            Alias::new("score"),
        )
        .expr_as(
            Expr::col((wl_tbl.clone(), world_bloom::Column::Rank)),
            Alias::new("rank"),
        )
        .expr_as(
            Expr::col((wl_tbl.clone(), world_bloom::Column::CharacterId)),
            Alias::new("character_id"),
        )
        .from(wl_tbl.clone())
        .inner_join(
            time_tbl.clone(),
            Expr::col((wl_tbl.clone(), world_bloom::Column::TimeId))
                .equals((time_tbl.clone(), time_id::Column::TimeId)),
        )
        .inner_join(
            users_tbl.clone(),
            Expr::col((wl_tbl.clone(), world_bloom::Column::UserIdKey))
                .equals((users_tbl.clone(), event_users::Column::UserIdKey)),
        )
        .to_owned()
}

#[tracing::instrument(skip(engine), fields(event_id, user_id = %user_id, character_id))]
pub async fn fetch_latest_world_bloom_ranking(
    engine: &DatabaseEngine,
    event_id: i64,
    user_id: &str,
    character_id: i64,
) -> Result<Option<RecordedWorldBloomRankingSchema>, DbErr> {
    let users_tbl = Alias::new(intern(TableKind::EventUsers, event_id));
    let wl_tbl = Alias::new(intern(TableKind::WorldBloom, event_id));
    let time_tbl = Alias::new(intern(TableKind::TimeId, event_id));
    let stmt = wl_select(event_id)
        .and_where(Expr::col((users_tbl, event_users::Column::UserId)).eq(user_id))
        .and_where(Expr::col((wl_tbl, world_bloom::Column::CharacterId)).eq(character_id))
        .order_by((time_tbl, time_id::Column::Timestamp), Order::Desc)
        .limit(1)
        .to_owned();

    let backend = engine.backend();
    RecordedWorldBloomRankingSchema::find_by_statement(backend.build(&stmt))
        .one(engine.conn())
        .await
}

#[tracing::instrument(skip(engine), fields(event_id, user_id = %user_id, character_id))]
pub async fn fetch_all_world_bloom_rankings(
    engine: &DatabaseEngine,
    event_id: i64,
    user_id: &str,
    character_id: i64,
) -> Result<Vec<RecordedWorldBloomRankingSchema>, DbErr> {
    let users_tbl = Alias::new(intern(TableKind::EventUsers, event_id));
    let wl_tbl = Alias::new(intern(TableKind::WorldBloom, event_id));
    let time_tbl = Alias::new(intern(TableKind::TimeId, event_id));
    let stmt = wl_select(event_id)
        .and_where(Expr::col((users_tbl, event_users::Column::UserId)).eq(user_id))
        .and_where(Expr::col((wl_tbl, world_bloom::Column::CharacterId)).eq(character_id))
        .order_by((time_tbl, time_id::Column::Timestamp), Order::Asc)
        .to_owned();

    let backend = engine.backend();
    RecordedWorldBloomRankingSchema::find_by_statement(backend.build(&stmt))
        .all(engine.conn())
        .await
}

#[tracing::instrument(skip(engine), fields(event_id, rank, character_id))]
pub async fn fetch_latest_world_bloom_ranking_by_rank(
    engine: &DatabaseEngine,
    event_id: i64,
    rank: i64,
    character_id: i64,
) -> Result<Option<RecordedWorldBloomRankingSchema>, DbErr> {
    let wl_tbl = Alias::new(intern(TableKind::WorldBloom, event_id));
    let time_tbl = Alias::new(intern(TableKind::TimeId, event_id));
    let stmt = wl_select(event_id)
        .and_where(Expr::col((wl_tbl.clone(), world_bloom::Column::Rank)).eq(rank))
        .and_where(Expr::col((wl_tbl, world_bloom::Column::CharacterId)).eq(character_id))
        .order_by((time_tbl, time_id::Column::Timestamp), Order::Desc)
        .limit(1)
        .to_owned();

    let backend = engine.backend();
    RecordedWorldBloomRankingSchema::find_by_statement(backend.build(&stmt))
        .one(engine.conn())
        .await
}

#[tracing::instrument(skip(engine), fields(event_id, rank, character_id))]
pub async fn fetch_all_world_bloom_rankings_by_rank(
    engine: &DatabaseEngine,
    event_id: i64,
    rank: i64,
    character_id: i64,
) -> Result<Vec<RecordedWorldBloomRankingSchema>, DbErr> {
    let wl_tbl = Alias::new(intern(TableKind::WorldBloom, event_id));
    let time_tbl = Alias::new(intern(TableKind::TimeId, event_id));
    let stmt = wl_select(event_id)
        .and_where(Expr::col((wl_tbl.clone(), world_bloom::Column::Rank)).eq(rank))
        .and_where(Expr::col((wl_tbl, world_bloom::Column::CharacterId)).eq(character_id))
        .order_by((time_tbl, time_id::Column::Timestamp), Order::Asc)
        .to_owned();

    let backend = engine.backend();
    RecordedWorldBloomRankingSchema::find_by_statement(backend.build(&stmt))
        .all(engine.conn())
        .await
}
