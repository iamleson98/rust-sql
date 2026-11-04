//! Join-variant probe: timing + EXPLAIN QUERY PLAN, ours vs SQLite.
//! Usage: cargo run --release --example probe_join_variants
use rustqlite::{Database, Value};
use std::time::Instant;

fn main() {
    let mut o = Database::open_in_memory().unwrap();
    let s = rusqlite::Connection::open_in_memory().unwrap();
    let ddl =
        "CREATE TABLE t (id INTEGER PRIMARY KEY, k INTEGER, name TEXT, val INTEGER, score REAL);
         CREATE TABLE u (id INTEGER PRIMARY KEY, t_id INTEGER, amt INTEGER);
         CREATE INDEX u_t ON u(t_id);
         CREATE TABLE w (id INTEGER PRIMARY KEY, u_id INTEGER, tag TEXT);
         CREATE TABLE tw (id INTEGER PRIMARY KEY, name TEXT) WITHOUT ROWID;
         CREATE TABLE uw (id INTEGER PRIMARY KEY, tw_id INTEGER, amt INTEGER);
         CREATE TABLE ts (id INTEGER PRIMARY KEY, code INTEGER, name TEXT);
         CREATE INDEX ts_code ON ts(code);
         CREATE TABLE us (id INTEGER PRIMARY KEY, code INTEGER, amt INTEGER);";
    for q in ddl.split(';').map(str::trim).filter(|x| !x.is_empty()) {
        o.execute(q, []).unwrap();
    }
    s.execute_batch(ddl).unwrap();
    o.execute("BEGIN", []).unwrap();
    s.execute_batch("BEGIN").unwrap();
    for i in 1..=100_000i64 {
        o.execute(
            "INSERT INTO t VALUES (?,?,?,?,?)",
            [
                Value::Integer(i),
                Value::Integer(i % 100),
                Value::Text(format!("n{i}").into()),
                Value::Integer(i % 1000),
                Value::Real(1.0),
            ],
        )
        .unwrap();
        s.execute(
            "INSERT INTO t VALUES (?1,?2,?3,?4,?5)",
            rusqlite::params![i, i % 100, format!("n{i}"), i % 1000, 1.0],
        )
        .unwrap();
        o.execute(
            "INSERT INTO tw VALUES (?,?)",
            [Value::Integer(i), Value::Text(format!("n{i}").into())],
        )
        .unwrap();
        s.execute(
            "INSERT INTO tw VALUES (?1,?2)",
            rusqlite::params![i, format!("n{i}")],
        )
        .unwrap();
        o.execute(
            "INSERT INTO ts VALUES (?,?,?)",
            [
                Value::Integer(i),
                Value::Integer(i),
                Value::Text(format!("n{i}").into()),
            ],
        )
        .unwrap();
        s.execute(
            "INSERT INTO ts VALUES (?1,?2,?3)",
            rusqlite::params![i, i, format!("n{i}")],
        )
        .unwrap();
    }
    for i in 1..=300_000i64 {
        let tid = (i * 17) % 100_000 + 1;
        for tbl in ["u", "uw", "us"] {
            o.execute(
                &format!("INSERT INTO {tbl} VALUES (?,?,?)"),
                [
                    Value::Integer(i),
                    Value::Integer(tid),
                    Value::Integer(i % 1000),
                ],
            )
            .unwrap();
            s.execute(
                &format!("INSERT INTO {tbl} VALUES (?1,?2,?3)"),
                rusqlite::params![i, tid, i % 1000],
            )
            .unwrap();
        }
        o.execute(
            "INSERT INTO w VALUES (?,?,?)",
            [
                Value::Integer(i),
                Value::Integer(i),
                Value::Text("x".into()),
            ],
        )
        .unwrap();
        s.execute(
            "INSERT INTO w VALUES (?1,?2,?3)",
            rusqlite::params![i, i, "x"],
        )
        .unwrap();
    }
    o.execute("COMMIT", []).unwrap();
    s.execute_batch("COMMIT").unwrap();
    let qs = [
        ("base join BETWEEN", "SELECT u.id, t.name, u.amt FROM u JOIN t ON t.id = u.t_id WHERE u.id BETWEEN 1000 AND 1049"),
        ("reversed eq", "SELECT u.id, t.name, u.amt FROM u JOIN t ON u.t_id = t.id WHERE u.id BETWEEN 1000 AND 1049"),
        ("LEFT JOIN", "SELECT u.id, t.name, u.amt FROM u LEFT JOIN t ON t.id = u.t_id WHERE u.id BETWEEN 1000 AND 1049"),
        ("comma join", "SELECT u.id, t.name, u.amt FROM u, t WHERE t.id = u.t_id AND u.id BETWEEN 1000 AND 1049"),
        ("comma join reversed order", "SELECT u.id, t.name, u.amt FROM t, u WHERE t.id = u.t_id AND u.id BETWEEN 1000 AND 1049"),
        ("t first JOIN", "SELECT u.id, t.name, u.amt FROM t JOIN u ON t.id = u.t_id WHERE u.id BETWEEN 1000 AND 1049"),
        ("3-table chain", "SELECT w.id, u.id, t.name FROM w JOIN u ON u.id = w.u_id JOIN t ON t.id = u.t_id WHERE w.id BETWEEN 1000 AND 1049"),
        ("secondary idx join", "SELECT us.id, ts.name FROM us JOIN ts ON ts.code = us.code WHERE us.id BETWEEN 1000 AND 1049"),
        ("WITHOUT ROWID inner", "SELECT uw.id, tw.name FROM uw JOIN tw ON tw.id = uw.tw_id WHERE uw.id BETWEEN 1000 AND 1049"),
        ("count(*) join", "SELECT count(*) FROM u JOIN t ON t.id = u.t_id WHERE u.id BETWEEN 1000 AND 1049"),
        ("join u.id=?", "SELECT u.id, t.name, u.amt FROM u JOIN t ON t.id = u.t_id WHERE u.id = 1000"),
        ("join u.id<100 limit 50", "SELECT u.id, t.name, u.amt FROM u JOIN t ON t.id = u.t_id WHERE u.id < 100 LIMIT 50"),
        ("self-join", "SELECT a.id, b.name FROM t a JOIN t b ON a.id = b.id WHERE a.id < 50"),
        ("unfiltered join count (hash ok)", "SELECT count(*) FROM u JOIN t ON t.id = u.t_id"),
    ];
    for (name, q) in qs {
        let n = 20;
        let t0 = Instant::now();
        let mut c = 0;
        let mut ro: Vec<String> = vec![];
        for _ in 0..n {
            let rs = o.query(q, []).unwrap();
            c += rs.len();
            if ro.is_empty() {
                ro = rs
                    .iter()
                    .map(|r| {
                        (0..r.len())
                            .map(|i| r[i].as_text())
                            .collect::<Vec<_>>()
                            .join(",")
                    })
                    .collect();
            }
        }
        let to = t0.elapsed().as_secs_f64() / n as f64;
        let t0 = Instant::now();
        let mut c2 = 0;
        let mut rs_: Vec<String> = vec![];
        for _ in 0..n {
            let mut st = s.prepare_cached(q).unwrap();
            let nc = st.column_count();
            let mut r = st.query([]).unwrap();
            let mut cur = vec![];
            while let Some(row) = r.next().unwrap() {
                c2 += 1;
                cur.push(
                    (0..nc)
                        .map(|i| match row.get_ref(i).unwrap() {
                            rusqlite::types::ValueRef::Null => "NULL".to_string(),
                            rusqlite::types::ValueRef::Integer(i) => i.to_string(),
                            rusqlite::types::ValueRef::Real(f) => f.to_string(),
                            rusqlite::types::ValueRef::Text(t) => String::from_utf8_lossy(t).into(),
                            rusqlite::types::ValueRef::Blob(_) => "blob".into(),
                        })
                        .collect::<Vec<_>>()
                        .join(","),
                );
            }
            rs_ = cur;
        }
        let ts = t0.elapsed().as_secs_f64() / n as f64;
        let mut a = ro.clone();
        a.sort();
        let mut b = rs_.clone();
        b.sort();
        println!(
            "{name:<36} ours {:>9.1}us sqlite {:>9.1}us ratio {:.3}x rows {}/{} {}",
            to * 1e6,
            ts * 1e6,
            ts / to,
            c / n,
            c2 / n,
            if a == b { "MATCH" } else { "DIFF" }
        );
        if let Ok(rows) = o.query(&format!("EXPLAIN QUERY PLAN {q}"), []) {
            for r in rows {
                println!("     ours   plan: {}", r[3].as_text());
            }
        }
        let mut st = s.prepare(&format!("EXPLAIN QUERY PLAN {q}")).unwrap();
        let mut r = st.query([]).unwrap();
        while let Some(row) = r.next().unwrap() {
            println!("     sqlite plan: {}", row.get::<_, String>(3).unwrap());
        }
    }
}
