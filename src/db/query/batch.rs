//! Transactional batch inserts plus their two helper lookups
//! (Go: `BatchInsertEventRankings`, `BatchInsertWorldBloomRankings`,
//! `batchGetOrCreateTimeIDs`, `batchGetOrCreateUserIDKeys`).
//!
//! `batch_get_or_create_time_ids` executes inside the caller's transaction
//! so the time-id row and the ranking rows commit atomically. The user
//! dimension upsert is idempotent and independently useful, so it runs
//! *before* the transaction — keeping the write transaction (and its row
//! locks on `event_<id>_users`) as short as possible.

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

/// Lean per-user dimension state: everything needed to decide whether the
/// stored row differs from the incoming payload. Profile columns (three of
/// them multi-KB JSON blobs) are folded into `profile_hash` so the per-tick
/// read-back never transfers them.
#[derive(FromQueryResult)]
struct UserKeyRow {
    user_id: String,
    user_id_key: i64,
    unique_id: Option<String>,
    name: String,
    cheerful_team_id: Option<i64>,
    profile_hash: Option<i64>,
}

#[derive(FromQueryResult)]
struct UserKeyOnlyRow {
    user_id: String,
    user_id_key: i64,
}

/// Look up `time_id` per timestamp, inserting a new row with `status` when
/// the timestamp is not yet present. Returns a `timestamp -> time_id` map.
pub(crate) async fn batch_get_or_create_time_ids(
    tx: &DatabaseTransaction,
    backend: DatabaseBackend,
    table_name: &str,
    timestamps: &HashSet<i64>,
    status: i16,
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
    pub honor_missions_json: Option<String>,
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
            honor_missions_json: json_array_or_none(&r.profile.honor_missions),
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

/// Deterministic digest of the profile columns, stored in `profile_hash` so
/// change detection never reads the JSON blobs back. SHA-256-based (not the
/// std hasher) because the value is persisted: it must stay stable across
/// process restarts and toolchain upgrades. A hash mismatch merely re-writes
/// the row, so rows predating the column (NULL) converge on first sight.
fn profile_hash(u: &UserDimRow) -> i64 {
    use sha2::{Digest, Sha256};
    fn int(h: &mut Sha256, v: Option<i64>) {
        match v {
            Some(v) => {
                h.update([1]);
                h.update(v.to_le_bytes());
            }
            None => h.update([0]),
        }
    }
    fn text(h: &mut Sha256, v: Option<&str>) {
        match v {
            Some(v) => {
                h.update([1]);
                h.update((v.len() as u64).to_le_bytes());
                h.update(v.as_bytes());
            }
            None => h.update([0]),
        }
    }
    let mut h = Sha256::new();
    int(&mut h, u.card_id);
    int(&mut h, u.card_level);
    int(&mut h, u.card_master_rank);
    text(&mut h, u.card_special_training_status.as_deref());
    text(&mut h, u.card_default_image.as_deref());
    text(&mut h, u.profile_word.as_deref());
    text(&mut h, u.profile_honors_json.as_deref());
    text(&mut h, u.honor_missions_json.as_deref());
    text(&mut h, u.player_frames_json.as_deref());
    let digest = h.finalize();
    i64::from_le_bytes(digest[..8].try_into().expect("digest is 32 bytes"))
}

/// Look up `user_id_key` per `user_id`, inserting a new row when missing.
/// Refreshes stored dimension columns when the upstream payload disagrees
/// with the stored row — matches Go's `Save` semantics. Changed and missing
/// rows go through one chunked multi-row upsert instead of per-user
/// round trips; a stored `cheerful_team_id` is never overwritten with NULL
/// (resolved in Rust before the upsert, so no dialect-specific COALESCE).
pub(crate) async fn batch_get_or_create_user_id_keys<C: ConnectionTrait>(
    conn: &C,
    backend: DatabaseBackend,
    table_name: &str,
    users: &HashMap<String, UserDimRow>,
) -> Result<HashMap<String, i64>, DbErr> {
    let mut out = HashMap::with_capacity(users.len());
    let use_unique_ids = users.values().any(|u| u.unique_id.is_some());
    let all_ids: Vec<&str> = users.keys().map(String::as_str).collect();
    let hashes: HashMap<&str, i64> = users
        .iter()
        .map(|(id, u)| (id.as_str(), profile_hash(u)))
        .collect();

    // `(user_id, effective cheerful_team_id)` rows that need writing.
    let mut dirty: Vec<(&str, Option<i64>)> = Vec::new();

    for row in select_user_rows(conn, backend, table_name, &all_ids, use_unique_ids).await? {
        let Some((user_id, info)) = users.get_key_value(&row.user_id) else {
            continue;
        };
        let name_changed = row.name != info.name;
        let cheerful_changed = match (row.cheerful_team_id, info.cheerful_team_id) {
            (_, None) => false,
            (Some(stored), Some(new)) => stored != new,
            (None, Some(_)) => true,
        };
        let unique_changed = use_unique_ids && row.unique_id != info.unique_id;
        let profile_changed = row.profile_hash != Some(hashes[user_id.as_str()]);

        if name_changed || cheerful_changed || unique_changed || profile_changed {
            dirty.push((
                user_id.as_str(),
                info.cheerful_team_id.or(row.cheerful_team_id),
            ));
        }
        out.insert(row.user_id, row.user_id_key);
    }

    let missing: Vec<&str> = users
        .keys()
        .filter(|k| !out.contains_key(*k))
        .map(String::as_str)
        .collect();
    let mut upserts = dirty;
    upserts.extend(missing.iter().map(|id| (*id, users[*id].cheerful_team_id)));
    if upserts.is_empty() {
        return Ok(out);
    }

    for chunk in upserts.chunks(INSERT_CHUNK) {
        let mut ins = Query::insert();
        ins.into_table(Alias::new(table_name));
        let mut columns = vec![
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
            event_users::Column::HonorMissionsJson,
            event_users::Column::PlayerFramesJson,
            event_users::Column::ProfileHash,
        ];
        if use_unique_ids {
            columns.push(event_users::Column::UniqueId);
        }
        ins.columns(columns.clone());
        for (user_id, cheerful_team_id) in chunk {
            let info = &users[*user_id];
            let mut values = vec![
                (*user_id).into(),
                info.name.clone().into(),
                (*cheerful_team_id).into(),
                info.card_id.into(),
                info.card_level.into(),
                info.card_master_rank.into(),
                info.card_special_training_status.clone().into(),
                info.card_default_image.clone().into(),
                info.profile_word.clone().into(),
                info.profile_honors_json.clone().into(),
                info.honor_missions_json.clone().into(),
                info.player_frames_json.clone().into(),
                hashes[user_id].into(),
            ];
            if use_unique_ids {
                values.push(info.unique_id.clone().into());
            }
            ins.values_panic(values);
        }
        // Rows only reach this statement when they genuinely changed (or are
        // new), so the conflict action can overwrite unconditionally.
        let mut conflict = OnConflict::column(event_users::Column::UserId);
        conflict.update_columns(columns.into_iter().skip(1));
        ins.on_conflict(conflict);
        conn.execute(&ins).await?;
    }

    if missing.is_empty() {
        return Ok(out);
    }
    for chunk in missing.chunks(INSERT_CHUNK) {
        let sel = Query::select()
            .expr_as(
                Expr::col(event_users::Column::UserId),
                Alias::new("user_id"),
            )
            .expr_as(
                Expr::col(event_users::Column::UserIdKey),
                Alias::new("user_id_key"),
            )
            .from(Alias::new(table_name))
            .and_where(Expr::col(event_users::Column::UserId).is_in(chunk.iter().copied()))
            .to_owned();
        for row in UserKeyOnlyRow::find_by_statement(backend.build(&sel))
            .all(conn)
            .await?
        {
            out.insert(row.user_id, row.user_id_key);
        }
    }
    if out.len() != users.len() {
        return Err(DbErr::Custom(format!(
            "inserted user_id_key rows vanished ({} of {} resolved)",
            out.len(),
            users.len()
        )));
    }
    Ok(out)
}

/// Keep multi-row statements well under every backend's bind-parameter cap
/// (13 columns × 500 rows = 6 500 params; Postgres allows 65 535).
const INSERT_CHUNK: usize = 500;

async fn select_user_rows<C: ConnectionTrait>(
    conn: &C,
    backend: DatabaseBackend,
    table_name: &str,
    user_ids: &[&str],
    use_unique_ids: bool,
) -> Result<Vec<UserKeyRow>, DbErr> {
    let mut rows = Vec::with_capacity(user_ids.len());
    for chunk in user_ids.chunks(INSERT_CHUNK) {
        let mut sel = Query::select();
        sel.expr_as(
            Expr::col(event_users::Column::UserId),
            Alias::new("user_id"),
        )
        .expr_as(
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
            Expr::col(event_users::Column::ProfileHash),
            Alias::new("profile_hash"),
        )
        .from(Alias::new(table_name))
        .and_where(Expr::col(event_users::Column::UserId).is_in(chunk.iter().copied()));
        rows.extend(
            UserKeyRow::find_by_statement(backend.build(&sel))
                .all(conn)
                .await?,
        );
    }
    Ok(rows)
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

    batch_get_or_create_user_id_keys(engine.conn(), backend, users_tbl, &users).await?;
    Ok(())
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

    let user_lookup =
        batch_get_or_create_user_id_keys(engine.conn(), backend, users_tbl, &users).await?;

    engine
        .conn()
        .transaction::<_, (), DbErr>(move |tx| {
            Box::pin(async move {
                let time_lookup =
                    batch_get_or_create_time_ids(tx, backend, time_tbl, &timestamps, 0).await?;

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
    user_key_cache: &mut HashMap<i64, i64>,
) -> Result<usize, DbErr> {
    if records.is_empty() {
        return Ok(0);
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
    let user_lookup =
        batch_get_or_create_user_id_keys(engine.conn(), backend, users_tbl, &users).await?;
    // Feed the tracker's uid -> key memo so future ticks can pre-diff these
    // users before materializing their rows at all.
    for (user_id, key) in &user_lookup {
        if let Ok(uid) = user_id.parse::<i64>() {
            user_key_cache.insert(uid, *key);
        }
    }

    // Diff against the previous state outside the transaction: a no-change
    // tick never opens one, and the state map is only updated after the
    // rows actually committed (a failed tick retries the same diff; the
    // ranking insert's DO NOTHING dedups any partially-landed rows).
    let mut changed: Vec<(i64, i64, i64, i64, i64)> = Vec::new();
    let mut new_state: Vec<(WorldBloomKey, PlayerState)> = Vec::new();
    for r in records {
        let user_key = *user_lookup
            .get(&r.base.user_id)
            .ok_or_else(|| DbErr::Custom("missing user_id_key lookup".into()))?;
        let key = WorldBloomKey {
            user_id_key: user_key,
            character_id: r.character_id,
        };
        let last = prev_state.get(&key).copied();
        if last.is_none_or(|p| p.score != r.base.score || p.rank != r.base.rank) {
            changed.push((
                r.base.timestamp,
                user_key,
                r.character_id,
                r.base.score,
                r.base.rank,
            ));
            new_state.push((
                key,
                PlayerState {
                    score: r.base.score,
                    rank: r.base.rank,
                },
            ));
        }
    }
    if changed.is_empty() {
        return Ok(0);
    }
    let changed_len = changed.len();

    engine
        .conn()
        .transaction::<_, (), DbErr>(move |tx| {
            Box::pin(async move {
                let time_lookup =
                    batch_get_or_create_time_ids(tx, backend, time_tbl, &timestamps, 0).await?;

                let mut ins = Query::insert();
                ins.into_table(Alias::new(wl_tbl)).columns([
                    world_bloom::Column::TimeId,
                    world_bloom::Column::UserIdKey,
                    world_bloom::Column::CharacterId,
                    world_bloom::Column::Score,
                    world_bloom::Column::Rank,
                ]);
                for (ts, u, c, s, rk) in &changed {
                    let time_id_v = *time_lookup
                        .get(ts)
                        .ok_or_else(|| DbErr::Custom("missing time_id lookup".into()))?;
                    ins.values_panic([
                        time_id_v.into(),
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
                Ok(())
            })
        })
        .await
        .map_err(unwrap_tx_err)?;

    prev_state.extend(new_state);
    Ok(changed_len)
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
                honor_missions: vec![
                    serde_json::from_str(r#"{"honorMissionType":"character","progress":3}"#)
                        .unwrap(),
                ],
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
        assert_eq!(user.user_honor_missions.len(), 1);
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

    #[tokio::test]
    async fn world_bloom_insert_reports_noop_when_state_unchanged() {
        let conn = Database::connect("sqlite::memory:").await.unwrap();
        let engine = DatabaseEngine::from_connection(conn, DatabaseBackend::Sqlite);
        let event_id = 6161;
        create_event_tables(&engine, SekaiServerRegion::Cn, event_id, true)
            .await
            .unwrap();

        let mut prev_state = HashMap::new();
        let mut user_keys = HashMap::new();
        let mut record = PlayerWorldBloomRankingRecordSchema {
            base: PlayerEventRankingRecordSchema {
                timestamp: 1_710_000_000,
                user_id: "100".into(),
                name: "Miku".into(),
                score: 123,
                rank: 1,
                cheerful_team_id: None,
                profile: PlayerProfileSchema::default(),
            },
            character_id: 19,
        };

        let inserted = batch_insert_world_bloom_rankings(
            &engine,
            SekaiServerRegion::Cn,
            event_id,
            &UidAnonymizer::disabled(),
            &[record.clone()],
            &mut prev_state,
            &mut user_keys,
        )
        .await
        .unwrap();
        assert_eq!(inserted, 1);

        record.base.timestamp += 10;
        let inserted = batch_insert_world_bloom_rankings(
            &engine,
            SekaiServerRegion::Cn,
            event_id,
            &UidAnonymizer::disabled(),
            &[record.clone()],
            &mut prev_state,
            &mut user_keys,
        )
        .await
        .unwrap();
        assert_eq!(inserted, 0);

        record.base.timestamp += 10;
        record.base.score += 1;
        let inserted = batch_insert_world_bloom_rankings(
            &engine,
            SekaiServerRegion::Cn,
            event_id,
            &UidAnonymizer::disabled(),
            &[record],
            &mut prev_state,
            &mut user_keys,
        )
        .await
        .unwrap();
        assert_eq!(inserted, 1);

        let stmt = Query::select()
            .expr_as(
                Func::count(Expr::col(world_bloom::Column::TimeId)),
                Alias::new("n"),
            )
            .from(Alias::new(intern(TableKind::WorldBloom, event_id)))
            .to_owned();
        let count = CountRow::find_by_statement(engine.backend().build(&stmt))
            .one(engine.conn())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(count.n, 2);
    }
}
