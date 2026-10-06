//! Focused reproducer for the 100M M6 post-vacuum integrity failure
//! (CI run 37427014651, all three OS): the marathon's M6 — capped
//! mass-DELETE + VACUUM — leaves exact answers but a failing
//! PRAGMA integrity_check. This probe isolates M1(build)+M4(churn)+
//! M6(delete+vacuum) so iterations run against a snapshot instead of
//! rebuilding 100M rows every time.
//!
//!   cargo run --release --example m6_probe -- build <dir> <rows> [band_cap]
//!   cargo run --release --example m6_probe -- churn <dir> <rows> [band_cap]
//!   cargo run --release --example m6_probe -- m6 <dir> <rows> <m6_cap> [--no-vac]
//!   cargo run --release --example m6_probe -- state <dir>
//!
//! build:  events(id INTEGER PRIMARY KEY, k INTEGER), k = id % 1M,
//!         CREATE INDEX before the load, batched 20000-row INSERTs in
//!         BEGIN/COMMIT — the marathon's M1 shape verbatim.
//! churn:  M4.1 UPDATE k=k+1 WHERE id%20=7 AND id<=band_cap,
//!         M4.2 DELETE [rows/2, rows/2+rows/50) + batched reinsert.
//! m6:     works on a COPY of dir/base.db -> dir/work.db: the M6
//!         DELETE (id <= cap AND id%10<4), checkpoint, VACUUM,
//!         checkpoint, exact answers + FULL integrity report.

use rustqlite::Database;
use std::path::{Path, PathBuf};
use std::time::Instant;

fn one_i64(db: &Database, sql: &str) -> i64 {
    let rows = db.query(sql, []).expect(sql);
    rows[0][0].as_integer()
}

fn one_text(db: &Database, sql: &str) -> String {
    let rows = db.query(sql, []).expect(sql);
    rows[0][0].as_text()
}

fn gen_k(i: u64, band_adjust: bool, band_cap: u64) -> i64 {
    let k = (i % 1_000_000) as i64;
    if band_adjust && i % 20 == 7 && i <= band_cap {
        k + 1
    } else {
        k
    }
}

fn build_batch_sql(start: u64, n: u64, band_adjust: bool, band_cap: u64) -> String {
    let mut sql = String::with_capacity(64 * n as usize + 64);
    sql.push_str("INSERT INTO events (id, k) VALUES ");
    for j in 0..n {
        let i = start + j + 1;
        if j > 0 {
            sql.push(',');
        }
        sql.push_str(&format!("({}, {})", i, gen_k(i, band_adjust, band_cap)));
    }
    sql
}

fn open_db(path: &Path) -> Database {
    let mut db = Database::open(path).unwrap();
    db.execute("PRAGMA journal_mode = WAL", []).unwrap();
    db.execute("PRAGMA synchronous = NORMAL", []).unwrap();
    db
}

fn file_bytes(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

fn dir_bytes(dir: &Path) -> u64 {
    let mut t = 0;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            t += e.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
    t
}

fn copy_base_to_work(dir: &Path) -> PathBuf {
    let base = dir.join("base.db");
    let work = dir.join("work.db");
    let _ = std::fs::remove_file(&work);
    let _ = std::fs::remove_file(dir.join("work.db-wal"));
    let _ = std::fs::remove_file(dir.join("work.db-shm"));
    std::fs::copy(&base, &work).unwrap();
    work
}

fn print_state(db: &Database, tag: &str) {
    let count = one_i64(db, "SELECT count(*) FROM events");
    let sum = one_i64(db, "SELECT sum(k) FROM events");
    let free = one_i64(db, "PRAGMA freelist_count");
    let pages = one_i64(db, "PRAGMA page_count");
    let integ = one_text(db, "PRAGMA integrity_check");
    println!("[{tag}] count={count} sum={sum} freelist={free} pages={pages} integrity={integ:?}");
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().map(String::as_str).unwrap_or("");
    match mode {
        "build" => {
            let dir = PathBuf::from(&args[1]);
            let rows: u64 = args[2].parse().unwrap();
            let band_cap: u64 = args.get(3).and_then(|v| v.parse().ok()).unwrap_or(rows);
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("base.db");
            let _ = std::fs::remove_file(&path);
            let t0 = Instant::now();
            let mut db = open_db(&path);
            db.execute(
                "CREATE TABLE events (id INTEGER PRIMARY KEY, k INTEGER)",
                [],
            )
            .unwrap();
            db.execute("CREATE INDEX ix_events_k ON events (k)", [])
                .unwrap();
            let batch = 20000u64.min(rows);
            let mut next_id = 0u64;
            while next_id < rows {
                let n = batch.min(rows - next_id);
                let sql = build_batch_sql(next_id, n, false, band_cap);
                db.execute("BEGIN", []).unwrap();
                db.execute(&sql, []).unwrap();
                db.execute("COMMIT", []).unwrap();
                next_id += n;
            }
            db.execute("PRAGMA wal_checkpoint(TRUNCATE)", []).unwrap();
            println!(
                "[build] {rows} rows in {:.0}s | file={:.1}MB",
                t0.elapsed().as_secs_f64(),
                file_bytes(&path) as f64 / 1e6
            );
            print_state(&db, "build");
        }
        "churn" => {
            let dir = PathBuf::from(&args[1]);
            let rows: u64 = args[2].parse().unwrap();
            let band_cap: u64 = args.get(3).and_then(|v| v.parse().ok()).unwrap_or(rows);
            let path = dir.join("base.db");
            let mut db = open_db(&path);
            // M4.1 band update
            let t0 = Instant::now();
            db.execute(
                &format!("UPDATE events SET k = k + 1 WHERE id % 20 = 7 AND id <= {band_cap}"),
                [],
            )
            .unwrap();
            println!(
                "[churn] band-update (id%20=7, id<={band_cap}): {:.0}ms",
                t0.elapsed().as_secs_f64() * 1000.0
            );
            // M4.2 band delete + reinsert (rowid range, exact round trip)
            let band_a = rows / 2;
            let band_b = band_a + rows / 50 - 1;
            let band_n = band_b - band_a + 1;
            let t1 = Instant::now();
            db.execute("BEGIN", []).unwrap();
            db.execute(
                &format!("DELETE FROM events WHERE id >= {band_a} AND id <= {band_b}"),
                [],
            )
            .unwrap();
            db.execute("COMMIT", []).unwrap();
            println!(
                "[churn] band-delete [{band_a},{band_b}]: {:.0}ms",
                t1.elapsed().as_secs_f64() * 1000.0
            );
            let t2 = Instant::now();
            db.execute("BEGIN", []).unwrap();
            let mut reins_left = band_n;
            let mut reins_next = band_a - 1;
            let batch = 20000u64.min(band_n.max(1));
            while reins_left > 0 {
                let n = batch.min(reins_left);
                let sql = build_batch_sql(reins_next, n, true, band_cap);
                db.execute(&sql, []).unwrap();
                reins_next += n;
                reins_left -= n;
            }
            db.execute("COMMIT", []).unwrap();
            println!(
                "[churn] band-reinsert {band_n} rows: {:.0}ms",
                t2.elapsed().as_secs_f64() * 1000.0
            );
            db.execute("PRAGMA wal_checkpoint(TRUNCATE)", []).unwrap();
            print_state(&db, "churn");
        }
        "m6" => {
            let dir = PathBuf::from(&args[1]);
            let rows: u64 = args[2].parse().unwrap();
            let m6_cap: u64 = args[3].parse().unwrap();
            let no_vac = args.iter().any(|a| a == "--no-vac");
            let work = copy_base_to_work(&dir);
            let mut db = open_db(&work);
            let pre = file_bytes(&work);
            let t0 = Instant::now();
            db.execute(
                &format!("DELETE FROM events WHERE id <= {m6_cap} AND id % 10 < 4"),
                [],
            )
            .unwrap();
            println!(
                "[m6] delete (id<={m6_cap}, id%10<4): {:.0}ms",
                t0.elapsed().as_secs_f64() * 1000.0
            );
            db.execute("PRAGMA wal_checkpoint(TRUNCATE)", []).unwrap();
            let post_del = file_bytes(&work);
            let free = one_i64(&db, "PRAGMA freelist_count");
            let pages = one_i64(&db, "PRAGMA page_count");
            println!("[m6] file {pre} -> {post_del} | freelist={free} pages={pages}");
            // Expected survivors: (rows - deleted) with deleted = |id<=cap, id%10<4|
            let deleted: u64 = (1..=m6_cap.min(rows)).filter(|i| i % 10 < 4).count() as u64;
            let want_count = (rows - deleted) as i64;
            let got = one_i64(&db, "SELECT count(*) FROM events");
            println!(
                "[m6] count={got} want={want_count} match={}",
                got == want_count
            );
            if !no_vac {
                let t1 = Instant::now();
                db.execute("VACUUM", []).unwrap();
                println!("[m6] VACUUM: {:.0}ms", t1.elapsed().as_secs_f64() * 1000.0);
                db.execute("PRAGMA wal_checkpoint(TRUNCATE)", []).unwrap();
                let post_vac = file_bytes(&work);
                println!(
                    "[m6] file after vac {post_vac} (reclaimed {:.1}%)",
                    100.0 * (1.0 - post_vac as f64 / pre as f64)
                );
                let got2 = one_i64(&db, "SELECT count(*) FROM events");
                println!("[m6] post-vac count={got2} match={}", got2 == want_count);
            }
            // The full integrity report — the whole point.
            let report = one_text(&db, "PRAGMA integrity_check");
            if report == "ok" {
                println!("[m6] integrity_check: ok");
            } else {
                println!("[m6] integrity_check REPORT (first 60 lines):");
                for (i, line) in report.lines().take(60).enumerate() {
                    println!("  {i:>3}: {line}");
                }
            }
            println!("[m6] dir now: {:.1}MB", dir_bytes(&dir) as f64 / 1e6);
        }
        "state" => {
            let dir = PathBuf::from(&args[1]);
            let work = copy_base_to_work(&dir);
            let db = open_db(&work);
            print_state(&db, "state");
        }
        _ => {
            eprintln!("usage: m6_probe build|churn|m6|state <dir> <rows> [band_cap|m6_cap]");
            std::process::exit(2);
        }
    }
}
