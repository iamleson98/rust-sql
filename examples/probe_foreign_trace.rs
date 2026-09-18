//! Trace which persistence path each statement takes on a SQLite-format
//! (foreign) database: main-file rewrites vs -wal appends vs journal.
use rustqlite::Value;
use std::path::Path;

fn stat(p: &Path) -> String {
    let l = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    format!("{}", l)
}

fn snap(db_path: &Path) -> String {
    let base = db_path.to_str().unwrap();
    format!(
        "db={} wal={} journal={}",
        stat(db_path),
        stat(Path::new(&format!("{base}-wal"))),
        stat(Path::new(&format!("{base}-journal"))),
    )
}

fn main() {
    let path = Path::new("/home/z/rust-sql/probe-tmp/trace.db");
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file("/home/z/rust-sql/probe-tmp/trace.db-wal");
    let mut db = rustqlite::Database::open_sqlite_format(path).unwrap();
    println!("after open:               {}", snap(path));
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", [])
        .unwrap();
    println!("after CREATE:             {}", snap(path));
    for i in 1..=6i64 {
        let big = "x".repeat(3000);
        db.execute("INSERT INTO t (v) VALUES (?)", [Value::Text(big.into())])
            .unwrap();
        println!("after INSERT {i} (3KB):     {}", snap(path));
    }
    db.execute("UPDATE t SET v = v WHERE id = 1", []).unwrap();
    println!("after UPDATE:             {}", snap(path));
    db.execute("DELETE FROM t WHERE id = 2", []).unwrap();
    println!("after DELETE:             {}", snap(path));
    db.execute("PRAGMA journal_mode", []).unwrap();
    println!(
        "journal_mode reported:    {}",
        db.query("PRAGMA journal_mode", []).unwrap()[0][0].as_text()
    );
    drop(db);
    println!("after Drop:               {}", snap(path));
}
