use rustqlite::Database;

fn main() {
    // Transaction modes sqlx/sea-orm issue (pool BEGIN, migrator lock).
    let mut db = Database::open_in_memory().unwrap();
    for mode in ["BEGIN", "BEGIN DEFERRED", "BEGIN IMMEDIATE", "BEGIN EXCLUSIVE"] {
        let r = db.execute(mode, []);
        println!("{mode:16} -> {:?}", r);
        let _ = db.execute("COMMIT", []);
    }
    // Savepoints inside IMMEDIATE txns (sea-orm nested tx).
    let r = db.execute("BEGIN IMMEDIATE", []);
    println!("begin immediate: {:?}", r);
    let r = db.execute("SAVEPOINT sp1", []);
    println!("savepoint:       {:?}", r);
    let r = db.execute("INSERT INTO t VALUES (1)", []);
    println!("insert (no tbl): {:?}", r.map_err(|e| e.to_string()));
    let r = db.execute("ROLLBACK TO sp1", []);
    println!("rollback to:     {:?}", r);
    let r = db.execute("RELEASE sp1", []);
    println!("release:         {:?}", r);
    let _ = db.execute("COMMIT", []);

    // END alias for COMMIT.
    let r = db.execute("BEGIN", []);
    println!("begin:           {:?}", r);
    let r = db.execute("END", []);
    println!("end (commit):    {:?}", r);

    // Locking pragmas the sqlx migrator uses.
    let mut db2 = Database::open_in_memory().unwrap();
    let r = db2.query("PRAGMA locking_mode", []);
    println!("locking_mode:    {:?}", r);
    let r = db2.execute("PRAGMA locking_mode = EXCLUSIVE", []);
    println!("set exclusive:   {:?}", r);
    let r = db2.query("PRAGMA locking_mode", []);
    println!("locking_mode:    {:?}", r);
    let r = db2.query("PRAGMA journal_mode", []);
    println!("journal_mode:    {:?}", r);
    let r = db2.query("PRAGMA synchronous", []);
    println!("synchronous:     {:?}", r);
    let r = db2.query("PRAGMA cache_size", []);
    println!("cache_size:      {:?}", r);
    let r = db2.query("PRAGMA temp_store", []);
    println!("temp_store:      {:?}", r);
}
