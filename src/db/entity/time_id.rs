//! `event_<id>_time_id` table.
//!
//! Every ranking row is timestamped indirectly via `time_id`; this table
//! deduplicates `(timestamp, status)` and gets joined back from `event_*`
//! and `wl_*` tables. `status=0` means a real ranking sample, `status=1+`
//! is a heartbeat written when the upstream API failed or nothing changed.
//!
//! Column names and types match the Go `TimeIDTable` (`utils/gorm/tables.go`).

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
    pub timestamp: i64,
    pub status: i8,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveColumn)]
pub enum Column {
    TimeId,
    Timestamp,
    Status,
}

impl ColumnTrait for Column {
    type EntityName = Entity;
    fn def(&self) -> ColumnDef {
        match self {
            Self::TimeId => ColumnType::BigInteger.def(),
            Self::Timestamp => ColumnType::BigInteger.def().unique(),
            Self::Status => ColumnType::TinyInteger.def().default(0),
        }
    }
}

#[derive(Copy, Clone, Debug, EnumIter, DerivePrimaryKey)]
pub enum PrimaryKey {
    TimeId,
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
