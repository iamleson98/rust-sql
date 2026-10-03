// Differential probe: real SQLite's PRAGMA wal_checkpoint output shapes.
use rusqlite::Connection;

fn main() {
    // Non-WAL (delete-mode) database.
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
        .unwrap();
    let mut stmt = conn.prepare("PRAGMA wal_checkpoint").unwrap();
    let cols: Vec<String> = stmt
        .column_names()
        .into_iter()
        .map(|c| c.to_string())
        .collect();
    let rows: Vec<String> = stmt
        .query_map([], |r| Ok(format!("{:?}", r.get::<_, i64>(0).unwrap())))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    println!("non-WAL cols={cols:?} row0={:?}", rows.first());
    let mut stmt = conn.prepare("PRAGMA wal_checkpoint(FULL)").unwrap();
    let cols2: Vec<String> = stmt
        .column_names()
        .into_iter()
        .map(|c| c.to_string())
        .collect();
    println!("non-WAL call cols={cols2:?}");
    for row in stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })
        .unwrap()
    {
        println!("non-WAL call row={:?}", row.unwrap());
    }

    // WAL database with committed frames.
    let dir = std::env::temp_dir().join(format!("rsql-sqprobe-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("w.db");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT);")
        .unwrap();
    for i in 0..50 {
        conn.execute("INSERT INTO t (v) VALUES (?1)", [format!("v{i}")])
            .unwrap();
    }
    // Sole-connection PASSIVE checkpoint.
    for mode in ["", "(PASSIVE)", "(FULL)", "(TRUNCATE)"] {
        let sql = if mode.is_empty() {
            "PRAGMA wal_checkpoint".to_string()
        } else {
            format!("PRAGMA wal_checkpoint{mode}")
        };
        let mut stmt = conn.prepare(&sql).unwrap();
        let cols: Vec<String> = stmt
            .column_names()
            .into_iter()
            .map(|c| c.to_string())
            .collect();
        let rows: Vec<(i64, i64, i64)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        println!("WAL {sql:?}: cols={cols:?} rows={rows:?}");
    }
    drop(conn);
    let _ = std::fs::remove_dir_all(&dir);
}
