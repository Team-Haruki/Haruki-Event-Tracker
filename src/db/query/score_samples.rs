//! Per-player `(timestamp, score)` histories — what the cloud round metrics
//! (`api::handler::leaderboard::round_metrics`) derive their speeds from.
//!
//! A trace (`web::search_user_trace`) joins the users table for the public
//! id string and carries the rank on every row; none of that reaches the
//! metrics. This select reads the ranking table by the player's key
//! (resolved first, `user::user_key_lookup`) and the time table only. The
//! time join stays because it is what makes the timestamp right on legacy
//! events whose `time_id`s are sequence numbers; rows are ordered by
//! `time_id`, which the `time_id` order == `timestamp` order invariant
//! makes the timestamp order.

use sea_orm::sea_query::{Alias, Expr, Order, Query};
use sea_orm::{DbErr, ExprTrait, FromQueryResult};

use crate::db::engine::DatabaseEngine;
use crate::db::entity::time_id;
use crate::db::query::edge::{TimeWindow, and_where_time_id_within};
use crate::db::query::user::{PublicUserIdMode, user_key_lookup};
use crate::db::table_name::{TableKind, intern};

#[derive(Debug, Clone, Copy, PartialEq, Eq, FromQueryResult)]
pub struct ScoreSample {
    pub timestamp: i64,
    pub score: i64,
}

/// `user_id`'s rows up to `end_time` (inclusive; `None` for the whole
/// history), oldest first; World Bloom rows of the chapter when
/// `character_id` is set. Skips the user id as a tracing field: for cloud
/// callers it is the raw upstream UID.
#[tracing::instrument(skip(engine, user_id), fields(event_id, character_id, end_time))]
pub async fn fetch_user_score_samples(
    engine: &DatabaseEngine,
    event_id: i64,
    character_id: Option<i64>,
    user_id: &str,
    end_time: Option<i64>,
    mode: PublicUserIdMode,
) -> Result<Vec<ScoreSample>, DbErr> {
    let tbl = Alias::new(match character_id {
        Some(_) => intern(TableKind::WorldBloom, event_id),
        None => intern(TableKind::Event, event_id),
    });
    let time_tbl = Alias::new(intern(TableKind::TimeId, event_id));
    let time_id_col = Alias::new("time_id");
    let mut stmt = Query::select();
    stmt.expr_as(
        Expr::col((time_tbl.clone(), time_id::Column::Timestamp)),
        Alias::new("timestamp"),
    )
    .expr_as(
        Expr::col((tbl.clone(), Alias::new("score"))),
        Alias::new("score"),
    )
    .from(tbl.clone())
    .inner_join(
        time_tbl.clone(),
        Expr::col((tbl.clone(), time_id_col.clone()))
            .equals((time_tbl.clone(), time_id::Column::TimeId)),
    )
    .and_where(
        Expr::col((tbl.clone(), Alias::new("user_id_key")))
            .eq(user_key_lookup(event_id, user_id, mode)),
    );
    if let Some(character_id) = character_id {
        stmt.and_where(Expr::col((tbl.clone(), Alias::new("character_id"))).eq(character_id));
    }
    if let Some(end_time) = end_time {
        stmt.and_where(Expr::col((time_tbl, time_id::Column::Timestamp)).lte(end_time));
    }
    and_where_time_id_within(
        &mut stmt,
        Expr::col((tbl.clone(), time_id_col.clone())),
        intern(TableKind::TimeId, event_id),
        TimeWindow::new(None, end_time),
    );
    stmt.order_by((tbl, time_id_col), Order::Asc);
    ScoreSample::find_by_statement(engine.backend().build(&stmt))
        .all(engine.conn())
        .await
}
