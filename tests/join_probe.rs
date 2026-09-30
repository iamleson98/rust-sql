//! Regression: a WHERE-filtered Index Nested-Loop Join under an
//! EXPRESSION projection must not lose the projection.
//!
//! Found via pdf-tts's migration backfill:
//! `INSERT INTO layout_block_audio (...) SELECT b.id,
//! p.document_id || '-legacy', b.audio_url, COALESCE(b.created_at,
//! datetime('now')) FROM layout_blocks b JOIN pages p ON b.page_id =
//! p.id WHERE b.audio_url IS NOT NULL` — the planner picks INLJ when the
//! outer is filtered (the WHERE), and the fused Project-over-INLJ path
//! used to emit the raw combined rows (ALL columns of both tables) and
//! silently drop the projection whenever it contained expressions,

use rustqlite::{Database, Value};

#[test]
fn inlj_expression_projection_is_applied() {
    use rustqlite::Database;
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE pages (id TEXT PRIMARY KEY, document_id TEXT, page_number INTEGER, processed INTEGER, created_at TEXT)",
        [],
    )
    .unwrap();
    db.execute(
        "CREATE TABLE layout_blocks (id TEXT PRIMARY KEY, page_id TEXT, block_type TEXT, x1 REAL, y1 REAL, x2 REAL, y2 REAL, confidence REAL, text TEXT, image_url TEXT, audio_url TEXT, end_with_hyphen_continued INTEGER, order_index INTEGER, created_at TEXT)",
        [],
    )
    .unwrap();
    db.execute("INSERT INTO pages VALUES ('p1','d1',1,1,'t')", [])
        .unwrap();
    db.execute(
        "INSERT INTO layout_blocks VALUES ('b1','p1','paragraph',0,0,1,1,1.0,'x',NULL,'au',0,0,'t')",
        [],
    )
    .unwrap();

    // The backfill shape: WHERE (outer selective) + expression projection.
    let rows = db
        .query(
            "SELECT b.id, p.document_id || '-legacy', b.audio_url, COALESCE(b.created_at, datetime('now')) \
             FROM layout_blocks b JOIN pages p ON b.page_id = p.id \
             WHERE b.audio_url IS NOT NULL",
            [],
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].len(),
        4,
        "the SELECT-list projection must be applied, got {} columns",
        rows[0].len()
    );
    assert_eq!(rows[0][0].to_string(), "b1");
    assert_eq!(rows[0][1].to_string(), "d1-legacy");
    assert_eq!(rows[0][2].to_string(), "au");

    // INSERT ... SELECT on the same shape (what the migration does).
    db.execute(
        "CREATE TABLE layout_block_audio (block_id TEXT NOT NULL, version_id TEXT NOT NULL, audio_url TEXT NOT NULL, created_at TEXT NOT NULL, PRIMARY KEY (block_id, version_id))",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO layout_block_audio (block_id, version_id, audio_url, created_at) \
         SELECT b.id, p.document_id || '-legacy', b.audio_url, COALESCE(b.created_at, datetime('now')) \
         FROM layout_blocks b JOIN pages p ON b.page_id = p.id \
         WHERE b.audio_url IS NOT NULL",
        [],
    )
    .unwrap();
    let n = db
        .query("SELECT COUNT(*) FROM layout_block_audio", [])
        .unwrap();
    assert_eq!(n[0][0].to_string(), "1");

    // Bare-column projection over the same filtered join still takes the
    // fused fast path and stays correct.
    let rows = db
        .query(
            "SELECT b.id, p.document_id FROM layout_blocks b JOIN pages p ON b.page_id = p.id \
             WHERE b.audio_url IS NOT NULL",
            [],
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].len(), 2);
}

// ---------------------------------------------------------------------------
// COUNT(*) over a hash join — the fused COUNT-ONLY mode (fast path #3e).
// The join's cardinality is the answer: no output rows are built. Every
// edge the fused machine declines (outer joins, residual predicates,
// NULL/cross-type keys, non-scan sides) falls to the general path and
// must produce the SAME count.
// ---------------------------------------------------------------------------

#[test]
fn count_over_hash_join_matches_general_path() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE f1 (id INTEGER PRIMARY KEY, x INT, t TEXT)",
        [],
    )
    .unwrap();
    db.execute(
        "CREATE TABLE f2 (id INTEGER PRIMARY KEY, x INT, u TEXT)",
        [],
    )
    .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=2_000i64 {
        db.execute(
            "INSERT INTO f1 (x, t) VALUES (?, 't')",
            [Value::Integer(i % 37)],
        )
        .unwrap();
        db.execute(
            "INSERT INTO f2 (x, u) VALUES (?, 'u')",
            [Value::Integer(i % 37)],
        )
        .unwrap();
    }
    // NULLs, REALs, cross-type duplicates, and a TEXT key (never matches
    // the numeric build keys) — the gates + 3VL classes the count must
    // reproduce exactly.
    db.execute("INSERT INTO f1 (x, t) VALUES (NULL, 'n')", [])
        .unwrap();
    db.execute("INSERT INTO f2 (x, u) VALUES (NULL, 'n')", [])
        .unwrap();
    db.execute("INSERT INTO f1 (x, t) VALUES (2.0, 'r')", [])
        .unwrap();
    db.execute("INSERT INTO f2 (x, u) VALUES (2, 'i')", [])
        .unwrap();
    db.execute("INSERT INTO f1 (x, t) VALUES ('2', 's')", [])
        .unwrap();
    db.execute("COMMIT", []).unwrap();

    let fused = db
        .query("SELECT COUNT(*) FROM f1 JOIN f2 ON f1.x = f2.x", [])
        .unwrap();
    // The same cardinality through a shape the fused machine must
    // DECLINE (a residual predicate in the condition) — the general
    // path answers, and the two must agree.
    let general = db
        .query(
            "SELECT COUNT(*) FROM f1 JOIN f2 ON f1.x = f2.x AND f1.id > 0",
            [],
        )
        .unwrap();
    assert_eq!(fused, general);
    // And against the implicit-join spelling.
    let implicit = db
        .query("SELECT COUNT(*) FROM f1, f2 WHERE f1.x = f2.x", [])
        .unwrap();
    assert_eq!(fused, implicit);

    // Differential: real SQLite agrees.
    let oracle = rusqlite::Connection::open_in_memory().unwrap();
    oracle
        .execute_batch(
            "CREATE TABLE f1 (id INTEGER PRIMARY KEY, x INT, t TEXT);
             CREATE TABLE f2 (id INTEGER PRIMARY KEY, x INT, u TEXT);",
        )
        .unwrap();
    for i in 1..=2_000i64 {
        oracle
            .execute("INSERT INTO f1 (x, t) VALUES (?, 't')", (i % 37,))
            .unwrap();
        oracle
            .execute("INSERT INTO f2 (x, u) VALUES (?, 'u')", (i % 37,))
            .unwrap();
    }
    oracle
        .execute("INSERT INTO f1 (x, t) VALUES (NULL, 'n')", [])
        .unwrap();
    oracle
        .execute("INSERT INTO f2 (x, u) VALUES (NULL, 'n')", [])
        .unwrap();
    oracle
        .execute("INSERT INTO f1 (x, t) VALUES (2.0, 'r')", [])
        .unwrap();
    oracle
        .execute("INSERT INTO f2 (x, u) VALUES (2, 'i')", [])
        .unwrap();
    oracle
        .execute("INSERT INTO f1 (x, t) VALUES ('2', 's')", [])
        .unwrap();
    let n: i64 = oracle
        .query_row("SELECT COUNT(*) FROM f1 JOIN f2 ON f1.x = f2.x", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(fused[0][0].as_integer(), n, "vs real SQLite");
}

#[test]
fn count_over_hash_join_edges() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE a (k INT)", []).unwrap();
    db.execute("CREATE TABLE b (k INT)", []).unwrap();
    // Empty sides.
    assert_eq!(
        db.query("SELECT COUNT(*) FROM a JOIN b ON a.k = b.k", [])
            .unwrap(),
        vec![vec![Value::Integer(0)]]
    );
    // Multi-key.
    db.execute("CREATE TABLE m1 (p INT, q INT)", []).unwrap();
    db.execute("CREATE TABLE m2 (p INT, q INT)", []).unwrap();
    db.execute("INSERT INTO m1 VALUES (1,1),(1,2),(2,1),(NULL,1)", [])
        .unwrap();
    db.execute(
        "INSERT INTO m2 VALUES (1,1),(1,1),(2,1),(1,NULL),(NULL,1)",
        [],
    )
    .unwrap();
    let got = db
        .query(
            "SELECT COUNT(*) FROM m1 JOIN m2 ON m1.p = m2.p AND m1.q = m2.q",
            [],
        )
        .unwrap();
    // (1,1)x2 + (2,1)x1 + (NULL,1) never matches = 3.
    assert_eq!(got, vec![vec![Value::Integer(3)]]);
    // Large rowids beyond 2^53: the build side aborts the fused path —
    // the general path answers with exact semantics.
    db.execute("CREATE TABLE l1 (id INTEGER PRIMARY KEY, k INT)", [])
        .unwrap();
    db.execute("CREATE TABLE l2 (id INTEGER PRIMARY KEY, k INT)", [])
        .unwrap();
    let big = (1i64 << 54) + 1;
    db.execute(
        "INSERT INTO l1 (id, k) VALUES (?, 1)",
        [Value::Integer(big)],
    )
    .unwrap();
    db.execute(
        "INSERT INTO l2 (id, k) VALUES (?, 1)",
        [Value::Integer(big + 1)],
    )
    .unwrap();
    // rowid-alias join: distinct ids beyond 2^53 must NOT match.
    assert_eq!(
        db.query("SELECT COUNT(*) FROM l1 JOIN l2 ON l1.id = l2.id", [])
            .unwrap(),
        vec![vec![Value::Integer(0)]]
    );
    // k-join: both match on k=1.
    assert_eq!(
        db.query("SELECT COUNT(*) FROM l1 JOIN l2 ON l1.k = l2.k", [])
            .unwrap(),
        vec![vec![Value::Integer(1)]]
    );
}
