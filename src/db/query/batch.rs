//! Transactional batch inserts plus their two helper lookups
//! (Go: `BatchInsertEventRankings`, `BatchInsertWorldBloomRankings`,
//! `batchGetOrCreateTimeIDs`, `batchGetOrCreateUserIDKeys`).
//!
//! One flush is one transaction: the user dimension upsert, the time-id
//! rows and the ranking rows commit together. On PostgreSQL the whole
//! flush — including the post-commit `pg_current_wal_lsn()` the cluster
//! invalidation needs — runs on a single pooled connection through
//! `db::pg_session` (one release ping instead of three, one statement
//! cache, `write_timeout` enforced with a hard close). Other dialects use
//! sea-orm's transaction handle; the statements are shared.
//!
//! The writer is the only process that writes a event's `users` table, so
//! it remembers what it wrote ([`UserMemo`]): a user whose incoming
//! dimension values match the memo needs neither the read-back nor the
//! upsert, and its `user_id_key` comes from memory.

use std::collections::{HashMap, HashSet};

use sea_orm::sea_query::{Alias, Expr, OnConflict, Query};
use sea_orm::{
    ConnectionTrait, DatabaseBackend, DbErr, ExprTrait, FromQueryResult, TransactionTrait,
};

use crate::db::engine::DatabaseEngine;
use crate::db::entity::time_id::time_id_for_timestamp;
use crate::db::entity::{event, event_users, time_id, world_bloom};
use crate::db::pg_session::{PgSession, with_writer_session};
use crate::db::table_name::{TableKind, intern};
use crate::model::enums::SekaiServerRegion;
use crate::model::tracker::{
    PlayerEventRankingRecordSchema, PlayerState, PlayerWorldBloomRankingRecordSchema, WorldBloomKey,
};
use crate::privacy::UidAnonymizer;

#[derive(FromQueryResult)]
struct TimeIdRow {
    time_id: i64,
    timestamp: i64,
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

impl UserKeyRow {
    fn into_memo(self) -> Option<(i64, UserMemoEntry)> {
        let uid = self.user_id.parse().ok()?;
        Some((
            uid,
            UserMemoEntry {
                user_id_key: self.user_id_key,
                name: self.name,
                cheerful_team_id: self.cheerful_team_id,
                unique_id: self.unique_id,
                profile_hash: self.profile_hash,
            },
        ))
    }
}

#[derive(FromQueryResult)]
struct UserKeyOnlyRow {
    user_id: String,
    user_id_key: i64,
}

/// What the writer knows a user's stored dimension row to hold: the values
/// it last read back or wrote. `unique_id` is only meaningful while
/// anonymization is on (it is what the row carries, `None` otherwise).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserMemoEntry {
    pub user_id_key: i64,
    pub name: String,
    pub cheerful_team_id: Option<i64>,
    pub unique_id: Option<String>,
    pub profile_hash: Option<i64>,
}

/// `uid -> stored dimension row` memo, one per tracked event, owned by the
/// tracker and fed by every committed flush. Correct because this writer is
/// the only process that writes the event's `users` table: an entry only
/// ever reflects a row this process read or wrote, and it advances only
/// after the transaction that wrote it committed. A stale or missing entry
/// costs one extra read-back/upsert, never a wrong key: a fresh process
/// starts empty and warms up on its first flushes.
///
/// Salt rotation or a rename shows up as a mismatch against the entry
/// (`unique_id`, `name`) and re-writes the row, exactly like the read-back
/// did. Recreating an event's tables under a live writer is not supported
/// (the memo would hand out keys the new table never allocated).
#[derive(Debug, Default)]
pub struct UserMemo {
    entries: HashMap<i64, UserMemoEntry>,
}

impl UserMemo {
    pub fn user_id_key(&self, uid: i64) -> Option<i64> {
        self.entries.get(&uid).map(|e| e.user_id_key)
    }

    pub fn get(&self, uid: i64) -> Option<&UserMemoEntry> {
        self.entries.get(&uid)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn insert(&mut self, uid: i64, entry: UserMemoEntry) {
        self.entries.insert(uid, entry);
    }

    /// The memoized key for `uid` when the stored row already carries
    /// `info`'s dimension values, so the flush can skip the user entirely.
    pub(crate) fn current_key(&self, uid: i64, info: &UserDimRow) -> Option<i64> {
        self.entries
            .get(&uid)
            .filter(|entry| !user_row_changed(entry, info))
            .map(|entry| entry.user_id_key)
    }

    fn absorb(&mut self, learned: Vec<(i64, UserMemoEntry)>) {
        self.entries.extend(learned);
    }
}

type DirtyUser = (i64, Option<i64>);

/// Keys and learned rows of one users-table resolution.
struct ResolvedUsers {
    keys: HashMap<i64, i64>,
    learned: Vec<(i64, UserMemoEntry)>,
}

/// Look up `time_id` per timestamp, inserting new rows with `status` for
/// timestamps not yet present. Returns a `timestamp -> time_id` map.
///
/// This is the legacy-table path: a table that still carries
/// sequence-numbered rows has to be read back, because an existing row's
/// id is reused as-is. Tables where every row already has
/// `time_id == timestamp` skip the two lookups (see
/// [`resolve_time_ids`]).
///
/// New rows get `time_id_for_timestamp` as their id rather than the
/// sequence: a coalesced flush carries a whole window of timestamps, the
/// main and World Bloom batches allocate separately, and a heartbeat can
/// land while earlier samples are still buffered — with sequence ids each
/// of those could hand a later sample a smaller id.
pub(crate) async fn batch_get_or_create_time_ids<C: ConnectionTrait>(
    conn: &C,
    backend: DatabaseBackend,
    table_name: &str,
    timestamps: &HashSet<i64>,
    status: i16,
) -> Result<HashMap<i64, i64>, DbErr> {
    let mut out = HashMap::with_capacity(timestamps.len());
    if timestamps.is_empty() {
        return Ok(out);
    }
    let select_by_ts = |ts: Vec<i64>| {
        Query::select()
            .expr_as(Expr::col(time_id::Column::TimeId), Alias::new("time_id"))
            .expr_as(
                Expr::col(time_id::Column::Timestamp),
                Alias::new("timestamp"),
            )
            .from(Alias::new(table_name))
            .and_where(Expr::col(time_id::Column::Timestamp).is_in(ts))
            .to_owned()
    };

    let sel = select_by_ts(timestamps.iter().copied().collect());
    for row in TimeIdRow::find_by_statement(backend.build(&sel))
        .all(conn)
        .await?
    {
        out.insert(row.timestamp, row.time_id);
    }

    let mut missing: Vec<i64> = timestamps
        .iter()
        .copied()
        .filter(|ts| !out.contains_key(ts))
        .collect();
    if missing.is_empty() {
        return Ok(out);
    }
    missing.sort_unstable();
    insert_time_rows(conn, table_name, &missing, status).await?;

    let sel = select_by_ts(missing);
    for row in TimeIdRow::find_by_statement(backend.build(&sel))
        .all(conn)
        .await?
    {
        out.insert(row.timestamp, row.time_id);
    }
    if out.len() != timestamps.len() {
        return Err(DbErr::Custom(format!(
            "inserted time_id rows vanished ({} of {} resolved)",
            out.len(),
            timestamps.len()
        )));
    }
    Ok(out)
}

/// One multi-row, conflict-ignoring insert of `time_id = timestamp` rows.
async fn insert_time_rows<C: ConnectionTrait>(
    conn: &C,
    table_name: &str,
    timestamps: &[i64],
    status: i16,
) -> Result<(), DbErr> {
    let mut ins = Query::insert();
    ins.into_table(Alias::new(table_name)).columns([
        time_id::Column::TimeId,
        time_id::Column::Timestamp,
        time_id::Column::Status,
    ]);
    for &ts in timestamps {
        ins.values_panic([time_id_for_timestamp(ts).into(), ts.into(), status.into()]);
    }
    ins.on_conflict(
        OnConflict::column(time_id::Column::Timestamp)
            .do_nothing_on([time_id::Column::Timestamp])
            .to_owned(),
    );
    conn.execute(&ins).await?;
    Ok(())
}

/// Whether every row of `event_<id>_time_id` has `time_id == timestamp`.
/// True for every table this writer created; false while legacy
/// sequence-numbered rows remain (until `repair-time-ids` renumbers them).
pub(crate) async fn probe_time_id_alignment<C: ConnectionTrait>(
    conn: &C,
    table_name: &str,
) -> Result<bool, DbErr> {
    let sel = Query::select()
        .expr(Expr::val(1))
        .from(Alias::new(table_name))
        .and_where(Expr::col(time_id::Column::TimeId).ne(Expr::col(time_id::Column::Timestamp)))
        .limit(1)
        .to_owned();
    Ok(conn.query_one(&sel).await?.is_none())
}

/// `timestamp -> time_id` for a flush, creating the time rows. On an
/// aligned table the map is derived — `time_id == timestamp` is what this
/// writer inserts and what every existing row already satisfies, so the
/// select/insert/re-select becomes one insert. The alignment answer is
/// probed once per event and cached on the engine.
async fn resolve_time_ids<C: ConnectionTrait>(
    conn: &C,
    engine: &DatabaseEngine,
    event_id: i64,
    table_name: &str,
    timestamps: &HashSet<i64>,
) -> Result<HashMap<i64, i64>, DbErr> {
    let aligned = match engine.time_id_alignment(event_id) {
        Some(aligned) => aligned,
        None => {
            let aligned = probe_time_id_alignment(conn, table_name).await?;
            tracing::info!(event_id, aligned, "probed time_id alignment");
            engine.set_time_id_alignment(event_id, aligned);
            aligned
        }
    };
    if !aligned {
        return batch_get_or_create_time_ids(conn, engine.backend(), table_name, timestamps, 0)
            .await;
    }
    let mut ordered: Vec<i64> = timestamps.iter().copied().collect();
    ordered.sort_unstable();
    insert_time_rows(conn, table_name, &ordered, 0).await?;
    Ok(ordered
        .into_iter()
        .map(|ts| (ts, time_id_for_timestamp(ts)))
        .collect())
}

/// One user's dimension values as the writer stores them: profile columns
/// already serialized, `unique_id` already salted, and the profile digest
/// computed once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserDimRow {
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
    pub profile_hash: i64,
}

impl UserDimRow {
    pub fn from_record(
        server: SekaiServerRegion,
        event_id: i64,
        anonymizer: &UidAnonymizer,
        r: &PlayerEventRankingRecordSchema,
    ) -> Self {
        let card = r.profile.card.as_ref();
        let mut row = Self {
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
            profile_hash: 0,
        };
        row.profile_hash = profile_hash(&row);
        row
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

/// Look up `user_id_key` per uid, inserting a new row when missing.
/// Refreshes stored dimension columns when the upstream payload disagrees
/// with the stored row — matches Go's `Save` semantics. Users the memo
/// already holds with these exact values are answered from memory; the
/// rest are read back in one select, and changed and missing rows go
/// through one chunked multi-row upsert instead of per-user round trips.
/// A stored `cheerful_team_id` is never overwritten with NULL (resolved in
/// Rust before the upsert, so no dialect-specific COALESCE).
async fn batch_get_or_create_user_id_keys<C: ConnectionTrait>(
    conn: &C,
    backend: DatabaseBackend,
    table_name: &str,
    users: &HashMap<i64, UserDimRow>,
    memo: &UserMemo,
) -> Result<ResolvedUsers, DbErr> {
    let use_unique_ids = users.values().any(|u| u.unique_id.is_some());
    let mut keys = HashMap::with_capacity(users.len());
    let mut probe: Vec<i64> = Vec::new();
    for (&uid, info) in users {
        match memo.current_key(uid, info) {
            Some(key) => {
                keys.insert(uid, key);
            }
            None => probe.push(uid),
        }
    }
    if probe.is_empty() {
        return Ok(ResolvedUsers {
            keys,
            learned: Vec::new(),
        });
    }
    let probe_ids: Vec<String> = probe.iter().map(i64::to_string).collect();
    let rows = select_user_rows(conn, backend, table_name, &probe_ids, use_unique_ids).await?;
    let mut learned = Vec::new();
    let mut dirty: Vec<DirtyUser> = Vec::new();
    for row in rows {
        let Some((uid, entry)) = row.into_memo() else {
            continue;
        };
        let Some(info) = users.get(&uid) else {
            continue;
        };
        if user_row_changed(&entry, info) {
            dirty.push((uid, info.cheerful_team_id.or(entry.cheerful_team_id)));
        } else {
            learned.push((uid, entry.clone()));
        }
        keys.insert(uid, entry.user_id_key);
    }

    let missing: Vec<i64> = probe
        .iter()
        .copied()
        .filter(|uid| !keys.contains_key(uid))
        .collect();
    let mut upserts = dirty;
    upserts.extend(
        missing
            .iter()
            .map(|&uid| (uid, users[&uid].cheerful_team_id)),
    );
    if !upserts.is_empty() {
        upsert_user_rows(conn, table_name, users, &upserts, use_unique_ids).await?;
    }

    load_missing_user_keys(conn, backend, table_name, &missing, &mut keys).await?;
    if keys.len() != users.len() {
        return Err(DbErr::Custom(format!(
            "inserted user_id_key rows vanished ({} of {} resolved)",
            keys.len(),
            users.len()
        )));
    }
    for (uid, cheerful_team_id) in upserts {
        let info = &users[&uid];
        learned.push((
            uid,
            UserMemoEntry {
                user_id_key: keys[&uid],
                name: info.name.clone(),
                cheerful_team_id,
                unique_id: use_unique_ids.then(|| info.unique_id.clone()).flatten(),
                profile_hash: Some(info.profile_hash),
            },
        ));
    }
    Ok(ResolvedUsers { keys, learned })
}

/// Whether the stored row (as memoized or read back) differs from the
/// incoming values. An incoming `cheerful_team_id` of NULL never counts
/// as a change; `unique_id` is compared only while anonymization is on.
fn user_row_changed(entry: &UserMemoEntry, info: &UserDimRow) -> bool {
    let cheerful_changed = match (entry.cheerful_team_id, info.cheerful_team_id) {
        (_, None) => false,
        (Some(stored), Some(new)) => stored != new,
        (None, Some(_)) => true,
    };
    entry.name != info.name
        || cheerful_changed
        || info.unique_id.is_some() && entry.unique_id != info.unique_id
        || entry.profile_hash != Some(info.profile_hash)
}

async fn upsert_user_rows<C: ConnectionTrait>(
    conn: &C,
    table_name: &str,
    users: &HashMap<i64, UserDimRow>,
    upserts: &[DirtyUser],
    use_unique_ids: bool,
) -> Result<(), DbErr> {
    for chunk in upserts.chunks(INSERT_CHUNK) {
        let mut ins = Query::insert();
        ins.into_table(Alias::new(table_name));
        let mut columns = user_upsert_columns();
        if use_unique_ids {
            columns.push(event_users::Column::UniqueId);
        }
        ins.columns(columns.clone());
        for &(uid, cheerful_team_id) in chunk {
            let info = &users[&uid];
            let mut values = user_upsert_values(&uid.to_string(), cheerful_team_id, info);
            if use_unique_ids {
                values.push(info.unique_id.clone().into());
            }
            ins.values_panic(values);
        }
        let mut conflict = OnConflict::column(event_users::Column::UserId);
        conflict.update_columns(columns.into_iter().skip(1));
        ins.on_conflict(conflict);
        conn.execute(&ins).await?;
    }
    Ok(())
}

fn user_upsert_columns() -> Vec<event_users::Column> {
    vec![
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
    ]
}

fn user_upsert_values(
    user_id: &str,
    cheerful_team_id: Option<i64>,
    info: &UserDimRow,
) -> Vec<Expr> {
    vec![
        user_id.into(),
        info.name.clone().into(),
        cheerful_team_id.into(),
        info.card_id.into(),
        info.card_level.into(),
        info.card_master_rank.into(),
        info.card_special_training_status.clone().into(),
        info.card_default_image.clone().into(),
        info.profile_word.clone().into(),
        info.profile_honors_json.clone().into(),
        info.honor_missions_json.clone().into(),
        info.player_frames_json.clone().into(),
        info.profile_hash.into(),
    ]
}

async fn load_missing_user_keys<C: ConnectionTrait>(
    conn: &C,
    backend: DatabaseBackend,
    table_name: &str,
    missing: &[i64],
    out: &mut HashMap<i64, i64>,
) -> Result<(), DbErr> {
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
            .and_where(
                Expr::col(event_users::Column::UserId).is_in(chunk.iter().map(i64::to_string)),
            )
            .to_owned();
        for row in UserKeyOnlyRow::find_by_statement(backend.build(&sel))
            .all(conn)
            .await?
        {
            if let Ok(uid) = row.user_id.parse::<i64>() {
                out.insert(uid, row.user_id_key);
            }
        }
    }
    Ok(())
}

/// Keep multi-row statements well under every backend's bind-parameter cap
/// (13 columns × 500 rows = 6 500 params; Postgres allows 65 535).
const INSERT_CHUNK: usize = 500;

async fn select_user_rows<C: ConnectionTrait>(
    conn: &C,
    backend: DatabaseBackend,
    table_name: &str,
    user_ids: &[String],
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
        .and_where(Expr::col(event_users::Column::UserId).is_in(chunk.iter().map(String::as_str)));
        rows.extend(
            UserKeyRow::find_by_statement(backend.build(&sel))
                .all(conn)
                .await?,
        );
    }
    Ok(rows)
}

fn parse_uid(user_id: &str) -> Result<i64, DbErr> {
    user_id
        .parse()
        .map_err(|_| DbErr::Custom(format!("user_id {user_id:?} is not a numeric id")))
}

fn collect_users<'a, I>(
    server: SekaiServerRegion,
    event_id: i64,
    anonymizer: &UidAnonymizer,
    records: I,
) -> Result<HashMap<i64, UserDimRow>, DbErr>
where
    I: Iterator<Item = &'a PlayerEventRankingRecordSchema>,
{
    // A coalesced flush carries several samples of one user; the dimension
    // row must reflect the newest one (a rename mid-window, a card swap).
    // Latest timestamp wins, ties go to the later occurrence — the main and
    // World Bloom buffers are chained, so plain iteration order is not
    // sample order.
    let mut latest: HashMap<i64, &PlayerEventRankingRecordSchema> = HashMap::new();
    for r in records {
        latest
            .entry(parse_uid(&r.user_id)?)
            .and_modify(|cur| {
                if r.timestamp >= cur.timestamp {
                    *cur = r;
                }
            })
            .or_insert(r);
    }
    Ok(latest
        .into_iter()
        .map(|(uid, r)| {
            (
                uid,
                UserDimRow::from_record(server, event_id, anonymizer, r),
            )
        })
        .collect())
}

#[tracing::instrument(skip(engine, records, memo), fields(event_id, n = records.len()))]
pub async fn batch_upsert_event_users(
    engine: &DatabaseEngine,
    server: SekaiServerRegion,
    event_id: i64,
    anonymizer: &UidAnonymizer,
    records: &[PlayerEventRankingRecordSchema],
    memo: &mut UserMemo,
) -> Result<(), DbErr> {
    if records.is_empty() {
        return Ok(());
    }
    let backend = engine.backend();
    let users_tbl = intern(TableKind::EventUsers, event_id);
    let users = collect_users(server, event_id, anonymizer, records.iter())?;

    let resolved =
        batch_get_or_create_user_id_keys(engine.conn(), backend, users_tbl, &users, memo).await?;
    memo.absorb(resolved.learned);
    Ok(())
}

/// Rows a flush wrote, per table, and (PostgreSQL) the WAL position right
/// after its commit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FlushOutcome {
    pub main_rows: usize,
    pub world_bloom_rows: usize,
    pub lsn: Option<String>,
}

impl FlushOutcome {
    pub fn wrote_rows(&self) -> bool {
        self.main_rows > 0 || self.world_bloom_rows > 0
    }
}

#[tracing::instrument(skip(engine, records), fields(event_id, n = records.len()))]
pub async fn batch_insert_event_rankings(
    engine: &DatabaseEngine,
    server: SekaiServerRegion,
    event_id: i64,
    anonymizer: &UidAnonymizer,
    records: &[PlayerEventRankingRecordSchema],
) -> Result<(), DbErr> {
    batch_insert_flush(
        engine,
        server,
        event_id,
        anonymizer,
        records,
        &[],
        &mut HashMap::new(),
        &mut UserMemo::default(),
    )
    .await
    .map(|_| ())
}

#[tracing::instrument(skip(engine, records, prev_state, memo), fields(event_id, n = records.len()))]
pub async fn batch_insert_world_bloom_rankings(
    engine: &DatabaseEngine,
    server: SekaiServerRegion,
    event_id: i64,
    anonymizer: &UidAnonymizer,
    records: &[PlayerWorldBloomRankingRecordSchema],
    prev_state: &mut HashMap<WorldBloomKey, PlayerState>,
    memo: &mut UserMemo,
) -> Result<usize, DbErr> {
    batch_insert_flush(
        engine,
        server,
        event_id,
        anonymizer,
        &[],
        records,
        prev_state,
        memo,
    )
    .await
    .map(|outcome| outcome.world_bloom_rows)
}

struct FlushInput<'a> {
    server: SekaiServerRegion,
    event_id: i64,
    anonymizer: &'a UidAnonymizer,
    main: &'a [PlayerEventRankingRecordSchema],
    world_bloom: &'a [PlayerWorldBloomRankingRecordSchema],
    prev_state: &'a HashMap<WorldBloomKey, PlayerState>,
    memo: &'a UserMemo,
}

/// What a committed flush hands back to the caller's in-memory state.
struct FlushWrite {
    outcome: FlushOutcome,
    running: HashMap<WorldBloomKey, PlayerState>,
    learned: Vec<(i64, UserMemoEntry)>,
}

/// Writes one tracker flush — main and World Bloom rows of every buffered
/// sample — in a single transaction, so a reader (or a streaming replica)
/// sees either none of it or all of it: never a sample with only some of
/// its rank moves, and never main rows without the chapter rows sampled
/// with them. `flush_max_rows` / hot-rank triggers only decide *when* the
/// whole buffer flushes; nothing splits it.
///
/// World Bloom rows are diffed against `prev_state` first (a no-change
/// batch writes nothing), and the state only advances after the commit (a
/// failed flush retries the same diff; the inserts' DO NOTHING dedups any
/// rows that already landed). `running` advances per record within the
/// batch: a coalesced flush can carry several samples for one
/// `(user, chapter)`, and a value that oscillates back to the pre-batch
/// state is still a real trace point. `memo` likewise learns the users
/// rows only once they are committed.
///
/// On PostgreSQL the flush is bounded by the engine's `write_timeout`; a
/// timed-out flush surfaces as an error and is safe to retry.
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(
    skip(engine, anonymizer, main, world_bloom, prev_state, memo),
    fields(event_id, main = main.len(), world_bloom = world_bloom.len())
)]
pub async fn batch_insert_flush(
    engine: &DatabaseEngine,
    server: SekaiServerRegion,
    event_id: i64,
    anonymizer: &UidAnonymizer,
    main: &[PlayerEventRankingRecordSchema],
    world_bloom: &[PlayerWorldBloomRankingRecordSchema],
    prev_state: &mut HashMap<WorldBloomKey, PlayerState>,
    memo: &mut UserMemo,
) -> Result<FlushOutcome, DbErr> {
    if main.is_empty() && world_bloom.is_empty() {
        return Ok(FlushOutcome::default());
    }
    let input = FlushInput {
        server,
        event_id,
        anonymizer,
        main,
        world_bloom,
        prev_state,
        memo,
    };
    let written = match engine.backend() {
        DatabaseBackend::Postgres => flush_on_postgres(engine, &input).await?,
        _ => {
            let tx = engine.conn().begin().await?;
            match write_flush(&tx, engine, &input).await {
                Ok(written) => {
                    tx.commit().await?;
                    written
                }
                Err(err) => {
                    tx.rollback().await?;
                    return Err(err);
                }
            }
        }
    };

    memo.absorb(written.learned);
    prev_state.extend(written.running);
    Ok(written.outcome)
}

/// The PostgreSQL flush: `BEGIN` with the server-side timeouts, every
/// statement, then `COMMIT` and the WAL position in one round trip — all
/// on one connection. A failed statement rolls back on the same
/// connection; if the rollback itself fails the connection is discarded.
async fn flush_on_postgres(
    engine: &DatabaseEngine,
    input: &FlushInput<'_>,
) -> Result<FlushWrite, DbErr> {
    let begin = begin_sql(engine.write_timeout());
    with_writer_session(engine, |session| async move {
        if let Err(err) = session.execute_simple(begin).await {
            session.rollback_or_mark_broken().await;
            return Err(err);
        }
        let mut written = match write_flush(&*session, engine, input).await {
            Ok(written) => written,
            Err(err) => {
                session.rollback_or_mark_broken().await;
                return Err(err);
            }
        };
        match commit_and_read_lsn(&session).await {
            Ok(lsn) => {
                written.outcome.lsn = lsn;
                Ok(written)
            }
            Err(err) => {
                session.rollback_or_mark_broken().await;
                Err(err)
            }
        }
    })
    .await
}

/// `BEGIN` plus, when a write timeout is configured, the matching
/// server-side caps for this transaction only: a runaway statement is
/// cancelled, and a session left idle mid-transaction (client stalled or
/// gone) is terminated so its locks don't outlive the client-side timeout.
/// Transaction-local so init-time DDL on big legacy tables is unaffected.
fn begin_sql(write_timeout: Option<std::time::Duration>) -> String {
    match write_timeout {
        Some(limit) => {
            let ms = limit.as_millis().max(1);
            format!(
                "BEGIN; SET LOCAL statement_timeout = {ms}; \
                 SET LOCAL idle_in_transaction_session_timeout = {ms}"
            )
        }
        None => "BEGIN".into(),
    }
}

/// Commit and read the flushed WAL position in the same round trip: the
/// `SELECT` runs after `COMMIT` returned on this backend, so the position
/// covers the commit record (default `synchronous_commit`).
async fn commit_and_read_lsn(session: &PgSession) -> Result<Option<String>, DbErr> {
    let rows = session
        .query_simple("COMMIT; SELECT pg_current_wal_lsn()::text AS lsn".into())
        .await?;
    rows.last()
        .map(|row| row.try_get::<String>("", "lsn"))
        .transpose()
}

/// Every statement of one flush, in order, on `conn` — sea-orm's
/// transaction handle or the PostgreSQL writer session between its
/// `BEGIN` and `COMMIT`.
async fn write_flush<C: ConnectionTrait>(
    conn: &C,
    engine: &DatabaseEngine,
    input: &FlushInput<'_>,
) -> Result<FlushWrite, DbErr> {
    let backend = engine.backend();
    let event_id = input.event_id;
    let time_tbl = intern(TableKind::TimeId, event_id);
    let users_tbl = intern(TableKind::EventUsers, event_id);
    let event_tbl = intern(TableKind::Event, event_id);
    let wl_tbl = intern(TableKind::WorldBloom, event_id);

    let users = collect_users(
        input.server,
        event_id,
        input.anonymizer,
        input
            .main
            .iter()
            .chain(input.world_bloom.iter().map(|r| &r.base)),
    )?;
    let resolved =
        batch_get_or_create_user_id_keys(conn, backend, users_tbl, &users, input.memo).await?;
    let user_key = |user_id: &str| {
        parse_uid(user_id).and_then(|uid| {
            resolved
                .keys
                .get(&uid)
                .copied()
                .ok_or_else(|| DbErr::Custom("missing user_id_key lookup".into()))
        })
    };

    let mut main_rows: Vec<(i64, i64, i64, i64)> = Vec::with_capacity(input.main.len());
    for r in input.main {
        main_rows.push((r.timestamp, user_key(&r.user_id)?, r.score, r.rank));
    }
    let mut wl_rows: Vec<(i64, i64, i64, i64, i64)> = Vec::new();
    let mut running: HashMap<WorldBloomKey, PlayerState> = HashMap::new();
    for r in input.world_bloom {
        let user_key = user_key(&r.base.user_id)?;
        let key = WorldBloomKey {
            user_id_key: user_key,
            character_id: r.character_id,
        };
        let last = running
            .get(&key)
            .copied()
            .or_else(|| input.prev_state.get(&key).copied());
        if last.is_none_or(|p| p.score != r.base.score || p.rank != r.base.rank) {
            wl_rows.push((
                r.base.timestamp,
                user_key,
                r.character_id,
                r.base.score,
                r.base.rank,
            ));
            running.insert(
                key,
                PlayerState {
                    score: r.base.score,
                    rank: r.base.rank,
                },
            );
        }
    }
    if main_rows.is_empty() && wl_rows.is_empty() {
        return Ok(FlushWrite {
            outcome: FlushOutcome::default(),
            running,
            learned: resolved.learned,
        });
    }
    let outcome = FlushOutcome {
        main_rows: main_rows.len(),
        world_bloom_rows: wl_rows.len(),
        lsn: None,
    };
    let timestamps: HashSet<i64> = main_rows
        .iter()
        .map(|row| row.0)
        .chain(wl_rows.iter().map(|row| row.0))
        .collect();

    let time_lookup = resolve_time_ids(conn, engine, event_id, time_tbl, &timestamps).await?;
    let time_id = |ts: &i64| {
        time_lookup
            .get(ts)
            .copied()
            .ok_or_else(|| DbErr::Custom("missing time_id lookup".into()))
    };

    // Chunked inside the one transaction: a backlog after a DB outage
    // must stay under PostgreSQL's 65,535 bind parameters.
    for chunk in main_rows.chunks(INSERT_CHUNK) {
        let mut ins = Query::insert();
        ins.into_table(Alias::new(event_tbl)).columns([
            event::Column::TimeId,
            event::Column::UserIdKey,
            event::Column::Score,
            event::Column::Rank,
        ]);
        for (ts, user_key, score, rank) in chunk {
            ins.values_panic([
                time_id(ts)?.into(),
                (*user_key).into(),
                (*score).into(),
                (*rank).into(),
            ]);
        }
        ins.on_conflict(
            OnConflict::columns([event::Column::TimeId, event::Column::UserIdKey])
                .do_nothing_on([event::Column::TimeId, event::Column::UserIdKey])
                .to_owned(),
        );
        conn.execute(&ins).await?;
    }

    for chunk in wl_rows.chunks(INSERT_CHUNK) {
        let mut ins = Query::insert();
        ins.into_table(Alias::new(wl_tbl)).columns([
            world_bloom::Column::TimeId,
            world_bloom::Column::UserIdKey,
            world_bloom::Column::CharacterId,
            world_bloom::Column::Score,
            world_bloom::Column::Rank,
        ]);
        for (ts, user_key, character_id, score, rank) in chunk {
            ins.values_panic([
                time_id(ts)?.into(),
                (*user_key).into(),
                (*character_id).into(),
                (*score).into(),
                (*rank).into(),
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
        conn.execute(&ins).await?;
    }
    Ok(FlushWrite {
        outcome,
        running,
        learned: resolved.learned,
    })
}

#[cfg(test)]
mod tests {
    use sea_orm::sea_query::{Alias, Expr, Func, Order, Query};
    use sea_orm::{Database, DatabaseBackend, FromQueryResult, Statement};

    use super::*;
    use crate::db::engine::DatabaseEngine;
    use crate::db::query::heartbeat::write_heartbeat;
    use crate::db::query::lines::fetch_ranking_lines;
    use crate::db::query::user::{PublicUserIdMode, get_user_data};
    use crate::db::schema::create_event_tables;
    use crate::model::sekai::{UserCard, UserPlayerFrame, UserProfileHonor};
    use crate::model::tracker::PlayerProfileSchema;

    #[derive(FromQueryResult)]
    struct CountRow {
        n: i64,
    }

    fn record(
        timestamp: i64,
        user_id: &str,
        rank: i64,
        score: i64,
    ) -> PlayerEventRankingRecordSchema {
        PlayerEventRankingRecordSchema {
            timestamp,
            user_id: user_id.into(),
            name: format!("player-{user_id}"),
            score,
            rank,
            cheerful_team_id: None,
            profile: PlayerProfileSchema::default(),
        }
    }

    async fn time_rows_by_id(engine: &DatabaseEngine, event_id: i64) -> Vec<(i64, i64)> {
        let stmt = Query::select()
            .expr_as(Expr::col(time_id::Column::TimeId), Alias::new("time_id"))
            .expr_as(
                Expr::col(time_id::Column::Timestamp),
                Alias::new("timestamp"),
            )
            .from(Alias::new(intern(TableKind::TimeId, event_id)))
            .order_by(time_id::Column::TimeId, Order::Asc)
            .to_owned();
        TimeIdRow::find_by_statement(engine.backend().build(&stmt))
            .all(engine.conn())
            .await
            .unwrap()
            .into_iter()
            .map(|row| (row.time_id, row.timestamp))
            .collect()
    }

    /// Reproduces every allocation path the writer has — a coalesced main
    /// flush carrying several timestamps, a heartbeat written while older
    /// samples are still buffered, a World Bloom batch landing after a main
    /// batch with newer timestamps — and checks id order can't diverge from
    /// timestamp order, so `MAX(time_id)` and "latest by timestamp" agree.
    #[tokio::test]
    async fn time_ids_follow_timestamps_whatever_the_insert_order() {
        let conn = Database::connect("sqlite::memory:").await.unwrap();
        let engine = DatabaseEngine::from_connection(conn, DatabaseBackend::Sqlite);
        let event_id = 7171;
        create_event_tables(&engine, SekaiServerRegion::Jp, event_id, true)
            .await
            .unwrap();
        let t = 1_710_000_000;
        let anonymizer = UidAnonymizer::disabled();

        batch_insert_event_rankings(
            &engine,
            SekaiServerRegion::Jp,
            event_id,
            &anonymizer,
            &[
                record(t + 3, "100", 1, 1_300),
                record(t + 1, "100", 1, 1_100),
            ],
        )
        .await
        .unwrap();
        write_heartbeat(&engine, event_id, t + 5, 1).await.unwrap();
        let wl = PlayerWorldBloomRankingRecordSchema {
            base: record(t + 2, "100", 1, 500),
            character_id: 19,
        };
        batch_insert_world_bloom_rankings(
            &engine,
            SekaiServerRegion::Jp,
            event_id,
            &anonymizer,
            &[wl],
            &mut HashMap::new(),
            &mut UserMemo::default(),
        )
        .await
        .unwrap();
        batch_insert_event_rankings(
            &engine,
            SekaiServerRegion::Jp,
            event_id,
            &anonymizer,
            &[record(t + 4, "100", 1, 1_400)],
        )
        .await
        .unwrap();

        let rows = time_rows_by_id(&engine, event_id).await;
        assert_eq!(
            rows,
            vec![
                (t + 1, t + 1),
                (t + 2, t + 2),
                (t + 3, t + 3),
                (t + 4, t + 4),
                (t + 5, t + 5),
            ]
        );
        let lines = fetch_ranking_lines(&engine, event_id, &[1], None)
            .await
            .unwrap();
        assert_eq!((lines[0].timestamp, lines[0].score), (t + 4, 1_400));
    }

    /// A fresh table probes as aligned and the flush skips the time-id
    /// lookups; a legacy row makes the probe say no and the lookups return.
    #[tokio::test]
    async fn time_id_alignment_is_probed_once_and_picks_the_path() {
        let conn = Database::connect("sqlite::memory:").await.unwrap();
        let engine = DatabaseEngine::from_connection(conn, DatabaseBackend::Sqlite);
        let event_id = 7373;
        create_event_tables(&engine, SekaiServerRegion::Jp, event_id, false)
            .await
            .unwrap();
        let t = 1_710_000_000;
        assert_eq!(engine.time_id_alignment(event_id), None);

        batch_insert_event_rankings(
            &engine,
            SekaiServerRegion::Jp,
            event_id,
            &UidAnonymizer::disabled(),
            &[record(t + 1, "100", 1, 1_001), record(t, "100", 1, 1_000)],
        )
        .await
        .unwrap();
        assert_eq!(engine.time_id_alignment(event_id), Some(true));
        assert_eq!(
            time_rows_by_id(&engine, event_id).await,
            vec![(t, t), (t + 1, t + 1)]
        );
        // Re-flushing an already-present timestamp is a no-op on the time
        // table and dedups the ranking row.
        batch_insert_event_rankings(
            &engine,
            SekaiServerRegion::Jp,
            event_id,
            &UidAnonymizer::disabled(),
            &[
                record(t + 1, "100", 1, 1_001),
                record(t + 2, "100", 1, 1_002),
            ],
        )
        .await
        .unwrap();
        assert_eq!(
            time_rows_by_id(&engine, event_id).await,
            vec![(t, t), (t + 1, t + 1), (t + 2, t + 2)]
        );
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
        assert_eq!(count.n, 3);

        let legacy_event = 7374;
        create_event_tables(&engine, SekaiServerRegion::Jp, legacy_event, false)
            .await
            .unwrap();
        engine
            .conn()
            .execute_raw(Statement::from_string(
                DatabaseBackend::Sqlite,
                format!(
                    "INSERT INTO {} (time_id, timestamp, status) VALUES (5, {t}, 0)",
                    intern(TableKind::TimeId, legacy_event)
                ),
            ))
            .await
            .unwrap();
        assert!(
            !probe_time_id_alignment(engine.conn(), intern(TableKind::TimeId, legacy_event))
                .await
                .unwrap()
        );
        batch_insert_event_rankings(
            &engine,
            SekaiServerRegion::Jp,
            legacy_event,
            &UidAnonymizer::disabled(),
            &[record(t, "100", 1, 1_000)],
        )
        .await
        .unwrap();
        assert_eq!(engine.time_id_alignment(legacy_event), Some(false));
        assert_eq!(time_rows_by_id(&engine, legacy_event).await, vec![(5, t)]);
    }

    #[tokio::test]
    async fn existing_time_id_rows_are_reused_not_reallocated() {
        let conn = Database::connect("sqlite::memory:").await.unwrap();
        let engine = DatabaseEngine::from_connection(conn, DatabaseBackend::Sqlite);
        let event_id = 7272;
        create_event_tables(&engine, SekaiServerRegion::Jp, event_id, false)
            .await
            .unwrap();
        let t = 1_710_000_000;
        // A legacy sequence-numbered row, as written before ids followed
        // timestamps.
        engine
            .conn()
            .execute_raw(Statement::from_string(
                DatabaseBackend::Sqlite,
                format!(
                    "INSERT INTO {} (timestamp, status) VALUES ({t}, 0)",
                    intern(TableKind::TimeId, event_id)
                ),
            ))
            .await
            .unwrap();

        batch_insert_event_rankings(
            &engine,
            SekaiServerRegion::Jp,
            event_id,
            &UidAnonymizer::disabled(),
            &[record(t, "100", 1, 1_000), record(t + 1, "100", 1, 1_001)],
        )
        .await
        .unwrap();
        write_heartbeat(&engine, event_id, t, 1).await.unwrap();

        assert_eq!(
            time_rows_by_id(&engine, event_id).await,
            vec![(1, t), (t + 1, t + 1)]
        );
        let stmt = Query::select()
            .expr_as(
                Func::count(Expr::col(event::Column::TimeId)),
                Alias::new("n"),
            )
            .from(Alias::new(intern(TableKind::Event, event_id)))
            .and_where(Expr::col(event::Column::TimeId).eq(1))
            .to_owned();
        let count = CountRow::find_by_statement(engine.backend().build(&stmt))
            .one(engine.conn())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(count.n, 1);
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

        let mut memo = UserMemo::default();
        batch_upsert_event_users(
            &engine,
            SekaiServerRegion::Jp,
            event_id,
            &UidAnonymizer::disabled(),
            &records,
            &mut memo,
        )
        .await
        .unwrap();
        assert_eq!(memo.len(), 1);

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

    #[test]
    fn collect_users_keeps_the_newest_sample_of_each_user() {
        let t = 1_710_000_000;
        let mut newest = record(t + 2, "100", 1, 1_200);
        newest.name = "renamed".into();
        newest.cheerful_team_id = Some(2);
        let mut older = record(t + 1, "100", 1, 1_100);
        older.name = "old".into();
        older.cheerful_team_id = Some(1);
        let mut oldest = record(t, "100", 1, 1_000);
        oldest.name = "oldest".into();
        let other = record(t, "200", 2, 900);

        // Main buffer in sample order, then the World Bloom buffer chained
        // after it carrying an *earlier* sample of the same user.
        let main = [oldest.clone(), newest.clone(), other.clone()];
        let wl = [older.clone()];
        let users = collect_users(
            SekaiServerRegion::Jp,
            1,
            &UidAnonymizer::disabled(),
            main.iter().chain(wl.iter()),
        )
        .unwrap();
        assert_eq!(users.len(), 2);
        assert_eq!(users[&100].name, "renamed");
        assert_eq!(users[&100].cheerful_team_id, Some(2));
        assert_eq!(users[&200].name, "player-200");

        // Same timestamp: the later occurrence wins.
        let mut same_ts = record(t + 2, "100", 1, 1_300);
        same_ts.name = "later".into();
        let main = [newest, same_ts];
        let users = collect_users(
            SekaiServerRegion::Jp,
            1,
            &UidAnonymizer::disabled(),
            main.iter(),
        )
        .unwrap();
        assert_eq!(users[&100].name, "later");
    }

    async fn stored_name(engine: &DatabaseEngine, event_id: i64, user_id: &str) -> String {
        get_user_data(engine, event_id, user_id, PublicUserIdMode::Raw)
            .await
            .unwrap()
            .unwrap()
            .name
    }

    /// A user the memo holds with unchanged values is neither read back
    /// nor rewritten: an out-of-band edit of the stored row survives the
    /// next flush of that user. Once the payload changes, the row is
    /// rewritten and the memo follows; a stored `cheerful_team_id` is kept
    /// when the payload has none.
    #[tokio::test]
    async fn memoized_users_skip_the_read_back_and_upsert() {
        let conn = Database::connect("sqlite::memory:").await.unwrap();
        let engine = DatabaseEngine::from_connection(conn, DatabaseBackend::Sqlite);
        let event_id = 8181;
        create_event_tables(&engine, SekaiServerRegion::Jp, event_id, false)
            .await
            .unwrap();
        let t = 1_710_000_000;
        let anonymizer = UidAnonymizer::disabled();
        let mut memo = UserMemo::default();
        let mut first = record(t, "100", 1, 1_000);
        first.cheerful_team_id = Some(2);

        batch_insert_flush(
            &engine,
            SekaiServerRegion::Jp,
            event_id,
            &anonymizer,
            std::slice::from_ref(&first),
            &[],
            &mut HashMap::new(),
            &mut memo,
        )
        .await
        .unwrap();
        let entry = memo.get(100).unwrap().clone();
        assert_eq!(entry.name, "player-100");
        assert_eq!(entry.cheerful_team_id, Some(2));
        assert!(entry.profile_hash.is_some());

        engine
            .conn()
            .execute_unprepared(&format!(
                "UPDATE {} SET name = 'edited' WHERE user_id = '100'",
                intern(TableKind::EventUsers, event_id)
            ))
            .await
            .unwrap();
        let mut again = record(t + 1, "100", 1, 1_001);
        again.cheerful_team_id = None;
        batch_insert_flush(
            &engine,
            SekaiServerRegion::Jp,
            event_id,
            &anonymizer,
            std::slice::from_ref(&again),
            &[],
            &mut HashMap::new(),
            &mut memo,
        )
        .await
        .unwrap();
        assert_eq!(stored_name(&engine, event_id, "100").await, "edited");
        assert_eq!(memo.get(100), Some(&entry));

        let mut renamed = record(t + 2, "100", 1, 1_002);
        renamed.name = "renamed".into();
        batch_insert_flush(
            &engine,
            SekaiServerRegion::Jp,
            event_id,
            &anonymizer,
            std::slice::from_ref(&renamed),
            &[],
            &mut HashMap::new(),
            &mut memo,
        )
        .await
        .unwrap();
        assert_eq!(stored_name(&engine, event_id, "100").await, "renamed");
        let entry = memo.get(100).unwrap();
        assert_eq!(entry.name, "renamed");
        assert_eq!(
            entry.cheerful_team_id,
            Some(2),
            "stored team survives a NULL payload"
        );
        assert_eq!(entry.user_id_key, memo.user_id_key(100).unwrap());

        // A fresh memo (writer restart) reads the row back and relearns it
        // without rewriting anything.
        let mut fresh = UserMemo::default();
        let mut same = record(t + 3, "100", 1, 1_003);
        same.name = "renamed".into();
        batch_insert_flush(
            &engine,
            SekaiServerRegion::Jp,
            event_id,
            &anonymizer,
            std::slice::from_ref(&same),
            &[],
            &mut HashMap::new(),
            &mut fresh,
        )
        .await
        .unwrap();
        assert_eq!(fresh.get(100), memo.get(100));
        let lines = fetch_ranking_lines(&engine, event_id, &[1], None)
            .await
            .unwrap();
        assert_eq!((lines[0].timestamp, lines[0].score), (t + 3, 1_003));
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
        let mut user_keys = UserMemo::default();
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
    /// `HET_TEST_PG_URL=postgres://... cargo test --lib -- --ignored
    /// flush_on_postgres`.
    #[tokio::test]
    #[ignore = "needs HET_TEST_PG_URL"]
    async fn flush_on_postgres_commits_on_one_connection_and_reports_the_lsn() {
        use std::time::Duration;

        use crate::db::pg_session::tests::pg_engine;

        let Some(engine) = pg_engine(Duration::from_secs(5)).await else {
            return;
        };
        let event_id = 900_000 + (chrono::Utc::now().timestamp() % 100_000);
        for kind in [
            TableKind::WorldBloom,
            TableKind::Event,
            TableKind::EventUsers,
            TableKind::TimeId,
        ] {
            engine
                .conn()
                .execute_unprepared(&format!("DROP TABLE IF EXISTS {}", intern(kind, event_id)))
                .await
                .unwrap();
        }
        create_event_tables(&engine, SekaiServerRegion::Jp, event_id, true)
            .await
            .unwrap();
        let t = 1_710_000_000;
        let anonymizer = UidAnonymizer::disabled();
        let mut prev_state = HashMap::new();
        let mut user_keys = UserMemo::default();
        let wl = PlayerWorldBloomRankingRecordSchema {
            base: record(t + 1, "100", 1, 500),
            character_id: 19,
        };

        let outcome = batch_insert_flush(
            &engine,
            SekaiServerRegion::Jp,
            event_id,
            &anonymizer,
            &[record(t, "100", 1, 1_000), record(t + 1, "200", 2, 900)],
            std::slice::from_ref(&wl),
            &mut prev_state,
            &mut user_keys,
        )
        .await
        .unwrap();
        assert_eq!((outcome.main_rows, outcome.world_bloom_rows), (2, 1));
        let lsn = outcome.lsn.expect("postgres flush reports the lsn");
        assert!(lsn.contains('/'), "{lsn}");
        assert_eq!(engine.time_id_alignment(event_id), Some(true));
        assert_eq!(
            time_rows_by_id(&engine, event_id).await,
            vec![(t, t), (t + 1, t + 1)]
        );
        assert_eq!(prev_state.len(), 1);
        assert_eq!(user_keys.len(), 2);

        // Retry of the same window: nothing duplicated, and every pool
        // connection is back to idle (none stuck in a release ping).
        let again = batch_insert_flush(
            &engine,
            SekaiServerRegion::Jp,
            event_id,
            &anonymizer,
            &[record(t, "100", 1, 1_000)],
            &[wl],
            &mut prev_state,
            &mut user_keys,
        )
        .await
        .unwrap();
        assert_eq!((again.main_rows, again.world_bloom_rows), (1, 0));
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
        assert_eq!(count.n, 2);
        let lines = fetch_ranking_lines(&engine, event_id, &[1, 2], None)
            .await
            .unwrap();
        assert_eq!(lines.len(), 2);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let pool = engine.conn().get_postgres_connection_pool();
        assert_eq!(pool.num_idle(), pool.size() as usize);

        // A statement failure inside the flush rolls back on the same
        // connection: the users upsert from that attempt is gone too.
        engine
            .conn()
            .execute_unprepared(&format!(
                "ALTER TABLE {} ADD CONSTRAINT het_block CHECK (score < 5000)",
                intern(TableKind::Event, event_id)
            ))
            .await
            .unwrap();
        let err = batch_insert_flush(
            &engine,
            SekaiServerRegion::Jp,
            event_id,
            &anonymizer,
            &[record(t + 5, "300", 3, 9_999)],
            &[],
            &mut prev_state,
            &mut user_keys,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("het_block"), "{err}");
        assert!(
            get_user_data(&engine, event_id, "300", PublicUserIdMode::Raw)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            time_rows_by_id(&engine, event_id).await,
            vec![(t, t), (t + 1, t + 1)]
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(pool.num_idle(), pool.size() as usize);
    }
}
