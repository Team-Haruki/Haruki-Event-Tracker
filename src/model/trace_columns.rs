//! Compact columnar encoding of a trace (`traceFormat=columns`).
//!
//! A trace's row form is an array of `{timestamp, userId, score, rank,
//! characterId?}` objects. In a rank trace the rank is constant and the
//! user changes only when the seat changes hands; in a player trace the
//! user is constant. Each row still spells out all of it, so `userId`
//! (64 hex digits) and `rank` are two thirds of the bytes. The columnar
//! form stores each column once as a delta list, a constant, or a run
//! list, round-trips to exactly the same row array (order, values and
//! `characterId` presence included) and compresses several times better.
//! The wire shape is documented in `WEB_API_CAPABILITIES.md`.

use std::borrow::Cow;
use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::model::api::{
    RecordedRankData, RecordedRankingSchema, RecordedWorldBloomRankingSchema, TraceRows,
};

/// The trace format a detail request asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TraceFormat {
    /// The row array (default; the only form older servers produce).
    #[default]
    Rows,
    /// The columnar object below.
    Columns,
}

impl TraceFormat {
    /// Parses a `traceFormat` query value; absent and empty mean rows.
    pub fn parse(raw: Option<&str>) -> Result<Self, String> {
        match raw.map(str::trim) {
            None | Some("") | Some("rows") => Ok(Self::Rows),
            Some("columns") => Ok(Self::Columns),
            Some(other) => Err(format!("traceFormat must be rows or columns, got {other}")),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TraceColumnsError {
    #[error("{0}")]
    Json(#[from] sonic_rs::Error),
    #[error("trace columns are malformed: {0}")]
    Shape(String),
}

/// Serializes as the literal `"columns"` and refuses anything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum FormatTag {
    Columns,
}

/// A row-array trace as columns. Every list is in row order. Invariants
/// (checked by [`Self::to_rows`]): with `n` rows, `dt`, `ds` and `dr`
/// hold `n - 1` deltas (none when `n <= 1`); exactly one of `rank`
/// (constant) or `r0` + `dr` is present when `n > 0`; `u` lists runs
/// `[startRow, indexIntoUsers]` with the first run starting at row 0 and
/// strictly increasing starts; `characterId` (every row carries it) and
/// `cid` (one entry per row, `null` for rows without one) are mutually
/// exclusive and both absent when no row has a `characterId`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TraceColumns {
    format: FormatTag,
    pub n: usize,
    pub t0: i64,
    pub dt: Vec<i64>,
    pub s0: i64,
    pub ds: Vec<i64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub rank: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub r0: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub dr: Option<Vec<i64>>,
    pub users: Vec<String>,
    pub u: Vec<(usize, usize)>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub character_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub cid: Option<Vec<Option<i64>>>,
}

/// The two leading fields, decoded to check a cached value's shape.
#[derive(Deserialize)]
#[allow(dead_code)] // decoded only to check the shape
struct ColumnsHead {
    format: FormatTag,
    n: usize,
}

#[derive(Clone, Copy)]
struct RowParts<'a> {
    timestamp: i64,
    user_id: &'a str,
    score: i64,
    rank: i64,
    character_id: Option<i64>,
}

fn row_parts(row: &RecordedRankData) -> RowParts<'_> {
    match row {
        RecordedRankData::Normal(row) => RowParts {
            timestamp: row.timestamp,
            user_id: &row.user_id,
            score: row.score,
            rank: row.rank,
            character_id: None,
        },
        RecordedRankData::WorldBloom(row) => RowParts {
            timestamp: row.timestamp,
            user_id: &row.user_id,
            score: row.score,
            rank: row.rank,
            character_id: row.character_id,
        },
    }
}

impl TraceColumns {
    /// Encodes rows in one pass. The rank-delta and per-row `characterId`
    /// lists are only materialized once a row differs from the first.
    pub fn from_rows(rows: &[RecordedRankData]) -> Self {
        let n = rows.len();
        let Some(first) = rows.first() else {
            return Self::empty();
        };
        let first = row_parts(first);
        let mut dt = Vec::with_capacity(n - 1);
        let mut ds = Vec::with_capacity(n - 1);
        let mut dr: Option<Vec<i64>> = None;
        let mut users = vec![first.user_id.to_owned()];
        let mut user_index: HashMap<&str, usize, ahash::RandomState> = HashMap::default();
        user_index.insert(first.user_id, 0);
        let mut runs = vec![(0, 0)];
        let mut cid: Option<Vec<Option<i64>>> = None;
        let mut prev = first;
        for (index, row) in rows.iter().enumerate().skip(1) {
            let row = row_parts(row);
            dt.push(row.timestamp - prev.timestamp);
            ds.push(row.score - prev.score);
            let rank_delta = row.rank - prev.rank;
            match dr.as_mut() {
                Some(dr) => dr.push(rank_delta),
                None if rank_delta != 0 => {
                    let mut deltas = vec![0; index - 1];
                    deltas.push(rank_delta);
                    dr = Some(deltas);
                }
                None => {}
            }
            if row.user_id != prev.user_id {
                let next = users.len();
                let user = *user_index.entry(row.user_id).or_insert(next);
                if user == next {
                    users.push(row.user_id.to_owned());
                }
                runs.push((index, user));
            }
            match cid.as_mut() {
                Some(cid) => cid.push(row.character_id),
                None if row.character_id != first.character_id => {
                    let mut column = vec![first.character_id; index];
                    column.push(row.character_id);
                    cid = Some(column);
                }
                None => {}
            }
            prev = row;
        }
        let (rank, r0) = match dr {
            Some(_) => (None, Some(first.rank)),
            None => (Some(first.rank), None),
        };
        let character_id = if cid.is_none() {
            first.character_id
        } else {
            None
        };
        Self {
            format: FormatTag::Columns,
            n,
            t0: first.timestamp,
            dt,
            s0: first.score,
            ds,
            rank,
            r0,
            dr,
            users,
            u: runs,
            character_id,
            cid,
        }
    }

    /// The columns of a cached trace response's `rankData` array (absent
    /// means no rows).
    pub fn from_trace_json(json: &[u8]) -> sonic_rs::Result<Self> {
        let rows: Vec<RecordedRankData> = match sonic_rs::get_from_slice(json, ["rankData"]) {
            Ok(value) => sonic_rs::from_str(value.as_raw_str())?,
            Err(err) if err.is_not_found() => Vec::new(),
            Err(err) => return Err(err),
        };
        Ok(Self::from_rows(&rows))
    }

    fn empty() -> Self {
        Self {
            format: FormatTag::Columns,
            n: 0,
            t0: 0,
            dt: Vec::new(),
            s0: 0,
            ds: Vec::new(),
            rank: None,
            r0: None,
            dr: None,
            users: Vec::new(),
            u: Vec::new(),
            character_id: None,
            cid: None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// Whether bytes are a columns object: the leading `format` tag and
    /// `n` must decode. Cheap enough for every cache hit.
    pub fn json_is_well_formed(json: &[u8]) -> sonic_rs::Result<()> {
        sonic_rs::from_slice::<ColumnsHead>(json).map(|_| ())
    }

    /// Decodes back to rows, validating the invariants above.
    pub fn to_rows(&self) -> Result<Vec<RecordedRankData>, TraceColumnsError> {
        let n = self.n;
        if n == 0 {
            return Ok(Vec::new());
        }
        let deltas = n - 1;
        let shape = |ok: bool, what: &str| {
            if ok {
                Ok(())
            } else {
                Err(TraceColumnsError::Shape(what.to_owned()))
            }
        };
        shape(self.dt.len() == deltas, "dt length")?;
        shape(self.ds.len() == deltas, "ds length")?;
        let ranks: Box<dyn Fn(usize, i64) -> i64> = match (self.rank, self.r0, &self.dr) {
            (Some(rank), None, None) => Box::new(move |_, _| rank),
            (None, Some(_), Some(dr)) => {
                shape(dr.len() == deltas, "dr length")?;
                Box::new(move |index, prev| {
                    if index == 0 {
                        prev
                    } else {
                        prev + dr[index - 1]
                    }
                })
            }
            _ => return Err(TraceColumnsError::Shape("rank columns".to_owned())),
        };
        shape(
            self.u.first().is_some_and(|(start, _)| *start == 0),
            "first run",
        )?;
        shape(
            self.u.windows(2).all(|pair| pair[0].0 < pair[1].0)
                && self.u.iter().all(|(start, _)| *start < n),
            "run starts",
        )?;
        shape(
            self.u.iter().all(|(_, user)| *user < self.users.len()),
            "run users",
        )?;
        let cids: Box<dyn Fn(usize) -> Option<i64>> = match (self.character_id, &self.cid) {
            (constant, None) => Box::new(move |_| constant),
            (None, Some(cid)) => {
                shape(cid.len() == n, "cid length")?;
                Box::new(move |index| cid[index])
            }
            (Some(_), Some(_)) => {
                return Err(TraceColumnsError::Shape("characterId and cid".to_owned()));
            }
        };
        let mut rows = Vec::with_capacity(n);
        let mut timestamp = self.t0;
        let mut score = self.s0;
        let mut rank = self.r0.or(self.rank).unwrap_or_default();
        let mut run = 0;
        for index in 0..n {
            if index > 0 {
                timestamp += self.dt[index - 1];
                score += self.ds[index - 1];
            }
            rank = ranks(index, rank);
            if self
                .u
                .get(run + 1)
                .is_some_and(|(start, _)| *start == index)
            {
                run += 1;
            }
            let user_id = self.users[self.u[run].1].clone();
            rows.push(match cids(index) {
                Some(character_id) => {
                    RecordedRankData::WorldBloom(RecordedWorldBloomRankingSchema {
                        timestamp,
                        user_id,
                        score,
                        rank,
                        character_id: Some(character_id),
                    })
                }
                None => RecordedRankData::Normal(RecordedRankingSchema {
                    timestamp,
                    user_id,
                    score,
                    rank,
                }),
            });
        }
        Ok(rows)
    }

    /// Replaces every occurrence of a user id (the subject of a raw-UID
    /// lookup).
    pub fn rename_user(&mut self, from: &str, to: &str) {
        for user in &mut self.users {
            if user == from {
                *user = to.to_owned();
            }
        }
    }
}

/// A detail's trace field in either wire form: the row array (spliced or
/// typed, see [`TraceRows`]) or a cached columns object spliced verbatim.
#[derive(Debug, Clone)]
pub enum TracePayload {
    Rows(TraceRows),
    Columns(RawColumns),
}

/// A columns object as its JSON text, serialized verbatim (through
/// sonic-rs, like `TraceRows::Raw`), with its row count read up front.
#[derive(Debug, Clone)]
pub struct RawColumns {
    n: usize,
    text: sonic_rs::FastStr,
    value: sonic_rs::OwnedLazyValue,
}

impl RawColumns {
    fn from_json(json: &[u8]) -> sonic_rs::Result<Self> {
        let head: ColumnsHead = sonic_rs::from_slice(json)?;
        let value: sonic_rs::LazyValue<'_> = sonic_rs::from_slice(json)?;
        Ok(Self {
            n: head.n,
            text: value.as_raw_faststr(),
            value: value.into(),
        })
    }

    fn decode(&self) -> Result<TraceColumns, TraceColumnsError> {
        Ok(sonic_rs::from_str(&self.text)?)
    }
}

impl Default for TracePayload {
    fn default() -> Self {
        Self::Rows(TraceRows::default())
    }
}

impl From<TraceRows> for TracePayload {
    fn from(rows: TraceRows) -> Self {
        Self::Rows(rows)
    }
}

impl From<Vec<RecordedRankData>> for TracePayload {
    fn from(rows: Vec<RecordedRankData>) -> Self {
        Self::Rows(rows.into())
    }
}

impl TracePayload {
    /// A cached columns object (the bytes `TraceColumns` serialized to).
    pub fn columns_from_json(json: &[u8]) -> sonic_rs::Result<Self> {
        RawColumns::from_json(json).map(Self::Columns)
    }

    pub fn from_columns(columns: &TraceColumns) -> sonic_rs::Result<Self> {
        Self::columns_from_json(&sonic_rs::to_vec(columns)?)
    }

    /// Typed rows in the requested format.
    pub fn encode(rows: Vec<RecordedRankData>, format: TraceFormat) -> sonic_rs::Result<Self> {
        match format {
            TraceFormat::Rows => Ok(rows.into()),
            TraceFormat::Columns => Self::from_columns(&TraceColumns::from_rows(&rows)),
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Self::Rows(rows) => rows.is_empty(),
            Self::Columns(columns) => columns.n == 0,
        }
    }

    pub fn format(&self) -> TraceFormat {
        match self {
            Self::Rows(_) => TraceFormat::Rows,
            Self::Columns(_) => TraceFormat::Columns,
        }
    }

    /// The rows in either form (O(rows); not for the hot path).
    pub fn to_rows(&self) -> Result<Cow<'_, [RecordedRankData]>, TraceColumnsError> {
        match self {
            Self::Rows(rows) => Ok(rows.to_rows()?),
            Self::Columns(columns) => columns.decode()?.to_rows().map(Cow::Owned),
        }
    }

    /// Decoded rows for assertions.
    #[cfg(test)]
    pub fn rows(&self) -> Vec<RecordedRankData> {
        self.to_rows().expect("trace payload decode").into_owned()
    }

    /// Replaces every occurrence of a user id, keeping the wire form.
    pub fn rename_user(&mut self, from: &str, to: &str) -> Result<(), TraceColumnsError> {
        match self {
            Self::Rows(rows) => {
                for row in rows.rows_mut()? {
                    let user_id = match row {
                        RecordedRankData::Normal(row) => &mut row.user_id,
                        RecordedRankData::WorldBloom(row) => &mut row.user_id,
                    };
                    if user_id == from {
                        *user_id = to.to_owned();
                    }
                }
            }
            Self::Columns(raw) => {
                let mut columns = raw.decode()?;
                columns.rename_user(from, to);
                *raw = RawColumns::from_json(&sonic_rs::to_vec(&columns)?)?;
            }
        }
        Ok(())
    }
}

impl Serialize for TracePayload {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Rows(rows) => rows.serialize(serializer),
            Self::Columns(columns) => columns.value.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for TracePayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Either {
            Rows(Vec<RecordedRankData>),
            Columns(TraceColumns),
        }
        match Either::deserialize(deserializer)? {
            Either::Rows(rows) => Ok(rows.into()),
            Either::Columns(columns) => {
                Self::from_columns(&columns).map_err(serde::de::Error::custom)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn normal(timestamp: i64, user_id: &str, score: i64, rank: i64) -> RecordedRankData {
        RecordedRankData::Normal(RecordedRankingSchema {
            timestamp,
            user_id: user_id.to_owned(),
            score,
            rank,
        })
    }

    fn world_bloom(
        timestamp: i64,
        user_id: &str,
        score: i64,
        rank: i64,
        character_id: Option<i64>,
    ) -> RecordedRankData {
        RecordedRankData::WorldBloom(RecordedWorldBloomRankingSchema {
            timestamp,
            user_id: user_id.to_owned(),
            score,
            rank,
            character_id,
        })
    }

    fn json(rows: &[RecordedRankData]) -> String {
        sonic_rs::to_string(rows).unwrap()
    }

    /// Columns must decode to the same row JSON, through the typed
    /// object, its JSON, and the spliced payload.
    fn assert_round_trip(rows: &[RecordedRankData]) -> TraceColumns {
        let columns = TraceColumns::from_rows(rows);
        assert_eq!(columns.n, rows.len());
        assert_eq!(json(&columns.to_rows().unwrap()), json(rows));
        let text = sonic_rs::to_string(&columns).unwrap();
        let decoded: TraceColumns = sonic_rs::from_str(&text).unwrap();
        assert_eq!(decoded, columns);
        let payload = TracePayload::from_columns(&columns).unwrap();
        assert_eq!(payload.format(), TraceFormat::Columns);
        assert_eq!(payload.is_empty(), rows.is_empty());
        assert_eq!(sonic_rs::to_string(&payload).unwrap(), text);
        assert_eq!(json(&payload.rows()), json(rows));
        let via_serde: TracePayload = serde_json::from_str(&text).unwrap();
        assert_eq!(json(&via_serde.rows()), json(rows));
        columns
    }

    #[test]
    fn rank_trace_with_seat_changes_is_a_constant_rank_and_user_runs() {
        let rows = vec![
            normal(1_700_000_000, "a", 100, 7),
            normal(1_700_000_002, "a", 130, 7),
            normal(1_700_000_003, "b", 131, 7),
            normal(1_700_000_010, "b", 200, 7),
            normal(1_700_000_011, "a", 201, 7),
            normal(1_700_000_012, "c", 250, 7),
        ];
        let columns = assert_round_trip(&rows);
        assert_eq!(columns.rank, Some(7));
        assert_eq!(columns.r0, None);
        assert_eq!(columns.dr, None);
        assert_eq!(columns.dt, vec![2, 1, 7, 1, 1]);
        assert_eq!(columns.ds, vec![30, 1, 69, 1, 49]);
        assert_eq!(columns.users, vec!["a", "b", "c"]);
        assert_eq!(columns.u, vec![(0, 0), (2, 1), (4, 0), (5, 2)]);
        assert_eq!(columns.character_id, None);
        assert_eq!(columns.cid, None);
        assert_eq!(
            sonic_rs::to_string(&columns).unwrap(),
            r#"{"format":"columns","n":6,"t0":1700000000,"dt":[2,1,7,1,1],"s0":100,"ds":[30,1,69,1,49],"rank":7,"users":["a","b","c"],"u":[[0,0],[2,1],[4,0],[5,2]]}"#
        );
    }

    #[test]
    fn player_trace_with_rank_moves_keeps_rank_deltas() {
        let rows = vec![
            normal(10, "p", 5, 3),
            normal(11, "p", 9, 3),
            normal(15, "p", 20, 2),
            normal(16, "p", 18, 4),
        ];
        let columns = assert_round_trip(&rows);
        assert_eq!(columns.rank, None);
        assert_eq!(columns.r0, Some(3));
        assert_eq!(columns.dr, Some(vec![0, -1, 2]));
        assert_eq!(columns.ds, vec![4, 11, -2]);
        assert_eq!(columns.users, vec!["p"]);
        assert_eq!(columns.u, vec![(0, 0)]);
    }

    #[test]
    fn world_bloom_traces_carry_a_constant_or_per_row_character_id() {
        let rows = vec![
            world_bloom(1, "a", 1, 1, Some(17)),
            world_bloom(2, "b", 2, 1, Some(17)),
        ];
        let columns = assert_round_trip(&rows);
        assert_eq!(columns.character_id, Some(17));
        assert_eq!(columns.cid, None);

        let mixed = vec![
            normal(1, "a", 1, 1),
            world_bloom(2, "a", 2, 1, Some(17)),
            world_bloom(3, "a", 3, 1, Some(19)),
            world_bloom(4, "a", 4, 1, None),
        ];
        let columns = assert_round_trip(&mixed);
        assert_eq!(columns.character_id, None);
        assert_eq!(columns.cid, Some(vec![None, Some(17), Some(19), None]));

        // A leading constant that later changes is materialized in full.
        let late = vec![
            world_bloom(1, "a", 1, 1, Some(17)),
            world_bloom(2, "a", 2, 1, Some(17)),
            normal(3, "a", 3, 1),
        ];
        let columns = assert_round_trip(&late);
        assert_eq!(columns.cid, Some(vec![Some(17), Some(17), None]));
    }

    #[test]
    fn empty_and_single_row_traces_round_trip() {
        let columns = assert_round_trip(&[]);
        assert!(columns.is_empty());
        assert_eq!(
            sonic_rs::to_string(&columns).unwrap(),
            r#"{"format":"columns","n":0,"t0":0,"dt":[],"s0":0,"ds":[],"users":[],"u":[]}"#
        );
        let columns = assert_round_trip(&[normal(5, "a", 6, 7)]);
        assert_eq!(columns.rank, Some(7));
        assert!(columns.dt.is_empty());
        assert_eq!(columns.u, vec![(0, 0)]);
        assert_round_trip(&[world_bloom(5, "a", 6, 7, Some(1))]);
    }

    #[test]
    fn cursor_increments_encode_independently() {
        let all = vec![
            normal(1, "a", 1, 2),
            normal(2, "a", 3, 2),
            normal(3, "b", 4, 2),
            normal(4, "b", 8, 2),
        ];
        let increment = &all[2..];
        let columns = assert_round_trip(increment);
        assert_eq!(columns.t0, 3);
        assert_eq!(columns.s0, 4);
        assert_eq!(columns.users, vec!["b"]);
        let all_rows = assert_round_trip(&all).to_rows().unwrap();
        assert_eq!(json(&all_rows[2..]), json(&columns.to_rows().unwrap()));
    }

    #[test]
    fn columns_come_from_cached_trace_json() {
        let rows = vec![normal(1, "a", 1, 2), normal(2, "b", 3, 2)];
        let trace = format!(r#"{{"meta":{{}},"rankData":{}}}"#, json(&rows));
        let columns = TraceColumns::from_trace_json(trace.as_bytes()).unwrap();
        assert_eq!(columns, TraceColumns::from_rows(&rows));
        let without = TraceColumns::from_trace_json(br#"{"meta":{}}"#).unwrap();
        assert!(without.is_empty());
        assert!(TraceColumns::from_trace_json(br#"{"rankData":{}}"#).is_err());
        assert!(TraceColumns::from_trace_json(b"nope").is_err());
    }

    #[test]
    fn malformed_columns_are_rejected() {
        let good = TraceColumns::from_rows(&[normal(1, "a", 1, 2), normal(2, "b", 3, 2)]);
        let mut short_dt = good.clone();
        short_dt.dt.clear();
        assert!(short_dt.to_rows().is_err());
        let mut both_ranks = good.clone();
        both_ranks.r0 = Some(2);
        assert!(both_ranks.to_rows().is_err());
        let mut no_ranks = good.clone();
        no_ranks.rank = None;
        assert!(no_ranks.to_rows().is_err());
        let mut bad_run_start = good.clone();
        bad_run_start.u[0].0 = 1;
        assert!(bad_run_start.to_rows().is_err());
        let mut bad_run_user = good.clone();
        bad_run_user.u[1].1 = 9;
        assert!(bad_run_user.to_rows().is_err());
        let mut run_past_end = good.clone();
        run_past_end.u.push((2, 0));
        assert!(run_past_end.to_rows().is_err());
        let mut both_cids = good.clone();
        both_cids.character_id = Some(1);
        both_cids.cid = Some(vec![None, None]);
        assert!(both_cids.to_rows().is_err());
        let mut short_cid = good;
        short_cid.cid = Some(vec![None]);
        assert!(short_cid.to_rows().is_err());

        assert!(TraceColumns::json_is_well_formed(br#"{"format":"columns","n":3}"#).is_ok());
        assert!(TraceColumns::json_is_well_formed(br#"{"format":"rows","n":3}"#).is_err());
        assert!(TraceColumns::json_is_well_formed(br#"[{"format":"columns"}]"#).is_err());
        assert!(TracePayload::columns_from_json(br#"{"format":"columns"}"#).is_err());
        let payload: Result<TracePayload, _> =
            serde_json::from_str(r#"{"format":"columns","n":1}"#);
        assert!(payload.is_err());
    }

    #[test]
    fn payload_rows_form_serializes_like_trace_rows_and_decodes_arrays() {
        let rows = vec![normal(1, "a", 1, 2), world_bloom(2, "b", 3, 2, Some(4))];
        let payload = TracePayload::encode(rows.clone(), TraceFormat::Rows).unwrap();
        assert_eq!(payload.format(), TraceFormat::Rows);
        assert_eq!(sonic_rs::to_string(&payload).unwrap(), json(&rows));
        let decoded: TracePayload = sonic_rs::from_str(&json(&rows)).unwrap();
        assert_eq!(decoded.format(), TraceFormat::Rows);
        assert_eq!(json(&decoded.rows()), json(&rows));
        assert!(TracePayload::default().is_empty());
        let columns = TracePayload::encode(rows.clone(), TraceFormat::Columns).unwrap();
        assert_eq!(json(&columns.rows()), json(&rows));
    }

    #[test]
    fn rename_user_keeps_the_wire_form() {
        let rows = vec![
            normal(1, "a", 1, 2),
            normal(2, "b", 3, 2),
            normal(3, "a", 4, 2),
        ];
        let renamed = vec![
            normal(1, "x", 1, 2),
            normal(2, "b", 3, 2),
            normal(3, "x", 4, 2),
        ];
        for format in [TraceFormat::Rows, TraceFormat::Columns] {
            let mut payload = TracePayload::encode(rows.clone(), format).unwrap();
            payload.rename_user("a", "x").unwrap();
            assert_eq!(payload.format(), format);
            assert_eq!(json(&payload.rows()), json(&renamed));
        }
    }

    #[test]
    fn trace_format_parses_query_values() {
        assert_eq!(TraceFormat::parse(None).unwrap(), TraceFormat::Rows);
        assert_eq!(TraceFormat::parse(Some("")).unwrap(), TraceFormat::Rows);
        assert_eq!(TraceFormat::parse(Some("rows")).unwrap(), TraceFormat::Rows);
        assert_eq!(
            TraceFormat::parse(Some(" columns ")).unwrap(),
            TraceFormat::Columns
        );
        assert!(TraceFormat::parse(Some("Columns")).is_err());
        assert!(TraceFormat::parse(Some("csv")).is_err());
    }
}
