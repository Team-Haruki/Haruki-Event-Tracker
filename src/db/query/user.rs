//! `event_<id>_users` lookups (Go: `GetUserData`).

use sea_orm::sea_query::{Alias, Expr, Query};
use sea_orm::{DbErr, ExprTrait, FromQueryResult};

use crate::db::engine::DatabaseEngine;
use crate::db::entity::event_users;
use crate::db::table_name::{TableKind, intern};
use crate::model::api::RecordedUserNameSchema;

#[derive(Debug, FromQueryResult)]
struct RecordedUserNameRow {
    user_id: String,
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

impl RecordedUserNameRow {
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

#[derive(Debug, Copy, Clone)]
pub enum PublicUserIdMode {
    Raw,
    Unique,
}

impl PublicUserIdMode {
    pub fn output_column(self) -> event_users::Column {
        match self {
            Self::Raw => event_users::Column::UserId,
            Self::Unique => event_users::Column::UniqueId,
        }
    }
}

#[tracing::instrument(skip(engine), fields(event_id, user_id = %user_id))]
pub async fn get_user_data(
    engine: &DatabaseEngine,
    event_id: i64,
    user_id: &str,
    mode: PublicUserIdMode,
) -> Result<Option<RecordedUserNameSchema>, DbErr> {
    let table = Alias::new(intern(TableKind::EventUsers, event_id));
    let stmt = Query::select()
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
        .from(table)
        .and_where(Expr::col(mode.output_column()).eq(user_id))
        .limit(1)
        .to_owned();

    let backend = engine.backend();
    Ok(RecordedUserNameRow::find_by_statement(backend.build(&stmt))
        .one(engine.conn())
        .await?
        .map(RecordedUserNameRow::into_schema))
}
