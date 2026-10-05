//! Focused VACUUM timing probe: build 2M rows (sequential-k), mass-delete
//! 40%, checkpoint, then time VACUUM with phase instrumentation.
use rustqlite::Database;
use std::time::Instant;

fn rss_mb() -> f64 {
    if let Ok(s) = std::fs::read_to_string("/proc/self/status") {
        for line in s.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                if let Some(kb) = rest.split_whitespace().next() {
                    return kb.parse::<u64>().unwrap_or(0) as f64 / 1024.0;
                }
            }
        }
    }
    0.0
}

#[test]
fn vacuum_probe() {
    let rows: u64 = std::env::var("MEGA_ROWS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_000_000);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vac.db");
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("PRAGMA journal_mode = WAL", []).unwrap();
        db.execute("PRAGMA synchronous = NORMAL", []).unwrap();
        db.execute(
            "CREATE TABLE events (id INTEGER PRIMARY KEY, k INTEGER, note TEXT)",
            [],
        )
        .unwrap();
        db.execute("CREATE INDEX ix_events_k ON events (k)", [])
            .unwrap();
        let t = Instant::now();
        let batch = 5000u64;
        let mut next = 0u64;
        while next < rows {
            let n = batch.min(rows - next);
            let mut sql = String::with_capacity(48 * n as usize + 48);
            sql.push_str("INSERT INTO events (id, k, note) VALUES ");
            for j in 0..n {
                let i = next + j + 1;
                if j > 0 {
                    sql.push(',');
                }
                sql.push_str(&format!(
                    "({}, {}, 'n{:08x}')",
                    i,
                    i % 1_000_000,
                    i & 0xffff_ffff
                ));
            }
            db.execute("BEGIN", []).unwrap();
            db.execute(&sql, []).unwrap();
            db.execute("COMMIT", []).unwrap();
            next += n;
        }
        println!(
            "[vp] build {rows} in {:.1}s rss={:.0}MB",
            t.elapsed().as_secs_f64(),
            rss_mb()
        );

        let t = Instant::now();
        db.execute(
            &format!("DELETE FROM events WHERE id <= {rows} AND id % 10 < 4"),
            [],
        )
        .unwrap();
        println!("[vp] mass-delete in {:.1}s", t.elapsed().as_secs_f64());
        db.execute("PRAGMA wal_checkpoint(TRUNCATE)", []).unwrap();
        let n: i64 = db.query("SELECT count(*) FROM events", []).unwrap()[0][0].as_integer();
        println!("[vp] survivors={n}");

        let t = Instant::now();
        let rss_before = rss_mb();
        db.execute("VACUUM", []).unwrap();
        println!(
            "[vp] VACUUM in {:.1}s rss {rss_before:.0} -> {:.0}MB",
            t.elapsed().as_secs_f64(),
            rss_mb()
        );
        let n2: i64 = db.query("SELECT count(*) FROM events", []).unwrap()[0][0].as_integer();
        let s: i64 = db.query("SELECT sum(k) FROM events", []).unwrap()[0][0].as_integer();
        println!("[vp] post-vacuum count={n2} (want {n}) sum={s}");
        assert_eq!(n2, n);
        drop(db);
    }
    // reopen + verify
    let db = Database::open(&path).unwrap();
    let n3: i64 = db.query("SELECT count(*) FROM events", []).unwrap()[0][0].as_integer();
    println!("[vp] reopen count={n3}");
    assert!(db.query("PRAGMA integrity_check", []).unwrap()[0][0].as_text() == "ok");
}
