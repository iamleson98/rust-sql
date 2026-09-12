//! Insert-phase memory attribution: RSS checkpoints during the 1M-row txn,
//! plus WAL/pager structural counters at the end.

use rustqlite::{Database, Value};

fn rss() -> f64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest
                .trim()
                .trim_end_matches("kB")
                .trim()
                .parse::<f64>()
                .unwrap()
                / 1024.0;
        }
    }
    0.0
}

fn main() {
    let chunk_txn: bool = std::env::args().nth(1) == Some("chunked".into());
    let path = "/tmp/s17i.rq.db";
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{path}-wal"));
    let mut db = Database::open(path).unwrap();
    db.execute("PRAGMA journal_mode=WAL", []).unwrap();
    db.execute("PRAGMA synchronous=OFF", []).unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
        [],
    )
    .unwrap();
    let base = rss();
    println!("base rss={base:.1}MB chunked={chunk_txn}");
    if !chunk_txn {
        db.execute("BEGIN", []).unwrap();
    }
    for i in 1..=1_000_000i64 {
        if chunk_txn && (i as u64 - 1) % 200_000 == 0 {
            if i > 1 {
                db.execute("COMMIT", []).unwrap();
            }
            db.execute("BEGIN", []).unwrap();
        }
        db.execute(
            "INSERT INTO t (name, val, score) VALUES (?, ?, ?)",
            [
                Value::Text(format!("name{i}").into()),
                Value::Integer(i),
                Value::Real(i as f64 * 1.5),
            ],
        )
        .unwrap();
        if i % 200_000 == 0 {
            println!(
                "  {i:8} rss=+{:.1}MB cache_pages={} wal_frames={} miss={} hit={}",
                rss() - base,
                db.pager().cache_size(),
                db.pager().wal_frames(),
                db.pager().cache_misses(),
                db.pager().cache_hits()
            );
        }
    }
    db.execute("COMMIT", []).unwrap();
    println!(
        "after COMMIT rss=+{:.1}MB cache_pages={} wal_frames={}",
        rss() - base,
        db.pager().cache_size(),
        db.pager().wal_frames()
    );
    drop(db);
    println!("after drop rss=+{:.1}MB", rss() - base);
}
