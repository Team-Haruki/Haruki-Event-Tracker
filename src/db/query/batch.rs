//! Transactional batch inserts plus their two helper lookups
//! (Go: `BatchInsertEventRankings`, `BatchInsertWorldBloomRankings`,
//! `batchGetOrCreateTimeIDs`, `batchGetOrCreateUserIDKeys`).
//!
//! One flush is one transaction: the user dimension upsert, the time-id
//! rows and the ranking rows commit together. On PostgreSQL the whole
//! flush — including the post-commit `pg_current_wal_lsn()` the cluster
//! invalidation needs — runs on a single pooled connection through
//! `db::pg_session` (`write_timeout` enforced with a hard close), and on
//! an aligned time table it is one simple-protocol message: `BEGIN`, one
//! statement of data-modifying CTEs (users upsert, time rows, ranking
//! rows), `COMMIT`, the LSN — a single round trip ([`pg_flush_message`]).
//! Other dialects (and legacy time tables) use the shared sea-query
//! statement builders inside a transaction.
//!
//! The writer is the only process that writes an event's `users` table,
//! so it remembers what it wrote ([`UserMemo`]): a user whose incoming
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
    users: &HashMap<i64, &UserDimRow>,
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
        let info = users[&uid];
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
    users: &HashMap<i64, &UserDimRow>,
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
            let info = users[&uid];
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

pub(crate) fn parse_uid(user_id: &str) -> Result<i64, DbErr> {
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
    let users: HashMap<i64, &UserDimRow> = users.iter().map(|(&uid, u)| (uid, u)).collect();

    let resolved =
        batch_get_or_create_user_id_keys(engine.conn(), backend, users_tbl, &users, memo).await?;
    memo.absorb(resolved.learned);
    Ok(())
}

/// One ranking row as the tracker buffers it: the four stored columns and
/// nothing else, so a backlog costs 32 bytes per row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SampleRow {
    pub timestamp: i64,
    pub uid: i64,
    pub score: i64,
    pub rank: i64,
}

impl SampleRow {
    pub fn from_record(r: &PlayerEventRankingRecordSchema) -> Result<Self, DbErr> {
        Ok(Self {
            timestamp: r.timestamp,
            uid: parse_uid(&r.user_id)?,
            score: r.score,
            rank: r.rank,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorldBloomSampleRow {
    pub row: SampleRow,
    pub character_id: i64,
}

/// The rows of one flush plus the dimension values they need. `profiles`
/// holds the newest values per uid for every user whose stored row may
/// differ from what the memo holds; a uid absent from it must be in the
/// memo (its key is taken from there and the users table is not touched).
/// The map may be a superset of the users the rows reference.
#[derive(Debug, Clone, Copy)]
pub struct FlushBatch<'a> {
    pub main: &'a [SampleRow],
    pub world_bloom: &'a [WorldBloomSampleRow],
    pub profiles: &'a HashMap<i64, UserDimRow>,
}

impl FlushBatch<'_> {
    pub fn is_empty(&self) -> bool {
        self.main.is_empty() && self.world_bloom.is_empty()
    }

    /// Distinct uids the rows reference.
    fn uids(&self) -> HashSet<i64> {
        self.main
            .iter()
            .map(|r| r.uid)
            .chain(self.world_bloom.iter().map(|r| r.row.uid))
            .collect()
    }
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
    event_id: i64,
    batch: FlushBatch<'a>,
    prev_state: &'a HashMap<WorldBloomKey, PlayerState>,
    memo: &'a UserMemo,
}

/// What a committed flush hands back to the caller's in-memory state.
struct FlushWrite {
    outcome: FlushOutcome,
    running: HashMap<WorldBloomKey, PlayerState>,
    learned: Vec<(i64, UserMemoEntry)>,
}

/// Record-shaped entry to [`flush_batch`]: every record's dimension values
/// are offered as the profile (newest sample per user wins, see
/// [`collect_users`]), so nothing is assumed about the memo.
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
    let (main_rows, wl_rows, profiles) =
        record_batch(server, event_id, anonymizer, main, world_bloom)?;
    flush_batch(
        engine,
        event_id,
        FlushBatch {
            main: &main_rows,
            world_bloom: &wl_rows,
            profiles: &profiles,
        },
        prev_state,
        memo,
    )
    .await
}

/// The owned parts of a [`FlushBatch`]: compact rows and profiles.
pub(crate) type RecordBatch = (
    Vec<SampleRow>,
    Vec<WorldBloomSampleRow>,
    HashMap<i64, UserDimRow>,
);

/// The owned parts of a [`FlushBatch`] for record-shaped input: compact
/// rows and every user's newest dimension values.
pub(crate) fn record_batch(
    server: SekaiServerRegion,
    event_id: i64,
    anonymizer: &UidAnonymizer,
    main: &[PlayerEventRankingRecordSchema],
    world_bloom: &[PlayerWorldBloomRankingRecordSchema],
) -> Result<RecordBatch, DbErr> {
    let profiles = collect_users(
        server,
        event_id,
        anonymizer,
        main.iter().chain(world_bloom.iter().map(|r| &r.base)),
    )?;
    let main_rows = main
        .iter()
        .map(SampleRow::from_record)
        .collect::<Result<Vec<_>, _>>()?;
    let wl_rows = world_bloom
        .iter()
        .map(|r| {
            Ok(WorldBloomSampleRow {
                row: SampleRow::from_record(&r.base)?,
                character_id: r.character_id,
            })
        })
        .collect::<Result<Vec<_>, DbErr>>()?;
    Ok((main_rows, wl_rows, profiles))
}

/// Writes one tracker flush — main and World Bloom rows of every sample in
/// `batch` — in a single transaction, so a reader (or a streaming replica)
/// sees either none of it or all of it: never a sample with only some of
/// its rank moves, and never main rows without the chapter rows sampled
/// with them. The tracker only ever hands over whole samples.
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
#[tracing::instrument(
    skip(engine, batch, prev_state, memo),
    fields(event_id, main = batch.main.len(), world_bloom = batch.world_bloom.len())
)]
pub async fn flush_batch(
    engine: &DatabaseEngine,
    event_id: i64,
    batch: FlushBatch<'_>,
    prev_state: &mut HashMap<WorldBloomKey, PlayerState>,
    memo: &mut UserMemo,
) -> Result<FlushOutcome, DbErr> {
    flush_batch_with(engine, event_id, batch, prev_state, memo, PgFlushMode::Auto).await
}

/// How a PostgreSQL flush reaches the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PgFlushMode {
    /// One simple-protocol message when the event's time table is aligned
    /// (`time_id == timestamp`), the statement path otherwise.
    Auto,
    /// The shared statement builders inside `BEGIN`/`COMMIT`, one round
    /// trip per statement — what every other dialect runs. Forced by the
    /// equivalence tests; production picks it through `Auto`.
    #[cfg_attr(not(test), allow(dead_code))]
    Statements,
}

pub(crate) async fn flush_batch_with(
    engine: &DatabaseEngine,
    event_id: i64,
    batch: FlushBatch<'_>,
    prev_state: &mut HashMap<WorldBloomKey, PlayerState>,
    memo: &mut UserMemo,
    mode: PgFlushMode,
) -> Result<FlushOutcome, DbErr> {
    if batch.is_empty() {
        return Ok(FlushOutcome::default());
    }
    let input = FlushInput {
        event_id,
        batch,
        prev_state,
        memo,
    };
    let written = match engine.backend() {
        DatabaseBackend::Postgres => flush_on_postgres(engine, &input, mode).await?,
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

/// The PostgreSQL flush, on one pooled connection under `write_timeout`.
///
/// On an aligned time table the whole flush is one simple-protocol
/// message ([`pg_flush_message`]): `BEGIN` with the server-side timeouts,
/// one statement whose data-modifying CTEs upsert the users, insert the
/// time rows and the ranking rows, `COMMIT`, and the WAL position — one
/// round trip. A legacy table (or a World Bloom batch that needs keys
/// the memo lacks to diff against `prev_state`) takes the statement
/// path: the shared builders between `BEGIN` and `COMMIT` + LSN.
///
/// Either way a failure rolls back on the same connection; if the
/// rollback itself fails the connection is discarded.
async fn flush_on_postgres(
    engine: &DatabaseEngine,
    input: &FlushInput<'_>,
    mode: PgFlushMode,
) -> Result<FlushWrite, DbErr> {
    if mode == PgFlushMode::Auto && single_message_applies(engine, input).await? {
        return flush_on_postgres_message(engine, input).await;
    }
    flush_on_postgres_statements(engine, input).await
}

/// The single message needs derived time ids (an aligned table) and, to
/// diff World Bloom rows against `prev_state` (keyed by `user_id_key`),
/// the key of every World Bloom user — unknown users are new only when
/// there is no state to diff against.
async fn single_message_applies(
    engine: &DatabaseEngine,
    input: &FlushInput<'_>,
) -> Result<bool, DbErr> {
    let event_id = input.event_id;
    let aligned = match engine.time_id_alignment(event_id) {
        Some(aligned) => aligned,
        None => {
            let aligned =
                probe_time_id_alignment(engine.conn(), intern(TableKind::TimeId, event_id)).await?;
            tracing::info!(event_id, aligned, "probed time_id alignment");
            engine.set_time_id_alignment(event_id, aligned);
            aligned
        }
    };
    if !aligned {
        return Ok(false);
    }
    Ok(input.prev_state.is_empty()
        || input
            .batch
            .world_bloom
            .iter()
            .all(|r| input.memo.user_id_key(r.row.uid).is_some()))
}

/// One round trip: the message from [`pg_flush_message`], then the
/// returned users rows (keys of the users just upserted) and the LSN.
async fn flush_on_postgres_message(
    engine: &DatabaseEngine,
    input: &FlushInput<'_>,
) -> Result<FlushWrite, DbErr> {
    let plan = pg_flush_message(input, engine.write_timeout())?;
    let Some(sql) = plan.sql else {
        return Ok(FlushWrite {
            outcome: FlushOutcome::default(),
            running: HashMap::new(),
            learned: Vec::new(),
        });
    };
    let rows = with_writer_session(engine, |session| async move {
        match session.query_simple(sql).await {
            Ok(rows) => Ok(rows),
            Err(err) => {
                session.rollback_or_mark_broken().await;
                Err(err)
            }
        }
    })
    .await?;
    plan.bookkeeping.complete(input, rows)
}

/// The statement path: `BEGIN` with the server-side timeouts, every
/// statement, then `COMMIT` and the WAL position in one round trip.
async fn flush_on_postgres_statements(
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

/// Users offered with a profile, and keys of the memoized rest.
type SplitUsers<'a> = (HashMap<i64, &'a UserDimRow>, HashMap<i64, i64>);

/// Partition the users a batch references: those with a profile offered
/// go through the users table (the memo may still short-circuit them),
/// the rest must be memoized and are answered from memory.
fn split_users<'a>(batch: &FlushBatch<'a>, memo: &UserMemo) -> Result<SplitUsers<'a>, DbErr> {
    let mut offered = HashMap::new();
    let mut keys = HashMap::new();
    for uid in batch.uids() {
        if let Some(info) = batch.profiles.get(&uid) {
            offered.insert(uid, info);
        } else if let Some(key) = memo.user_id_key(uid) {
            keys.insert(uid, key);
        } else {
            return Err(DbErr::Custom(format!(
                "user {uid} has neither a profile in the batch nor a memo entry"
            )));
        }
    }
    Ok((offered, keys))
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

    let batch = input.batch;
    let (users, mut keys) = split_users(&batch, input.memo)?;
    let resolved =
        batch_get_or_create_user_id_keys(conn, backend, users_tbl, &users, input.memo).await?;
    keys.extend(resolved.keys);
    let user_key = |uid: i64| {
        keys.get(&uid)
            .copied()
            .ok_or_else(|| DbErr::Custom("missing user_id_key lookup".into()))
    };

    let mut main_rows: Vec<(i64, i64, i64, i64)> = Vec::with_capacity(batch.main.len());
    for r in batch.main {
        main_rows.push((r.timestamp, user_key(r.uid)?, r.score, r.rank));
    }
    let mut wl_rows: Vec<(i64, i64, i64, i64, i64)> = Vec::new();
    let mut running: HashMap<WorldBloomKey, PlayerState> = HashMap::new();
    for r in batch.world_bloom {
        let user_key = user_key(r.row.uid)?;
        let key = WorldBloomKey {
            user_id_key: user_key,
            character_id: r.character_id,
        };
        let last = running
            .get(&key)
            .copied()
            .or_else(|| input.prev_state.get(&key).copied());
        if last.is_none_or(|p| p.score != r.row.score || p.rank != r.row.rank) {
            wl_rows.push((
                r.row.timestamp,
                user_key,
                r.character_id,
                r.row.score,
                r.row.rank,
            ));
            running.insert(
                key,
                PlayerState {
                    score: r.row.score,
                    rank: r.row.rank,
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

/// The single-message plan: `sql` is `None` when the flush has nothing to
/// write (every World Bloom row deduped and no user to upsert).
struct PgFlushPlan {
    sql: Option<String>,
    bookkeeping: PgFlushBookkeeping,
}

/// What the client must remember to turn the message's reply into a
/// [`FlushWrite`]: the users it upserted (their keys and resolved team
/// come back in the `RETURNING` rows), the keys it already knew, and the
/// World Bloom state it wrote keyed by uid (the keys of new users are
/// only known afterwards).
struct PgFlushBookkeeping {
    upserted: Vec<i64>,
    use_unique_ids: bool,
    keys: HashMap<i64, i64>,
    running_by_uid: HashMap<(i64, i64), PlayerState>,
    outcome: FlushOutcome,
}

impl PgFlushBookkeeping {
    fn complete(
        mut self,
        input: &FlushInput<'_>,
        mut rows: Vec<sea_orm::QueryResult>,
    ) -> Result<FlushWrite, DbErr> {
        let lsn = rows
            .pop()
            .map(|row| row.try_get::<String>("", "lsn"))
            .transpose()?;
        let mut returned: HashMap<i64, (i64, Option<i64>)> = HashMap::with_capacity(rows.len());
        for row in rows {
            let uid = parse_uid(&row.try_get::<String>("", "user_id")?)?;
            let key = row.try_get::<i64>("", "user_id_key")?;
            let team = row.try_get::<Option<i64>>("", "cheerful_team_id")?;
            returned.insert(uid, (key, team));
        }
        let mut learned = Vec::with_capacity(self.upserted.len());
        for uid in &self.upserted {
            let Some(&(key, cheerful_team_id)) = returned.get(uid) else {
                return Err(DbErr::Custom(format!(
                    "users upsert returned no row for user {uid}"
                )));
            };
            let info = &input.batch.profiles[uid];
            self.keys.insert(*uid, key);
            learned.push((
                *uid,
                UserMemoEntry {
                    user_id_key: key,
                    name: info.name.clone(),
                    cheerful_team_id,
                    unique_id: self
                        .use_unique_ids
                        .then(|| info.unique_id.clone())
                        .flatten(),
                    profile_hash: Some(info.profile_hash),
                },
            ));
        }
        let mut running = HashMap::with_capacity(self.running_by_uid.len());
        for ((uid, character_id), state) in self.running_by_uid {
            let Some(&user_id_key) = self.keys.get(&uid) else {
                return Err(DbErr::Custom(format!("no user_id_key for user {uid}")));
            };
            running.insert(
                WorldBloomKey {
                    user_id_key,
                    character_id,
                },
                state,
            );
        }
        self.outcome.lsn = lsn;
        Ok(FlushWrite {
            outcome: self.outcome,
            running,
            learned,
        })
    }
}

/// Build the flush as one simple-protocol message:
///
/// ```text
/// BEGIN; SET LOCAL statement_timeout ...;
/// WITH u AS (INSERT INTO users ... SELECT * FROM unnest(...)
///            ON CONFLICT (user_id) DO UPDATE ... RETURNING ...),
///      t AS (INSERT INTO time_id ... FROM unnest(...) ON CONFLICT DO NOTHING),
///      e AS (INSERT INTO event ... FROM unnest(...) LEFT JOIN u ...
///            ON CONFLICT DO NOTHING),
///      w AS (... world bloom ...)
/// SELECT user_id, user_id_key, cheerful_team_id FROM u;
/// COMMIT; SELECT pg_current_wal_lsn()
/// ```
///
/// Values are inlined as literals (integer arrays, `E'...'` strings)
/// because the extended protocol cannot put `COMMIT` and the post-commit
/// LSN read in the same round trip as a bound statement; the message is
/// planned per flush, which costs well under a millisecond. Everything
/// the old path did is preserved: the users upsert never overwrites a
/// stored `cheerful_team_id` with NULL (`COALESCE`), a row of a user the
/// memo does not know gets its key from the upsert's `RETURNING` through
/// the `LEFT JOIN u`, the time rows are `time_id = timestamp`, and every
/// ranking insert is `ON CONFLICT DO NOTHING` so a retry after a lost
/// reply is harmless.
fn pg_flush_message(
    input: &FlushInput<'_>,
    write_timeout: Option<std::time::Duration>,
) -> Result<PgFlushPlan, DbErr> {
    let batch = input.batch;
    let event_id = input.event_id;
    let (offered, mut keys) = split_users(&batch, input.memo)?;
    let use_unique_ids = offered.values().any(|u| u.unique_id.is_some());
    let mut upserted: Vec<i64> = Vec::new();
    for (&uid, info) in &offered {
        match input.memo.current_key(uid, info) {
            Some(key) => {
                keys.insert(uid, key);
            }
            None => {
                upserted.push(uid);
                // A changed profile still has its key when the memo knows
                // the user: the World Bloom diff below needs it.
                if let Some(key) = input.memo.user_id_key(uid) {
                    keys.insert(uid, key);
                }
            }
        }
    }
    upserted.sort_unstable();

    // World Bloom diff, mirroring `write_flush`: a user without a key is
    // new to this writer and cannot be in `prev_state` (the caller checked
    // `single_message_applies`), so its rows are always changed.
    let mut running_by_uid: HashMap<(i64, i64), PlayerState> = HashMap::new();
    let mut wl_rows: Vec<&WorldBloomSampleRow> = Vec::new();
    for r in batch.world_bloom {
        let uid = r.row.uid;
        let last = running_by_uid
            .get(&(uid, r.character_id))
            .copied()
            .or_else(|| {
                let user_id_key = *keys.get(&uid)?;
                input
                    .prev_state
                    .get(&WorldBloomKey {
                        user_id_key,
                        character_id: r.character_id,
                    })
                    .copied()
            });
        if last.is_none_or(|p| p.score != r.row.score || p.rank != r.row.rank) {
            wl_rows.push(r);
            running_by_uid.insert(
                (uid, r.character_id),
                PlayerState {
                    score: r.row.score,
                    rank: r.row.rank,
                },
            );
        }
    }
    let outcome = FlushOutcome {
        main_rows: batch.main.len(),
        world_bloom_rows: wl_rows.len(),
        lsn: None,
    };
    let bookkeeping = PgFlushBookkeeping {
        upserted: upserted.clone(),
        use_unique_ids,
        keys,
        running_by_uid,
        outcome,
    };
    if batch.main.is_empty() && wl_rows.is_empty() && upserted.is_empty() {
        return Ok(PgFlushPlan {
            sql: None,
            bookkeeping,
        });
    }

    let mut sql = begin_sql(write_timeout);
    sql.push_str("; WITH ");
    let mut ctes: Vec<String> = Vec::new();
    if !upserted.is_empty() {
        ctes.push(users_upsert_cte(
            intern(TableKind::EventUsers, event_id),
            &upserted,
            &offered,
            use_unique_ids,
        )?);
    }
    let has_rows = !batch.main.is_empty() || !wl_rows.is_empty();
    if has_rows {
        let mut timestamps: Vec<i64> = batch
            .main
            .iter()
            .map(|r| r.timestamp)
            .chain(wl_rows.iter().map(|r| r.row.timestamp))
            .collect();
        timestamps.sort_unstable();
        timestamps.dedup();
        let mut t = format!(
            "t AS (INSERT INTO {} ({}, {}, {}) SELECT ts, ts, 0 FROM unnest(",
            quote(intern(TableKind::TimeId, event_id)),
            quote("time_id"),
            quote("timestamp"),
            quote("status"),
        );
        push_int_array(&mut t, timestamps.iter().map(|&ts| Some(ts)));
        t.push_str(") AS ts ON CONFLICT (\"timestamp\") DO NOTHING)");
        ctes.push(t);
    }
    let joined = !upserted.is_empty();
    let key_of = |uid: i64| bookkeeping.keys.get(&uid).copied();
    if !batch.main.is_empty() {
        let mut e = format!(
            "e AS (INSERT INTO {} ({}, {}, {}, {}) SELECT r.ts, {}, r.score, r.rank FROM unnest(",
            quote(intern(TableKind::Event, event_id)),
            quote("time_id"),
            quote("user_id_key"),
            quote("score"),
            quote("rank"),
            if joined {
                "COALESCE(r.key, u.user_id_key)"
            } else {
                "r.key"
            },
        );
        push_int_array(&mut e, batch.main.iter().map(|r| Some(r.timestamp)));
        e.push_str(", ");
        push_int_array(&mut e, batch.main.iter().map(|r| key_of(r.uid)));
        e.push_str(", ");
        push_int_array(&mut e, batch.main.iter().map(|r| Some(r.uid)));
        e.push_str(", ");
        push_int_array(&mut e, batch.main.iter().map(|r| Some(r.score)));
        e.push_str(", ");
        push_int_array(&mut e, batch.main.iter().map(|r| Some(r.rank)));
        e.push_str(") AS r(ts, key, uid, score, rank)");
        if joined {
            e.push_str(" LEFT JOIN u ON u.user_id = r.uid::text");
        }
        e.push_str(" ON CONFLICT (\"time_id\", \"user_id_key\") DO NOTHING)");
        ctes.push(e);
    }
    if !wl_rows.is_empty() {
        let mut w = format!(
            "w AS (INSERT INTO {} ({}, {}, {}, {}, {}) SELECT r.ts, {}, r.character_id, r.score, r.rank FROM unnest(",
            quote(intern(TableKind::WorldBloom, event_id)),
            quote("time_id"),
            quote("user_id_key"),
            quote("character_id"),
            quote("score"),
            quote("rank"),
            if joined {
                "COALESCE(r.key, u.user_id_key)"
            } else {
                "r.key"
            },
        );
        push_int_array(&mut w, wl_rows.iter().map(|r| Some(r.row.timestamp)));
        w.push_str(", ");
        push_int_array(&mut w, wl_rows.iter().map(|r| key_of(r.row.uid)));
        w.push_str(", ");
        push_int_array(&mut w, wl_rows.iter().map(|r| Some(r.row.uid)));
        w.push_str(", ");
        push_int_array(&mut w, wl_rows.iter().map(|r| Some(r.character_id)));
        w.push_str(", ");
        push_int_array(&mut w, wl_rows.iter().map(|r| Some(r.row.score)));
        w.push_str(", ");
        push_int_array(&mut w, wl_rows.iter().map(|r| Some(r.row.rank)));
        w.push_str(") AS r(ts, key, uid, character_id, score, rank)");
        if joined {
            w.push_str(" LEFT JOIN u ON u.user_id = r.uid::text");
        }
        w.push_str(" ON CONFLICT (\"time_id\", \"user_id_key\", \"character_id\") DO NOTHING)");
        ctes.push(w);
    }
    sql.push_str(&ctes.join(", "));
    if joined {
        sql.push_str(" SELECT user_id, user_id_key, cheerful_team_id FROM u");
    } else {
        sql.push_str(" SELECT 0 WHERE false");
    }
    sql.push_str("; COMMIT; SELECT pg_current_wal_lsn()::text AS lsn");
    Ok(PgFlushPlan {
        sql: Some(sql),
        bookkeeping,
    })
}

/// `u AS (INSERT INTO users AS tbl (...) SELECT * FROM unnest(...) ON
/// CONFLICT (user_id) DO UPDATE SET ... RETURNING ...)`.
fn users_upsert_cte(
    table: &str,
    upserted: &[i64],
    offered: &HashMap<i64, &UserDimRow>,
    use_unique_ids: bool,
) -> Result<String, DbErr> {
    let mut columns: Vec<&str> = vec![
        "user_id",
        "name",
        "cheerful_team_id",
        "card_id",
        "card_level",
        "card_master_rank",
        "card_special_training_status",
        "card_default_image",
        "profile_word",
        "profile_honors_json",
        "honor_missions_json",
        "player_frames_json",
        "profile_hash",
    ];
    if use_unique_ids {
        columns.push("unique_id");
    }
    let quoted: Vec<String> = columns.iter().map(|c| quote(c)).collect();
    let mut u = format!(
        "u AS (INSERT INTO {} AS tbl ({}) SELECT * FROM unnest(",
        quote(table),
        quoted.join(", ")
    );
    let rows: Vec<&UserDimRow> = upserted.iter().map(|uid| offered[uid]).collect();
    let ids: Vec<String> = upserted.iter().map(i64::to_string).collect();
    push_text_array(&mut u, ids.iter().map(|id| Some(id.as_str())))?;
    u.push_str(", ");
    push_text_array(&mut u, rows.iter().map(|r| Some(r.name.as_str())))?;
    u.push_str(", ");
    push_int_array(&mut u, rows.iter().map(|r| r.cheerful_team_id));
    u.push_str(", ");
    push_int_array(&mut u, rows.iter().map(|r| r.card_id));
    u.push_str(", ");
    push_int_array(&mut u, rows.iter().map(|r| r.card_level));
    u.push_str(", ");
    push_int_array(&mut u, rows.iter().map(|r| r.card_master_rank));
    u.push_str(", ");
    push_text_array(
        &mut u,
        rows.iter()
            .map(|r| r.card_special_training_status.as_deref()),
    )?;
    u.push_str(", ");
    push_text_array(&mut u, rows.iter().map(|r| r.card_default_image.as_deref()))?;
    u.push_str(", ");
    push_text_array(&mut u, rows.iter().map(|r| r.profile_word.as_deref()))?;
    u.push_str(", ");
    push_text_array(
        &mut u,
        rows.iter().map(|r| r.profile_honors_json.as_deref()),
    )?;
    u.push_str(", ");
    push_text_array(
        &mut u,
        rows.iter().map(|r| r.honor_missions_json.as_deref()),
    )?;
    u.push_str(", ");
    push_text_array(&mut u, rows.iter().map(|r| r.player_frames_json.as_deref()))?;
    u.push_str(", ");
    push_int_array(&mut u, rows.iter().map(|r| Some(r.profile_hash)));
    if use_unique_ids {
        u.push_str(", ");
        push_text_array(&mut u, rows.iter().map(|r| r.unique_id.as_deref()))?;
    }
    u.push_str(") ON CONFLICT (\"user_id\") DO UPDATE SET ");
    let updates: Vec<String> = columns
        .iter()
        .skip(1)
        .map(|c| {
            let q = quote(c);
            if *c == "cheerful_team_id" {
                format!("{q} = COALESCE(EXCLUDED.{q}, tbl.{q})")
            } else {
                format!("{q} = EXCLUDED.{q}")
            }
        })
        .collect();
    u.push_str(&updates.join(", "));
    u.push_str(" RETURNING \"user_id\", \"user_id_key\", \"cheerful_team_id\")");
    Ok(u)
}

fn quote(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// `ARRAY[1, NULL, 3]::bigint[]`.
fn push_int_array(out: &mut String, values: impl Iterator<Item = Option<i64>>) {
    use std::fmt::Write;
    out.push_str("ARRAY[");
    for (i, v) in values.enumerate() {
        if i > 0 {
            out.push(',');
        }
        match v {
            Some(v) => write!(out, "{v}").expect("writing to a String cannot fail"),
            None => out.push_str("NULL"),
        }
    }
    out.push_str("]::bigint[]");
}

/// `ARRAY[E'a', NULL]::text[]`, each element through [`push_text_literal`].
fn push_text_array<'a>(
    out: &mut String,
    values: impl Iterator<Item = Option<&'a str>>,
) -> Result<(), DbErr> {
    out.push_str("ARRAY[");
    for (i, v) in values.enumerate() {
        if i > 0 {
            out.push(',');
        }
        match v {
            Some(v) => push_text_literal(out, v)?,
            None => out.push_str("NULL"),
        }
    }
    out.push_str("]::text[]");
    Ok(())
}

/// An `E'...'` string literal: quotes doubled and backslashes doubled, so
/// the literal reads the same whatever `standard_conforming_strings` and
/// `backslash_quote` say. A NUL byte cannot be represented (the simple
/// protocol carries the query as a C string, and PostgreSQL text rejects
/// it anyway), so it is refused rather than truncating the message.
fn push_text_literal(out: &mut String, s: &str) -> Result<(), DbErr> {
    if s.contains('\0') {
        return Err(DbErr::Custom(
            "text value contains a NUL byte, which PostgreSQL cannot store".into(),
        ));
    }
    out.push_str("E'");
    for ch in s.chars() {
        match ch {
            '\'' => out.push_str("''"),
            '\\' => out.push_str("\\\\"),
            ch => out.push(ch),
        }
    }
    out.push('\'');
    Ok(())
}

#[cfg(test)]
mod tests {
    use sea_orm::sea_query::{Alias, Expr, Func, Order, Query};
    use sea_orm::{Database, DatabaseBackend, FromQueryResult, Statement};

    use super::*;
    use crate::db::engine::DatabaseEngine;
    use crate::db::query::edge::tests::quiet_connect;
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
    #[test]
    fn text_literals_escape_quotes_and_backslashes_and_refuse_nul() {
        let mut out = String::new();
        push_text_literal(&mut out, "it's \\ a \"test\"\nÜ🎵").unwrap();
        assert_eq!(out, "E'it''s \\\\ a \"test\"\nÜ🎵'");
        let mut out = String::new();
        push_text_array(&mut out, [Some("a"), None].into_iter()).unwrap();
        assert_eq!(out, "ARRAY[E'a',NULL]::text[]");
        let mut out = String::new();
        push_int_array(&mut out, [Some(1), None, Some(-3)].into_iter());
        assert_eq!(out, "ARRAY[1,NULL,-3]::bigint[]");
        assert!(push_text_literal(&mut String::new(), "a\0b").is_err());
    }

    /// The message carries every part in one string: the timeouts, the
    /// users upsert with `COALESCE` on the team, derived time rows, the
    /// ranking insert joined to the upsert for a user the memo lacks, and
    /// the commit plus LSN read.
    #[test]
    fn flush_message_has_every_part_in_one_round_trip() {
        let t = 1_710_000_000;
        let mut memo = UserMemo::default();
        memo.insert(
            200,
            UserMemoEntry {
                user_id_key: 9,
                name: "player-200".into(),
                cheerful_team_id: None,
                unique_id: None,
                profile_hash: Some(
                    UserDimRow::from_record(
                        SekaiServerRegion::Jp,
                        1,
                        &UidAnonymizer::disabled(),
                        &record(t, "200", 2, 900),
                    )
                    .profile_hash,
                ),
            },
        );
        let (main, wl, profiles) = record_batch(
            SekaiServerRegion::Jp,
            1,
            &UidAnonymizer::disabled(),
            &[record(t, "100", 1, 1_000), record(t, "200", 2, 900)],
            &[],
        )
        .unwrap();
        let prev_state = HashMap::new();
        let input = FlushInput {
            event_id: 1,
            batch: FlushBatch {
                main: &main,
                world_bloom: &wl,
                profiles: &profiles,
            },
            prev_state: &prev_state,
            memo: &memo,
        };
        let plan = pg_flush_message(&input, Some(std::time::Duration::from_secs(5))).unwrap();
        let sql = plan.sql.unwrap();
        assert!(sql.starts_with("BEGIN; SET LOCAL statement_timeout = 5000;"));
        assert!(sql.contains("u AS (INSERT INTO \"event_1_users\" AS tbl"));
        assert!(sql.contains("COALESCE(EXCLUDED.\"cheerful_team_id\", tbl.\"cheerful_team_id\")"));
        assert!(sql.contains("RETURNING \"user_id\", \"user_id_key\", \"cheerful_team_id\")"));
        assert!(sql.contains("t AS (INSERT INTO \"event_1_time_id\""));
        assert!(sql.contains("SELECT ts, ts, 0 FROM unnest(ARRAY[1710000000]::bigint[])"));
        assert!(sql.contains("COALESCE(r.key, u.user_id_key)"));
        assert!(sql.contains("LEFT JOIN u ON u.user_id = r.uid::text"));
        assert!(sql.contains("ON CONFLICT (\"time_id\", \"user_id_key\") DO NOTHING"));
        assert!(sql.ends_with("SELECT user_id, user_id_key, cheerful_team_id FROM u; COMMIT; SELECT pg_current_wal_lsn()::text AS lsn"));
        assert_eq!(plan.bookkeeping.upserted, vec![100]);
        assert_eq!(plan.bookkeeping.keys.get(&200), Some(&9));
        assert_eq!(sql.matches(';').count(), 5, "{sql}");

        // Only memoized users: no upsert CTE, no join, a dummy primary query.
        let (main, wl, profiles) = record_batch(
            SekaiServerRegion::Jp,
            1,
            &UidAnonymizer::disabled(),
            &[record(t + 1, "200", 2, 901)],
            &[],
        )
        .unwrap();
        let input = FlushInput {
            event_id: 1,
            batch: FlushBatch {
                main: &main,
                world_bloom: &wl,
                profiles: &profiles,
            },
            prev_state: &prev_state,
            memo: &memo,
        };
        let sql = pg_flush_message(&input, None).unwrap().sql.unwrap();
        assert!(sql.starts_with("BEGIN; WITH t AS"));
        assert!(!sql.contains("u AS") && !sql.contains("JOIN"));
        assert!(sql.contains("SELECT r.ts, r.key, r.score, r.rank FROM unnest(ARRAY[1710000001]::bigint[], ARRAY[9]::bigint[], ARRAY[200]::bigint[], ARRAY[901]::bigint[], ARRAY[2]::bigint[])"));
        assert!(sql.contains(" SELECT 0 WHERE false; COMMIT;"));
    }

    // --- Equivalence of the old and new write paths on random flush sequences ---

    use crate::db::query::edge::tests::Rng;
    use crate::tracker::pending::PendingBuffer;

    /// A user's upstream state at some sample; mutated between samples.
    #[derive(Clone)]
    struct Persona {
        name: String,
        cheerful_team_id: Option<i64>,
        profile: PlayerProfileSchema,
    }

    const NAMES: &[&str] = &[
        "plain",
        "it's",
        "back\\slash",
        "quote\"d",
        "ミク🎵",
        "new\nline",
        "tab\tbed",
        "dollar$1",
        "semi;colon",
        "per%cent",
        "",
    ];

    fn mutate_persona(rng: &mut Rng, p: &mut Persona) {
        match rng.below(6) {
            0 => p.name = NAMES[rng.below(NAMES.len() as u64) as usize].to_string(),
            1 => p.cheerful_team_id = [None, Some(1), Some(2)][rng.below(3) as usize],
            2 => {
                p.profile.card = if rng.chance(20) {
                    None
                } else {
                    Some(UserCard {
                        card_id: Some(rng.range(1, 2000)),
                        level: Some(rng.range(1, 60)),
                        master_rank: Some(rng.range(0, 5)),
                        special_training_status: [None, Some("done".to_string())]
                            [rng.below(2) as usize]
                            .clone(),
                        default_image: Some(NAMES[rng.below(NAMES.len() as u64) as usize].into()),
                    })
                };
            }
            3 => {
                p.profile.profile_word = if rng.chance(30) {
                    None
                } else {
                    Some(format!(
                        "{} {}",
                        NAMES[rng.below(NAMES.len() as u64) as usize],
                        rng.below(1000)
                    ))
                };
            }
            4 => {
                p.profile.profile_honors = (0..rng.below(4))
                    .map(|i| UserProfileHonor {
                        seq: Some(i as i64),
                        profile_honor_type: Some("normal".into()),
                        honor_id: Some(rng.range(1, 500)),
                        honor_level: Some(rng.range(1, 9)),
                        bonds_honor_view_type: None,
                        bonds_honor_word_id: None,
                    })
                    .collect();
            }
            _ => {
                p.profile.honor_missions = (0..rng.below(3))
                    .map(|i| {
                        serde_json::json!({"honorMissionType": NAMES[i as usize], "progress": rng.below(50)})
                    })
                    .collect();
                p.profile.player_frames = if rng.chance(50) {
                    Vec::new()
                } else {
                    vec![UserPlayerFrame {
                        player_frame_id: Some(rng.range(1, 99)),
                        player_frame_attach_status: Some("first".into()),
                    }]
                };
            }
        }
    }

    enum Step {
        Flush(
            Vec<PlayerEventRankingRecordSchema>,
            Vec<PlayerWorldBloomRankingRecordSchema>,
        ),
        /// Re-send the previous flush unchanged (a lost reply).
        Retry,
        /// The writer restarted: memo and World Bloom baseline gone.
        Restart,
    }

    fn generate_steps(rng: &mut Rng, world_bloom: bool, steps: usize) -> Vec<Step> {
        const USERS: i64 = 8;
        let mut personas: Vec<Persona> = (0..USERS)
            .map(|uid| Persona {
                name: format!("p{uid}"),
                cheerful_team_id: None,
                profile: PlayerProfileSchema::default(),
            })
            .collect();
        let mut ts = 1_760_000_000;
        let mut out = Vec::new();
        for _ in 0..steps {
            match rng.below(10) {
                0 => {
                    out.push(Step::Retry);
                    continue;
                }
                1 => {
                    out.push(Step::Restart);
                    continue;
                }
                _ => {}
            }
            let mut main = Vec::new();
            let mut wl = Vec::new();
            for _ in 0..rng.range(1, 4) {
                ts += rng.range(1, 3);
                for uid in 0..USERS {
                    if rng.chance(25) {
                        mutate_persona(rng, &mut personas[uid as usize]);
                    }
                }
                let make =
                    |p: &Persona, uid: i64, rank: i64, score: i64| PlayerEventRankingRecordSchema {
                        timestamp: ts,
                        user_id: uid.to_string(),
                        name: p.name.clone(),
                        score,
                        rank,
                        cheerful_team_id: p.cheerful_team_id,
                        profile: p.profile.clone(),
                    };
                let mut listed = HashSet::new();
                for _ in 0..rng.range(0, 6) {
                    let uid = rng.range(0, USERS - 1);
                    if !listed.insert(uid) {
                        continue;
                    }
                    let p = &personas[uid as usize];
                    main.push(make(p, uid, rng.range(1, 10), rng.range(1, 5000)));
                }
                if world_bloom {
                    for character_id in [1, 2] {
                        let mut listed = HashSet::new();
                        for _ in 0..rng.range(0, 3) {
                            let uid = rng.range(0, USERS - 1);
                            if !listed.insert(uid) {
                                continue;
                            }
                            // Small ranges so unchanged (score, rank) pairs recur
                            // and the World Bloom dedup actually triggers.
                            let p = &personas[uid as usize];
                            wl.push(PlayerWorldBloomRankingRecordSchema {
                                base: make(p, uid, rng.range(1, 3), rng.range(1, 3)),
                                character_id,
                            });
                        }
                    }
                }
            }
            if main.is_empty() && wl.is_empty() {
                continue;
            }
            out.push(Step::Flush(main, wl));
        }
        out
    }

    async fn dump_tables(engine: &DatabaseEngine, event_id: i64, world_bloom: bool) -> Vec<String> {
        let users = intern(TableKind::EventUsers, event_id);
        let cols = [
            "user_id",
            "unique_id",
            "name",
            "cheerful_team_id",
            "card_id",
            "card_level",
            "card_master_rank",
            "card_special_training_status",
            "card_default_image",
            "profile_word",
            "profile_honors_json",
            "honor_missions_json",
            "player_frames_json",
            "profile_hash",
        ];
        let user_line = cols
            .iter()
            .map(|c| format!("coalesce(CAST(u.\"{c}\" AS TEXT), '~')"))
            .collect::<Vec<_>>()
            .join(" || '|' || ");
        let mut queries = vec![
            format!("SELECT 'u|' || {user_line} AS line FROM {users} u"),
            format!(
                "SELECT 't|' || CAST(time_id AS TEXT) || '|' || CAST(timestamp AS TEXT) || '|' || CAST(status AS TEXT) AS line FROM {}",
                intern(TableKind::TimeId, event_id)
            ),
            format!(
                "SELECT 'e|' || CAST(e.time_id AS TEXT) || '|' || u.user_id || '|' || CAST(e.score AS TEXT) || '|' || CAST(e.rank AS TEXT) AS line FROM {} e JOIN {users} u ON u.user_id_key = e.user_id_key",
                intern(TableKind::Event, event_id)
            ),
        ];
        if world_bloom {
            queries.push(format!(
                "SELECT 'w|' || CAST(w.time_id AS TEXT) || '|' || u.user_id || '|' || CAST(w.character_id AS TEXT) || '|' || CAST(w.score AS TEXT) || '|' || CAST(w.rank AS TEXT) AS line FROM {} w JOIN {users} u ON u.user_id_key = w.user_id_key",
                intern(TableKind::WorldBloom, event_id)
            ));
        }
        let mut lines = Vec::new();
        for sql in queries {
            let rows = engine
                .conn()
                .query_all_raw(Statement::from_string(engine.backend(), sql))
                .await
                .unwrap();
            for row in rows {
                lines.push(row.try_get::<String>("", "line").unwrap());
            }
        }
        lines.sort();
        lines
    }

    async fn reset_tables(engine: &DatabaseEngine, event_id: i64, world_bloom: bool) {
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
        create_event_tables(engine, SekaiServerRegion::Jp, event_id, world_bloom)
            .await
            .unwrap();
    }

    /// Drive one scenario through both paths. Event `old_id` gets the
    /// pre-memo behaviour: every flush offers every profile to a fresh
    /// memo and, on PostgreSQL, the statement path. Event `new_id` is the
    /// tracker's path: a `PendingBuffer` that keeps only profiles the
    /// retained memo does not hold, chunked flushes, and `Auto` mode (the
    /// single message on PostgreSQL). After every step all four tables
    /// must read identically (keys normalized to `user_id`).
    async fn run_equivalence(engine: &DatabaseEngine, first_event: i64, seeds: u64, steps: usize) {
        for seed in 0..seeds {
            let mut rng = Rng::new(seed * 31 + 7);
            let world_bloom = seed % 2 == 0;
            let anonymizer = if rng.chance(50) {
                UidAnonymizer::enabled(format!("salt-{seed}"))
            } else {
                UidAnonymizer::disabled()
            };
            let chunk = [0, 3, 7][rng.below(3) as usize];
            let old_id = first_event + seed as i64 * 2;
            let new_id = old_id + 1;
            reset_tables(engine, old_id, world_bloom).await;
            reset_tables(engine, new_id, world_bloom).await;
            let steps = generate_steps(&mut rng, world_bloom, steps);

            let mut old_state = HashMap::new();
            let mut new_state = HashMap::new();
            let mut new_memo = UserMemo::default();
            let mut last: Option<(
                Vec<PlayerEventRankingRecordSchema>,
                Vec<PlayerWorldBloomRankingRecordSchema>,
            )> = None;
            let mut flushes = 0;
            for (index, step) in steps.iter().enumerate() {
                let (main, wl) = match step {
                    Step::Flush(main, wl) => (main, wl),
                    Step::Retry => match &last {
                        Some((main, wl)) => (main, wl),
                        None => continue,
                    },
                    Step::Restart => {
                        old_state.clear();
                        new_state.clear();
                        new_memo = UserMemo::default();
                        continue;
                    }
                };
                flushes += 1;
                // Old path. Both events anonymize under `first_event` so the
                // stored unique_ids are comparable.
                let (rows, wl_rows, profiles) =
                    record_batch(SekaiServerRegion::Jp, first_event, &anonymizer, main, wl)
                        .unwrap();
                flush_batch_with(
                    engine,
                    old_id,
                    FlushBatch {
                        main: &rows,
                        world_bloom: &wl_rows,
                        profiles: &profiles,
                    },
                    &mut old_state,
                    &mut UserMemo::default(),
                    PgFlushMode::Statements,
                )
                .await
                .unwrap();

                // New path, as the tracker drives it.
                let mut buffer = PendingBuffer::default();
                for r in main {
                    let row = SampleRow::from_record(r).unwrap();
                    let info =
                        UserDimRow::from_record(SekaiServerRegion::Jp, first_event, &anonymizer, r);
                    buffer.offer_profile(row.uid, row.timestamp, info, &new_memo);
                    buffer.push_main(row);
                }
                for r in wl {
                    let row = SampleRow::from_record(&r.base).unwrap();
                    let info = UserDimRow::from_record(
                        SekaiServerRegion::Jp,
                        first_event,
                        &anonymizer,
                        &r.base,
                    );
                    buffer.offer_profile(row.uid, row.timestamp, info, &new_memo);
                    buffer.push_world_bloom(WorldBloomSampleRow {
                        row,
                        character_id: r.character_id,
                    });
                }
                while !buffer.is_empty() {
                    let taken = buffer.take_chunk(chunk);
                    flush_batch(
                        engine,
                        new_id,
                        FlushBatch {
                            main: &taken.main,
                            world_bloom: &taken.world_bloom,
                            profiles: buffer.profiles(),
                        },
                        &mut new_state,
                        &mut new_memo,
                    )
                    .await
                    .unwrap();
                    buffer.forget_flushed(&taken, &new_memo);
                }
                assert!(
                    buffer.profiles().is_empty(),
                    "seed {seed} step {index}: profiles left after a full drain"
                );

                let old_dump = dump_tables(engine, old_id, world_bloom).await;
                let new_dump = dump_tables(engine, new_id, world_bloom).await;
                if old_dump != new_dump {
                    let only_old: Vec<&String> =
                        old_dump.iter().filter(|l| !new_dump.contains(l)).collect();
                    let only_new: Vec<&String> =
                        new_dump.iter().filter(|l| !old_dump.contains(l)).collect();
                    panic!(
                        "seed {seed} step {index} (chunk {chunk}, anonymized {}): tables differ\nonly old: {only_old:#?}\nonly new: {only_new:#?}",
                        anonymizer.is_enabled(),
                    );
                }
                if let Step::Flush(main, wl) = step {
                    last = Some((main.clone(), wl.clone()));
                }
            }
            assert!(flushes > 0, "seed {seed} produced no flush");
        }
    }

    /// A private in-memory database: sqlx gives every `sqlite::memory:`
    /// connection in the process one shared-cache database, and this run's
    /// drops and hundreds of write transactions would starve the other
    /// tests' reads (SQLITE_BUSY surfacing as 500s in the API tests).
    #[tokio::test]
    async fn old_and_new_write_paths_agree_on_sqlite() {
        let engine = quiet_connect(
            "sqlite:file:het_write_paths?mode=memory&cache=private",
            DatabaseBackend::Sqlite,
        )
        .await;
        run_equivalence(&engine, 8_800_000, 12, 24).await;
    }

    /// `HET_TEST_PG_URL=postgres://... cargo test --lib -- --ignored
    /// old_and_new_write_paths_agree_on_postgres`.
    #[tokio::test]
    #[ignore = "needs HET_TEST_PG_URL"]
    async fn old_and_new_write_paths_agree_on_postgres() {
        let Ok(url) = std::env::var("HET_TEST_PG_URL") else {
            return;
        };
        let engine = quiet_connect(&url, DatabaseBackend::Postgres).await;
        let engine = engine.with_write_timeout(Some(std::time::Duration::from_secs(5)));
        run_equivalence(&engine, 8_810_000, 12, 24).await;
    }

    async fn user_xmins(engine: &DatabaseEngine, event_id: i64) -> Vec<(String, String)> {
        let rows = engine
            .conn()
            .query_all_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                format!(
                    "SELECT user_id, xmin::text AS x FROM {} ORDER BY user_id",
                    intern(TableKind::EventUsers, event_id)
                ),
            ))
            .await
            .unwrap();
        rows.into_iter()
            .map(|r| {
                (
                    r.try_get::<String>("", "user_id").unwrap(),
                    r.try_get::<String>("", "x").unwrap(),
                )
            })
            .collect()
    }

    /// On the single-message path, a flush whose users are unchanged does
    /// not touch their rows (same `xmin`); a rename rewrites that row only.
    #[tokio::test]
    #[ignore = "needs HET_TEST_PG_URL"]
    async fn message_path_leaves_unchanged_user_rows_alone() {
        use std::time::Duration;

        use crate::db::pg_session::tests::pg_engine;

        let Some(engine) = pg_engine(Duration::from_secs(5)).await else {
            return;
        };
        let event_id = 8_820_000;
        reset_tables(&engine, event_id, false).await;
        let t = 1_710_000_000;
        let anonymizer = UidAnonymizer::enabled("xmin-salt");
        let mut memo = UserMemo::default();
        let mut state = HashMap::new();
        let outcome = batch_insert_flush(
            &engine,
            SekaiServerRegion::Jp,
            event_id,
            &anonymizer,
            &[record(t, "100", 1, 1_000), record(t, "200", 2, 900)],
            &[],
            &mut state,
            &mut memo,
        )
        .await
        .unwrap();
        assert!(outcome.lsn.is_some());
        assert_eq!(memo.len(), 2);
        let before = user_xmins(&engine, event_id).await;
        assert_eq!(before.len(), 2);

        batch_insert_flush(
            &engine,
            SekaiServerRegion::Jp,
            event_id,
            &anonymizer,
            &[record(t + 1, "100", 1, 1_001), record(t + 1, "200", 2, 901)],
            &[],
            &mut state,
            &mut memo,
        )
        .await
        .unwrap();
        assert_eq!(
            user_xmins(&engine, event_id).await,
            before,
            "unchanged users must not be rewritten"
        );

        let mut renamed = record(t + 2, "100", 1, 1_002);
        renamed.name = "renamed".into();
        batch_insert_flush(
            &engine,
            SekaiServerRegion::Jp,
            event_id,
            &anonymizer,
            &[renamed, record(t + 2, "200", 2, 902)],
            &[],
            &mut state,
            &mut memo,
        )
        .await
        .unwrap();
        let after = user_xmins(&engine, event_id).await;
        assert_ne!(after[0], before[0], "the renamed user is rewritten");
        assert_eq!(after[1], before[1], "the other user is not");
        assert_eq!(memo.get(100).unwrap().name, "renamed");
        assert_eq!(
            get_user_data(&engine, event_id, "100", PublicUserIdMode::Raw)
                .await
                .unwrap()
                .unwrap()
                .name,
            "renamed"
        );
        let lines = fetch_ranking_lines(&engine, event_id, &[1, 2], None)
            .await
            .unwrap();
        assert_eq!((lines[0].timestamp, lines[0].score), (t + 2, 1_002));
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
