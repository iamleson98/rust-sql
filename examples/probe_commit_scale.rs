//! Per-commit cost on LARGE SQLite-format files — rustqlite vs real
//! SQLite (rusqlite, bundled amalgamation). This is the benchmark row
//! for the incremental page-diff architecture: after a session's first
//! full publish, every commit splices ONLY the changed objects' pages
//! (per-root change epochs pick them), so per-commit CPU and I/O are
//! O(changed object) — the whole image is never re-derived. The old
//! container paid an O(database) diff at EVERY commit boundary.
//!
//! What is measured, per engine, at three file scales (rows in the
//! table): the wall time of M autocommit single-row statements against
//! a quiet database in WAL mode with `synchronous=OFF` (isolates the
//! commit CPU + frame-append path from the fsync floor; the fsync-floor
//! parity shape is already covered by the mixed-R/W bench row):
//!   - INSERT (auto rowid, ~100-byte row, scattered index key)
//!   - UPDATE by PK (row rewrite + index key move)
//!
//! The scaling column is the point: O(changed object) means per-commit
//! cost stays FLAT as the file grows; any residual slope is cache
//! pressure, not commit-path work.
//!
//! Run: `cargo run --release --example probe_commit_scale [-- rows small
//! m]` (defaults: scales 100k/500k/2M, warmup 2M rows is the standard
//! large-file size, M=300 statement pair per scale).
use rusqlite::params;
use rustqlite::{Database, Value};
use std::time::Instant;

const PAD: usize = 96;

fn pad_for(i: i64, salt: i64) -> String {
    let mut s = format!("p{i}x{salt}-");
    while s.len() < PAD {
        s.push('x');
    }
    s
}

/// rustqlite, SQLite-format container: build `n` rows, then time `m`
/// autocommit INSERTs + `m` autocommit UPDATE-by-PK. Returns
/// (ins_us, upd_us, file_bytes).
fn engine_round(path: &std::path::Path, n: i64, m: i64) -> (f64, f64, u64) {
    let _ = std::fs::remove_file(path);
    for p in [
        format!("{}-wal", path.display()),
        format!("{}-shm", path.display()),
        format!("{}-journal", path.display()),
    ] {
        let _ = std::fs::remove_file(p);
    }
    let mut db = Database::open_sqlite_format(path).unwrap();
    db.execute("PRAGMA journal_mode = WAL", []).unwrap();
    db.execute("PRAGMA synchronous = OFF", []).unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b INTEGER, pad TEXT)",
        [],
    )
    .unwrap();
    db.execute("CREATE INDEX ib ON t(b)", []).unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=n {
        db.execute(
            "INSERT INTO t(a, b, pad) VALUES (?, ?, ?)",
            [
                Value::Text(format!("a{i}").into()),
                Value::Integer((i).wrapping_mul(48271).rem_euclid(n)), // scattered
                Value::Text(pad_for(i, 0).into()),
            ],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();

    // Autocommit INSERTs (scattered index keys: each commit dirties the
    // table leaf + the index leaf).
    let t0 = Instant::now();
    for i in 1..=m {
        db.execute(
            "INSERT INTO t(a, b, pad) VALUES (?, ?, ?)",
            [
                Value::Text(format!("n{i}").into()),
                Value::Integer((i).wrapping_mul(7919).rem_euclid(n)),
                Value::Text(pad_for(i, 1).into()),
            ],
        )
        .unwrap();
    }
    let ins = t0.elapsed().as_secs_f64() / m as f64;

    // Autocommit UPDATEs by PK (row rewrite + index key move).
    let t1 = Instant::now();
    for i in 1..=m {
        db.execute(
            "UPDATE t SET b = ? WHERE id = ?",
            [
                Value::Integer((i).wrapping_mul(104729).rem_euclid(n)),
                Value::Integer(i),
            ],
        )
        .unwrap();
    }
    let upd = t1.elapsed().as_secs_f64() / m as f64;

    let want = n + m;
    let got = db.query("SELECT count(*) FROM t", []).unwrap()[0][0].clone();
    assert_eq!(
        format!("{got:?}"),
        format!("Integer({want})"),
        "engine row count mismatch at n={n}"
    );
    drop(db);
    let file = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let _ = std::fs::remove_file(path);
    for p in [
        format!("{}-wal", path.display()),
        format!("{}-shm", path.display()),
        format!("{}-journal", path.display()),
    ] {
        let _ = std::fs::remove_file(p);
    }
    (ins * 1e6, upd * 1e6, file)
}

/// real SQLite (rusqlite bundled): the identical protocol.
fn sqlite_round(path: &std::path::Path, n: i64, m: i64) -> (f64, f64, u64) {
    let _ = std::fs::remove_file(path);
    for p in [
        format!("{}-wal", path.display()),
        format!("{}-shm", path.display()),
        format!("{}-journal", path.display()),
    ] {
        let _ = std::fs::remove_file(p);
    }
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    conn.pragma_update(None, "synchronous", "OFF").unwrap();
    conn.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b INTEGER, pad TEXT)",
        [],
    )
    .unwrap();
    conn.execute("CREATE INDEX ib ON t(b)", []).unwrap();
    conn.execute_batch("BEGIN").unwrap();
    for i in 1..=n {
        conn.execute(
            "INSERT INTO t(a, b, pad) VALUES (?1, ?2, ?3)",
            params![
                format!("a{i}"),
                (i.wrapping_mul(48271)).rem_euclid(n),
                pad_for(i, 0)
            ],
        )
        .unwrap();
    }
    conn.execute_batch("COMMIT").unwrap();

    let t0 = Instant::now();
    for i in 1..=m {
        conn.execute(
            "INSERT INTO t(a, b, pad) VALUES (?1, ?2, ?3)",
            params![
                format!("n{i}"),
                (i.wrapping_mul(7919)).rem_euclid(n),
                pad_for(i, 1)
            ],
        )
        .unwrap();
    }
    let ins = t0.elapsed().as_secs_f64() / m as f64;

    let t1 = Instant::now();
    for i in 1..=m {
        conn.execute(
            "UPDATE t SET b = ?1 WHERE id = ?2",
            params![(i.wrapping_mul(104729)).rem_euclid(n), i],
        )
        .unwrap();
    }
    let upd = t1.elapsed().as_secs_f64() / m as f64;

    let got: i64 = conn
        .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
        .unwrap();
    assert_eq!(got, n + m, "sqlite row count mismatch at n={n}");
    drop(conn);
    let file = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let _ = std::fs::remove_file(path);
    for p in [
        format!("{}-wal", path.display()),
        format!("{}-shm", path.display()),
        format!("{}-journal", path.display()),
    ] {
        let _ = std::fs::remove_file(p);
    }
    (ins * 1e6, upd * 1e6, file)
}

/// Multi-object shape: `tables` small tables of `per_table` rows each
/// (same total file scale); the measured commits all touch ONE table —
/// the O(changed object) claim of the incremental page-diff architecture.
fn engine_many_objects(path: &std::path::Path, tables: i64, per_table: i64, m: i64) -> f64 {
    let _ = std::fs::remove_file(path);
    let mut db = Database::open_sqlite_format(path).unwrap();
    db.execute("PRAGMA journal_mode = WAL", []).unwrap();
    db.execute("PRAGMA synchronous = OFF", []).unwrap();
    for t in 0..tables {
        db.execute(
            &format!("CREATE TABLE t{t} (id INTEGER PRIMARY KEY, a TEXT, b INTEGER, pad TEXT)"),
            [],
        )
        .unwrap();
    }
    db.execute("BEGIN", []).unwrap();
    for t in 0..tables {
        for i in 1..=per_table {
            db.execute(
                &format!("INSERT INTO t{t}(a, b, pad) VALUES ('a{i}', {i}, ?)"),
                [Value::Text(pad_for(i, 0).into())],
            )
            .unwrap();
        }
    }
    db.execute("COMMIT", []).unwrap();
    // All commits touch table t0 (per_table rows) on a file of
    // tables*per_table rows.
    let t0 = Instant::now();
    for i in 1..=m {
        db.execute(
            "INSERT INTO t0(a, b, pad) VALUES ('n', ?, ?)",
            [Value::Integer(i), Value::Text(pad_for(i, 1).into())],
        )
        .unwrap();
    }
    let ins = t0.elapsed().as_secs_f64() / m as f64;
    drop(db);
    let _ = std::fs::remove_file(path);
    ins * 1e6
}

fn sqlite_many_objects(path: &std::path::Path, tables: i64, per_table: i64, m: i64) -> f64 {
    let _ = std::fs::remove_file(path);
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    conn.pragma_update(None, "synchronous", "OFF").unwrap();
    for t in 0..tables {
        conn.execute(
            &format!("CREATE TABLE t{t} (id INTEGER PRIMARY KEY, a TEXT, b INTEGER, pad TEXT)"),
            [],
        )
        .unwrap();
    }
    conn.execute_batch("BEGIN").unwrap();
    for t in 0..tables {
        for i in 1..=per_table {
            conn.execute(
                &format!("INSERT INTO t{t}(a, b, pad) VALUES ('a{i}', {i}, ?1)"),
                params![pad_for(i, 0)],
            )
            .unwrap();
        }
    }
    conn.execute_batch("COMMIT").unwrap();
    let t0 = Instant::now();
    for i in 1..=m {
        conn.execute(
            "INSERT INTO t0(a, b, pad) VALUES ('n', ?1, ?2)",
            params![i, pad_for(i, 1)],
        )
        .unwrap();
    }
    let ins = t0.elapsed().as_secs_f64() / m as f64;
    drop(conn);
    let _ = std::fs::remove_file(path);
    ins * 1e6
}

fn main() {
    let mut args = std::env::args().skip(1);
    let small: i64 = args.next().and_then(|a| a.parse().ok()).unwrap_or(100_000);
    let m: i64 = args.next().and_then(|a| a.parse().ok()).unwrap_or(300);
    let scales = [small, small * 5, small * 20];
    println!("Per-commit cost, 1-row autocommit, WAL + synchronous=OFF (us/commit; M={m} per op)");
    println!("scenario A: single large table (the changed object IS the file)");
    println!(
        "{:>10} {:>22} {:>22} {:>12} {:>12}",
        "rows", "rustqlite ins/upd", "SQLite ins/upd", "ins ratio", "upd ratio"
    );
    let dir = tempfile::tempdir().unwrap();
    for n in scales {
        let (e_ins, e_upd, e_file) = engine_round(&dir.path().join("e.db"), n, m);
        let (s_ins, s_upd, s_file) = sqlite_round(&dir.path().join("s.db"), n, m);
        println!(
            "{:>10} {:>10.1} /{:<10.1} {:>10.1} /{:<10.1} {:>11.2}x {:>11.2}x   (files: {} MB vs {} MB)",
            n,
            e_ins,
            e_upd,
            s_ins,
            s_upd,
            s_ins / e_ins,
            s_upd / e_upd,
            e_file / 1_000_000,
            s_file / 1_000_000,
        );
    }
    println!(
        "scenario B: many small tables (commit touches ONE {small}-row table; file scale scales)"
    );
    for tables in [small / 100, small / 10] {
        let per_table = 100i64;
        let e = engine_many_objects(&dir.path().join("me.db"), tables, per_table, m);
        let s = sqlite_many_objects(&dir.path().join("ms.db"), tables, per_table, m);
        println!(
            "{:>8} tables x {:>4} rows ({:>8} total): rustqlite {:>9.1} us vs SQLite {:>7.1} us  ({:>8.2}x)",
            tables,
            per_table,
            tables * per_table,
            e,
            s,
            s / e
        );
    }
}
