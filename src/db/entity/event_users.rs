//! `event_<id>_users` table — per-event user dimension.
//!
//! `user_id` (the upstream Sekai user id, decimal-string in Go) is
//! deduplicated to the autoincremented `user_id_key`, which is what the
//! ranking tables actually reference. `name` and `cheerful_team_id` are
//! refreshed in place when the user updates them.
//! `unique_id` is a public, salted one-way identifier used by the API when
//! UID anonymization is enabled.
//!
//! Column types mirror `EventUsersTable` in `utils/gorm/tables.go`:
//! `user_id varchar(30)`, `name varchar(300)`, `cheerful_team_id` nullable.

use sea_orm::entity::prelude::*;

#[derive(Copy, Clone, Default, Debug, DeriveEntity)]
pub struct Entity {
    pub table_name: &'static str,
}

impl EntityName for Entity {
    fn table_name(&self) -> &'static str {
        self.table_name
    }
}

#[derive(Clone, Debug, PartialEq, Eq, DeriveModel, DeriveActiveModel)]
pub struct Model {
    pub user_id_key: i64,
    pub user_id: String,
    pub unique_id: Option<String>,
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
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveColumn)]
pub enum Column {
    UserIdKey,
    UserId,
    UniqueId,
    Name,
    CheerfulTeamId,
    CardId,
    CardLevel,
    CardMasterRank,
    CardSpecialTrainingStatus,
    CardDefaultImage,
    ProfileWord,
    ProfileHonorsJson,
    HonorMissionsJson,
    PlayerFramesJson,
}

impl ColumnTrait for Column {
    type EntityName = Entity;
    fn def(&self) -> ColumnDef {
        match self {
            Self::UserIdKey => ColumnType::BigInteger.def(),
            Self::UserId => ColumnType::String(StringLen::N(30)).def().unique(),
            Self::UniqueId => ColumnType::String(StringLen::N(64))
                .def()
                .unique()
                .nullable(),
            Self::Name => ColumnType::String(StringLen::N(300)).def(),
            Self::CheerfulTeamId => ColumnType::BigInteger.def().nullable(),
            Self::CardId | Self::CardLevel | Self::CardMasterRank => {
                ColumnType::BigInteger.def().nullable()
            }
            Self::CardSpecialTrainingStatus | Self::CardDefaultImage => {
                ColumnType::String(StringLen::N(64)).def().nullable()
            }
            Self::ProfileWord => ColumnType::String(StringLen::N(300)).def().nullable(),
            Self::ProfileHonorsJson | Self::HonorMissionsJson | Self::PlayerFramesJson => {
                ColumnType::Text.def().nullable()
            }
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

#[cfg(test)]
mod tests {
    use sea_orm::sea_query::MysqlQueryBuilder;
    use sea_orm::{DatabaseBackend, Schema};

    use super::*;

    #[test]
    fn profile_json_columns_are_text_on_mysql() {
        let schema = Schema::new(DatabaseBackend::MySql);
        let sql = schema
            .create_table_from_entity(Entity {
                table_name: "event_208_users",
            })
            .to_string(MysqlQueryBuilder);

        assert!(
            sql.contains("`profile_honors_json` text"),
            "unexpected MySQL schema SQL: {sql}",
        );
        assert!(
            sql.contains("`player_frames_json` text"),
            "unexpected MySQL schema SQL: {sql}",
        );
        assert!(
            sql.contains("`honor_missions_json` text"),
            "unexpected MySQL schema SQL: {sql}",
        );
    }
}
