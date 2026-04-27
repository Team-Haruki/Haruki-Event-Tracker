//! `EventDataParser` — reads `events.json` / `worldBlooms.json` from the
//! per-server master data directory and produces an `EventStatus` for the
//! current real-world wallclock. Direct port of `tracker/eventparser.go`.
//!
//! The Go version exposed a generic hash-cached `LoadData(path) interface{}`
//! that turned out to be dead code (no caller); we drop it. Master data
//! files are small (low MB) and only read on each tracker tick, so a fresh
//! read each call is fine.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::Utc;
use thiserror::Error;
use tokio::fs;

use crate::model::enums::{
    SekaiEventStatus, SekaiEventType, SekaiServerRegion, SekaiWorldBloomType,
};
use crate::model::event::{Event, EventStatus, WorldBloom, WorldBloomChapterStatus};

#[derive(Debug, Error)]
pub enum ParseError {
    #[error("read master data file `{path}`: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("parse master data file `{path}`: {source}")]
    Parse {
        path: String,
        #[source]
        source: sonic_rs::Error,
    },
}

#[derive(Debug, Clone)]
pub struct EventDataParser {
    server: SekaiServerRegion,
    master_dir: PathBuf,
}

impl EventDataParser {
    pub fn new(server: SekaiServerRegion, master_dir: impl Into<PathBuf>) -> Self {
        Self {
            server,
            master_dir: master_dir.into(),
        }
    }

    pub fn server(&self) -> SekaiServerRegion {
        self.server
    }

    pub async fn load_event_data(&self) -> Result<Vec<Event>, ParseError> {
        load_json(&self.master_dir.join("events.json")).await
    }

    pub async fn load_world_bloom_chapter_data(&self) -> Result<Vec<WorldBloom>, ParseError> {
        load_json(&self.master_dir.join("worldBlooms.json")).await
    }

    /// Returns one `WorldBloomChapterStatus` per `game_character_id` for
    /// the given `event_id`, skipping the `Finale` chapter (which is the
    /// joint chapter, not a per-character one).
    #[tracing::instrument(skip(self), fields(server = %self.server, event_id))]
    pub async fn get_world_bloom_character_statuses(
        &self,
        event_id: i64,
    ) -> Result<HashMap<i64, WorldBloomChapterStatus>, ParseError> {
        let chapters = self.load_world_bloom_chapter_data().await?;
        let now = Utc::now().timestamp_millis();
        let mut out = HashMap::new();
        for chapter in chapters {
            if chapter.event_id != event_id {
                continue;
            }
            if chapter.world_bloom_chapter_type == SekaiWorldBloomType::Finale {
                continue;
            }
            let status = if chapter.chapter_end_at <= now {
                SekaiEventStatus::Ended
            } else if chapter.aggregate_at < now && now < chapter.chapter_end_at {
                SekaiEventStatus::Aggregating
            } else if chapter.chapter_start_at < now && now < chapter.aggregate_at {
                SekaiEventStatus::Ongoing
            } else {
                SekaiEventStatus::NotStarted
            };
            out.insert(
                chapter.game_character_id,
                WorldBloomChapterStatus {
                    server: self.server,
                    event_id,
                    character_id: chapter.game_character_id,
                    chapter_status: status,
                },
            );
        }
        Ok(out)
    }

    /// Walks `events.json` and returns the `EventStatus` for the first
    /// event whose `start_at < now < closed_at` window covers the current
    /// wallclock — matches Go's `GetCurrentEventStatus`. `Ok(None)` means
    /// no event is active.
    #[tracing::instrument(skip(self), fields(server = %self.server))]
    pub async fn get_current_event_status(&self) -> Result<Option<EventStatus>, ParseError> {
        let events = self.load_event_data().await?;
        let now = Utc::now().timestamp_millis();
        for event in events {
            if !(event.start_at < now && now < event.closed_at) {
                continue;
            }
            let mut remain = String::new();
            let status = if event.start_at < now && now < event.aggregate_at {
                let remain_secs = (event.aggregate_at - now) / 1000;
                remain = event_time_remain(remain_secs as f64, true, self.server);
                SekaiEventStatus::Ongoing
            } else if event.aggregate_at < now && now < event.aggregate_at + 600_000 {
                SekaiEventStatus::Aggregating
            } else {
                SekaiEventStatus::Ended
            };
            let chapter_statuses = if event.event_type == SekaiEventType::WorldBloom {
                self.get_world_bloom_character_statuses(event.id).await?
            } else {
                HashMap::new()
            };
            return Ok(Some(EventStatus {
                server: self.server,
                event_id: event.id,
                event_type: event.event_type,
                event_status: status,
                remain,
                assetbundle_name: event.assetbundle_name.clone(),
                chapter_statuses,
                detail: event,
            }));
        }
        Ok(None)
    }
}

async fn load_json<T: for<'de> serde::Deserialize<'de>>(path: &Path) -> Result<T, ParseError> {
    let bytes = fs::read(path).await.map_err(|source| ParseError::Read {
        path: path.display().to_string(),
        source,
    })?;
    sonic_rs::from_slice(&bytes).map_err(|source| ParseError::Parse {
        path: path.display().to_string(),
        source,
    })
}

#[derive(Debug, Clone, Copy)]
struct TimeUnit {
    second: &'static str,
    minute: &'static str,
    hour: &'static str,
    day: &'static str,
}

const TIME_UNITS_JP: TimeUnit = TimeUnit {
    second: "秒",
    minute: "分",
    hour: "小时",
    day: "天",
};
const TIME_UNITS_TW: TimeUnit = TimeUnit {
    second: "秒",
    minute: "分",
    hour: "小時",
    day: "天",
};
const TIME_UNITS_EN: TimeUnit = TimeUnit {
    second: "s",
    minute: "m",
    hour: "h",
    day: "d",
};
const TIME_UNITS_KR: TimeUnit = TimeUnit {
    second: "초",
    minute: "분",
    hour: "시간",
    day: "일",
};

fn time_units(server: SekaiServerRegion) -> TimeUnit {
    // JP and CN share zh-cn-style units; TW gets traditional, EN gets ASCII,
    // KR gets hangul. Matches Go `GetTimeTranslations`.
    match server {
        SekaiServerRegion::Jp | SekaiServerRegion::Cn => TIME_UNITS_JP,
        SekaiServerRegion::Tw => TIME_UNITS_TW,
        SekaiServerRegion::En => TIME_UNITS_EN,
        SekaiServerRegion::Kr => TIME_UNITS_KR,
    }
}

/// Format a "remaining time" string. Recursively splits days/hours/min/sec
/// in the locale of `server`. `show_seconds = false` truncates the trailing
/// seconds component on > 1-minute windows.
pub fn event_time_remain(remain: f64, show_seconds: bool, server: SekaiServerRegion) -> String {
    let t = time_units(server);
    let n = remain as i64;
    if remain < 60.0 {
        if show_seconds {
            return format!("{n}{}", t.second);
        }
        return format!("0{}", t.minute);
    }
    if remain < 3600.0 {
        let minutes = n / 60;
        let seconds = n % 60;
        if show_seconds {
            return format!("{minutes}{}{seconds}{}", t.minute, t.second);
        }
        return format!("{minutes}{}", t.minute);
    }
    if remain < 86_400.0 {
        let hours = n / 3600;
        let rem = n - 3600 * hours;
        let minutes = rem / 60;
        let seconds = rem % 60;
        if show_seconds {
            return format!(
                "{hours}{}{minutes}{}{seconds}{}",
                t.hour, t.minute, t.second
            );
        }
        return format!("{hours}{}{minutes}{}", t.hour, t.minute);
    }
    let days = n / 86_400;
    let rem = (n - 86_400 * days) as f64;
    format!(
        "{days}{}{}",
        t.day,
        event_time_remain(rem, show_seconds, server)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remain_under_minute() {
        assert_eq!(event_time_remain(45.0, true, SekaiServerRegion::En), "45s");
        assert_eq!(event_time_remain(45.0, false, SekaiServerRegion::En), "0m");
    }

    #[test]
    fn remain_under_hour() {
        assert_eq!(
            event_time_remain(125.0, true, SekaiServerRegion::En),
            "2m5s"
        );
        assert_eq!(
            event_time_remain(125.0, false, SekaiServerRegion::En),
            "2m"
        );
    }

    #[test]
    fn remain_under_day() {
        assert_eq!(
            event_time_remain(3725.0, true, SekaiServerRegion::En),
            "1h2m5s"
        );
    }

    #[test]
    fn remain_multi_day() {
        // 2 days, 3 hours, 4 minutes, 5 seconds = 183_845s
        assert_eq!(
            event_time_remain(183_845.0, true, SekaiServerRegion::En),
            "2d3h4m5s"
        );
    }

    #[test]
    fn remain_locale_jp() {
        assert_eq!(
            event_time_remain(3725.0, true, SekaiServerRegion::Jp),
            "1小时2分5秒"
        );
    }
}
