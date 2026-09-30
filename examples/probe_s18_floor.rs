//! probe_s18_floor — phase-attributed RSS for the S18 differential shape
//! (in-memory db, 25k-row build txn, 60k random ops, ordered scan).
//! Usage: cargo run --release --example probe_s18_floor -- [rq|sq]
use rustqlite::{Database, Value};

#[cfg(target_os = "linux")]
fn rss_mb() -> f64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest
                .split_whitespace()
                .next()
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(0.0)
                / 1024.0;
        }
    }
    0.0
}
#[cfg(not(target_os = "linux"))]
fn rss_mb() -> f64 {
    0.0
}
fn mark(tag: &str) {
    println!("RSS  {tag:34} {:#7.2} MB", rss_mb());
}

fn lcg(seed: &mut u64) -> u64 {
    *seed = seed
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    *seed >> 33
}

fn main() {
    let which = std::env::args().nth(1).unwrap_or_else(|| "rq".to_string());
    let rows: u64 = 25_000;
    let ops: u64 = 15_000;
    let mut seed = 0xD1FFE5_u64;
    if which == "sq" {
        use rusqlite::params;
        mark("sq baseline");
        let mut inserted: std::collections::HashSet<i64> = std::collections::HashSet::new();
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE d (id INTEGER PRIMARY KEY, val INTEGER, score REAL)",
            [],
        )
        .unwrap();
        conn.execute("BEGIN", []).unwrap();
        let mut st = conn
            .prepare("INSERT INTO d (id, val, score) VALUES (?1, ?2, ?3)")
            .unwrap();
        for i in 1..=rows {
            st.execute(params![i as i64, (i % 977) as i64, i as f64 * 0.001])
                .unwrap();
        }
        drop(st);
        conn.execute("COMMIT", []).unwrap();
        mark("sq after build txn");
        for k in 0..ops {
            let op = lcg(&mut seed) % 10;
            let rid = (lcg(&mut seed) % rows) as i64 + 1;
            if op < 5 {
                conn.execute("UPDATE d SET val = val + 1 WHERE id = ?1", params![rid])
                    .unwrap();
            } else if op < 7 {
                conn.execute("DELETE FROM d WHERE id = ?1", params![rid])
                    .unwrap();
            } else {
                let nid = 1_000_000 + (lcg(&mut seed) % 500_000) as i64;
                if !inserted.insert(nid) {
                    continue;
                }
                conn.execute(
                    "INSERT INTO d (id, val, score) VALUES (?1, ?2, ?3)",
                    params![nid, (lcg(&mut seed) as i64) % 977, nid as f64 * 0.001],
                )
                .unwrap();
            }
            if (k + 1) % (ops / 3).max(1) == 0 {
                mark(&format!("sq ops {}%", 100 * (k + 1) / ops));
            }
        }
        mark("sq after ops");
        let mut h: u64 = 0x5A17;
        let mut stmt = conn.prepare("SELECT id, val FROM d ORDER BY id").unwrap();
        let mut it = stmt.query([]).unwrap();
        while let Some(r) = it.next().unwrap() {
            let id: i64 = r.get(0).unwrap();
            let val: i64 = r.get(1).unwrap();
            h = h.wrapping_mul(1_000_003).wrapping_add(id as u64);
            h = h.wrapping_mul(1_000_003).wrapping_add(val as u64);
        }
        println!("h={h}");
        mark("sq after scan");
        return;
    }
    mark("rq baseline");
    let mut inserted: std::collections::HashSet<i64> = std::collections::HashSet::new();
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE d (id INTEGER PRIMARY KEY, val INTEGER, score REAL)",
        [],
    )
    .unwrap();
    mark("rq after create");
    db.execute("BEGIN", []).unwrap();
    for i in 1..=rows {
        db.execute(
            "INSERT INTO d (id, val, score) VALUES (?, ?, ?)",
            [
                Value::Integer(i as i64),
                Value::Integer((i % 977) as i64),
                Value::Real(i as f64 * 0.001),
            ],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    mark("rq after build txn");
    for k in 0..ops {
        let op = lcg(&mut seed) % 10;
        let rid = (lcg(&mut seed) % rows) as i64 + 1;
        if op < 5 {
            db.execute(
                "UPDATE d SET val = val + 1 WHERE id = ?",
                [Value::Integer(rid)],
            )
            .unwrap();
        } else if op < 7 {
            db.execute("DELETE FROM d WHERE id = ?", [Value::Integer(rid)])
                .unwrap();
        } else {
            let nid = 1_000_000 + (lcg(&mut seed) % 500_000) as i64;
            if !inserted.insert(nid) {
                continue;
            }
            db.execute(
                "INSERT INTO d (id, val, score) VALUES (?, ?, ?)",
                [
                    Value::Integer(nid),
                    Value::Integer((lcg(&mut seed) as i64) % 977),
                    Value::Real(nid as f64 * 0.001),
                ],
            )
            .unwrap();
        }
        if (k + 1) % (ops / 3).max(1) == 0 {
            mark(&format!("rq ops {}%", 100 * (k + 1) / ops));
        }
    }
    mark("rq after ops");
    let mut h: u64 = 0x5A17;
    let mut stmt = db.prepare("SELECT id, val FROM d ORDER BY id").unwrap();
    while let Ok(rustqlite::StepResult::Row) = stmt.step() {
        if let Some(r) = stmt.row() {
            let id = r
                .first()
                .and_then(|v| {
                    if let Value::Integer(x) = v {
                        Some(*x as u64)
                    } else {
                        None
                    }
                })
                .unwrap_or(0);
            let val = r
                .get(1)
                .and_then(|v| {
                    if let Value::Integer(x) = v {
                        Some(*x as u64)
                    } else {
                        None
                    }
                })
                .unwrap_or(0);
            h = h.wrapping_mul(1_000_003).wrapping_add(id);
            h = h.wrapping_mul(1_000_003).wrapping_add(val);
        }
    }
    println!("h={h}");
    mark("rq after scan");
}
