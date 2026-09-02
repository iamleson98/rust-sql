//! Debug the failing correlated cases (with all tables).
use rustqlite::Database;

fn main() {
    let mut db = Database::open_in_memory().unwrap();
    for s in [
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, dept TEXT, salary REAL)",
        "CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER, total INTEGER)",
        "CREATE TABLE items (id INTEGER PRIMARY KEY, order_id INTEGER, qty INTEGER)",
        "INSERT INTO users VALUES (1, 'alice', 'eng', 100.0), (2, 'bob', 'eng', 90.0), (3, 'dan', 'sales', NULL)",
        "INSERT INTO orders VALUES (1, 1, 30), (2, 1, 10), (3, 2, 15)",
        "INSERT INTO items VALUES (1, 1, 2), (2, 1, 1), (3, 2, 5), (4, 3, 1)",
    ] {
        db.execute(s, []).unwrap();
    }

    let cases = [
        // 1: COALESCE(SUM) UNCORRELATED standalone
        "SELECT COALESCE(SUM(total), 0) FROM orders",
        // 2: COALESCE(SUM) uncorrelated subquery
        "SELECT (SELECT COALESCE(SUM(total), 0) FROM orders)",
        // 3: correlated COALESCE(SUM)
        "SELECT name, (SELECT COALESCE(SUM(total), 0) FROM orders o WHERE o.user_id = u.id) FROM users u",
        // 4: correlated COUNT in join WHERE — the differential failure
        "SELECT u.name, o.id FROM users u JOIN orders o ON o.user_id = u.id WHERE (SELECT COUNT(*) FROM items i WHERE i.order_id = o.id) >= 2",
        // 5: same but unqualified inner ref
        "SELECT u.name, o.id FROM users u JOIN orders o ON o.user_id = u.id WHERE (SELECT COUNT(*) FROM items WHERE items.order_id = o.id) >= 2",
        // 6: correlated over join with scalar in projection (passed in differential?)
        "SELECT u.name, (SELECT SUM(i.qty) FROM items i WHERE i.order_id = o.id) FROM users u JOIN orders o ON o.user_id = u.id",
        // 7: LEFT JOIN + correlated (passed in differential)
        "SELECT u.name, (SELECT COUNT(*) FROM items i WHERE i.order_id = o.id) FROM users u LEFT JOIN orders o ON o.user_id = u.id",
    ];
    for sql in cases {
        match db.query(sql, []) {
            Ok(rows) => println!(
                "{:.66} => {} rows: {:?}",
                sql,
                rows.len(),
                rows.iter().take(4).collect::<Vec<_>>()
            ),
            Err(e) => println!("{:.66} => ERR {}", sql, e),
        }
    }
}
