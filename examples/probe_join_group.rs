// Profile the join+GROUP BY regression: 3.34ms vs the old 1.75ms.
fn main() {
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, dept TEXT, region TEXT)",
        [],
    )
    .unwrap();
    db.execute(
        "CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER, total REAL)",
        [],
    )
    .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=1000i64 {
        let dept = format!("d{}", i % 20);
        db.execute(
            "INSERT INTO users (id, dept) VALUES (?, ?)",
            [
                rustqlite::Value::Integer(i),
                rustqlite::Value::Text(dept.into()),
            ],
        )
        .unwrap();
    }
    for i in 1..=10000i64 {
        db.execute(
            "INSERT INTO orders (id, user_id, total) VALUES (?, ?, ?)",
            [
                rustqlite::Value::Integer(i),
                rustqlite::Value::Integer((i % 1000) + 1),
                rustqlite::Value::Real(i as f64 * 1.5),
            ],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();

    let join_group = "SELECT u.dept, COUNT(*), SUM(o.total) FROM users u JOIN orders o ON u.id = o.user_id GROUP BY u.dept";
    let join_only = "SELECT u.dept, o.total FROM users u JOIN orders o ON u.id = o.user_id";
    let group_only = "SELECT dept, COUNT(*), SUM(id) FROM users GROUP BY dept";

    for sql in [join_group, join_only] {
        let rows = db.query(&format!("EXPLAIN QUERY PLAN {sql}"), []).unwrap();
        for r in &rows {
            println!("PLAN: {:?}", r);
        }
    }
    for (label, sql) in [
        ("join+group", join_group),
        ("join only", join_only),
        ("group only", group_only),
    ] {
        let _ = db.query(sql, []).unwrap(); // warmup
        let mut best = f64::MAX;
        for _ in 0..5 {
            let t = std::time::Instant::now();
            let out = db.query(sql, []).unwrap();
            let e = t.elapsed().as_secs_f64() * 1000.0;
            if e < best {
                best = e;
            }
            std::hint::black_box(&out);
        }
        println!("{label:12}: {best:.3}ms");
    }
}

#[allow(dead_code)]
fn explain_things() {}
