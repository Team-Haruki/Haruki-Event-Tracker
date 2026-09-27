//! Key sets (ranks, user keys) in query text.
//!
//! `IN ($1, $2, …)` lists and `SELECT $1 UNION ALL SELECT $2 …` key tables
//! produce a different statement per key count, so PostgreSQL keeps one
//! prepared statement (and its plan) per distinct N — about 0.8 MB per
//! backend for the wider ones — and plans each new N from scratch. There,
//! the keys go in as one `bigint[]` parameter: `col = ANY($1)` and
//! `unnest($1)`, whose text is the same for every N. SQLite and MySQL have
//! no array type and keep the portable builders.

use sea_orm::DatabaseBackend;
use sea_orm::sea_query::extension::postgres::PgFunc;
use sea_orm::sea_query::{Alias, Expr, ExprTrait, Func, Query, SelectStatement, UnionType};

/// `col IN (keys)`; on PostgreSQL `col = ANY(CAST($1 AS bigint[]))`.
/// Never true for an empty key set.
pub(crate) fn col_in_keys(backend: DatabaseBackend, col: Expr, keys: &[i64]) -> Expr {
    match backend {
        DatabaseBackend::Postgres => col.eq(PgFunc::any(bigint_array(keys))),
        _ => col.is_in(keys.iter().copied()),
    }
}

/// A one-column derived table of the keys, in key order:
/// `SELECT $1 AS <col> UNION ALL SELECT $2 AS <col> …`, or on PostgreSQL
/// `SELECT key_rows.key_rows AS <col> FROM unnest(CAST($1 AS bigint[])) AS
/// key_rows` (a scalar set-returning function's one column is named after
/// its table alias). Empty key sets yield no rows on every backend.
pub(crate) fn keys_select(backend: DatabaseBackend, keys: &[i64], col: &str) -> SelectStatement {
    let col = Alias::new(col);
    match backend {
        DatabaseBackend::Postgres => {
            let source = Alias::new("key_rows");
            Query::select()
                .expr_as(Expr::col((source.clone(), source.clone())), col)
                .from_function(
                    Func::cust(Alias::new("unnest")).arg(bigint_array(keys)),
                    source,
                )
                .to_owned()
        }
        _ => union_all_keys(keys, col),
    }
}

fn union_all_keys(keys: &[i64], col: Alias) -> SelectStatement {
    let mut keys = keys.iter().copied();
    let mut stmt = Query::select();
    let Some(first) = keys.next() else {
        stmt.expr_as(Expr::val(0i64), col)
            .and_where(Expr::val(1i64).eq(0i64));
        return stmt;
    };
    stmt.expr_as(Expr::val(first), col.clone());
    stmt.unions(keys.map(|key| {
        (
            UnionType::All,
            Query::select()
                .expr_as(Expr::val(key), col.clone())
                .to_owned(),
        )
    }));
    stmt
}

/// Keeps a derived table from being pulled up into the enclosing query
/// (PostgreSQL would otherwise re-evaluate its correlated subqueries as
/// join conditions). `LIMIT n` (`n` = the row count, a no-op) is the
/// portable fence; PostgreSQL gets `OFFSET 0`, whose value is the same for
/// every key count.
pub(crate) fn fence(backend: DatabaseBackend, stmt: &mut SelectStatement, rows: usize) {
    match backend {
        DatabaseBackend::Postgres => {
            stmt.offset(0);
        }
        _ => {
            stmt.limit(rows.max(1) as u64);
        }
    }
}

fn bigint_array(keys: &[i64]) -> Expr {
    Expr::val(keys.to_vec()).cast_as(Alias::new("bigint[]"))
}

#[cfg(test)]
mod tests {
    use sea_orm::sea_query::{MysqlQueryBuilder, PostgresQueryBuilder, SqliteQueryBuilder};

    use super::*;

    /// The parametrised text, which is what the server caches a plan for.
    fn pg(stmt: &SelectStatement) -> String {
        stmt.build(PostgresQueryBuilder).0
    }

    #[test]
    fn postgres_text_does_not_vary_with_key_count() {
        let col = || Expr::col(Alias::new("rank"));
        let one = Query::select()
            .expr(col())
            .and_where(col_in_keys(DatabaseBackend::Postgres, col(), &[1]))
            .to_owned();
        let many = Query::select()
            .expr(col())
            .and_where(col_in_keys(
                DatabaseBackend::Postgres,
                col(),
                &(1..=500).collect::<Vec<_>>(),
            ))
            .to_owned();
        let (one_sql, one_values) = one.build(PostgresQueryBuilder);
        let (many_sql, many_values) = many.build(PostgresQueryBuilder);
        assert_eq!(one_sql, many_sql);
        assert_eq!(
            one_sql,
            r#"SELECT "rank" WHERE "rank" = ANY(CAST($1 AS bigint[]))"#
        );
        assert_eq!(one_values.0.len(), 1);
        assert_eq!(many_values.0.len(), 1);

        let one = keys_select(DatabaseBackend::Postgres, &[7], "rank");
        let many = keys_select(DatabaseBackend::Postgres, &[7, 8, 9], "rank");
        assert_eq!(pg(&one), pg(&many));
        assert_eq!(
            pg(&one),
            r#"SELECT "key_rows"."key_rows" AS "rank" FROM unnest(CAST($1 AS bigint[])) AS "key_rows""#
        );
        let mut fenced = many;
        fence(DatabaseBackend::Postgres, &mut fenced, 3);
        assert!(pg(&fenced).ends_with(" OFFSET $2"), "{}", pg(&fenced));
    }

    #[test]
    fn other_backends_keep_portable_forms() {
        let col = || Expr::col(Alias::new("rank"));
        let stmt = Query::select()
            .expr(col())
            .and_where(col_in_keys(DatabaseBackend::Sqlite, col(), &[1, 2]))
            .to_owned();
        assert_eq!(
            stmt.to_string(SqliteQueryBuilder),
            r#"SELECT "rank" WHERE "rank" IN (1, 2)"#
        );
        let mut keys = keys_select(DatabaseBackend::MySql, &[1, 2], "rank");
        fence(DatabaseBackend::MySql, &mut keys, 2);
        assert_eq!(
            keys.to_string(MysqlQueryBuilder),
            "SELECT 1 AS `rank` UNION ALL (SELECT 2 AS `rank`) LIMIT 2"
        );
        let empty = keys_select(DatabaseBackend::Sqlite, &[], "rank");
        assert_eq!(
            empty.to_string(SqliteQueryBuilder),
            r#"SELECT 0 AS "rank" WHERE 1 = 0"#
        );
    }
}
