//! Phase-level profile of the multi-VALUES INSERT path (bench_full_vs_sqlite's
//! losing row): parse / plan / cache / exec attribution per statement.
//!
//! Two phases use SEPARATE tables in identical fresh states so the B-tree
//! shape is never a confound.

use rustqlite::Database;

fn run_phase(db: &mut Database, table: &str, tag: &str, n: usize, chunk: usize) {
    let t0 = std::time::Instant::now();
    let mut i = 1;
    let mut stmts = 0;
    while i <= n {
        let end = (i + chunk - 1).min(n);
        let values: String = (i..=end)
            .map(|j| format!("('name{}', {})", j, j))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!("INSERT INTO {} (name, val) VALUES {}", table, values);
        db.execute(&sql, []).unwrap();
        i = end + 1;
        stmts += 1;
    }
    let full = t0.elapsed();
    println!(
        "{:<24}: {:>10.2?}  ({:.3} us/row, {} stmts)",
        tag,
        full,
        full.as_nanos() as f64 / n as f64 / 1000.0,
        stmts
    );
    rustqlite::api::profile::dump();
    rustqlite::api::profile::reset();
}

fn main() {
    rustqlite::api::profile::ENABLED.store(true, std::sync::atomic::Ordering::SeqCst);
    let n = 2000;
    let chunk = 100;

    let mut db = Database::open_in_memory().unwrap();
    for t in ["t1", "t2", "t3"] {
        db.execute(
            &format!("CREATE TABLE {t} (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)"),
            [],
        )
        .unwrap();
    }

    // Phase 0: warm-up (page growth, allocator arenas) on t1.
    run_phase(&mut db, "t1", "warm-up (cache ON)", n, chunk);

    // Phase 1: cache ON (default capacity), fresh table t2.
    run_phase(&mut db, "t2", "cache ON", n, chunk);

    // Phase 2: cache DISABLED, fresh table t3.
    db.set_stmt_cache_capacity(0);
    run_phase(&mut db, "t3", "cache OFF", n, chunk);
}
