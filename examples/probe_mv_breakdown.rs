//! Exact-shape probe of bench_insert_multi_values: per-iteration breakdown
//! (open, CREATE TABLE, first chunk, remaining chunks) for BOTH engines.

fn main() {
    let n = 1000;
    let chunk_size = 100;
    let iters = 30;

    // ---------- rustqlite ----------
    let mut open_ns: u128 = 0;
    let mut create_ns: u128 = 0;
    let mut first_ns: u128 = 0;
    let mut rest_ns: u128 = 0;
    let mut sql_build_ns: u128 = 0;
    for _ in 0..iters {
        let t = std::time::Instant::now();
        let mut db = rustqlite::Database::open_in_memory().unwrap();
        open_ns += t.elapsed().as_nanos();

        let t = std::time::Instant::now();
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)",
            [],
        )
        .unwrap();
        create_ns += t.elapsed().as_nanos();

        let mut i = 1;
        let mut first = true;
        while i <= n {
            let end = (i + chunk_size - 1).min(n);
            let t = std::time::Instant::now();
            let values: String = (i..=end)
                .map(|j| format!("('name{}', {})", j, j))
                .collect::<Vec<_>>()
                .join(",");
            sql_build_ns += t.elapsed().as_nanos();
            let sql = format!("INSERT INTO t (name, val) VALUES {}", values);
            let t = std::time::Instant::now();
            db.execute(&sql, []).unwrap();
            let dt = t.elapsed().as_nanos();
            if first {
                first_ns += dt;
                first = false;
            } else {
                rest_ns += dt;
            }
            i = end + 1;
        }
    }
    let per = |v: u128| v as f64 / iters as f64 / 1000.0;
    println!("rustqlite: open={:.1}us create={:.1}us first-chunk={:.1}us rest(9)={:.1}us sql-build={:.1}us",
        per(open_ns), per(create_ns), per(first_ns), per(rest_ns), per(sql_build_ns));
    println!(
        "  total/iter = {:.1}us  ({:.0} ns/row)",
        per(open_ns + create_ns + first_ns + rest_ns + sql_build_ns),
        (open_ns + create_ns + first_ns + rest_ns + sql_build_ns) as f64 / (iters * n) as f64
    );

    // ---------- rusqlite (SQLite) ----------
    let mut open_ns: u128 = 0;
    let mut create_ns: u128 = 0;
    let mut first_ns: u128 = 0;
    let mut rest_ns: u128 = 0;
    let mut sql_build_ns: u128 = 0;
    for _ in 0..iters {
        let t = std::time::Instant::now();
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        open_ns += t.elapsed().as_nanos();

        let t = std::time::Instant::now();
        conn.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)",
            [],
        )
        .unwrap();
        create_ns += t.elapsed().as_nanos();

        let mut i = 1;
        let mut first = true;
        while i <= n {
            let end = (i + chunk_size - 1).min(n);
            let t = std::time::Instant::now();
            let values: String = (i..=end)
                .map(|j| format!("('name{}', {})", j, j))
                .collect::<Vec<_>>()
                .join(",");
            sql_build_ns += t.elapsed().as_nanos();
            let sql = format!("INSERT INTO t (name, val) VALUES {}", values);
            let t = std::time::Instant::now();
            conn.execute(&sql, []).unwrap();
            let dt = t.elapsed().as_nanos();
            if first {
                first_ns += dt;
                first = false;
            } else {
                rest_ns += dt;
            }
            i = end + 1;
        }
    }
    println!("rusqlite : open={:.1}us create={:.1}us first-chunk={:.1}us rest(9)={:.1}us sql-build={:.1}us",
        per(open_ns), per(create_ns), per(first_ns), per(rest_ns), per(sql_build_ns));
    println!(
        "  total/iter = {:.1}us  ({:.0} ns/row)",
        per(open_ns + create_ns + first_ns + rest_ns + sql_build_ns),
        (open_ns + create_ns + first_ns + rest_ns + sql_build_ns) as f64 / (iters * n) as f64
    );
}
