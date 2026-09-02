//! Benchmark: range scans (SELECT WHERE id BETWEEN).

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use rusqlite::params;

fn setup_rusqlite(n: i64) -> rusqlite::Connection {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)",
        [],
    )
    .unwrap();
    for i in 1..=n {
        conn.execute(
            "INSERT INTO t (name, val) VALUES (?1, ?2)",
            params![format!("name{}", i), i * 2],
        )
        .unwrap();
    }
    conn
}

fn setup_rustqlite(n: i64) -> rustqlite::Database {
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)",
        [],
    )
    .unwrap();
    for i in 1..=n {
        let sql = format!("INSERT INTO t (name, val) VALUES ('name{}', {})", i, i * 2);
        db.execute(&sql, []).unwrap();
    }
    db
}

fn bench_rusqlite_range_scan(c: &mut Criterion) {
    let conn = setup_rusqlite(10000);
    c.bench_function("rusqlite_range_scan", |b| {
        b.iter(|| {
            let mut stmt = conn
                .prepare("SELECT name, val FROM t WHERE id BETWEEN ?1 AND ?2")
                .unwrap();
            let mut rows = stmt
                .query(params![black_box(1000), black_box(5000)])
                .unwrap();
            while rows.next().unwrap().is_some() {}
        })
    });
}

fn bench_rustqlite_range_scan(c: &mut Criterion) {
    let db = setup_rustqlite(10000);
    c.bench_function("rustqlite_range_scan", |b| {
        b.iter(|| {
            let _rows = db
                .query("SELECT name, val FROM t WHERE id BETWEEN 1000 AND 5000", [])
                .unwrap();
            let _ = black_box(1000);
        })
    });
}

criterion_group!(
    benches,
    bench_rusqlite_range_scan,
    bench_rustqlite_range_scan
);
criterion_main!(benches);
