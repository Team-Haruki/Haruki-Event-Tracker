//! `event_<id>` table — top-100 ranking history (non-World-Bloom events).
//!
//! Composite PK `(time_id, user_id_key)`: each tracker tick writes at most
//! one row per user per timestamp, only when score or rank changed (the
//! rank-based diff lives in `tracker::diff`).

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
    pub time_id: i64,
    pub user_id_key: i64,
    pub score: i64,
    pub rank: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveColumn)]
pub enum Column {
    TimeId,
    UserIdKey,
    Score,
    Rank,
}

impl ColumnTrait for Column {
    type EntityName = Entity;
    fn def(&self) -> ColumnDef {
        match self {
            Self::TimeId | Self::UserIdKey | Self::Score | Self::Rank => {
                ColumnType::BigInteger.def()
            }
        }
    }
}

#[derive(Copy, Clone, Debug, EnumIter, DerivePrimaryKey)]
pub enum PrimaryKey {
    TimeId,
    UserIdKey,
}

impl PrimaryKeyTrait for PrimaryKey {
    type ValueType = (i64, i64);
    fn auto_increment() -> bool {
        false
    }
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
