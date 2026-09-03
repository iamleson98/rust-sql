//! open_in_memory cost breakdown: tempfile creation vs Database::open vs
//! first-statement page growth.

fn main() {
    let iters = 200;
    let mut t_tmp: u128 = 0;
    let mut t_open: u128 = 0;
    let mut t_create: u128 = 0;
    let mut t_first_insert: u128 = 0;
    for _ in 0..iters {
        let t = std::time::Instant::now();
        let f = tempfile::NamedTempFile::new().unwrap();
        t_tmp += t.elapsed().as_nanos();
        drop(f);

        let t = std::time::Instant::now();
        let mut db = rustqlite::Database::open_in_memory().unwrap();
        t_open += t.elapsed().as_nanos();

        let t = std::time::Instant::now();
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)",
            [],
        )
        .unwrap();
        t_create += t.elapsed().as_nanos();

        let t = std::time::Instant::now();
        db.execute("INSERT INTO t (name, val) VALUES ('x', 1)", [])
            .unwrap();
        t_first_insert += t.elapsed().as_nanos();
    }
    let us = |v: u128| v as f64 / iters as f64 / 1000.0;
    println!(
        "tempfile::new = {:.1}us  open_in_memory = {:.1}us  CREATE = {:.1}us  first INSERT = {:.1}us",
        us(t_tmp),
        us(t_open),
        us(t_create),
        us(t_first_insert)
    );

    // SQLite comparison.
    let mut t_open: u128 = 0;
    let mut t_create: u128 = 0;
    let mut t_first: u128 = 0;
    for _ in 0..iters {
        let t = std::time::Instant::now();
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        t_open += t.elapsed().as_nanos();
        let t = std::time::Instant::now();
        conn.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)",
            [],
        )
        .unwrap();
        t_create += t.elapsed().as_nanos();
        let t = std::time::Instant::now();
        conn.execute("INSERT INTO t (name, val) VALUES ('x', 1)", [])
            .unwrap();
        t_first += t.elapsed().as_nanos();
    }
    println!(
        "SQLite: open = {:.1}us  CREATE = {:.1}us  first INSERT = {:.1}us",
        us(t_open),
        us(t_create),
        us(t_first)
    );
}
