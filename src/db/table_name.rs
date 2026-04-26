//! Single source of truth for per-event table names.
//!
//! Names match the Go reference (`utils/gorm/tables.go`) so a Rust daemon can
//! read or write tables created by the Go daemon without rewriting:
//!
//! - `event_{event_id}_time_id`
//! - `event_{event_id}_users`
//! - `event_{event_id}`
//! - `wl_{event_id}`
//!
//! The names are interned (via `Box::leak`) so they can be stored as
//! `&'static str` inside the SeaORM `Entity { table_name }` value, which has to
//! be `Copy` to satisfy `IdenStatic`. Intern set is bounded — at most a few
//! hundred unique tables over the daemon's lifetime — so the leak is a non-issue.

use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TableKind {
    TimeId,
    EventUsers,
    Event,
    WorldBloom,
}

impl TableKind {
    pub fn format(self, event_id: i64) -> String {
        match self {
            TableKind::TimeId => format!("event_{event_id}_time_id"),
            TableKind::EventUsers => format!("event_{event_id}_users"),
            TableKind::Event => format!("event_{event_id}"),
            TableKind::WorldBloom => format!("wl_{event_id}"),
        }
    }
}

static INTERN: Mutex<Option<HashMap<(TableKind, i64), &'static str>>> = Mutex::new(None);

/// Returns a `'static` name for `(kind, event_id)`. Repeated calls with the
/// same arguments return the same pointer.
pub fn intern(kind: TableKind, event_id: i64) -> &'static str {
    let mut guard = INTERN.lock().expect("table_name intern poisoned");
    let map = guard.get_or_insert_with(HashMap::new);
    if let Some(s) = map.get(&(kind, event_id)) {
        return s;
    }
    let leaked: &'static str = Box::leak(kind.format(event_id).into_boxed_str());
    map.insert((kind, event_id), leaked);
    leaked
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_match_go() {
        assert_eq!(TableKind::TimeId.format(137), "event_137_time_id");
        assert_eq!(TableKind::EventUsers.format(137), "event_137_users");
        assert_eq!(TableKind::Event.format(137), "event_137");
        assert_eq!(TableKind::WorldBloom.format(137), "wl_137");
    }

    #[test]
    fn intern_returns_same_pointer() {
        let a = intern(TableKind::Event, 999_001);
        let b = intern(TableKind::Event, 999_001);
        assert_eq!(a.as_ptr(), b.as_ptr());
        assert_eq!(a, "event_999001");
    }
}
