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
    ConnectionTrait, DatabaseBackend, DatabaseTransaction, DbErr, ExprTrait, FromQueryResult,
    TransactionError, TransactionTrait,
};

use crate::db::engine::DatabaseEngine;
use crate::db::entity::{event, event_users, time_id, world_bloom};
use crate::db::table_name::{TableKind, intern};
use crate::model::enums::SekaiServerRegion;
use crate::model::tracker::{
    PlayerEventRankingRecordSchema, PlayerState, PlayerWorldBloomRankingRecordSchema, WorldBloomKey,
};
use crate::privacy::UidAnonymizer;

#[derive(FromQueryResult)]
struct TimeIdRow {
    time_id: i64,
}

#[derive(FromQueryResult)]
struct UserKeyRow {
    user_id_key: i64,
    unique_id: Option<String>,
    name: String,
    cheerful_team_id: Option<i64>,
    card_id: Option<i64>,
    card_level: Option<i64>,
    card_master_rank: Option<i64>,
    card_special_training_status: Option<String>,
    card_default_image: Option<String>,
    profile_word: Option<String>,
    profile_honors_json: Option<String>,
    player_frames_json: Option<String>,
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
            .values_panic([ts.into(), i16::from(status).into()])
            .to_owned();
        tx.execute(&ins).await?;

        let row = TimeIdRow::find_by_statement(backend.build(&sel))
            .one(tx)
            .await?
            .ok_or_else(|| {
                DbErr::Custom(format!("inserted time_id row vanished for timestamp={ts}"))
            })?;
        out.insert(ts, row.time_id);
    }
    Ok(out)
}

#[derive(Debug, Clone)]
pub(crate) struct UserDimRow {
    pub name: String,
    pub cheerful_team_id: Option<i64>,
    pub unique_id: Option<String>,
    pub card_id: Option<i64>,
    pub card_level: Option<i64>,
    pub card_master_rank: Option<i64>,
    pub card_special_training_status: Option<String>,
    pub card_default_image: Option<String>,
    pub profile_word: Option<String>,
    pub profile_honors_json: Option<String>,
    pub player_frames_json: Option<String>,
}

impl UserDimRow {
    fn from_record(
        server: SekaiServerRegion,
        event_id: i64,
        anonymizer: &UidAnonymizer,
        r: &PlayerEventRankingRecordSchema,
    ) -> Self {
        let card = r.profile.card.as_ref();
        Self {
            name: r.name.clone(),
            cheerful_team_id: r.cheerful_team_id,
            unique_id: anonymizer
                .is_enabled()
                .then(|| anonymizer.public_user_id(server, event_id, &r.user_id)),
            card_id: card.and_then(|c| c.card_id),
            card_level: card.and_then(|c| c.level),
            card_master_rank: card.and_then(|c| c.master_rank),
            card_special_training_status: card.and_then(|c| c.special_training_status.clone()),
            card_default_image: card.and_then(|c| c.default_image.clone()),
            profile_word: r.profile.profile_word.clone(),
            profile_honors_json: json_array_or_none(&r.profile.profile_honors),
            player_frames_json: json_array_or_none(&r.profile.player_frames),
        }
    }
}

fn json_array_or_none<T>(values: &[T]) -> Option<String>
where
    T: serde::Serialize,
{
    if values.is_empty() {
        None
    } else {
        sonic_rs::to_string(values).ok()
    }
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
    let use_unique_ids = users.values().any(|u| u.unique_id.is_some());
    for (user_id, info) in users {
        let mut sel = Query::select();
        sel.expr_as(
            Expr::col(event_users::Column::UserIdKey),
            Alias::new("user_id_key"),
        )
        .expr_as(Expr::col(event_users::Column::Name), Alias::new("name"));
        if use_unique_ids {
            sel.expr_as(
                Expr::col(event_users::Column::UniqueId),
                Alias::new("unique_id"),
            );
        } else {
            sel.expr_as(Expr::val(Option::<String>::None), Alias::new("unique_id"));
        }
        sel.expr_as(
            Expr::col(event_users::Column::CheerfulTeamId),
            Alias::new("cheerful_team_id"),
        )
        .expr_as(
            Expr::col(event_users::Column::CardId),
            Alias::new("card_id"),
        )
        .expr_as(
            Expr::col(event_users::Column::CardLevel),
            Alias::new("card_level"),
        )
        .expr_as(
            Expr::col(event_users::Column::CardMasterRank),
            Alias::new("card_master_rank"),
        )
        .expr_as(
            Expr::col(event_users::Column::CardSpecialTrainingStatus),
            Alias::new("card_special_training_status"),
        )
        .expr_as(
            Expr::col(event_users::Column::CardDefaultImage),
            Alias::new("card_default_image"),
        )
        .expr_as(
            Expr::col(event_users::Column::ProfileWord),
            Alias::new("profile_word"),
        )
        .expr_as(
            Expr::col(event_users::Column::ProfileHonorsJson),
            Alias::new("profile_honors_json"),
        )
        .expr_as(
            Expr::col(event_users::Column::PlayerFramesJson),
            Alias::new("player_frames_json"),
        )
        .from(Alias::new(table_name))
        .and_where(Expr::col(event_users::Column::UserId).eq(user_id.as_str()))
        .limit(1);
        let sel = sel.to_owned();

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
                tx.execute(&upd).await?;
            }
            if use_unique_ids && row.unique_id != info.unique_id {
                let upd = Query::update()
                    .table(Alias::new(table_name))
                    .value(event_users::Column::UniqueId, info.unique_id.clone())
                    .and_where(Expr::col(event_users::Column::UserIdKey).eq(row.user_id_key))
                    .to_owned();
                tx.execute(&upd).await?;
            }
            if row.card_id != info.card_id
                || row.card_level != info.card_level
                || row.card_master_rank != info.card_master_rank
                || row.card_special_training_status != info.card_special_training_status
                || row.card_default_image != info.card_default_image
                || row.profile_word != info.profile_word
                || row.profile_honors_json != info.profile_honors_json
                || row.player_frames_json != info.player_frames_json
            {
                let upd = Query::update()
                    .table(Alias::new(table_name))
                    .value(event_users::Column::CardId, info.card_id)
                    .value(event_users::Column::CardLevel, info.card_level)
                    .value(event_users::Column::CardMasterRank, info.card_master_rank)
                    .value(
                        event_users::Column::CardSpecialTrainingStatus,
                        info.card_special_training_status.clone(),
                    )
                    .value(
                        event_users::Column::CardDefaultImage,
                        info.card_default_image.clone(),
                    )
                    .value(event_users::Column::ProfileWord, info.profile_word.clone())
                    .value(
                        event_users::Column::ProfileHonorsJson,
                        info.profile_honors_json.clone(),
                    )
                    .value(
                        event_users::Column::PlayerFramesJson,
                        info.player_frames_json.clone(),
                    )
                    .and_where(Expr::col(event_users::Column::UserIdKey).eq(row.user_id_key))
                    .to_owned();
                tx.execute(&upd).await?;
            }
            out.insert(user_id.clone(), row.user_id_key);
            continue;
        }

        let mut ins = Query::insert();
        ins.into_table(Alias::new(table_name));
        if use_unique_ids {
            ins.columns([
                event_users::Column::UserId,
                event_users::Column::UniqueId,
                event_users::Column::Name,
                event_users::Column::CheerfulTeamId,
                event_users::Column::CardId,
                event_users::Column::CardLevel,
                event_users::Column::CardMasterRank,
                event_users::Column::CardSpecialTrainingStatus,
                event_users::Column::CardDefaultImage,
                event_users::Column::ProfileWord,
                event_users::Column::ProfileHonorsJson,
                event_users::Column::PlayerFramesJson,
            ])
            .values_panic([
                user_id.as_str().into(),
                info.unique_id.clone().into(),
                info.name.clone().into(),
                info.cheerful_team_id.into(),
                info.card_id.into(),
                info.card_level.into(),
                info.card_master_rank.into(),
                info.card_special_training_status.clone().into(),
                info.card_default_image.clone().into(),
                info.profile_word.clone().into(),
                info.profile_honors_json.clone().into(),
                info.player_frames_json.clone().into(),
            ]);
        } else {
            ins.columns([
                event_users::Column::UserId,
                event_users::Column::Name,
                event_users::Column::CheerfulTeamId,
                event_users::Column::CardId,
                event_users::Column::CardLevel,
                event_users::Column::CardMasterRank,
                event_users::Column::CardSpecialTrainingStatus,
                event_users::Column::CardDefaultImage,
                event_users::Column::ProfileWord,
                event_users::Column::ProfileHonorsJson,
                event_users::Column::PlayerFramesJson,
            ])
            .values_panic([
                user_id.as_str().into(),
                info.name.clone().into(),
                info.cheerful_team_id.into(),
                info.card_id.into(),
                info.card_level.into(),
                info.card_master_rank.into(),
                info.card_special_training_status.clone().into(),
                info.card_default_image.clone().into(),
                info.profile_word.clone().into(),
                info.profile_honors_json.clone().into(),
                info.player_frames_json.clone().into(),
            ]);
        }
        let ins = ins.to_owned();
        tx.execute(&ins).await?;

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

fn collect_dims<'a, I>(
    server: SekaiServerRegion,
    event_id: i64,
    anonymizer: &UidAnonymizer,
    records: I,
) -> (HashSet<i64>, HashMap<String, UserDimRow>)
where
    I: Iterator<Item = &'a PlayerEventRankingRecordSchema>,
{
    let mut timestamps = HashSet::new();
    let mut users: HashMap<String, UserDimRow> = HashMap::new();
    for r in records {
        timestamps.insert(r.timestamp);
        users
            .entry(r.user_id.clone())
            .or_insert_with(|| UserDimRow::from_record(server, event_id, anonymizer, r));
    }
    (timestamps, users)
}

fn collect_users<'a, I>(
    server: SekaiServerRegion,
    event_id: i64,
    anonymizer: &UidAnonymizer,
    records: I,
) -> HashMap<String, UserDimRow>
where
    I: Iterator<Item = &'a PlayerEventRankingRecordSchema>,
{
    let mut users = HashMap::new();
    for r in records {
        users
            .entry(r.user_id.clone())
            .or_insert_with(|| UserDimRow::from_record(server, event_id, anonymizer, r));
    }
    users
}

#[tracing::instrument(skip(engine, records), fields(event_id, n = records.len()))]
pub async fn batch_upsert_event_users(
    engine: &DatabaseEngine,
    server: SekaiServerRegion,
    event_id: i64,
    anonymizer: &UidAnonymizer,
    records: &[PlayerEventRankingRecordSchema],
) -> Result<(), DbErr> {
    if records.is_empty() {
        return Ok(());
    }
    let backend = engine.backend();
    let users_tbl = intern(TableKind::EventUsers, event_id);
    let users = collect_users(server, event_id, anonymizer, records.iter());

    engine
        .conn()
        .transaction::<_, (), DbErr>(move |tx| {
            Box::pin(async move {
                batch_get_or_create_user_id_keys(tx, backend, users_tbl, &users).await?;
                Ok(())
            })
        })
        .await
        .map_err(unwrap_tx_err)
}

#[tracing::instrument(skip(engine, records), fields(event_id, n = records.len()))]
pub async fn batch_insert_event_rankings(
    engine: &DatabaseEngine,
    server: SekaiServerRegion,
    event_id: i64,
    anonymizer: &UidAnonymizer,
    records: &[PlayerEventRankingRecordSchema],
) -> Result<(), DbErr> {
    if records.is_empty() {
        return Ok(());
    }
    let backend = engine.backend();
    let time_tbl = intern(TableKind::TimeId, event_id);
    let users_tbl = intern(TableKind::EventUsers, event_id);
    let event_tbl = intern(TableKind::Event, event_id);

    let (timestamps, users) = collect_dims(server, event_id, anonymizer, records.iter());
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
                        .do_nothing_on([event::Column::TimeId, event::Column::UserIdKey])
                        .to_owned(),
                );
                tx.execute(&ins).await?;
                Ok(())
            })
        })
        .await
        .map_err(unwrap_tx_err)
}

#[tracing::instrument(skip(engine, records, prev_state), fields(event_id, n = records.len()))]
pub async fn batch_insert_world_bloom_rankings(
    engine: &DatabaseEngine,
    server: SekaiServerRegion,
    event_id: i64,
    anonymizer: &UidAnonymizer,
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

    let (timestamps, users) = collect_dims(
        server,
        event_id,
        anonymizer,
        records.iter().map(|r| &r.base),
    );
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
                    .do_nothing_on([
                        world_bloom::Column::TimeId,
                        world_bloom::Column::UserIdKey,
                        world_bloom::Column::CharacterId,
                    ])
                    .to_owned(),
                );
                tx.execute(&ins).await?;
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

#[cfg(test)]
mod tests {
    use sea_orm::sea_query::{Alias, Expr, Func, Query};
    use sea_orm::{Database, DatabaseBackend, FromQueryResult};

    use super::*;
    use crate::db::engine::DatabaseEngine;
    use crate::db::query::user::{PublicUserIdMode, get_user_data};
    use crate::db::schema::create_event_tables;
    use crate::model::sekai::{UserCard, UserPlayerFrame, UserProfileHonor};
    use crate::model::tracker::PlayerProfileSchema;

    #[derive(FromQueryResult)]
    struct CountRow {
        n: i64,
    }

    #[tokio::test]
    async fn users_only_upsert_updates_profile_without_ranking_rows() {
        let conn = Database::connect("sqlite::memory:").await.unwrap();
        let engine = DatabaseEngine::from_connection(conn, DatabaseBackend::Sqlite);
        let event_id = 5151;
        create_event_tables(&engine, SekaiServerRegion::Jp, event_id, false)
            .await
            .unwrap();

        let records = vec![PlayerEventRankingRecordSchema {
            timestamp: 1_710_000_000,
            user_id: "100".into(),
            name: "Miku".into(),
            score: 123,
            rank: 1,
            cheerful_team_id: None,
            profile: PlayerProfileSchema {
                card: Some(UserCard {
                    card_id: Some(1404),
                    level: Some(60),
                    master_rank: Some(5),
                    special_training_status: Some("done".into()),
                    default_image: Some("special_training".into()),
                }),
                profile_word: Some("hello".into()),
                profile_honors: vec![UserProfileHonor {
                    seq: Some(1),
                    profile_honor_type: Some("normal".into()),
                    honor_id: Some(95),
                    honor_level: Some(9),
                    bonds_honor_view_type: Some("none".into()),
                    bonds_honor_word_id: Some(0),
                }],
                player_frames: vec![UserPlayerFrame {
                    player_frame_id: Some(10050),
                    player_frame_attach_status: Some("first".into()),
                }],
            },
        }];

        batch_upsert_event_users(
            &engine,
            SekaiServerRegion::Jp,
            event_id,
            &UidAnonymizer::disabled(),
            &records,
        )
        .await
        .unwrap();

        let user = get_user_data(&engine, event_id, "100", PublicUserIdMode::Raw)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(user.card_id, Some(1404));
        assert_eq!(user.profile_word.as_deref(), Some("hello"));
        assert_eq!(user.profile_honors[0].honor_id, Some(95));
        assert_eq!(user.user_player_frames[0].player_frame_id, Some(10050));

        let stmt = Query::select()
            .expr_as(
                Func::count(Expr::col(event::Column::TimeId)),
                Alias::new("n"),
            )
            .from(Alias::new(intern(TableKind::Event, event_id)))
            .to_owned();
        let count = CountRow::find_by_statement(engine.backend().build(&stmt))
            .one(engine.conn())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(count.n, 0);
    }
}
