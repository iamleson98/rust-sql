//! Fast-path autocommit INSERT failure after splits: which structures
//! hold the discarded statement's roots? (regression probe for the
//! "index ux tree is corrupt: page 4 out of range" class)

use rustqlite::Database;

fn main() {
    let path = "/tmp/fastfail.rq.db";
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{path}-wal"));
    let mut db = Database::open(path).unwrap();
    db.execute("PRAGMA journal_mode = WAL", []).unwrap();
    db.execute(
        "CREATE TABLE events (id INTEGER PRIMARY KEY, k INTEGER)",
        [],
    )
    .unwrap();
    db.execute("CREATE UNIQUE INDEX ux ON events (k)", [])
        .unwrap();
    for i in 1..=100i64 {
        db.execute(
            "INSERT INTO events (id, k) VALUES (?, ?)",
            [rustqlite::Value::Integer(i), rustqlite::Value::Integer(i)],
        )
        .unwrap();
    }
    let master_before: Vec<Vec<rustqlite::Value>> = db
        .query("SELECT name, rootpage FROM sqlite_master", [])
        .unwrap();
    println!("sqlite_master before: {master_before:?}");
    println!(
        "page_count before: {}",
        db.query("PRAGMA page_count", []).unwrap()[0][0].as_integer()
    );

    // The failing giant statement: 200 good rows (splits), then a dup.
    let mut sql = String::from("INSERT INTO events (id, k) VALUES ");
    for j in 0..200i64 {
        if j > 0 {
            sql.push(',');
        }
        sql.push_str(&format!("({}, {})", 1000 + j, 1000 + j));
    }
    sql.push_str(", (5000, 1)");
    let err = db.execute(&sql, []).unwrap_err();
    println!("error: {err}");

    let master_after: Vec<Vec<rustqlite::Value>> = db
        .query("SELECT name, rootpage FROM sqlite_master", [])
        .unwrap();
    println!("sqlite_master after:  {master_after:?}");
    println!(
        "page_count after: {}",
        db.query("PRAGMA page_count", []).unwrap()[0][0].as_integer()
    );
    let n = db.query("SELECT count(*) FROM events", []).unwrap()[0][0].as_integer();
    println!("count after: {n}");
    let ic = db.query("PRAGMA integrity_check", []).unwrap();
    println!("integrity: {:?}", ic.first().map(|r| r.first().cloned()));
    // A plain k-range scan through the index (no integrity walker).
    let via_ix = db
        .query("SELECT count(*) FROM events WHERE k >= 0", [])
        .unwrap()[0][0]
        .as_integer();
    println!("index-visible count: {via_ix}");
}
