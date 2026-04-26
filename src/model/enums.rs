use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SekaiServerRegion {
    Jp,
    En,
    Tw,
    Kr,
    Cn,
}

impl SekaiServerRegion {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Jp => "jp",
            Self::En => "en",
            Self::Tw => "tw",
            Self::Kr => "kr",
            Self::Cn => "cn",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "jp" => Some(Self::Jp),
            "en" => Some(Self::En),
            "tw" => Some(Self::Tw),
            "kr" => Some(Self::Kr),
            "cn" => Some(Self::Cn),
            _ => None,
        }
    }
}

impl fmt::Display for SekaiServerRegion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SekaiEventType {
    Marathon,
    CheerfulCarnival,
    WorldBloom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SekaiWorldBloomType {
    GameCharacter,
    Finale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SekaiEventStatus {
    NotStarted,
    Ongoing,
    Aggregating,
    Ended,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SekaiEventSpeedType {
    Hourly,
    SemiDaily,
    Daily,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SekaiUnit {
    None,
    LightSound,
    Idol,
    Street,
    ThemePark,
    SchoolRefusal,
}

pub const SEKAI_EVENT_RANKING_LINES_NORMAL: &[i64] = &[
    10, 20, 30, 40, 50, 100, 200, 300, 400, 500,
    1000, 1500, 2000, 2500, 3000, 4000, 5000,
    10000, 20000, 30000, 40000, 50000,
    100000, 200000, 300000,
];

pub const SEKAI_EVENT_RANKING_LINES_WORLD_BLOOM: &[i64] = &[
    10, 20, 30, 40, 50, 100, 200, 300, 400, 500,
    1000, 2000, 3000, 4000, 5000, 7000,
    10000, 20000, 30000, 40000, 50000, 70000,
    100000,
];
