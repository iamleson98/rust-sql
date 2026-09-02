//! Probe: page fill factor + file size breakdown vs SQLite for the same data.

fn main() {
    let path = "/tmp/fillprobe_rust.db";
    let _ = std::fs::remove_file(path);
    let mut db = rustqlite::Database::open(path).unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, val INTEGER, score REAL, name TEXT)",
        [],
    )
    .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=10_000i64 {
        let sql = format!(
            "INSERT INTO t (val, score, name) VALUES ({}, {}, 'user{}')",
            i, i as f64, i
        );
        db.execute(&sql, []).unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    let sz = std::fs::metadata(path).unwrap().len();
    let pc = db.page_count();
    let ps = db.page_size();
    println!(
        "rustqlite: file={} pages={} page_size={} bytes/page={:.0} fill={:.1}%",
        sz,
        pc,
        ps,
        sz as f64 / pc as f64,
        (sz - 100) as f64 / (pc as f64 * ps as f64) * 100.0
    );

    // Same data in SQLite
    let spath = "/tmp/fillprobe_sqlite.db";
    let _ = std::fs::remove_file(spath);
    let conn = rusqlite::Connection::open(spath).unwrap();
    conn.execute_batch("PRAGMA page_size=16384; CREATE TABLE t (id INTEGER PRIMARY KEY, val INTEGER, score REAL, name TEXT)").unwrap();
    for i in 1..=10_000i64 {
        let sql = format!(
            "INSERT INTO t (val, score, name) VALUES ({}, {}, 'user{}')",
            i, i as f64, i
        );
        conn.execute(&sql, []).unwrap();
    }
    let sz2 = std::fs::metadata(spath).unwrap().len();
    let pc2: i64 = conn
        .query_row("PRAGMA page_count", [], |r| r.get(0))
        .unwrap();
    println!(
        "sqlite16k: file={} pages={} bytes/page={:.0}",
        sz2,
        pc2,
        sz2 as f64 / pc2 as f64
    );

    // Now: does our file shrink after deleting half the rows + checkpoint?
    let _ = std::fs::remove_file(path);
    let mut db = rustqlite::Database::open(path).unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, val INTEGER, score REAL, name TEXT)",
        [],
    )
    .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=10_000i64 {
        let sql = format!(
            "INSERT INTO t (val, score, name) VALUES ({}, {}, 'user{}')",
            i, i as f64, i
        );
        db.execute(&sql, []).unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    let before = std::fs::metadata(path).unwrap().len();
    db.execute("DELETE FROM t WHERE id > 1000", []).unwrap();
    let after = std::fs::metadata(path).unwrap().len();
    println!(
        "after delete 90%: {} -> {} pages={} freelist=?",
        before,
        after,
        db.page_count()
    );
    drop(db);
    let after_close = std::fs::metadata(path).unwrap().len();
    println!("after close: {}", after_close);

    // Insert again — freelist should be reused, not grow
    let mut db = rustqlite::Database::open(path).unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=5_000i64 {
        let sql = format!(
            "INSERT INTO t (val, score, name) VALUES ({}, {}, 'user{}')",
            i, i as f64, i
        );
        db.execute(&sql, []).unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    println!(
        "after reinsert 5k: pages={} freelist=? size={}",
        db.page_count(),
        std::fs::metadata(path).unwrap().len()
    );
}
