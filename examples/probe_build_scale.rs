//! Marathon M1 build shape in isolation: `events (id INTEGER PRIMARY KEY,
//! k INTEGER)` + `ix_events_k`, 20k-row multi-VALUES batches inside
//! BEGIN..COMMIT, WAL + synchronous=NORMAL. Prints per-decile batch/commit
//! latency, the cache hit/miss profile, file size and RSS — the head-vs-tail
//! curve that separates insert-path costs from commit/WAL costs.
//!
//! `--sqlite` runs the IDENTICAL statements (same generator, same batch
//! text) on bundled rusqlite for the ratio.

use std::time::Instant;

use rustqlite::Database;

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

/// The marathon's exact generator: k = id % 1_000_000.
fn gen_k(i: u64) -> i64 {
    (i % 1_000_000) as i64
}

fn build_batch_sql(start: u64, n: u64) -> String {
    let mut sql = String::with_capacity(64 * n as usize + 64);
    sql.push_str("INSERT INTO events (id, k) VALUES ");
    for j in 0..n {
        let i = start + j + 1;
        if j > 0 {
            sql.push(',');
        }
        sql.push_str(&format!("({}, {})", i, gen_k(i)));
    }
    sql
}

fn decile_report(label: &str, ms: &[f64]) {
    if ms.is_empty() {
        return;
    }
    let n = ms.len();
    let mut parts = Vec::new();
    for d in 0..10 {
        let lo = d * n / 10;
        let hi = (d + 1) * n / 10;
        if lo >= hi {
            continue;
        }
        let slice = &ms[lo..hi];
        let med = {
            let mut v = slice.to_vec();
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            v[v.len() / 2]
        };
        parts.push(format!("d{d}={med:.1}"));
    }
    println!("  {label}: {}", parts.join(" "));
}

fn db_bytes(path: &str) -> u64 {
    let mut total = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if let Ok(m) = std::fs::metadata(format!("{path}-wal")) {
        total += m.len();
    }
    total
}

fn main() {
    let rows: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(20_000_000);
    let batch: u64 = 20_000;
    let vs_sqlite = std::env::args().any(|a| a == "--sqlite");
    println!("build_scale rows={rows} batch={batch} vs_sqlite={vs_sqlite}");

    // ---------------- engine ----------------
    {
        let path = "/tmp/build_scale.rq.db";
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(format!("{path}-wal"));
        let mut db = Database::open(path).unwrap();
        db.execute("PRAGMA journal_mode = WAL", []).unwrap();
        db.execute("PRAGMA synchronous = NORMAL", []).unwrap();
        db.execute(
            "CREATE TABLE events (id INTEGER PRIMARY KEY, k INTEGER)",
            [],
        )
        .unwrap();
        db.execute("CREATE INDEX ix_events_k ON events (k)", [])
            .unwrap();

        let mut batch_ms: Vec<f64> = Vec::new();
        let mut commit_ms: Vec<f64> = Vec::new();
        let t0 = Instant::now();
        let mut next_id = 0u64;
        while next_id < rows {
            let n = batch.min(rows - next_id);
            let sql = build_batch_sql(next_id, n);
            db.execute("BEGIN", []).unwrap();
            let t = Instant::now();
            db.execute(&sql, []).unwrap();
            batch_ms.push(t.elapsed().as_secs_f64() * 1000.0);
            let t = Instant::now();
            db.execute("COMMIT", []).unwrap();
            commit_ms.push(t.elapsed().as_secs_f64() * 1000.0);
            next_id += n;
        }
        let build_s = t0.elapsed().as_secs_f64();
        let (hits, misses) = db.cache_hit_stats();
        let (c_size, c_cap) = db.cache_stats();
        let count: i64 =
            db.query("SELECT count(*), sum(k) FROM events", []).unwrap()[0][0].as_integer();
        let sum: i64 =
            db.query("SELECT count(*), sum(k) FROM events", []).unwrap()[0][1].as_integer();
        let exp_sum: i64 = (1..=rows).map(gen_k).sum();
        assert_eq!(count as u64, rows, "engine count");
        assert_eq!(sum, exp_sum, "engine sum");
        // secondary index integrity through a real index-ordered scan
        let idx_rows = db
            .query("SELECT count(*) FROM events WHERE k >= 0", [])
            .unwrap()[0][0]
            .as_integer();
        assert_eq!(idx_rows as u64, rows, "engine index-visible count");
        println!(
            "engine: build {rows} rows in {build_s:.2}s = {:.0} rows/s | rss={:.1}MB | cache {c_size}/{c_cap} hits={hits} misses={misses} ({:.1}% miss) | db+wal={:.1}MB",
            rows as f64 / build_s,
            rss(),
            misses as f64 / (hits + misses).max(1) as f64 * 100.0,
            db_bytes(path) as f64 / (1 << 20) as f64,
        );
        decile_report("batch-ms", &batch_ms);
        decile_report("commit-ms", &commit_ms);
        let _ = count;
    }

    // ---------------- sqlite ----------------
    if vs_sqlite {
        let spath = "/tmp/build_scale.sq.db";
        let _ = std::fs::remove_file(spath);
        let _ = std::fs::remove_file(format!("{spath}-wal"));
        let rc = rusqlite::Connection::open(spath).unwrap();
        rc.execute_batch("PRAGMA journal_mode = WAL;").unwrap();
        rc.execute_batch("PRAGMA synchronous = NORMAL;").unwrap();
        rc.execute_batch("CREATE TABLE events (id INTEGER PRIMARY KEY, k INTEGER);")
            .unwrap();
        rc.execute_batch("CREATE INDEX ix_events_k ON events (k);")
            .unwrap();
        let t0 = Instant::now();
        let mut next_id = 0u64;
        while next_id < rows {
            let n = batch.min(rows - next_id);
            let sql = build_batch_sql(next_id, n);
            rc.execute_batch(&format!("BEGIN;\n{sql};\nCOMMIT;"))
                .unwrap();
            next_id += n;
        }
        let build_s = t0.elapsed().as_secs_f64();
        let (count, sum): (i64, i64) = rc
            .query_row("SELECT count(*), sum(k) FROM events", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        let exp_sum: i64 = (1..=rows).map(gen_k).sum();
        assert_eq!(count as u64, rows, "sqlite count");
        assert_eq!(sum, exp_sum, "sqlite sum");
        println!(
            "sqlite: build {rows} rows in {build_s:.2}s = {:.0} rows/s | db+wal={:.1}MB",
            rows as f64 / build_s,
            db_bytes(spath) as f64 / (1 << 20) as f64,
        );
    }
}
