//! Unindexed scans with BOUND parameters must answer exactly like the
//! same query with literals: `col BETWEEN ? AND ?` (integer fast path
//! through the bound's comparison affinity), `col LIKE ?` (bound
//! `%needle%` patterns decided on the stored bytes) and `length(col)`
//! aggregate arguments (read from the selective decode). Mixed storage
//! classes, NULLs, embedded NULs, case folding and an ADD COLUMN default
//! cover the fallbacks. File-backed, so text decodes take the validating
//! path.

use rustqlite::{Database, Value};

fn lit(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Integer(i) => i.to_string(),
        Value::Real(r) => format!("{r:?}"),
        Value::Text(t) => format!("'{}'", t.as_str().replace('\'', "''")),
        Value::Blob(b) => format!(
            "x'{}'",
            b.iter().map(|x| format!("{x:02x}")).collect::<String>()
        ),
    }
}

/// Run `sql` with `?` placeholders bound, and with them inlined as
/// literals; the answers must agree.
fn check(db: &Database, sql: &str, params: &[Value]) {
    let bound = db.query(sql, params.to_vec()).unwrap();
    let mut inlined = String::new();
    let mut it = params.iter();
    for ch in sql.chars() {
        if ch == '?' {
            inlined.push_str(&lit(it.next().unwrap()));
        } else {
            inlined.push(ch);
        }
    }
    let literal = db.query(&inlined, []).unwrap();
    assert_eq!(
        format!("{bound:?}"),
        format!("{literal:?}"),
        "{sql} with {params:?}"
    );
}

#[test]
fn bound_scan_predicates_match_literals() {
    let path = std::env::temp_dir().join(format!("bound_scan_preds_{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut db = Database::open(&path).unwrap();
    db.execute("CREATE TABLE z(a INTEGER, b INTEGER, c TEXT)", [])
        .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 0..400i64 {
        let b = match i % 9 {
            0 => Value::Null,
            1 => Value::Real(i as f64 + 0.5),
            2 => Value::Text(format!("{i}").into()),
            _ => Value::Integer(i),
        };
        let c = match i % 11 {
            0 => Value::Null,
            1 => Value::Integer(i * 7),
            2 => Value::Blob(format!("on{i}").into_bytes()),
            3 => Value::Text(format!("ON{i}\0on").into()),
            4 => Value::Text(format!("crème {i} ONE").into()),
            _ => Value::Text(format!("two thousand {i} one").into()),
        };
        db.execute("INSERT INTO z VALUES (?, ?, ?)", [Value::Integer(i), b, c])
            .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    // Rows written before the column existed read its default.
    db.execute("ALTER TABLE z ADD COLUMN d TEXT DEFAULT 'one-on'", [])
        .unwrap();
    db.execute("INSERT INTO z VALUES (1000, 1000, 'tw', 'x')", [])
        .unwrap();

    let agg =
        "SELECT count(*), avg(b), sum(length(c)), group_concat(c), sum(length(d)) FROM z WHERE ";
    let t = |s: &str| Value::Text(s.into());
    for (lo, hi) in [
        (Value::Integer(10), Value::Integer(90)),
        (Value::Integer(-5), Value::Integer(1000)),
        (Value::Real(10.5), Value::Integer(200)),
        (t("20"), t("300")),
        (Value::Null, Value::Integer(50)),
    ] {
        check(
            &db,
            &format!("{agg}b BETWEEN ? AND ?"),
            &[lo.clone(), hi.clone()],
        );
        check(
            &db,
            &format!("{agg}b NOT BETWEEN ? AND ?"),
            &[lo.clone(), hi.clone()],
        );
        check(
            &db,
            "SELECT a, b, c FROM z WHERE b BETWEEN ? AND ? ORDER BY a LIMIT 7",
            &[lo, hi],
        );
    }
    for pat in ["%on%", "%ON%", "%tw%", "%7%", "%è%", "on%", "%o_e%", "%"] {
        check(&db, &format!("{agg}c LIKE ?"), &[t(pat)]);
        check(&db, &format!("{agg}c NOT LIKE ?"), &[t(pat)]);
        check(&db, &format!("{agg}d LIKE ?"), &[t(pat)]);
        check(
            &db,
            &format!("{agg}c LIKE ? AND b BETWEEN ? AND ?"),
            &[t(pat), Value::Integer(5), Value::Integer(300)],
        );
        check(
            &db,
            "SELECT a, b, c FROM z WHERE c LIKE ? ORDER BY a LIMIT 10",
            &[t(pat)],
        );
    }
    check(&db, &format!("{agg}c LIKE ?"), &[Value::Integer(7)]);
    check(&db, &format!("{agg}c LIKE ?"), &[Value::Null]);
    drop(db);
    let _ = std::fs::remove_file(&path);
}
