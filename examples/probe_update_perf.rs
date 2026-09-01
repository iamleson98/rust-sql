//! UPDATE-path performance probe: verify the write-set simulation added
//! no regression on the hot paths (bulk UPDATE on a non-indexed column,
//! point UPDATE by PK) and measure the unique-index enforcement cost.

use std::time::Instant;

fn bench_rustqlite(n: i64) {
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INT, s INT, u TEXT UNIQUE, idx INT)", []).unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=n {
        let row: Vec<rustqlite::Value> = vec![
            rustqlite::Value::Integer(i),
            rustqlite::Value::Integer(i),
            rustqlite::Value::Text(format!("u{i}").into()),
            rustqlite::Value::Integer(i),
        ];
        db.execute("INSERT INTO t (v, s, u, idx) VALUES (?, ?, ?, ?)", row).unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    db.execute("CREATE INDEX si ON t(s)", []).unwrap();

    let t0 = Instant::now();
    db.execute("UPDATE t SET v = v + 1", []).unwrap();
    let bulk_plain = t0.elapsed();

    let t0 = Instant::now();
    db.execute("UPDATE t SET s = s + 1", []).unwrap();
    let bulk_indexed = t0.elapsed();

    let t0 = Instant::now();
    for i in 1..=n {
        db.execute("UPDATE t SET v = v + 1 WHERE id = ?", vec![rustqlite::Value::Integer(i)]).unwrap();
    }
    let point = t0.elapsed();

    let t0 = Instant::now();
    db.execute("UPDATE t SET v = v + 1 WHERE u = 'u5000'", []).unwrap();
    let via_unique = t0.elapsed();

    println!(
        "  rustqlite: bulk(non-idx) {:>8.1?}  bulk(idx) {:>8.1?}  point x{n} {:>8.1?}  unique-lookup {:>8.1?}",
        bulk_plain, bulk_indexed, point, via_unique
    );
}

fn bench_rusqlite(n: i64) {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INT, s INT, u TEXT UNIQUE, idx INT)", []).unwrap();
    conn.execute("BEGIN", []).unwrap();
    for i in 1..=n {
        conn.execute(
            "INSERT INTO t (v, s, u, idx) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![i, i, format!("u{i}"), i],
        ).unwrap();
    }
    conn.execute("COMMIT", []).unwrap();
    conn.execute("CREATE INDEX si ON t(s)", []).unwrap();

    let t0 = Instant::now();
    conn.execute("UPDATE t SET v = v + 1", []).unwrap();
    let bulk_plain = t0.elapsed();

    let t0 = Instant::now();
    conn.execute("UPDATE t SET s = s + 1", []).unwrap();
    let bulk_indexed = t0.elapsed();

    let t0 = Instant::now();
    for i in 1..=n {
        conn.execute("UPDATE t SET v = v + 1 WHERE id = ?1", rusqlite::params![i]).unwrap();
    }
    let point = t0.elapsed();

    let t0 = Instant::now();
    conn.execute("UPDATE t SET v = v + 1 WHERE u = 'u5000'", []).unwrap();
    let via_unique = t0.elapsed();

    println!(
        "  sqlite:    bulk(non-idx) {:>8.1?}  bulk(idx) {:>8.1?}  point x{n} {:>8.1?}  unique-lookup {:>8.1?}",
        bulk_plain, bulk_indexed, point, via_unique
    );
}

fn main() {
    let n = 10_000;
    // Warm up both.
    bench_rustqlite(100);
    bench_rusqlite(100);
    println!("== n = {n} ==");
    bench_rustqlite(n);
    bench_rusqlite(n);
    bench_rusqlite(n);
    bench_rusqlite(n);
    bench_rusqlite(n);
}
