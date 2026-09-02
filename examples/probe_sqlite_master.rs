use rustqlite::Database;

fn main() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, email TEXT UNIQUE)",
        [],
    )
    .unwrap();
    db.execute("CREATE INDEX idx_users_name ON users(name)", [])
        .unwrap();
    db.execute("CREATE VIEW v AS SELECT id, name FROM users", [])
        .unwrap();
    db.execute(
        "CREATE TRIGGER trg AFTER INSERT ON users BEGIN UPDATE users SET name = name; END",
        [],
    )
    .unwrap();

    // The sqlite_master surface every migration tool reads.
    let r = db.query(
        "SELECT type, name, tbl_name, rootpage, sql FROM sqlite_master",
        [],
    );
    match r {
        Ok(rows) => {
            for row in rows {
                println!("ROW: {:?}", row);
            }
        }
        Err(e) => println!("sqlite_master query FAILED: {e}"),
    }

    // The migration-manager pattern: does the migrations table exist?
    let r = db.query(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'",
        [],
    );
    println!("migrations-table check: {:?}", r);

    // CREATE TABLE IF NOT EXISTS (used by every migrator).
    let r = db.execute(
        "CREATE TABLE IF NOT EXISTS _sqlx_migrations (version BIGINT PRIMARY KEY, description TEXT NOT NULL)",
        [],
    );
    println!("create if not exists: {:?}", r);
    let r = db.execute(
        "CREATE TABLE IF NOT EXISTS _sqlx_migrations (version BIGINT PRIMARY KEY, description TEXT NOT NULL)",
        [],
    );
    println!("repeat create if not exists: {:?}", r);

    // sqlite_version() function presence.
    let r = db.query("SELECT sqlite_version()", []);
    println!("sqlite_version(): {:?}", r);
    // Table list order (migrators rely on rowid ordering of sqlite_master).
    let r = db.query(
        "SELECT name FROM sqlite_master WHERE type='table' ORDER BY rowid",
        [],
    );
    println!("tables by rowid: {:?}", r);
}
