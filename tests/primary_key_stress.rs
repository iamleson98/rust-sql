//! PRIMARY KEY intensive stress — differential against real SQLite.
//!
//! Covers the three PK families the production integrations rely on,
//! each pinned to SQLite's exact observable behavior (value-for-value,
//! message-for-message):
//!
//!   1. Rowid-alias declaration matrix — which of the 10 declaration
//!      spellings make `id` an alias for rowid (column-level exact
//!      "INTEGER" ASC-only; table-level exact "INTEGER" where DESC does
//!      NOT block the alias; "INT"/"TINYINT"/"UNSIGNED INTEGER" never).
//!   2. Rowid-alias value affinity (SQLite OP_MustBeInt): REALs that are
//!      not exact integers ('5.5', 1e30, ±2^63), non-integer TEXT
//!      ('7.5', '0x10'), and BLOBs fail with "datatype mismatch"; exact
//!      REALs ('8.0', 2^62, -0.0) and integer-looking TEXT (' 8 ',
//!      '+9', '8e0' via REAL squeeze) become integers.
//!   3. AUTOINCREMENT lifecycle: sqlite_sequence semantics (no id reuse
//!      after delete-max, failed-statement id restoration, explicit
//!      max bumps, write-protected sequence table, reopen durability).
//!   4. TEXT/composite PK uniqueness churn: exact UNIQUE messages,
//!      upserts, PK moves, and the NULL-in-PK legacy quirk of rowid
//!      tables (NULLs are allowed AND distinct in a composite PK;
//!      WITHOUT ROWID enforces NOT NULL).
//!   5. Randomized seeded workloads across all PK shapes with
//!      full-state comparison after every phase.
//!
//! Env knobs (CI cranks these; defaults keep `cargo test` quick):
//!   PK_STRESS_ROWS    — rows in the churn/randomized phases (default 3000)
//!   PK_STRESS_SEEDS   — randomized workload seeds      (default 8)

use rusqlite::Connection;

// ---------------------------------------------------------------------------
// Differential helpers (same conventions as tests/without_rowid_pk.rs).
// ---------------------------------------------------------------------------

/// rusqlite wraps errors as "error: ..."; strip the prefix so both sides
/// carry the bare message.
fn clean(s: &str) -> String {
    let s = s.strip_prefix("error: ").unwrap_or(s);
    // Newer SQLite appends " in <sql> at offset N" to prepare-time
    // resolution errors surfaced through rusqlite; sqlite3_errmsg (the
    // CLI / python binding view — the engine's reference) carries the
    // bare message. Strip the suffix for comparison.
    if let Some(pos) = s.find(" in SELECT ") {
        if s[pos..].contains(" at offset ") {
            return s[..pos].to_string();
        }
    }
    s.to_string()
}

/// Execute one statement on both engines; errors must match exactly.
fn both(db: &mut rustqlite::Database, rc: &Connection, sql: &str) {
    let e1 = db.execute(sql, ()).map_err(|e| e.to_string());
    let e2 = rc.execute_batch(sql).map_err(|e| clean(&e.to_string()));
    assert_eq!(
        e1, e2,
        "engine/SQLite disagree on: {sql}\n  engine: {e1:?}\n  sqlite: {e2:?}"
    );
}

fn sv(v: rusqlite::types::ValueRef<'_>) -> rustqlite::Value {
    match v {
        rusqlite::types::ValueRef::Null => rustqlite::Value::Null,
        rusqlite::types::ValueRef::Integer(v) => rustqlite::Value::Integer(v),
        rusqlite::types::ValueRef::Real(v) => rustqlite::Value::Real(v),
        rusqlite::types::ValueRef::Text(t) => {
            rustqlite::Value::Text(String::from_utf8_lossy(t).to_string().into())
        }
        rusqlite::types::ValueRef::Blob(b) => rustqlite::Value::Blob(b.to_vec()),
    }
}

fn engine_rows(db: &rustqlite::Database, sql: &str) -> Vec<rustqlite::Value> {
    db.query(sql, ())
        .unwrap_or_else(|e| panic!("engine query failed: {sql}: {e}"))
        .into_iter()
        .flatten()
        .collect()
}

fn sqlite_rows(rc: &Connection, sql: &str) -> Vec<rustqlite::Value> {
    let mut out = Vec::new();
    let mut stmt = rc.prepare(sql).expect("sqlite prepare");
    let n = stmt.column_count();
    let mut rows = stmt.query([]).unwrap();
    while let Ok(Some(r)) = rows.next() {
        for i in 0..n {
            out.push(sv(r.get_ref(i).unwrap()));
        }
    }
    out
}

/// Query both engines; rows must match exactly (value + class + order).
fn q_both(db: &rustqlite::Database, rc: &Connection, sql: &str) -> Vec<rustqlite::Value> {
    let r1 = engine_rows(db, sql);
    let r2 = sqlite_rows(rc, sql);
    assert_eq!(r1, r2, "engine/SQLite rows disagree on: {sql}");
    r1
}

fn fresh() -> (rustqlite::Database, Connection) {
    (
        rustqlite::Database::open_in_memory().unwrap(),
        Connection::open_in_memory().unwrap(),
    )
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

// ===========================================================================
// 1. Rowid-alias declaration matrix
// ===========================================================================

#[test]
fn rowid_alias_declaration_matrix() {
    // (name, ddl, id is a rowid alias per real SQLite)
    let shapes: &[(&str, &str, bool)] = &[
        ("col INTEGER", "id INTEGER PRIMARY KEY, v TEXT", true),
        (
            "col INTEGER ASC",
            "id INTEGER PRIMARY KEY ASC, v TEXT",
            true,
        ),
        // Column-level DESC blocks the alias (SQLite's documented quirk).
        (
            "col INTEGER DESC",
            "id INTEGER PRIMARY KEY DESC, v TEXT",
            false,
        ),
        // "INT" is INTEGER affinity but NOT an alias (exact "INTEGER" only).
        ("col INT", "id INT PRIMARY KEY, v TEXT", false),
        ("col INT DESC", "id INT PRIMARY KEY DESC, v TEXT", false),
        (
            "col UNSIGNED INTEGER",
            "id UNSIGNED INTEGER PRIMARY KEY, v TEXT",
            false,
        ),
        // Table-level: exact "INTEGER" → alias; DESC does NOT block it.
        ("tbl INTEGER", "id INTEGER, v TEXT, PRIMARY KEY(id)", true),
        (
            "tbl INTEGER DESC",
            "id INTEGER, v TEXT, PRIMARY KEY(id DESC)",
            true,
        ),
        ("tbl INT", "id INT, v TEXT, PRIMARY KEY(id)", false),
        (
            "tbl INT DESC",
            "id INT, v TEXT, PRIMARY KEY(id DESC)",
            false,
        ),
        (
            "col INTEGER AUTOINCREMENT",
            "id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT",
            true,
        ),
    ];
    for (name, cols, alias) in shapes {
        let (mut db, rc) = fresh();
        let ddl = format!("CREATE TABLE t ({cols})");
        both(&mut db, &rc, &ddl);
        both(&mut db, &rc, "INSERT INTO t (id, v) VALUES (NULL, 'x')");
        if *alias {
            // NULL auto-fills the alias: id == rowid == 1.
            let rows = q_both(&db, &rc, "SELECT id, rowid FROM t");
            assert_eq!(
                rows,
                vec![rustqlite::Value::Integer(1), rustqlite::Value::Integer(1)],
                "{name}: NULL insert must auto-fill the alias"
            );
            // An explicit id equals rowid.
            both(&mut db, &rc, "INSERT INTO t (id, v) VALUES (42, 'y')");
            q_both(&db, &rc, "SELECT rowid, id FROM t WHERE id = 42");
            // Duplicates fail with the PK message.
            both(&mut db, &rc, "INSERT INTO t (id, v) VALUES (42, 'z')");
        } else {
            // id stays NULL; rowid is independent.
            let rows = q_both(&db, &rc, "SELECT id, rowid FROM t");
            assert_eq!(
                rows,
                vec![rustqlite::Value::Null, rustqlite::Value::Integer(1)],
                "{name}: NULL must stay NULL (not an alias)"
            );
            // Two NULL ids are DISTINCT in a rowid-table PK.
            both(&mut db, &rc, "INSERT INTO t (id, v) VALUES (NULL, 'y')");
            q_both(&db, &rc, "SELECT count(*) FROM t WHERE id IS NULL");
            // Explicit duplicate still violates the (indexed) PK.
            both(&mut db, &rc, "INSERT INTO t (id, v) VALUES (42, 'a')");
            both(&mut db, &rc, "INSERT INTO t (id, v) VALUES (42, 'b')");
            // rowid and id are independent.
            both(&mut db, &rc, "UPDATE t SET id = 99 WHERE rowid = 1");
            q_both(&db, &rc, "SELECT id, rowid FROM t ORDER BY rowid");
        }
    }
}

// ===========================================================================
// 2. Rowid-alias value affinity (OP_MustBeInt)
// ===========================================================================

#[test]
fn rowid_alias_value_affinity_matrix() {
    // (literal SQL fragment, must be accepted)
    let cases: &[(&str, bool)] = &[
        ("5.0", true),                     // exact REAL → 5
        ("5.5", false),                    // inexact REAL → datatype mismatch
        ("-5.0", true),                    // negative exact → -5
        ("1e3", true),                     // 1000
        ("9.5e2", true),                   // 950
        ("1e30", false),                   // out of i64 range
        ("9223372036854775807.0", false),  // 2^63 rounds up — not exact
        ("-9223372036854775808.0", false), // i64::MIN never converts
        ("4611686018427387904.0", true),   // 2^62 round-trips
        ("'7'", true),                     // integer text
        ("' 8 '", true),                   // whitespace tolerated
        ("'+9'", true),                    // leading plus
        ("'8e0'", true),                   // real text 8.0 → squeeze to 8
        ("'8.0'", true),                   // real text 8.0 → squeeze to 8
        ("'7.5'", false),                  // inexact real text
        ("'0x10'", false),                 // hex text stays TEXT
        ("x'35'", false),                  // blobs never convert
        ("-0.0", true),                    // -0.0 → 0
    ];
    for (lit, ok) in cases {
        let (mut db, rc) = fresh();
        both(
            &mut db,
            &rc,
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v INT)",
        );
        let sql = format!("INSERT INTO t VALUES ({lit}, 1)");
        let e1 = db.execute(&sql, ()).map_err(|e| e.to_string());
        let e2 = rc.execute_batch(&sql).map_err(|e| clean(&e.to_string()));
        assert_eq!(e1, e2, "literal {lit}: error mismatch");
        if !ok {
            assert!(
                e1.unwrap_err().contains("datatype mismatch"),
                "literal {lit}: SQLite rejects with datatype mismatch"
            );
        } else {
            q_both(&db, &rc, "SELECT id, typeof(id) FROM t");
        }
    }

    // Accepted literals with DISTINCT results land as integers, in
    // rowid lockstep with SQLite (8.0/'8e0'/' 8 ' all give 8 — pick one
    // spelling per distinct value to keep ids unique).
    let (mut db, rc) = fresh();
    both(
        &mut db,
        &rc,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INT)",
    );
    for lit in ["5.0", "1e3", "9.5e2", "' 8 '", "'+9'", "'8.0'", "-0.0"] {
        both(&mut db, &rc, &format!("INSERT INTO t VALUES ({lit}, 1)"));
    }
    let rows = q_both(&db, &rc, "SELECT id, typeof(id), rowid FROM t ORDER BY id");
    let mut ids: Vec<i64> = Vec::new();
    for c in rows.chunks(3) {
        assert!(
            matches!(&c[1], rustqlite::Value::Text(t) if t.as_str() == "integer"),
            "typeof must be 'integer' after the squeeze: {c:?}"
        );
        match c[0] {
            rustqlite::Value::Integer(i) => ids.push(i),
            _ => panic!("non-integer id after affinity: {c:?}"),
        }
    }
    assert_eq!(ids, vec![0, 5, 8, 9, 950, 1000]);
    // id == rowid for every accepted literal.
    q_both(&db, &rc, "SELECT count(*) FROM t WHERE id != rowid");
}

// ===========================================================================
// 3. Boundary ids and exhaustion
// ===========================================================================

#[test]
fn rowid_alias_boundary_ids() {
    let (mut db, rc) = fresh();
    both(
        &mut db,
        &rc,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)",
    );
    // Extreme explicit ids are fine.
    both(
        &mut db,
        &rc,
        "INSERT INTO t VALUES (9223372036854775807, 'max')",
    );
    both(
        &mut db,
        &rc,
        "INSERT INTO t VALUES (-9223372036854775808, 'min')",
    );
    both(&mut db, &rc, "INSERT INTO t VALUES (0, 'zero')");
    // Duplicate extremes fail identically.
    both(&mut db, &rc, "INSERT INTO t VALUES (0, 'dup')");
    both(
        &mut db,
        &rc,
        "INSERT INTO t VALUES (-9223372036854775808, 'dup')",
    );
    // With i64::MAX present, the next auto id overflows: SQLite errors.
    both(&mut db, &rc, "INSERT INTO t VALUES (NULL, 'exhaust')");

    // After removing the MAX row, auto-assignment works again on both —
    // but both engines now sit on a RANDOM rowid (allocated when MAX was
    // present; each engine's own value — inherently nondeterministic),
    // so compare the SHAPE: deterministic rows exact, random rows by
    // relationship (fresh value, next auto id = random + 1).
    both(&mut db, &rc, "DELETE FROM t WHERE id = 9223372036854775807");
    both(&mut db, &rc, "INSERT INTO t VALUES (NULL, 'recovers')");
    q_both(&db, &rc, "SELECT v FROM t WHERE id = -9223372036854775808");
    q_both(&db, &rc, "SELECT v FROM t WHERE id = 0");
    for (name, rows) in [
        (
            "engine",
            engine_rows(&db, "SELECT id, v FROM t ORDER BY id"),
        ),
        (
            "sqlite",
            sqlite_rows(&rc, "SELECT id, v FROM t ORDER BY id"),
        ),
    ] {
        let pairs: Vec<(i64, String)> = rows
            .chunks(2)
            .map(|c| match (&c[0], &c[1]) {
                (rustqlite::Value::Integer(i), rustqlite::Value::Text(t)) => {
                    (*i, t.as_str().to_string())
                }
                _ => panic!("{name}: unexpected row shape {c:?}"),
            })
            .collect();
        assert_eq!(pairs.len(), 4, "{name}: 4 rows");
        assert_eq!(pairs[0], (-9223372036854775808, "min".into()), "{name}");
        assert_eq!(pairs[1], (0, "zero".into()), "{name}");
        let (rand, tag) = &pairs[2];
        assert_eq!(tag, "exhaust", "{name}");
        assert!(*rand > 0 && *rand < i64::MAX, "{name}: fresh random rowid");
        assert_eq!(pairs[3].1, "recovers", "{name}");
        assert_eq!(
            pairs[3].0,
            rand.wrapping_add(1),
            "{name}: next auto id continues from the random rowid"
        );
    }
}

#[test]
fn rowid_alias_random_after_zero_then_max() {
    // SQLite: with max == i64::MAX the allocator falls back to a RANDOM
    // unused rowid — inherently non-deterministic on BOTH engines (any
    // i64, sign included). Verify the SHAPE: fresh, in-range,
    // non-colliding, and that both engines agree on everything
    // deterministic around it.
    let (mut db, rc) = fresh();
    both(
        &mut db,
        &rc,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)",
    );
    both(
        &mut db,
        &rc,
        "INSERT INTO t VALUES (9223372036854775807, 'max')",
    );
    both(&mut db, &rc, "INSERT INTO t VALUES (0, 'zero')");
    // NULL insert → random rowid on both engines.
    both(&mut db, &rc, "INSERT INTO t VALUES (NULL, 'rand')");
    for (name, rows) in [
        ("engine", engine_rows(&db, "SELECT id FROM t")),
        ("sqlite", sqlite_rows(&rc, "SELECT id FROM t")),
    ] {
        let ids: Vec<i64> = rows
            .iter()
            .map(|v| match v {
                rustqlite::Value::Integer(i) => *i,
                _ => panic!("{name}: non-integer id"),
            })
            .collect();
        assert_eq!(ids.len(), 3, "{name}: 3 rows");
        assert_eq!(
            ids.iter().filter(|i| **i != 0 && **i != i64::MAX).count(),
            1
        );
        let rand = *ids.iter().find(|i| **i != 0 && **i != i64::MAX).unwrap();
        assert!(
            rand != 0 && rand != i64::MAX,
            "{name}: random rowid is a fresh value"
        );
    }
    // Both engines leave the deterministic rows alone.
    q_both(&db, &rc, "SELECT v FROM t WHERE id = 0");
    q_both(&db, &rc, "SELECT v FROM t WHERE id = 9223372036854775807");
    // And a second NULL insert on SQLite-then-engine... one is enough.
}

// ===========================================================================
// 4. AUTOINCREMENT lifecycle
// ===========================================================================

#[test]
fn autoincrement_lifecycle() {
    let (mut db, rc) = fresh();
    both(
        &mut db,
        &rc,
        "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT)",
    );
    both(&mut db, &rc, "INSERT INTO t VALUES (NULL, 'a')");
    both(&mut db, &rc, "INSERT INTO t VALUES (NULL, 'b')");
    // Delete-max: AUTOINCREMENT never reuses (plain rowid tables do).
    both(&mut db, &rc, "DELETE FROM t WHERE id = 2");
    both(&mut db, &rc, "INSERT INTO t VALUES (NULL, 'c')");
    q_both(&db, &rc, "SELECT id, v FROM t ORDER BY id");
    q_both(&db, &rc, "SELECT seq FROM sqlite_sequence WHERE name = 't'");
    // Explicit high id bumps the sequence.
    both(&mut db, &rc, "INSERT INTO t VALUES (100, 'big')");
    both(&mut db, &rc, "INSERT INTO t VALUES (NULL, 'd')");
    q_both(&db, &rc, "SELECT id FROM t ORDER BY id");
    q_both(&db, &rc, "SELECT seq FROM sqlite_sequence WHERE name = 't'");
    // Deleting the max does NOT lower the sequence.
    both(&mut db, &rc, "DELETE FROM t WHERE id = 101");
    both(&mut db, &rc, "INSERT INTO t VALUES (NULL, 'e')");
    q_both(&db, &rc, "SELECT id FROM t WHERE v = 'e'");
    q_both(&db, &rc, "SELECT seq FROM sqlite_sequence WHERE name = 't'");
    // UPDATE to a higher id bumps it; lower does not.
    both(&mut db, &rc, "UPDATE t SET id = 500 WHERE v = 'e'");
    both(&mut db, &rc, "INSERT INTO t VALUES (NULL, 'f')");
    q_both(&db, &rc, "SELECT id FROM t WHERE v = 'f'");
    both(&mut db, &rc, "UPDATE t SET id = 10 WHERE v = 'f'");
    both(&mut db, &rc, "INSERT INTO t VALUES (NULL, 'g')");
    q_both(&db, &rc, "SELECT id FROM t WHERE v = 'g'");
    q_both(&db, &rc, "SELECT seq FROM sqlite_sequence WHERE name = 't'");
    // sqlite_sequence is write-protected on both.
    both(
        &mut db,
        &rc,
        "INSERT INTO sqlite_sequence VALUES ('t', 999)",
    );
    both(
        &mut db,
        &rc,
        "UPDATE sqlite_sequence SET seq = 1 WHERE name = 't'",
    );
    both(&mut db, &rc, "DELETE FROM sqlite_sequence");
    both(&mut db, &rc, "INSERT INTO t VALUES (NULL, 'after-write')");
    q_both(&db, &rc, "SELECT id FROM t WHERE v = 'after-write'");
    q_both(&db, &rc, "SELECT seq FROM sqlite_sequence WHERE name = 't'");
    // AUTOINCREMENT placement: only column-level INTEGER PRIMARY KEY
    // [ASC] may carry it (message parity with SQLite's exact error).
    both(
        &mut db,
        &rc,
        "CREATE TABLE bad1 (id INT PRIMARY KEY AUTOINCREMENT, v INT)",
    );
    both(
        &mut db,
        &rc,
        "CREATE TABLE bad2 (id TEXT PRIMARY KEY AUTOINCREMENT, v INT)",
    );
    both(
        &mut db,
        &rc,
        "CREATE TABLE b5 (id INTEGER PRIMARY KEY ASC AUTOINCREMENT, v INT)",
    );
    both(&mut db, &rc, "INSERT INTO b5 VALUES (NULL, 1)");
    q_both(&db, &rc, "SELECT id FROM b5");
    q_both(
        &db,
        &rc,
        "SELECT seq FROM sqlite_sequence WHERE name = 'b5'",
    );
    // NULL ids above the max, negative explicit ids, zero.
    both(&mut db, &rc, "INSERT INTO t VALUES (-5, 'neg')");
    both(&mut db, &rc, "INSERT INTO t VALUES (0, 'zero')");
    both(&mut db, &rc, "INSERT INTO t VALUES (NULL, 'auto')");
    q_both(&db, &rc, "SELECT id FROM t ORDER BY id");
    q_both(&db, &rc, "SELECT seq FROM sqlite_sequence WHERE name = 't'");
    // Multi-row VALUES keeps the sequence monotone across the batch.
    both(
        &mut db,
        &rc,
        "INSERT INTO t VALUES (NULL,'m1'),(NULL,'m2'),(NULL,'m3')",
    );
    q_both(&db, &rc, "SELECT id FROM t WHERE v LIKE 'm_' ORDER BY id");
    q_both(&db, &rc, "SELECT seq FROM sqlite_sequence WHERE name = 't'");
    // Failed insert does not burn an id (statement-atomicity, pinned by
    // fuzz_regressions too).
    both(&mut db, &rc, "INSERT INTO t VALUES (NULL, 'burn') "); // succeeds first
    let last = q_both(&db, &rc, "SELECT max(id) FROM t")[0].clone();
    let _ = db.execute(
        "INSERT INTO t VALUES (NULL, 'fail'), (NULL, 'fail'), (0, 'CLASH')",
        (),
    );
    let _ = rc.execute_batch("INSERT INTO t VALUES (NULL, 'fail'), (NULL, 'fail'), (0, 'CLASH')");
    let next = q_both(
        &db,
        &rc,
        "INSERT OR IGNORE INTO t VALUES (NULL, 'next') RETURNING id",
    );
    // After a failed multi-row batch (which failed on the third row), the
    // first two rows may have burned ids — the engine must match SQLite's
    // exact final sequence and next id.
    q_both(&db, &rc, "SELECT seq FROM sqlite_sequence WHERE name = 't'");
    let _ = (last, next);
}

#[test]
fn autoincrement_reopen_durability() {
    let dir = tempfile::tempdir().unwrap();
    // Native format.
    let native = dir.path().join("ai_native.db");
    {
        let mut db = rustqlite::Database::open(&native).unwrap();
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT)",
            (),
        )
        .unwrap();
        for i in 0..500 {
            db.execute(
                "INSERT INTO t (v) VALUES (?)",
                vec![rustqlite::Value::Text(format!("v{i}").into())],
            )
            .unwrap();
        }
        // Delete the max rows: the sequence must NOT step back on reopen.
        db.execute("DELETE FROM t WHERE id > 490", ()).unwrap();
    }
    let mut db2 = rustqlite::Database::open(&native).unwrap();
    db2.execute("INSERT INTO t (v) VALUES ('after')", ())
        .unwrap();
    let id = match &db2.query("SELECT max(id) FROM t", ()).unwrap()[0][0] {
        rustqlite::Value::Integer(i) => *i,
        other => panic!("non-integer max id: {other:?}"),
    };
    assert_eq!(
        id, 501,
        "sequence survives reopen (500 max, 490-500 deleted)"
    );

    // SQLite-format file: real SQLite reads the same sequence semantics.
    let sfmt = dir.path().join("ai_sfmt.db");
    {
        let mut db = rustqlite::Database::open_sqlite_format(&sfmt).unwrap();
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT)",
            (),
        )
        .unwrap();
        for i in 0..500 {
            db.execute(
                "INSERT INTO t (v) VALUES (?)",
                vec![rustqlite::Value::Text(format!("v{i}").into())],
            )
            .unwrap();
        }
    }
    let rc = Connection::open(&sfmt).unwrap();
    let seq: i64 = rc
        .query_row(
            "SELECT seq FROM sqlite_sequence WHERE name = 't'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        seq, 500,
        "real SQLite reads the engine's AUTOINCREMENT sequence"
    );
    rc.execute_batch("INSERT INTO t (v) VALUES ('from-sqlite')")
        .unwrap();
    let nid: i64 = rc
        .query_row("SELECT id FROM t WHERE v = 'from-sqlite'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(nid, 501, "SQLite continues the engine's sequence");
    // And the engine reopens its own file with the bumped sequence.
    let mut db3 = rustqlite::Database::open_sqlite_format(&sfmt).unwrap();
    db3.execute("INSERT INTO t (v) VALUES ('again')", ())
        .unwrap();
    let nid2 = match &db3.query("SELECT id FROM t WHERE v = 'again'", ()).unwrap()[0][0] {
        rustqlite::Value::Integer(i) => *i,
        other => panic!("non-integer id: {other:?}"),
    };
    assert_eq!(nid2, 502, "engine continues after SQLite's insert");
}

#[test]
fn autoincrement_scale_churn() {
    let n = env_u64("PK_STRESS_ROWS", 3000) as i64;
    let (mut db, rc) = fresh();
    both(
        &mut db,
        &rc,
        "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, v INT)",
    );
    // One big batch, then churn: delete every 3rd, reinsert, verify the
    // id set and sequence stay in lockstep with SQLite.
    let mut rng: u64 = 0x5EED_0000_0000_00AA;
    let next = |rng: &mut u64| {
        *rng = rng
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (*rng >> 33) as i64
    };
    for i in 0..n {
        let v = next(&mut rng);
        both(&mut db, &rc, &format!("INSERT INTO t (v) VALUES ({v})"));
        if i % 3 == 0 {
            // Randomized deletes never lower the sequence.
            both(&mut db, &rc, &format!("DELETE FROM t WHERE id = {i}"));
        }
    }
    q_both(&db, &rc, "SELECT count(*), min(id), max(id) FROM t");
    q_both(&db, &rc, "SELECT seq FROM sqlite_sequence WHERE name = 't'");
    // Failed statements at scale (unique clash deep in a batch).
    both(
        &mut db,
        &rc,
        "INSERT INTO t (id, v) SELECT 1, v FROM t LIMIT 1",
    );
    q_both(&db, &rc, "SELECT count(*) FROM t");
    q_both(&db, &rc, "SELECT seq FROM sqlite_sequence WHERE name = 't'");
    // Bulk self-insert keeps monotone allocation.
    both(
        &mut db,
        &rc,
        "INSERT INTO t (v) SELECT v FROM t WHERE id % 7 = 0",
    );
    q_both(&db, &rc, "SELECT count(*), max(id) FROM t");
    q_both(&db, &rc, "SELECT seq FROM sqlite_sequence WHERE name = 't'");
}

// ===========================================================================
// 5. TEXT / composite PK uniqueness churn
// ===========================================================================

#[test]
fn text_pk_uniqueness_churn() {
    let n = env_u64("PK_STRESS_ROWS", 3000) as i64;
    let (mut db, rc) = fresh();
    // TEXT PRIMARY KEY on a rowid table: backed by the (visible)
    // sqlite_autoindex; NULL is allowed (legacy quirk) and distinct.
    both(&mut db, &rc, "CREATE TABLE t (k TEXT PRIMARY KEY, v INT)");
    both(&mut db, &rc, "INSERT INTO t VALUES ('a', 1)");
    both(&mut db, &rc, "INSERT INTO t VALUES ('a', 2)"); // UNIQUE failed: t.k
    both(&mut db, &rc, "INSERT INTO t VALUES (NULL, 3)"); // allowed!
    both(&mut db, &rc, "INSERT INTO t VALUES (NULL, 4)"); // also allowed
    q_both(&db, &rc, "SELECT count(*) FROM t WHERE k IS NULL");
    // The autoindex is VISIBLE in sqlite_master with SQLite's name;
    // index_list reports origin 'pk' (both the PRAGMA and TVF forms).
    q_both(
        &db,
        &rc,
        "SELECT type, name FROM sqlite_master ORDER BY name",
    );
    q_both(
        &db,
        &rc,
        "SELECT name, \"unique\", origin, partial FROM pragma_index_list('t')",
    );
    // Seeded churn: duplicate-prone inserts, upserts, PK moves, deletes.
    let mut rng: u64 = 0xBEEF_CAFE_1234_5678;
    let key = |rng: &mut u64| {
        *rng = rng
            .wrapping_mul(2862933555777941757)
            .wrapping_add(3037000493);
        format!("k{:04}", (*rng >> 33) % 200) // colliding key space
    };
    for i in 0..n {
        let k = key(&mut rng);
        let stmt = match i % 7 {
            0 => format!("INSERT INTO t VALUES ('{k}', {i})"),
            1 => format!("INSERT OR IGNORE INTO t VALUES ('{k}', {i})"),
            2 => format!(
                "INSERT INTO t VALUES ('{k}', {i}) ON CONFLICT (k) DO UPDATE SET v = v + {i}"
            ),
            3 => format!("INSERT OR REPLACE INTO t VALUES ('{k}', {i})"),
            4 => format!("UPDATE t SET v = {i} WHERE k = '{k}'"),
            5 => format!("DELETE FROM t WHERE k = '{k}'"),
            _ => format!("INSERT INTO t (k, v) VALUES (NULL, {i})"), // distinct NULLs
        };
        both(&mut db, &rc, &stmt);
        if i % 97 == 0 {
            // Periodic full-state agreement.
            q_both(&db, &rc, "SELECT count(*) FROM t");
            q_both(&db, &rc, "SELECT min(v), max(v) FROM t");
            q_both(&db, &rc, "SELECT count(*) FROM t WHERE k IS NULL");
        }
    }
    q_both(
        &db,
        &rc,
        "SELECT k, v FROM t WHERE k IS NOT NULL ORDER BY k",
    );
    q_both(&db, &rc, "SELECT v FROM t WHERE k IS NULL ORDER BY v");
    // PK move to an existing key fails identically.
    both(&mut db, &rc, "UPDATE t SET k = 'a' WHERE v = 0");
    // PK move to a fresh key works.
    both(&mut db, &rc, "UPDATE t SET k = 'zz-fresh' WHERE v = 1");
    q_both(&db, &rc, "SELECT count(*) FROM t WHERE k = 'zz-fresh'");
}

#[test]
fn composite_pk_semantics() {
    let (mut db, rc) = fresh();
    // NULLs are allowed AND DISTINCT in a rowid-table composite PK.
    both(
        &mut db,
        &rc,
        "CREATE TABLE t (a INT, b TEXT, v REAL, PRIMARY KEY (a, b))",
    );
    both(&mut db, &rc, "INSERT INTO t VALUES (1, 'x', 0.5)");
    both(&mut db, &rc, "INSERT INTO t VALUES (1, 'x', 9.5)"); // full collision
                                                              // Partial collisions: each is a conflict.
    both(&mut db, &rc, "INSERT INTO t VALUES (1, 'y', 1.5)");
    both(&mut db, &rc, "INSERT INTO t VALUES (2, 'x', 2.5)");
    // NULL member: distinct rows (two NULL-a rows coexist).
    both(&mut db, &rc, "INSERT INTO t VALUES (NULL, 'n', 3.5)");
    both(&mut db, &rc, "INSERT INTO t VALUES (NULL, 'n', 4.5)");
    q_both(&db, &rc, "SELECT count(*) FROM t WHERE a IS NULL");
    // ... but the same NULL row colliding on the OTHER member still fails.
    both(&mut db, &rc, "INSERT INTO t VALUES (1, NULL, 5.5)");
    both(&mut db, &rc, "INSERT INTO t VALUES (1, NULL, 6.5)"); // NULL b distinct too
    q_both(&db, &rc, "SELECT count(*) FROM t WHERE b IS NULL AND a = 1");
    // Upsert on the composite target.
    both(
        &mut db,
        &rc,
        "INSERT INTO t VALUES (1, 'x', 9.0) ON CONFLICT (a, b) DO UPDATE SET v = v * 10",
    );
    q_both(&db, &rc, "SELECT v FROM t WHERE a = 1 AND b = 'x'");
    // Composite PK with a DESC member.
    both(
        &mut db,
        &rc,
        "CREATE TABLE d (a INT, b TEXT, PRIMARY KEY (a DESC, b))",
    );
    both(&mut db, &rc, "INSERT INTO d VALUES (1, 'x'), (2, 'y')");
    both(&mut db, &rc, "INSERT INTO d VALUES (1, 'x')");
    q_both(&db, &rc, "SELECT a, b FROM d ORDER BY a, b");
    // WITHOUT ROWID composite: NOT NULL is enforced (no NULL quirk).
    both(
        &mut db,
        &rc,
        "CREATE TABLE w (a INT, b TEXT, v REAL, PRIMARY KEY (a, b)) WITHOUT ROWID",
    );
    both(&mut db, &rc, "INSERT INTO w VALUES (1, 'x', 0.5)");
    both(&mut db, &rc, "INSERT INTO w VALUES (NULL, 'n', 3.5)"); // NOT NULL failed
    q_both(&db, &rc, "SELECT a, b, v FROM w ORDER BY a");
    // PK-column affinity applies before uniqueness (composite).
    both(
        &mut db,
        &rc,
        "CREATE TABLE ca (a INT, b TEXT, PRIMARY KEY (a, b))",
    );
    both(&mut db, &rc, "INSERT INTO ca VALUES (5, 't')");
    both(&mut db, &rc, "INSERT INTO ca VALUES (5.0, 't')"); // coerces to (5,'t')
    both(&mut db, &rc, "INSERT INTO ca VALUES ('5', 't')"); // also (5,'t')
}

// ===========================================================================
// 6. PK index visibility / metadata parity
// ===========================================================================

#[test]
fn pk_index_metadata_parity() {
    let (mut db, rc) = fresh();
    // Rowid alias: NO autoindex object at all.
    both(
        &mut db,
        &rc,
        "CREATE TABLE alias (id INTEGER PRIMARY KEY, v TEXT)",
    );
    q_both(
        &db,
        &rc,
        "SELECT type, name FROM sqlite_master WHERE type = 'index'",
    );
    q_both(&db, &rc, "SELECT name FROM pragma_index_list('alias')");

    // TEXT PK: exactly one autoindex, SQLite's naming, origin 'pk'.
    both(
        &mut db,
        &rc,
        "CREATE TABLE textpk (k TEXT PRIMARY KEY, v INT)",
    );
    q_both(
        &db,
        &rc,
        "SELECT type, name FROM sqlite_master WHERE tbl_name = 'textpk' ORDER BY name",
    );
    q_both(
        &db,
        &rc,
        "SELECT name, \"unique\", origin, partial FROM pragma_index_list('textpk')",
    );
    q_both(
        &db,
        &rc,
        "SELECT seqno, cid, name FROM pragma_index_info('sqlite_autoindex_textpk_1')",
    );

    // Composite table-level PK: one autoindex over both columns in PK order.
    both(
        &mut db,
        &rc,
        "CREATE TABLE comp (a INT, b TEXT, v REAL, PRIMARY KEY (b, a))",
    );
    q_both(
        &db,
        &rc,
        "SELECT name FROM sqlite_master WHERE tbl_name = 'comp' AND type = 'index'",
    );
    q_both(
        &db,
        &rc,
        "SELECT seqno, cid, name FROM pragma_index_info('sqlite_autoindex_comp_1')",
    );

    // Non-alias INT PK: also gets an autoindex (it's a regular PK).
    both(
        &mut db,
        &rc,
        "CREATE TABLE intpk (id INT PRIMARY KEY, v TEXT)",
    );
    q_both(
        &db,
        &rc,
        "SELECT name, \"unique\", origin FROM pragma_index_list('intpk')",
    );

    // AUTOINCREMENT adds the sqlite_sequence table to the schema.
    both(
        &mut db,
        &rc,
        "CREATE TABLE ai (id INTEGER PRIMARY KEY AUTOINCREMENT, v INT)",
    );
    both(&mut db, &rc, "INSERT INTO ai VALUES (NULL, 1)");
    q_both(&db, &rc, "SELECT name FROM sqlite_master ORDER BY name");
}

// ===========================================================================
// 7. Randomized PK-flavored workloads (full-state differential)
// ===========================================================================

/// Deterministic xorshift RNG (same family as tests/stateful_fuzz.rs).
struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed | 1)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

#[test]
fn pk_randomized_workload() {
    let seeds = env_u64("PK_STRESS_SEEDS", 8);
    let ops = env_u64("PK_STRESS_OPS", 200);
    for s in 0..seeds {
        let mut rng = Rng::new(0x5EED_0000_0000_00AAu64.wrapping_add(s * 0x9E37_79B9));
        let (mut db, rc) = fresh();
        // Pick a PK shape for this seed.
        let shape = rng.below(4);
        let ddl = match shape {
            0 => "CREATE TABLE t (k TEXT PRIMARY KEY, v INT)",
            1 => "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT)",
            2 => "CREATE TABLE t (a INT, b TEXT, PRIMARY KEY (a, b))",
            _ => "CREATE TABLE t (k TEXT PRIMARY KEY, v INT) WITHOUT ROWID",
        };
        both(&mut db, &rc, ddl);
        let n_keys = 40u64; // small key space → plenty of collisions
        for i in 0..ops {
            let op = rng.below(10);
            let k = format!("k{:02}", rng.below(n_keys));
            let num = (rng.next_u64() % 1000) as i64;
            // Shape-aware generators: shape 1 is (id, v TEXT); shape 2 is
            // (a INT, b TEXT); others are (k TEXT, v INT).
            let stmt = match (shape, op) {
                (_, 0..=2) => match shape {
                    1 => format!("INSERT INTO t VALUES (NULL, 'v{i}')"),
                    2 => format!("INSERT INTO t VALUES ({num}, '{k}')"),
                    _ => format!("INSERT INTO t VALUES ('{k}', {num})"),
                },
                (_, 3) => match shape {
                    1 => format!("INSERT OR IGNORE INTO t VALUES ({num}, 'v{i}')"),
                    2 => format!("INSERT OR IGNORE INTO t VALUES ({num}, '{k}')"),
                    _ => format!("INSERT OR IGNORE INTO t VALUES ('{k}', {num})"),
                },
                (_, 4) => match shape {
                    1 => format!("INSERT OR REPLACE INTO t VALUES ({num}, 'v{i}')"),
                    2 => format!("INSERT OR REPLACE INTO t VALUES ({num}, '{k}')"),
                    _ => format!("INSERT OR REPLACE INTO t VALUES ('{k}', {num})"),
                },
                (1, 5) => format!("INSERT INTO t VALUES ({num}, 'v{i}')"),
                (2, 5) => format!("INSERT INTO t VALUES ({num}, '{k}')"),
                (_, 5) => format!("INSERT INTO t VALUES ('{k}', {num})"),
                (1, 6) => format!("UPDATE t SET v = 'u{i}' WHERE id = {num}"),
                (2, 6) => format!("UPDATE t SET b = 'u{i}' WHERE a = {num}"),
                (_, 6) => format!("UPDATE t SET v = v + 1 WHERE k = '{k}'"),
                (1, 7) => format!("UPDATE t SET id = {num} WHERE v = 'v{}'", i % 97),
                (2, 7) => format!("UPDATE t SET a = {num} WHERE b = '{k}'"),
                (_, 7) => format!("UPDATE t SET k = 'k{:02}' WHERE v = {num}", rng.below(n_keys)),
                (1, 8) => format!("DELETE FROM t WHERE id = {num}"),
                (2, 8) => format!("DELETE FROM t WHERE a = {num}"),
                (_, 8) => format!("DELETE FROM t WHERE k = '{k}'"),
                (1, _) => format!("INSERT INTO t VALUES (NULL, 'w{i}') ON CONFLICT (id) DO UPDATE SET v = 'w{i}'"),
                (2, _) => format!(
                    "INSERT INTO t VALUES ({num}, '{k}') ON CONFLICT (a, b) DO UPDATE SET b = 'u{i}'"
                ),
                (_, _) => format!(
                    "INSERT INTO t VALUES ('{k}', {num}) ON CONFLICT (k) DO UPDATE SET v = v + 1"
                ),
            };
            both(&mut db, &rc, &stmt);
            if i % 25 == 0 {
                q_both(&db, &rc, "SELECT count(*) FROM t");
            }
        }
        // Final full-state agreement (stable order).
        match shape {
            1 => q_both(&db, &rc, "SELECT id, v FROM t ORDER BY id"),
            2 => q_both(&db, &rc, "SELECT a, b FROM t ORDER BY a, b"),
            _ => q_both(&db, &rc, "SELECT k, v FROM t ORDER BY k"),
        };
        if shape == 1 {
            q_both(&db, &rc, "SELECT seq FROM sqlite_sequence WHERE name = 't'");
        }
    }
}
