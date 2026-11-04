//! Scoreboard probe: single-thread, FILE-backed (WAL + synchronous=NORMAL)
//! everyday workloads, ours vs SQLite, best of 3 each, same process.
//!
//! cargo run --release --example probe_score_speed [-- name-filter]
//! Schema:  t(id pk, k idx, name idx, val, score) 100k rows,
//!          u(id pk, t_id idx, amt) 300k rows.
use rustqlite::{Database, Value};
use std::time::Instant;

const NT: i64 = 100_000;
const NU: i64 = 300_000;

fn v(i: i64) -> Value {
    Value::Integer(i)
}
fn t(s: String) -> Value {
    Value::Text(s.into())
}

fn setup_ours(path: &str) -> Database {
    for s in ["", "-wal", "-shm", "-journal", "-rsqllock"] {
        let _ = std::fs::remove_file(format!("{path}{s}"));
    }
    let mut db = Database::open(path).unwrap();
    db.execute("PRAGMA journal_mode=WAL", []).unwrap();
    db.execute("PRAGMA synchronous=NORMAL", []).unwrap();
    for q in [
        "CREATE TABLE t (id INTEGER PRIMARY KEY, k INTEGER, name TEXT, val INTEGER, score REAL)",
        "CREATE INDEX t_k ON t(k)",
        "CREATE INDEX t_name ON t(name)",
        "CREATE TABLE u (id INTEGER PRIMARY KEY, t_id INTEGER, amt INTEGER)",
        "CREATE INDEX u_t ON u(t_id)",
        "CREATE TABLE lg (id INTEGER PRIMARY KEY, a INTEGER, b TEXT)",
        "BEGIN",
    ] {
        db.execute(q, []).unwrap();
    }
    for i in 1..=NT {
        db.execute(
            "INSERT INTO t VALUES (?,?,?,?,?)",
            [
                v(i),
                v((i * 7919) % 10007),
                t(format!("name{}", (i * 31) % NT)),
                v((i * 13) % 1000),
                Value::Real(i as f64 * 0.5),
            ],
        )
        .unwrap();
    }
    for i in 1..=NU {
        db.execute(
            "INSERT INTO u VALUES (?,?,?)",
            [v(i), v((i * 17) % NT + 1), v((i * 29) % 1000)],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    db
}

fn setup_sqlite(path: &str) -> rusqlite::Connection {
    for s in ["", "-wal", "-shm", "-journal"] {
        let _ = std::fs::remove_file(format!("{path}{s}"));
    }
    let c = rusqlite::Connection::open(path).unwrap();
    c.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
         CREATE TABLE t (id INTEGER PRIMARY KEY, k INTEGER, name TEXT, val INTEGER, score REAL);
         CREATE INDEX t_k ON t(k); CREATE INDEX t_name ON t(name);
         CREATE TABLE u (id INTEGER PRIMARY KEY, t_id INTEGER, amt INTEGER);
         CREATE INDEX u_t ON u(t_id);
         CREATE TABLE lg (id INTEGER PRIMARY KEY, a INTEGER, b TEXT);
         BEGIN;",
    )
    .unwrap();
    {
        let mut s = c.prepare("INSERT INTO t VALUES (?1,?2,?3,?4,?5)").unwrap();
        for i in 1..=NT {
            s.execute(rusqlite::params![
                i,
                (i * 7919) % 10007,
                format!("name{}", (i * 31) % NT),
                (i * 13) % 1000,
                i as f64 * 0.5
            ])
            .unwrap();
        }
        let mut s = c.prepare("INSERT INTO u VALUES (?1,?2,?3)").unwrap();
        for i in 1..=NU {
            s.execute(rusqlite::params![i, (i * 17) % NT + 1, (i * 29) % 1000])
                .unwrap();
        }
    }
    c.execute_batch("COMMIT").unwrap();
    c
}

struct Rng(u64);
impl Rng {
    fn next(&mut self, m: i64) -> i64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        (x.wrapping_mul(0x2545F4914F6CDD1D) % m as u64) as i64
    }
}

fn best<F: FnMut() -> i64>(mut f: F) -> (f64, i64) {
    let mut b = f64::MAX;
    let mut chk = 0;
    for _ in 0..3 {
        let t0 = Instant::now();
        chk = f();
        b = b.min(t0.elapsed().as_secs_f64());
    }
    (b, chk)
}

fn sum_val(v: &Value) -> i64 {
    match v {
        Value::Integer(i) => *i,
        Value::Real(r) => *r as i64,
        Value::Text(s) => s.as_str().len() as i64,
        _ => 0,
    }
}
fn sum_ref(v: rusqlite::types::ValueRef) -> i64 {
    use rusqlite::types::ValueRef::*;
    match v {
        Integer(i) => i,
        Real(r) => r as i64,
        Text(s) => s.len() as i64,
        _ => 0,
    }
}

/// ours read: run `sql` with params, return checksum of all cells.
fn oq(db: &Database, sql: &str, p: Vec<Value>) -> i64 {
    let rows = db.query(sql, p).unwrap();
    let mut c = 0;
    for r in &rows {
        for v in r.iter() {
            c += sum_val(v);
        }
    }
    c
}
fn sq(c: &rusqlite::Connection, sql: &str, p: &[&dyn rusqlite::ToSql]) -> i64 {
    let mut st = c.prepare_cached(sql).unwrap();
    let mut rows = st.query(p).unwrap();
    let mut s = 0;
    while let Some(r) = rows.next().unwrap() {
        let n = r.as_ref().column_count();
        for i in 0..n {
            s += sum_ref(r.get_ref(i).unwrap());
        }
    }
    s
}

fn main() {
    let filter = std::env::args().nth(1).unwrap_or_default();
    let dir = "/tmp/claude-501/score/speed";
    std::fs::create_dir_all(dir).unwrap();
    let po = format!("{dir}/ours.db");
    let ps = format!("{dir}/sqlite.db");
    let t0 = Instant::now();
    let mut o = setup_ours(&po);
    let ot = t0.elapsed().as_secs_f64();
    let t0 = Instant::now();
    let s = setup_sqlite(&ps);
    let st = t0.elapsed().as_secs_f64();
    println!("setup (100k t + 300k u rows, 3 idx, one txn): ours {ot:.3}s  sqlite {st:.3}s  ratio {:.2}x", st / ot);
    {
        let sz = |p: &str, s: &str| {
            std::fs::metadata(format!("{p}{s}"))
                .map(|m| m.len())
                .unwrap_or(0)
        };
        println!(
            "after setup: ours db={} wal={} | sqlite db={} wal={}",
            sz(&po, ""),
            sz(&po, "-wal"),
            sz(&ps, ""),
            sz(&ps, "-wal")
        );
    }
    println!(
        "{:<46} {:>9} {:>13} {:>13} {:>7}",
        "case", "ops", "ours ops/s", "sqlite ops/s", "ratio"
    );

    let report = |name: &str, n: i64, (ot, oc): (f64, i64), (st, sc): (f64, i64)| {
        let flag = if oc != sc { "  CHECKSUM-MISMATCH" } else { "" };
        println!(
            "{:<46} {:>9} {:>13.0} {:>13.0} {:>6.2}x{}",
            name,
            n,
            n as f64 / ot,
            n as f64 / st,
            st / ot,
            flag
        );
    };
    macro_rules! case {
        ($name:expr, $n:expr, $ours:expr, $sq:expr) => {
            if filter.is_empty() || $name.contains(filter.as_str()) {
                let r_o = best(|| $ours);
                let r_s = best(|| $sq);
                report($name, $n, r_o, r_s);
            }
        };
    }

    // ---- writes -----------------------------------------------------------
    {
        let mut ctr = 0i64;
        let mut ctr2 = 0i64;
        case!(
            "autocommit INSERT (1 row/txn)",
            5000,
            {
                for _ in 0..5000 {
                    ctr += 1;
                    o.execute(
                        "INSERT INTO lg VALUES (?,?,?)",
                        [v(ctr), v(ctr), t(format!("x{ctr}"))],
                    )
                    .unwrap();
                }
                0
            },
            {
                for _ in 0..5000 {
                    ctr2 += 1;
                    s.execute(
                        "INSERT INTO lg VALUES (?1,?2,?3)",
                        rusqlite::params![ctr2 + 10_000_000, ctr2, format!("x{ctr2}")],
                    )
                    .unwrap();
                }
                0
            }
        );
    }
    {
        let mut ctr = 100_000_000i64;
        let mut ctr2 = 100_000_000i64;
        case!(
            "10-row txn INSERT x500 (commit cost)",
            5000,
            {
                for _ in 0..500 {
                    o.execute("BEGIN", []).unwrap();
                    for _ in 0..10 {
                        ctr += 1;
                        o.execute(
                            "INSERT INTO lg VALUES (?,?,?)",
                            [v(ctr), v(ctr), t(format!("x{ctr}"))],
                        )
                        .unwrap();
                    }
                    o.execute("COMMIT", []).unwrap();
                }
                0
            },
            {
                for _ in 0..500 {
                    s.execute_batch("BEGIN").unwrap();
                    for _ in 0..10 {
                        ctr2 += 1;
                        s.execute(
                            "INSERT INTO lg VALUES (?1,?2,?3)",
                            rusqlite::params![ctr2, ctr2, format!("x{ctr2}")],
                        )
                        .unwrap();
                    }
                    s.execute_batch("COMMIT").unwrap();
                }
                0
            }
        );
    }
    {
        let mut base = 1_000_000_000i64;
        let mut base2 = 1_000_000_000i64;
        case!(
            "bulk INSERT 50k in 1 txn (t, 2 secondary idx)",
            50_000,
            {
                base += 100_000;
                o.execute("BEGIN", []).unwrap();
                for i in 0..50_000 {
                    let id = base + i;
                    o.execute(
                        "INSERT INTO t VALUES (?,?,?,?,?)",
                        [
                            v(id),
                            v(id % 10007),
                            t(format!("zz{}", id % 50_000)),
                            v(id % 1000),
                            Value::Real(0.5),
                        ],
                    )
                    .unwrap();
                }
                o.execute("COMMIT", []).unwrap();
                o.execute("DELETE FROM t WHERE id >= 1000000000", [])
                    .unwrap();
                0
            },
            {
                base2 += 100_000;
                s.execute_batch("BEGIN").unwrap();
                {
                    let mut st = s
                        .prepare_cached("INSERT INTO t VALUES (?1,?2,?3,?4,?5)")
                        .unwrap();
                    for i in 0..50_000 {
                        let id = base2 + i;
                        st.execute(rusqlite::params![
                            id,
                            id % 10007,
                            format!("zz{}", id % 50_000),
                            id % 1000,
                            0.5
                        ])
                        .unwrap();
                    }
                }
                s.execute_batch("COMMIT").unwrap();
                s.execute("DELETE FROM t WHERE id >= 1000000000", [])
                    .unwrap();
                0
            }
        );
    }
    case!(
        "UPDATE indexed col by pk, autocommit",
        5000,
        {
            let mut r = Rng(1);
            for _ in 0..5000 {
                o.execute("UPDATE t SET k = k + 1 WHERE id = ?", [v(r.next(NT) + 1)])
                    .unwrap();
            }
            0
        },
        {
            let mut r = Rng(1);
            for _ in 0..5000 {
                s.execute("UPDATE t SET k = k + 1 WHERE id = ?1", [r.next(NT) + 1])
                    .unwrap();
            }
            0
        }
    );
    case!(
        "UPDATE non-indexed col by pk, 50k in 1 txn",
        50_000,
        {
            let mut r = Rng(2);
            o.execute("BEGIN", []).unwrap();
            for _ in 0..50_000 {
                o.execute(
                    "UPDATE t SET val = val + 1 WHERE id = ?",
                    [v(r.next(NT) + 1)],
                )
                .unwrap();
            }
            o.execute("COMMIT", []).unwrap();
            0
        },
        {
            let mut r = Rng(2);
            s.execute_batch("BEGIN").unwrap();
            for _ in 0..50_000 {
                s.execute("UPDATE t SET val = val + 1 WHERE id = ?1", [r.next(NT) + 1])
                    .unwrap();
            }
            s.execute_batch("COMMIT").unwrap();
            0
        }
    );
    case!(
        "DELETE+re-INSERT by pk, 20k pairs in 1 txn",
        40_000,
        {
            let mut r = Rng(3);
            o.execute("BEGIN", []).unwrap();
            for _ in 0..20_000 {
                let id = r.next(NT) + 1;
                o.execute("DELETE FROM t WHERE id = ?", [v(id)]).unwrap();
                o.execute(
                    "INSERT INTO t VALUES (?,?,?,?,?)",
                    [
                        v(id),
                        v(id % 10007),
                        t(format!("name{}", id % NT)),
                        v(id % 1000),
                        Value::Real(0.5),
                    ],
                )
                .unwrap();
            }
            o.execute("COMMIT", []).unwrap();
            0
        },
        {
            let mut r = Rng(3);
            s.execute_batch("BEGIN").unwrap();
            for _ in 0..20_000 {
                let id = r.next(NT) + 1;
                s.execute("DELETE FROM t WHERE id = ?1", [id]).unwrap();
                s.execute(
                    "INSERT INTO t VALUES (?1,?2,?3,?4,?5)",
                    rusqlite::params![id, id % 10007, format!("name{}", id % NT), id % 1000, 0.5],
                )
                .unwrap();
            }
            s.execute_batch("COMMIT").unwrap();
            0
        }
    );
    case!(
        "INSERT OR REPLACE existing pk, 50k in 1 txn",
        50_000,
        {
            let mut r = Rng(4);
            o.execute("BEGIN", []).unwrap();
            for _ in 0..50_000 {
                let id = r.next(NT) + 1;
                o.execute(
                    "INSERT OR REPLACE INTO t VALUES (?,?,?,?,?)",
                    [
                        v(id),
                        v(id % 10007),
                        t(format!("name{}", id % NT)),
                        v(id % 1000),
                        Value::Real(0.5),
                    ],
                )
                .unwrap();
            }
            o.execute("COMMIT", []).unwrap();
            0
        },
        {
            let mut r = Rng(4);
            s.execute_batch("BEGIN").unwrap();
            for _ in 0..50_000 {
                let id = r.next(NT) + 1;
                s.execute(
                    "INSERT OR REPLACE INTO t VALUES (?1,?2,?3,?4,?5)",
                    rusqlite::params![id, id % 10007, format!("name{}", id % NT), id % 1000, 0.5],
                )
                .unwrap();
            }
            s.execute_batch("COMMIT").unwrap();
            0
        }
    );
    case!(
        "UPSERT ON CONFLICT DO UPDATE, 50k in 1 txn",
        50_000,
        {
            let mut r = Rng(5);
            o.execute("BEGIN", []).unwrap();
            for _ in 0..50_000 {
                let id = r.next(NT) + 1;
                o.execute(
                    "INSERT INTO t VALUES (?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET val = val + 1",
                    [v(id), v(1), t("n".into()), v(1), Value::Real(0.5)],
                )
                .unwrap();
            }
            o.execute("COMMIT", []).unwrap();
            0
        },
        {
            let mut r = Rng(5);
            s.execute_batch("BEGIN").unwrap();
            for _ in 0..50_000 {
                let id = r.next(NT) + 1;
                s.execute("INSERT INTO t VALUES (?1,?2,?3,?4,?5) ON CONFLICT(id) DO UPDATE SET val = val + 1", rusqlite::params![id, 1, "n", 1, 0.5]).unwrap();
            }
            s.execute_batch("COMMIT").unwrap();
            0
        }
    );
    case!(
        "DELETE range via index (k<20) + restore",
        1,
        {
            o.execute("BEGIN", []).unwrap();
            o.execute("CREATE TEMP TABLE sv AS SELECT * FROM t WHERE k < 200", [])
                .unwrap();
            o.execute("DELETE FROM t WHERE k < 200", []).unwrap();
            o.execute("INSERT INTO t SELECT * FROM sv", []).unwrap();
            o.execute("DROP TABLE sv", []).unwrap();
            o.execute("COMMIT", []).unwrap();
            0
        },
        {
            s.execute_batch("BEGIN; CREATE TEMP TABLE sv AS SELECT * FROM t WHERE k < 200; DELETE FROM t WHERE k < 200; INSERT INTO t SELECT * FROM sv; DROP TABLE sv; COMMIT;").unwrap();
            0
        }
    );

    // ---- reads ------------------------------------------------------------
    case!(
        "point read by pk (warm)",
        200_000,
        {
            let mut r = Rng(7);
            let mut c = 0;
            for _ in 0..200_000 {
                c += oq(
                    &o,
                    "SELECT name, val FROM t WHERE id = ?",
                    vec![v(r.next(NT) + 1)],
                );
            }
            c
        },
        {
            let mut r = Rng(7);
            let mut c = 0;
            for _ in 0..200_000 {
                c += sq(
                    &s,
                    "SELECT name, val FROM t WHERE id = ?1",
                    &[&(r.next(NT) + 1)],
                );
            }
            c
        }
    );
    case!(
        "point read by secondary idx (name)",
        100_000,
        {
            let mut r = Rng(8);
            let mut c = 0;
            for _ in 0..100_000 {
                c += oq(
                    &o,
                    "SELECT id, val FROM t WHERE name = ?",
                    vec![t(format!("name{}", r.next(NT)))],
                );
            }
            c
        },
        {
            let mut r = Rng(8);
            let mut c = 0;
            for _ in 0..100_000 {
                c += sq(
                    &s,
                    "SELECT id, val FROM t WHERE name = ?1",
                    &[&format!("name{}", r.next(NT))],
                );
            }
            c
        }
    );
    case!(
        "range scan 100 rows by idx (k BETWEEN)",
        5000,
        {
            let mut r = Rng(9);
            let mut c = 0;
            for _ in 0..5000 {
                let a = r.next(9000);
                c += oq(
                    &o,
                    "SELECT id, name FROM t WHERE k BETWEEN ? AND ?",
                    vec![v(a), v(a + 9)],
                );
            }
            c
        },
        {
            let mut r = Rng(9);
            let mut c = 0;
            for _ in 0..5000 {
                let a = r.next(9000);
                c += sq(
                    &s,
                    "SELECT id, name FROM t WHERE k BETWEEN ?1 AND ?2",
                    &[&a, &(a + 9)],
                );
            }
            c
        }
    );
    case!(
        "range scan 1000 rows by pk",
        1000,
        {
            let mut r = Rng(10);
            let mut c = 0;
            for _ in 0..1000 {
                let a = r.next(NT - 1000) + 1;
                c += oq(
                    &o,
                    "SELECT id, name, val FROM t WHERE id BETWEEN ? AND ?",
                    vec![v(a), v(a + 999)],
                );
            }
            c
        },
        {
            let mut r = Rng(10);
            let mut c = 0;
            for _ in 0..1000 {
                let a = r.next(NT - 1000) + 1;
                c += sq(
                    &s,
                    "SELECT id, name, val FROM t WHERE id BETWEEN ?1 AND ?2",
                    &[&a, &(a + 999)],
                );
            }
            c
        }
    );
    case!(
        "ORDER BY k LIMIT 10 (indexed)",
        5000,
        {
            let mut c = 0;
            for i in 0..5000 {
                c += oq(
                    &o,
                    "SELECT id, k FROM t WHERE k >= ? ORDER BY k LIMIT 10",
                    vec![v(i % 9000)],
                );
            }
            c
        },
        {
            let mut c = 0;
            for i in 0..5000i64 {
                c += sq(
                    &s,
                    "SELECT id, k FROM t WHERE k >= ?1 ORDER BY k LIMIT 10",
                    &[&(i % 9000)],
                );
            }
            c
        }
    );
    case!(
        "ORDER BY val LIMIT 10 (unindexed full scan)",
        50,
        {
            let mut c = 0;
            for _ in 0..50 {
                c += oq(
                    &o,
                    "SELECT id, val FROM t ORDER BY val DESC, id LIMIT 10",
                    vec![],
                );
            }
            c
        },
        {
            let mut c = 0;
            for _ in 0..50 {
                c += sq(
                    &s,
                    "SELECT id, val FROM t ORDER BY val DESC, id LIMIT 10",
                    &[],
                );
            }
            c
        }
    );
    case!(
        "aggregate sum/avg/count full scan",
        50,
        {
            let mut c = 0;
            for _ in 0..50 {
                c += oq(
                    &o,
                    "SELECT count(*), sum(val), avg(score) FROM t WHERE val > 100",
                    vec![],
                );
            }
            c
        },
        {
            let mut c = 0;
            for _ in 0..50 {
                c += sq(
                    &s,
                    "SELECT count(*), sum(val), avg(score) FROM t WHERE val > 100",
                    &[],
                );
            }
            c
        }
    );
    case!(
        "GROUP BY val (1000 groups) full scan",
        20,
        {
            let mut c = 0;
            for _ in 0..20 {
                c += oq(
                    &o,
                    "SELECT val, count(*), sum(score) FROM t GROUP BY val",
                    vec![],
                );
            }
            c
        },
        {
            let mut c = 0;
            for _ in 0..20 {
                c += sq(
                    &s,
                    "SELECT val, count(*), sum(score) FROM t GROUP BY val",
                    &[],
                );
            }
            c
        }
    );
    case!(
        "JOIN u->t by pk, 50 rows/query",
        1000,
        {
            let mut r = Rng(11);
            let mut c = 0;
            for _ in 0..1000 {
                let a = r.next(NU - 100) + 1;
                c += oq(&o, "SELECT u.id, t.name, u.amt FROM u JOIN t ON t.id = u.t_id WHERE u.id BETWEEN ? AND ?", vec![v(a), v(a + 49)]);
            }
            c
        },
        {
            let mut r = Rng(11);
            let mut c = 0;
            for _ in 0..1000 {
                let a = r.next(NU - 100) + 1;
                c += sq(&s, "SELECT u.id, t.name, u.amt FROM u JOIN t ON t.id = u.t_id WHERE u.id BETWEEN ?1 AND ?2", &[&a, &(a + 49)]);
            }
            c
        }
    );
    case!(
        "JOIN t->u by idx (t_id), 3 rows/pt",
        5000,
        {
            let mut r = Rng(12);
            let mut c = 0;
            for _ in 0..5000 {
                c += oq(
                    &o,
                    "SELECT t.name, u.amt FROM t JOIN u ON u.t_id = t.id WHERE t.id = ?",
                    vec![v(r.next(NT) + 1)],
                );
            }
            c
        },
        {
            let mut r = Rng(12);
            let mut c = 0;
            for _ in 0..5000 {
                c += sq(
                    &s,
                    "SELECT t.name, u.amt FROM t JOIN u ON u.t_id = t.id WHERE t.id = ?1",
                    &[&(r.next(NT) + 1)],
                );
            }
            c
        }
    );
    case!(
        "JOIN+GROUP BY full (u x t, 300k rows)",
        5,
        {
            let mut c = 0;
            for _ in 0..5 {
                c += oq(&o, "SELECT t.k % 50, count(*), sum(u.amt) FROM u JOIN t ON t.id = u.t_id GROUP BY t.k % 50", vec![]);
            }
            c
        },
        {
            let mut c = 0;
            for _ in 0..5 {
                c += sq(&s, "SELECT t.k % 50, count(*), sum(u.amt) FROM u JOIN t ON t.id = u.t_id GROUP BY t.k % 50", &[]);
            }
            c
        }
    );
    case!(
        "IN (subquery) on idx",
        20,
        {
            let mut c = 0;
            for _ in 0..20 {
                c += oq(&o, "SELECT count(*), sum(val) FROM t WHERE id IN (SELECT t_id FROM u WHERE amt > 990)", vec![]);
            }
            c
        },
        {
            let mut c = 0;
            for _ in 0..20 {
                c += sq(&s, "SELECT count(*), sum(val) FROM t WHERE id IN (SELECT t_id FROM u WHERE amt > 990)", &[]);
            }
            c
        }
    );
    case!(
        "correlated scalar subquery (1000 outer rows)",
        20,
        {
            let mut c = 0;
            for _ in 0..20 {
                c += oq(&o, "SELECT sum((SELECT count(*) FROM u WHERE u.t_id = t.id)) FROM t WHERE id <= 1000", vec![]);
            }
            c
        },
        {
            let mut c = 0;
            for _ in 0..20 {
                c += sq(&s, "SELECT sum((SELECT count(*) FROM u WHERE u.t_id = t.id)) FROM t WHERE id <= 1000", &[]);
            }
            c
        }
    );
    case!(
        "DISTINCT k over t (10007 vals)",
        20,
        {
            let mut c = 0;
            for _ in 0..20 {
                c += oq(
                    &o,
                    "SELECT count(*) FROM (SELECT DISTINCT name FROM t)",
                    vec![],
                );
            }
            c
        },
        {
            let mut c = 0;
            for _ in 0..20 {
                c += sq(
                    &s,
                    "SELECT count(*) FROM (SELECT DISTINCT name FROM t)",
                    &[],
                );
            }
            c
        }
    );

    // ---- open / cold ------------------------------------------------------
    drop(o);
    drop(s);
    case!(
        "open + 1 point read + close (file, WAL)",
        300,
        {
            let mut c = 0;
            for i in 0..300i64 {
                let d = Database::open(&po).unwrap();
                c += oq(&d, "SELECT name FROM t WHERE id = ?", vec![v(i + 1)]);
            }
            c
        },
        {
            let mut c = 0;
            for i in 0..300i64 {
                let d = rusqlite::Connection::open(&ps).unwrap();
                c += sq(&d, "SELECT name FROM t WHERE id = ?1", &[&(i + 1)]);
            }
            c
        }
    );
    case!(
        "FRESH conn point reads, no writes (200k)",
        200_000,
        {
            let d = Database::open(&po).unwrap();
            let mut r = Rng(13);
            let mut c = 0;
            for _ in 0..200_000 {
                c += oq(
                    &d,
                    "SELECT name, val FROM t WHERE id = ?",
                    vec![v(r.next(NT) + 1)],
                );
            }
            c
        },
        {
            let d = rusqlite::Connection::open(&ps).unwrap();
            let mut r = Rng(13);
            let mut c = 0;
            for _ in 0..200_000 {
                c += sq(
                    &d,
                    "SELECT name, val FROM t WHERE id = ?1",
                    &[&(r.next(NT) + 1)],
                );
            }
            c
        }
    );
    case!(
        "FRESH conn full scan count(*)+sum (cold open)",
        10,
        {
            let mut c = 0;
            for _ in 0..10 {
                let d = Database::open(&po).unwrap();
                c += oq(&d, "SELECT count(*), sum(val) FROM t", vec![]);
            }
            c
        },
        {
            let mut c = 0;
            for _ in 0..10 {
                let d = rusqlite::Connection::open(&ps).unwrap();
                c += sq(&d, "SELECT count(*), sum(val) FROM t", &[]);
            }
            c
        }
    );
    let sz = |p: &str, s: &str| {
        std::fs::metadata(format!("{p}{s}"))
            .map(|m| m.len())
            .unwrap_or(0)
    };
    println!(
        "final files: ours db={} wal={} | sqlite db={} wal={}",
        sz(&po, ""),
        sz(&po, "-wal"),
        sz(&ps, ""),
        sz(&ps, "-wal")
    );
}
