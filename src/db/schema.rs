use std::collections::HashSet;
use std::time::{Duration, Instant};

use sea_orm::sea_query::{Alias, Index, IndexCreateStatement};
use sea_orm::{ConnectionTrait, DatabaseBackend, DbErr, FromQueryResult, Schema, Statement};

use crate::db::engine::DatabaseEngine;
use crate::db::entity::{event, event_users, time_id, world_bloom};
use crate::db::table_name::{TableKind, intern};
use crate::model::enums::SekaiServerRegion;

/// Idempotent: creates `event_<id>_time_id`, `event_<id>_users`, `event_<id>`
/// (and `wl_<id>` for World Bloom events) if they don't already exist. Mirrors
/// `DatabaseEngine.CreateEventTables` in `utils/gorm/engine.go:125`.
#[tracing::instrument(skip(engine), fields(server = %server, event_id))]
pub async fn create_event_tables(
    engine: &DatabaseEngine,
    server: SekaiServerRegion,
    event_id: i64,
    is_world_bloom: bool,
) -> Result<(), DbErr> {
    let _ = server;
    let backend = engine.backend();
    let schema = Schema::new(backend);

    let time_id_ent = time_id::Entity {
        table_name: intern(TableKind::TimeId, event_id),
    };
    let users_ent = event_users::Entity {
        table_name: intern(TableKind::EventUsers, event_id),
    };
    let event_ent = event::Entity {
        table_name: intern(TableKind::Event, event_id),
    };

    let mut creates = vec![
        schema.create_table_from_entity(time_id_ent),
        schema.create_table_from_entity(users_ent),
        schema.create_table_from_entity(event_ent),
    ];
    if is_world_bloom {
        let wl_ent = world_bloom::Entity {
            table_name: intern(TableKind::WorldBloom, event_id),
        };
        creates.push(schema.create_table_from_entity(wl_ent));
    }

    let conn = engine.conn();
    for mut stmt in creates {
        stmt.if_not_exists();
        conn.execute(&stmt).await?;
    }
    // Fresh tables get every column from `create_table_from_entity`;
    // migrating older tables is `ensure_user_table_extensions`' job (run
    // from tracker init), so no per-column ALTER probing here.
    create_query_indexes(engine, event_id, is_world_bloom).await?;
    Ok(())
}

pub async fn create_query_indexes(
    engine: &DatabaseEngine,
    event_id: i64,
    is_world_bloom: bool,
) -> Result<(), DbErr> {
    let backend = engine.backend();
    let conn = engine.conn();
    let event_tbl = intern(TableKind::Event, event_id);

    // PostgreSQL: the key-walk indexes are created in their covering form
    // (`covering_indexes`) on a table that has neither form yet — a new
    // event, or one bootstrapped by a version that made no indexes at all,
    // which this call has always indexed in place. A table that already has
    // the plain form is left alone: adding a covering index to a table with
    // history is `create_covering_indexes`' job (concurrent, invoked by
    // hand), never the writer's.
    let mut indexes = Vec::new();
    let covered: HashSet<&'static str> = if backend == DatabaseBackend::Postgres {
        let existing = existing_index_names(engine, event_id, is_world_bloom).await?;
        let mut covered = HashSet::new();
        for cov in covering_indexes(event_id, is_world_bloom) {
            if existing.contains(&cov.name) || existing.contains(&cov.plain_name) {
                covered.insert(cov.plain_suffix);
                continue;
            }
            indexes.push(cov.create_statement());
            covered.insert(cov.plain_suffix);
        }
        covered
    } else {
        HashSet::new()
    };

    let mut plain = vec![
        event_index(event_id, event_tbl, "rank_time", |idx| {
            idx.col(event::Column::Rank).col(event::Column::TimeId);
        }),
        event_index(event_id, event_tbl, "user_time", |idx| {
            idx.col(event::Column::UserIdKey).col(event::Column::TimeId);
        }),
        event_index(event_id, event_tbl, "time_rank", |idx| {
            idx.col(event::Column::TimeId).col(event::Column::Rank);
        }),
        event_index(event_id, event_tbl, "time_score", |idx| {
            idx.col(event::Column::TimeId).col(event::Column::Score);
        }),
    ];

    let users_tbl = intern(TableKind::EventUsers, event_id);
    // Equality column first, `user_id_key` second: `/users` filters paginate
    // with `ORDER BY user_id_key` + keyset cursor, so these serve filter and
    // order in one scan.
    plain.extend([
        event_index(event_id, users_tbl, "users_card_user", |idx| {
            idx.col(event_users::Column::CardId)
                .col(event_users::Column::UserIdKey);
        }),
        event_index(event_id, users_tbl, "users_team_user", |idx| {
            idx.col(event_users::Column::CheerfulTeamId)
                .col(event_users::Column::UserIdKey);
        }),
    ]);

    if is_world_bloom {
        let wl_tbl = intern(TableKind::WorldBloom, event_id);
        plain.extend([
            event_index(event_id, wl_tbl, "wl_char_rank_time", |idx| {
                idx.col(world_bloom::Column::CharacterId)
                    .col(world_bloom::Column::Rank)
                    .col(world_bloom::Column::TimeId);
            }),
            event_index(event_id, wl_tbl, "wl_char_user_time", |idx| {
                idx.col(world_bloom::Column::CharacterId)
                    .col(world_bloom::Column::UserIdKey)
                    .col(world_bloom::Column::TimeId);
            }),
            event_index(event_id, wl_tbl, "wl_char_time_rank", |idx| {
                idx.col(world_bloom::Column::CharacterId)
                    .col(world_bloom::Column::TimeId)
                    .col(world_bloom::Column::Rank);
            }),
        ]);
    }

    indexes.extend(
        plain
            .into_iter()
            .filter(|(suffix, _)| !covered.contains(suffix))
            .map(|(_, stmt)| stmt),
    );

    for mut stmt in indexes {
        if supports_index_if_not_exists(backend) {
            stmt.if_not_exists();
        }
        if let Err(err) = conn.execute(&stmt).await
            && !is_duplicate_index_error(&err)
        {
            return Err(err);
        }
    }
    drop_legacy_user_indexes(engine, event_id, users_tbl).await?;
    Ok(())
}

/// A B-tree whose leaf entries carry the columns a trace reads (`INCLUDE`,
/// PostgreSQL only), so a rank or player trace is an index-only scan
/// instead of one heap fetch per row (`db::query::trace`). Its key columns
/// are those of the plain index it supersedes, so every query the plain
/// one serves — edge probes, `MAX(time_id) GROUP BY`, latest-row lookups —
/// gets the same plan from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoveringIndex {
    pub name: String,
    /// The plain `(key…)` index this one supersedes (`idx_<id>_<suffix>`).
    pub plain_name: String,
    plain_suffix: &'static str,
    pub table: &'static str,
    pub key: &'static [&'static str],
    pub include: &'static [&'static str],
}

impl CoveringIndex {
    fn new(
        event_id: i64,
        plain_suffix: &'static str,
        table: &'static str,
        key: &'static [&'static str],
        include: &'static [&'static str],
    ) -> Self {
        Self {
            name: format!("idx_{event_id}_{plain_suffix}_cov"),
            plain_name: format!("idx_{event_id}_{plain_suffix}"),
            plain_suffix,
            table,
            key,
            include,
        }
    }

    /// `CREATE INDEX [CONCURRENTLY] "<name>" ON "<table>" (<key>) INCLUDE
    /// (<include>)`. `CONCURRENTLY` builds without blocking writes and must
    /// run outside a transaction.
    pub fn create_sql(&self, concurrently: bool) -> String {
        let cols = |cols: &[&str]| {
            cols.iter()
                .map(|col| format!("\"{col}\""))
                .collect::<Vec<_>>()
                .join(", ")
        };
        format!(
            "CREATE INDEX {}\"{}\" ON \"{}\" ({}) INCLUDE ({})",
            if concurrently { "CONCURRENTLY " } else { "" },
            self.name,
            self.table,
            cols(self.key),
            cols(self.include)
        )
    }

    fn create_statement(&self) -> IndexCreateStatement {
        let mut idx = Index::create();
        idx.name(&self.name).table(Alias::new(self.table));
        for col in self.key {
            idx.col(Alias::new(*col));
        }
        for col in self.include {
            idx.include(Alias::new(*col));
        }
        idx.to_owned()
    }
}

/// The covering forms of the event's `(rank, time_id)` and
/// `(user_id_key, time_id)` indexes (World Bloom: with `character_id`
/// leading), in creation order.
pub fn covering_indexes(event_id: i64, is_world_bloom: bool) -> Vec<CoveringIndex> {
    let event_tbl = intern(TableKind::Event, event_id);
    let mut out = vec![
        CoveringIndex::new(
            event_id,
            "rank_time",
            event_tbl,
            &["rank", "time_id"],
            &["user_id_key", "score"],
        ),
        CoveringIndex::new(
            event_id,
            "user_time",
            event_tbl,
            &["user_id_key", "time_id"],
            &["rank", "score"],
        ),
    ];
    if is_world_bloom {
        let wl_tbl = intern(TableKind::WorldBloom, event_id);
        out.extend([
            CoveringIndex::new(
                event_id,
                "wl_char_rank_time",
                wl_tbl,
                &["character_id", "rank", "time_id"],
                &["user_id_key", "score"],
            ),
            CoveringIndex::new(
                event_id,
                "wl_char_user_time",
                wl_tbl,
                &["character_id", "user_id_key", "time_id"],
                &["rank", "score"],
            ),
        ]);
    }
    out
}

#[derive(Debug, FromQueryResult)]
struct NameRow {
    name: String,
}

/// The names of the indexes on the event's ranking tables (PostgreSQL).
async fn existing_index_names(
    engine: &DatabaseEngine,
    event_id: i64,
    is_world_bloom: bool,
) -> Result<HashSet<String>, DbErr> {
    let mut tables = vec![intern(TableKind::Event, event_id).to_owned()];
    if is_world_bloom {
        tables.push(intern(TableKind::WorldBloom, event_id).to_owned());
    }
    let rows = NameRow::find_by_statement(Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        "SELECT indexname AS name FROM pg_indexes \
         WHERE schemaname = current_schema() AND tablename = ANY($1)",
        [tables.into()],
    ))
    .all(engine.conn())
    .await?;
    Ok(rows.into_iter().map(|row| row.name).collect())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoveringIndexAction {
    /// A valid index of that name already exists.
    Skipped,
    Created,
    /// An invalid leftover (an interrupted concurrent build) was dropped
    /// and the index built again.
    Rebuilt,
}

#[derive(Debug, Clone)]
pub struct CoveringIndexReport {
    pub index: CoveringIndex,
    pub action: CoveringIndexAction,
    pub elapsed: Duration,
    /// On-disk size after the build (`pg_relation_size`), when known.
    pub size_bytes: Option<i64>,
}

/// Builds the covering indexes (`covering_indexes`) of one event, or of
/// every event whose tables exist when `event_id` is `None`, with `CREATE
/// INDEX CONCURRENTLY`, one at a time: the operator's path for tables with
/// history. Valid indexes are skipped, invalid leftovers are dropped and
/// rebuilt. The plain indexes stay: dropping them is a separate decision.
/// With `dry_run` nothing is changed and every report says what would be
/// done. PostgreSQL only.
pub async fn create_covering_indexes(
    engine: &DatabaseEngine,
    event_id: Option<i64>,
    dry_run: bool,
) -> Result<Vec<CoveringIndexReport>, DbErr> {
    if engine.backend() != DatabaseBackend::Postgres {
        return Err(DbErr::Custom(
            "covering indexes (INCLUDE) are PostgreSQL only".into(),
        ));
    }
    let events = match event_id {
        Some(event_id) => vec![(
            event_id,
            table_exists(engine, intern(TableKind::WorldBloom, event_id)).await?,
        )],
        None => discover_events(engine).await?,
    };
    let mut reports = Vec::new();
    for (event_id, is_world_bloom) in events {
        if !table_exists(engine, intern(TableKind::Event, event_id)).await? {
            return Err(DbErr::Custom(format!(
                "table {} does not exist",
                intern(TableKind::Event, event_id)
            )));
        }
        for index in covering_indexes(event_id, is_world_bloom) {
            reports.push(build_covering_index(engine, index, dry_run).await?);
        }
    }
    Ok(reports)
}

async fn build_covering_index(
    engine: &DatabaseEngine,
    index: CoveringIndex,
    dry_run: bool,
) -> Result<CoveringIndexReport, DbErr> {
    let conn = engine.conn();
    let started = Instant::now();
    let action = match index_validity(engine, &index.name).await? {
        Some(true) => CoveringIndexAction::Skipped,
        Some(false) => CoveringIndexAction::Rebuilt,
        None => CoveringIndexAction::Created,
    };
    if !dry_run && action != CoveringIndexAction::Skipped {
        if action == CoveringIndexAction::Rebuilt {
            conn.execute_unprepared(&format!("DROP INDEX \"{}\"", index.name))
                .await?;
        }
        conn.execute_unprepared(&index.create_sql(true)).await?;
    }
    let size_bytes = if dry_run && action != CoveringIndexAction::Skipped {
        None
    } else {
        index_size(engine, &index.name).await?
    };
    Ok(CoveringIndexReport {
        index,
        action,
        elapsed: started.elapsed(),
        size_bytes,
    })
}

/// `Some(indisvalid)` for an existing index, `None` for a missing one.
async fn index_validity(engine: &DatabaseEngine, name: &str) -> Result<Option<bool>, DbErr> {
    #[derive(FromQueryResult)]
    struct Row {
        valid: bool,
    }
    Ok(Row::find_by_statement(Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        "SELECT i.indisvalid AS valid FROM pg_class c \
         JOIN pg_index i ON i.indexrelid = c.oid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = current_schema() AND c.relname = $1",
        [name.into()],
    ))
    .one(engine.conn())
    .await?
    .map(|row| row.valid))
}

async fn index_size(engine: &DatabaseEngine, name: &str) -> Result<Option<i64>, DbErr> {
    #[derive(FromQueryResult)]
    struct Row {
        size: i64,
    }
    Ok(Row::find_by_statement(Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        "SELECT pg_relation_size(c.oid) AS size FROM pg_class c \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = current_schema() AND c.relname = $1",
        [name.into()],
    ))
    .one(engine.conn())
    .await?
    .map(|row| row.size))
}

async fn table_exists(engine: &DatabaseEngine, table: &str) -> Result<bool, DbErr> {
    Ok(NameRow::find_by_statement(Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        "SELECT tablename AS name FROM pg_tables \
         WHERE schemaname = current_schema() AND tablename = $1",
        [table.into()],
    ))
    .one(engine.conn())
    .await?
    .is_some())
}

/// Every `(event_id, is_world_bloom)` with an `event_<id>` table, ascending.
async fn discover_events(engine: &DatabaseEngine) -> Result<Vec<(i64, bool)>, DbErr> {
    let rows = NameRow::find_by_statement(Statement::from_string(
        DatabaseBackend::Postgres,
        "SELECT tablename AS name FROM pg_tables \
         WHERE schemaname = current_schema() \
         AND (tablename ~ '^event_[0-9]+$' OR tablename ~ '^wl_[0-9]+$')",
    ))
    .all(engine.conn())
    .await?;
    let mut events: Vec<i64> = Vec::new();
    let mut world_bloom: HashSet<i64> = HashSet::new();
    for row in rows {
        if let Some(id) = row.name.strip_prefix("event_") {
            events.extend(id.parse::<i64>().ok());
        } else if let Some(id) = row.name.strip_prefix("wl_") {
            world_bloom.extend(id.parse::<i64>().ok());
        }
    }
    events.sort_unstable();
    Ok(events
        .into_iter()
        .map(|id| (id, world_bloom.contains(&id)))
        .collect())
}

/// Indexes retired from the bootstrap set, dropped so existing tables shed
/// their write amplification: `users_name` can never serve the
/// leading-wildcard LIKE that queries names, and the single-column card/team
/// indexes are superseded by the composite keyset-pagination ones.
async fn drop_legacy_user_indexes(
    engine: &DatabaseEngine,
    event_id: i64,
    users_tbl: &'static str,
) -> Result<(), DbErr> {
    let backend = engine.backend();
    for suffix in ["users_name", "users_card_id", "users_cheerful_team"] {
        let name = format!("idx_{event_id}_{suffix}");
        let sql = match backend {
            DatabaseBackend::MySql => format!("DROP INDEX `{name}` ON `{users_tbl}`"),
            _ => format!("DROP INDEX IF EXISTS \"{name}\""),
        };
        if let Err(err) = engine
            .conn()
            .execute_raw(Statement::from_string(backend, sql))
            .await
            && !is_missing_index_error(&err)
        {
            return Err(err);
        }
    }
    Ok(())
}

fn is_missing_index_error(err: &DbErr) -> bool {
    let msg = err.to_string().to_ascii_lowercase();
    msg.contains("check that column/key exists")
        || msg.contains("does not exist")
        || msg.contains("no such index")
        || msg.contains("1091")
}

fn event_index(
    event_id: i64,
    table: &'static str,
    suffix: &'static str,
    columns: impl FnOnce(&mut IndexCreateStatement),
) -> (&'static str, IndexCreateStatement) {
    let mut idx = Index::create();
    idx.name(format!("idx_{event_id}_{suffix}"))
        .table(Alias::new(table));
    columns(&mut idx);
    (suffix, idx.to_owned())
}

fn supports_index_if_not_exists(backend: DatabaseBackend) -> bool {
    !matches!(backend, DatabaseBackend::MySql)
}

fn is_duplicate_index_error(err: &DbErr) -> bool {
    let msg = err.to_string().to_ascii_lowercase();
    msg.contains("duplicate key name")
        || msg.contains("already exists")
        || (msg.contains("duplicate") && msg.contains("index"))
}

#[cfg(test)]
mod tests {
    use super::{
        CoveringIndexAction, covering_indexes, create_covering_indexes, create_event_tables,
        is_missing_index_error,
    };
    use sea_orm::{ConnectionTrait, DatabaseBackend, DbErr};

    use crate::db::query::edge::tests::quiet_connect;
    use crate::db::table_name::{TableKind, intern};
    use crate::model::enums::SekaiServerRegion;

    #[test]
    fn covering_indexes_mirror_the_plain_key_walk_indexes() {
        let indexes = covering_indexes(180, true);
        assert_eq!(
            indexes
                .iter()
                .map(|idx| (idx.name.as_str(), idx.plain_name.as_str(), idx.table))
                .collect::<Vec<_>>(),
            vec![
                ("idx_180_rank_time_cov", "idx_180_rank_time", "event_180"),
                ("idx_180_user_time_cov", "idx_180_user_time", "event_180"),
                (
                    "idx_180_wl_char_rank_time_cov",
                    "idx_180_wl_char_rank_time",
                    "wl_180"
                ),
                (
                    "idx_180_wl_char_user_time_cov",
                    "idx_180_wl_char_user_time",
                    "wl_180"
                ),
            ]
        );
        assert_eq!(
            indexes[0].create_sql(true),
            r#"CREATE INDEX CONCURRENTLY "idx_180_rank_time_cov" ON "event_180" ("rank", "time_id") INCLUDE ("user_id_key", "score")"#
        );
        assert_eq!(
            indexes[3].create_sql(false),
            r#"CREATE INDEX "idx_180_wl_char_user_time_cov" ON "wl_180" ("character_id", "user_id_key", "time_id") INCLUDE ("rank", "score")"#
        );
        assert_eq!(covering_indexes(181, false).len(), 2);
    }

    /// `HET_TEST_PG_URL=postgres://... cargo test --lib -- --ignored
    /// covering_indexes_on_postgres`.
    #[tokio::test]
    #[ignore = "needs HET_TEST_PG_URL"]
    async fn covering_indexes_on_postgres() {
        let Ok(url) = std::env::var("HET_TEST_PG_URL") else {
            eprintln!("HET_TEST_PG_URL not set; skipping");
            return;
        };
        let engine = quiet_connect(&url, DatabaseBackend::Postgres).await;
        let conn = engine.conn();
        let (fresh, existing) = (9701, 9702);
        for event_id in [fresh, existing] {
            for kind in [
                TableKind::WorldBloom,
                TableKind::Event,
                TableKind::EventUsers,
                TableKind::TimeId,
            ] {
                conn.execute_unprepared(&format!(
                    "DROP TABLE IF EXISTS {}",
                    intern(kind, event_id)
                ))
                .await
                .unwrap();
            }
        }
        async fn index_names(
            engine: &crate::db::engine::DatabaseEngine,
            event_id: i64,
        ) -> Vec<String> {
            let mut names = super::existing_index_names(engine, event_id, true)
                .await
                .unwrap()
                .into_iter()
                .filter(|name| name.starts_with("idx_"))
                .collect::<Vec<_>>();
            names.sort();
            names
        }
        let names = |event_id: i64| index_names(&engine, event_id);

        // A new event gets the covering form of the key-walk indexes.
        create_event_tables(&engine, SekaiServerRegion::Jp, fresh, true)
            .await
            .unwrap();
        assert_eq!(
            names(fresh).await,
            vec![
                "idx_9701_rank_time_cov",
                "idx_9701_time_rank",
                "idx_9701_time_score",
                "idx_9701_user_time_cov",
                "idx_9701_wl_char_rank_time_cov",
                "idx_9701_wl_char_time_rank",
                "idx_9701_wl_char_user_time_cov",
            ]
        );
        // Running the bootstrap again changes nothing.
        create_event_tables(&engine, SekaiServerRegion::Jp, fresh, true)
            .await
            .unwrap();
        assert_eq!(names(fresh).await.len(), 7);

        // An event whose tables already carry the plain indexes keeps them
        // and gets no covering index from the bootstrap …
        create_event_tables(&engine, SekaiServerRegion::Jp, existing, true)
            .await
            .unwrap();
        for name in [
            "idx_9702_rank_time_cov",
            "idx_9702_user_time_cov",
            "idx_9702_wl_char_rank_time_cov",
            "idx_9702_wl_char_user_time_cov",
        ] {
            conn.execute_unprepared(&format!("DROP INDEX \"{name}\""))
                .await
                .unwrap();
        }
        for (name, table, cols) in [
            ("idx_9702_rank_time", "event_9702", "rank, time_id"),
            ("idx_9702_user_time", "event_9702", "user_id_key, time_id"),
            (
                "idx_9702_wl_char_rank_time",
                "wl_9702",
                "character_id, rank, time_id",
            ),
            (
                "idx_9702_wl_char_user_time",
                "wl_9702",
                "character_id, user_id_key, time_id",
            ),
        ] {
            conn.execute_unprepared(&format!("CREATE INDEX {name} ON {table} ({cols})"))
                .await
                .unwrap();
        }
        create_event_tables(&engine, SekaiServerRegion::Jp, existing, true)
            .await
            .unwrap();
        assert!(
            names(existing)
                .await
                .iter()
                .all(|name| !name.ends_with("_cov")),
            "{:?}",
            names(existing).await
        );

        // … only the explicit, concurrent path adds them: a dry run reports
        // without creating, the real run creates, a rerun skips, and an
        // invalid leftover is rebuilt.
        let actions = |reports: &[super::CoveringIndexReport]| {
            reports
                .iter()
                .map(|r| (r.index.name.clone(), r.action))
                .collect::<Vec<_>>()
        };
        let dry = create_covering_indexes(&engine, Some(existing), true)
            .await
            .unwrap();
        assert!(dry.iter().all(|r| r.action == CoveringIndexAction::Created));
        assert_eq!(dry.len(), 4);
        assert!(
            names(existing)
                .await
                .iter()
                .all(|name| !name.ends_with("_cov"))
        );
        let built = create_covering_indexes(&engine, Some(existing), false)
            .await
            .unwrap();
        assert_eq!(actions(&built), actions(&dry));
        assert!(built.iter().all(|r| r.size_bytes.is_some()));
        assert_eq!(names(existing).await.len(), 11);
        conn.execute_unprepared(
            "UPDATE pg_index SET indisvalid = false WHERE indexrelid = 'idx_9702_user_time_cov'::regclass",
        )
        .await
        .unwrap();
        let again = create_covering_indexes(&engine, Some(existing), false)
            .await
            .unwrap();
        assert_eq!(
            actions(&again),
            vec![
                (
                    "idx_9702_rank_time_cov".to_owned(),
                    CoveringIndexAction::Skipped
                ),
                (
                    "idx_9702_user_time_cov".to_owned(),
                    CoveringIndexAction::Rebuilt
                ),
                (
                    "idx_9702_wl_char_rank_time_cov".to_owned(),
                    CoveringIndexAction::Skipped
                ),
                (
                    "idx_9702_wl_char_user_time_cov".to_owned(),
                    CoveringIndexAction::Skipped
                ),
            ]
        );
        assert_eq!(
            super::index_validity(&engine, "idx_9702_user_time_cov")
                .await
                .unwrap(),
            Some(true)
        );

        // Without an event, every event table in the database is visited.
        let all = create_covering_indexes(&engine, None, true).await.unwrap();
        assert!(
            all.iter()
                .filter(|r| r.index.name.contains("_9701_") || r.index.name.contains("_9702_"))
                .all(|r| r.action == CoveringIndexAction::Skipped)
        );
        assert!(
            create_covering_indexes(&engine, Some(9703), true)
                .await
                .unwrap_err()
                .to_string()
                .contains("does not exist")
        );
        for event_id in [fresh, existing] {
            for kind in [
                TableKind::WorldBloom,
                TableKind::Event,
                TableKind::EventUsers,
                TableKind::TimeId,
            ] {
                conn.execute_unprepared(&format!(
                    "DROP TABLE IF EXISTS {}",
                    intern(kind, event_id)
                ))
                .await
                .unwrap();
            }
        }
    }

    #[test]
    fn missing_index_errors_are_ignorable() {
        for msg in [
            // MySQL 1091
            "Can't DROP 'idx_1_users_name'; check that column/key exists",
            "error 1091 (42000)",
            // Postgres
            "index \"idx_1_users_name\" does not exist",
            // SQLite
            "no such index: idx_1_users_name",
        ] {
            assert!(
                is_missing_index_error(&DbErr::Custom(msg.to_owned())),
                "should be ignorable: {msg}"
            );
        }
    }

    #[test]
    fn other_errors_are_not_ignorable() {
        for msg in [
            "connection refused",
            "syntax error at or near \"DROP\"",
            "permission denied for table event_1_users",
        ] {
            assert!(
                !is_missing_index_error(&DbErr::Custom(msg.to_owned())),
                "should not be ignorable: {msg}"
            );
        }
    }
}
