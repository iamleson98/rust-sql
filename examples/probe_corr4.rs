//! Debug UPDATE SET correlated.
use rustqlite::Database;

fn main() {
    let mut db = Database::open_in_memory().unwrap();
    for s in [
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, dept TEXT, salary REAL)",
        "CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER, total INTEGER)",
        "CREATE TABLE items (id INTEGER PRIMARY KEY, order_id INTEGER, qty INTEGER)",
        "CREATE TABLE deleted_users (id INTEGER PRIMARY KEY, name TEXT)",
        "INSERT INTO users VALUES (1, 'alice', 'eng', 100.0), (2, 'bob', 'eng', 90.0), (3, 'dan', 'sales', NULL)",
        "INSERT INTO orders VALUES (1, 1, 30), (2, 1, 10), (3, 1, 20), (4, 2, 15), (5, 3, 25), (6, 3, 5)",
    ] {
        db.execute(s, []).unwrap();
    }
    db.execute("UPDATE users SET salary = (SELECT MAX(total) FROM orders o WHERE o.user_id = users.id) WHERE id <= 2", []).unwrap();
    println!("after UPDATE: {:?}", db.query("SELECT id, salary FROM users ORDER BY id", []).unwrap());
}
