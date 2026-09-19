//! Probe: cold-start (open + first query) latency on SUPER LARGE files,
//! engine vs real SQLite, both disk containers.
//!
//! Modes (driven by argv[1], paths by argv[2..]):
//!   build-sqlite  <file> <rows>   — create the SQLite-format source with rusqlite
//!   build-native  <file> <rows>   — create the native-format file with the engine
//!   bench-sqlite  <file>          — rusqlite: open, first query, second query, RSS
//!   bench-sqlfmt  <file>          — engine on the SQLite-format file: same
//!   bench-native  <file>          — engine on the native-format file: same
//!
//! Each bench mode prints one line: `RESULT <open_ms> <q1_ms> <q2_ms> <rss_mb>`.
//! Run each bench in a FRESH process (the parent shell does), with the page
//! cache dropped via posix_fadvise(DONTNEED) between runs.
use std::process::exit;
use std::time::Instant;

fn vmhwm_mb() -> f64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            let kb: f64 = rest
                .trim()
                .trim_end_matches("kB")
                .trim()
                .parse()
                .unwrap_or(0.0);
            return kb / 1024.0;
        }
    }
    0.0
}

const Q1: &str = "SELECT COUNT(*), SUM(a) FROM t";
const Q2: &str = "SELECT txt FROM t WHERE id = 424242";

fn build_sqlite(file: &str, rows: i64) {
    let conn = rusqlite::Connection::open(file).unwrap();
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=OFF;
         CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, txt TEXT);
         CREATE INDEX ia ON t(a);",
    )
    .unwrap();
    let t0 = Instant::now();
    conn.execute_batch("BEGIN").unwrap();
    {
        use rusqlite::params;
        let mut stmt = conn
            .prepare("INSERT INTO t (id, a, txt) VALUES (?, ?, ?)")
            .unwrap();
        let mut batch = String::new();
        for i in 1..=rows {
            batch.clear();
            // ~250-byte payload, deterministic content
            let txt = format!(
                "row-{:07}-abcdefghijklmnopqrstuvwxyz-0123456789-{:05}-{}",
                i,
                (i * 7919) % 100000,
                "x".repeat(180)
            );
            stmt.execute(params![i, i % 100_000, txt]).unwrap();
            if i % 100000 == 0 {
                println!(
                    "  built {}/{} rows ({:.1}s)",
                    i,
                    rows,
                    t0.elapsed().as_secs_f64()
                );
            }
        }
    }
    conn.execute_batch("COMMIT; PRAGMA wal_checkpoint(TRUNCATE);")
        .unwrap();
    let sz = std::fs::metadata(file).unwrap().len() as f64 / (1024.0 * 1024.0);
    println!(
        "BUILT sqlite-format: {} rows, {:.0} MB, build {:.1}s",
        rows,
        sz,
        t0.elapsed().as_secs_f64()
    );
}

fn build_native(file: &str, rows: i64) {
    use rustqlite::types::Value;
    // Plain Database::open on a fresh path creates the engine's NATIVE
    // container (RSQLDB04).
    let mut db = rustqlite::Database::open(file).unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, txt TEXT); CREATE INDEX ia ON t(a);",
        [],
    )
    .unwrap();
    let t0 = Instant::now();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=rows {
        let txt = format!(
            "row-{:07}-abcdefghijklmnopqrstuvwxyz-0123456789-{:05}-{}",
            i,
            (i * 7919) % 100000,
            "x".repeat(180)
        );
        db.execute(
            "INSERT INTO t (id, a, txt) VALUES (?, ?, ?)",
            [
                Value::Integer(i),
                Value::Integer(i % 100_000),
                Value::Text(txt.into()),
            ],
        )
        .unwrap();
        if i % 100000 == 0 {
            println!(
                "  built {}/{} rows ({:.1}s)",
                i,
                rows,
                t0.elapsed().as_secs_f64()
            );
        }
    }
    db.execute("COMMIT", []).unwrap();
    drop(db);
    let sz = std::fs::metadata(file).unwrap().len() as f64 / (1024.0 * 1024.0);
    println!(
        "BUILT native-format: {} rows, {:.0} MB, build {:.1}s",
        rows,
        sz,
        t0.elapsed().as_secs_f64()
    );
}

fn bench_sqlite(file: &str) {
    let t0 = Instant::now();
    let conn = rusqlite::Connection::open(file).unwrap();
    let open = t0.elapsed();
    let t1 = Instant::now();
    let (n, s): (i64, i64) = conn
        .query_row(Q1, [], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap();
    let q1 = t1.elapsed();
    let t2 = Instant::now();
    let _txt: String = conn
        .query_row(Q2, [], |r| r.get(0))
        .unwrap_or_else(|_| "MISS".into());
    let q2 = t2.elapsed();
    println!(
        "RESULT sqlite  open={:.1}ms q1={:.1}ms q2={:.2}ms rss={:.0}MB count={} sum={}",
        open.as_secs_f64() * 1e3,
        q1.as_secs_f64() * 1e3,
        q2.as_secs_f64() * 1e3,
        vmhwm_mb(),
        n,
        s
    );
    assert!(n > 0, "empty table");
}

fn bench_engine(file: &str, sqlfmt: bool) {
    // open auto-detects the container by magic (SQLite fileformat2 vs RSQLDB04)
    let t0 = Instant::now();
    let db = rustqlite::Database::open(file).unwrap();
    let open = t0.elapsed();
    let t1 = Instant::now();
    let rows = db.query(Q1, []).unwrap();
    let n = rows[0][0].as_integer();
    let s = rows[0][1].as_integer();
    let q1 = t1.elapsed();
    let t2 = Instant::now();
    let _ = db.query(Q2, []).unwrap();
    let q2 = t2.elapsed();
    let kind = if sqlfmt { "sqlfmt" } else { "native" };
    println!(
        "RESULT {}  open={:.1}ms q1={:.1}ms q2={:.2}ms rss={:.0}MB count={} sum={}",
        kind,
        open.as_secs_f64() * 1e3,
        q1.as_secs_f64() * 1e3,
        q2.as_secs_f64() * 1e3,
        vmhwm_mb(),
        n,
        s
    );
    assert!(n > 0, "empty table");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: probe_cold_large <build-sqlite|build-native|bench-sqlite|bench-sqlfmt|bench-native> <file> [rows]");
        exit(2);
    }
    match args[1].as_str() {
        "build-sqlite" => build_sqlite(
            &args[2],
            args.get(3)
                .and_then(|s| s.parse().ok())
                .unwrap_or(5_000_000),
        ),
        "build-native" => build_native(
            &args[2],
            args.get(3)
                .and_then(|s| s.parse().ok())
                .unwrap_or(5_000_000),
        ),
        "bench-sqlite" => bench_sqlite(&args[2]),
        "bench-sqlfmt" => bench_engine(&args[2], true),
        "bench-native" => bench_engine(&args[2], false),
        m => {
            eprintln!("unknown mode {m}");
            exit(2);
        }
    }
}
