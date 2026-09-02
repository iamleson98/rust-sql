//! Profile join phases: scan cost vs hash build vs probe+emit.

use rustqlite::Database;

fn main() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE a (id INTEGER PRIMARY KEY, x INTEGER)", [])
        .unwrap();
    db.execute(
        "CREATE TABLE b (id INTEGER PRIMARY KEY, a_id INTEGER, y INTEGER)",
        [],
    )
    .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=1000 {
        db.execute(&format!("INSERT INTO a (x) VALUES ({})", i), [])
            .unwrap();
    }
    for i in 1..=1000 {
        db.execute(
            &format!("INSERT INTO b (a_id, y) VALUES ({}, {})", (i % 1000) + 1, i),
            [],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();

    // Warm up
    for _ in 0..50 {
        let _ = db
            .query(
                "SELECT a.id, a.x, b.id, b.y FROM a INNER JOIN b ON a.id = b.a_id LIMIT 1000",
                [],
            )
            .unwrap();
        let _ = db.query("SELECT * FROM a", []).unwrap();
        let _ = db.query("SELECT * FROM b", []).unwrap();
    }

    let n = 200;

    // 1. Full join
    let t = std::time::Instant::now();
    for _ in 0..n {
        let rows = db
            .query(
                "SELECT a.id, a.x, b.id, b.y FROM a INNER JOIN b ON a.id = b.a_id LIMIT 1000",
                [],
            )
            .unwrap();
        assert_eq!(rows.len(), 1000);
    }
    let d = t.elapsed() / n;
    println!(
        "full join          : {:>9.2?}  ({:.1} ns/out-row)",
        d,
        d.as_nanos() as f64 / 1000.0
    );

    // 2. Scan a only
    let t = std::time::Instant::now();
    for _ in 0..n {
        let rows = db.query("SELECT * FROM a", []).unwrap();
        assert_eq!(rows.len(), 1000);
    }
    let d = t.elapsed() / n;
    println!(
        "scan a (2 cols)    : {:>9.2?}  ({:.1} ns/row)",
        d,
        d.as_nanos() as f64 / 1000.0
    );

    // 3. Scan b only
    let t = std::time::Instant::now();
    for _ in 0..n {
        let rows = db.query("SELECT * FROM b", []).unwrap();
        assert_eq!(rows.len(), 1000);
    }
    let d = t.elapsed() / n;
    println!(
        "scan b (3 cols)    : {:>9.2?}  ({:.1} ns/row)",
        d,
        d.as_nanos() as f64 / 1000.0
    );

    // 4. Scans + query overhead baseline
    let t = std::time::Instant::now();
    for _ in 0..n {
        let rows = db.query("SELECT 1", []).unwrap();
        assert_eq!(rows.len(), 1);
    }
    let d = t.elapsed() / n;
    println!("SELECT 1 baseline  : {:>9.2?}", d);
}
