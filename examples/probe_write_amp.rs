//! Write-amplification probe: replicates the reported production shape —
//! "the per-page save path issues ~5 autocommit statements per page" —
//! against a SQLite-format (foreign/interop) database, and against real
//! SQLite (rusqlite, bundled amalgamation) as the standard-behavior
//! oracle. Cumulative bytes written are counted at the syscall level by
//! the LD_PRELOAD shim (iocount.c) in the driver script.
//!
//! Scenarios (argv[1]):
//!   engine-delete-auto   rustqlite SQLite-format, default journal mode,
//!                        every statement autocommits
//!   engine-delete-batch  same, but BEGIN..COMMIT around each 100-page
//!                        batch (the app-level batching fix)
//!   engine-wal-auto      PRAGMA journal_mode=WAL, autocommit
//!   engine-wal-batch     WAL + 100-page batches
//!   sqlite-delete-auto   real SQLite, default journal mode, autocommit
//!   sqlite-delete-batch  real SQLite, batched
//!   sqlite-wal-auto      real SQLite, WAL, autocommit
//!   sqlite-wal-batch     real SQLite, WAL, batched
//!
//! Pages (argv[2], default 400). Each app-page = 5 statements:
//!   1 INSERT pages (meta ~120B)
//!   2 INSERT rows  (~1.9KB payload each)
//!   1 UPDATE pages SET rev = rev + 1
//!   1 DELETE FROM scratch (maintenance shape)
//! ~4KB of logical data per app-page, like the reported 40k-page bulk
//! parse (~160MB database).
use rustqlite::Value;
use std::time::Instant;

const META: &str = "meta-";
const ROW: &str = "row-";

fn payload(prefix: &str, page: i64, seq: i64, kb: usize) -> String {
    let mut s = format!("{prefix}{page}-{seq}-");
    while s.len() < kb * 1024 {
        s.push('x');
    }
    s
}

fn sidecar_paths(path: &std::path::Path) -> Vec<std::path::PathBuf> {
    let base = path.to_str().unwrap().to_string();
    vec![
        std::path::PathBuf::from(format!("{base}-wal")),
        std::path::PathBuf::from(format!("{base}-shm")),
        std::path::PathBuf::from(format!("{base}-journal")),
    ]
}

fn cleanup(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    for p in sidecar_paths(path) {
        let _ = std::fs::remove_file(p);
    }
}

fn file_len(path: &std::path::Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn run_engine(scenario: &str, pages: i64, dir: &str) -> std::time::Duration {
    let path = std::path::PathBuf::from(format!("{dir}/probe-engine-{scenario}.db"));
    cleanup(&path);
    // Production shape: the database file was created by REAL SQLite with
    // its default rollback (DELETE) journal mode — a plain .db with no
    // sidecars. The engine opens it through the SQLite-format interop
    // path (Database::open's magic sniff -> from_sqlite_file). The
    // journal mode then comes from the file header (18/19 = 1/1).
    let seed = rusqlite::Connection::open(&path).unwrap();
    seed.execute(
        "CREATE TABLE pages (id INTEGER PRIMARY KEY, name TEXT, rev INTEGER, payload TEXT)",
        [],
    )
    .unwrap();
    seed.execute(
        "CREATE TABLE rows (id INTEGER PRIMARY KEY, page_id INTEGER, seq INTEGER, data TEXT)",
        [],
    )
    .unwrap();
    seed.execute(
        "CREATE TABLE scratch (page_id INTEGER, seq INTEGER, note TEXT)",
        [],
    )
    .unwrap();
    drop(seed);
    let mut db = rustqlite::Database::open(&path).unwrap();
    if scenario.contains("wal") {
        db.execute("PRAGMA journal_mode = WAL", []).unwrap();
    }

    let batch = scenario.contains("batch");
    let mut stats = DbStats::default();
    let start = Instant::now();
    for p in 1..=pages {
        if batch && (p - 1) % 100 == 0 {
            db.execute("BEGIN", []).unwrap();
        }
        db.execute(
            "INSERT INTO pages (name, rev, payload) VALUES (?, ?, ?)",
            [
                Value::Text(format!("page-{p}").into()),
                Value::Integer(0),
                Value::Text(payload(META, p, 0, 0).into()),
            ],
        )
        .unwrap();
        db.execute(
            "INSERT INTO rows (page_id, seq, data) VALUES (?, ?, ?)",
            [
                Value::Integer(p),
                Value::Integer(1),
                Value::Text(payload(ROW, p, 1, 1).into()),
            ],
        )
        .unwrap();
        db.execute(
            "INSERT INTO rows (page_id, seq, data) VALUES (?, ?, ?)",
            [
                Value::Integer(p),
                Value::Integer(2),
                Value::Text(payload(ROW, p, 2, 1).into()),
            ],
        )
        .unwrap();
        db.execute(
            "UPDATE pages SET rev = rev + 1 WHERE id = ?",
            [Value::Integer(p)],
        )
        .unwrap();
        db.execute("DELETE FROM scratch WHERE page_id = ?", [Value::Integer(p)])
            .unwrap();
        if batch && p % 100 == 0 {
            db.execute("COMMIT", []).unwrap();
        }
    }
    if batch && pages % 100 != 0 {
        db.execute("COMMIT", []).unwrap();
    }
    let elapsed = start.elapsed();
    stats.verify_engine(&mut db, pages);
    drop(db);
    report("engine", scenario, pages, &path, elapsed);
    elapsed
}

#[derive(Default)]
struct DbStats {
    pages: i64,
    rows: i64,
    rev_sum: i64,
}

impl DbStats {
    fn verify_engine(&mut self, db: &mut rustqlite::Database, pages: i64) {
        let r = db
            .query("SELECT COUNT(*), COALESCE(SUM(rev), 0) FROM pages", [])
            .unwrap();
        self.pages = r[0][0].as_integer();
        self.rev_sum = r[0][1].as_integer();
        let r = db.query("SELECT COUNT(*) FROM rows", []).unwrap();
        self.rows = r[0][0].as_integer();
        assert_eq!(self.pages, pages, "pages row count");
        assert_eq!(self.rows, 2 * pages, "rows row count");
        assert_eq!(self.rev_sum, pages, "rev sum (one UPDATE per page)");
    }
}

fn run_sqlite(scenario: &str, pages: i64, dir: &str) -> std::time::Duration {
    let path = format!("{dir}/probe-sqlite-{scenario}.db");
    let p = std::path::Path::new(&path);
    cleanup(p);
    let conn = rusqlite::Connection::open(&path).unwrap();
    if scenario.contains("wal") {
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    }
    conn.execute(
        "CREATE TABLE pages (id INTEGER PRIMARY KEY, name TEXT, rev INTEGER, payload TEXT)",
        [],
    )
    .unwrap();
    conn.execute(
        "CREATE TABLE rows (id INTEGER PRIMARY KEY, page_id INTEGER, seq INTEGER, data TEXT)",
        [],
    )
    .unwrap();
    conn.execute(
        "CREATE TABLE scratch (page_id INTEGER, seq INTEGER, note TEXT)",
        [],
    )
    .unwrap();

    let batch = scenario.contains("batch");
    let start = Instant::now();
    for pg in 1..=pages {
        if batch && (pg - 1) % 100 == 0 {
            conn.execute_batch("BEGIN").unwrap();
        }
        conn.execute(
            "INSERT INTO pages (name, rev, payload) VALUES (?, ?, ?)",
            rusqlite::params![format!("page-{pg}"), 0i64, payload(META, pg, 0, 0)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO rows (page_id, seq, data) VALUES (?, ?, ?)",
            rusqlite::params![pg, 1i64, payload(ROW, pg, 1, 1)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO rows (page_id, seq, data) VALUES (?, ?, ?)",
            rusqlite::params![pg, 2i64, payload(ROW, pg, 2, 1)],
        )
        .unwrap();
        conn.execute(
            "UPDATE pages SET rev = rev + 1 WHERE id = ?",
            rusqlite::params![pg],
        )
        .unwrap();
        conn.execute(
            "DELETE FROM scratch WHERE page_id = ?",
            rusqlite::params![pg],
        )
        .unwrap();
        if batch && pg % 100 == 0 {
            conn.execute_batch("COMMIT").unwrap();
        }
    }
    if batch && pages % 100 != 0 {
        conn.execute_batch("COMMIT").unwrap();
    }
    let elapsed = start.elapsed();
    let (n, rev): (i64, i64) = conn
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(rev), 0) FROM pages",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    let nrows: i64 = conn
        .query_row("SELECT COUNT(*) FROM rows", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, pages, "sqlite pages row count");
    assert_eq!(nrows, 2 * pages, "sqlite rows row count");
    assert_eq!(rev, pages, "sqlite rev sum");
    drop(conn);
    report("sqlite", scenario, pages, p, elapsed);
    elapsed
}

fn report(
    engine: &str,
    scenario: &str,
    pages: i64,
    path: &std::path::Path,
    elapsed: std::time::Duration,
) {
    let db = file_len(path);
    let mut side = 0u64;
    for p in sidecar_paths(path) {
        side += file_len(&p);
    }
    let logical = pages * 4 * 1024;
    println!(
        "RESULT engine={engine} scenario={scenario} pages={pages} stmts={} \
         db_bytes={db} sidecar_bytes={side} logical_bytes={logical} \
         elapsed_ms={:.0}",
        pages * 5,
        elapsed.as_millis()
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let scenario = args
        .get(1)
        .map(|s| s.as_str())
        .expect("usage: probe_write_amp <scenario> [pages]");
    let pages: i64 = args
        .get(2)
        .map(|s| s.parse().expect("pages"))
        .or_else(|| std::env::var("PAGES").ok().and_then(|v| v.parse().ok()))
        .unwrap_or(400);
    let dir = std::env::var("PROBE_DIR").unwrap_or_else(|_| ".".to_string());
    if scenario.starts_with("engine-") {
        run_engine(scenario, pages, &dir);
    } else if scenario.starts_with("sqlite-") {
        run_sqlite(scenario, pages, &dir);
    } else {
        eprintln!("unknown scenario {scenario}");
        std::process::exit(2);
    }
}
