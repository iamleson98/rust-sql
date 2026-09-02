//! Transaction example: insert multiple rows atomically.

use rustqlite::Database;

fn main() {
    let mut db = Database::open_in_memory().unwrap();

    db.execute(
        "CREATE TABLE accounts (id INTEGER PRIMARY KEY, name TEXT, balance INTEGER)",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO accounts (name, balance) VALUES ('Alice', 1000)",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO accounts (name, balance) VALUES ('Bob', 500)",
        [],
    )
    .unwrap();

    // Transfer 200 from Alice to Bob in a transaction.
    db.execute("BEGIN", []).unwrap();
    db.execute(
        "UPDATE accounts SET balance = balance - 200 WHERE name = 'Alice'",
        [],
    )
    .unwrap();
    db.execute(
        "UPDATE accounts SET balance = balance + 200 WHERE name = 'Bob'",
        [],
    )
    .unwrap();
    db.execute("COMMIT", []).unwrap();

    let rows = db
        .query("SELECT name, balance FROM accounts ORDER BY name", [])
        .unwrap();
    for row in &rows {
        println!("{}: {}", row[0], row[1]);
    }
    // Alice: 800, Bob: 700
}
