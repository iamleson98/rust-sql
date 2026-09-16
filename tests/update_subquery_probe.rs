// Correlated scalar subquery over an ALTERed composite-PK table.
#[test]
fn correlated_subquery_after_alter() {
    use rustqlite::Database;
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE layout_blocks (id TEXT PRIMARY KEY, page_id TEXT, audio_url TEXT, created_at TEXT)", []).unwrap();
    db.execute("CREATE TABLE layout_block_audio (block_id TEXT NOT NULL, version_id TEXT NOT NULL, audio_url TEXT NOT NULL, created_at TEXT NOT NULL, PRIMARY KEY (block_id, version_id))", []).unwrap();
    // ALTER present (autoindex re-registered by the fix).
    db.execute(
        "ALTER TABLE layout_block_audio ADD COLUMN merged_into TEXT NULL",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO layout_blocks VALUES ('d1-b0','p1','audio/d1/d1-b0.opus','t')",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO layout_blocks VALUES ('d1-b1','p1',NULL,'t')",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO layout_block_audio VALUES ('d1-b0','ver-a','audio/d1/d1-b0.opus','t',NULL)",
        [],
    )
    .unwrap();
    db.execute("INSERT INTO layout_block_audio VALUES ('d1-b1','ver-b','audio/d1/ver-b/d1-b1.opus','t',NULL)", []).unwrap();

    // The mirror-sync subquery shape.
    let rows = db.query(
        "SELECT b.id, (SELECT CASE WHEN lba.merged_into IS NOT NULL THEN NULL ELSE lba.audio_url END FROM layout_block_audio AS lba WHERE lba.block_id = b.id AND lba.version_id = 'ver-b') AS x FROM layout_blocks b",
        [],
    ).unwrap();
    for r in &rows {
        println!("row: {:?}", r);
    }
    assert_eq!(rows[1][1].to_string(), "audio/d1/ver-b/d1-b1.opus");
}
