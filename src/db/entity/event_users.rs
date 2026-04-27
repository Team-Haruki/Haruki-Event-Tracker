//! `event_<id>_users` table — per-event user dimension.
//!
//! `user_id` (the upstream Sekai user id, decimal-string in Go) is
//! deduplicated to the autoincremented `user_id_key`, which is what the
//! ranking tables actually reference. `name` and `cheerful_team_id` are
//! refreshed in place when the user updates them.
//!
//! Column types mirror `EventUsersTable` in `utils/gorm/tables.go`:
//! `user_id varchar(30)`, `name varchar(300)`, `cheerful_team_id` nullable.

use sea_orm::entity::prelude::*;

#[derive(Copy, Clone, Default, Debug, DeriveEntity)]
pub struct Entity {
    pub table_name: &'static str,
}

impl EntityName for Entity {
    fn table_name(&self) -> &str {
        self.table_name
    }
}

#[derive(Clone, Debug, PartialEq, Eq, DeriveModel, DeriveActiveModel)]
pub struct Model {
    pub user_id_key: i64,
    pub user_id: String,
    pub name: String,
    pub cheerful_team_id: Option<i64>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveColumn)]
pub enum Column {
    UserIdKey,
    UserId,
    Name,
    CheerfulTeamId,
}

impl ColumnTrait for Column {
    type EntityName = Entity;
    fn def(&self) -> ColumnDef {
        match self {
            Self::UserIdKey => ColumnType::BigInteger.def(),
            Self::UserId => ColumnType::String(StringLen::N(30)).def().unique(),
            Self::Name => ColumnType::String(StringLen::N(300)).def(),
            Self::CheerfulTeamId => ColumnType::BigInteger.def().nullable(),
        }
    }
}

#[derive(Copy, Clone, Debug, EnumIter, DerivePrimaryKey)]
pub enum PrimaryKey {
    UserIdKey,
}

impl PrimaryKeyTrait for PrimaryKey {
    type ValueType = i64;
    fn auto_increment() -> bool {
        true
    }
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
