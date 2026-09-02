//! Benchmark: INSERT throughput.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use rusqlite::params;

fn bench_rusqlite_insert(c: &mut Criterion) {
    c.bench_function("rusqlite_insert_batch", |b| {
        b.iter_with_setup(
            || {
                let conn = rusqlite::Connection::open_in_memory().unwrap();
                conn.execute(
                    "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)",
                    [],
                )
                .unwrap();
                conn
            },
            |conn| {
                for i in 1..=1000 {
                    conn.execute(
                        "INSERT INTO t (name, val) VALUES (?1, ?2)",
                        params![format!("name{}", black_box(i)), black_box(i)],
                    )
                    .unwrap();
                }
            },
        )
    });
}

fn bench_rustqlite_insert(c: &mut Criterion) {
    c.bench_function("rustqlite_insert_batch", |b| {
        b.iter_with_setup(
            || {
                let mut db = rustqlite::Database::open_in_memory().unwrap();
                db.execute(
                    "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)",
                    [],
                )
                .unwrap();
                db
            },
            |mut db| {
                for i in 1..=1000 {
                    let sql = format!(
                        "INSERT INTO t (name, val) VALUES ('name{}', {})",
                        black_box(i),
                        black_box(i)
                    );
                    db.execute(&sql, []).unwrap();
                }
            },
        )
    });
}

criterion_group!(benches, bench_rusqlite_insert, bench_rustqlite_insert);
criterion_main!(benches);
