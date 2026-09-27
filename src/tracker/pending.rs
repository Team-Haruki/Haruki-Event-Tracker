//! The tracker's diffed-but-not-yet-flushed rows.
//!
//! Rows are kept as `(timestamp, uid, score, rank)` — 32 bytes each — and
//! dimension values (name, team, the multi-KB profile JSON) once per user,
//! and only while the memo says the stored row differs: a user whose
//! profile already matches what the writer last wrote keeps no copy at
//! all. A day-long database outage therefore costs the rows plus the few
//! profiles that actually changed, not a full record per row.
//!
//! Flushes take the oldest whole samples: a chunk never splits a sample's
//! rows (main and World Bloom alike), so a reader sees a sample entirely
//! or not at all whatever the chunk size.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::db::query::batch::{SampleRow, UserDimRow, UserMemo, WorldBloomSampleRow};

#[derive(Debug, Default)]
pub(crate) struct PendingBuffer {
    main: Vec<SampleRow>,
    world_bloom: Vec<WorldBloomSampleRow>,
    /// Newest offered values per uid, for users whose stored row (as the
    /// memo knows it) differs. Absent means "the memo is current".
    profiles: HashMap<i64, UserDimRow>,
    /// Newest sample time offered per uid since its rows were last
    /// flushed — kept even when no profile is retained, so an older
    /// listing offered later (a World Bloom row chained after the main
    /// rows) can never displace what the newest sample established.
    offered_ts: HashMap<i64, i64>,
}

/// Whole samples taken out of the buffer for one flush. Put back on
/// failure, in front of anything buffered since.
#[derive(Debug, Default)]
pub(crate) struct FlushChunk {
    pub main: Vec<SampleRow>,
    pub world_bloom: Vec<WorldBloomSampleRow>,
}

impl FlushChunk {
    pub fn is_empty(&self) -> bool {
        self.main.is_empty() && self.world_bloom.is_empty()
    }

    fn uids(&self) -> HashSet<i64> {
        self.main
            .iter()
            .map(|r| r.uid)
            .chain(self.world_bloom.iter().map(|r| r.row.uid))
            .collect()
    }
}

impl PendingBuffer {
    pub fn rows(&self) -> usize {
        self.main.len() + self.world_bloom.len()
    }

    pub fn is_empty(&self) -> bool {
        self.main.is_empty() && self.world_bloom.is_empty()
    }

    pub fn main_len(&self) -> usize {
        self.main.len()
    }

    pub fn world_bloom_len(&self) -> usize {
        self.world_bloom.len()
    }

    pub fn profiles(&self) -> &HashMap<i64, UserDimRow> {
        &self.profiles
    }

    pub fn push_main(&mut self, row: SampleRow) {
        self.main.push(row);
    }

    pub fn push_world_bloom(&mut self, row: WorldBloomSampleRow) {
        self.world_bloom.push(row);
    }

    /// Offer the dimension values a sample at `timestamp` carried for
    /// `uid`. The newest sample wins (ties go to the later offer, so a
    /// World Bloom listing after the main listing of the same tick wins,
    /// as the record-shaped flush resolved it). Values the memo already
    /// holds are not kept — and they drop an older pending copy, because
    /// the newest sample is what the row must end up as.
    pub fn offer_profile(&mut self, uid: i64, timestamp: i64, info: UserDimRow, memo: &UserMemo) {
        if self
            .offered_ts
            .get(&uid)
            .is_some_and(|&held| timestamp < held)
        {
            return;
        }
        self.offered_ts.insert(uid, timestamp);
        if memo.current_key(uid, &info).is_some() {
            self.profiles.remove(&uid);
        } else {
            self.profiles.insert(uid, info);
        }
    }

    /// The oldest whole samples, at most `limit` rows unless the sample
    /// that crosses the limit is itself larger; `limit == 0` takes
    /// everything. Rows keep their buffered order.
    pub fn take_chunk(&mut self, limit: usize) -> FlushChunk {
        if limit == 0 || self.rows() <= limit {
            return FlushChunk {
                main: std::mem::take(&mut self.main),
                world_bloom: std::mem::take(&mut self.world_bloom),
            };
        }
        let cut = cut_timestamp(&self.main, &self.world_bloom, limit);
        let (main, rest_main): (Vec<_>, Vec<_>) =
            self.main.drain(..).partition(|r| r.timestamp <= cut);
        let (world_bloom, rest_wl): (Vec<_>, Vec<_>) = self
            .world_bloom
            .drain(..)
            .partition(|r| r.row.timestamp <= cut);
        self.main = rest_main;
        self.world_bloom = rest_wl;
        FlushChunk { main, world_bloom }
    }

    /// Return a chunk that failed to flush, ahead of anything buffered
    /// since it was taken.
    pub fn put_back(&mut self, mut chunk: FlushChunk) {
        chunk.main.append(&mut self.main);
        chunk.world_bloom.append(&mut self.world_bloom);
        self.main = chunk.main;
        self.world_bloom = chunk.world_bloom;
    }

    /// After `chunk` committed: drop the profiles the memo now holds.
    /// A profile of a user the chunk referenced is always current
    /// afterwards (the flush wrote it and the memo learned it); anything
    /// else stays for the rows still buffered.
    pub fn forget_flushed(&mut self, chunk: &FlushChunk, memo: &UserMemo) {
        for uid in chunk.uids() {
            let current = self
                .profiles
                .get(&uid)
                .is_none_or(|info| memo.current_key(uid, info).is_some());
            if current {
                self.profiles.remove(&uid);
                self.offered_ts.remove(&uid);
            }
        }
    }
}

/// The sample timestamp at which the oldest samples reach `limit` rows.
/// Rows are grouped by timestamp whatever their buffered order.
fn cut_timestamp(main: &[SampleRow], world_bloom: &[WorldBloomSampleRow], limit: usize) -> i64 {
    let mut per_ts: BTreeMap<i64, usize> = BTreeMap::new();
    for ts in main
        .iter()
        .map(|r| r.timestamp)
        .chain(world_bloom.iter().map(|r| r.row.timestamp))
    {
        *per_ts.entry(ts).or_default() += 1;
    }
    let mut taken = 0;
    let mut cut = i64::MIN;
    for (ts, n) in per_ts {
        taken += n;
        cut = ts;
        if taken >= limit {
            break;
        }
    }
    cut
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::query::batch::UserMemoEntry;

    fn row(timestamp: i64, uid: i64) -> SampleRow {
        SampleRow {
            timestamp,
            uid,
            score: uid * 10,
            rank: uid,
        }
    }

    fn wl(timestamp: i64, uid: i64) -> WorldBloomSampleRow {
        WorldBloomSampleRow {
            row: row(timestamp, uid),
            character_id: 1,
        }
    }

    fn profile(name: &str) -> UserDimRow {
        UserDimRow {
            name: name.into(),
            cheerful_team_id: None,
            unique_id: None,
            card_id: None,
            card_level: None,
            card_master_rank: None,
            card_special_training_status: None,
            card_default_image: None,
            profile_word: None,
            profile_honors_json: None,
            honor_missions_json: None,
            player_frames_json: None,
            profile_hash: 7,
        }
    }

    fn memo_with(uid: i64, name: &str) -> UserMemo {
        let mut memo = UserMemo::default();
        memo.insert(
            uid,
            UserMemoEntry {
                user_id_key: 1,
                name: name.into(),
                cheerful_team_id: None,
                unique_id: None,
                profile_hash: Some(7),
            },
        );
        memo
    }

    #[test]
    fn chunks_take_whole_oldest_samples_and_put_back_in_front() {
        let mut buf = PendingBuffer::default();
        for (ts, uid) in [(10, 1), (10, 2), (11, 1), (12, 1), (12, 2), (12, 3)] {
            buf.push_main(row(ts, uid));
        }
        buf.push_world_bloom(wl(10, 5));
        buf.push_world_bloom(wl(12, 5));
        assert_eq!(buf.rows(), 8);

        // Limit 2 is crossed inside sample 10 (three rows): the sample is
        // taken whole, main and World Bloom alike.
        let chunk = buf.take_chunk(2);
        assert_eq!(chunk.main, vec![row(10, 1), row(10, 2)]);
        assert_eq!(chunk.world_bloom, vec![wl(10, 5)]);
        assert_eq!(buf.rows(), 5);

        // A failed flush puts it back ahead of newer rows.
        buf.put_back(chunk);
        assert_eq!(buf.rows(), 8);
        assert_eq!(buf.main[0], row(10, 1));
        assert_eq!(buf.world_bloom[0], wl(10, 5));

        // Limit 4 reaches exactly the end of sample 11.
        let chunk = buf.take_chunk(4);
        assert_eq!(chunk.main, vec![row(10, 1), row(10, 2), row(11, 1)]);
        assert_eq!(chunk.world_bloom, vec![wl(10, 5)]);

        // At or under the limit — or limit 0 — everything goes.
        let chunk = buf.take_chunk(0);
        assert_eq!(chunk.main.len(), 3);
        assert_eq!(chunk.world_bloom.len(), 1);
        assert!(buf.is_empty());
    }

    #[test]
    fn profiles_keep_the_newest_offer_unless_the_memo_already_has_it() {
        let memo = memo_with(1, "stored");
        let mut buf = PendingBuffer::default();

        // Differs from the memo: kept. An older offer never replaces it,
        // a newer one does; a tie goes to the later offer.
        buf.offer_profile(1, 10, profile("renamed"), &memo);
        buf.offer_profile(1, 9, profile("older"), &memo);
        assert_eq!(buf.profiles()[&1].name, "renamed");
        buf.offer_profile(1, 10, profile("tie"), &memo);
        assert_eq!(buf.profiles()[&1].name, "tie");
        buf.offer_profile(1, 11, profile("newest"), &memo);
        assert_eq!(buf.profiles()[&1].name, "newest");

        // The newest sample matching the memo drops the pending copy: the
        // row must end up as the memo already has it.
        buf.offer_profile(1, 12, profile("stored"), &memo);
        assert!(buf.profiles().is_empty());
        // ...but an older matching sample changes nothing, and neither does
        // an older differing sample offered after a matching newer one.
        buf.offer_profile(1, 13, profile("again"), &memo);
        buf.offer_profile(1, 12, profile("stored"), &memo);
        assert_eq!(buf.profiles()[&1].name, "again");
        buf.offer_profile(1, 14, profile("stored"), &memo);
        buf.offer_profile(1, 13, profile("again"), &memo);
        assert!(buf.profiles().is_empty());
        buf.offer_profile(1, 15, profile("again"), &memo);
        assert_eq!(buf.profiles()[&1].name, "again");

        // Unknown users are always kept.
        buf.offer_profile(2, 1, profile("new"), &memo);
        assert_eq!(buf.profiles().len(), 2);

        // After a flush the memo has learned user 1; its copy goes, user
        // 2's stays because the chunk did not reference it.
        buf.push_main(row(13, 1));
        let chunk = buf.take_chunk(0);
        let learned = memo_with(1, "again");
        buf.forget_flushed(&chunk, &learned);
        assert_eq!(buf.profiles().keys().copied().collect::<Vec<_>>(), [2]);
    }
}
