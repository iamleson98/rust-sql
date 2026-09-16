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
//! because the fused resolver only handles bare column references.

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
