use sea_orm::sea_query::{
    Alias, Expr, IntoCondition, JoinType, Order, Query, SelectStatement, SimpleExpr,
};
use sea_orm::{DbErr, ExprTrait, FromQueryResult};

use crate::db::engine::DatabaseEngine;
use crate::db::entity::{event, event_users, time_id, world_bloom};
use crate::db::query::user::PublicUserIdMode;
use crate::db::table_name::{TableKind, intern};
use crate::model::api::{
    RecordedRankData, RecordedRankingSchema, RecordedUserNameSchema,
    RecordedWorldBloomRankingSchema, TopRankingPlayerGrowthSchema, WebRankingItemSchema,
};

#[derive(Debug, Clone)]
pub struct WebRankingFilter {
    pub rank_min: Option<i64>,
    pub rank_max: Option<i64>,
    pub score_min: Option<i64>,
    pub score_max: Option<i64>,
    pub start_time: Option<i64>,
    pub end_time: Option<i64>,
    pub before: Option<i64>,
    pub after: Option<i64>,
    pub timestamp: Option<i64>,
    pub cursor: Option<WebRankingCursor>,
    pub limit: u64,
}

impl WebRankingFilter {
    fn is_rank_window(&self) -> bool {
        self.rank_min.is_some() || self.rank_max.is_some()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebRankingCursor {
    pub timestamp: i64,
    pub rank: i64,
    pub user_id_key: i64,
}

#[derive(Debug, Clone)]
pub struct WebTraceFilter {
    pub start_time: Option<i64>,
    pub end_time: Option<i64>,
    pub cursor: Option<i64>,
    pub limit: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct WebUserSearchFilter {
    pub unique_id: Option<String>,
    pub name: Option<String>,
    pub profile_word: Option<String>,
    pub card_id: Option<i64>,
    pub card_level: Option<i64>,
    pub card_master_rank: Option<i64>,
    pub card_special_training_status: Option<String>,
    pub card_default_image: Option<String>,
    pub cheerful_team_id: Option<i64>,
    pub cursor: Option<i64>,
    pub limit: u64,
}

#[derive(Debug, Clone, FromQueryResult)]
pub struct RankingPageRow {
    timestamp: i64,
    user_id: String,
    user_id_key: i64,
    score: i64,
    rank: i64,
    name: String,
    cheerful_team_id: Option<i64>,
    card_id: Option<i64>,
    card_level: Option<i64>,
    card_master_rank: Option<i64>,
    card_special_training_status: Option<String>,
    card_default_image: Option<String>,
    profile_word: Option<String>,
    profile_honors_json: Option<String>,
    honor_missions_json: Option<String>,
    player_frames_json: Option<String>,
}

#[derive(Debug, Clone, FromQueryResult)]
pub struct WorldBloomRankingPageRow {
    timestamp: i64,
    user_id: String,
    user_id_key: i64,
    score: i64,
    rank: i64,
    character_id: Option<i64>,
    name: String,
    cheerful_team_id: Option<i64>,
    card_id: Option<i64>,
    card_level: Option<i64>,
    card_master_rank: Option<i64>,
    card_special_training_status: Option<String>,
    card_default_image: Option<String>,
    profile_word: Option<String>,
    profile_honors_json: Option<String>,
    honor_missions_json: Option<String>,
    player_frames_json: Option<String>,
}

impl RankingPageRow {
    fn cursor(&self) -> WebRankingCursor {
        WebRankingCursor {
            timestamp: self.timestamp,
            rank: self.rank,
            user_id_key: self.user_id_key,
        }
    }

    fn into_schema(self) -> RecordedRankingSchema {
        RecordedRankingSchema {
            timestamp: self.timestamp,
            user_id: self.user_id,
            score: self.score,
            rank: self.rank,
        }
    }

    pub fn into_web_item(self) -> WebRankingItemSchema {
        let rank_data = RecordedRankData::Normal(self.clone_rank_schema());
        WebRankingItemSchema {
            rank_data,
            user_data: Some(self.into_user_schema()),
        }
    }

    pub fn user_id_key(&self) -> i64 {
        self.user_id_key
    }

    pub fn user_id(&self) -> &str {
        &self.user_id
    }

    pub fn rank(&self) -> i64 {
        self.rank
    }

    pub fn score(&self) -> i64 {
        self.score
    }

    pub fn timestamp(&self) -> i64 {
        self.timestamp
    }

    fn clone_rank_schema(&self) -> RecordedRankingSchema {
        RecordedRankingSchema {
            timestamp: self.timestamp,
            user_id: self.user_id.clone(),
            score: self.score,
            rank: self.rank,
        }
    }

    fn into_user_schema(self) -> RecordedUserNameSchema {
        RecordedUserNameSchema {
            user_id: self.user_id,
            name: self.name,
            cheerful_team_id: self.cheerful_team_id,
            card_id: self.card_id,
            card_level: self.card_level,
            card_master_rank: self.card_master_rank,
            card_special_training_status: self.card_special_training_status,
            card_default_image: self.card_default_image,
            profile_word: self.profile_word,
            profile_honors: parse_json_array(self.profile_honors_json.as_deref()),
            user_honor_missions: parse_json_array(self.honor_missions_json.as_deref()),
            user_player_frames: parse_json_array(self.player_frames_json.as_deref()),
        }
    }
}

impl WorldBloomRankingPageRow {
    fn cursor(&self) -> WebRankingCursor {
        WebRankingCursor {
            timestamp: self.timestamp,
            rank: self.rank,
            user_id_key: self.user_id_key,
        }
    }

    fn into_schema(self) -> RecordedWorldBloomRankingSchema {
        RecordedWorldBloomRankingSchema {
            timestamp: self.timestamp,
            user_id: self.user_id,
            score: self.score,
            rank: self.rank,
            character_id: self.character_id,
        }
    }

    pub fn into_web_item(self) -> WebRankingItemSchema {
        let rank_data = RecordedRankData::WorldBloom(self.clone_rank_schema());
        WebRankingItemSchema {
            rank_data,
            user_data: Some(self.into_user_schema()),
        }
    }

    pub fn user_id_key(&self) -> i64 {
        self.user_id_key
    }

    pub fn user_id(&self) -> &str {
        &self.user_id
    }

    pub fn rank(&self) -> i64 {
        self.rank
    }

    pub fn score(&self) -> i64 {
        self.score
    }

    pub fn timestamp(&self) -> i64 {
        self.timestamp
    }

    pub fn character_id(&self) -> Option<i64> {
        self.character_id
    }

    fn clone_rank_schema(&self) -> RecordedWorldBloomRankingSchema {
        RecordedWorldBloomRankingSchema {
            timestamp: self.timestamp,
            user_id: self.user_id.clone(),
            score: self.score,
            rank: self.rank,
            character_id: self.character_id,
        }
    }

    fn into_user_schema(self) -> RecordedUserNameSchema {
        RecordedUserNameSchema {
            user_id: self.user_id,
            name: self.name,
            cheerful_team_id: self.cheerful_team_id,
            card_id: self.card_id,
            card_level: self.card_level,
            card_master_rank: self.card_master_rank,
            card_special_training_status: self.card_special_training_status,
            card_default_image: self.card_default_image,
            profile_word: self.profile_word,
            profile_honors: parse_json_array(self.profile_honors_json.as_deref()),
            user_honor_missions: parse_json_array(self.honor_missions_json.as_deref()),
            user_player_frames: parse_json_array(self.player_frames_json.as_deref()),
        }
    }
}

#[derive(Debug, FromQueryResult)]
struct PlayerGrowthRow {
    user_id_key: i64,
    timestamp: i64,
    score: i64,
}

fn select_user_profile_columns(stmt: &mut SelectStatement, users_tbl: Alias) {
    stmt.expr_as(
        Expr::col((users_tbl.clone(), event_users::Column::Name)),
        Alias::new("name"),
    )
    .expr_as(
        Expr::col((users_tbl.clone(), event_users::Column::CheerfulTeamId)),
        Alias::new("cheerful_team_id"),
    )
    .expr_as(
        Expr::col((users_tbl.clone(), event_users::Column::CardId)),
        Alias::new("card_id"),
    )
    .expr_as(
        Expr::col((users_tbl.clone(), event_users::Column::CardLevel)),
        Alias::new("card_level"),
    )
    .expr_as(
        Expr::col((users_tbl.clone(), event_users::Column::CardMasterRank)),
        Alias::new("card_master_rank"),
    )
    .expr_as(
        Expr::col((
            users_tbl.clone(),
            event_users::Column::CardSpecialTrainingStatus,
        )),
        Alias::new("card_special_training_status"),
    )
    .expr_as(
        Expr::col((users_tbl.clone(), event_users::Column::CardDefaultImage)),
        Alias::new("card_default_image"),
    )
    .expr_as(
        Expr::col((users_tbl.clone(), event_users::Column::ProfileWord)),
        Alias::new("profile_word"),
    )
    .expr_as(
        Expr::col((users_tbl.clone(), event_users::Column::ProfileHonorsJson)),
        Alias::new("profile_honors_json"),
    )
    .expr_as(
        Expr::col((users_tbl.clone(), event_users::Column::HonorMissionsJson)),
        Alias::new("honor_missions_json"),
    )
    .expr_as(
        Expr::col((users_tbl, event_users::Column::PlayerFramesJson)),
        Alias::new("player_frames_json"),
    );
}

fn ranking_select(event_id: i64, mode: PublicUserIdMode) -> SelectStatement {
    let event_tbl = Alias::new(intern(TableKind::Event, event_id));
    let time_tbl = Alias::new(intern(TableKind::TimeId, event_id));
    let users_tbl = Alias::new(intern(TableKind::EventUsers, event_id));

    let mut stmt = Query::select();
    stmt.expr_as(
        Expr::col((time_tbl.clone(), time_id::Column::Timestamp)),
        Alias::new("timestamp"),
    )
    .expr_as(
        Expr::col((users_tbl.clone(), mode.output_column())),
        Alias::new("user_id"),
    )
    .expr_as(
        Expr::col((users_tbl.clone(), event_users::Column::UserIdKey)),
        Alias::new("user_id_key"),
    )
    .expr_as(
        Expr::col((event_tbl.clone(), event::Column::Score)),
        Alias::new("score"),
    )
    .expr_as(
        Expr::col((event_tbl.clone(), event::Column::Rank)),
        Alias::new("rank"),
    );
    select_user_profile_columns(&mut stmt, users_tbl.clone());
    stmt.from(event_tbl.clone())
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

fn world_bloom_select(event_id: i64, mode: PublicUserIdMode) -> SelectStatement {
    let wl_tbl = Alias::new(intern(TableKind::WorldBloom, event_id));
    let time_tbl = Alias::new(intern(TableKind::TimeId, event_id));
    let users_tbl = Alias::new(intern(TableKind::EventUsers, event_id));

    let mut stmt = Query::select();
    stmt.expr_as(
        Expr::col((time_tbl.clone(), time_id::Column::Timestamp)),
        Alias::new("timestamp"),
    )
    .expr_as(
        Expr::col((users_tbl.clone(), mode.output_column())),
        Alias::new("user_id"),
    )
    .expr_as(
        Expr::col((users_tbl.clone(), event_users::Column::UserIdKey)),
        Alias::new("user_id_key"),
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
    );
    select_user_profile_columns(&mut stmt, users_tbl.clone());
    stmt.from(wl_tbl.clone())
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

fn apply_common_filters(
    stmt: &mut SelectStatement,
    score_col: SimpleExpr,
    rank_col: SimpleExpr,
    timestamp_col: SimpleExpr,
    user_key_col: SimpleExpr,
    filter: &WebRankingFilter,
) {
    if let Some(rank_min) = filter.rank_min {
        stmt.and_where(rank_col.clone().gte(rank_min));
    }
    if let Some(rank_max) = filter.rank_max {
        stmt.and_where(rank_col.clone().lte(rank_max));
    }
    if let Some(score_min) = filter.score_min {
        stmt.and_where(score_col.clone().gte(score_min));
    }
    if let Some(score_max) = filter.score_max {
        stmt.and_where(score_col.clone().lte(score_max));
    }
    if let Some(start_time) = filter.start_time {
        stmt.and_where(timestamp_col.clone().gte(start_time));
    }
    if let Some(end_time) = filter.end_time {
        stmt.and_where(timestamp_col.clone().lte(end_time));
    }
    if let Some(before) = filter.before {
        stmt.and_where(timestamp_col.clone().lte(before));
    }
    if let Some(after) = filter.after {
        stmt.and_where(timestamp_col.clone().gte(after));
    }
    if let Some(timestamp) = filter.timestamp {
        stmt.and_where(timestamp_col.clone().eq(timestamp));
    }
    if let Some(cursor) = filter.cursor {
        stmt.and_where(
            Expr::expr(timestamp_col.clone())
                .lt(cursor.timestamp)
                .or(Expr::expr(timestamp_col.clone())
                    .eq(cursor.timestamp)
                    .and(Expr::expr(rank_col.clone()).gt(cursor.rank)))
                .or(Expr::expr(timestamp_col)
                    .eq(cursor.timestamp)
                    .and(Expr::expr(rank_col).eq(cursor.rank))
                    .and(Expr::expr(user_key_col).gt(cursor.user_id_key)))
                .into_condition()
                .into(),
        );
    }
}

fn apply_rank_window_time_filters(
    stmt: &mut SelectStatement,
    timestamp_col: SimpleExpr,
    filter: &WebRankingFilter,
) {
    if let Some(start_time) = filter.start_time {
        stmt.and_where(timestamp_col.clone().gte(start_time));
    }
    if let Some(end_time) = filter.end_time {
        stmt.and_where(timestamp_col.clone().lte(end_time));
    }
    if let Some(before) = filter.before {
        stmt.and_where(timestamp_col.clone().lte(before));
    }
    if let Some(after) = filter.after {
        stmt.and_where(timestamp_col.clone().gte(after));
    }
    if let Some(timestamp) = filter.timestamp {
        stmt.and_where(timestamp_col.lte(timestamp));
    }
}

fn apply_rank_window_score_filters(
    stmt: &mut SelectStatement,
    score_col: SimpleExpr,
    filter: &WebRankingFilter,
) {
    if let Some(score_min) = filter.score_min {
        stmt.and_where(score_col.clone().gte(score_min));
    }
    if let Some(score_max) = filter.score_max {
        stmt.and_where(score_col.lte(score_max));
    }
}

fn apply_rank_window_outer_filters(
    stmt: &mut SelectStatement,
    score_col: SimpleExpr,
    rank_col: SimpleExpr,
    user_key_col: SimpleExpr,
    filter: &WebRankingFilter,
) {
    if let Some(score_min) = filter.score_min {
        stmt.and_where(score_col.clone().gte(score_min));
    }
    if let Some(score_max) = filter.score_max {
        stmt.and_where(score_col.lte(score_max));
    }
    if let Some(cursor) = filter.cursor {
        stmt.and_where(
            Expr::expr(rank_col.clone())
                .gt(cursor.rank)
                .or(Expr::expr(rank_col)
                    .eq(cursor.rank)
                    .and(Expr::expr(user_key_col).gt(cursor.user_id_key)))
                .into_condition()
                .into(),
        );
    }
}

fn limit_rank_window(filter: &WebRankingFilter) -> u64 {
    filter.limit + 1
}

fn latest_rank_window_select(
    event_id: i64,
    filter: &WebRankingFilter,
    mode: PublicUserIdMode,
) -> SelectStatement {
    let event_tbl = Alias::new(intern(TableKind::Event, event_id));
    let time_tbl = Alias::new(intern(TableKind::TimeId, event_id));
    let users_tbl = Alias::new(intern(TableKind::EventUsers, event_id));
    let latest_tbl = Alias::new("latest_rank");

    let mut latest = Query::select();
    latest
        .expr_as(
            Expr::col((event_tbl.clone(), event::Column::Rank)),
            Alias::new("rank"),
        )
        .expr_as(
            Expr::col((event_tbl.clone(), event::Column::TimeId)).max(),
            Alias::new("time_id"),
        )
        .from(event_tbl.clone())
        .inner_join(
            time_tbl.clone(),
            Expr::col((event_tbl.clone(), event::Column::TimeId))
                .equals((time_tbl.clone(), time_id::Column::TimeId)),
        );
    if let Some(rank_min) = filter.rank_min {
        latest.and_where(Expr::col((event_tbl.clone(), event::Column::Rank)).gte(rank_min));
    }
    if let Some(rank_max) = filter.rank_max {
        latest.and_where(Expr::col((event_tbl.clone(), event::Column::Rank)).lte(rank_max));
    }
    apply_rank_window_time_filters(
        &mut latest,
        Expr::col((time_tbl.clone(), time_id::Column::Timestamp)),
        filter,
    );
    apply_rank_window_score_filters(
        &mut latest,
        Expr::col((event_tbl.clone(), event::Column::Score)),
        filter,
    );
    latest.group_by_col((event_tbl.clone(), event::Column::Rank));

    let mut stmt = Query::select();
    stmt.expr_as(
        Expr::col((time_tbl.clone(), time_id::Column::Timestamp)),
        Alias::new("timestamp"),
    )
    .expr_as(
        Expr::col((users_tbl.clone(), mode.output_column())),
        Alias::new("user_id"),
    )
    .expr_as(
        Expr::col((users_tbl.clone(), event_users::Column::UserIdKey)),
        Alias::new("user_id_key"),
    )
    .expr_as(
        Expr::col((event_tbl.clone(), event::Column::Score)),
        Alias::new("score"),
    )
    .expr_as(
        Expr::col((event_tbl.clone(), event::Column::Rank)),
        Alias::new("rank"),
    );
    select_user_profile_columns(&mut stmt, users_tbl.clone());
    stmt.from(event_tbl.clone())
        .inner_join(
            time_tbl.clone(),
            Expr::col((event_tbl.clone(), event::Column::TimeId))
                .equals((time_tbl.clone(), time_id::Column::TimeId)),
        )
        .join_subquery(
            JoinType::InnerJoin,
            latest.to_owned(),
            latest_tbl.clone(),
            Expr::col((event_tbl.clone(), event::Column::Rank))
                .equals((latest_tbl.clone(), Alias::new("rank")))
                .and(
                    Expr::col((event_tbl.clone(), event::Column::TimeId))
                        .equals((latest_tbl, Alias::new("time_id"))),
                ),
        )
        .inner_join(
            users_tbl.clone(),
            Expr::col((event_tbl.clone(), event::Column::UserIdKey))
                .equals((users_tbl.clone(), event_users::Column::UserIdKey)),
        );

    apply_rank_window_outer_filters(
        &mut stmt,
        Expr::col((event_tbl.clone(), event::Column::Score)),
        Expr::col((event_tbl.clone(), event::Column::Rank)),
        Expr::col((event_tbl.clone(), event::Column::UserIdKey)),
        filter,
    );
    stmt.order_by((event_tbl.clone(), event::Column::Rank), Order::Asc)
        .order_by((event_tbl, event::Column::UserIdKey), Order::Asc)
        .limit(limit_rank_window(filter))
        .to_owned()
}

fn latest_world_bloom_rank_window_select(
    event_id: i64,
    character_id: i64,
    filter: &WebRankingFilter,
    mode: PublicUserIdMode,
) -> SelectStatement {
    let wl_tbl = Alias::new(intern(TableKind::WorldBloom, event_id));
    let time_tbl = Alias::new(intern(TableKind::TimeId, event_id));
    let users_tbl = Alias::new(intern(TableKind::EventUsers, event_id));
    let latest_tbl = Alias::new("latest_rank");

    let mut latest = Query::select();
    latest
        .expr_as(
            Expr::col((wl_tbl.clone(), world_bloom::Column::Rank)),
            Alias::new("rank"),
        )
        .expr_as(
            Expr::col((wl_tbl.clone(), world_bloom::Column::TimeId)).max(),
            Alias::new("time_id"),
        )
        .from(wl_tbl.clone())
        .inner_join(
            time_tbl.clone(),
            Expr::col((wl_tbl.clone(), world_bloom::Column::TimeId))
                .equals((time_tbl.clone(), time_id::Column::TimeId)),
        )
        .and_where(Expr::col((wl_tbl.clone(), world_bloom::Column::CharacterId)).eq(character_id));
    if let Some(rank_min) = filter.rank_min {
        latest.and_where(Expr::col((wl_tbl.clone(), world_bloom::Column::Rank)).gte(rank_min));
    }
    if let Some(rank_max) = filter.rank_max {
        latest.and_where(Expr::col((wl_tbl.clone(), world_bloom::Column::Rank)).lte(rank_max));
    }
    apply_rank_window_time_filters(
        &mut latest,
        Expr::col((time_tbl.clone(), time_id::Column::Timestamp)),
        filter,
    );
    apply_rank_window_score_filters(
        &mut latest,
        Expr::col((wl_tbl.clone(), world_bloom::Column::Score)),
        filter,
    );
    latest.group_by_col((wl_tbl.clone(), world_bloom::Column::Rank));

    let mut stmt = Query::select();
    stmt.expr_as(
        Expr::col((time_tbl.clone(), time_id::Column::Timestamp)),
        Alias::new("timestamp"),
    )
    .expr_as(
        Expr::col((users_tbl.clone(), mode.output_column())),
        Alias::new("user_id"),
    )
    .expr_as(
        Expr::col((users_tbl.clone(), event_users::Column::UserIdKey)),
        Alias::new("user_id_key"),
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
    );
    select_user_profile_columns(&mut stmt, users_tbl.clone());
    stmt.from(wl_tbl.clone())
        .inner_join(
            time_tbl.clone(),
            Expr::col((wl_tbl.clone(), world_bloom::Column::TimeId))
                .equals((time_tbl.clone(), time_id::Column::TimeId)),
        )
        .join_subquery(
            JoinType::InnerJoin,
            latest.to_owned(),
            latest_tbl.clone(),
            Expr::col((wl_tbl.clone(), world_bloom::Column::Rank))
                .equals((latest_tbl.clone(), Alias::new("rank")))
                .and(
                    Expr::col((wl_tbl.clone(), world_bloom::Column::TimeId))
                        .equals((latest_tbl, Alias::new("time_id"))),
                ),
        )
        .inner_join(
            users_tbl.clone(),
            Expr::col((wl_tbl.clone(), world_bloom::Column::UserIdKey))
                .equals((users_tbl.clone(), event_users::Column::UserIdKey)),
        )
        .and_where(Expr::col((wl_tbl.clone(), world_bloom::Column::CharacterId)).eq(character_id));

    apply_rank_window_outer_filters(
        &mut stmt,
        Expr::col((wl_tbl.clone(), world_bloom::Column::Score)),
        Expr::col((wl_tbl.clone(), world_bloom::Column::Rank)),
        Expr::col((wl_tbl.clone(), world_bloom::Column::UserIdKey)),
        filter,
    );
    stmt.order_by((wl_tbl.clone(), world_bloom::Column::Rank), Order::Asc)
        .order_by((wl_tbl, world_bloom::Column::UserIdKey), Order::Asc)
        .limit(limit_rank_window(filter))
        .to_owned()
}

#[tracing::instrument(skip(engine, filter), fields(event_id))]
pub async fn search_rankings(
    engine: &DatabaseEngine,
    event_id: i64,
    filter: &WebRankingFilter,
    mode: PublicUserIdMode,
) -> Result<(Vec<WebRankingItemSchema>, Option<WebRankingCursor>), DbErr> {
    let (rows, next_cursor) = search_ranking_rows(engine, event_id, filter, mode).await?;
    Ok((
        rows.into_iter()
            .map(RankingPageRow::into_web_item)
            .collect(),
        next_cursor,
    ))
}

#[tracing::instrument(skip(engine, filter), fields(event_id))]
pub async fn search_ranking_rows(
    engine: &DatabaseEngine,
    event_id: i64,
    filter: &WebRankingFilter,
    mode: PublicUserIdMode,
) -> Result<(Vec<RankingPageRow>, Option<WebRankingCursor>), DbErr> {
    let event_tbl = Alias::new(intern(TableKind::Event, event_id));
    let time_tbl = Alias::new(intern(TableKind::TimeId, event_id));
    let users_tbl = Alias::new(intern(TableKind::EventUsers, event_id));
    let stmt = if filter.is_rank_window() {
        latest_rank_window_select(event_id, filter, mode)
    } else {
        let mut stmt = ranking_select(event_id, mode);
        apply_common_filters(
            &mut stmt,
            Expr::col((event_tbl.clone(), event::Column::Score)),
            Expr::col((event_tbl.clone(), event::Column::Rank)),
            Expr::col((time_tbl.clone(), time_id::Column::Timestamp)),
            Expr::col((users_tbl, event_users::Column::UserIdKey)),
            filter,
        );
        stmt.order_by((time_tbl, time_id::Column::Timestamp), Order::Desc)
            .order_by((event_tbl.clone(), event::Column::Rank), Order::Asc)
            .order_by((event_tbl, event::Column::UserIdKey), Order::Asc)
            .limit(filter.limit + 1)
            .to_owned()
    };

    let backend = engine.backend();
    let mut rows = RankingPageRow::find_by_statement(backend.build(&stmt))
        .all(engine.conn())
        .await?;
    let next_cursor = if rows.len() > filter.limit as usize {
        rows.pop().map(|row| row.cursor())
    } else {
        None
    };
    Ok((rows, next_cursor))
}

#[tracing::instrument(skip(engine, filter), fields(event_id, character_id))]
pub async fn search_world_bloom_rankings(
    engine: &DatabaseEngine,
    event_id: i64,
    character_id: i64,
    filter: &WebRankingFilter,
    mode: PublicUserIdMode,
) -> Result<(Vec<WebRankingItemSchema>, Option<WebRankingCursor>), DbErr> {
    let (rows, next_cursor) =
        search_world_bloom_ranking_rows(engine, event_id, character_id, filter, mode).await?;
    Ok((
        rows.into_iter()
            .map(WorldBloomRankingPageRow::into_web_item)
            .collect(),
        next_cursor,
    ))
}

#[tracing::instrument(skip(engine, filter), fields(event_id, character_id))]
pub async fn search_world_bloom_ranking_rows(
    engine: &DatabaseEngine,
    event_id: i64,
    character_id: i64,
    filter: &WebRankingFilter,
    mode: PublicUserIdMode,
) -> Result<(Vec<WorldBloomRankingPageRow>, Option<WebRankingCursor>), DbErr> {
    let wl_tbl = Alias::new(intern(TableKind::WorldBloom, event_id));
    let time_tbl = Alias::new(intern(TableKind::TimeId, event_id));
    let users_tbl = Alias::new(intern(TableKind::EventUsers, event_id));
    let stmt = if filter.is_rank_window() {
        latest_world_bloom_rank_window_select(event_id, character_id, filter, mode)
    } else {
        let mut stmt = world_bloom_select(event_id, mode);
        stmt.and_where(
            Expr::col((wl_tbl.clone(), world_bloom::Column::CharacterId)).eq(character_id),
        );
        apply_common_filters(
            &mut stmt,
            Expr::col((wl_tbl.clone(), world_bloom::Column::Score)),
            Expr::col((wl_tbl.clone(), world_bloom::Column::Rank)),
            Expr::col((time_tbl.clone(), time_id::Column::Timestamp)),
            Expr::col((users_tbl, event_users::Column::UserIdKey)),
            filter,
        );
        stmt.order_by((time_tbl, time_id::Column::Timestamp), Order::Desc)
            .order_by((wl_tbl.clone(), world_bloom::Column::Rank), Order::Asc)
            .order_by((wl_tbl, world_bloom::Column::UserIdKey), Order::Asc)
            .limit(filter.limit + 1)
            .to_owned()
    };

    let backend = engine.backend();
    let mut rows = WorldBloomRankingPageRow::find_by_statement(backend.build(&stmt))
        .all(engine.conn())
        .await?;
    let next_cursor = if rows.len() > filter.limit as usize {
        rows.pop().map(|row| row.cursor())
    } else {
        None
    };
    Ok((rows, next_cursor))
}

#[tracing::instrument(skip(engine, top_rows), fields(event_id, top_len = top_rows.len(), start_time))]
pub async fn fetch_top_player_growths(
    engine: &DatabaseEngine,
    event_id: i64,
    top_rows: &[RankingPageRow],
    start_time: i64,
    end_time: Option<i64>,
) -> Result<Vec<TopRankingPlayerGrowthSchema>, DbErr> {
    if top_rows.is_empty() {
        return Ok(Vec::new());
    }
    let user_keys = top_rows
        .iter()
        .map(RankingPageRow::user_id_key)
        .collect::<Vec<_>>();
    let event_tbl = Alias::new(intern(TableKind::Event, event_id));
    let time_tbl = Alias::new(intern(TableKind::TimeId, event_id));
    let stmt = Query::select()
        .expr_as(
            Expr::col((event_tbl.clone(), event::Column::UserIdKey)),
            Alias::new("user_id_key"),
        )
        .expr_as(
            Expr::col((time_tbl.clone(), time_id::Column::Timestamp)),
            Alias::new("timestamp"),
        )
        .expr_as(
            Expr::col((event_tbl.clone(), event::Column::Score)),
            Alias::new("score"),
        )
        .from(event_tbl.clone())
        .inner_join(
            time_tbl.clone(),
            Expr::col((event_tbl.clone(), event::Column::TimeId))
                .equals((time_tbl.clone(), time_id::Column::TimeId)),
        )
        .and_where(Expr::col((event_tbl.clone(), event::Column::UserIdKey)).is_in(user_keys))
        .and_where(Expr::col((time_tbl.clone(), time_id::Column::Timestamp)).gte(start_time))
        .to_owned();
    let mut stmt = stmt;
    if let Some(end_time) = end_time {
        stmt.and_where(Expr::col((time_tbl.clone(), time_id::Column::Timestamp)).lte(end_time));
    }
    stmt.order_by((event_tbl.clone(), event::Column::UserIdKey), Order::Asc)
        .order_by((time_tbl, time_id::Column::Timestamp), Order::Asc);

    let backend = engine.backend();
    let rows = PlayerGrowthRow::find_by_statement(backend.build(&stmt))
        .all(engine.conn())
        .await?;
    Ok(build_top_player_growths(top_rows, rows, None))
}

#[tracing::instrument(skip(engine, top_rows), fields(event_id, character_id, top_len = top_rows.len(), start_time))]
pub async fn fetch_world_bloom_top_player_growths(
    engine: &DatabaseEngine,
    event_id: i64,
    character_id: i64,
    top_rows: &[WorldBloomRankingPageRow],
    start_time: i64,
    end_time: Option<i64>,
) -> Result<Vec<TopRankingPlayerGrowthSchema>, DbErr> {
    if top_rows.is_empty() {
        return Ok(Vec::new());
    }
    let user_keys = top_rows
        .iter()
        .map(WorldBloomRankingPageRow::user_id_key)
        .collect::<Vec<_>>();
    let wl_tbl = Alias::new(intern(TableKind::WorldBloom, event_id));
    let time_tbl = Alias::new(intern(TableKind::TimeId, event_id));
    let stmt = Query::select()
        .expr_as(
            Expr::col((wl_tbl.clone(), world_bloom::Column::UserIdKey)),
            Alias::new("user_id_key"),
        )
        .expr_as(
            Expr::col((time_tbl.clone(), time_id::Column::Timestamp)),
            Alias::new("timestamp"),
        )
        .expr_as(
            Expr::col((wl_tbl.clone(), world_bloom::Column::Score)),
            Alias::new("score"),
        )
        .from(wl_tbl.clone())
        .inner_join(
            time_tbl.clone(),
            Expr::col((wl_tbl.clone(), world_bloom::Column::TimeId))
                .equals((time_tbl.clone(), time_id::Column::TimeId)),
        )
        .and_where(Expr::col((wl_tbl.clone(), world_bloom::Column::CharacterId)).eq(character_id))
        .and_where(Expr::col((wl_tbl.clone(), world_bloom::Column::UserIdKey)).is_in(user_keys))
        .and_where(Expr::col((time_tbl.clone(), time_id::Column::Timestamp)).gte(start_time))
        .to_owned();
    let mut stmt = stmt;
    if let Some(end_time) = end_time {
        stmt.and_where(Expr::col((time_tbl.clone(), time_id::Column::Timestamp)).lte(end_time));
    }
    stmt.order_by((wl_tbl.clone(), world_bloom::Column::UserIdKey), Order::Asc)
        .order_by((time_tbl, time_id::Column::Timestamp), Order::Asc);

    let backend = engine.backend();
    let rows = PlayerGrowthRow::find_by_statement(backend.build(&stmt))
        .all(engine.conn())
        .await?;
    Ok(build_wb_top_player_growths(
        top_rows,
        rows,
        Some(character_id),
    ))
}

#[tracing::instrument(skip(engine, filter), fields(event_id, user_id = %user_id))]
pub async fn search_user_trace(
    engine: &DatabaseEngine,
    event_id: i64,
    user_id: &str,
    filter: &WebTraceFilter,
    mode: PublicUserIdMode,
) -> Result<Vec<RecordedRankData>, DbErr> {
    let users_tbl = Alias::new(intern(TableKind::EventUsers, event_id));
    let time_tbl = Alias::new(intern(TableKind::TimeId, event_id));
    let mut stmt = ranking_select(event_id, mode);
    stmt.and_where(Expr::col((users_tbl, mode.output_column())).eq(user_id));
    apply_trace_filters(
        &mut stmt,
        Expr::col((time_tbl.clone(), time_id::Column::Timestamp)),
        filter,
    );
    stmt.order_by((time_tbl, time_id::Column::Timestamp), Order::Asc);
    if let Some(limit) = filter.limit {
        stmt.limit(limit);
    }

    let backend = engine.backend();
    Ok(RankingPageRow::find_by_statement(backend.build(&stmt))
        .all(engine.conn())
        .await?
        .into_iter()
        .map(RankingPageRow::into_schema)
        .map(RecordedRankData::Normal)
        .collect())
}

#[tracing::instrument(skip(engine, filter), fields(event_id, character_id, user_id = %user_id))]
pub async fn search_world_bloom_user_trace(
    engine: &DatabaseEngine,
    event_id: i64,
    character_id: i64,
    user_id: &str,
    filter: &WebTraceFilter,
    mode: PublicUserIdMode,
) -> Result<Vec<RecordedRankData>, DbErr> {
    let users_tbl = Alias::new(intern(TableKind::EventUsers, event_id));
    let wl_tbl = Alias::new(intern(TableKind::WorldBloom, event_id));
    let time_tbl = Alias::new(intern(TableKind::TimeId, event_id));
    let mut stmt = world_bloom_select(event_id, mode);
    stmt.and_where(Expr::col((users_tbl, mode.output_column())).eq(user_id))
        .and_where(Expr::col((wl_tbl, world_bloom::Column::CharacterId)).eq(character_id));
    apply_trace_filters(
        &mut stmt,
        Expr::col((time_tbl.clone(), time_id::Column::Timestamp)),
        filter,
    );
    stmt.order_by((time_tbl, time_id::Column::Timestamp), Order::Asc);
    if let Some(limit) = filter.limit {
        stmt.limit(limit);
    }

    let backend = engine.backend();
    Ok(
        WorldBloomRankingPageRow::find_by_statement(backend.build(&stmt))
            .all(engine.conn())
            .await?
            .into_iter()
            .map(WorldBloomRankingPageRow::into_schema)
            .map(RecordedRankData::WorldBloom)
            .collect(),
    )
}

#[tracing::instrument(skip(engine, filter), fields(event_id, rank))]
pub async fn search_rank_trace(
    engine: &DatabaseEngine,
    event_id: i64,
    rank: i64,
    filter: &WebTraceFilter,
    mode: PublicUserIdMode,
) -> Result<Vec<RecordedRankData>, DbErr> {
    let event_tbl = Alias::new(intern(TableKind::Event, event_id));
    let time_tbl = Alias::new(intern(TableKind::TimeId, event_id));
    let mut stmt = ranking_select(event_id, mode);
    stmt.and_where(Expr::col((event_tbl, event::Column::Rank)).eq(rank));
    apply_trace_filters(
        &mut stmt,
        Expr::col((time_tbl.clone(), time_id::Column::Timestamp)),
        filter,
    );
    stmt.order_by((time_tbl, time_id::Column::Timestamp), Order::Asc);
    if let Some(limit) = filter.limit {
        stmt.limit(limit);
    }

    let backend = engine.backend();
    Ok(RankingPageRow::find_by_statement(backend.build(&stmt))
        .all(engine.conn())
        .await?
        .into_iter()
        .map(RankingPageRow::into_schema)
        .map(RecordedRankData::Normal)
        .collect())
}

#[tracing::instrument(skip(engine, filter), fields(event_id, character_id, rank))]
pub async fn search_world_bloom_rank_trace(
    engine: &DatabaseEngine,
    event_id: i64,
    character_id: i64,
    rank: i64,
    filter: &WebTraceFilter,
    mode: PublicUserIdMode,
) -> Result<Vec<RecordedRankData>, DbErr> {
    let wl_tbl = Alias::new(intern(TableKind::WorldBloom, event_id));
    let time_tbl = Alias::new(intern(TableKind::TimeId, event_id));
    let mut stmt = world_bloom_select(event_id, mode);
    stmt.and_where(Expr::col((wl_tbl.clone(), world_bloom::Column::Rank)).eq(rank))
        .and_where(Expr::col((wl_tbl, world_bloom::Column::CharacterId)).eq(character_id));
    apply_trace_filters(
        &mut stmt,
        Expr::col((time_tbl.clone(), time_id::Column::Timestamp)),
        filter,
    );
    stmt.order_by((time_tbl, time_id::Column::Timestamp), Order::Asc);
    if let Some(limit) = filter.limit {
        stmt.limit(limit);
    }

    let backend = engine.backend();
    Ok(
        WorldBloomRankingPageRow::find_by_statement(backend.build(&stmt))
            .all(engine.conn())
            .await?
            .into_iter()
            .map(WorldBloomRankingPageRow::into_schema)
            .map(RecordedRankData::WorldBloom)
            .collect(),
    )
}

fn apply_trace_filters(
    stmt: &mut SelectStatement,
    timestamp_col: SimpleExpr,
    filter: &WebTraceFilter,
) {
    if let Some(start_time) = filter.start_time {
        stmt.and_where(timestamp_col.clone().gte(start_time));
    }
    if let Some(end_time) = filter.end_time {
        stmt.and_where(timestamp_col.clone().lte(end_time));
    }
    if let Some(cursor) = filter.cursor {
        stmt.and_where(timestamp_col.gt(cursor));
    }
}

fn build_top_player_growths(
    top_rows: &[RankingPageRow],
    rows: Vec<PlayerGrowthRow>,
    character_id: Option<i64>,
) -> Vec<TopRankingPlayerGrowthSchema> {
    top_rows
        .iter()
        .filter_map(|top| {
            build_top_player_growth(
                top.user_id_key(),
                top.user_id(),
                top.rank(),
                top.score(),
                top.timestamp(),
                character_id,
                &rows,
            )
        })
        .collect()
}

fn build_wb_top_player_growths(
    top_rows: &[WorldBloomRankingPageRow],
    rows: Vec<PlayerGrowthRow>,
    character_id: Option<i64>,
) -> Vec<TopRankingPlayerGrowthSchema> {
    top_rows
        .iter()
        .filter_map(|top| {
            build_top_player_growth(
                top.user_id_key(),
                top.user_id(),
                top.rank(),
                top.score(),
                top.timestamp(),
                character_id.or_else(|| top.character_id()),
                &rows,
            )
        })
        .collect()
}

fn build_top_player_growth(
    user_id_key: i64,
    user_id: &str,
    latest_rank: i64,
    latest_score: i64,
    latest_timestamp: i64,
    character_id: Option<i64>,
    rows: &[PlayerGrowthRow],
) -> Option<TopRankingPlayerGrowthSchema> {
    let mut earlier: Option<&PlayerGrowthRow> = None;
    let mut has_distinct_latest = false;
    for row in rows.iter().filter(|row| row.user_id_key == user_id_key) {
        if row.timestamp < latest_timestamp {
            earlier.get_or_insert(row);
            has_distinct_latest = true;
        }
    }
    let earlier = earlier?;
    if !has_distinct_latest || earlier.timestamp == latest_timestamp {
        return None;
    }
    Some(TopRankingPlayerGrowthSchema {
        rank: latest_rank,
        user_id: user_id.to_owned(),
        score_latest: latest_score,
        timestamp_latest: latest_timestamp,
        score_earlier: earlier.score,
        timestamp_earlier: earlier.timestamp,
        time_diff: latest_timestamp - earlier.timestamp,
        growth: latest_score - earlier.score,
        character_id,
    })
}

#[tracing::instrument(skip(engine, filter), fields(event_id))]
pub async fn search_users(
    engine: &DatabaseEngine,
    event_id: i64,
    filter: &WebUserSearchFilter,
    mode: PublicUserIdMode,
) -> Result<(Vec<RecordedUserNameSchema>, Option<i64>), DbErr> {
    let table = Alias::new(intern(TableKind::EventUsers, event_id));
    let mut stmt = Query::select()
        .expr_as(Expr::col(mode.output_column()), Alias::new("user_id"))
        .column(event_users::Column::Name)
        .column(event_users::Column::CheerfulTeamId)
        .column(event_users::Column::CardId)
        .column(event_users::Column::CardLevel)
        .column(event_users::Column::CardMasterRank)
        .column(event_users::Column::CardSpecialTrainingStatus)
        .column(event_users::Column::CardDefaultImage)
        .column(event_users::Column::ProfileWord)
        .column(event_users::Column::ProfileHonorsJson)
        .column(event_users::Column::HonorMissionsJson)
        .column(event_users::Column::PlayerFramesJson)
        .column(event_users::Column::UserIdKey)
        .from(table)
        .to_owned();

    if let Some(unique_id) = filter.unique_id.as_deref() {
        stmt.and_where(Expr::col(event_users::Column::UniqueId).eq(unique_id));
    }
    if let Some(name) = filter.name.as_deref() {
        stmt.and_where(Expr::col(event_users::Column::Name).like(format!("%{name}%")));
    }
    if let Some(profile_word) = filter.profile_word.as_deref() {
        stmt.and_where(
            Expr::col(event_users::Column::ProfileWord).like(format!("%{profile_word}%")),
        );
    }
    if let Some(card_id) = filter.card_id {
        stmt.and_where(Expr::col(event_users::Column::CardId).eq(card_id));
    }
    if let Some(card_level) = filter.card_level {
        stmt.and_where(Expr::col(event_users::Column::CardLevel).eq(card_level));
    }
    if let Some(card_master_rank) = filter.card_master_rank {
        stmt.and_where(Expr::col(event_users::Column::CardMasterRank).eq(card_master_rank));
    }
    if let Some(status) = filter.card_special_training_status.as_deref() {
        stmt.and_where(Expr::col(event_users::Column::CardSpecialTrainingStatus).eq(status));
    }
    if let Some(image) = filter.card_default_image.as_deref() {
        stmt.and_where(Expr::col(event_users::Column::CardDefaultImage).eq(image));
    }
    if let Some(team) = filter.cheerful_team_id {
        stmt.and_where(Expr::col(event_users::Column::CheerfulTeamId).eq(team));
    }
    if let Some(cursor) = filter.cursor {
        stmt.and_where(Expr::col(event_users::Column::UserIdKey).gt(cursor));
    }

    stmt.order_by(event_users::Column::UserIdKey, Order::Asc)
        .limit(filter.limit + 1);

    let backend = engine.backend();
    let mut rows = UserSearchRow::find_by_statement(backend.build(&stmt))
        .all(engine.conn())
        .await?;
    let next_cursor = if rows.len() > filter.limit as usize {
        rows.pop().map(|row| row.user_id_key)
    } else {
        None
    };
    Ok((
        rows.into_iter().map(UserSearchRow::into_schema).collect(),
        next_cursor,
    ))
}

#[derive(Debug, FromQueryResult)]
pub struct UserSearchRow {
    pub user_id: String,
    pub name: String,
    pub cheerful_team_id: Option<i64>,
    pub card_id: Option<i64>,
    pub card_level: Option<i64>,
    pub card_master_rank: Option<i64>,
    pub card_special_training_status: Option<String>,
    pub card_default_image: Option<String>,
    pub profile_word: Option<String>,
    pub profile_honors_json: Option<String>,
    pub honor_missions_json: Option<String>,
    pub player_frames_json: Option<String>,
    pub user_id_key: i64,
}

impl UserSearchRow {
    fn into_schema(self) -> RecordedUserNameSchema {
        RecordedUserNameSchema {
            user_id: self.user_id,
            name: self.name,
            cheerful_team_id: self.cheerful_team_id,
            card_id: self.card_id,
            card_level: self.card_level,
            card_master_rank: self.card_master_rank,
            card_special_training_status: self.card_special_training_status,
            card_default_image: self.card_default_image,
            profile_word: self.profile_word,
            profile_honors: parse_json_array(self.profile_honors_json.as_deref()),
            user_honor_missions: parse_json_array(self.honor_missions_json.as_deref()),
            user_player_frames: parse_json_array(self.player_frames_json.as_deref()),
        }
    }
}

fn parse_json_array<T>(raw: Option<&str>) -> Vec<T>
where
    T: serde::de::DeserializeOwned,
{
    raw.and_then(|s| sonic_rs::from_str(s).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::{ConnectionTrait, Database, DatabaseBackend, Statement};

    use crate::db::engine::DatabaseEngine;
    use crate::db::query::growth::fetch_ranking_score_growths;
    use crate::db::schema::create_event_tables;
    use crate::model::enums::SekaiServerRegion;

    #[test]
    fn ranking_cursor_round_trip_parts() {
        let cursor = WebRankingCursor {
            timestamp: 100,
            rank: 2,
            user_id_key: 30,
        };
        assert_eq!(cursor.timestamp, 100);
        assert_eq!(cursor.rank, 2);
        assert_eq!(cursor.user_id_key, 30);
    }

    #[tokio::test]
    async fn web_ranking_search_paginates_and_projects_public_ids() {
        let engine = sqlite_engine().await;
        let event_id = 551;
        create_event_tables(&engine, SekaiServerRegion::Jp, event_id, false)
            .await
            .unwrap();
        seed_normal_event(&engine, event_id).await;

        let filter = WebRankingFilter {
            rank_min: Some(1),
            rank_max: Some(2),
            score_min: None,
            score_max: None,
            start_time: None,
            end_time: None,
            before: None,
            after: None,
            timestamp: Some(1_710_000_000),
            cursor: None,
            limit: 1,
        };
        let (items, cursor) = search_rankings(&engine, event_id, &filter, PublicUserIdMode::Unique)
            .await
            .unwrap();

        assert_eq!(items.len(), 1);
        assert!(cursor.is_some());
        let item = &items[0];
        let RecordedRankData::Normal(row) = &item.rank_data else {
            panic!("expected normal ranking");
        };
        assert_eq!(row.user_id, "u-public-1");
        assert_eq!(row.rank, 1);
        let user = item.user_data.as_ref().expect("expected ranking user data");
        assert_eq!(user.user_id, "u-public-1");
        assert_eq!(user.name, "Alpha");
        assert_eq!(user.card_id, Some(1404));
        assert_eq!(user.card_level, Some(60));
        assert_eq!(user.profile_honors[0].honor_id, Some(95));
        assert_eq!(user.user_player_frames[0].player_frame_id, Some(10050));
    }

    #[tokio::test]
    async fn web_ranking_window_returns_latest_row_per_rank() {
        let engine = sqlite_engine().await;
        let event_id = 553;
        create_event_tables(&engine, SekaiServerRegion::Jp, event_id, false)
            .await
            .unwrap();
        seed_normal_event_with_history(&engine, event_id).await;

        let filter = WebRankingFilter {
            rank_min: Some(1),
            rank_max: Some(3),
            score_min: None,
            score_max: None,
            start_time: None,
            end_time: None,
            before: None,
            after: None,
            timestamp: None,
            cursor: None,
            limit: 10,
        };
        let (items, cursor) = search_rankings(&engine, event_id, &filter, PublicUserIdMode::Unique)
            .await
            .unwrap();

        assert!(cursor.is_none());
        let rows: Vec<_> = items
            .into_iter()
            .map(|item| match item {
                WebRankingItemSchema {
                    rank_data: RecordedRankData::Normal(row),
                    user_data,
                } => {
                    assert!(user_data.is_some());
                    row
                }
                WebRankingItemSchema {
                    rank_data: RecordedRankData::WorldBloom(_),
                    ..
                } => panic!("expected normal ranking"),
            })
            .collect();
        assert_eq!(
            rows.iter().map(|row| row.rank).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(
            rows.iter().map(|row| row.timestamp).collect::<Vec<_>>(),
            vec![1_710_000_060, 1_710_000_060, 1_710_000_060]
        );
        assert_eq!(
            rows.iter().map(|row| row.score).collect::<Vec<_>>(),
            vec![1300, 1200, 1100]
        );
    }

    #[tokio::test]
    async fn top_player_growth_follows_current_player_not_rank() {
        let engine = sqlite_engine().await;
        let event_id = 557;
        create_event_tables(&engine, SekaiServerRegion::Jp, event_id, false)
            .await
            .unwrap();
        seed_normal_event_with_history(&engine, event_id).await;

        let filter = WebRankingFilter {
            rank_min: Some(1),
            rank_max: Some(3),
            score_min: None,
            score_max: None,
            start_time: None,
            end_time: None,
            before: None,
            after: None,
            timestamp: None,
            cursor: None,
            limit: 10,
        };
        let (top_rows, _) =
            search_ranking_rows(&engine, event_id, &filter, PublicUserIdMode::Unique)
                .await
                .unwrap();
        let growths = fetch_top_player_growths(&engine, event_id, &top_rows, 1_710_000_000, None)
            .await
            .unwrap();

        assert_eq!(growths.len(), 3);
        assert_eq!(
            growths
                .iter()
                .map(|growth| (growth.rank, growth.user_id.as_str(), growth.growth))
                .collect::<Vec<_>>(),
            vec![
                (1, "u-public-1", 300),
                (2, "u-public-2", 300),
                (3, "u-public-3", 300)
            ]
        );
    }

    #[tokio::test]
    async fn search_rank_trace_follows_rank_not_current_player() {
        let engine = sqlite_engine().await;
        let event_id = 559;
        create_event_tables(&engine, SekaiServerRegion::Jp, event_id, false)
            .await
            .unwrap();
        seed_normal_event_with_rank_changes(&engine, event_id).await;

        let filter = WebTraceFilter {
            start_time: None,
            end_time: None,
            cursor: None,
            limit: Some(10),
        };
        let trace = search_rank_trace(&engine, event_id, 2, &filter, PublicUserIdMode::Unique)
            .await
            .unwrap();

        let rows = trace
            .into_iter()
            .map(|rank_data| match rank_data {
                RecordedRankData::Normal(row) => row,
                RecordedRankData::WorldBloom(_) => panic!("expected normal ranking"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            rows.iter()
                .map(|row| (row.timestamp, row.user_id.as_str(), row.rank, row.score))
                .collect::<Vec<_>>(),
            vec![
                (1_710_000_000, "u-public-2", 2, 900),
                (1_710_000_060, "u-public-1", 2, 1500),
            ]
        );
    }

    #[tokio::test]
    async fn search_world_bloom_rank_trace_follows_rank_not_current_player() {
        let engine = sqlite_engine().await;
        let event_id = 560;
        create_event_tables(&engine, SekaiServerRegion::Jp, event_id, true)
            .await
            .unwrap();
        seed_world_bloom_event_with_rank_changes(&engine, event_id).await;

        let filter = WebTraceFilter {
            start_time: None,
            end_time: None,
            cursor: None,
            limit: Some(10),
        };
        let trace = search_world_bloom_rank_trace(
            &engine,
            event_id,
            17,
            2,
            &filter,
            PublicUserIdMode::Unique,
        )
        .await
        .unwrap();

        let rows = trace
            .into_iter()
            .map(|rank_data| match rank_data {
                RecordedRankData::WorldBloom(row) => row,
                RecordedRankData::Normal(_) => panic!("expected world bloom ranking"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            rows.iter()
                .map(|row| {
                    (
                        row.timestamp,
                        row.user_id.as_str(),
                        row.rank,
                        row.score,
                        row.character_id,
                    )
                })
                .collect::<Vec<_>>(),
            vec![
                (1_710_000_000, "u-public-2", 2, 1900, Some(17)),
                (1_710_000_060, "u-public-1", 2, 2500, Some(17)),
            ]
        );
    }

    #[tokio::test]
    async fn ranking_score_growth_respects_replay_end_time() {
        let engine = sqlite_engine().await;
        let event_id = 558;
        create_event_tables(&engine, SekaiServerRegion::Jp, event_id, false)
            .await
            .unwrap();
        seed_normal_event_with_history(&engine, event_id).await;
        let time_tbl = intern(TableKind::TimeId, event_id);
        let event_tbl = intern(TableKind::Event, event_id);
        for sql in [
            format!("INSERT INTO {time_tbl} (timestamp, status) VALUES (1710000120, 0)"),
            format!(
                "INSERT INTO {event_tbl} (time_id, user_id_key, score, rank) VALUES \
                (3, 1, 5000, 1), (3, 2, 4900, 2), (3, 3, 4800, 3)"
            ),
        ] {
            engine
                .conn()
                .execute_raw(Statement::from_string(DatabaseBackend::Sqlite, sql))
                .await
                .unwrap();
        }

        let growths = fetch_ranking_score_growths(
            &engine,
            event_id,
            &[1],
            1_710_000_000,
            Some(1_710_000_060),
        )
        .await
        .unwrap();

        assert_eq!(growths.len(), 1);
        assert_eq!(growths[0].timestamp_latest, 1_710_000_060);
        assert_eq!(growths[0].score_latest, 1300);
        assert_eq!(growths[0].growth, Some(300));
    }

    #[tokio::test]
    async fn web_ranking_window_timestamp_uses_latest_snapshot_before_cutoff() {
        let engine = sqlite_engine().await;
        let event_id = 555;
        create_event_tables(&engine, SekaiServerRegion::Jp, event_id, false)
            .await
            .unwrap();
        seed_normal_event_with_history(&engine, event_id).await;

        let filter = WebRankingFilter {
            rank_min: Some(1),
            rank_max: Some(3),
            score_min: None,
            score_max: None,
            start_time: None,
            end_time: None,
            before: None,
            after: None,
            timestamp: Some(1_710_000_030),
            cursor: None,
            limit: 10,
        };
        let (items, cursor) = search_rankings(&engine, event_id, &filter, PublicUserIdMode::Unique)
            .await
            .unwrap();

        assert!(cursor.is_none());
        let rows: Vec<_> = items
            .into_iter()
            .map(|item| match item {
                WebRankingItemSchema {
                    rank_data: RecordedRankData::Normal(row),
                    ..
                } => row,
                WebRankingItemSchema {
                    rank_data: RecordedRankData::WorldBloom(_),
                    ..
                } => panic!("expected normal ranking"),
            })
            .collect();
        assert_eq!(
            rows.iter().map(|row| row.timestamp).collect::<Vec<_>>(),
            vec![1_710_000_000, 1_710_000_000, 1_710_000_000]
        );
        assert_eq!(
            rows.iter().map(|row| row.score).collect::<Vec<_>>(),
            vec![1000, 900, 800]
        );
    }

    #[tokio::test]
    async fn web_world_bloom_window_returns_latest_row_per_rank() {
        let engine = sqlite_engine().await;
        let event_id = 554;
        create_event_tables(&engine, SekaiServerRegion::Jp, event_id, true)
            .await
            .unwrap();
        seed_world_bloom_event_with_history(&engine, event_id).await;

        let filter = WebRankingFilter {
            rank_min: Some(1),
            rank_max: Some(3),
            score_min: None,
            score_max: None,
            start_time: None,
            end_time: None,
            before: None,
            after: None,
            timestamp: None,
            cursor: None,
            limit: 10,
        };
        let (items, cursor) =
            search_world_bloom_rankings(&engine, event_id, 17, &filter, PublicUserIdMode::Unique)
                .await
                .unwrap();

        assert!(cursor.is_none());
        let rows: Vec<_> = items
            .into_iter()
            .map(|item| match item {
                WebRankingItemSchema {
                    rank_data: RecordedRankData::WorldBloom(row),
                    user_data,
                } => {
                    assert!(user_data.is_some());
                    row
                }
                WebRankingItemSchema {
                    rank_data: RecordedRankData::Normal(_),
                    ..
                } => panic!("expected world bloom ranking"),
            })
            .collect();
        assert_eq!(
            rows.iter().map(|row| row.rank).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(
            rows.iter().map(|row| row.timestamp).collect::<Vec<_>>(),
            vec![1_710_000_060, 1_710_000_060, 1_710_000_060]
        );
        assert_eq!(
            rows.iter().map(|row| row.score).collect::<Vec<_>>(),
            vec![2300, 2200, 2100]
        );
    }

    #[tokio::test]
    async fn world_bloom_top_player_growth_filters_character() {
        let engine = sqlite_engine().await;
        let event_id = 558;
        create_event_tables(&engine, SekaiServerRegion::Jp, event_id, true)
            .await
            .unwrap();
        seed_world_bloom_event_with_history(&engine, event_id).await;

        let filter = WebRankingFilter {
            rank_min: Some(1),
            rank_max: Some(3),
            score_min: None,
            score_max: None,
            start_time: None,
            end_time: None,
            before: None,
            after: None,
            timestamp: None,
            cursor: None,
            limit: 10,
        };
        let (top_rows, _) = search_world_bloom_ranking_rows(
            &engine,
            event_id,
            17,
            &filter,
            PublicUserIdMode::Unique,
        )
        .await
        .unwrap();
        let growths = fetch_world_bloom_top_player_growths(
            &engine,
            event_id,
            17,
            &top_rows,
            1_710_000_000,
            None,
        )
        .await
        .unwrap();

        assert_eq!(growths.len(), 3);
        assert!(growths.iter().all(|growth| growth.character_id == Some(17)));
        assert_eq!(
            growths
                .iter()
                .map(|growth| (growth.rank, growth.user_id.as_str(), growth.growth))
                .collect::<Vec<_>>(),
            vec![
                (1, "u-public-1", 300),
                (2, "u-public-2", 300),
                (3, "u-public-3", 300)
            ]
        );
    }

    #[tokio::test]
    async fn web_world_bloom_window_timestamp_uses_latest_snapshot_before_cutoff() {
        let engine = sqlite_engine().await;
        let event_id = 556;
        create_event_tables(&engine, SekaiServerRegion::Jp, event_id, true)
            .await
            .unwrap();
        seed_world_bloom_event_with_history(&engine, event_id).await;

        let filter = WebRankingFilter {
            rank_min: Some(1),
            rank_max: Some(3),
            score_min: None,
            score_max: None,
            start_time: None,
            end_time: None,
            before: None,
            after: None,
            timestamp: Some(1_710_000_030),
            cursor: None,
            limit: 10,
        };
        let (items, cursor) =
            search_world_bloom_rankings(&engine, event_id, 17, &filter, PublicUserIdMode::Unique)
                .await
                .unwrap();

        assert!(cursor.is_none());
        let rows: Vec<_> = items
            .into_iter()
            .map(|item| match item {
                WebRankingItemSchema {
                    rank_data: RecordedRankData::WorldBloom(row),
                    ..
                } => row,
                WebRankingItemSchema {
                    rank_data: RecordedRankData::Normal(_),
                    ..
                } => panic!("expected world bloom ranking"),
            })
            .collect();
        assert_eq!(
            rows.iter().map(|row| row.timestamp).collect::<Vec<_>>(),
            vec![1_710_000_000, 1_710_000_000, 1_710_000_000]
        );
        assert_eq!(
            rows.iter().map(|row| row.score).collect::<Vec<_>>(),
            vec![2000, 1900, 1800]
        );
    }

    #[tokio::test]
    async fn web_user_search_filters_profile_fields() {
        let engine = sqlite_engine().await;
        let event_id = 552;
        create_event_tables(&engine, SekaiServerRegion::Jp, event_id, false)
            .await
            .unwrap();
        seed_normal_event(&engine, event_id).await;

        let filter = WebUserSearchFilter {
            unique_id: None,
            name: Some("Alpha".into()),
            profile_word: Some("hello".into()),
            card_id: Some(1404),
            card_level: None,
            card_master_rank: None,
            card_special_training_status: None,
            card_default_image: None,
            cheerful_team_id: None,
            cursor: None,
            limit: 10,
        };
        let (items, cursor) = search_users(&engine, event_id, &filter, PublicUserIdMode::Unique)
            .await
            .unwrap();

        assert_eq!(items.len(), 1);
        assert!(cursor.is_none());
        assert_eq!(items[0].user_id, "u-public-1");
        assert_eq!(items[0].profile_word.as_deref(), Some("hello world"));
        assert_eq!(items[0].profile_honors[0].honor_id, Some(95));
    }

    async fn sqlite_engine() -> DatabaseEngine {
        let conn = Database::connect("sqlite::memory:").await.unwrap();
        DatabaseEngine::from_connection(conn, DatabaseBackend::Sqlite)
    }

    async fn seed_normal_event(engine: &DatabaseEngine, event_id: i64) {
        let users_tbl = intern(TableKind::EventUsers, event_id);
        let time_tbl = intern(TableKind::TimeId, event_id);
        let event_tbl = intern(TableKind::Event, event_id);
        for sql in [
            format!(
                "INSERT INTO {users_tbl} \
                (user_id, unique_id, name, cheerful_team_id, card_id, card_level, \
                card_master_rank, card_special_training_status, card_default_image, \
                profile_word, profile_honors_json, player_frames_json) VALUES \
                ('100', 'u-public-1', 'Alpha', NULL, 1404, 60, 5, 'done', 'original', \
                'hello world', '[{{\"seq\":1,\"profileHonorType\":\"normal\",\"honorId\":95,\"honorLevel\":1,\"bondsHonorViewType\":\"none\",\"bondsHonorWordId\":0}}]', \
                '[{{\"playerFrameId\":10050,\"playerFrameAttachStatus\":\"first\"}}]'), \
                ('200', 'u-public-2', 'Beta', NULL, 1300, 50, 0, 'none', 'original', \
                'other word', '[]', '[]')"
            ),
            format!("INSERT INTO {time_tbl} (timestamp, status) VALUES (1710000000, 0)"),
            format!(
                "INSERT INTO {event_tbl} (time_id, user_id_key, score, rank) \
                VALUES (1, 1, 1000, 1), (1, 2, 900, 2)"
            ),
        ] {
            engine
                .conn()
                .execute_raw(Statement::from_string(DatabaseBackend::Sqlite, sql))
                .await
                .unwrap();
        }
    }

    async fn seed_normal_event_with_history(engine: &DatabaseEngine, event_id: i64) {
        let users_tbl = intern(TableKind::EventUsers, event_id);
        let time_tbl = intern(TableKind::TimeId, event_id);
        let event_tbl = intern(TableKind::Event, event_id);
        for sql in [
            format!(
                "INSERT INTO {users_tbl} \
                (user_id, unique_id, name, cheerful_team_id, card_id, card_level, \
                card_master_rank, card_special_training_status, card_default_image, \
                profile_word, profile_honors_json, player_frames_json) VALUES \
                ('100', 'u-public-1', 'Alpha', NULL, 1404, 60, 5, 'done', 'original', \
                'hello world', '[]', '[]'), \
                ('200', 'u-public-2', 'Beta', NULL, 1300, 50, 0, 'none', 'original', \
                'other word', '[]', '[]'), \
                ('300', 'u-public-3', 'Gamma', NULL, 1200, 50, 0, 'none', 'original', \
                'third word', '[]', '[]')"
            ),
            format!(
                "INSERT INTO {time_tbl} (timestamp, status) VALUES \
                (1710000000, 0), (1710000060, 0)"
            ),
            format!(
                "INSERT INTO {event_tbl} (time_id, user_id_key, score, rank) VALUES \
                (1, 1, 1000, 1), (1, 2, 900, 2), (1, 3, 800, 3), \
                (2, 1, 1300, 1), (2, 2, 1200, 2), (2, 3, 1100, 3)"
            ),
        ] {
            engine
                .conn()
                .execute_raw(Statement::from_string(DatabaseBackend::Sqlite, sql))
                .await
                .unwrap();
        }
    }

    async fn seed_normal_event_with_rank_changes(engine: &DatabaseEngine, event_id: i64) {
        let users_tbl = intern(TableKind::EventUsers, event_id);
        let time_tbl = intern(TableKind::TimeId, event_id);
        let event_tbl = intern(TableKind::Event, event_id);
        for sql in [
            format!(
                "INSERT INTO {users_tbl} \
                (user_id, unique_id, name, cheerful_team_id, card_id, card_level, \
                card_master_rank, card_special_training_status, card_default_image, \
                profile_word, profile_honors_json, player_frames_json) VALUES \
                ('100', 'u-public-1', 'Alpha', NULL, 1404, 60, 5, 'done', 'original', \
                'hello world', '[]', '[]'), \
                ('200', 'u-public-2', 'Beta', NULL, 1300, 50, 0, 'none', 'original', \
                'other word', '[]', '[]')"
            ),
            format!(
                "INSERT INTO {time_tbl} (timestamp, status) VALUES \
                (1710000000, 0), (1710000060, 0)"
            ),
            format!(
                "INSERT INTO {event_tbl} (time_id, user_id_key, score, rank) VALUES \
                (1, 1, 1000, 1), (1, 2, 900, 2), \
                (2, 2, 1600, 1), (2, 1, 1500, 2)"
            ),
        ] {
            engine
                .conn()
                .execute_raw(Statement::from_string(DatabaseBackend::Sqlite, sql))
                .await
                .unwrap();
        }
    }

    async fn seed_world_bloom_event_with_history(engine: &DatabaseEngine, event_id: i64) {
        let users_tbl = intern(TableKind::EventUsers, event_id);
        let time_tbl = intern(TableKind::TimeId, event_id);
        let wl_tbl = intern(TableKind::WorldBloom, event_id);
        for sql in [
            format!(
                "INSERT INTO {users_tbl} \
                (user_id, unique_id, name, cheerful_team_id, card_id, card_level, \
                card_master_rank, card_special_training_status, card_default_image, \
                profile_word, profile_honors_json, player_frames_json) VALUES \
                ('100', 'u-public-1', 'Alpha', NULL, 1404, 60, 5, 'done', 'original', \
                'hello world', '[]', '[]'), \
                ('200', 'u-public-2', 'Beta', NULL, 1300, 50, 0, 'none', 'original', \
                'other word', '[]', '[]'), \
                ('300', 'u-public-3', 'Gamma', NULL, 1200, 50, 0, 'none', 'original', \
                'third word', '[]', '[]')"
            ),
            format!(
                "INSERT INTO {time_tbl} (timestamp, status) VALUES \
                (1710000000, 0), (1710000060, 0)"
            ),
            format!(
                "INSERT INTO {wl_tbl} (time_id, user_id_key, character_id, score, rank) VALUES \
                (1, 1, 17, 2000, 1), (1, 2, 17, 1900, 2), (1, 3, 17, 1800, 3), \
                (2, 1, 17, 2300, 1), (2, 2, 17, 2200, 2), (2, 3, 17, 2100, 3), \
                (2, 1, 19, 9000, 1)"
            ),
        ] {
            engine
                .conn()
                .execute_raw(Statement::from_string(DatabaseBackend::Sqlite, sql))
                .await
                .unwrap();
        }
    }

    async fn seed_world_bloom_event_with_rank_changes(engine: &DatabaseEngine, event_id: i64) {
        let users_tbl = intern(TableKind::EventUsers, event_id);
        let time_tbl = intern(TableKind::TimeId, event_id);
        let wl_tbl = intern(TableKind::WorldBloom, event_id);
        for sql in [
            format!(
                "INSERT INTO {users_tbl} \
                (user_id, unique_id, name, cheerful_team_id, card_id, card_level, \
                card_master_rank, card_special_training_status, card_default_image, \
                profile_word, profile_honors_json, player_frames_json) VALUES \
                ('100', 'u-public-1', 'Alpha', NULL, 1404, 60, 5, 'done', 'original', \
                'hello world', '[]', '[]'), \
                ('200', 'u-public-2', 'Beta', NULL, 1300, 50, 0, 'none', 'original', \
                'other word', '[]', '[]')"
            ),
            format!(
                "INSERT INTO {time_tbl} (timestamp, status) VALUES \
                (1710000000, 0), (1710000060, 0)"
            ),
            format!(
                "INSERT INTO {wl_tbl} (time_id, user_id_key, character_id, score, rank) VALUES \
                (1, 1, 17, 2000, 1), (1, 2, 17, 1900, 2), \
                (2, 2, 17, 2600, 1), (2, 1, 17, 2500, 2), \
                (1, 1, 19, 9000, 1), (2, 1, 19, 9100, 1)"
            ),
        ] {
            engine
                .conn()
                .execute_raw(Statement::from_string(DatabaseBackend::Sqlite, sql))
                .await
                .unwrap();
        }
    }
}
