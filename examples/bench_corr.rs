// Correlated subquery rewrite perf probe: measure the per-outer-row
// correlated subquery cost before/after the parameter rewrite.
// Uses 800 users x 4000 orders shape from tests/correlated.rs.
// Run: cargo run --release --example bench_corr
use rustqlite::{Database, Value};

fn main() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, dept TEXT, salary REAL)",
        [],
    )
    .unwrap();
    db.execute(
        "CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER, total INTEGER)",
        [],
    )
    .unwrap();
    let mut sql = String::from("INSERT INTO users VALUES ");
    for i in 1..=2000 {
        if i > 1 {
            sql.push(',');
        }
        sql.push_str(&format!(
            "({}, 'u{}', '{}', {})",
            i,
            i,
            if i % 3 == 0 { "eng" } else { "ops" },
            i
        ));
    }
    db.execute(&sql, []).unwrap();
    let mut sql = String::from("INSERT INTO orders VALUES ");
    for i in 1..=20000 {
        if i > 1 {
            sql.push(',');
        }
        sql.push_str(&format!("({}, {}, {})", i, i % 2000 + 1, (i * 7) % 100));
    }
    db.execute(&sql, []).unwrap();

    let queries = [
        "SELECT COUNT(*) FROM users u WHERE (SELECT COUNT(*) FROM orders o WHERE o.user_id = u.id) >= 3",
        "SELECT COUNT(*) FROM users u WHERE EXISTS (SELECT 1 FROM orders o WHERE o.user_id = u.id AND o.total > 90)",
        "SELECT COUNT(*) FROM users u WHERE u.id IN (SELECT o.user_id FROM orders o WHERE o.total > 95)",
    ];

    for q in &queries {
        // warmup + correctness sanity
        let r = db.query(q, []).unwrap();
        let count: i64 = match &r[0][0] {
            Value::Integer(i) => *i,
            _ => -1,
        };
        let t0 = std::time::Instant::now();
        let iters = 5;
        for _ in 0..iters {
            db.query(q, []).unwrap();
        }
        let dt = t0.elapsed().as_secs_f64() / iters as f64;
        println!("{:.3}s (count={}) :: {}", dt, count, q);
    }

    // Correlated subquery in the SELECT list — 2000 outer rows, each
    // running a scalar subquery
    let q = "SELECT (SELECT SUM(o.total) FROM orders o WHERE o.user_id = u.id) FROM users u";
    let t0 = std::time::Instant::now();
    let rows = db.query(q, []).unwrap();
    let dt = t0.elapsed().as_secs_f64();
    println!("{:.3}s ({} rows) :: {}", dt, rows.len(), q);

    // INDEXED correlated subqueries — the parameter rewrite should turn
    // the correlation into an index SEEK per outer row.
    db.execute("CREATE INDEX ix_orders_user ON orders(user_id)", [])
        .unwrap();
    for q in [
        "SELECT COUNT(*) FROM users u WHERE (SELECT COUNT(*) FROM orders o WHERE o.user_id = u.id) >= 3",
        "SELECT COUNT(*) FROM users u WHERE EXISTS (SELECT 1 FROM orders o WHERE o.user_id = u.id AND o.total > 90)",
        "SELECT (SELECT SUM(o.total) FROM orders o WHERE o.user_id = u.id) FROM users u",
    ] {
        let t0 = std::time::Instant::now();
        let r = db.query(q, []).unwrap();
        let dt = t0.elapsed().as_secs_f64();
        let count = match &r[r.len()-1][0] { Value::Integer(i) => *i, _ => -1 };
        println!("{:.4}s (n={}/{}) :: {}", dt, r.len(), count, q);
    }

    // EXISTS form vs SCALAR form of the SAME body — isolate which arm
    // is slow
    for q in [
        "SELECT COUNT(*) FROM users u WHERE (SELECT COUNT(*) FROM orders o WHERE o.user_id = u.id AND o.total > 90) >= 0",
        "SELECT COUNT(*) FROM users u WHERE EXISTS (SELECT 1 FROM orders o WHERE o.user_id = u.id AND o.total > 90)",
        "SELECT COUNT(*) FROM users u WHERE (SELECT 1 FROM orders o WHERE o.user_id = u.id AND o.total > 90) IS NOT NULL",
        "SELECT COUNT(*) FROM users u WHERE 1 = (SELECT 1 FROM orders o WHERE o.user_id = u.id AND o.total > 90 LIMIT 1)",
    ] {
        let t0 = std::time::Instant::now();
        let r = db.query(q, []).unwrap();
        let dt = t0.elapsed().as_secs_f64();
        let count = match &r[0][0] { Value::Integer(i) => *i, _ => -1 };
        println!("{:.3}s (count={}) :: {}", dt, count, q);
    }
}
