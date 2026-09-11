//! Non-equi (nested-loop) joins — compiled condition evaluation and the
//! parallel LEFT-side split, differential against real SQLite.
//!
//! The generic nested loop evaluated its ON condition through `eval_row`
//! per (left, right) pair — a full AST walk + name resolution per pair,
//! plus a combined-row Vec allocation PER PAIR even when the condition
//! rejected it. The compiled path (`JTerm`) splits the column space at
//! `n_left` (no combined row, borrowed-operand comparisons, zero cost
//! for rejected pairs), threads COLLATE collations resolved once on the
//! main thread, and — above the min-rows threshold — splits the LEFT
//! (outer) side across workers: each worker runs the identical nested
//! loop over the shared read-only right rows; concatenation in range
//! order is bit-identical to the serial left-driven order; RIGHT/FULL
//! tails merge through per-worker bitmaps.
//!
//! Also pinned here: the hash join's no-equi-keys fallback now runs the
//! nested loop over its ALREADY-materialized sides (the old call
//! re-executed both sides), and `Filter` over a condition-less
//! INNER/CROSS join (`FROM a, b WHERE a.y < b.y`) evaluates the
//! predicate as the join condition — same row set, same order, no
//! cross-product materialization.

use rusqlite::Connection;

fn engine_rows(db: &rustqlite::Database, sql: &str) -> Vec<Vec<rustqlite::Value>> {
    db.query(sql, vec![]).unwrap()
}

fn sqlite_rows(rc: &Connection, sql: &str) -> Vec<Vec<rustqlite::Value>> {
    let mut stmt = rc.prepare(sql).unwrap();
    let ncols = stmt.column_count();
    let rows: Vec<Vec<rusqlite::types::Value>> = stmt
        .query_map([], |r| {
            Ok((0..ncols)
                .map(|i| r.get::<_, rusqlite::types::Value>(i).unwrap())
                .collect())
        })
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    rows.into_iter()
        .map(|row| {
            row.into_iter()
                .map(|v| match v {
                    rusqlite::types::Value::Null => rustqlite::Value::Null,
                    rusqlite::types::Value::Integer(i) => rustqlite::Value::Integer(i),
                    rusqlite::types::Value::Real(f) => rustqlite::Value::Real(f),
                    rusqlite::types::Value::Text(s) => rustqlite::Value::Text(s.into()),
                    rusqlite::types::Value::Blob(b) => rustqlite::Value::Blob(b),
                })
                .collect()
        })
        .collect()
}

fn differential(setup: &[&str], query: &str) {
    let mut db = rustqlite::Database::open(":memory:").unwrap();
    let rc = Connection::open_in_memory().unwrap();
    for sql in setup {
        // Expression-only queries (no setup statements) still run.
        if sql.trim_start().to_ascii_uppercase().starts_with("SELECT") {
            continue;
        }
        db.execute(sql, vec![]).unwrap();
        rc.execute_batch(sql).unwrap();
    }
    let mine = engine_rows(&db, query);
    let theirs = sqlite_rows(&rc, query);
    assert_eq!(
        format!("{:?}", mine),
        format!("{:?}", theirs),
        "engine/SQLite disagree on: {query}\n  engine: {:?}\n  sqlite: {:?}",
        mine,
        theirs
    );
}

const SETUP: &[&str] = &[
    "CREATE TABLE a (x TEXT, y INT)",
    "CREATE TABLE b (y INT, z TEXT)",
    "INSERT INTO a VALUES ('Alpha',1),('beta',2),('Gamma',3),('delta',NULL)",
    "INSERT INTO b VALUES (1,'x'),(2,'y'),(4,'w'),(NULL,'n')",
];

/// Every join flavor × non-equi condition shapes, row-for-row against
/// SQLite.
#[test]
fn non_equi_conditions_match_sqlite() {
    for q in [
        "SELECT a.x, b.z FROM a JOIN b ON a.y < b.y ORDER BY 1,2",
        "SELECT a.x, b.z FROM a JOIN b ON a.y > b.y ORDER BY 1,2",
        "SELECT a.x, b.z FROM a JOIN b ON a.y <= b.y ORDER BY 1,2",
        "SELECT a.x, b.z FROM a JOIN b ON a.y >= b.y ORDER BY 1,2",
        "SELECT a.x, b.z FROM a JOIN b ON a.y <> b.y ORDER BY 1,2",
        // arithmetic on both sides
        "SELECT a.x, b.z FROM a JOIN b ON a.y + 1 <= b.y ORDER BY 1,2",
        "SELECT a.x, b.z FROM a JOIN b ON a.y * 2 < b.y % 3 ORDER BY 1,2",
        "SELECT a.x, b.z FROM a JOIN b ON -a.y < b.y - 4 ORDER BY 1,2",
        // text comparisons + collations
        "SELECT a.x, b.z FROM a JOIN b ON a.x < b.z ORDER BY 1,2",
        "SELECT a.x, b.z FROM a JOIN b ON a.x < b.z COLLATE NOCASE ORDER BY 1,2",
        "SELECT a.x, b.z FROM a JOIN b ON a.x COLLATE NOCASE < b.z ORDER BY 1,2",
        "SELECT a.x, b.z FROM a JOIN b ON a.x < b.z COLLATE BINARY ORDER BY 1,2",
        // boolean trees
        "SELECT a.x, b.z FROM a JOIN b ON a.y < b.y AND a.x < b.z ORDER BY 1,2",
        "SELECT a.x, b.z FROM a JOIN b ON a.y < b.y OR a.x > b.z ORDER BY 1,2",
        "SELECT a.x, b.z FROM a JOIN b ON NOT (a.y >= b.y) ORDER BY 1,2",
        "SELECT a.x, b.z FROM a JOIN b ON (a.y < b.y AND a.x < b.z) OR b.y = 4 ORDER BY 1,2",
        "SELECT count(*) FROM a JOIN b ON a.y < b.y AND a.x < b.z OR b.y > 2",
        // NULL keys on either side (comparison never true)
        "SELECT count(*) FROM a JOIN b ON a.y < b.y",
        // implicit join (Filter over CROSS fusion)
        "SELECT a.x, b.z FROM a, b WHERE a.y < b.y ORDER BY 1,2",
        "SELECT a.x, b.z FROM a, b WHERE a.y + 1 = b.y ORDER BY 1,2",
        "SELECT count(*) FROM a, b WHERE a.x < b.z COLLATE NOCASE",
        // parameter in the condition
        "SELECT count(*) FROM a JOIN b ON a.y < 2",
        // literals as both operands
        "SELECT a.x FROM a JOIN b ON 1 < 2 ORDER BY 1",
    ] {
        differential(SETUP, q);
    }
}

/// OUTER joins: LEFT NULL-extension inline, RIGHT/FULL tails, all under
/// non-equi conditions.
#[test]
fn non_equi_outer_joins_match_sqlite() {
    for q in [
        "SELECT a.x, b.z FROM a LEFT JOIN b ON a.y < b.y ORDER BY 1,2",
        "SELECT count(*) FROM a LEFT JOIN b ON a.y < b.y",
        "SELECT a.x, b.z FROM a RIGHT JOIN b ON a.y < b.y ORDER BY 1,2",
        "SELECT count(*) FROM a RIGHT JOIN b ON a.y < b.y",
        "SELECT a.x, b.z FROM a FULL JOIN b ON a.y < b.y ORDER BY 1,2",
        "SELECT count(*) FROM a FULL JOIN b ON a.y < b.y",
        "SELECT a.x, b.z FROM a FULL JOIN b ON a.y < b.y AND a.x < b.z ORDER BY 1,2",
        "SELECT a.x, b.z FROM a LEFT JOIN b ON a.x < b.z COLLATE NOCASE ORDER BY 1,2",
        // every row participates (aggregate over the join output)
        "SELECT count(*), count(b.z) FROM a LEFT JOIN b ON a.y < b.y",
    ] {
        differential(SETUP, q);
    }
}

/// The parallel split: parallel == serial, bit for bit, above the
/// threshold — for every join flavor and a projection above the join.
#[test]
fn parallel_nested_join_matches_serial() {
    let mut db = rustqlite::Database::open(":memory:").unwrap();
    db.execute("CREATE TABLE l (y INT, tag TEXT)", vec![])
        .unwrap();
    db.execute("CREATE TABLE r (y INT, z TEXT)", vec![])
        .unwrap();
    let mut batch = String::from("INSERT INTO l VALUES ");
    for i in 0..150_000i64 {
        if i > 0 {
            batch.push(',');
        }
        batch.push_str(&format!("({}, 't{}')", i % 997, i % 97));
    }
    db.execute(&batch, vec![]).unwrap();
    let mut batch = String::from("INSERT INTO r VALUES ");
    for i in 0..500i64 {
        if i > 0 {
            batch.push(',');
        }
        batch.push_str(&format!("({}, 'z{}')", i, i % 53));
    }
    db.execute(&batch, vec![]).unwrap();

    for q in [
        // INNER, filtered (small output, full 75M-pair sweep)
        "SELECT count(*) FROM l JOIN r ON l.y < r.y AND r.y < 3",
        // INNER projection + ORDER BY prefix
        "SELECT l.y, r.y FROM l JOIN r ON l.y < r.y AND r.y < 3 ORDER BY 1, 2 LIMIT 500",
        // LEFT with a mix of matching / non-matching left rows
        "SELECT count(*) FROM l LEFT JOIN r ON l.y < r.y AND r.y < 3",
        "SELECT l.y, r.y FROM l LEFT JOIN r ON l.y < r.y AND r.y < 3 ORDER BY 1, 2 LIMIT 500",
        // RIGHT: bitmap-merged tail
        "SELECT count(*) FROM l RIGHT JOIN r ON l.y < r.y AND r.y < 3",
        // FULL: inline + tail
        "SELECT count(*) FROM l FULL JOIN r ON l.y < r.y AND r.y < 3",
        // implicit-join form (Filter over CROSS -> same compiled path)
        "SELECT count(*) FROM l, r WHERE l.y < r.y AND r.y < 3",
        // boolean trees + arithmetic
        "SELECT count(*) FROM l JOIN r ON l.y * 2 < r.y OR r.y % 97 = l.y % 97",
    ] {
        db.execute("PRAGMA parallel_scan = 0", vec![]).unwrap();
        let serial = db.query(q, vec![]).unwrap();
        db.execute("PRAGMA parallel_scan = 1", vec![]).unwrap();
        let parallel = db.query(q, vec![]).unwrap();
        assert_eq!(
            format!("{:?}", serial),
            format!("{:?}", parallel),
            "parallel != serial on: {q}\n  serial:   {:?}\n  parallel: {:?}",
            serial,
            parallel
        );
    }
}

/// Gate contracts: an open transaction declines to the serial path
/// (same answer), and the PRAGMA threshold honors 0 = disabled.
#[test]
fn nested_join_parallel_gates() {
    let mut db = rustqlite::Database::open(":memory:").unwrap();
    db.execute("CREATE TABLE l (y INT)", vec![]).unwrap();
    db.execute("CREATE TABLE r (y INT)", vec![]).unwrap();
    let mut batch = String::from("INSERT INTO l VALUES ");
    for i in 0..140_000i64 {
        if i > 0 {
            batch.push(',');
        }
        batch.push_str(&format!("({})", i % 401));
    }
    db.execute(&batch, vec![]).unwrap();
    db.execute("INSERT INTO r VALUES (0),(1),(2),(3)", vec![])
        .unwrap();
    let q = "SELECT count(*) FROM l JOIN r ON l.y < r.y";
    let direct = db.query(q, vec![]).unwrap();
    db.execute("BEGIN", vec![]).unwrap();
    let in_txn = db.query(q, vec![]).unwrap();
    db.execute("COMMIT", vec![]).unwrap();
    assert_eq!(direct, in_txn, "open transaction must decline (same rows)");
    db.execute("PRAGMA parallel_scan = 0", vec![]).unwrap();
    let off = db.query(q, vec![]).unwrap();
    assert_eq!(direct, off, "parallel_scan=0 must be bit-identical");
}

/// Cross-check the big shape against real SQLite (counts).
#[test]
fn non_equi_counts_match_sqlite_at_scale() {
    let mut db = rustqlite::Database::open(":memory:").unwrap();
    let rc = Connection::open_in_memory().unwrap();
    for conn_sql in [
        "CREATE TABLE l (y INT, tag TEXT)",
        "CREATE TABLE r (y INT, z TEXT)",
    ] {
        db.execute(conn_sql, vec![]).unwrap();
        rc.execute_batch(conn_sql).unwrap();
    }
    let mut batch = String::from("INSERT INTO l VALUES ");
    for i in 0..20_000i64 {
        if i > 0 {
            batch.push(',');
        }
        batch.push_str(&format!("({}, 't{}')", i % 197, i % 31));
    }
    db.execute(&batch, vec![]).unwrap();
    rc.execute_batch(&batch).unwrap();
    let mut batch = String::from("INSERT INTO r VALUES ");
    for i in 0..200i64 {
        if i > 0 {
            batch.push(',');
        }
        batch.push_str(&format!("({}, 'z{}')", i, i % 29));
    }
    db.execute(&batch, vec![]).unwrap();
    rc.execute_batch(&batch).unwrap();
    for q in [
        "SELECT count(*) FROM l JOIN r ON l.y < r.y AND r.y < 40",
        "SELECT count(*) FROM l LEFT JOIN r ON l.y < r.y AND r.y < 40",
        "SELECT count(*) FROM l, r WHERE l.y < r.y AND r.y < 40",
        "SELECT l.y, count(*) FROM l JOIN r ON l.y < r.y AND r.y < 40 GROUP BY l.y ORDER BY 1 LIMIT 25",
    ] {
        let mine = engine_rows(&db, q);
        let theirs = sqlite_rows(&rc, q);
        assert_eq!(
            format!("{:?}", mine),
            format!("{:?}", theirs),
            "scale count disagrees on: {q}"
        );
    }
}

/// Boolean-context truthiness — SQLite's exact coercion rules, probed
/// empirically (the old engine treated any non-empty TEXT as true and
/// NOT NULL as 1):
/// - TEXT numericizes with `sqlite3AtoF` PREFIX semantics ('abc' -> 0 =
///   false, '1x' -> 1 = true, 'inf' -> false, '.5' -> true);
/// - a non-empty BLOB is TRUE in a WHERE context AND under NOT
///   (`NOT x'31'` = 0 — the same truthiness rule, probed);
/// - `NOT NULL` is NULL (three-valued logic — the row is filtered, not
///   included).
#[test]
fn boolean_truthiness_matches_sqlite() {
    let mut db = rustqlite::Database::open(":memory:").unwrap();
    db.execute("CREATE TABLE t (x INT)", vec![]).unwrap();
    db.execute("INSERT INTO t VALUES (NULL),(1),(0)", vec![])
        .unwrap();
    let rc = Connection::open_in_memory().unwrap();
    rc.execute_batch("CREATE TABLE t (x INT); INSERT INTO t VALUES (NULL),(1),(0);")
        .unwrap();
    let t_setup = &[
        "CREATE TABLE t (x INT)",
        "INSERT INTO t VALUES (NULL),(1),(0)",
    ];
    for q in [
        "SELECT count(*) FROM t WHERE NOT (x >= 5)",
        "SELECT count(*) FROM t WHERE NOT x",
        "SELECT x, NOT (x >= 5) FROM t ORDER BY x",
        "SELECT 1 WHERE 'abc'",
        "SELECT 1 WHERE '1x'",
        "SELECT 1 WHERE '0'",
        "SELECT 1 WHERE '0.0'",
        "SELECT 1 WHERE '  +2e1  '",
        "SELECT 1 WHERE ''",
        "SELECT 1 WHERE 'inf'",
        "SELECT 1 WHERE '.5'",
        "SELECT 1 WHERE '.'",
        "SELECT 1 WHERE 'e5'",
        "SELECT 1 WHERE x'31'",
        "SELECT 1 WHERE x''",
        "SELECT 1 WHERE NOT 'abc'",
        "SELECT NOT 'abc'",
        "SELECT NOT NULL",
        "SELECT NOT 1, NOT 0, NOT 2.5",
        "SELECT NOT x'31'",
    ] {
        let setup: &[&str] = if q.contains(" FROM t") { t_setup } else { &[] };
        differential(setup, q);
    }
    // The join-path NOT shapes (three-valued through the compiled term).
    differential(SETUP, "SELECT count(*) FROM a JOIN b ON NOT (a.y >= b.y)");
    differential(
        SETUP,
        "SELECT a.x, b.z FROM a JOIN b ON NOT (a.y >= b.y) ORDER BY 1,2",
    );
}
