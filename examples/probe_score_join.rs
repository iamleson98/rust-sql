//! Scoreboard probe: why is `u JOIN t ON t.id=u.t_id WHERE u.id BETWEEN a AND b` slow?
//! Times query variants + prints EXPLAIN QUERY PLAN for ours and SQLite.
use rustqlite::{Database, Value};
use std::time::Instant;

fn main() {
    let mut o = Database::open_in_memory().unwrap();
    let s = rusqlite::Connection::open_in_memory().unwrap();
    let ddl =
        "CREATE TABLE t (id INTEGER PRIMARY KEY, k INTEGER, name TEXT, val INTEGER, score REAL);
         CREATE TABLE u (id INTEGER PRIMARY KEY, t_id INTEGER, amt INTEGER);
         CREATE INDEX u_t ON u(t_id);";
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
    }
    for i in 1..=300_000i64 {
        let tid = (i * 17) % 100_000 + 1;
        o.execute(
            "INSERT INTO u VALUES (?,?,?)",
            [
                Value::Integer(i),
                Value::Integer(tid),
                Value::Integer(i % 1000),
            ],
        )
        .unwrap();
        s.execute(
            "INSERT INTO u VALUES (?1,?2,?3)",
            rusqlite::params![i, tid, i % 1000],
        )
        .unwrap();
    }
    o.execute("COMMIT", []).unwrap();
    s.execute_batch("COMMIT").unwrap();
    let qs = [
        ("join, u.id BETWEEN (50 rows)", "SELECT u.id, t.name, u.amt FROM u JOIN t ON t.id = u.t_id WHERE u.id BETWEEN 1000 AND 1049"),
        ("join, u.id = 1000 (1 row)", "SELECT u.id, t.name, u.amt FROM u JOIN t ON t.id = u.t_id WHERE u.id = 1000"),
        ("join, u.id < 50 LIMIT 50", "SELECT u.id, t.name, u.amt FROM u JOIN t ON t.id = u.t_id WHERE u.id < 100 LIMIT 50"),
        ("self-join t a JOIN t b ON a.id=b.id WHERE a.id<50", "SELECT a.id, b.name FROM t a JOIN t b ON a.id = b.id WHERE a.id < 50"),
        ("ORDER BY k LIMIT 10 WHERE k>=5 (idx on k absent here)", "SELECT id, k FROM t WHERE k >= 5 ORDER BY k LIMIT 10"),
    ];
    for (name, q) in qs {
        let n = 20;
        let t0 = Instant::now();
        let mut c = 0;
        for _ in 0..n {
            c += o.query(q, []).unwrap().len();
        }
        let to = t0.elapsed().as_secs_f64() / n as f64;
        let t0 = Instant::now();
        let mut c2 = 0;
        for _ in 0..n {
            let mut st = s.prepare_cached(q).unwrap();
            let mut r = st.query([]).unwrap();
            while r.next().unwrap().is_some() {
                c2 += 1;
            }
        }
        let ts = t0.elapsed().as_secs_f64() / n as f64;
        println!(
            "{name:<56} ours {:>9.1}us sqlite {:>9.1}us ratio {:.3}x rows {}/{}",
            to * 1e6,
            ts * 1e6,
            ts / to,
            c / n,
            c2 / n
        );
        let eo = o.query(&format!("EXPLAIN QUERY PLAN {q}"), []);
        match eo {
            Ok(rows) => {
                for r in rows {
                    let line: Vec<String> = (0..r.len()).map(|i| r[i].as_text()).collect();
                    println!("     ours   plan: {}", line.join(" | "));
                }
            }
            Err(e) => println!("     ours plan err {e:?}"),
        }
        let mut st = s.prepare(&format!("EXPLAIN QUERY PLAN {q}")).unwrap();
        let mut r = st.query([]).unwrap();
        while let Some(row) = r.next().unwrap() {
            println!("     sqlite plan: {}", row.get::<_, String>(3).unwrap());
        }
    }
}
