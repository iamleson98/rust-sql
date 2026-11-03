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

/// PRAGMA case_sensitive_like: LIKE turns case-sensitive (operator and
/// like() function forms, every fast path), the index range follows the
/// BINARY rule, and turning it off restores the default — against SQLite.
#[test]
fn case_sensitive_like_matches_sqlite() {
    let setup = [
        "CREATE TABLE t1(x TEXT UNIQUE)",
        "CREATE TABLE t2(x TEXT UNIQUE COLLATE nocase)",
        "CREATE TABLE t3(x TEXT, y INT)",
    ];
    let values = [
        "'a'",
        "'ab'",
        "'abc'",
        "'abcd'",
        "'ABC'",
        "'ABC abc xyz'",
        "'Abd'",
        "'bcd'",
        "'xyz'",
        "'aBc'",
        "x'616263'",
        "1",
        "'12'",
        "'ZZ-uu'",
        "'zZ-lu'",
        "'Zz-ul'",
        "'zz-ll'",
    ];
    let queries = [
        "SELECT x FROM t1 WHERE x LIKE 'abc' ORDER BY 1",
        "SELECT x FROM t1 WHERE x LIKE 'abc%' ORDER BY 1",
        "SELECT x FROM t1 WHERE x LIKE 'a%' ORDER BY 1",
        "SELECT x FROM t1 WHERE x LIKE 'A%' ORDER BY 1",
        "SELECT x FROM t2 WHERE x LIKE 'abc%' ORDER BY 1",
        "SELECT x FROM t2 WHERE x LIKE 'ABC%' ORDER BY 1",
        "SELECT x FROM t2 WHERE x LIKE 'zZ%' ORDER BY 1",
        "SELECT x FROM t2 WHERE x LIKE 'ZZ%' ORDER BY 1",
        "SELECT x FROM t2 WHERE x GLOB 'zZ*' ORDER BY 1",
        "SELECT x FROM t1 WHERE x LIKE 'zZ%' ORDER BY 1",
        "SELECT x FROM t1 WHERE x GLOB 'Zz*' ORDER BY 1",
        "SELECT x FROM t3 WHERE x LIKE '%bc%' ORDER BY 1",
        "SELECT x FROM t3 WHERE x LIKE '%BC%' ORDER BY 1",
        "SELECT x FROM t3 WHERE x NOT LIKE 'a%' ORDER BY 1",
        "SELECT x FROM t3 WHERE x LIKE 'a_c' ESCAPE '\\' ORDER BY 1",
        "SELECT x FROM t3 WHERE like('%B%', x) ORDER BY 1",
        "SELECT 'ABC' LIKE 'abc', 'abc' LIKE 'abc', like('A%', 'abc')",
    ];
    let mut db = Database::open_in_memory().unwrap();
    let con = rusqlite::Connection::open_in_memory().unwrap();
    for s in setup {
        db.execute(s, []).unwrap();
        con.execute_batch(s).unwrap();
    }
    for t in ["t1", "t2", "t3"] {
        for v in values {
            let sql = if t == "t3" {
                format!("INSERT INTO {t} VALUES({v}, 1)")
            } else {
                format!("INSERT OR IGNORE INTO {t} VALUES({v})")
            };
            db.execute(&sql, []).unwrap();
            con.execute_batch(&sql).unwrap();
        }
    }
    for mode in ["on", "off", "1", "0"] {
        let pragma = format!("PRAGMA case_sensitive_like = {mode}");
        db.execute(&pragma, []).unwrap();
        con.execute_batch(&pragma).unwrap();
        for q in queries {
            let ours: Vec<String> = db
                .query(q, [])
                .unwrap()
                .iter()
                .map(|r| r.iter().map(render).collect::<Vec<_>>().join("|"))
                .collect();
            let mut stmt = con.prepare(q).unwrap();
            let n = stmt.column_count();
            let theirs: Vec<String> = stmt
                .query_map([], |r| {
                    Ok((0..n)
                        .map(|i| {
                            let v: rusqlite::types::Value = r.get(i).unwrap();
                            render(&match v {
                                rusqlite::types::Value::Null => Value::Null,
                                rusqlite::types::Value::Integer(i) => Value::Integer(i),
                                rusqlite::types::Value::Real(f) => Value::Real(f),
                                rusqlite::types::Value::Text(t) => Value::Text(t.as_str().into()),
                                rusqlite::types::Value::Blob(b) => Value::Blob(b),
                            })
                        })
                        .collect::<Vec<_>>()
                        .join("|"))
                })
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            assert_eq!(ours, theirs, "case_sensitive_like={mode}: {q}");
        }
    }
}
