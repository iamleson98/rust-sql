//! Pin the engine's autoindex synthesis for WITHOUT ROWID PK shapes
//! vs SQLite's (single-col vs composite, with/without other UNIQUEs).
use rustqlite::{Database, Value};

fn show(label: &str, ddl: &str) {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(ddl, []).unwrap();
    let master = db
        .query("SELECT type, name, rootpage FROM sqlite_master", [])
        .unwrap();
    println!("[{label}] master:");
    for r in &master {
        println!("   {:?}", r);
    }
    // Extract table name for index_list.
    let tname: String = match &master[0][1] {
        Value::Text(t) => t.to_string(),
        _ => String::new(),
    };
    let il = db
        .query(&format!("PRAGMA index_list({tname})"), [])
        .unwrap();
    println!("[{label}] index_list({tname}):");
    for r in &il {
        println!("   {:?}", r);
    }
    let ti = db
        .query(&format!("PRAGMA table_info({tname})"), [])
        .unwrap();
    println!("[{label}] table_info({tname}):");
    for r in &ti {
        println!("   {:?}", r);
    }
    println!();
}

fn main() {
    show(
        "wr-single",
        "CREATE TABLE t (a INTEGER, b TEXT, PRIMARY KEY(a)) WITHOUT ROWID",
    );
    show(
        "wr-composite",
        "CREATE TABLE t (a INTEGER, b TEXT, PRIMARY KEY(a, b)) WITHOUT ROWID",
    );
    show(
        "wr-composite-unique",
        "CREATE TABLE t (a INTEGER UNIQUE, b TEXT, PRIMARY KEY(a, b)) WITHOUT ROWID",
    );
    show(
        "rowid-composite",
        "CREATE TABLE t (a INTEGER, b TEXT, PRIMARY KEY(a, b))",
    );
    show(
        "wr-pk-notfirst",
        "CREATE TABLE t (b TEXT, a INTEGER, PRIMARY KEY(a)) WITHOUT ROWID",
    );
}
