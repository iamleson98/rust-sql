//! Rowid pseudo-column references in JOIN conditions and projections —
//! differential against real SQLite.
//!
//! The engine's scan drivers append a hidden trailing rowid slot
//! (`planner::HIDDEN_ROWID`) to every rowid-table scan without an
//! INTEGER-PK alias column. Before the side-qualified slot names
//! (`"a.\0rowid"`), every scan named its slot identically, and a
//! qualified ref (`a.rowid`) that failed qualified resolution fell back
//! to the FIRST same-named slot in the combined list — so inside a join
//! both `a.rowid` and `b.rowid` bound to the LEFT side's rowid:
//!
//! - `ON a.rowid = b.rowid` compared left.rowid with left.rowid →
//!   EVERY pair matched (a silent cross product — wrong rows, the worst
//!   bug class);
//! - `ON a.rowid < b.rowid` was always false → empty result;
//! - `SELECT a.rowid, b.rowid ...` projected the left rowid into both
//!   output columns (and NULLs through the fused join, which dropped
//!   the slots the materialized path carried).
//!
//! The fix: each side's slot is named with its effective prefix, and
//! every resolution chain (EvalContext::lookup, resolve_column_index,
//! col_index) learns a "hidden slot by qualifier" pass. These tests pin
//! the semantics against real SQLite (row-level differential), plus:
//!
//! - `COLLATE BINARY` (the DEFAULT collation — SQLite accepts it
//!   everywhere; the engine used to error "no such collation sequence:
//!   BINARY");
//! - `Database::open(":memory:")` is a PURE in-memory database
//!   (sqlite3_open semantics — no file named `:memory:` is ever created
//!   in the CWD, and two opens share nothing).

use rusqlite::Connection;
use std::path::PathBuf;

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

/// Run the same setup + query in both engines and compare every row.
fn differential(setup: &[&str], query: &str) {
    let mut db = rustqlite::Database::open(":memory:").unwrap();
    let rc = Connection::open_in_memory().unwrap();
    for sql in setup {
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
    "INSERT INTO a VALUES ('Alpha',1),('beta',2),('Gamma',3)",
    "INSERT INTO b VALUES (1,'x'),(2,'y'),(4,'w')",
];

/// Join conditions comparing the two sides' rowids — the old
/// cross-product bug (`a.rowid = b.rowid` matched every pair).
#[test]
fn rowid_rowid_conditions_match_sqlite() {
    for q in [
        "SELECT a.x, b.z FROM a JOIN b ON a.rowid = b.rowid ORDER BY 1,2",
        "SELECT count(*) FROM a JOIN b ON a.rowid = b.rowid",
        "SELECT a.x, b.z FROM a JOIN b ON a.rowid < b.rowid ORDER BY 1,2",
        "SELECT count(*) FROM a JOIN b ON a.rowid < b.rowid",
        "SELECT a.x, b.z FROM a JOIN b ON a.rowid > b.rowid ORDER BY 1,2",
        "SELECT a.x FROM a JOIN b ON a.rowid <= b.rowid ORDER BY 1",
        // implicit join (WHERE form)
        "SELECT count(*) FROM a, b WHERE a.rowid < b.rowid",
        "SELECT a.x, b.z FROM a, b WHERE a.rowid = b.rowid ORDER BY 1,2",
        // rowid vs a real column (mixed bindings)
        "SELECT a.x, b.z FROM a JOIN b ON a.y = b.rowid ORDER BY 1,2",
        "SELECT count(*) FROM a JOIN b ON a.rowid = b.y",
        // composite condition: rowid pair AND column pair
        "SELECT a.x, b.z FROM a JOIN b ON a.rowid = b.rowid AND a.y = b.y ORDER BY 1,2",
        "SELECT count(*) FROM a JOIN b ON a.rowid < b.rowid AND a.y < b.y",
        // rowid spellings
        "SELECT count(*) FROM a JOIN b ON a._rowid_ = b.oid",
        "SELECT count(*) FROM a JOIN b ON a.oid < b._rowid_",
    ] {
        differential(SETUP, q);
    }
}

/// OUTER joins with rowid conditions — the NULL-extension semantics and
/// the unmatched tails.
#[test]
fn rowid_outer_joins_match_sqlite() {
    for q in [
        "SELECT a.x, b.z FROM a LEFT JOIN b ON a.rowid < b.rowid ORDER BY 1,2",
        "SELECT count(*) FROM a LEFT JOIN b ON a.rowid < b.rowid",
        "SELECT a.x, b.z FROM a LEFT JOIN b ON a.rowid = b.rowid ORDER BY 1,2",
        "SELECT a.x, b.z FROM a RIGHT JOIN b ON a.rowid = b.rowid ORDER BY 1,2",
        "SELECT a.x, b.z FROM a FULL JOIN b ON a.rowid = b.rowid ORDER BY 1,2",
        "SELECT count(*) FROM a FULL JOIN b ON a.rowid < b.rowid",
        "SELECT count(*) FROM a LEFT JOIN b ON a.y = b.rowid",
    ] {
        differential(SETUP, q);
    }
}

/// Projections of one/both sides' rowids over a join — the old NULL /
/// wrong-side bug (both output columns carried the LEFT rowid; the
/// fused path emitted NULLs outright).
#[test]
fn rowid_projections_over_joins_match_sqlite() {
    for q in [
        "SELECT a.rowid, b.rowid FROM a JOIN b ON a.y = b.y ORDER BY 1,2",
        "SELECT a.rowid, b.rowid FROM a JOIN b ON a.y = b.y",
        "SELECT a.rowid FROM a JOIN b ON a.y = b.y ORDER BY 1",
        "SELECT b.rowid FROM a JOIN b ON a.y = b.y ORDER BY 1",
        "SELECT a.rowid, b.z FROM a JOIN b ON a.y = b.y ORDER BY 1,2",
        // Sort above the join (the fused path's no-Project mode)
        "SELECT a.rowid, b.rowid FROM a JOIN b ON a.y = b.y ORDER BY 1,2",
        "SELECT a.rowid FROM a JOIN b ON a.y = b.y ORDER BY a.rowid",
        "SELECT a.rowid, b.rowid FROM a JOIN b ON a.y = b.y ORDER BY 1 DESC, 2 DESC",
        // subquery-wrapped (materialized) shape
        "SELECT * FROM (SELECT a.rowid r, b.rowid s FROM a JOIN b ON a.y = b.y) ORDER BY 1,2",
        // aggregate over a join with rowid args
        "SELECT count(a.rowid), min(b.rowid), max(a.rowid) FROM a JOIN b ON a.y = b.y",
        // LIMIT shapes
        "SELECT a.rowid, b.rowid FROM a JOIN b ON a.y = b.y ORDER BY 1,2 LIMIT 2",
        // aliases as side prefixes
        "SELECT al.rowid, ci.rowid FROM a al JOIN b ci ON al.y < ci.y ORDER BY 1,2",
        "SELECT count(*) FROM a al JOIN b ci ON al.rowid = ci.rowid",
    ] {
        differential(SETUP, q);
    }
}

/// Three-table chains and star projections: every side's slot must stay
/// distinguishable when three scans each carry one.
#[test]
fn rowid_three_way_and_star_match_sqlite() {
    let setup: Vec<&str> = SETUP
        .iter()
        .chain(["CREATE TABLE c (w INT)", "INSERT INTO c VALUES (5),(6)"].iter())
        .copied()
        .collect();
    for q in [
        "SELECT count(*) FROM a JOIN b ON a.rowid < b.rowid JOIN c ON b.rowid < c.rowid",
        "SELECT a.x, c.w FROM a JOIN b ON a.rowid = b.rowid JOIN c ON b.rowid = c.rowid ORDER BY 1,2",
        "SELECT count(*) FROM a, b, c WHERE a.rowid = b.rowid AND b.rowid = c.rowid",
        // star arity: the hidden slots must NOT widen a star expansion
        "SELECT count(*) FROM (SELECT * FROM a JOIN b ON a.y = b.y)",
        "SELECT * FROM a JOIN b ON a.y = b.y ORDER BY 1,2,3,4",
    ] {
        differential(&setup, q);
    }
}

/// Tables WITH an INTEGER PRIMARY KEY alias column: rowid refs bind the
/// alias column (the slot does not exist for these tables).
#[test]
fn rowid_alias_tables_match_sqlite() {
    let setup = &[
        "CREATE TABLE u (id INTEGER PRIMARY KEY, v INT)",
        "CREATE TABLE o (id INTEGER PRIMARY KEY, u_id INT)",
        "INSERT INTO u VALUES (1,10),(2,20)",
        "INSERT INTO o VALUES (100,1),(200,2),(300,1)",
    ];
    for q in [
        "SELECT o.id, u.v FROM u JOIN o ON o.u_id = u.id ORDER BY 1",
        "SELECT count(*) FROM u JOIN o ON u.rowid = o.u_id",
        "SELECT u.rowid, o.rowid FROM u JOIN o ON o.u_id = u.id ORDER BY 1,2",
        "SELECT count(*) FROM u JOIN o ON u.rowid < o.rowid",
        "SELECT count(*) FROM u JOIN o ON o.id = u.id",
    ] {
        differential(setup, q);
    }
}

/// Self-joins: both sides share the table (and its slot name) — refs
/// resolve like SQLite's first-match rule.
#[test]
fn rowid_self_join_matches_sqlite() {
    let setup = &["CREATE TABLE t (v INT)", "INSERT INTO t VALUES (7),(8)"];
    for q in [
        "SELECT count(*) FROM t t1 JOIN t t2 ON t1.rowid = t2.rowid",
        "SELECT t1.rowid, t2.rowid FROM t t1 JOIN t t2 ON t1.rowid < t2.rowid ORDER BY 1,2",
        "SELECT count(*) FROM t t1 JOIN t t2 ON t1.rowid < t2.rowid",
        "SELECT t1.rowid, t2.rowid FROM t t1 JOIN t t2 ON t1.v = t2.v ORDER BY 1,2",
    ] {
        differential(setup, q);
    }
}

/// `COLLATE BINARY` is SQLite's DEFAULT collation — legal in every
/// comparison position. Unknown collations still error (both engines).
#[test]
fn collate_binary_is_the_default_not_an_error() {
    let setup = &[
        "CREATE TABLE w (s TEXT)",
        "INSERT INTO w VALUES ('apple'),('Banana'),('cherry')",
    ];
    for q in [
        "SELECT s FROM w WHERE s < 'b' COLLATE BINARY ORDER BY 1",
        "SELECT s FROM w WHERE 'a' < s COLLATE BINARY ORDER BY 1",
        "SELECT count(*) FROM w WHERE s = 'apple' COLLATE BINARY",
        "SELECT s < 'b' COLLATE BINARY FROM w ORDER BY 1",
        "SELECT count(*) FROM w WHERE s COLLATE BINARY = s",
        // NOCASE still collates
        "SELECT s FROM w WHERE s < 'b' COLLATE NOCASE ORDER BY 1",
    ] {
        differential(setup, q);
    }
    // Unknown collations: both engines error with the same message shape.
    let mut db = rustqlite::Database::open(":memory:").unwrap();
    for sql in setup {
        db.execute(sql, vec![]).unwrap();
    }
    let err = db
        .query("SELECT 1 WHERE 'a' < 'b' COLLATE NOSUCHCOLL", vec![])
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("no such collation sequence"),
        "unknown collation should error, got: {err}"
    );
}

/// `Database::open(":memory:")` must be a PURE in-memory database —
/// sqlite3_open(":memory:") semantics: no file in the CWD, no data
/// shared across opens. (It used to create a literal file named
/// `:memory:` in the working directory, silently persisting data.)
#[test]
fn memory_string_is_a_real_memory_database() {
    // Run from a scratch directory so a leaked file would be visible.
    let dir = tempfile::tempdir().unwrap();
    let cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir.path()).unwrap();
    let result = std::panic::catch_unwind(|| {
        {
            let mut db = rustqlite::Database::open(":memory:").unwrap();
            db.execute("CREATE TABLE t (x INT)", vec![]).unwrap();
            db.execute("INSERT INTO t VALUES (1)", vec![]).unwrap();
            assert_eq!(
                db.query("SELECT count(*) FROM t", vec![]).unwrap(),
                vec![vec![rustqlite::Value::Integer(1)]]
            );
        }
        // No file named ":memory:" in the CWD.
        assert!(
            !dir.path().join(":memory:").exists(),
            "open(\":memory:\") leaked a file into the CWD"
        );
        // A fresh open sees NO data (not the previous database's file).
        let db2 = rustqlite::Database::open(":memory:").unwrap();
        let r = db2
            .query("SELECT count(*) FROM sqlite_master", vec![])
            .unwrap();
        assert_eq!(r, vec![vec![rustqlite::Value::Integer(0)]]);
    });
    std::env::set_current_dir(cwd).unwrap();
    result.unwrap();
}

/// The plan-level mirrors (atom_out_cols) must report the same slot
/// names the executors emit — the join-reorder machinery keys on them.
/// A 4-table reorder with rowid projections exercises the static lists.
#[test]
fn rowid_refs_survive_join_reorder() {
    let setup: Vec<&str> = SETUP
        .iter()
        .chain(["CREATE TABLE c (w INT)", "INSERT INTO c VALUES (1),(2)"].iter())
        .copied()
        .collect();
    for q in [
        // reordering spine: 3 tables, join keys chosen so the greedy
        // reorder picks a different FROM order than written
        "SELECT a.x, b.z, c.w FROM c JOIN a ON a.y = c.w JOIN b ON b.y = a.y ORDER BY 1,2,3",
        "SELECT a.rowid, c.w FROM c JOIN a ON a.y = c.w ORDER BY 1,2",
        "SELECT b.rowid, a.x FROM c JOIN b ON b.y = c.w JOIN a ON a.y = c.w ORDER BY 1,2",
    ] {
        differential(&setup, q);
    }
}

/// Rowid refs inside WHERE filters applied ABOVE a join (the Filter
/// wraps the Join — evaluation over the combined columns).
#[test]
fn rowid_where_above_join_matches_sqlite() {
    for q in [
        "SELECT a.x, b.z FROM a JOIN b ON a.y = b.y WHERE a.rowid < 3 ORDER BY 1,2",
        "SELECT a.x, b.z FROM a JOIN b ON a.y = b.y WHERE b.rowid > 1 ORDER BY 1,2",
        "SELECT count(*) FROM a JOIN b ON a.y = b.y WHERE a.rowid = b.rowid",
        "SELECT a.x FROM a JOIN b ON a.y = b.y WHERE a.rowid = 2 ORDER BY 1",
    ] {
        differential(SETUP, q);
    }
}

/// A real file path still works (the :memory: fix must not divert real
/// paths) and round-trips through reopen.
#[test]
fn real_paths_unaffected_by_memory_fix() {
    let dir = tempfile::tempdir().unwrap();
    let mut path: PathBuf = dir.path().to_path_buf();
    path.push("real.db");
    {
        let mut db = rustqlite::Database::open(&path).unwrap();
        db.execute("CREATE TABLE t (x INT)", vec![]).unwrap();
        db.execute("INSERT INTO t VALUES (42)", vec![]).unwrap();
    }
    assert!(path.exists(), "real path must create the file");
    let db2 = rustqlite::Database::open(&path).unwrap();
    assert_eq!(
        db2.query("SELECT x FROM t", vec![]).unwrap(),
        vec![vec![rustqlite::Value::Integer(42)]]
    );
}
