//! Upsert target vs non-target unique-index conflict ordering.
fn main() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE audit (id INTEGER PRIMARY KEY, note TEXT);
         CREATE UNIQUE INDEX ix0 ON audit(note) WHERE note IS NOT NULL;
         INSERT INTO audit (id, note) VALUES (1, 'alpha');",
    )
    .unwrap();

    // Does SQLite error when a NON-target unique index conflicts?
    for (desc, sql) in [
        (
            "id exists + note conflict",
            "INSERT INTO audit (id, note) VALUES (1, 'alpha') ON CONFLICT(id) DO NOTHING",
        ),
        (
            "id fresh + note conflict",
            "INSERT INTO audit (id, note) VALUES (39, 'alpha') ON CONFLICT(id) DO NOTHING",
        ),
        (
            "target=ix0 + rowid conflict",
            "INSERT INTO audit (id, note) VALUES (1, 'beta') ON CONFLICT(note) DO NOTHING",
        ),
        (
            "no upsert, note conflict",
            "INSERT INTO audit (id, note) VALUES (40, 'alpha')",
        ),
    ] {
        let r: Result<usize, rusqlite::Error> = conn.execute(sql, []);
        println!("{desc}: {r:?}");
    }
    println!(
        "rows: {:?}",
        conn.query_row(
            "SELECT group_concat(id || ':' || note, ' ') FROM audit",
            [],
            |r| { r.get::<_, String>(0) }
        )
    );
}
