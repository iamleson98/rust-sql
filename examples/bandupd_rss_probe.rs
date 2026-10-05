//! Band-update RSS attribution probe: the mega marathon's M4 measured a
//! post-spill peak of 818 MB at 100M rows (5M updated, ~164 B/updated-row
//! of residual). This probe samples the RSS timeline around the single
//! band-update statement and discriminates mimalloc retention from live
//! bytes (forced collect) at its end.
//!
//!   cargo run --release --features mimalloc --example bandupd_rss_probe -- [rows]

use rustqlite::Database;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

fn rss_mb() -> f64 {
    if let Ok(s) = std::fs::read_to_string("/proc/self/status") {
        for line in s.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                if let Some(kb) = rest.split_whitespace().next() {
                    return kb.parse::<f64>().unwrap_or(0.0) / 1024.0;
                }
            }
        }
    }
    0.0
}

#[cfg(feature = "mimalloc")]
fn mi_collect_force() {
    unsafe {
        libmimalloc_sys::mi_collect(true);
    }
}

#[cfg(not(feature = "mimalloc"))]
fn mi_collect_force() {}

fn main() {
    let rows: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(2_000_000);
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::open(dir.path().join("b.db")).unwrap();
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
    let mut next = 0u64;
    while next < rows {
        let n = 5000u64.min(rows - next);
        let mut sql = String::with_capacity(64 * n as usize + 128);
        sql.push_str("INSERT INTO events (id, k, note) VALUES ");
        for i in 0..n {
            if i > 0 {
                sql.push(',');
            }
            let id = next + i + 1;
            sql.push_str(&format!("({}, {}, 'note{:06}')", id, id % 1_000_000, id));
        }
        db.execute(&sql, []).unwrap();
        next += n;
    }
    println!(
        "[rss-probe] built {rows} rows in {:.1}s rss={:.1}MB",
        t.elapsed().as_secs_f64(),
        rss_mb()
    );

    // Timeline sampler around the band-update.
    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let stop_sampler = std::sync::Arc::clone(&stop);
    let sampler = std::thread::spawn(move || {
        let mut last = 0.0f64;
        let mut mark = std::time::Instant::now();
        while !stop_sampler.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(10));
            let r = rss_mb();
            if r - last > 8.0 || mark.elapsed().as_secs_f64() > 1.0 {
                println!("  [rss] {r:.1}MB");
                last = r;
                mark = std::time::Instant::now();
            }
        }
    });

    let t = Instant::now();
    db.execute(
        &format!("UPDATE events SET k = k + 1 WHERE id % 20 = 7 AND id <= {rows}"),
        [],
    )
    .unwrap();
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    stop.store(true, Ordering::Relaxed);
    let _ = sampler.join();
    println!("[rss-probe] band-update {ms:.0}ms rss={:.1}MB", rss_mb());
    mi_collect_force();
    println!("[rss-probe] after mi_collect(true): rss={:.1}MB", rss_mb());
    let n: i64 = db
        .query("SELECT count(*) FROM events WHERE k > 999999", [])
        .unwrap()[0][0]
        .as_integer();
    println!("[rss-probe] sanity (wrapped k rows): {n}");
}
