//! Inspect page accounting for the 10k-row file: where do our 33 pages go
//! vs SQLite's 64?
use rustqlite::Database;

fn main() {
    // Our engine, default 8 KiB pages
    let path = "/tmp/probe_size_rsql.db";
    let _ = std::fs::remove_file(path);
    {
        let mut db = Database::open(path).unwrap();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)", []).unwrap();
        db.execute("BEGIN", []).unwrap();
        let sql = "INSERT INTO t (name, val, score) VALUES (?, ?, ?)";
        for i in 1..=10000i64 {
            db.execute(sql, [
                rustqlite::Value::Text(format!("name{}", i).into()),
                rustqlite::Value::Integer(i * 2),
                rustqlite::Value::Real(i as f64 * 1.5),
            ]).unwrap();
        }
        db.execute("COMMIT", []).unwrap();
        db.flush().unwrap();
    }
    let sz = std::fs::metadata(path).unwrap().len();
    println!("rustqlite: {} bytes = {} pages of 8192", sz, sz / 8192);

    // SQLite for comparison
    let spath = "/tmp/probe_size_sqlite.db";
    let _ = std::fs::remove_file(spath);
    {
        let conn = rusqlite::Connection::open(spath).unwrap();
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)", []).unwrap();
        conn.execute_batch("BEGIN").unwrap();
        for i in 1..=10000i64 {
            conn.execute("INSERT INTO t (name, val, score) VALUES (?1, ?2, ?3)",
                rusqlite::params![format!("name{}", i), i * 2, i as f64 * 1.5]).unwrap();
        }
        conn.execute_batch("COMMIT").unwrap();
    }
    let ssz = std::fs::metadata(spath).unwrap().len();
    println!("sqlite:    {} bytes = {} pages of 4096", ssz, ssz / 4096);

    // page_size=4096 comparison
    let path4 = "/tmp/probe_size_rsql4.db";
    let _ = std::fs::remove_file(path4);
    {
        let mut db = Database::open(path4).unwrap();
        db.execute("PRAGMA page_size = 4096", []).unwrap();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)", []).unwrap();
        db.execute("BEGIN", []).unwrap();
        let sql = "INSERT INTO t (name, val, score) VALUES (?, ?, ?)";
        for i in 1..=10000i64 {
            db.execute(sql, [
                rustqlite::Value::Text(format!("name{}", i).into()),
                rustqlite::Value::Integer(i * 2),
                rustqlite::Value::Real(i as f64 * 1.5),
            ]).unwrap();
        }
        db.execute("COMMIT", []).unwrap();
        db.flush().unwrap();
    }
    let sz4 = std::fs::metadata(path4).unwrap().len();
    println!("rustqlite page_size=4096: {} bytes = {} pages", sz4, sz4 / 4096);
}
