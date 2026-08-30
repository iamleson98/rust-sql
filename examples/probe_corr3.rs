//! Reproduce case-4 failure with the exact original data.
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

    // Sanity: order 1 has 2 items.
    println!("items of order 1: {:?}", db.query("SELECT COUNT(*) FROM items WHERE order_id = 1", []).unwrap());

    let sql = "SELECT u.name, o.id FROM users u JOIN orders o ON o.user_id = u.id WHERE (SELECT COUNT(*) FROM items i WHERE i.order_id = o.id) >= 2";
    println!("case4: {:?}", db.query(sql, []).map(|r| r.len()));

    // Variation: subquery alone in projection over the same join
    let sql2 = "SELECT u.name, o.id, (SELECT COUNT(*) FROM items i WHERE i.order_id = o.id) FROM users u JOIN orders o ON o.user_id = u.id";
    println!("proj:  {:?}", db.query(sql2, []).unwrap());

    // Variation: >= 2 in projection with WHERE on the count
    let sql3 = "SELECT u.name, o.id FROM users u JOIN orders o ON o.user_id = u.id WHERE 2 <= (SELECT COUNT(*) FROM items i WHERE i.order_id = o.id)";
    println!("reversed cmp: {:?}", db.query(sql3, []).map(|r| r.len()));

    // Variation: WHERE with subquery + another local conjunct
    let sql4 = "SELECT u.name, o.id FROM users u JOIN orders o ON o.user_id = u.id WHERE o.total > 0 AND (SELECT COUNT(*) FROM items i WHERE i.order_id = o.id) >= 2";
    println!("with conjunct: {:?}", db.query(sql4, []).map(|r| r.len()));
}
