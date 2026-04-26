//! Transactional batch inserts plus their two helper lookups
//! (Go: `BatchInsertEventRankings`, `BatchInsertWorldBloomRankings`,
//! `batchGetOrCreateTimeIDs`, `batchGetOrCreateUserIDKeys`).
//!
//! `batch_get_or_create_time_ids` and `batch_get_or_create_user_id_keys`
//! both execute inside the caller's transaction so the time-id /
//! user-id-key dimension rows and the ranking rows commit atomically.

use std::collections::{HashMap, HashSet};

use sea_orm::sea_query::{Alias, Expr, OnConflict, Query};
use sea_orm::{
    ConnectionTrait, DatabaseBackend, DatabaseTransaction, DbErr, FromQueryResult,
    TransactionError, TransactionTrait,
};

use crate::db::engine::DatabaseEngine;
use crate::db::entity::{event, event_users, time_id, world_bloom};
use crate::db::table_name::{TableKind, intern};
use crate::model::tracker::{
    PlayerEventRankingRecordSchema, PlayerState, PlayerWorldBloomRankingRecordSchema, WorldBloomKey,
};

#[derive(FromQueryResult)]
struct TimeIdRow {
    time_id: i64,
}

#[derive(FromQueryResult)]
struct UserKeyRow {
    user_id_key: i64,
    name: String,
    cheerful_team_id: Option<i64>,
}

/// Look up `time_id` per timestamp, inserting a new row with `status` when
/// the timestamp is not yet present. Returns a `timestamp -> time_id` map.
pub(crate) async fn batch_get_or_create_time_ids(
    tx: &DatabaseTransaction,
    backend: DatabaseBackend,
    table_name: &str,
    timestamps: &HashSet<i64>,
    status: i8,
) -> Result<HashMap<i64, i64>, DbErr> {
    let mut out = HashMap::with_capacity(timestamps.len());
    for &ts in timestamps {
        let sel = Query::select()
            .expr_as(Expr::col(time_id::Column::TimeId), Alias::new("time_id"))
            .from(Alias::new(table_name))
            .and_where(Expr::col(time_id::Column::Timestamp).eq(ts))
            .limit(1)
            .to_owned();

        if let Some(row) = TimeIdRow::find_by_statement(backend.build(&sel))
            .one(tx)
            .await?
        {
            out.insert(ts, row.time_id);
            continue;
        }

        let ins = Query::insert()
            .into_table(Alias::new(table_name))
            .columns([time_id::Column::Timestamp, time_id::Column::Status])
            .values_panic([ts.into(), status.into()])
            .to_owned();
        tx.execute(backend.build(&ins)).await?;

        let row = TimeIdRow::find_by_statement(backend.build(&sel))
            .one(tx)
            .await?
            .ok_or_else(|| {
                DbErr::Custom(format!(
                    "inserted time_id row vanished for timestamp={ts}"
                ))
            })?;
        out.insert(ts, row.time_id);
    }
    Ok(out)
}

#[derive(Debug, Clone)]
pub(crate) struct UserDimRow {
    pub name: String,
    pub cheerful_team_id: Option<i64>,
}

/// Look up `user_id_key` per `user_id`, inserting a new row when missing.
/// Updates `name` and/or `cheerful_team_id` in place when the upstream
/// payload disagrees with the stored row — matches Go's `Save` semantics.
pub(crate) async fn batch_get_or_create_user_id_keys(
    tx: &DatabaseTransaction,
    backend: DatabaseBackend,
    table_name: &str,
    users: &HashMap<String, UserDimRow>,
) -> Result<HashMap<String, i64>, DbErr> {
    let mut out = HashMap::with_capacity(users.len());
    for (user_id, info) in users {
        let sel = Query::select()
            .expr_as(
                Expr::col(event_users::Column::UserIdKey),
                Alias::new("user_id_key"),
            )
            .expr_as(Expr::col(event_users::Column::Name), Alias::new("name"))
            .expr_as(
                Expr::col(event_users::Column::CheerfulTeamId),
                Alias::new("cheerful_team_id"),
            )
            .from(Alias::new(table_name))
            .and_where(Expr::col(event_users::Column::UserId).eq(user_id.as_str()))
            .limit(1)
            .to_owned();

        if let Some(row) = UserKeyRow::find_by_statement(backend.build(&sel))
            .one(tx)
            .await?
        {
            let name_changed = row.name != info.name;
            let cheerful_changed = match (row.cheerful_team_id, info.cheerful_team_id) {
                (_, None) => false,
                (Some(stored), Some(new)) => stored != new,
                (None, Some(_)) => true,
            };
            if name_changed || cheerful_changed {
                let mut upd = Query::update();
                upd.table(Alias::new(table_name))
                    .value(event_users::Column::Name, info.name.clone())
                    .and_where(Expr::col(event_users::Column::UserIdKey).eq(row.user_id_key));
                if let Some(ct) = info.cheerful_team_id {
                    upd.value(event_users::Column::CheerfulTeamId, ct);
                }
                tx.execute(backend.build(&upd)).await?;
            }
            out.insert(user_id.clone(), row.user_id_key);
            continue;
        }

        let ins = Query::insert()
            .into_table(Alias::new(table_name))
            .columns([
                event_users::Column::UserId,
                event_users::Column::Name,
                event_users::Column::CheerfulTeamId,
            ])
            .values_panic([
                user_id.as_str().into(),
                info.name.clone().into(),
                info.cheerful_team_id.into(),
            ])
            .to_owned();
        tx.execute(backend.build(&ins)).await?;

        let row = UserKeyRow::find_by_statement(backend.build(&sel))
            .one(tx)
            .await?
            .ok_or_else(|| {
                DbErr::Custom(format!(
                    "inserted user_id_key row vanished for user_id={user_id}"
                ))
            })?;
        out.insert(user_id.clone(), row.user_id_key);
    }
    Ok(out)
}

/// Owned per-record fields we move into the transaction closure. Avoids the
/// HRTB lifetime trap where `for<'c> FnOnce(&'c Tx) -> ... + 'c` would force
/// any captured borrow to outlive `'static`.
struct OwnedRecord {
    timestamp: i64,
    user_id: String,
    score: i64,
    rank: i64,
}

struct OwnedWlRecord {
    timestamp: i64,
    user_id: String,
    character_id: i64,
    score: i64,
    rank: i64,
}

fn collect_dims<'a, I>(records: I) -> (HashSet<i64>, HashMap<String, UserDimRow>)
where
    I: Iterator<Item = (i64, &'a str, &'a str, Option<i64>)>,
{
    let mut timestamps = HashSet::new();
    let mut users: HashMap<String, UserDimRow> = HashMap::new();
    for (ts, user_id, name, cheerful_team_id) in records {
        timestamps.insert(ts);
        users.entry(user_id.to_string()).or_insert(UserDimRow {
            name: name.to_string(),
            cheerful_team_id,
        });
    }
    (timestamps, users)
}

#[tracing::instrument(skip(engine, records), fields(event_id, n = records.len()))]
pub async fn batch_insert_event_rankings(
    engine: &DatabaseEngine,
    event_id: i64,
    records: &[PlayerEventRankingRecordSchema],
) -> Result<(), DbErr> {
    if records.is_empty() {
        return Ok(());
    }
    let backend = engine.backend();
    let time_tbl = intern(TableKind::TimeId, event_id);
    let users_tbl = intern(TableKind::EventUsers, event_id);
    let event_tbl = intern(TableKind::Event, event_id);

    let (timestamps, users) = collect_dims(
        records
            .iter()
            .map(|r| (r.timestamp, r.user_id.as_str(), r.name.as_str(), r.cheerful_team_id)),
    );
    let owned: Vec<OwnedRecord> = records
        .iter()
        .map(|r| OwnedRecord {
            timestamp: r.timestamp,
            user_id: r.user_id.clone(),
            score: r.score,
            rank: r.rank,
        })
        .collect();

    engine
        .conn()
        .transaction::<_, (), DbErr>(move |tx| {
            Box::pin(async move {
                let time_lookup =
                    batch_get_or_create_time_ids(tx, backend, time_tbl, &timestamps, 0).await?;
                let user_lookup =
                    batch_get_or_create_user_id_keys(tx, backend, users_tbl, &users).await?;

                let mut ins = Query::insert();
                ins.into_table(Alias::new(event_tbl)).columns([
                    event::Column::TimeId,
                    event::Column::UserIdKey,
                    event::Column::Score,
                    event::Column::Rank,
                ]);
                for r in &owned {
                    let time_id_v = *time_lookup
                        .get(&r.timestamp)
                        .ok_or_else(|| DbErr::Custom("missing time_id lookup".into()))?;
                    let user_key_v = *user_lookup
                        .get(&r.user_id)
                        .ok_or_else(|| DbErr::Custom("missing user_id_key lookup".into()))?;
                    ins.values_panic([
                        time_id_v.into(),
                        user_key_v.into(),
                        r.score.into(),
                        r.rank.into(),
                    ]);
                }
                ins.on_conflict(
                    OnConflict::columns([event::Column::TimeId, event::Column::UserIdKey])
                        .do_nothing()
                        .to_owned(),
                );
                tx.execute(backend.build(&ins)).await?;
                Ok(())
            })
        })
        .await
        .map_err(unwrap_tx_err)
}

#[tracing::instrument(skip(engine, records, prev_state), fields(event_id, n = records.len()))]
pub async fn batch_insert_world_bloom_rankings(
    engine: &DatabaseEngine,
    event_id: i64,
    records: &[PlayerWorldBloomRankingRecordSchema],
    prev_state: &mut HashMap<WorldBloomKey, PlayerState>,
) -> Result<(), DbErr> {
    if records.is_empty() {
        return Ok(());
    }
    let backend = engine.backend();
    let time_tbl = intern(TableKind::TimeId, event_id);
    let users_tbl = intern(TableKind::EventUsers, event_id);
    let wl_tbl = intern(TableKind::WorldBloom, event_id);

    let (timestamps, users) = collect_dims(records.iter().map(|r| {
        (
            r.base.timestamp,
            r.base.user_id.as_str(),
            r.base.name.as_str(),
            r.base.cheerful_team_id,
        )
    }));
    let owned: Vec<OwnedWlRecord> = records
        .iter()
        .map(|r| OwnedWlRecord {
            timestamp: r.base.timestamp,
            user_id: r.base.user_id.clone(),
            character_id: r.character_id,
            score: r.base.score,
            rank: r.base.rank,
        })
        .collect();
    let owned_state = std::mem::take(prev_state);

    let updated_state = engine
        .conn()
        .transaction::<_, HashMap<WorldBloomKey, PlayerState>, DbErr>(move |tx| {
            Box::pin(async move {
                let mut state = owned_state;
                let time_lookup =
                    batch_get_or_create_time_ids(tx, backend, time_tbl, &timestamps, 0).await?;
                let user_lookup =
                    batch_get_or_create_user_id_keys(tx, backend, users_tbl, &users).await?;

                let mut changed: Vec<(i64, i64, i64, i64, i64)> = Vec::new();
                for r in &owned {
                    let user_key = *user_lookup
                        .get(&r.user_id)
                        .ok_or_else(|| DbErr::Custom("missing user_id_key lookup".into()))?;
                    let key = WorldBloomKey {
                        user_id_key: user_key,
                        character_id: r.character_id,
                    };
                    let last = state.get(&key).copied();
                    if last.is_none_or(|p| p.score != r.score || p.rank != r.rank) {
                        let time_id_v = *time_lookup
                            .get(&r.timestamp)
                            .ok_or_else(|| DbErr::Custom("missing time_id lookup".into()))?;
                        changed.push((time_id_v, user_key, r.character_id, r.score, r.rank));
                        state.insert(
                            key,
                            PlayerState {
                                score: r.score,
                                rank: r.rank,
                            },
                        );
                    }
                }

                if changed.is_empty() {
                    return Ok(state);
                }

                let mut ins = Query::insert();
                ins.into_table(Alias::new(wl_tbl)).columns([
                    world_bloom::Column::TimeId,
                    world_bloom::Column::UserIdKey,
                    world_bloom::Column::CharacterId,
                    world_bloom::Column::Score,
                    world_bloom::Column::Rank,
                ]);
                for (t, u, c, s, rk) in &changed {
                    ins.values_panic([
                        (*t).into(),
                        (*u).into(),
                        (*c).into(),
                        (*s).into(),
                        (*rk).into(),
                    ]);
                }
                ins.on_conflict(
                    OnConflict::columns([
                        world_bloom::Column::TimeId,
                        world_bloom::Column::UserIdKey,
                        world_bloom::Column::CharacterId,
                    ])
                    .do_nothing()
                    .to_owned(),
                );
                tx.execute(backend.build(&ins)).await?;
                Ok(state)
            })
        })
        .await
        .map_err(unwrap_tx_err)?;

    *prev_state = updated_state;
    Ok(())
}

fn unwrap_tx_err(e: TransactionError<DbErr>) -> DbErr {
    match e {
        TransactionError::Connection(err) | TransactionError::Transaction(err) => err,
    }
}
