//! Regression: correlated scalar subqueries over INDEXED composite-PK
//! tables (the pdf-tts mirror-sync shapes).
//!
//! Two engine defects found via pdf-tts's `sync_default_version_audio`:
//!
//! 1. The index/rowid lookup executors emitted BARE `table.col_names`,
//!    dropping the FROM alias — qualified refs (`lba.audio_url`) missed
//!    the local layout and fell through to the correlated outer-scope
//!    resolver, whose bare-suffix pass happily bound the OUTER table's
//!    same-named column (`layout_blocks.audio_url`): a silent outer
//!    VALUE leak that wrote NULLs into the mirror column.
//! 2. `lookup_outer`'s qualified-reference Pass 2 suffix-matched
//!    QUALIFIED frame names — `lba.audio_url` could bind an outer
//!    `layout_blocks.audio_url`. Removed; exact "qual.name" (Pass 1) and
//!    bare frames are the only legal binds for a qualified ref.
//!
//! Both are pinned here through the execute path AND the
//! prepare/bind/step statement path, over the exact pdf-tts schema
//! (composite TEXT PK + ALTER TABLE ADD COLUMN + an explicit
//! single-column index — the planner's IndexLookup triggers).

use rustqlite::{Database, StepResult, Value};

fn setup() -> Database {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE layout_blocks (id TEXT PRIMARY KEY, page_id TEXT, audio_url TEXT, created_at TEXT)",
        [],
    )
    .unwrap();
    db.execute(
        "CREATE TABLE layout_block_audio (block_id TEXT NOT NULL, version_id TEXT NOT NULL, \
         audio_url TEXT NOT NULL, created_at TEXT NOT NULL, PRIMARY KEY (block_id, version_id))",
        [],
    )
    .unwrap();
    db.execute(
        "ALTER TABLE layout_block_audio ADD COLUMN merged_into TEXT NULL",
        [],
    )
    .unwrap();
    db.execute(
        "CREATE INDEX idx_layout_block_audio_version_id ON layout_block_audio (version_id)",
        [],
    )
    .unwrap();
    for sql in [
        "INSERT INTO layout_blocks VALUES ('d1-b0', 'd1-p1', 'audio/d1/d1-b0.opus', 't')",
        "INSERT INTO layout_blocks VALUES ('d1-b1', 'd1-p1', NULL, 't')",
        "INSERT INTO layout_block_audio VALUES ('d1-b0', 'ver-a', 'audio/d1/d1-b0.opus', 't', NULL)",
        "INSERT INTO layout_block_audio VALUES ('d1-b1', 'ver-b', 'audio/d1/ver-b/d1-b1.opus', 't', NULL)",
    ] {
        db.execute(sql, []).unwrap();
    }
    db
}

fn scalar(db: &Database, sql: &str) -> Value {
    let rows = db.query(sql, []).unwrap();
    rows.first()
        .and_then(|r| r.first().cloned())
        .unwrap_or(Value::Null)
}

#[test]
fn correlated_case_projection_over_index_lookup() {
    let db = setup();
    // The exact mirror-sync subquery: CASE over the ALTERed column, both
    // access paths on the composite PK, explicit index present. Must NOT
    // leak the outer layout_blocks.audio_url (NULL for d1-b1).
    let v = scalar(
        &db,
        "SELECT (SELECT CASE WHEN \"lba\".\"merged_into\" IS NOT NULL THEN NULL ELSE \"lba\".\"audio_url\" END \
         FROM \"layout_block_audio\" AS \"lba\" \
         WHERE \"lba\".\"block_id\" = \"layout_blocks\".\"id\" AND \"lba\".\"version_id\" = 'ver-b') AS x \
         FROM \"layout_blocks\" WHERE \"id\" = 'd1-b1'",
    );
    assert_eq!(
        v,
        Value::Text("audio/d1/ver-b/d1-b1.opus".into()),
        "the subquery must read lba.audio_url, not the outer layout_blocks.audio_url"
    );
}

#[test]
fn correlated_shapes_matrix() {
    let db = setup();
    let q = |sql: &str| scalar(&db, sql);

    // Bare projection (the fused fast path).
    assert_eq!(
        q("SELECT (SELECT \"lba\".\"audio_url\" FROM \"layout_block_audio\" AS \"lba\" \
           WHERE \"lba\".\"block_id\" = \"layout_blocks\".\"id\" AND \"lba\".\"version_id\" = 'ver-b') \
           FROM \"layout_blocks\" WHERE \"id\" = 'd1-b1'"),
        Value::Text("audio/d1/ver-b/d1-b1.opus".into())
    );
    // CASE over a NON-altered column.
    assert_eq!(
        q("SELECT (SELECT CASE WHEN \"lba\".\"created_at\" IS NOT NULL THEN \"lba\".\"audio_url\" END \
           FROM \"layout_block_audio\" AS \"lba\" \
           WHERE \"lba\".\"block_id\" = \"layout_blocks\".\"id\" AND \"lba\".\"version_id\" = 'ver-b') \
           FROM \"layout_blocks\" WHERE \"id\" = 'd1-b1'"),
        Value::Text("audio/d1/ver-b/d1-b1.opus".into())
    );
    // Correlated ref on the INDEXED column (the index key side).
    // version_id = outer.id = 'd1-b1' — no such row: NULL is CORRECT
    // (pins that the fix did not start over-matching either).
    assert_eq!(
        q("SELECT (SELECT \"lba\".\"audio_url\" FROM \"layout_block_audio\" AS \"lba\" \
           WHERE \"lba\".\"version_id\" = \"layout_blocks\".\"id\" AND \"lba\".\"block_id\" = 'd1-b1') \
           FROM \"layout_blocks\" WHERE \"id\" = 'd1-b1'"),
        Value::Null
    );
    // Correlated over BOTH outer rows in one query.
    let rows = db
        .query(
            "SELECT \"id\", (SELECT \"lba\".\"audio_url\" FROM \"layout_block_audio\" AS \"lba\" \
             WHERE \"lba\".\"block_id\" = \"layout_blocks\".\"id\" AND \"lba\".\"version_id\" = 'ver-b') \
             FROM \"layout_blocks\" ORDER BY \"id\"",
            [],
        )
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][1], Value::Null); // b0: no ver-b row
    assert_eq!(rows[1][1], Value::Text("audio/d1/ver-b/d1-b1.opus".into())); // b1
}

#[test]
fn mirror_sync_update_through_statement_path() {
    let mut db = setup();
    let sync_sql = "UPDATE \"layout_blocks\" SET \"audio_url\" = \
        (SELECT CASE WHEN \"lba\".\"merged_into\" IS NOT NULL THEN NULL ELSE \"lba\".\"audio_url\" END \
         FROM \"layout_block_audio\" AS \"lba\" \
         WHERE \"lba\".\"block_id\" = \"layout_blocks\".\"id\" AND \"lba\".\"version_id\" = ?1) \
        WHERE \"page_id\" IN (?2)";

    let step_all = |db: &mut Database, sql: &str, params: &[Value]| {
        let mut stmt = db.prepare(sql).unwrap();
        stmt.bind_all(params).unwrap();
        while stmt.step().unwrap() == StepResult::Row {}
    };

    // sync(a): b0 gets its ver-a URL; b1 has no ver-a row → NULL.
    step_all(
        &mut db,
        sync_sql,
        &[Value::Text("ver-a".into()), Value::Text("d1-p1".into())],
    );
    assert_eq!(
        scalar(&db, "SELECT audio_url FROM layout_blocks WHERE id='d1-b0'"),
        Value::Text("audio/d1/d1-b0.opus".into())
    );
    assert_eq!(
        scalar(&db, "SELECT audio_url FROM layout_blocks WHERE id='d1-b1'"),
        Value::Null
    );

    // sync(b): b0 has no ver-b row → NULL; b1 mirrors its ver-b URL.
    // Fresh SQL text (trailing space) defeats statement caching.
    step_all(
        &mut db,
        &format!("{sync_sql} "),
        &[Value::Text("ver-b".into()), Value::Text("d1-p1".into())],
    );
    assert_eq!(
        scalar(&db, "SELECT audio_url FROM layout_blocks WHERE id='d1-b0'"),
        Value::Null
    );
    assert_eq!(
        scalar(&db, "SELECT audio_url FROM layout_blocks WHERE id='d1-b1'"),
        Value::Text("audio/d1/ver-b/d1-b1.opus".into()),
        "sync(b) must mirror b1's URL through the statement path"
    );
}

/// The same leak through the CORRELATED-IN and EXISTS shapes: the inner
/// alias-qualified predicate must bind INNER rows only.
#[test]
fn correlated_exists_and_in_do_not_leak_outer_columns() {
    let db = setup();
    // layout_blocks also has an audio_url column — the leak would make
    // this EXISTS true for d1-b1 via the OUTER's (NULL) value binding.
    let v = scalar(
        &db,
        "SELECT EXISTS (SELECT 1 FROM \"layout_block_audio\" AS \"lba\" \
         WHERE \"lba\".\"block_id\" = \"layout_blocks\".\"id\" AND \"lba\".\"audio_url\" = 'nope') \
         FROM \"layout_blocks\" WHERE \"id\" = 'd1-b1'",
    );
    assert_eq!(v, Value::Integer(0));
    // IN over the same shape.
    let v = scalar(
        &db,
        "SELECT (\"layout_blocks\".\"id\" IN (SELECT \"lba\".\"block_id\" FROM \"layout_block_audio\" AS \"lba\" \
         WHERE \"lba\".\"audio_url\" = 'nope')) \
         FROM \"layout_blocks\" WHERE \"id\" = 'd1-b1'",
    );
    assert_eq!(v, Value::Integer(0));
}
