//! Index-maintenance strategy probe (the sorted-batch apply round's
//! calibration data): where does a mass UPDATE-of-indexed-column spend
//! its time, and which apply strategy wins at each touched fraction?
//!
//! Measurements (all public-API, same statement shapes):
//!   A. UPDATE band WITH index     — today's per-row maintenance path
//!      (delete-descent + insert-descent per touched row)
//!   B. UPDATE band WITHOUT index  — the table-only floor
//!   C. CREATE INDEX on N rows     — the append-hinted sorted bulk build
//!      (the cost of building the ENTIRE index via the fast path)
//!   D. the rebuild strategy       — B + C: what a "rebuild the index
//!      from the table when the touched fraction is large" strategy
//!      would cost end to end
//!   E. SQLite A                   — the reference on identical hardware
//!
//! The strategy question this answers: per-row maintenance costs
//! ~2 random tree descents; a rebuild costs ~1 sequential pinned-leaf
//! append per TABLE row. If D < A at the marathon's M9 shapes, the
//! rebuild-in-place strategy (build a fresh index root from the updated
//! table, swap at statement end) is the cheap fix — no btree surgery —
//! and the probe gives the break-even touched fraction.
//!
//! Run: cargo run --release --example probe_index_maint -- [rows] [frac%]
//!   rows default 2,000,000; touched default 10% (id <= rows*frac).

use rustqlite::Database;
use std::time::Instant;

fn ms(d: std::time::Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Build the engine database (events table, k cyclic over 1M like the
/// marathon's M9 shape) and return it open. No index unless `with_index`.
fn build_engine(path: &std::path::Path, rows: u64, with_index: bool) -> Database {
    let mut db = Database::open(path).unwrap();
    db.execute("PRAGMA journal_mode = WAL", []).unwrap();
    db.execute("PRAGMA synchronous = NORMAL", []).unwrap();
    db.execute(
        "CREATE TABLE events (id INTEGER PRIMARY KEY, k INTEGER, note TEXT)",
        [],
    )
    .unwrap();
    if with_index {
        db.execute("CREATE INDEX ix_events_k ON events (k)", [])
            .unwrap();
    }
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
    db
}

fn main() {
    let rows: u64 = std::env::args()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_000_000);
    let frac_pct: u64 = std::env::args()
        .nth(2)
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);
    let band_end = (rows * frac_pct / 100).max(1);
    // The M9 shape: shift a band of k by half the cycle, id-ordered.
    let shift = 500_000i64;
    let upd_sql = format!("UPDATE events SET k = k + {shift} WHERE id <= {band_end}");
    let touched = band_end;
    let dir = tempfile::tempdir().unwrap();

    println!(
        "[probe] rows={rows} touched={touched} ({frac_pct}%) shape=M9 cyclic-k shift=+{shift}"
    );

    // ---- A: with index (today's per-row maintenance) ----
    let a_ms = {
        let mut db = build_engine(&dir.path().join("a.db"), rows, true);
        let t = Instant::now();
        db.execute(&upd_sql, []).unwrap();
        let a = t.elapsed();
        // Spot-check the answer stayed exact (k shifted for the band).
        let n: i64 = db
            .query(
                "SELECT COUNT(*) FROM events WHERE k >= 500000 AND k < 500000 + 1000 AND id <= 1000",
                [],
            )
            .unwrap()[0][0]
            .as_integer();
        println!(
            "[A] update WITH index (per-row maintain):  {:>9.1} ms  ({:.2} us/touched-row)  [spot-check {n}]",
            ms(a),
            ms(a) * 1000.0 / touched as f64
        );
        drop(db);
        ms(a)
    };

    // ---- B: without index (the table-only floor) ----
    let b_ms = {
        let mut db = build_engine(&dir.path().join("b.db"), rows, false);
        let t = Instant::now();
        db.execute(&upd_sql, []).unwrap();
        let b = t.elapsed();
        println!(
            "[B] update WITHOUT index (table floor):    {:>9.1} ms  ({:.2} us/touched-row)",
            ms(b),
            ms(b) * 1000.0 / touched as f64
        );
        drop(db);
        ms(b)
    };

    // ---- C: CREATE INDEX (append-hinted sorted bulk build) ----
    let c_ms = {
        let mut db = build_engine(&dir.path().join("c.db"), rows, false);
        db.execute(&upd_sql, []).unwrap(); // the same updated table D would build from
        let t = Instant::now();
        db.execute("CREATE INDEX ix_events_k ON events (k)", [])
            .unwrap();
        let c = t.elapsed();
        println!(
            "[C] CREATE INDEX on {rows} rows (bulk):     {:>9.1} ms  ({:.2} us/table-row)",
            ms(c),
            ms(c) * 1000.0 / rows as f64
        );
        drop(db);
        ms(c)
    };

    // ---- D: the rebuild strategy (B + C, what the engine would do) ----
    let d_ms = b_ms + c_ms;
    println!(
        "[D] rebuild strategy (B+C):                {:>9.1} ms  ({:.2} us/touched-row)",
        d_ms,
        d_ms * 1000.0 / touched as f64
    );

    // ---- E: bundled SQLite reference on A's shape ----
    let e_ms = {
        let spath = dir.path().join("sq.db");
        let sc = rusqlite::Connection::open(&spath).unwrap();
        sc.pragma_update(None, "journal_mode", "WAL").unwrap();
        sc.pragma_update(None, "synchronous", "NORMAL").unwrap();
        sc.execute_batch(
            "CREATE TABLE events (id INTEGER PRIMARY KEY, k INTEGER, note TEXT);
             CREATE INDEX ix_events_k ON events (k);",
        )
        .unwrap();
        let batch = 5000u64;
        let mut next = 0u64;
        while next < rows {
            let n = batch.min(rows - next);
            let mut sql = String::with_capacity(48 * n as usize + 48);
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
            sc.execute_batch(&sql).unwrap();
            next += n;
        }
        let t = Instant::now();
        sc.execute(&upd_sql, []).unwrap();
        let e = t.elapsed();
        println!(
            "[E] SQLite update WITH index (reference):  {:>9.1} ms  ({:.2} us/touched-row)",
            ms(e),
            ms(e) * 1000.0 / touched as f64
        );
        drop(sc);
        ms(e)
    };

    // ---- the verdict ----
    println!();
    println!(
        "index-maintenance share of A: {:.0}%  (A-B)/A",
        ((a_ms - b_ms) / a_ms * 100.0)
    );
    if d_ms < a_ms {
        // Break-even: maintenance cost is ~linear in touched rows
        // ((A-B)/touched per row); rebuild costs C regardless. t* rows
        // touched is where they cross: t* = C / per-row-maintenance.
        let per_row_maint = (a_ms - b_ms) / touched as f64;
        let t_star = c_ms.max(1e-9) / per_row_maint.max(1e-9);
        println!(
            "VERDICT: rebuild WINS at {frac_pct}% touched — D {d_ms:.0}ms vs A {a_ms:.0}ms ({:.2}x); \
             est. break-even ~ {:.0}% of the table (~{:.0} rows)",
            a_ms / d_ms,
            t_star * 100.0 / rows as f64,
            t_star
        );
    } else {
        println!(
            "VERDICT: per-row maintain WINS at {frac_pct}% touched — A {a_ms:.0}ms vs D {d_ms:.0}ms; \
             the sorted-batch apply (pinned-leaf walk) is the remaining lever"
        );
    }
    println!(
        "engine-vs-SQLite on A: {:.2}x ({})",
        a_ms / e_ms.max(1e-9),
        if a_ms <= e_ms { "WIN" } else { "LOSS" }
    );

    // ---- F: order sensitivity of the per-op index API (raw Btree) ----
    // The decisive number for the sorted-batch design: apply the SAME
    // (delete, insert) op set through the public per-op API in ROWID
    // order (today's executor order — cyclic keys scatter across the
    // whole tree) vs SORTED-by-key order (what a batch apply would
    // feed). If sorted order alone recovers most of the gap, the fix is
    // executor-side sorting with zero btree surgery; if not, the
    // pinned-leaf walk is required.
    experiment_f(KEYS_CYCLE, touched, shift);
}

/// The cyclic key space of the marathon's M9 shape (k = id % 1M).
const KEYS_CYCLE: u64 = 1_000_000;

/// Raw-Btree order-sensitivity experiment (see F above). Builds a
/// realistic `keys_cycle`-entry cyclic index tree with the append-hinted
/// bulk path, then times `touched` (delete+insert) pairs applied in
/// rowid order vs key-sorted order through the SAME per-op API.
fn experiment_f(keys_cycle: u64, touched: u64, shift: i64) {
    use rustqlite::storage::{Btree, Pager};

    let dir = tempfile::tempdir().unwrap();

    // Build a fresh `keys_cycle`-entry index tree in `file`, then time
    // `f` against it. Keys: BE-encoded integers — byte order == numeric
    // order, exactly like the engine's INTEGER-column index keys. The
    // bulk build uses the append-hinted path (CREATE INDEX's builder).
    let time_on_tree = |file: &str, f: &mut dyn FnMut(&mut Btree<'_>)| -> (f64, f64) {
        let path = dir.path().join(file);
        let pager = Pager::open(&path, 512).unwrap();
        let mut bt = Btree::create(&pager, true).unwrap();
        let mut hint = None;
        let t_build = Instant::now();
        for k in 0..keys_cycle as i64 {
            let kb = k.to_be_bytes();
            hint = bt.insert_index_append_hinted(&kb, k, hint).unwrap();
        }
        let build_ms = ms(t_build.elapsed());
        let t = Instant::now();
        f(&mut bt);
        (build_ms, ms(t.elapsed()))
    };

    // The band-update's op set: for rowid i in 1..=touched,
    // delete (i%cycle, i), insert ((i%cycle+shift)%cycle, i).
    let mk_key = |v: i64| -> [u8; 8] { (v.rem_euclid(keys_cycle as i64)).to_be_bytes() };
    let ops: Vec<(bool, [u8; 8], i64)> = {
        let mut v = Vec::with_capacity(touched as usize * 2);
        for i in 1..=touched as i64 {
            v.push((true, mk_key(i), i)); // delete old key
            v.push((false, mk_key(i + shift), i)); // insert new key
        }
        v
    };

    // F1: rowid order (today's interleaved executor order).
    let (build_ms, f1_ms) = time_on_tree("f1.idx", &mut |bt| {
        for (is_del, kb, rid) in &ops {
            if *is_del {
                let _ = bt.delete_index(kb, *rid);
            } else {
                bt.insert_index(kb, *rid).unwrap();
            }
        }
    });

    // F2: key-sorted order on an identical fresh tree — delete pass
    // then insert pass, each sorted by (key, rowid).
    let mut dels: Vec<(&[u8; 8], i64)> = Vec::with_capacity(touched as usize);
    let mut ins: Vec<(&[u8; 8], i64)> = Vec::with_capacity(touched as usize);
    for (is_del, kb, rid) in &ops {
        if *is_del {
            dels.push((kb, *rid));
        } else {
            ins.push((kb, *rid));
        }
    }
    dels.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
    ins.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
    let (_b2, f2_ms) = time_on_tree("f2.idx", &mut |bt| {
        for &(kb, rid) in &dels {
            let _ = bt.delete_index(kb, rid);
        }
        for &(kb, rid) in &ins {
            bt.insert_index(kb, rid).unwrap();
        }
    });

    println!("[F] raw per-op API on a {keys_cycle}-entry index ({build_ms:.0} ms bulk build):");
    println!(
        "    rowid-order (today):        {:>9.1} ms  ({:.2} us/op-pair)",
        f1_ms,
        f1_ms * 1000.0 / touched as f64
    );
    println!(
        "    key-sorted (batch feed):    {:>9.1} ms  ({:.2} us/op-pair)  -> {:.2}x",
        f2_ms,
        f2_ms * 1000.0 / touched as f64,
        f1_ms / f2_ms.max(1e-9)
    );
    println!(
        "    [sorted-order recovery: {:.0}% of the rowid-order cost is order-sensitive]",
        (1.0 - f2_ms / f1_ms.max(1e-9)) * 100.0
    );
}
