//! The LIKE / GLOB prefix range on non-TEXT columns (SQLite's
//! isLikeOrGlob numeric guard): the index range must never lose a row a
//! full scan finds — numbers, text and blobs mixed, against SQLite.
use rustqlite::{Database, Value};

fn render(v: &Value) -> String {
    format!("{v:?}")
}

#[test]
fn like_range_on_non_text_columns_matches_sqlite() {
    let setup = [
        "CREATE TABLE a(x UNIQUE COLLATE nocase)",
        "CREATE TABLE b(x INT UNIQUE COLLATE binary)",
        "CREATE TABLE c(x NUMERIC)",
        "CREATE INDEX c_x ON c(x COLLATE nocase)",
    ];
    let values = [
        "'/abc'",
        "123",
        "-234",
        "'-1x'",
        "'12a'",
        "1.5",
        "'1.5'",
        "'.1%'",
        "'abc'",
        "'ABD'",
        "x'616263'",
        "x'2f61'",
        "'+/'",
        "'./'",
        "-1",
        "'1e5'",
        "'e'",
        "'-'",
    ];
    let patterns = [
        "'/%'", "'/a%'", "'12%'", "'-2%'", "'-%'", "'1%'", "'1.%'", "'.1%'", "'ab%'", "'a%'",
        "'+/'", "'./'", "'1e%'", "'e%'", "'-1%'",
    ];
    let mut db = Database::open_in_memory().unwrap();
    let con = rusqlite::Connection::open_in_memory().unwrap();
    for s in setup {
        db.execute(s, []).unwrap();
        con.execute_batch(s).unwrap();
    }
    for t in ["a", "b", "c"] {
        for v in values {
            let sql = format!("INSERT OR IGNORE INTO {t} VALUES({v})");
            db.execute(&sql, []).unwrap();
            con.execute_batch(&sql).unwrap();
        }
    }
    for t in ["a", "b", "c"] {
        for p in patterns {
            for op in ["LIKE", "GLOB"] {
                let p = if op == "GLOB" {
                    p.replace('%', "*")
                } else {
                    p.to_string()
                };
                let sql = format!("SELECT quote(x) FROM {t} WHERE x {op} {p} ORDER BY quote(x)");
                let ours: Vec<String> = db
                    .query(&sql, [])
                    .unwrap()
                    .iter()
                    .map(|r| render(&r[0]))
                    .collect();
                let mut stmt = con.prepare(&sql).unwrap();
                let theirs: Vec<String> = stmt
                    .query_map([], |r| r.get::<_, String>(0))
                    .unwrap()
                    .map(|s| render(&Value::Text(s.unwrap().as_str().into())))
                    .collect();
                assert_eq!(ours, theirs, "{sql}");
            }
        }
    }
}

/// LIKE ... ESCAPE keeps the prefix range (like3.test 6.x): escaped
/// wildcards are literal prefix characters.
#[test]
fn like_escape_prefix_matches_sqlite() {
    let mut db = Database::open_in_memory().unwrap();
    let con = rusqlite::Connection::open_in_memory().unwrap();
    for s in [
        "CREATE TABLE t(path TEXT COLLATE nocase PRIMARY KEY, a) WITHOUT ROWID",
        "INSERT INTO t VALUES('a%b',1),('A_c',2),('ab',3),('a\\x',4),('b',5),('a%',6)",
    ] {
        db.execute(s, []).unwrap();
        con.execute_batch(s).unwrap();
    }
    for (p, e) in [
        ("a%", "x"),
        ("ax%%", "x"),
        ("ax_%", "x"),
        ("a\\%", "\\"),
        ("a\\%%", "\\"),
        ("a^%", "^"),
        ("ax", "x"),
    ] {
        let sql = format!("SELECT path FROM t WHERE path LIKE '{p}' ESCAPE '{e}' ORDER BY a");
        let ours: Vec<String> = db
            .query(&sql, [])
            .unwrap()
            .iter()
            .map(|r| render(&r[0]))
            .collect();
        let mut stmt = con.prepare(&sql).unwrap();
        let theirs: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|s| render(&Value::Text(s.unwrap().as_str().into())))
            .collect();
        assert_eq!(ours, theirs, "{sql}");
    }
}
