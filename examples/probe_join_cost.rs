//! TEMP probe: 3-table join cost decomposition.
use rustqlite::{Database, Value};
use std::time::Instant;

fn best_of<F: FnMut() -> usize>(mut f: F, n: usize) -> f64 {
    let mut best = f64::MAX;
    for _ in 0..n {
        let t = Instant::now();
        let sink = f();
        let ms = t.elapsed().as_secs_f64() * 1e6;
        if sink == usize::MAX {
            panic!();
        }
        if ms < best {
            best = ms;
        }
    }
    best
}

fn main() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, dept TEXT)",
        [],
    )
    .unwrap();
    db.execute(
        "CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER, total INTEGER)",
        [],
    )
    .unwrap();
    db.execute(
        "CREATE TABLE items (id INTEGER PRIMARY KEY, order_id INTEGER, name TEXT, price REAL)",
        [],
    )
    .unwrap();
    db.execute("CREATE INDEX idx_orders_user ON orders(user_id)", [])
        .unwrap();
    db.execute("CREATE INDEX idx_items_order ON items(order_id)", [])
        .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=1000i64 {
        db.execute(
            "INSERT INTO users (name, dept) VALUES (?, ?)",
            [
                Value::Text(format!("user{i}").into()),
                Value::Text(format!("d{}", i % 10).into()),
            ],
        )
        .unwrap();
    }
    for i in 1..=10000i64 {
        db.execute(
            "INSERT INTO orders (user_id, total) VALUES (?, ?)",
            [Value::Integer((i % 1000) + 1), Value::Integer(i * 10)],
        )
        .unwrap();
    }
    for i in 1..=50000i64 {
        db.execute(
            "INSERT INTO items (order_id, name, price) VALUES (?, ?, ?)",
            [
                Value::Integer((i % 10000) + 1),
                Value::Text(format!("item{i}").into()),
                Value::Real(i as f64 * 0.5),
            ],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();

    // warm
    for _ in 0..5 {
        let _ = db.query("SELECT u.name, o.total, i.name, i.price FROM users u JOIN orders o ON u.id = o.user_id JOIN items i ON o.id = i.order_id WHERE u.id = 500", []).unwrap();
    }

    let q3 = "SELECT u.name, o.total, i.name, i.price FROM users u JOIN orders o ON u.id = o.user_id JOIN items i ON o.id = i.order_id WHERE u.id = 500";
    let q2 =
        "SELECT u.name, o.total FROM users u JOIN orders o ON u.id = o.user_id WHERE u.id = 500";
    let q1 = "SELECT u.name FROM users WHERE u.id = 500";
    // order rows by PK (the 5 orders of user 500): order ids are user_id + k*1000
    let mut order_ids = Vec::new();
    for i in 1..=10000i64 {
        if (i % 1000) + 1 == 500 {
            order_ids.push(i);
        }
    }
    let qo = format!(
        "SELECT total FROM orders WHERE id IN ({})",
        order_ids
            .iter()
            .map(|x| x.to_string())
            .collect::<Vec<_>>()
            .join(",")
    );
    let item_ids: Vec<i64> = order_ids.iter().flat_map(|o| *o..*o + 5).collect();
    let qi = format!(
        "SELECT name, price FROM items WHERE id IN ({})",
        item_ids
            .iter()
            .map(|x| x.to_string())
            .collect::<Vec<_>>()
            .join(",")
    );

    let t3 = best_of(
        || {
            let r = db.query(q3, []).unwrap();
            r.len()
        },
        200,
    );
    let t2 = best_of(
        || {
            let r = db.query(q2, []).unwrap();
            r.len()
        },
        200,
    );
    let t1 = best_of(
        || {
            let r = db.query(q1, []).unwrap();
            r.len()
        },
        200,
    );
    let to = best_of(
        || {
            let r = db.query(&qo, []).unwrap();
            r.len()
        },
        200,
    );
    let ti = best_of(
        || {
            let r = db.query(&qi, []).unwrap();
            r.len()
        },
        200,
    );
    println!("user point lookup     : {:7.2}us", t1);
    println!(
        "2-table join          : {:7.2}us (orders of user 500: {})",
        t2,
        order_ids.len()
    );
    println!("3-table join          : {:7.2}us", t3);
    println!("5 order rows by PK IN : {:7.2}us", to);
    println!("20 item rows by PK IN : {:7.2}us", ti);
    println!("delta 3v2             : {:7.2}us (the items side)", t3 - t2);
    println!(
        "delta 2v1             : {:7.2}us (the orders side)",
        t2 - t1
    );
}
