fn main() {
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (a INT)", []).unwrap();
    db.execute("INSERT INTO t VALUES (10), (20)", []).unwrap();
    for sql in [
        "SELECT - 10",
        "SELECT -10",
        "SELECT 5 - - 3",
        "SELECT 5 - -3",
        "SELECT 5 - - ( 3 )",
        "SELECT + 10",
        "SELECT - 10 BETWEEN 8 AND 95",
        "SELECT ( - 10 ) BETWEEN 8 AND 95",
        "SELECT - 10 + 0",
        "SELECT - 10 * 2",
        "SELECT 3 - - 10",
    ] {
        let r = db
            .query(sql, [])
            .map(|rows| {
                rows.iter()
                    .map(|r| r[0].as_text().to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_else(|e| format!("ERR {e}"));
        let rc = rusqlite::Connection::open_in_memory().unwrap();
        let s = rc
            .query_row(sql, [], |r| r.get::<_, String>(0))
            .unwrap_or_else(|e| format!("ERR {e}"));
        let tag = if r == s { "OK  " } else { "DIFF" };
        println!("{tag} {sql:40} eng={r} sqli={s}");
    }
}
