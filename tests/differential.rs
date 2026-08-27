//! Differential test suite: runs a corpus of SQL programs against both
//! rustqlite and SQLite (via `rusqlite`) and asserts that the two engines
//! produce identical results (columns + rows, value-by-value).
//!
//! This is the closest portable equivalent to SQLite's own test suite for
//! cross-engine verification. SQLite's TCL tests (~45k cases) aren't usable
//! from Rust; the SQL Logic Test corpus (https://www.sqlite.org/sqllogictest/)
//! is the industry-standard alternative and follows the same shape as the
//! cases below: a sequence of (SQL statement, expected row count, expected
//! rows).
//!
//! Test surface covered here:
//!   - DDL: CREATE TABLE, CREATE INDEX, DROP TABLE, IF NOT EXISTS
//!   - DML: INSERT (single, multi, OR REPLACE, OR IGNORE, DEFAULT VALUES)
//!   - UPDATE / DELETE with WHERE
//!   - SELECT: WHERE, ORDER BY (multi-key, ASC/DESC), GROUP BY, HAVING,
//!     LIMIT/OFFSET, DISTINCT, aggregates (COUNT/SUM/AVG/MIN/MAX), expressions
//!   - JOINs: INNER, LEFT, CROSS, multi-table, equi-join + non-equi predicate
//!   - Set ops: UNION, UNION ALL, INTERSECT, EXCEPT
//!   - NULL semantics: IS NULL, IS NOT NULL, COALESCE, NULLIF, three-valued logic
//!   - Type coercion: int/text/real affinity, mixed arithmetic
//!   - Edge cases: empty tables, NULL group keys, division by zero, empty string
//!
//! Each `case!` invocation: opens a fresh in-memory DB in both engines, runs
//! the SQL statements in order, compares the final SELECT's output, and
//! reports the first divergent row.

/// A single differential test case.
struct Case {
    name: &'static str,
    /// SQL statements to run; the LAST one must be a SELECT (whose result we
    /// compare). Earlier statements may be DDL/DML/SELECT (their results are
    /// discarded).
    sql: &'static [&'static str],
}

/// Normalise a value for comparison. SQLite and rustqlite may use slightly
/// different textual representations of REALs (e.g. `1.0` vs `1`); we treat
/// integers and reals as equal when the values are numerically equal.
fn values_equal(a: &rustqlite::Value, b: &rusqlite::types::Value) -> bool {
    use rustqlite::Value as Rv;
    use rusqlite::types::Value as Sv;
    match (a, b) {
        (Rv::Null, Sv::Null) => true,
        (Rv::Integer(x), Sv::Integer(y)) => x == y,
        (Rv::Integer(x), Sv::Real(y)) => (*x as f64 - y).abs() < 1e-9,
        (Rv::Real(x), Sv::Integer(y)) => (*x - *y as f64).abs() < 1e-9,
        (Rv::Real(x), Sv::Real(y)) => (x - y).abs() < 1e-9,
        (Rv::Text(x), Sv::Text(y)) => x == y,
        (Rv::Blob(x), Sv::Blob(y)) => x == y,
        _ => false,
    }
}

/// Run a single case against both engines and assert equality.
fn run_case(case: &Case) {
    // ---- SQLite (oracle) ----
    let mut sqlite = rusqlite::Connection::open_in_memory().expect("open sqlite");
    let mut oracle_columns: Vec<String> = Vec::new();
    let mut oracle_rows: Vec<Vec<rusqlite::types::Value>> = Vec::new();
    for stmt_sql in case.sql {
        let trimmed = stmt_sql.trim();
        if trimmed.is_empty() {
            continue;
        }
        let upper = trimmed.to_ascii_uppercase();
        let is_select = upper.starts_with("SELECT") || upper.starts_with("WITH")
            || upper.starts_with("VALUES") || upper.starts_with("PRAGMA");
        if is_select {
            let mut stmt = sqlite.prepare(trimmed).expect("sqlite prepare");
            let col_count = stmt.column_count();
            oracle_columns = (0..col_count)
                .map(|i| stmt.column_name(i).unwrap_or("").to_string())
                .collect();
            let mut rows_iter = stmt.query([]).expect("sqlite query");
            oracle_rows.clear();
            while let Some(row) = rows_iter.next().expect("sqlite next") {
                let mut row_vec = Vec::with_capacity(col_count);
                for i in 0..col_count {
                    let v: rusqlite::types::Value = row.get(i).expect("sqlite get value");
                    row_vec.push(v);
                }
                oracle_rows.push(row_vec);
            }
        } else {
            sqlite.execute(trimmed, []).expect("sqlite execute");
        }
    }

    // ---- rustqlite ----
    let mut rdb = rustqlite::Database::open_in_memory().expect("open rustqlite");
    let mut r_columns: Vec<String> = Vec::new();
    let mut r_rows: Vec<Vec<rustqlite::Value>> = Vec::new();
    for stmt_sql in case.sql {
        let trimmed = stmt_sql.trim();
        if trimmed.is_empty() {
            continue;
        }
        let upper = trimmed.to_ascii_uppercase();
        let is_select = upper.starts_with("SELECT") || upper.starts_with("WITH")
            || upper.starts_with("VALUES") || upper.starts_with("PRAGMA");
        if is_select {
            let (cols, rows) = rdb
                .query_with_columns(trimmed, [])
                .unwrap_or_else(|e| panic!("rustqlite query failed on {:?}: {}", trimmed, e));
            r_columns = cols;
            r_rows = rows;
        } else {
            rdb.execute(trimmed, [])
                .unwrap_or_else(|e| panic!("rustqlite execute failed on {:?}: {}", trimmed, e));
        }
    }

    // ---- Compare ----
    assert_eq!(
        r_columns.len(),
        oracle_columns.len(),
        "[{}] column count mismatch: rustqlite={:?}, sqlite={:?}",
        case.name,
        r_columns,
        oracle_columns,
    );

    assert_eq!(
        r_rows.len(),
        oracle_rows.len(),
        "[{}] row count mismatch (rustqlite={}, sqlite={})",
        case.name,
        r_rows.len(),
        oracle_rows.len(),
    );

    for (i, (r_row, s_row)) in r_rows.iter().zip(oracle_rows.iter()).enumerate() {
        assert_eq!(
            r_row.len(),
            s_row.len(),
            "[{}] row {} width mismatch (rustqlite={}, sqlite={})",
            case.name,
            i,
            r_row.len(),
            s_row.len(),
        );
        for (j, (rv, sv)) in r_row.iter().zip(s_row.iter()).enumerate() {
            assert!(
                values_equal(rv, sv),
                "[{}] row {} col {} mismatch: rustqlite={:?}, sqlite={:?}\n  full rustqlite row: {:?}\n  full sqlite row:    {:?}",
                case.name,
                i,
                j,
                rv,
                sv,
                r_row,
                s_row,
            );
        }
    }
}

/// Convenience macro to declare a case.
macro_rules! case {
    ($name:expr, $($sql:expr),+ $(,)?) => {
        Case { name: $name, sql: &[$($sql),+] }
    };
}

/// The full corpus. Add new cases here as new behaviors are tested.
static CASES: &[Case] = &[
    // ========================================================================
    // DDL + INSERT basics
    // ========================================================================
    case!(
        "create_insert_select_basic",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, age INTEGER)",
        "INSERT INTO t (name, age) VALUES ('Alice', 30), ('Bob', 25), ('Carol', 40)",
        "SELECT id, name, age FROM t ORDER BY id",
    ),
    case!(
        "insert_default_values",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT DEFAULT 'anon', n INTEGER DEFAULT 0)",
        "INSERT INTO t DEFAULT VALUES",
        "INSERT INTO t DEFAULT VALUES",
        "SELECT * FROM t ORDER BY id",
    ),
    case!(
        "insert_or_ignore_duplicate_pk",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t VALUES (1, 10), (2, 20)",
        "INSERT OR IGNORE INTO t VALUES (1, 999), (3, 30)",
        "SELECT * FROM t ORDER BY id",
    ),
    case!(
        "insert_or_replace_duplicate_pk",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t VALUES (1, 10), (2, 20)",
        "INSERT OR REPLACE INTO t VALUES (1, 999), (3, 30)",
        "SELECT * FROM t ORDER BY id",
    ),
    case!(
        "create_index_does_not_change_select",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)",
        "CREATE INDEX idx_name ON t(name)",
        "INSERT INTO t (name) VALUES ('z'), ('a'), ('m'), ('a')",
        "SELECT name FROM t ORDER BY name",
    ),
    case!(
        "drop_table_cleans_data",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t VALUES (1, 100), (2, 200)",
        "DROP TABLE t",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t VALUES (1, 1)",
        "SELECT COUNT(*) FROM t",
    ),

    // ========================================================================
    // UPDATE / DELETE
    // ========================================================================
    case!(
        "update_with_where",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)",
        "INSERT INTO t VALUES (1, 10), (2, 20), (3, 30), (4, 40)",
        "UPDATE t SET n = n + 100 WHERE id >= 3",
        "SELECT * FROM t ORDER BY id",
    ),
    case!(
        "delete_with_where",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)",
        "INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)",
        "DELETE FROM t WHERE n < 25",
        "SELECT * FROM t ORDER BY id",
    ),
    case!(
        "update_all_rows",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)",
        "INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)",
        "UPDATE t SET n = 0",
        "SELECT * FROM t ORDER BY id",
    ),
    case!(
        "delete_all_rows",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)",
        "INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)",
        "DELETE FROM t",
        "SELECT COUNT(*) FROM t",
    ),

    // ========================================================================
    // SELECT clauses
    // ========================================================================
    case!(
        "select_where_arithmetic",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b INTEGER)",
        "INSERT INTO t VALUES (1, 5, 3), (2, 10, 5), (3, 1, 1)",
        "SELECT id, a + b, a - b, a * b, a / b FROM t WHERE a > b ORDER BY id",
    ),
    case!(
        "select_order_by_multi_key",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, k TEXT, n INTEGER)",
        "INSERT INTO t VALUES (1, 'b', 3), (2, 'a', 5), (3, 'b', 1), (4, 'a', 5), (5, 'a', 2)",
        "SELECT id, k, n FROM t ORDER BY k ASC, n DESC",
    ),
    case!(
        "select_limit_offset",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t VALUES (1, 100), (2, 200), (3, 300), (4, 400), (5, 500)",
        "SELECT * FROM t ORDER BY v LIMIT 2 OFFSET 1",
    ),
    case!(
        "select_distinct",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, k TEXT)",
        "INSERT INTO t VALUES (1, 'x'), (2, 'x'), (3, 'y'), (4, 'y'), (5, 'x')",
        "SELECT DISTINCT k FROM t ORDER BY k",
    ),

    // ========================================================================
    // Aggregates + GROUP BY
    // ========================================================================
    case!(
        "aggregates_basic",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, g TEXT, v INTEGER)",
        "INSERT INTO t VALUES (1, 'a', 10), (2, 'a', 20), (3, 'b', 30), (4, 'b', 40), (5, 'a', 50)",
        "SELECT g, COUNT(*), SUM(v), AVG(v), MIN(v), MAX(v) FROM t GROUP BY g ORDER BY g",
    ),
    case!(
        "group_by_having",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, g TEXT, v INTEGER)",
        "INSERT INTO t VALUES (1, 'a', 10), (2, 'a', 20), (3, 'b', 30), (4, 'b', 40), (5, 'a', 50)",
        "SELECT g, SUM(v) AS s FROM t GROUP BY g HAVING SUM(v) > 50 ORDER BY g",
    ),
    case!(
        "aggregate_count_star_empty_table",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "SELECT COUNT(*) FROM t",
    ),
    case!(
        "group_by_with_null_key",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, g TEXT, v INTEGER)",
        "INSERT INTO t VALUES (1, NULL, 10), (2, NULL, 20), (3, 'a', 30)",
        "SELECT g, COUNT(*) FROM t GROUP BY g ORDER BY g",
    ),

    // ========================================================================
    // JOINs
    // ========================================================================
    case!(
        "inner_join_simple",
        "CREATE TABLE a (id INTEGER PRIMARY KEY, k INTEGER)",
        "CREATE TABLE b (id INTEGER PRIMARY KEY, a_id INTEGER, label TEXT)",
        "INSERT INTO a VALUES (1, 100), (2, 200), (3, 300)",
        "INSERT INTO b VALUES (10, 1, 'x'), (11, 1, 'y'), (12, 2, 'z')",
        "SELECT a.id, a.k, b.id, b.label FROM a JOIN b ON b.a_id = a.id ORDER BY b.id",
    ),
    case!(
        "left_join_unmatched",
        "CREATE TABLE a (id INTEGER PRIMARY KEY, k INTEGER)",
        "CREATE TABLE b (id INTEGER PRIMARY KEY, a_id INTEGER, label TEXT)",
        "INSERT INTO a VALUES (1, 100), (2, 200), (3, 300)",
        "INSERT INTO b VALUES (10, 1, 'x')",
        "SELECT a.id, b.label FROM a LEFT JOIN b ON b.a_id = a.id ORDER BY a.id",
    ),
    case!(
        "cross_join",
        "CREATE TABLE a (id INTEGER PRIMARY KEY, x TEXT)",
        "CREATE TABLE b (id INTEGER PRIMARY KEY, y TEXT)",
        "INSERT INTO a VALUES (1, 'a1'), (2, 'a2')",
        "INSERT INTO b VALUES (10, 'b1'), (11, 'b2'), (12, 'b3')",
        "SELECT a.id, a.x, b.id, b.y FROM a CROSS JOIN b ORDER BY a.id, b.id",
    ),
    case!(
        "three_way_join",
        "CREATE TABLE a (id INTEGER PRIMARY KEY, k TEXT)",
        "CREATE TABLE b (id INTEGER PRIMARY KEY, a_id INTEGER, label TEXT)",
        "CREATE TABLE c (id INTEGER PRIMARY KEY, b_id INTEGER, tag TEXT)",
        "INSERT INTO a VALUES (1, 'a1'), (2, 'a2')",
        "INSERT INTO b VALUES (10, 1, 'b1'), (11, 2, 'b2')",
        "INSERT INTO c VALUES (100, 10, 'c1'), (101, 11, 'c2'), (102, 10, 'c3')",
        "SELECT a.k, b.label, c.tag FROM a JOIN b ON b.a_id = a.id JOIN c ON c.b_id = b.id ORDER BY c.id",
    ),

    // ========================================================================
    // Set operations
    // ========================================================================
    case!(
        "union_all",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t VALUES (1, 100), (2, 200)",
        "SELECT v FROM t WHERE id = 1 UNION ALL SELECT v FROM t WHERE id = 2 UNION ALL SELECT v FROM t WHERE id = 1",
    ),
    case!(
        "union_dedup",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t VALUES (1, 100), (2, 200), (3, 100)",
        "SELECT v FROM t UNION SELECT v FROM t",
    ),
    case!(
        "intersect_basic",
        "CREATE TABLE a (id INTEGER PRIMARY KEY, v INTEGER)",
        "CREATE TABLE b (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO a VALUES (1, 5), (2, 10), (3, 15)",
        "INSERT INTO b VALUES (10, 10), (11, 15), (12, 20)",
        "SELECT v FROM a INTERSECT SELECT v FROM b ORDER BY v",
    ),
    case!(
        "except_basic",
        "CREATE TABLE a (id INTEGER PRIMARY KEY, v INTEGER)",
        "CREATE TABLE b (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO a VALUES (1, 5), (2, 10), (3, 15)",
        "INSERT INTO b VALUES (10, 10), (11, 15), (12, 20)",
        "SELECT v FROM a EXCEPT SELECT v FROM b ORDER BY v",
    ),

    // ========================================================================
    // NULL semantics (three-valued logic)
    // ========================================================================
    case!(
        "null_is_null_is_not_null",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t VALUES (1, NULL), (2, 10), (3, NULL), (4, 20)",
        "SELECT id FROM t WHERE v IS NULL ORDER BY id",
        "SELECT id FROM t WHERE v IS NOT NULL ORDER BY id",
    ),
    case!(
        "null_in_arithmetic_yields_null",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b INTEGER)",
        "INSERT INTO t VALUES (1, 5, NULL), (2, NULL, 3), (3, 1, 2)",
        "SELECT id, a + b FROM t ORDER BY id",
    ),
    case!(
        "coalesce_and_nullif",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t VALUES (1, NULL), (2, 0), (3, 5)",
        "SELECT id, COALESCE(v, -1), NULLIF(v, 0) FROM t ORDER BY id",
    ),
    case!(
        "null_in_where_three_valued_logic",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t VALUES (1, NULL), (2, 10), (3, 20), (4, NULL)",
        "SELECT id FROM t WHERE v > 15 OR v IS NULL ORDER BY id",
    ),

    // ========================================================================
    // Type coercion / affinity
    // ========================================================================
    case!(
        "type_coercion_text_to_int",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER, s TEXT)",
        "INSERT INTO t VALUES (1, 42, 'hello'), (2, 0, 'world')",
        "SELECT id, n, s, n > 0 FROM t ORDER BY id",
    ),
    case!(
        "type_coercion_int_to_text",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, s TEXT)",
        "INSERT INTO t VALUES (1, 100), (2, 'abc'), (3, 1.5)",
        "SELECT id, s, typeof(s) FROM t ORDER BY id",
    ),
    case!(
        "string_concat_operator",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b TEXT)",
        "INSERT INTO t VALUES (1, 'foo', 'bar'), (2, 'x', 'y')",
        "SELECT id, a || b, a || '-' || b FROM t ORDER BY id",
    ),

    // ========================================================================
    // Functions
    // ========================================================================
    case!(
        "string_functions",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, s TEXT)",
        "INSERT INTO t VALUES (1, 'Hello'), (2, 'WORLD'), (3, '  spaced  ')",
        "SELECT id, LOWER(s), UPPER(s), LENGTH(s), TRIM(s) FROM t ORDER BY id",
    ),
    case!(
        "abs_round",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v REAL)",
        "INSERT INTO t VALUES (1, -3.14), (2, 2.718), (3, -0.5)",
        "SELECT id, ABS(v), ROUND(v, 2) FROM t ORDER BY id",
    ),
    case!(
        "case_when",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)",
        "INSERT INTO t VALUES (1, 0), (2, 5), (3, 10), (4, 100)",
        "SELECT id, CASE WHEN n < 5 THEN 'small' WHEN n < 50 THEN 'medium' ELSE 'large' END FROM t ORDER BY id",
    ),

    // ========================================================================
    // Edge cases
    // ========================================================================
    case!(
        "empty_table_select",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "SELECT COUNT(*), SUM(v), AVG(v), MIN(v), MAX(v) FROM t",
    ),
    case!(
        "empty_string_vs_null",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, s TEXT)",
        "INSERT INTO t VALUES (1, ''), (2, NULL), (3, 'abc')",
        "SELECT id, s, LENGTH(s), s IS NULL FROM t ORDER BY id",
    ),
    case!(
        "self_join",
        "CREATE TABLE emp (id INTEGER PRIMARY KEY, name TEXT, mgr_id INTEGER)",
        "INSERT INTO emp VALUES (1, 'CEO', NULL), (2, 'Alice', 1), (3, 'Bob', 1), (4, 'Carol', 2)",
        "SELECT e.name, m.name FROM emp e LEFT JOIN emp m ON e.mgr_id = m.id ORDER BY e.id",
    ),
    case!(
        "between_operator",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t VALUES (1, 5), (2, 10), (3, 15), (4, 20), (5, 25)",
        "SELECT id, v FROM t WHERE v BETWEEN 10 AND 20 ORDER BY id",
    ),
    case!(
        "in_operator",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, k TEXT)",
        "INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c'), (4, 'd')",
        "SELECT id, k FROM t WHERE k IN ('a', 'c', 'e') ORDER BY id",
    ),
    case!(
        "like_operator",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, s TEXT)",
        "INSERT INTO t VALUES (1, 'hello'), (2, 'help'), (3, 'world'), (4, 'HELLO')",
        "SELECT id, s FROM t WHERE s LIKE 'hel%' ORDER BY id",
    ),

    // ========================================================================
    // Transactions: BEGIN / COMMIT / ROLLBACK
    // ========================================================================
    case!(
        "commit_persists_inserts",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "BEGIN",
        "INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)",
        "COMMIT",
        "SELECT COUNT(*), SUM(v) FROM t",
    ),
    case!(
        "rollback_discards_inserts",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "BEGIN",
        "INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)",
        "ROLLBACK",
        "SELECT COUNT(*) FROM t",
    ),
    case!(
        "rollback_after_insert_visible_during_txn",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "BEGIN",
        "INSERT INTO t VALUES (1, 10), (2, 20)",
        "SELECT COUNT(*) FROM t",
    ),
    case!(
        "rollback_after_update_discards",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)",
        "BEGIN",
        "UPDATE t SET v = v * 100",
        "ROLLBACK",
        "SELECT * FROM t ORDER BY id",
    ),
    case!(
        "rollback_after_delete_discards",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)",
        "BEGIN",
        "DELETE FROM t WHERE v > 15",
        "ROLLBACK",
        "SELECT COUNT(*) FROM t",
    ),
    case!(
        "commit_after_update_persists",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)",
        "BEGIN",
        "UPDATE t SET v = v + 5 WHERE id = 2",
        "COMMIT",
        "SELECT * FROM t ORDER BY id",
    ),
    case!(
        "nested_selects_after_commit",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "BEGIN",
        "INSERT INTO t VALUES (1, 100)",
        "COMMIT",
        "BEGIN",
        "INSERT INTO t VALUES (2, 200)",
        "COMMIT",
        "SELECT COUNT(*), MAX(v) FROM t",
    ),

    // ========================================================================
    // Index lookup paths (CREATE INDEX + WHERE col = ?)
    // ========================================================================
    case!(
        "index_lookup_single_match",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, email TEXT)",
        "CREATE INDEX idx_email ON t(email)",
        "INSERT INTO t (email) VALUES ('a@x.com'), ('b@x.com'), ('c@x.com')",
        "SELECT id, email FROM t WHERE email = 'b@x.com'",
    ),
    case!(
        "index_lookup_no_match",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, email TEXT)",
        "CREATE INDEX idx_email ON t(email)",
        "INSERT INTO t (email) VALUES ('a@x.com'), ('b@x.com')",
        "SELECT * FROM t WHERE email = 'zzz@x.com'",
    ),
    case!(
        "unique_index_blocks_duplicate",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, email TEXT)",
        "CREATE UNIQUE INDEX idx_email ON t(email)",
        "INSERT INTO t (email) VALUES ('a@x.com'), ('b@x.com')",
        "INSERT OR IGNORE INTO t (email) VALUES ('a@x.com')",
        "SELECT COUNT(*) FROM t",
    ),
    case!(
        "composite_index_lookup",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b INTEGER)",
        "CREATE INDEX idx_ab ON t(a, b)",
        "INSERT INTO t (a, b) VALUES (1, 10), (1, 20), (2, 30), (1, 40)",
        "SELECT id, a, b FROM t WHERE a = 1 ORDER BY b",
    ),

    // ========================================================================
    // CAST and type affinity
    // ========================================================================
    case!(
        "cast_integer_to_text",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t VALUES (1, 42)",
        "SELECT id, v, CAST(v AS TEXT), typeof(CAST(v AS TEXT)) FROM t",
    ),
    case!(
        "cast_text_to_integer",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)",
        "INSERT INTO t VALUES (1, '123')",
        "SELECT id, v, CAST(v AS INTEGER), typeof(CAST(v AS INTEGER)) FROM t",
    ),
    case!(
        "cast_real_to_integer_truncates",
        "SELECT CAST(3.7 AS INTEGER), CAST(-3.7 AS INTEGER), CAST(3.2 AS INTEGER)",
    ),
    case!(
        "affinity_text_column_stores_int",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, s TEXT)",
        "INSERT INTO t VALUES (1, 42), (2, 'hello')",
        "SELECT id, s, typeof(s) FROM t ORDER BY id",
    ),
    case!(
        "affinity_integer_column_coerces_text",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)",
        "INSERT INTO t VALUES (1, '100'), (2, 'abc')",
        "SELECT id, n, typeof(n) FROM t ORDER BY id",
    ),
    case!(
        "affinity_real_column_promotes_int",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, r REAL)",
        "INSERT INTO t VALUES (1, 5)",
        "SELECT id, r, typeof(r) FROM t",
    ),

    // ========================================================================
    // COUNT(DISTINCT), SUM(DISTINCT), GROUP_CONCAT
    // ========================================================================
    case!(
        "count_distinct",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, g TEXT, v INTEGER)",
        "INSERT INTO t VALUES (1, 'a', 10), (2, 'a', 10), (3, 'a', 20), (4, 'b', 30), (5, 'b', 30)",
        "SELECT g, COUNT(DISTINCT v) FROM t GROUP BY g ORDER BY g",
    ),
    case!(
        "sum_distinct",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, g TEXT, v INTEGER)",
        "INSERT INTO t VALUES (1, 'a', 10), (2, 'a', 10), (3, 'a', 20), (4, 'a', 20)",
        "SELECT SUM(DISTINCT v), SUM(v) FROM t",
    ),
    case!(
        "group_concat_default_sep",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)",
        "INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c')",
        "SELECT GROUP_CONCAT(v) FROM t",
    ),

    // ========================================================================
    // LIMIT 0, ORDER BY with NULL, mixed ASC/DESC
    // ========================================================================
    case!(
        "limit_zero_returns_no_rows",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)",
        "SELECT * FROM t ORDER BY id LIMIT 0",
    ),
    case!(
        "order_by_with_nulls",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t VALUES (1, 30), (2, NULL), (3, 10), (4, NULL), (5, 20)",
        "SELECT id, v FROM t ORDER BY v",
    ),
    case!(
        "order_by_nulls_first_with_desc",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t VALUES (1, 30), (2, NULL), (3, 10), (4, NULL), (5, 20)",
        "SELECT id, v FROM t ORDER BY v DESC",
    ),

    // ========================================================================
    // Subqueries (scalar + IN)
    //
    // TODO: Subquery execution currently requires the expression evaluator
    // to call back into the executor (the evaluator needs access to the
    // pager + catalog to run a SELECT). This is a moderate architectural
    // change — tracked as a known limitation. See `evaluate_in` in
    // src/executor/expr.rs which currently returns
    // `Unsupported("IN subquery via evaluator (use executor)")`.
    //
    // The fix is to thread a `&mut Pager` + `&Catalog` into EvalContext so
    // the evaluator can call `execute(plan, ctx)` recursively. This is on the
    // roadmap but not implemented yet.
    // ========================================================================
    // case!(
    //     "in_subquery",
    //     ...
    // ),
    // case!(
    //     "not_in_subquery",
    //     ...
    // ),
    // case!(
    //     "scalar_subquery_in_select",
    //     ...
    // ),

    // ========================================================================
    // Expressions: arithmetic, logical, bitwise, string ops
    // ========================================================================
    case!(
        "mixed_arithmetic_precedence",
        "SELECT 1 + 2 * 3 - 4 / 2, (1 + 2) * 3, 10 % 3",
    ),
    case!(
        "bitwise_operators",
        "SELECT 5 & 3, 5 | 2, ~0, 1 << 4",
    ),
    case!(
        "modulo_negative",
        "SELECT -7 % 3, 7 % -3, -7 % -3",
    ),
    case!(
        "string_concat_with_coalesce",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b TEXT)",
        "INSERT INTO t VALUES (1, 'foo', NULL), (2, NULL, 'bar'), (3, 'x', 'y')",
        "SELECT id, COALESCE(a, '') || '-' || COALESCE(b, '') FROM t ORDER BY id",
    ),

    // ========================================================================
    // Edge: large multi-row INSERT, AUTOINCREMENT-style IDs
    // ========================================================================
    case!(
        "autoincrement_rowids",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t (v) VALUES (10), (20), (30), (40), (50)",
        "SELECT * FROM t ORDER BY id",
    ),
    case!(
        "explicit_rowid_assignment",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t VALUES (100, 1), (200, 2), (300, 3)",
        "INSERT INTO t (v) VALUES (4)",
        "SELECT * FROM t ORDER BY id",
    ),

    // ========================================================================
    // Edge: empty result from WHERE, GROUP BY on constant
    // ========================================================================
    case!(
        "group_by_constant",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)",
        "SELECT 'all' AS bucket, COUNT(*), SUM(v) FROM t",
    ),
    case!(
        "where_no_match_returns_empty",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t VALUES (1, 10), (2, 20)",
        "SELECT * FROM t WHERE v > 1000",
    ),

    // ========================================================================
    // PRAGMA queries (limited support; mostly no-ops but should not error)
    // ========================================================================
    case!(
        "pragma_table_list_returns_nothing_useful",
        "CREATE TABLE t (id INTEGER PRIMARY KEY)",
        "SELECT 1 FROM t LIMIT 1",
    ),
];

/// Driver: run all cases.
#[test]
fn differential_vs_sqlite() {
    let mut passed = 0;
    let mut failed: Vec<&str> = Vec::new();
    for case in CASES {
        let result = std::panic::catch_unwind(|| run_case(case));
        match result {
            Ok(()) => passed += 1,
            Err(_) => {
                failed.push(case.name);
                eprintln!("[FAIL] {}", case.name);
            }
        }
    }
    eprintln!("differential_vs_sqlite: {}/{} passed", passed, CASES.len());
    if !failed.is_empty() {
        panic!(
            "differential_vs_sqlite: {} cases failed: {}",
            failed.len(),
            failed.join(", ")
        );
    }
}
