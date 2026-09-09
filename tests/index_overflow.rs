//! Index overflow chains: index keys larger than one page spill to
//! overflow chains (SQLite parity — indexing a >page TEXT/BLOB is legal).
//! These tests pin: insert/lookup correctness at scale (multi-level
//! trees with oversized separators), UPDATE/DELETE maintenance (chain
//! freeing), persistence across reopen, integrity_check, and the
//! SQLite-file interop roundtrip.

use rustqlite::{Database, Value};

fn big(i: i64, base: usize) -> String {
    let pad = format!("{:06}", i);
    let mut s = String::with_capacity(base + pad.len());
    s.push_str(&pad);
    s.push_str(&"x".repeat(base));
    s
}

fn build(db: &mut Database, n: i64, base: usize) {
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, k TEXT, v INTEGER)",
        [],
    )
    .unwrap();
    db.execute("CREATE INDEX idx_big ON t(k)", []).unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=n {
        db.execute(
            "INSERT INTO t (id, k, v) VALUES (?, ?, ?)",
            [
                Value::Integer(i),
                Value::Text(big(i, base).as_str().into()),
                Value::Integer(i * 3),
            ],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
}

#[test]
fn overflow_index_insert_lookup_scale() {
    let mut db = Database::open_in_memory().unwrap();
    // 9-10KB keys: every index cell spills; the tree grows interior
    // levels whose separators are themselves overflow copies.
    build(&mut db, 60, 9000);
    // Point lookups by the full key (the binary searches must resolve
    // local-prefix ambiguity by chain reassembly).
    for i in [1i64, 7, 25, 31, 44, 59, 60] {
        let rows = db
            .query(
                "SELECT id, v FROM t WHERE k = ?",
                [Value::Text(big(i, 9000).as_str().into())],
            )
            .unwrap();
        assert_eq!(rows.len(), 1, "lookup {} missing", i);
        assert_eq!(rows[0][0], Value::Integer(i));
        assert_eq!(rows[0][1], Value::Integer(i * 3));
    }
    // Range scan over the index.
    let rows = db.query("SELECT COUNT(*) FROM t WHERE k > ''", []).unwrap();
    assert_eq!(rows[0][0], Value::Integer(60));
    // ORDER BY on the indexed column: prefix bytes are the row numbers —
    // lexicographic order = numeric order here (06-width padding).
    let rows = db.query("SELECT id FROM t ORDER BY k LIMIT 3", []).unwrap();
    let ids: Vec<i64> = rows.iter().map(|r| r[0].as_integer()).collect();
    assert_eq!(ids, vec![1, 2, 3]);
}

#[test]
fn overflow_index_update_moves_chains() {
    let mut db = Database::open_in_memory().unwrap();
    build(&mut db, 30, 9000);
    // UPDATE the indexed column: the old entry (with its chain) is
    // deleted, the new one inserted.
    db.execute(
        "UPDATE t SET k = ? WHERE id = 15",
        [Value::Text(big(99, 8500).as_str().into())],
    )
    .unwrap();
    let old = db
        .query(
            "SELECT COUNT(*) FROM t WHERE k = ?",
            [Value::Text(big(15, 9000).as_str().into())],
        )
        .unwrap();
    assert_eq!(old[0][0], Value::Integer(0), "old key still indexed");
    let new = db
        .query(
            "SELECT id FROM t WHERE k = ?",
            [Value::Text(big(99, 8500).as_str().into())],
        )
        .unwrap();
    assert_eq!(new.len(), 1);
    assert_eq!(new[0][0], Value::Integer(15));
    // Deleting rows must free the chains (the freelist grows; subsequent
    // inserts reuse those pages without corruption).
    db.execute("DELETE FROM t WHERE id <= 10", []).unwrap();
    let cnt = db.query("SELECT COUNT(*) FROM t WHERE k > ''", []).unwrap();
    assert_eq!(cnt[0][0], Value::Integer(20));
    db.execute("BEGIN", []).unwrap();
    for i in 100..115i64 {
        db.execute(
            "INSERT INTO t (id, k, v) VALUES (?, ?, ?)",
            [
                Value::Integer(i),
                Value::Text(big(i, 9100).as_str().into()),
                Value::Integer(i),
            ],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    let cnt = db.query("SELECT COUNT(*) FROM t WHERE k > ''", []).unwrap();
    assert_eq!(cnt[0][0], Value::Integer(35));
}

#[test]
fn overflow_index_persist_reopen() {
    let path = std::env::temp_dir().join("rq_idx_ovf_persist.db");
    let _ = std::fs::remove_file(&path);
    {
        let mut db = Database::open(path.to_str().unwrap()).unwrap();
        build(&mut db, 40, 9000);
        let ic = db.query("PRAGMA integrity_check", []).unwrap();
        assert_eq!(ic[0][0], Value::Text("ok".into()), "integrity pre-close");
    }
    {
        let db = Database::open(path.to_str().unwrap()).unwrap();
        for i in [1i64, 20, 40] {
            let rows = db
                .query(
                    "SELECT v FROM t WHERE k = ?",
                    [Value::Text(big(i, 9000).as_str().into())],
                )
                .unwrap();
            assert_eq!(rows.len(), 1, "row {} missing after reopen", i);
            assert_eq!(rows[0][0], Value::Integer(i * 3));
        }
        let ic = db.query("PRAGMA integrity_check", []).unwrap();
        assert_eq!(ic[0][0], Value::Text("ok".into()), "integrity post-reopen");
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn overflow_index_unique_and_multi_col() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE u (id INTEGER PRIMARY KEY, a TEXT, b INTEGER)",
        [],
    )
    .unwrap();
    db.execute("CREATE UNIQUE INDEX ux ON u(a, b)", []).unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=25i64 {
        db.execute(
            "INSERT INTO u (id, a, b) VALUES (?, ?, ?)",
            [
                Value::Integer(i),
                Value::Text(big(i, 5000).as_str().into()),
                Value::Integer(i % 3),
            ],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    // UNIQUE enforcement through the overflow index: a duplicate (a, b)
    // must be rejected by the index lookup.
    let dup = db.execute(
        "INSERT INTO u (id, a, b) VALUES (?, ?, ?)",
        [
            Value::Integer(100),
            Value::Text(big(1, 5000).as_str().into()),
            Value::Integer(1),
        ],
    );
    assert!(
        dup.is_err(),
        "duplicate (a,b) accepted through overflow index"
    );
    // Composite prefix lookup: only the leading column constrained.
    let rows = db
        .query(
            "SELECT id FROM u WHERE a = ?",
            [Value::Text(big(7, 5000).as_str().into())],
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Integer(7));
}

#[test]
fn overflow_index_sqlite_file_interop_roundtrip() {
    // A SQLite-built file with >page index keys loads and queries.
    let sq_path = std::env::temp_dir().join("rq_idx_ovf_from_sqlite.db");
    let out_path = std::env::temp_dir().join("rq_idx_ovf_dumped.db");
    for p in [&sq_path, &out_path] {
        let _ = std::fs::remove_file(p);
    }
    {
        let conn = rusqlite::Connection::open(sq_path.to_str().unwrap()).unwrap();
        conn.execute_batch(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, long_text TEXT);
             CREATE INDEX idx_long ON t(long_text);",
        )
        .unwrap();
        for i in 0..20i64 {
            let b = "x".repeat(9000 + (i as usize) * 100);
            conn.execute(
                "INSERT INTO t (id, long_text) VALUES (?, ?)",
                rusqlite::params![i, b],
            )
            .unwrap();
        }
    }
    {
        let mut db = Database::open(sq_path.to_str().unwrap()).unwrap();
        let rows = db
            .query("SELECT COUNT(*) FROM t WHERE long_text > ''", [])
            .unwrap();
        assert_eq!(rows[0][0], Value::Integer(20));
        // Autocommit write -> the file is re-dumped in SQLite format
        // (the writer emits overflow index cells byte-exactly).
        db.execute("INSERT INTO t (id, long_text) VALUES (100, 'small')", [])
            .unwrap();
    }
    {
        // The engine's own written file: reload + real SQLite verifies.
        let db = Database::open(sq_path.to_str().unwrap()).unwrap();
        // 20 overflow-key rows + the small row just inserted (matches > '').
        let rows = db
            .query("SELECT COUNT(*) FROM t WHERE long_text > ''", [])
            .unwrap();
        assert_eq!(rows[0][0], Value::Integer(21));
        drop(db);
        let conn = rusqlite::Connection::open(sq_path.to_str().unwrap()).unwrap();
        let (n, probe): (i64, String) = conn
            .query_row("SELECT COUNT(*), long_text FROM t WHERE id = 7", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(probe.len(), 9700);
        let ic: String = conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ic, "ok");
    }
    let _ = std::fs::remove_file(&sq_path);
    let _ = std::fs::remove_file(&out_path);
}

#[test]
fn overflow_index_integrity_and_differential() {
    let mut db = Database::open_in_memory().unwrap();
    build(&mut db, 50, 9200);
    // Churn: interleaved inserts and deletes keep the tree splitting and
    // merging while chains are allocated and freed.
    db.execute("BEGIN", []).unwrap();
    for i in 200..260i64 {
        db.execute(
            "INSERT INTO t (id, k, v) VALUES (?, ?, ?)",
            [
                Value::Integer(i),
                Value::Text(big(i, 8800).as_str().into()),
                Value::Integer(i),
            ],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    db.execute("DELETE FROM t WHERE id % 3 = 0", []).unwrap();
    let ic = db.query("PRAGMA integrity_check", []).unwrap();
    assert_eq!(ic[0][0], Value::Text("ok".into()));
    // Differential: every surviving row is reachable via its key.
    let total = db.query("SELECT COUNT(*) FROM t", []).unwrap();
    let n_total = total[0][0].as_integer();
    let via_key = db.query("SELECT COUNT(*) FROM t WHERE k > ''", []).unwrap();
    assert_eq!(
        via_key[0][0],
        Value::Integer(n_total),
        "index/table row mismatch"
    );
    // The same data through real SQLite for a sanity cross-check.
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, k TEXT, v INTEGER); CREATE INDEX idx_big ON t(k);",
    )
    .unwrap();
    let rows = db.query("SELECT id, k, v FROM t ORDER BY id", []).unwrap();
    conn.execute("BEGIN", []).unwrap();
    for r in &rows {
        let k = r[1].as_text().to_string();
        conn.execute(
            "INSERT INTO t (id, k, v) VALUES (?, ?, ?)",
            rusqlite::params![r[0].as_integer(), k, r[2].as_integer()],
        )
        .unwrap();
    }
    conn.execute("COMMIT", []).unwrap();
    let (sq_n, sq_sum): (i64, i64) = conn
        .query_row("SELECT COUNT(*), SUM(v) FROM t", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    let (rq_n, rq_sum): (i64, i64) = db
        .query("SELECT COUNT(*), SUM(v) FROM t", [])
        .unwrap()
        .iter()
        .map(|r| (r[0].as_integer(), r[1].as_integer()))
        .next()
        .unwrap();
    assert_eq!((sq_n, sq_sum), (rq_n, rq_sum));
}
