//! Band-update engine-vs-SQLite ratio probe: the M9 comparator shape at
//! any scale. Build N rows (events + ix_events_k) on BOTH engines, run
//! `UPDATE events SET k = k + 1 WHERE id % 20 = 7 AND id <= N` on both,
//! print the ratio. Run:
//!   cargo run --release --example bandupd_probe -- [rows]
use rustqlite::Database;
use std::time::Instant;

fn main() {
    let rows: u64 = std::env::args()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_000_000);
    let dir = tempfile::tempdir().unwrap();

    // ---- rustqlite ----
    let epath = dir.path().join("eng.db");
    let mut db = Database::open(&epath).unwrap();
    db.execute("PRAGMA journal_mode = WAL", []).unwrap();
    db.execute("PRAGMA synchronous = NORMAL", []).unwrap();
    let t = Instant::now();
    db.execute(
        "CREATE TABLE events (id INTEGER PRIMARY KEY, k INTEGER, note TEXT)",
        [],
    )
    .unwrap();
    db.execute("CREATE INDEX ix_events_k ON events (k)", [])
        .unwrap();
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
        "[cmp] engine build {rows} in {:.1}s",
        t.elapsed().as_secs_f64()
    );
    let t = Instant::now();
    db.execute(
        &format!("UPDATE events SET k = k + 1 WHERE id % 20 = 7 AND id <= {rows}"),
        [],
    )
    .unwrap();
    let eng_ms = t.elapsed().as_secs_f64() * 1000.0;
    println!("[cmp] engine band-update {eng_ms:.0}ms");
    drop(db);

    // ---- bundled SQLite (rusqlite) ----
    let spath = dir.path().join("sq.db");
    let sc = rusqlite::Connection::open(&spath).unwrap();
    sc.pragma_update(None, "journal_mode", "WAL").unwrap();
    sc.pragma_update(None, "synchronous", "NORMAL").unwrap();
    let t = Instant::now();
    let mut sql = String::with_capacity(64 * rows as usize + 128);
    sql.push_str("CREATE TABLE events (id INTEGER PRIMARY KEY, k INTEGER, note TEXT);");
    sql.push_str("CREATE INDEX ix_events_k ON events (k);");
    let batch = 5000u64;
    let mut next = 0u64;
    while next < rows {
        let n = batch.min(rows - next);
        sql.push_str("BEGIN;INSERT INTO events (id, k, note) VALUES ");
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
        sql.push_str(";COMMIT;");
        next += n;
    }
    sc.execute_batch(&sql).unwrap();
    println!(
        "[cmp] sqlite build {rows} in {:.1}s",
        t.elapsed().as_secs_f64()
    );
    let t = Instant::now();
    sc.execute(
        &format!("UPDATE events SET k = k + 1 WHERE id % 20 = 7 AND id <= {rows}"),
        [],
    )
    .unwrap();
    let sq_ms = t.elapsed().as_secs_f64() * 1000.0;
    println!("[cmp] sqlite band-update {sq_ms:.0}ms");
    drop(sc);

    println!(
        "[cmp] rows={rows} engine={eng_ms:.0}ms sqlite={sq_ms:.0}ms ratio={:.2}x",
        eng_ms / sq_ms.max(1e-9)
    );
}
