//! Single-row INSERT streams into indexed tables: clustered keys land in
//! the leaf their predecessor landed in without a root-to-leaf descent
//! (the placement hint), scattered keys take the full descent, oversized
//! keys always do. Whatever path an entry takes, every index must stay
//! ordered and complete — checked with `PRAGMA integrity_check` and by
//! comparing every index-driven answer with a NOT INDEXED scan.

use rustqlite::{Database, Value};

fn count(db: &Database, sql: &str, v: Value) -> i64 {
    db.query(sql, [v]).unwrap()[0][0].as_integer()
}

fn assert_indexes_agree(db: &Database, probes: &[i64]) {
    let ok = db.query("PRAGMA integrity_check", []).unwrap();
    assert_eq!(ok, vec![vec![Value::Text("ok".into())]]);
    for &p in probes {
        for (col, v) in [
            ("d", Value::Integer(p % 97)),
            ("a", Value::Integer((p * 7919) % 10_007)),
            ("c", Value::Text(format!("k{p}").into())),
        ] {
            let via_index = count(
                db,
                &format!("SELECT count(*) FROM t WHERE {col} = ?"),
                v.clone(),
            );
            let via_scan = count(
                db,
                &format!("SELECT count(*) FROM t NOT INDEXED WHERE {col} = ?"),
                v,
            );
            assert_eq!(via_index, via_scan, "{col} probe {p}");
        }
        let both = db
            .query(
                "SELECT count(*) FROM t WHERE d = ? AND a > ?",
                [Value::Integer(p % 97), Value::Integer(5_000)],
            )
            .unwrap()[0][0]
            .as_integer();
        let both_scan = db
            .query(
                "SELECT count(*) FROM t NOT INDEXED WHERE d = ? AND a > ?",
                [Value::Integer(p % 97), Value::Integer(5_000)],
            )
            .unwrap()[0][0]
            .as_integer();
        assert_eq!(both, both_scan, "(d, a) probe {p}");
    }
}

#[test]
fn clustered_and_scattered_index_streams_stay_ordered() {
    let mut db = Database::open_in_memory().unwrap();
    for sql in [
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, c TEXT, d INTEGER, big TEXT)",
        "CREATE INDEX t_a ON t(a)",
        "CREATE INDEX t_c ON t(c)",
        "CREATE INDEX t_d ON t(d)",
        "CREATE INDEX t_da ON t(d, a)",
        "CREATE INDEX t_big ON t(big)",
    ] {
        db.execute(sql, []).unwrap();
    }
    db.execute("BEGIN", []).unwrap();
    for i in 1..=20_000i64 {
        // Every 37th row carries a key too large for an in-page cell
        // (an overflow chain): the placement path must decline it.
        let big = if i % 37 == 0 {
            format!("{:0>3000}", i % 50)
        } else {
            format!("b{}", i % 300)
        };
        db.execute(
            "INSERT INTO t (id, a, c, d, big) VALUES (?, ?, ?, ?, ?)",
            [
                Value::Integer(i),
                Value::Integer((i * 7919) % 10_007),
                Value::Text(format!("k{i}").into()),
                Value::Integer(i % 97),
                Value::Text(big.into()),
            ],
        )
        .unwrap();
        // Interleaved deletes: leaves shrink under the carried pins.
        if i % 11 == 0 {
            db.execute("DELETE FROM t WHERE id = ?", [Value::Integer(i - 5)])
                .unwrap();
        }
    }
    db.execute("COMMIT", []).unwrap();
    let probes: Vec<i64> = (1..=20_000).step_by(733).collect();
    assert_indexes_agree(&db, &probes);
    let n = db.query("SELECT count(*) FROM t", []).unwrap()[0][0].as_integer();
    assert_eq!(n, 20_000 - 20_000 / 11);
    let big_rows = count(
        &db,
        "SELECT count(*) FROM t WHERE big = ?",
        Value::Text(format!("{:0>3000}", 0).into()),
    );
    let big_scan = count(
        &db,
        "SELECT count(*) FROM t NOT INDEXED WHERE big = ?",
        Value::Text(format!("{:0>3000}", 0).into()),
    );
    assert_eq!(big_rows, big_scan);

    // A second stream in autocommit mode (carried states across
    // statements, every statement its own transaction).
    for i in 20_001..=23_000i64 {
        db.execute(
            "INSERT INTO t (id, a, c, d, big) VALUES (?, ?, ?, ?, ?)",
            [
                Value::Integer(i),
                Value::Integer((i * 7919) % 10_007),
                Value::Text(format!("k{i}").into()),
                Value::Integer(i % 97),
                Value::Text(format!("b{}", i % 300).into()),
            ],
        )
        .unwrap();
    }
    assert_indexes_agree(&db, &[20_001, 21_500, 22_999, 7, 15_000]);
}
