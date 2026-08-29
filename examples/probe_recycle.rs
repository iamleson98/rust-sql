//! Probe: empty-leaf recycling + freelist reuse under delete/insert churn.
//! Insert 10k rows, record file size; DELETE all; insert 10k again —
//! the file must NOT grow (freed pages get reused).

fn main() {
    let path = "/tmp/probe_recycle.db";
    let _ = std::fs::remove_file(path);
    let mut db = rustqlite::Database::open(path).unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)", []).unwrap();

    let insert = |db: &mut rustqlite::Database| {
        db.execute("BEGIN", []).unwrap();
        let sql = "INSERT INTO t (name, val, score) VALUES (?, ?, ?)";
        for i in 1..=10_000i64 {
            db.execute(sql, [
                rustqlite::Value::Text(format!("name{}", i)),
                rustqlite::Value::Integer(i * 2),
                rustqlite::Value::Real(i as f64 * 1.5),
            ]).unwrap();
        }
        db.execute("COMMIT", []).unwrap();
    };

    insert(&mut db);
    let size_after_first = std::fs::metadata(path).unwrap().len();
    println!("after 1st 10k insert: {} KB, pages = {}", size_after_first / 1024, db.page_count());

    // Delete everything (in one txn).
    db.execute("BEGIN", []).unwrap();
    let sql = "DELETE FROM t WHERE id = ?";
    for i in 1..=10_000i64 {
        db.execute(sql, [rustqlite::Value::Integer(i)]).unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    let n = db.query("SELECT COUNT(*) FROM t", []).unwrap();
    println!("after delete all: count = {:?}, pages = {}", n[0][0], db.page_count());

    // Insert the same volume again — must reuse freed pages.
    insert(&mut db);
    let size_after_second = std::fs::metadata(path).unwrap().len();
    println!("after 2nd 10k insert: {} KB, pages = {}", size_after_second / 1024, db.page_count());

    // Correctness: random spot checks.
    for i in [1i64, 5000, 9999, 10000] {
        let rows = db.query("SELECT id, name, val FROM t WHERE id = ?", [rustqlite::Value::Integer(i)]).unwrap();
        println!("row {}: {:?}", i, rows[0]);
    }
    let sum = db.query("SELECT COUNT(*), SUM(val) FROM t", []).unwrap();
    println!("final count/sum: {:?}", sum[0]);

    if size_after_second <= size_after_first + 16384 {
        println!("PASS: file did not grow ({} -> {} bytes)", size_after_first, size_after_second);
    } else {
        println!("FAIL: file grew {} -> {} bytes", size_after_first, size_after_second);
    }
}
