//! Benchmark: point lookups (SELECT by rowid).

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use rusqlite::params;

fn setup_rusqlite(n: i64) -> rusqlite::Connection {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)", []).unwrap();
    for i in 1..=n {
        conn.execute(
            "INSERT INTO t (name, val) VALUES (?1, ?2)",
            params![format!("name{}", i), i * 2],
        ).unwrap();
    }
    conn
}

fn setup_rustqlite(n: i64) -> rustqlite::Database {
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)", []).unwrap();
    for i in 1..=n {
        let sql = format!("INSERT INTO t (name, val) VALUES ('name{}', {})", i, i * 2);
        db.execute(&sql, []).unwrap();
    }
    db
}

fn bench_rusqlite_point_lookup(c: &mut Criterion) {
    let conn = setup_rusqlite(1000);
    c.bench_function("rusqlite_point_lookup", |b| {
        b.iter(|| {
            let mut stmt = conn.prepare("SELECT name, val FROM t WHERE id = ?1").unwrap();
            let mut rows = stmt.query(params![black_box(500)]).unwrap();
            while rows.next().unwrap().is_some() {}
        })
    });
}

fn bench_rustqlite_point_lookup(c: &mut Criterion) {
    let db = setup_rustqlite(1000);
    c.bench_function("rustqlite_point_lookup", |b| {
        b.iter(|| {
            let _rows = db.query(
                "SELECT name, val FROM t WHERE id = 500",
                [],
            ).unwrap();
            let _ = black_box(500);
        })
    });
}

criterion_group!(benches, bench_rusqlite_point_lookup, bench_rustqlite_point_lookup);
criterion_main!(benches);
