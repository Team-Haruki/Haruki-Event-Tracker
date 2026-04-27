//! `event_<id>_users` lookups (Go: `GetUserData`).

use sea_orm::sea_query::{Alias, Expr, Query};
use sea_orm::{DbErr, FromQueryResult};

use crate::db::engine::DatabaseEngine;
use crate::db::entity::event_users;
use crate::db::table_name::{TableKind, intern};
use crate::model::api::RecordedUserNameSchema;

#[tracing::instrument(skip(engine), fields(event_id, user_id = %user_id))]
pub async fn get_user_data(
    engine: &DatabaseEngine,
    event_id: i64,
    user_id: &str,
) -> Result<Option<RecordedUserNameSchema>, DbErr> {
    let table = Alias::new(intern(TableKind::EventUsers, event_id));
    let stmt = Query::select()
        .columns([
            event_users::Column::UserId,
            event_users::Column::Name,
            event_users::Column::CheerfulTeamId,
        ])
        .from(table)
        .and_where(Expr::col(event_users::Column::UserId).eq(user_id))
        .limit(1)
        .to_owned();

    let backend = engine.backend();
    RecordedUserNameSchema::find_by_statement(backend.build(&stmt))
        .one(engine.conn())
        .await
}
