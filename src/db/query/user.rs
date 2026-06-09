//! `event_<id>_users` lookups (Go: `GetUserData`).

use sea_orm::sea_query::{Alias, Expr, Query};
use sea_orm::{DbErr, ExprTrait, FromQueryResult};

use crate::db::engine::DatabaseEngine;
use crate::db::entity::event_users;
use crate::db::table_name::{TableKind, intern};
use crate::model::api::RecordedUserNameSchema;

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
        .from(table)
        .and_where(Expr::col(mode.output_column()).eq(user_id))
        .limit(1)
        .to_owned();

    let backend = engine.backend();
    RecordedUserNameSchema::find_by_statement(backend.build(&stmt))
        .one(engine.conn())
        .await
}
