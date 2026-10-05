//! VACUUM RSS attribution probe: mimalloc's process info + forced
//! `mi_collect` discriminate LIVE bytes from allocator retention at
//! each VACUUM stage. Run:
//!   cargo run --release --features mimalloc --example vac_rss_probe -- [rows]
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
fn peak_mb() -> f64 {
    if let Ok(s) = std::fs::read_to_string("/proc/self/status") {
        for line in s.lines() {
            if let Some(rest) = line.strip_prefix("VmHWM:") {
                if let Some(kb) = rest.split_whitespace().next() {
                    return kb.parse::<u64>().unwrap_or(0) as f64 / 1024.0;
                }
            }
        }
    }
    0.0
}

#[cfg(feature = "mimalloc")]
fn mi_info(tag: &str) {
    unsafe {
        let mut el = 0usize;
        let mut u = 0usize;
        let mut s = 0usize;
        let mut rss = 0usize;
        let mut prss = 0usize;
        let mut commit = 0usize;
        let mut pcommit = 0usize;
        let mut pf = 0usize;
        libmimalloc_sys::mi_process_info(
            &mut el,
            &mut u,
            &mut s,
            &mut rss,
            &mut prss,
            &mut commit,
            &mut pcommit,
            &mut pf,
        );
        eprintln!(
            "[mi:{tag}] mi_rss={:.1}MB mi_commit={:.1}MB mi_peak_commit={:.1}MB faults={}",
            rss as f64 / 1048576.0,
            commit as f64 / 1048576.0,
            pcommit as f64 / 1048576.0,
            pf
        );
    }
}
#[cfg(not(feature = "mimalloc"))]
fn mi_info(_tag: &str) {}

#[cfg(feature = "mimalloc")]
fn mi_collect_force(tag: &str) {
    unsafe {
        libmimalloc_sys::mi_collect(true);
    }
    eprintln!("[mi:{tag}] after mi_collect(true): rss={:.1}MB", rss_mb());
}
#[cfg(not(feature = "mimalloc"))]
fn mi_collect_force(_tag: &str) {}

fn main() {
    let rows: u64 = std::env::args()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_000_000);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vac.db");
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
        "[vp] build {rows} in {:.1}s rss={:.0}MB peak={:.0}MB",
        t.elapsed().as_secs_f64(),
        rss_mb(),
        peak_mb()
    );
    mi_info("post-build");

    let t = Instant::now();
    db.execute(
        &format!("DELETE FROM events WHERE id <= {rows} AND id % 10 < 4"),
        [],
    )
    .unwrap();
    println!(
        "[vp] mass-delete in {:.1}s rss={:.0}MB peak={:.0}MB",
        t.elapsed().as_secs_f64(),
        rss_mb(),
        peak_mb()
    );
    mi_info("post-delete");
    db.execute("PRAGMA wal_checkpoint(TRUNCATE)", []).unwrap();
    println!(
        "[vp] post-checkpoint rss={:.0}MB peak={:.0}MB",
        rss_mb(),
        peak_mb()
    );
    mi_info("post-checkpoint");

    // VACUUM with a 50ms timeline sampler.
    let t1 = Instant::now();
    let samples: std::sync::Arc<std::sync::Mutex<Vec<(f64, f64, f64)>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sx = samples.clone();
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let dx = done.clone();
    std::thread::spawn(move || {
        while !dx.load(std::sync::atomic::Ordering::Relaxed) {
            let t = t1.elapsed().as_secs_f64();
            let r = rss_mb();
            let pk = peak_mb();
            if let Ok(mut v) = sx.lock() {
                v.push((t, r, pk));
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    });
    let t = Instant::now();
    db.execute("VACUUM", []).unwrap();
    done.store(true, std::sync::atomic::Ordering::Relaxed);
    println!(
        "[vp] VACUUM in {:.1}s rss={:.0}MB peak={:.0}MB",
        t.elapsed().as_secs_f64(),
        rss_mb(),
        peak_mb()
    );
    mi_info("post-vacuum");
    mi_collect_force("post-vacuum");
    if let Ok(v) = samples.lock() {
        let mut last_r = -1.0;
        for (ts, r, pk) in v.iter() {
            if (r - last_r).abs() >= 1.0 {
                println!(
                    "[vp] vac-timeline t={:5.2}s rss={:6.1}MB peak={:6.1}MB",
                    ts, r, pk
                );
                last_r = *r;
            }
        }
    }

    let t = Instant::now();
    let n: i64 = db.query("SELECT count(*) FROM events", []).unwrap()[0][0].as_integer();
    println!(
        "[vp] count={n} in {:.2}s rss={:.0}MB peak={:.0}MB",
        t.elapsed().as_secs_f64(),
        rss_mb(),
        peak_mb()
    );
    mi_info("post-count");
    mi_collect_force("post-count");

    let ok = db.query("PRAGMA integrity_check", []).unwrap()[0][0].as_text() == "ok";
    println!("[vp] integrity ok={ok}");
    drop(db);
    println!(
        "[vp] after drop(db) rss={:.0}MB peak={:.0}MB",
        rss_mb(),
        peak_mb()
    );
    mi_info("post-drop");
    mi_collect_force("post-drop");
}
