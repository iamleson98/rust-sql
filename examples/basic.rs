//! Basic usage example: create a table, insert data, query it back.

use rustqlite::{Database, Value};

fn main() {
    let mut db = Database::open_in_memory().unwrap();

    // Create a table.
    db.execute(
        "CREATE TABLE users (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            email TEXT UNIQUE,
            age INTEGER
        )",
        [],
    )
    .unwrap();

    // Insert some rows.
    db.execute(
        "INSERT INTO users (name, email, age) VALUES ('Alice', 'alice@example.com', 30)",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO users (name, email, age) VALUES ('Bob', 'bob@example.com', 25)",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO users (name, email, age) VALUES ('Charlie', 'charlie@example.com', 35)",
        [],
    )
    .unwrap();

    // Query all rows.
    println!("All users:");
    let rows = db
        .query("SELECT id, name, email, age FROM users ORDER BY age", [])
        .unwrap();
    for row in &rows {
        println!("  {} | {} | {} | {}", row[0], row[1], row[2], row[3]);
    }

    // Query with a filter.
    println!("\nUsers over 28:");
    let rows = db
        .query(
            "SELECT name, age FROM users WHERE age > 28 ORDER BY age",
            [],
        )
        .unwrap();
    for row in &rows {
        println!("  {} (age {})", row[0], row[1]);
    }

    // Aggregate.
    println!("\nStats:");
    let rows = db
        .query(
            "SELECT COUNT(*), MIN(age), MAX(age), AVG(age) FROM users",
            [],
        )
        .unwrap();
    let row = &rows[0];
    println!(
        "  count={} min={} max={} avg={}",
        row[0], row[1], row[2], row[3]
    );

    // Update.
    db.execute("UPDATE users SET age = 31 WHERE name = 'Alice'", [])
        .unwrap();
    let rows = db
        .query("SELECT name, age FROM users WHERE name = 'Alice'", [])
        .unwrap();
    println!("\nAfter update: Alice is now {}", rows[0][1]);

    // Delete.
    db.execute("DELETE FROM users WHERE name = 'Bob'", [])
        .unwrap();
    let rows = db.query("SELECT COUNT(*) FROM users", []).unwrap();
    println!("After delete: {} users remaining", rows[0][0]);

    // Group by.
    println!("\nGroup by age bucket:");
    let rows = db
        .query(
            "SELECT CASE WHEN age < 30 THEN 'young' ELSE 'old' END AS bucket, COUNT(*) FROM users GROUP BY bucket",
            [],
        )
        .unwrap();
    for row in &rows {
        println!("  {}: {} users", row[0], row[1]);
    }

    // Bound parameters.
    let _ = Value::Integer(30);
    println!("\nParameterized query (age > 30):");
    let rows = db
        .query(
            "SELECT name FROM users WHERE age > ?",
            vec![Value::Integer(30)],
        )
        .unwrap();
    for row in &rows {
        println!("  {}", row[0]);
    }
}
