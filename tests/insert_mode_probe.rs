//! Micro-benchmark: multi-VALUES INSERT cost — literal vs param,
//! WAL vs DELETE journal mode. Isolates the vacuum-copy slowness.
use rustqlite::{Database, Value};
use std::time::Instant;

fn rows_literal(sql: &mut String, n: usize) {
    sql.push_str("INSERT INTO t (id, k, note) VALUES ");
    for j in 0..n {
        if j > 0 {
            sql.push(',');
        }
        sql.push_str(&format!(
            "({}, {}, 'n{:08x}')",
            j + 1,
            j % 1000,
            (j as u64).wrapping_mul(2654435761) & 0xffff
        ));
    }
}

#[test]
fn insert_bigtxn() {
    // The vacuum-copy shape: 600 statements x 2000 rows in ONE txn.
    for (label, wal) in [("BIG-DEL", false), ("BIG-WAL", true)] {
        let n: usize = 2000;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.db");
        let mut db = Database::open(&path).unwrap();
        if wal {
            db.execute("PRAGMA journal_mode = WAL", []).unwrap();
            db.execute("PRAGMA synchronous = NORMAL", []).unwrap();
        }
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, k INTEGER, note TEXT)",
            [],
        )
        .unwrap();
        db.execute("BEGIN", []).unwrap();
        let t = Instant::now();
        for s in 0..600 {
            let mut sql = String::new();
            sql.push_str("INSERT INTO t (id, k, note) VALUES ");
            let mut flat: Vec<Value> = Vec::with_capacity(n * 3);
            for j in 0..n {
                if j > 0 {
                    sql.push(',');
                }
                sql.push_str("(?,?,?)");
                let id = s * n + j + 1;
                flat.push(Value::Integer(id as i64));
                flat.push(Value::Integer((id % 1_000_000) as i64));
                flat.push(Value::Text(
                    format!("n{:08x}", (id as u64) & 0xffff_ffff).into(),
                ));
            }
            db.execute(&sql, flat).unwrap();
            if s % 100 == 0 {
                println!("[bt {label}] stmt {s}: {:.2}s", t.elapsed().as_secs_f64());
            }
        }
        let ins_s = t.elapsed().as_secs_f64();
        db.execute("COMMIT", []).unwrap();
        let total_s = t.elapsed().as_secs_f64();
        let cnt: i64 = db.query("SELECT count(*) FROM t", []).unwrap()[0][0].as_integer();
        println!(
            "[bt] {label} 600x2000 rows: statements={ins_s:.1}s total={total_s:.1}s count={cnt} rate={:.0} rows/s",
            (n * 600) as f64 / total_s.max(1e-9)
        );
        drop(db);
    }
}

#[test]
fn insert_mode_matrix() {
    let n: usize = 2000;
    for (label, wal, params) in [
        ("WAL+literal", true, false),
        ("WAL+params", true, true),
        ("DEL+literal", false, false),
        ("DEL+params", false, true),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.db");
        let mut db = Database::open(&path).unwrap();
        if wal {
            db.execute("PRAGMA journal_mode = WAL", []).unwrap();
            db.execute("PRAGMA synchronous = NORMAL", []).unwrap();
        }
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, k INTEGER, note TEXT)",
            [],
        )
        .unwrap();
        db.execute("BEGIN", []).unwrap();
        // 10 statements of n rows each
        let t = Instant::now();
        for s in 0..10 {
            if params {
                let mut sql = String::new();
                sql.push_str("INSERT INTO t (id, k, note) VALUES ");
                let mut flat: Vec<Value> = Vec::with_capacity(n * 3);
                for j in 0..n {
                    if j > 0 {
                        sql.push(',');
                    }
                    sql.push_str("(?,?,?)");
                    let id = s * n + j + 1;
                    flat.push(Value::Integer(id as i64));
                    flat.push(Value::Integer((j % 1000) as i64));
                    flat.push(Value::Text(
                        format!("n{:08x}", (j as u64).wrapping_mul(2654435761) & 0xffff).into(),
                    ));
                }
                db.execute(&sql, flat).unwrap();
            } else {
                let mut sql = String::new();
                rows_literal(&mut sql, n);
                // fix ids per statement chunk
                let base = s * n;
                let body = sql.rsplit_once("VALUES ").unwrap().1.to_string();
                let mut fixed = String::from("INSERT INTO t (id, k, note) VALUES ");
                for (j, tuple) in body.split("),(").enumerate() {
                    if j > 0 {
                        fixed.push_str("),(");
                    } else {
                        fixed.push('(');
                    }
                    let mut parts = tuple.trim_matches(|c| c == '(' || c == ')').split(", ");
                    let _old_id = parts.next().unwrap();
                    let rest: Vec<&str> = parts.collect();
                    fixed.push_str(&format!("{}, {}", base + j + 1, rest.join(", ")));
                }
                fixed.push(')');
                db.execute(&fixed, []).unwrap();
            }
        }
        let ins_s = t.elapsed().as_secs_f64();
        db.execute("COMMIT", []).unwrap();
        let total_s = t.elapsed().as_secs_f64();
        let cnt: i64 = db.query("SELECT count(*) FROM t", []).unwrap()[0][0].as_integer();
        println!(
            "[im] {label:12} 10x{n} rows: statements={ins_s:.3}s total(incl commit)={total_s:.3}s count={cnt} rate={:.0} rows/s",
            (n * 10) as f64 / total_s.max(1e-9)
        );
        drop(db);
    }
}
