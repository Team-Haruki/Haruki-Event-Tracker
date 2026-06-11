use sea_orm::sea_query::{Alias, Expr, IntoCondition, Order, Query, SelectStatement, SimpleExpr};
use sea_orm::{DbErr, ExprTrait, FromQueryResult};

use crate::db::engine::DatabaseEngine;
use crate::db::entity::{event, event_users, time_id, world_bloom};
use crate::db::query::user::PublicUserIdMode;
use crate::db::table_name::{TableKind, intern};
use crate::model::api::{
    RecordedRankData, RecordedRankingSchema, RecordedUserNameSchema,
    RecordedWorldBloomRankingSchema,
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
    pub limit: u64,
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

#[derive(Debug, FromQueryResult)]
struct RankingPageRow {
    timestamp: i64,
    user_id: String,
    user_id_key: i64,
    score: i64,
    rank: i64,
}

#[derive(Debug, FromQueryResult)]
struct WorldBloomRankingPageRow {
    timestamp: i64,
    user_id: String,
    user_id_key: i64,
    score: i64,
    rank: i64,
    character_id: Option<i64>,
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
}

fn ranking_select(event_id: i64, mode: PublicUserIdMode) -> SelectStatement {
    let event_tbl = Alias::new(intern(TableKind::Event, event_id));
    let time_tbl = Alias::new(intern(TableKind::TimeId, event_id));
    let users_tbl = Alias::new(intern(TableKind::EventUsers, event_id));

    Query::select()
        .expr_as(
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

fn world_bloom_select(event_id: i64, mode: PublicUserIdMode) -> SelectStatement {
    let wl_tbl = Alias::new(intern(TableKind::WorldBloom, event_id));
    let time_tbl = Alias::new(intern(TableKind::TimeId, event_id));
    let users_tbl = Alias::new(intern(TableKind::EventUsers, event_id));

    Query::select()
        .expr_as(
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

#[tracing::instrument(skip(engine, filter), fields(event_id))]
pub async fn search_rankings(
    engine: &DatabaseEngine,
    event_id: i64,
    filter: &WebRankingFilter,
    mode: PublicUserIdMode,
) -> Result<(Vec<RecordedRankData>, Option<WebRankingCursor>), DbErr> {
    let event_tbl = Alias::new(intern(TableKind::Event, event_id));
    let time_tbl = Alias::new(intern(TableKind::TimeId, event_id));
    let users_tbl = Alias::new(intern(TableKind::EventUsers, event_id));
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
        .limit(filter.limit + 1);

    let backend = engine.backend();
    let mut rows = RankingPageRow::find_by_statement(backend.build(&stmt))
        .all(engine.conn())
        .await?;
    let next_cursor = if rows.len() > filter.limit as usize {
        rows.pop().map(|row| row.cursor())
    } else {
        None
    };
    Ok((
        rows.into_iter()
            .map(RankingPageRow::into_schema)
            .map(RecordedRankData::Normal)
            .collect(),
        next_cursor,
    ))
}

#[tracing::instrument(skip(engine, filter), fields(event_id, character_id))]
pub async fn search_world_bloom_rankings(
    engine: &DatabaseEngine,
    event_id: i64,
    character_id: i64,
    filter: &WebRankingFilter,
    mode: PublicUserIdMode,
) -> Result<(Vec<RecordedRankData>, Option<WebRankingCursor>), DbErr> {
    let wl_tbl = Alias::new(intern(TableKind::WorldBloom, event_id));
    let time_tbl = Alias::new(intern(TableKind::TimeId, event_id));
    let users_tbl = Alias::new(intern(TableKind::EventUsers, event_id));
    let mut stmt = world_bloom_select(event_id, mode);
    stmt.and_where(Expr::col((wl_tbl.clone(), world_bloom::Column::CharacterId)).eq(character_id));
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
        .limit(filter.limit + 1);

    let backend = engine.backend();
    let mut rows = WorldBloomRankingPageRow::find_by_statement(backend.build(&stmt))
        .all(engine.conn())
        .await?;
    let next_cursor = if rows.len() > filter.limit as usize {
        rows.pop().map(|row| row.cursor())
    } else {
        None
    };
    Ok((
        rows.into_iter()
            .map(WorldBloomRankingPageRow::into_schema)
            .map(RecordedRankData::WorldBloom)
            .collect(),
        next_cursor,
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
    stmt.order_by((time_tbl, time_id::Column::Timestamp), Order::Asc)
        .limit(filter.limit);

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
    stmt.order_by((time_tbl, time_id::Column::Timestamp), Order::Asc)
        .limit(filter.limit);

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
        let RecordedRankData::Normal(row) = &items[0] else {
            panic!("expected normal ranking");
        };
        assert_eq!(row.user_id, "u-public-1");
        assert_eq!(row.rank, 1);
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
}
