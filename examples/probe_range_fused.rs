//! Fused range-probe scan correctness: BETWEEN / AND-chains / equality /
//! rowid-alias ranges / mixed-type column values / NULL / REAL / TEXT.
//! Every shape cross-checked against the expected row set (table scans
//! emit rows in rowid order). REAL- and TEXT-typed bounds decline the
//! fused path and take the general predicate evaluator — the cases below
//! pin BOTH routes to identical, SQLite-exact semantics.
use rustqlite::{Database, Value};

fn rows(db: &Database, sql: &str) -> Vec<Vec<String>> {
    db.query(sql, [])
        .unwrap()
        .into_iter()
        .map(|r| {
            r.iter()
                .map(|v| match v {
                    Value::Null => "NULL".into(),
                    Value::Integer(i) => i.to_string(),
                    Value::Real(f) => format!("{f:.1}"),
                    Value::Text(t) => format!("'{}'", t.as_str()),
                    Value::Blob(b) => format!("blob({})", b.len()),
                })
                .collect()
        })
        .collect()
}

fn count(db: &Database, sql: &str, params: impl IntoIterator<Item = Value>) -> i64 {
    let params: Vec<Value> = params.into_iter().collect();
    match db
        .query(sql, params)
        .unwrap()
        .first()
        .and_then(|r| r.first())
    {
        Some(Value::Integer(n)) => *n,
        other => panic!("{sql}: expected INTEGER count, got {other:?}"),
    }
}

fn main() {
    let mut db = Database::open_in_memory().unwrap();
    db.set_deferred_flush(true);
    // Mixed-type column `a`: integers, a REAL inside/outside ranges, NULL,
    // TEXT below/above numbers, plus a rowid-alias table.
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, name TEXT)",
        [],
    )
    .unwrap();
    let mut id = 0i64;
    let mut ins = |a: Value, name: &str, db: &mut Database| {
        id += 1;
        db.execute(
            "INSERT INTO t (a, name) VALUES (?, ?)",
            [a, Value::Text(name.into())],
        )
        .unwrap();
    };
    ins(Value::Integer(5), "five", &mut db);
    ins(Value::Integer(10), "ten", &mut db);
    ins(Value::Integer(15), "fifteen", &mut db);
    ins(Value::Integer(20), "twenty", &mut db);
    ins(Value::Real(12.5), "real125", &mut db); // REAL inside [10, 15]
    ins(Value::Real(99.5), "real995", &mut db); // REAL outside
    ins(Value::Null, "null", &mut db);
    ins(Value::Text("text-low".into()), "textlow", &mut db);
    ins(Value::Text("zzz".into()), "textzzz", &mut db);
    ins(Value::Integer(-3), "neg", &mut db);
    ins(Value::Integer(5), "five2", &mut db); // duplicate

    // Table scans emit rows in rowid order (ids 1..=11).
    let cases: Vec<(&str, Vec<Vec<String>>)> = vec![
        (
            // fused probe: INTEGER bounds + REAL payload (12.5) in range
            "SELECT id, name FROM t WHERE a BETWEEN 8 AND 16",
            vec![
                vec!["2".into(), "'ten'".into()],
                vec!["3".into(), "'fifteen'".into()],
                vec!["5".into(), "'real125'".into()],
            ],
        ),
        (
            "SELECT id, name FROM t WHERE a >= 10 AND a <= 20",
            vec![
                vec!["2".into(), "'ten'".into()],
                vec!["3".into(), "'fifteen'".into()],
                vec!["4".into(), "'twenty'".into()],
                vec!["5".into(), "'real125'".into()],
            ],
        ),
        (
            "SELECT id, name FROM t WHERE 10 <= a AND a <= 15",
            vec![
                vec!["2".into(), "'ten'".into()],
                vec!["3".into(), "'fifteen'".into()],
                vec!["5".into(), "'real125'".into()],
            ],
        ),
        (
            // equality → degenerate range (fused)
            "SELECT id, name FROM t WHERE a = 5",
            vec![
                vec!["1".into(), "'five'".into()],
                vec!["11".into(), "'five2'".into()],
            ],
        ),
        (
            // NULL never matches; all 8 numeric rows (incl. REALs ±) hit
            "SELECT COUNT(*) FROM t WHERE a BETWEEN -100 AND 100",
            vec![vec!["8".into()]],
        ),
        (
            // TEXT/BLOB values are above all numbers: never in an int range
            "SELECT COUNT(*) FROM t WHERE a BETWEEN 1 AND 1000000",
            vec![vec!["7".into()]],
        ),
        (
            // outside range on the left
            "SELECT id FROM t WHERE a BETWEEN -10 AND -1",
            vec![vec!["10".into()]],
        ),
        (
            // rowid-alias range (fused seek): ids 2..=4
            "SELECT id, name FROM t WHERE id BETWEEN 2 AND 4",
            vec![
                vec!["2".into(), "'ten'".into()],
                vec!["3".into(), "'fifteen'".into()],
                vec!["4".into(), "'twenty'".into()],
            ],
        ),
        (
            // rowid equality
            "SELECT name FROM t WHERE id = 7",
            vec![vec!["'null'".into()]],
        ),
        (
            // inverted range: always false for every value class
            "SELECT COUNT(*) FROM t WHERE a BETWEEN 20 AND 5",
            vec![vec!["0".into()]],
        ),
        (
            // one-sided (NOT fused — must take the general path and still
            // match TEXT rows per type order)
            "SELECT COUNT(*) FROM t WHERE a > 20",
            vec![vec!["3".into()]], // real995, textlow, textzzz
        ),
        (
            // NOT BETWEEN declines the fused path; NULL excluded
            "SELECT COUNT(*) FROM t WHERE a NOT BETWEEN 5 AND 20",
            vec![vec!["4".into()]], // neg, real995, textlow, textzzz
        ),
    ];
    let mut pass = 0;
    for (sql, want) in &cases {
        let got = rows(&db, sql);
        if got == *want {
            pass += 1;
        } else {
            println!("FAIL {sql}\n  got  {got:?}\n  want {want:?}");
        }
    }
    assert_eq!(
        pass,
        cases.len(),
        "{}/{} row-set cases passed",
        pass,
        cases.len()
    );

    // Bounds-typing: REAL / TEXT bounds must NOT truncate into the fused
    // range — they decline to the general path with full mixed-type
    // semantics.
    // ten(10), fifteen(15), real125(12.5) ∈ [8.5, 15.5]
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM t WHERE a BETWEEN 8.5 AND 15.5",
            []
        ),
        3
    );
    // INTEGER params fuse: 10, 12.5, 15 ∈ [10, 15]
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM t WHERE a BETWEEN ? AND ?",
            [Value::Integer(10), Value::Integer(15)]
        ),
        3
    );
    // REAL params decline — same answer as REAL literals
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM t WHERE a BETWEEN ? AND ?",
            [Value::Real(8.5), Value::Real(15.5)]
        ),
        3
    );
    // TEXT bounds: numbers < TEXT, so only TEXT rows can match;
    // 'text-low' ∈ ['5', 'z'], 'zzz' > 'z'
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM t WHERE a BETWEEN '5' AND 'z'",
            []
        ),
        1
    );
    // TEXT param bound declines as well
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM t WHERE a >= ? AND a <= ?",
            [Value::Text("5".into()), Value::Text("z".into())]
        ),
        1
    );

    println!(
        "fused range scan: {}/{} row-set cases + 5 bounds-typing cases passed",
        pass,
        cases.len()
    );
}
