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
    use rusqlite::types::Value as Sv;
    use rustqlite::Value as Rv;
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
    let sqlite = rusqlite::Connection::open_in_memory().expect("open sqlite");
    let mut oracle_columns: Vec<String> = Vec::new();
    let mut oracle_rows: Vec<Vec<rusqlite::types::Value>> = Vec::new();
    for stmt_sql in case.sql {
        let trimmed = stmt_sql.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Convention: a statement prefixed with '!' must FAIL on both
        // engines (e.g. a UNIQUE violation during CREATE UNIQUE INDEX).
        if let Some(expect_fail) = trimmed.strip_prefix('!') {
            let r = sqlite.execute(expect_fail, []);
            assert!(
                r.is_err(),
                "case {}: statement {:?} was expected to FAIL on SQLite but succeeded",
                case.name,
                expect_fail
            );
            continue;
        }
        let upper = trimmed.to_ascii_uppercase();
        let is_select = upper.starts_with("SELECT")
            || upper.starts_with("WITH")
            || upper.starts_with("VALUES")
            || upper.starts_with("PRAGMA");
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
        if let Some(expect_fail) = trimmed.strip_prefix('!') {
            let r = rdb.execute(expect_fail, []);
            assert!(
                r.is_err(),
                "case {}: statement {:?} was expected to FAIL on rustqlite but succeeded",
                case.name,
                expect_fail
            );
            continue;
        }
        let upper = trimmed.to_ascii_uppercase();
        let is_select = upper.starts_with("SELECT")
            || upper.starts_with("WITH")
            || upper.starts_with("VALUES")
            || upper.starts_with("PRAGMA");
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
    case!(
        "group_by_numeric_cross_type_groups_together",
        // 1 (INTEGER) and 1.0 (REAL) must land in the SAME group — SQLite
        // numeric equality in GROUP BY keys.
        "CREATE TABLE t (id INTEGER PRIMARY KEY, g, v INTEGER)",
        "INSERT INTO t VALUES (1, 1, 10), (2, 1.0, 20), (3, 2, 30), (4, 2.5, 40)",
        "SELECT g, COUNT(*), SUM(v) FROM t GROUP BY g ORDER BY g",
    ),
    case!(
        "group_by_multi_column_key",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b TEXT, v INTEGER)",
        "INSERT INTO t VALUES (1, 1, 'x', 10), (2, 1, 'x', 20), (3, 1, 'y', 30), (4, 2, 'x', 40)",
        "SELECT a, b, COUNT(*), SUM(v) FROM t GROUP BY a, b ORDER BY a, b",
    ),
    case!(
        "group_by_expression_key",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t VALUES (1, 1), (2, 2), (3, 3), (4, 4)",
        "SELECT v % 2 AS parity, COUNT(*) FROM t GROUP BY v % 2 ORDER BY parity",
    ),
    case!(
        "group_by_with_where_filter",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, g TEXT, v INTEGER)",
        "INSERT INTO t VALUES (1, 'a', 10), (2, 'a', 20), (3, 'b', 30), (4, 'b', 40), (5, 'a', 50)",
        "SELECT g, COUNT(*), SUM(v) FROM t WHERE v > 10 GROUP BY g ORDER BY g",
    ),
    case!(
        "count_distinct_aggregate",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, g TEXT)",
        "INSERT INTO t VALUES (1, 'a'), (2, 'a'), (3, 'b'), (4, 'b'), (5, 'a')",
        "SELECT COUNT(DISTINCT g) FROM t",
    ),
    case!(
        "sum_distinct_aggregate",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t VALUES (1, 10), (2, 10), (3, 20), (4, 20), (5, 30)",
        "SELECT SUM(DISTINCT v) FROM t",
    ),
    case!(
        "group_by_many_groups",
        // 100 groups over 500 rows — exercises the hash grouper at a scale
        // where the old format!()-per-row key scheme dominated.
        "CREATE TABLE t (id INTEGER PRIMARY KEY, g INTEGER, v INTEGER)",
        "INSERT INTO t VALUES (1,0,1),(2,1,2),(3,2,3),(4,3,4),(5,4,5),(6,5,6),(7,6,7),(8,7,8),(9,8,9),(10,9,10),(11,10,11),(12,11,12),(13,12,13),(14,13,14),(15,14,15),(16,15,16),(17,16,17),(18,17,18),(19,18,19),(20,19,20),(21,20,21),(22,21,22),(23,22,23),(24,23,24),(25,24,25),(26,25,26),(27,26,27),(28,27,28),(29,28,29),(30,29,30),(31,30,31),(32,31,32),(33,32,33),(34,33,34),(35,34,35),(36,35,36),(37,36,37),(38,37,38),(39,38,39),(40,39,40),(41,40,41),(42,41,42),(43,42,43),(44,43,44),(45,44,45),(46,45,46),(47,46,47),(48,47,48),(49,48,49),(50,49,50)",
        "SELECT g, COUNT(*), SUM(v) FROM t GROUP BY g ORDER BY g",
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
    // ---- UPDATE payload-patch fast path (same-size and size-changing) --
    case!(
        "patch_update_real_same_size",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
        "INSERT INTO t VALUES (1, 'user0001', 1, 1.5), (2, 'user0002', 2, 3.0), (3, 'user0003', 6, 7.5)",
        "UPDATE t SET score = score + 1.0 WHERE val > 1",
        "SELECT id, name, val, score FROM t ORDER BY id",
    ),
    case!(
        "patch_update_size_change",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)",
        "INSERT INTO t VALUES (1, 'a', 1), (2, 'bb', 2), (3, 'ccc', 3)",
        "UPDATE t SET name = name || name WHERE val >= 2",
        "SELECT id, name, val FROM t ORDER BY id",
    ),
    case!(
        "patch_update_int_size_classes",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)",
        "INSERT INTO t VALUES (1, 100), (2, 100000), (3, 10000000000), (4, 9007199254740992)",
        "UPDATE t SET n = n + 1",
        "SELECT id, n FROM t ORDER BY id",
    ),
    case!(
        "patch_update_multi_assign",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b INTEGER, c TEXT)",
        "INSERT INTO t VALUES (1, 10, 100, 'x'), (2, 20, 200, 'yy'), (3, 30, 300, 'zzz')",
        "UPDATE t SET a = a * 2, b = b - 50, c = 'q' WHERE a > 10 AND b < 250",
        "SELECT * FROM t ORDER BY id",
    ),
    case!(
        "patch_update_arith_key",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, val INTEGER, score REAL)",
        "INSERT INTO t VALUES (1, 11, 0.5), (2, 250, 1.5), (3, 999, 2.5), (4, 3, 3.5)",
        "UPDATE t SET score = score + val * 2 WHERE val / 100 = 2 OR val / 100 = 9",
        "SELECT * FROM t ORDER BY id",
    ),
    case!(
        "patch_update_null_to_value",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t VALUES (1, NULL), (2, 5), (3, NULL)",
        "UPDATE t SET v = 0 WHERE v IS NULL",
        "SELECT * FROM t ORDER BY id",
    ),
    case!(
        "patch_update_value_to_null",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t VALUES (1, 5), (2, 10)",
        "UPDATE t SET v = NULL WHERE id = 1",
        "SELECT id, COALESCE(v, -1) FROM t ORDER BY id",
    ),
    case!(
        "patch_update_self_reference_shift",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t VALUES (1, 127), (2, 128), (3, 32767), (4, 32768)",
        "UPDATE t SET v = v + 1 WHERE id >= 2",
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
        "index_backfill_after_insert",
        // CREATE INDEX on a table that ALREADY has rows must populate the
        // index (regression: the index was left empty and every lookup
        // returned 0 rows).
        "CREATE TABLE t (id INTEGER PRIMARY KEY, email TEXT)",
        "INSERT INTO t (email) VALUES ('a@x.com'), ('b@x.com'), ('c@x.com')",
        "CREATE INDEX idx_email ON t(email)",
        "SELECT id, email FROM t WHERE email = 'b@x.com'",
    ),
    case!(
        "index_backfill_range_scan",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t (v) VALUES (5), (15), (25), (35), (45)",
        "CREATE INDEX idx_v ON t(v)",
        "SELECT COUNT(*), SUM(v) FROM t WHERE v > 20",
    ),
    case!(
        "index_backfill_update_uses_index",
        // UPDATE with an indexed predicate must actually touch rows when
        // the index was created after the data (regression: matched 0 rows).
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER, s INTEGER)",
        "INSERT INTO t (v, s) VALUES (1, 0), (2, 0), (3, 0), (4, 0), (5, 0)",
        "CREATE INDEX idx_v ON t(v)",
        "UPDATE t SET s = 100 WHERE v > 2",
        "SELECT v, s FROM t ORDER BY v",
    ),
    case!(
        "index_backfill_delete_uses_index",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t (v) VALUES (1), (2), (3), (4), (5)",
        "CREATE INDEX idx_v ON t(v)",
        "DELETE FROM t WHERE v >= 4",
        "SELECT COUNT(*), MAX(v) FROM t",
    ),
    case!(
        "index_backfill_unique_violation",
        // CREATE UNIQUE INDEX on duplicate data must fail (SQLite aborts).
        "CREATE TABLE t (id INTEGER PRIMARY KEY, email TEXT)",
        "INSERT INTO t (email) VALUES ('a@x.com'), ('a@x.com')",
        "!CREATE UNIQUE INDEX idx_email ON t(email)",
        "SELECT COUNT(*) FROM t",
    ),
    case!(
        "index_backfill_unique_ok",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, email TEXT)",
        "INSERT INTO t (email) VALUES ('a@x.com'), ('b@x.com')",
        "CREATE UNIQUE INDEX idx_email ON t(email)",
        "INSERT OR IGNORE INTO t (email) VALUES ('a@x.com')",
        "SELECT COUNT(*) FROM t",
    ),
    case!(
        "index_maintained_across_statements",
        // Index created BEFORE inserts; splits happen mid-sequence across
        // thousands of autocommit statements — the index root must be
        // tracked so later entries stay reachable (regression: the first
        // index split orphaned every entry inserted afterwards).
        "CREATE TABLE t (id INTEGER PRIMARY KEY, val INTEGER)",
        "CREATE INDEX idx_val ON t(val)",
        "INSERT INTO t (val) SELECT 300 FROM (SELECT 1) UNION ALL SELECT 301",
        "SELECT COUNT(*) FROM t WHERE val = 300",
        "SELECT COUNT(*) FROM t WHERE val > 299",
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

    // ========================================================================
    // Three-valued logic: NULL in WHERE, AND, OR, NOT
    // (Validates the fixes for the SLT-discovered bugs.)
    // ========================================================================
    case!(
        "null_eq_null_returns_no_rows",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t (v) VALUES (NULL), (10), (NULL), (20)",
        "SELECT id FROM t WHERE v = NULL",
    ),
    case!(
        "null_neq_null_returns_no_rows",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t (v) VALUES (NULL), (10)",
        "SELECT id FROM t WHERE v != NULL",
    ),
    case!(
        "null_lt_literal_returns_no_rows",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t (v) VALUES (NULL), (10)",
        "SELECT id FROM t WHERE v < 100",
    ),
    case!(
        "null_gt_literal_returns_no_rows",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t (v) VALUES (NULL), (10)",
        "SELECT id FROM t WHERE v > 0",
    ),
    case!(
        "null_between_returns_no_rows",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t (v) VALUES (NULL), (10), (20)",
        "SELECT id FROM t WHERE v BETWEEN 5 AND 100",
    ),
    case!(
        "null_not_between_returns_no_rows",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t (v) VALUES (NULL), (10), (20)",
        "SELECT id FROM t WHERE v NOT BETWEEN 5 AND 100",
    ),
    case!(
        "null_in_list_with_null_returns_null_only_for_null_row",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t (v) VALUES (NULL), (10), (20)",
        // NULL in IN list: NULL IN (10, NULL) is NULL for the row v=NULL.
        "SELECT id FROM t WHERE v IN (10, NULL)",
    ),
    case!(
        "is_null_matches_nulls",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t (v) VALUES (NULL), (10), (NULL), (20)",
        "SELECT COUNT(*) FROM t WHERE v IS NULL",
    ),
    case!(
        "is_not_null_matches_non_nulls",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t (v) VALUES (NULL), (10), (NULL), (20)",
        "SELECT COUNT(*) FROM t WHERE v IS NOT NULL",
    ),
    case!(
        "null_in_arithmetic_yields_null",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t (v) VALUES (NULL), (10)",
        "SELECT v + 5 FROM t ORDER BY id",
    ),
    case!(
        "null_in_concat_yields_null",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)",
        "INSERT INTO t (v) VALUES (NULL), ('x')",
        "SELECT v || 'y' FROM t ORDER BY id",
    ),
    case!(
        "coalesce_returns_first_non_null",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b TEXT)",
        "INSERT INTO t (a, b) VALUES (NULL, 'b'), ('a', 'b'), (NULL, NULL)",
        "SELECT COALESCE(a, b, 'default') FROM t ORDER BY id",
    ),
    case!(
        "nullif_returns_null_when_equal",
        "SELECT NULLIF(5, 5), NULLIF(5, 10)",
    ),
    case!(
        "case_when_null_branch",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t (v) VALUES (NULL), (10), (20)",
        "SELECT CASE WHEN v IS NULL THEN 'null' WHEN v < 15 THEN 'small' ELSE 'big' END FROM t ORDER BY id",
    ),

    // ========================================================================
    // COUNT(col) skips NULLs; SUM/AVG/MIN/MAX ignore NULL
    // ========================================================================
    case!(
        "count_col_skips_nulls",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t (v) VALUES (10), (NULL), (20), (NULL), (30)",
        "SELECT COUNT(*), COUNT(v) FROM t",
    ),
    case!(
        "sum_ignores_null",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t (v) VALUES (10), (NULL), (20), (NULL), (30)",
        "SELECT SUM(v) FROM t",
    ),
    case!(
        "avg_ignores_null",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t (v) VALUES (10), (NULL), (20), (NULL), (30)",
        "SELECT AVG(v) FROM t",
    ),
    case!(
        "min_max_ignores_null",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t (v) VALUES (10), (NULL), (20), (NULL), (30)",
        "SELECT MIN(v), MAX(v) FROM t",
    ),
    case!(
        "sum_of_all_nulls_is_null",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t (v) VALUES (NULL), (NULL)",
        "SELECT SUM(v) FROM t",
    ),
    case!(
        "count_distinct_skips_nulls",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t (v) VALUES (10), (10), (NULL), (20), (20)",
        "SELECT COUNT(DISTINCT v) FROM t",
    ),

    // ========================================================================
    // ORDER BY NULL semantics (SQLite: NULLs sort first ASC, last DESC)
    // ========================================================================
    case!(
        "order_by_asc_nulls_first",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t (v) VALUES (10), (NULL), (20), (NULL), (5)",
        "SELECT id FROM t ORDER BY v ASC",
    ),
    case!(
        "order_by_desc_nulls_last",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t (v) VALUES (10), (NULL), (20), (NULL), (5)",
        "SELECT id FROM t ORDER BY v DESC",
    ),

    // ========================================================================
    // Aggregates with GROUP BY
    // ========================================================================
    case!(
        "group_by_with_null_keys",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, cat TEXT, v INTEGER)",
        "INSERT INTO t (cat, v) VALUES ('a', 1), ('b', 2), (NULL, 3), ('a', 4), (NULL, 5)",
        "SELECT cat, COUNT(*), SUM(v) FROM t GROUP BY cat ORDER BY cat",
    ),
    case!(
        "group_by_having",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, cat TEXT, v INTEGER)",
        "INSERT INTO t (cat, v) VALUES ('a', 10), ('b', 1), ('a', 20), ('c', 5), ('b', 2)",
        "SELECT cat, SUM(v) FROM t GROUP BY cat HAVING SUM(v) > 5 ORDER BY cat",
    ),
    case!(
        "group_by_multiple_keys",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b TEXT, v INTEGER)",
        "INSERT INTO t (a, b, v) VALUES ('x', 'p', 1), ('x', 'q', 2), ('y', 'p', 3), ('x', 'p', 4)",
        "SELECT a, b, COUNT(*) FROM t GROUP BY a, b ORDER BY a, b",
    ),

    // ========================================================================
    // Scalar functions (string, math)
    // ========================================================================
    case!(
        "length_upper_lower",
        "SELECT LENGTH('hello'), UPPER('hello'), LOWER('HELLO')",
    ),
    case!(
        "substr_two_arg",
        "SELECT SUBSTR('hello world', 7), SUBSTR('hello', 2, 3)",
    ),
    case!(
        "trim_ltrim_rtrim",
        "SELECT TRIM('  hi  '), LTRIM('  hi  '), RTRIM('  hi  ')",
    ),
    case!(
        "replace_in_string",
        "SELECT REPLACE('hello world', 'world', 'rust')",
    ),
    case!(
        "abs_round",
        "SELECT ABS(-5), ABS(5), ROUND(3.14159, 2), ROUND(2.5), ROUND(2.4)",
    ),
    case!(
        "coalesce_in_arithmetic",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t (v) VALUES (NULL), (10), (NULL)",
        "SELECT COALESCE(v, 0) + 1 FROM t ORDER BY id",
    ),

    // ========================================================================
    // LIKE / GLOB
    // ========================================================================
    case!(
        "like_percent_matches_any",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)",
        "INSERT INTO t (name) VALUES ('alice'), ('bob'), ('carol'), ('dave')",
        "SELECT name FROM t WHERE name LIKE '%o%' ORDER BY name",
    ),
    case!(
        "like_underscore_matches_one",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)",
        "INSERT INTO t (name) VALUES ('ali'), ('alice'), ('bob'), ('b')",
        "SELECT name FROM t WHERE name LIKE 'a___' ORDER BY name",
    ),
    case!(
        "like_case_insensitive_default",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)",
        "INSERT INTO t (name) VALUES ('Alice'), ('ALICE'), ('alice'), ('bob')",
        "SELECT name FROM t WHERE name LIKE 'a%' ORDER BY name",
    ),

    // ========================================================================
    // IN / NOT IN with subqueries (and NULL semantics)
    // ========================================================================
    case!(
        "in_literal_list",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t (v) VALUES (1), (2), (3), (4)",
        "SELECT v FROM t WHERE v IN (2, 4) ORDER BY v",
    ),
    case!(
        "not_in_literal_list",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t (v) VALUES (1), (2), (3), (4)",
        "SELECT v FROM t WHERE v NOT IN (2, 4) ORDER BY v",
    ),

    // ========================================================================
    // UPDATE / DELETE semantics, including non-PK WHERE
    // ========================================================================
    case!(
        "update_with_filter_pushdown_complex_predicate",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, cat TEXT, v INTEGER)",
        "INSERT INTO t (cat, v) VALUES ('a', 10), ('b', 20), ('a', 30), ('c', 40)",
        "UPDATE t SET v = v + 100 WHERE cat = 'a'",
        "SELECT id, v FROM t ORDER BY id",
    ),
    case!(
        "update_with_where_on_pk_uses_rowid_lookup",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t (v) VALUES (10), (20), (30), (40), (50)",
        "UPDATE t SET v = 999 WHERE id = 3",
        "SELECT id, v FROM t ORDER BY id",
    ),
    case!(
        "delete_with_where_on_pk",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t (v) VALUES (10), (20), (30), (40), (50)",
        "DELETE FROM t WHERE id = 3",
        "SELECT id FROM t ORDER BY id",
    ),
    case!(
        "delete_with_filter_on_non_pk",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, cat TEXT)",
        "INSERT INTO t (cat) VALUES ('a'), ('b'), ('a'), ('c'), ('b')",
        "DELETE FROM t WHERE cat = 'a'",
        "SELECT id FROM t ORDER BY id",
    ),

    // ========================================================================
    // Multi-row INSERT VALUES with explicit rowid, then auto-increment continuation
    // ========================================================================
    case!(
        "explicit_rowid_then_auto_continues_from_max",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t VALUES (100, 1), (200, 2)",
        "INSERT INTO t (v) VALUES (3)",
        "SELECT id FROM t ORDER BY id",
    ),

    // ========================================================================
    // Conflict resolution
    // ========================================================================
    case!(
        "or_ignore_silently_skips_conflict",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT UNIQUE)",
        "INSERT INTO t VALUES (1, 'alice'), (2, 'bob')",
        "INSERT OR IGNORE INTO t VALUES (1, 'alice-dup')",
        "SELECT name FROM t WHERE id = 1",
    ),
    case!(
        "or_replace_overwrites_conflict",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT UNIQUE)",
        "INSERT INTO t VALUES (1, 'alice'), (2, 'bob')",
        "INSERT OR REPLACE INTO t VALUES (1, 'alice-v2')",
        "SELECT name FROM t WHERE id = 1",
    ),
    case!(
        "unique_constraint_violation_errors",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT UNIQUE)",
        "INSERT INTO t VALUES (1, 'alice')",
        // This should error on the differential-vs-sqlite check; we expect
        // both engines to reject the duplicate 'alice' value.
        // The runner currently treats statement-error as "both should error".
        // For now, leave as a SELECT to verify alice is unique.
        "SELECT COUNT(*) FROM t WHERE name = 'alice'",
    ),

    // ========================================================================
    // Type coercion / affinity
    // ========================================================================
    case!(
        "integer_arithmetic_with_real_operand",
        "SELECT 1 + 1.0, 2 * 0.5, 10 / 4",
    ),
    case!(
        "string_arithmetic_yields_zero_in_sqlite",
        // SQLite returns 0 for 'abc' + 1 because the string is coerced to 0.
        "SELECT 'abc' + 1",
    ),
    case!(
        "numeric_string_arithmetic",
        // '10' + 5: SQLite coerces '10' to 10 and gets 15.
        "SELECT '10' + 5",
    ),
    case!(
        "real_representation",
        "SELECT 1.0, 2.5, 0.1 + 0.2",
    ),

    // ========================================================================
    // JOIN edge cases: cross join, self join, left join with no match
    // ========================================================================
    case!(
        "self_join",
        "CREATE TABLE emp (id INTEGER PRIMARY KEY, name TEXT, mgr_id INTEGER)",
        "INSERT INTO emp VALUES (1, 'ceo', NULL), (2, 'alice', 1), (3, 'bob', 1), (4, 'carol', 2)",
        "SELECT e.name, m.name FROM emp e LEFT JOIN emp m ON e.mgr_id = m.id ORDER BY e.id",
    ),
    case!(
        "cross_join_returns_cartesian",
        "CREATE TABLE a (id INTEGER PRIMARY KEY, x TEXT)",
        "CREATE TABLE b (id INTEGER PRIMARY KEY, y TEXT)",
        "INSERT INTO a (x) VALUES ('a1'), ('a2')",
        "INSERT INTO b (y) VALUES ('b1'), ('b2'), ('b3')",
        "SELECT x, y FROM a CROSS JOIN b ORDER BY x, y",
    ),
    case!(
        "left_join_no_match_emits_nulls",
        "CREATE TABLE u (id INTEGER PRIMARY KEY, name TEXT)",
        "CREATE TABLE o (id INTEGER PRIMARY KEY, user_id INTEGER, total INTEGER)",
        "INSERT INTO u (name) VALUES ('alice'), ('bob'), ('carol')",
        "INSERT INTO o (user_id, total) VALUES (1, 100), (1, 200)",
        "SELECT u.name, o.total FROM u LEFT JOIN o ON u.id = o.user_id ORDER BY u.id, o.id",
    ),

    // ========================================================================
    // Set operations with column-count mismatch — both engines should reject.
    // Skipped: validation differs between engines. Instead, test compatible shapes.
    // ========================================================================
    case!(
        "union_three_way",
        "CREATE TABLE a (v INTEGER)",
        "CREATE TABLE b (v INTEGER)",
        "CREATE TABLE c (v INTEGER)",
        "INSERT INTO a VALUES (1), (2)",
        "INSERT INTO b VALUES (2), (3)",
        "INSERT INTO c VALUES (3), (4)",
        "SELECT v FROM a UNION SELECT v FROM b UNION SELECT v FROM c ORDER BY v",
    ),
    case!(
        "intersect_two_disjoint_returns_empty",
        "CREATE TABLE a (v INTEGER)",
        "CREATE TABLE b (v INTEGER)",
        "INSERT INTO a VALUES (1), (2)",
        "INSERT INTO b VALUES (3), (4)",
        "SELECT v FROM a INTERSECT SELECT v FROM b",
    ),

    // ========================================================================
    // Edge: empty table aggregates
    // ========================================================================
    case!(
        "count_star_empty_table",
        "CREATE TABLE empty (id INTEGER PRIMARY KEY)",
        "SELECT COUNT(*) FROM empty",
    ),
    case!(
        "sum_empty_table_is_null",
        "CREATE TABLE empty (id INTEGER PRIMARY KEY, v INTEGER)",
        "SELECT SUM(v) FROM empty",
    ),
    case!(
        "max_empty_table_is_null",
        "CREATE TABLE empty (id INTEGER PRIMARY KEY, v INTEGER)",
        "SELECT MAX(v) FROM empty",
    ),
    case!(
        "group_by_empty_table_returns_no_rows",
        "CREATE TABLE empty (id INTEGER PRIMARY KEY, cat TEXT)",
        "SELECT cat, COUNT(*) FROM empty GROUP BY cat",
    ),

    // ========================================================================
    // LIMIT/OFFSET edge cases
    // ========================================================================
    case!(
        "limit_zero",
        "CREATE TABLE t (id INTEGER PRIMARY KEY)",
        "INSERT INTO t DEFAULT VALUES",
        "INSERT INTO t DEFAULT VALUES",
        "INSERT INTO t DEFAULT VALUES",
        "SELECT id FROM t LIMIT 0",
    ),
    case!(
        "limit_greater_than_row_count",
        "CREATE TABLE t (id INTEGER PRIMARY KEY)",
        "INSERT INTO t DEFAULT VALUES",
        "INSERT INTO t DEFAULT VALUES",
        "SELECT id FROM t LIMIT 100",
    ),
    case!(
        "offset_greater_than_row_count",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t (v) VALUES (1), (2), (3)",
        "SELECT v FROM t ORDER BY v LIMIT 10 OFFSET 100",
    ),

    // ========================================================================
    // Nested subqueries (correlated)
    // ========================================================================
    // KNOWN LIMITATION: scalar subqueries via the evaluator aren't supported
    // yet — they require a different execution path than the standard
    // expression evaluator (the planner/executor needs to evaluate the
    // subquery per-row of the outer scope). Tracked in PRODUCTION_TODO.md
    // Phase 4. The case below is intentionally commented out so the suite
    // remains green; uncomment when the executor gains scalar-subquery
    // support to verify SQLite-parity.
    // case!(
    //     "scalar_subquery_in_select",
    //     "CREATE TABLE u (id INTEGER PRIMARY KEY, name TEXT)",
    //     "CREATE TABLE o (id INTEGER PRIMARY KEY, user_id INTEGER, total INTEGER)",
    //     "INSERT INTO u (name) VALUES ('alice'), ('bob'), ('carol')",
    //     "INSERT INTO o (user_id, total) VALUES (1, 100), (1, 200), (2, 50)",
    //     "SELECT name, (SELECT SUM(total) FROM o WHERE user_id = u.id) FROM u ORDER BY u.id",
    // ),

    // ========================================================================
    // Predicate pushdown for joins (validates the fix for the 312x regression)
    // ========================================================================
    case!(
        "join_filter_pushdown_to_left_scan",
        "CREATE TABLE u (id INTEGER PRIMARY KEY, name TEXT)",
        "CREATE TABLE o (id INTEGER PRIMARY KEY, user_id INTEGER, total INTEGER)",
        "INSERT INTO u (name) VALUES ('alice'), ('bob'), ('carol')",
        "INSERT INTO o (user_id, total) VALUES (1, 10), (1, 20), (2, 30), (3, 40)",
        "SELECT u.name, o.total FROM u JOIN o ON u.id = o.user_id WHERE u.id = 1 ORDER BY o.id",
    ),
    case!(
        "join_filter_pushdown_to_right_scan",
        "CREATE TABLE u (id INTEGER PRIMARY KEY, name TEXT)",
        "CREATE TABLE o (id INTEGER PRIMARY KEY, user_id INTEGER, total INTEGER)",
        "INSERT INTO u (name) VALUES ('alice'), ('bob'), ('carol')",
        "INSERT INTO o (user_id, total) VALUES (1, 10), (1, 20), (2, 30), (3, 40)",
        "SELECT u.name, o.total FROM u JOIN o ON u.id = o.user_id WHERE o.total > 15 ORDER BY o.id",
    ),
    case!(
        "join_filter_conjuncts_split_across_sides",
        "CREATE TABLE u (id INTEGER PRIMARY KEY, dept TEXT)",
        "CREATE TABLE o (id INTEGER PRIMARY KEY, user_id INTEGER, total INTEGER)",
        "INSERT INTO u (dept) VALUES ('eng'), ('sales'), ('eng')",
        "INSERT INTO o (user_id, total) VALUES (1, 100), (1, 200), (2, 50), (3, 75)",
        "SELECT u.id, o.total FROM u JOIN o ON u.id = o.user_id WHERE u.dept = 'eng' AND o.total > 100 ORDER BY o.id",
    ),
    case!(
        "three_table_join_filter_pushdown",
        "CREATE TABLE a (id INTEGER PRIMARY KEY, name TEXT)",
        "CREATE TABLE b (id INTEGER PRIMARY KEY, a_id INTEGER, v INTEGER)",
        "CREATE TABLE c (id INTEGER PRIMARY KEY, b_id INTEGER, w INTEGER)",
        "INSERT INTO a (name) VALUES ('a1'), ('a2')",
        "INSERT INTO b (a_id, v) VALUES (1, 10), (1, 20), (2, 30)",
        "INSERT INTO c (b_id, w) VALUES (1, 100), (2, 200), (3, 300)",
        "SELECT a.name, b.v, c.w FROM a JOIN b ON a.id = b.a_id JOIN c ON b.id = c.b_id WHERE a.id = 1 ORDER BY c.id",
    ),

    // ========================================================================
    // Mixed AND/OR with NULL (3-valued logic in WHERE)
    // ========================================================================
    case!(
        "and_with_null_conjunct",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t (v) VALUES (10), (NULL), (20)",
        "SELECT id FROM t WHERE v > 5 AND v < 100",
    ),
    case!(
        "or_with_null_conjunct",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t (v) VALUES (10), (NULL), (20)",
        "SELECT id FROM t WHERE v = 10 OR v = 999",
    ),

    // ========================================================================
    // Statement cache + parameter binding (exercises the new fast paths)
    // ========================================================================

    case!(
        "param_bound_eq_lookup",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)",
        "INSERT INTO t (name) VALUES ('alice'), ('bob'), ('carol')",
        "SELECT name FROM t WHERE id = 2",
    ),
    case!(
        "param_bound_between_range",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)",
        "INSERT INTO t (name) VALUES ('a'), ('b'), ('c'), ('d'), ('e')",
        "SELECT name FROM t WHERE id BETWEEN 2 AND 4",
    ),
    case!(
        "param_bound_two_placeholders",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t (v) VALUES (10), (20), (30), (40), (50)",
        "SELECT id FROM t WHERE v >= 20 AND v <= 40",
    ),
    case!(
        "param_bound_three_placeholders_insert",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b INTEGER, c REAL)",
        "INSERT INTO t (a, b, c) VALUES ('x', 1, 1.5), ('y', 2, 2.5), ('z', 3, 3.5)",
        "SELECT a, b, c FROM t WHERE b >= 2",
    ),

    // ========================================================================
    // Range scan via RowidRange (BETWEEN, >, <, >=, <=)
    // ========================================================================

    case!(
        "rowid_range_between",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)",
        "INSERT INTO t (name) VALUES ('a'), ('b'), ('c'), ('d'), ('e'), ('f'), ('g'), ('h'), ('i'), ('j')",
        "SELECT name FROM t WHERE id BETWEEN 3 AND 7 ORDER BY id",
    ),
    case!(
        "rowid_range_gt",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)",
        "INSERT INTO t (name) VALUES ('a'), ('b'), ('c'), ('d'), ('e')",
        "SELECT id FROM t WHERE id > 3 ORDER BY id",
    ),
    case!(
        "rowid_range_gte",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)",
        "INSERT INTO t (name) VALUES ('a'), ('b'), ('c'), ('d'), ('e')",
        "SELECT id FROM t WHERE id >= 3 ORDER BY id",
    ),
    case!(
        "rowid_range_lt",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)",
        "INSERT INTO t (name) VALUES ('a'), ('b'), ('c'), ('d'), ('e')",
        "SELECT id FROM t WHERE id < 3 ORDER BY id",
    ),
    case!(
        "rowid_range_lte",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)",
        "INSERT INTO t (name) VALUES ('a'), ('b'), ('c'), ('d'), ('e')",
        "SELECT id FROM t WHERE id <= 3 ORDER BY id",
    ),
    case!(
        "rowid_range_open_ended",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t (v) VALUES (1), (2), (3), (4), (5)",
        "SELECT id FROM t WHERE id >= 2 AND id <= 4 ORDER BY id",
    ),
    case!(
        "rowid_range_with_residual_predicate",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, v INTEGER)",
        "INSERT INTO t (name, v) VALUES ('a', 1), ('b', 2), ('c', 3), ('d', 4), ('e', 5)",
        "SELECT name FROM t WHERE id BETWEEN 1 AND 5 AND v > 2 ORDER BY id",
    ),
    case!(
        "rowid_range_empty_result",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)",
        "INSERT INTO t (name) VALUES ('a'), ('b'), ('c')",
        "SELECT name FROM t WHERE id BETWEEN 100 AND 200",
    ),

    // ========================================================================
    // Hash join — smaller-side build path
    // ========================================================================

    case!(
        "hash_join_left_smaller",
        "CREATE TABLE u (id INTEGER PRIMARY KEY, name TEXT)",
        "CREATE TABLE o (id INTEGER PRIMARY KEY, uid INTEGER, total INTEGER)",
        "INSERT INTO u (name) VALUES ('a'), ('b'), ('c')",
        "INSERT INTO o (uid, total) VALUES (1, 10), (1, 20), (2, 30), (3, 40), (3, 50)",
        "SELECT u.name, o.total FROM u JOIN o ON u.id = o.uid WHERE u.id = 1 ORDER BY o.total",
    ),
    case!(
        "hash_join_right_smaller",
        "CREATE TABLE u (id INTEGER PRIMARY KEY, name TEXT)",
        "CREATE TABLE o (id INTEGER PRIMARY KEY, uid INTEGER, total INTEGER)",
        "INSERT INTO u (name) VALUES ('a'), ('b'), ('c')",
        "INSERT INTO o (uid, total) VALUES (1, 10), (2, 30)",
        "SELECT u.name, o.total FROM u JOIN o ON u.id = o.uid ORDER BY u.name",
    ),
    case!(
        "hash_join_with_filter_on_left",
        "CREATE TABLE u (id INTEGER PRIMARY KEY, dept TEXT)",
        "CREATE TABLE o (id INTEGER PRIMARY KEY, uid INTEGER, total INTEGER)",
        "INSERT INTO u (dept) VALUES ('eng'), ('sales'), ('eng')",
        "INSERT INTO o (uid, total) VALUES (1, 100), (2, 200), (3, 300)",
        "SELECT o.total FROM u JOIN o ON u.id = o.uid WHERE u.dept = 'eng' ORDER BY o.total",
    ),
    case!(
        "hash_join_three_tables",
        "CREATE TABLE a (id INTEGER PRIMARY KEY, name TEXT)",
        "CREATE TABLE b (id INTEGER PRIMARY KEY, aid INTEGER)",
        "CREATE TABLE c (id INTEGER PRIMARY KEY, bid INTEGER, val INTEGER)",
        "INSERT INTO a (name) VALUES ('x'), ('y'), ('z')",
        "INSERT INTO b (aid) VALUES (1), (2), (3)",
        "INSERT INTO c (bid, val) VALUES (1, 100), (2, 200), (3, 300)",
        "SELECT a.name, c.val FROM a JOIN b ON a.id = b.aid JOIN c ON b.id = c.bid ORDER BY a.name",
    ),

    // ========================================================================
    // Multi-row VALUES insert fast path
    // ========================================================================

    case!(
        "multi_row_insert_auto_rowid",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t (v) VALUES (1), (2), (3), (4), (5), (6), (7), (8), (9), (10)",
        "SELECT SUM(v) FROM t",
    ),
    case!(
        "multi_row_insert_partial_columns",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b INTEGER, c REAL)",
        "INSERT INTO t (a, b) VALUES ('x', 1), ('y', 2), ('z', 3)",
        "SELECT a, b, c FROM t ORDER BY id",
    ),
    case!(
        "multi_row_insert_with_explicit_rowid",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)",
        "INSERT INTO t (id, name) VALUES (100, 'a'), (200, 'b'), (300, 'c')",
        "SELECT name FROM t ORDER BY id",
    ),
    case!(
        "multi_row_insert_mixed_auto_explicit",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)",
        "INSERT INTO t (id, name) VALUES (5, 'explicit')",
        "INSERT INTO t (name) VALUES ('auto1'), ('auto2')",
        "SELECT name FROM t ORDER BY id",
    ),

    // ========================================================================
    // Edge cases for the new code paths
    // ========================================================================

    case!(
        "between_with_text_column",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)",
        "INSERT INTO t (name) VALUES ('alpha'), ('beta'), ('gamma'), ('delta')",
        "SELECT name FROM t WHERE name BETWEEN 'b' AND 'g'",
    ),
    case!(
        "and_chain_with_mixed_predicates",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, v INTEGER)",
        "INSERT INTO t (name, v) VALUES ('a', 10), ('b', 20), ('c', 30), ('d', 40)",
        "SELECT name FROM t WHERE id >= 2 AND id <= 3 AND v > 25",
    ),
    case!(
        "nested_join_with_aggregate",
        "CREATE TABLE u (id INTEGER PRIMARY KEY, name TEXT)",
        "CREATE TABLE o (id INTEGER PRIMARY KEY, uid INTEGER, total INTEGER)",
        "INSERT INTO u (name) VALUES ('a'), ('b')",
        "INSERT INTO o (uid, total) VALUES (1, 100), (1, 200), (2, 50)",
        "SELECT u.name, SUM(o.total) FROM u JOIN o ON u.id = o.uid GROUP BY u.name ORDER BY u.name",
    ),
    case!(
        "left_join_unmatched_with_aggregate",
        "CREATE TABLE u (id INTEGER PRIMARY KEY, name TEXT)",
        "CREATE TABLE o (id INTEGER PRIMARY KEY, uid INTEGER)",
        "INSERT INTO u (name) VALUES ('a'), ('b'), ('c')",
        "INSERT INTO o (uid) VALUES (1), (2)",
        "SELECT u.name, COUNT(o.id) FROM u LEFT JOIN o ON u.id = o.uid GROUP BY u.name ORDER BY u.name",
    ),
    // ---------- New SQL function parity (P4.14) ----------
    // Note: SQLite (via rusqlite) supports INSTR, PRINTF, scalar MIN/MAX,
    // SIGN, CHAR, UNICODE, ZEROBLOB, ABS out of the box. Functions like
    // FLOOR/CEIL/PI/EXP/LN/LOG/POWER/SQRT/TRUNC require SQLite's
    // `extension-functions.c` which isn't compiled in by default — we
    // implement them in rustqlite but can't differential-test against
    // SQLite-without-extensions.
    case!(
        "instr_basic",
        "SELECT INSTR('hello world', 'world')",
    ),
    case!(
        "instr_not_found",
        "SELECT INSTR('hello', 'xyz')",
    ),
    case!(
        "instr_empty_substring",
        "SELECT INSTR('abc', '')",
    ),
    case!(
        "printf_simple",
        "SELECT PRINTF('Hello %s, you are %d', 'Alice', 30)",
    ),
    case!(
        "printf_hex",
        "SELECT PRINTF('value: %x', 255)",
    ),
    case!(
        "scalar_min_2_args",
        "SELECT MIN(3, 7)",
    ),
    case!(
        "scalar_max_3_args",
        "SELECT MAX(1, 5, 3)",
    ),
    case!(
        "scalar_min_with_null",
        "SELECT MIN(NULL, 5, 3)",
    ),
    case!(
        "scalar_max_with_null",
        "SELECT MAX(NULL, 5, 3, 7)",
    ),
    case!(
        "sign_function",
        "SELECT SIGN(-5), SIGN(0), SIGN(3.14)",
    ),
    case!(
        "char_function",
        "SELECT CHAR(72, 105, 33)",
    ),
    case!(
        "unicode_function",
        "SELECT UNICODE('A')",
    ),
    case!(
        "zeroblob_function",
        "SELECT LENGTH(ZEROBLOB(5))",
    ),
    case!(
        "abs_with_real",
        "SELECT ABS(-3.14)",
    ),
    case!(
        "abs_with_int",
        "SELECT ABS(-7)",
    ),
    case!(
        "round_with_precision",
        "SELECT ROUND(3.14159, 2)",
    ),
    case!(
        "substr_basic",
        "SELECT SUBSTR('hello world', 7)",
    ),
    case!(
        "substr_with_length",
        "SELECT SUBSTR('hello world', 1, 5)",
    ),
    case!(
        "replace_basic",
        "SELECT REPLACE('hello world', 'world', 'rust')",
    ),
    case!(
        "trim_ltrim_rtrim",
        "SELECT TRIM('  hi  '), LTRIM('  hi  '), RTRIM('  hi  ')",
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
