//! PROBE: where does the SQLite-format per-commit fixed cost go?
//!
//! Runs the probe_commit_scale shape (1-row autocommit INSERT/UPDATE,
//! WAL + synchronous=OFF) at one scale and prints the container's own
//! phase/reader instrumentation per commit, A/B against
//! `RSQL_NO_RAW_FAST=1`.
use rustqlite::storage::sqlitefmt::container::{
    commit_timer_snapshot, APPEND_FAST_NS, APPEND_SLOW_NS, CAN_SPLICE_NS, GUARD_PROBE_NS,
    PHASE_CHECKPOINT_NS, PHASE_COMMIT_NS, PHASE_MUTATE_NS, PHASE_SPLICE_NS, PHASE_TAIL_NS,
    PUBLISH_COUNT, PUBLISH_NS, READER_CACHE_HITS, READER_FILE_OPENS, READER_FILE_READS,
    READER_OVERLAY_HITS,
};
use rustqlite::storage::sqlitefmt::mutator::{
    MUT_DESC_READS, MUT_MODEL_LEAF, MUT_RAW_INDEX_FALLBACK, MUT_RAW_INDEX_OK,
    MUT_RAW_TABLE_FALLBACK, MUT_RAW_TABLE_OK, MUT_SPLITS, MUT_SRC_BYTES, MUT_SRC_CALLS,
    MUT_TABLE_LEAF_MODEL_MAP, MUT_TABLE_LEAF_RAW, MUT_TABLE_OP,
};
use rustqlite::{Database, Value};
use std::sync::atomic::Ordering::Relaxed;

fn build(path: &std::path::Path, n: i64) -> Database {
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
                Value::Integer(i.wrapping_mul(48271).rem_euclid(n)),
                Value::Text("p".repeat(i as usize % 40).to_string().into()),
            ],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    db
}

fn snapshot() -> [u64; 24] {
    let r = std::sync::atomic::Ordering::Relaxed;
    [
        CAN_SPLICE_NS.load(r),
        GUARD_PROBE_NS.load(r),
        APPEND_FAST_NS.load(r),
        APPEND_SLOW_NS.load(r),
        PUBLISH_NS.load(r),
        PUBLISH_COUNT.load(r),
        PHASE_MUTATE_NS.load(r),
        PHASE_SPLICE_NS.load(r),
        PHASE_TAIL_NS.load(r),
        PHASE_COMMIT_NS.load(r),
        PHASE_CHECKPOINT_NS.load(r),
        READER_FILE_OPENS.load(r),
        MUT_SRC_CALLS.load(r),
        MUT_SRC_BYTES.load(r),
        MUT_RAW_TABLE_OK.load(r),
        MUT_RAW_TABLE_FALLBACK.load(r),
        MUT_RAW_INDEX_OK.load(r),
        MUT_RAW_INDEX_FALLBACK.load(r),
        MUT_MODEL_LEAF.load(r),
        MUT_SPLITS.load(r),
        MUT_TABLE_OP.load(r),
        MUT_TABLE_LEAF_RAW.load(r),
        MUT_TABLE_LEAF_MODEL_MAP.load(r),
        MUT_DESC_READS.load(r),
    ]
}

fn main() {
    let n: i64 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(100_000);
    let m: usize = 300;
    let dir = tempfile::tempdir().unwrap();
    let mut db = build(&dir.path().join("e.db"), n);

    // Warm the page cache with one round.
    for i in n + 1..=n + 10 {
        db.execute(
            "INSERT INTO t(a, b, pad) VALUES (?, ?, ?)",
            [
                Value::Text(format!("a{i}").into()),
                Value::Integer(i.wrapping_mul(7919).rem_euclid(n)),
                Value::Text(format!("x{}", "q".repeat(i as usize % 40)).into()),
            ],
        )
        .unwrap();
    }
    let b = snapshot();
    let t0 = std::time::Instant::now();
    for i in 1..=m as i64 {
        db.execute(
            "INSERT INTO t(a, b, pad) VALUES (?, ?, ?)",
            [
                Value::Text(format!("n{i}").into()),
                Value::Integer(i.wrapping_mul(7919).rem_euclid(n)),
                Value::Text("z".repeat(i as usize % 40).to_string().into()),
            ],
        )
        .unwrap();
    }
    let ins_dur = t0.elapsed();
    let a = snapshot();
    let d: Vec<u64> = a.iter().zip(b.iter()).map(|(x, y)| x - y).collect();
    println!(
        "INSERT  total {:>7.1} us/commit  (wall over {m})",
        ins_dur.as_micros() as f64 / m as f64
    );
    println!(
        "  can_splice {:>7.1}  guard_probe {:>7.1}  append_fast {:>7.1}  append_slow {:>7.1}",
        d[0] as f64 / m as f64 / 1000.0,
        d[1] as f64 / m as f64 / 1000.0,
        d[2] as f64 / m as f64 / 1000.0,
        d[3] as f64 / m as f64 / 1000.0
    );
    println!(
        "  publish_ns {:>7.1} (count {})",
        d[4] as f64 / m as f64 / 1000.0,
        d[5] / m as u64
    );
    println!(
        "  phases: mutate {:>7.1}  splice {:>7.1}  tail {:>7.1}  commit {:>7.1}  ckpt {:>7.1}",
        d[6] as f64 / m as f64 / 1000.0,
        d[7] as f64 / m as f64 / 1000.0,
        d[8] as f64 / m as f64 / 1000.0,
        d[9] as f64 / m as f64 / 1000.0,
        d[10] as f64 / m as f64 / 1000.0
    );
    println!(
        "  unaccounted inside publish: {:.1}",
        d[4] as f64 / m as f64 / 1000.0
            - (d[6] as f64 + d[7] as f64 + d[8] as f64 + d[9] as f64 + d[10] as f64)
                / m as f64
                / 1000.0
    );
    println!("  reader file opens this round: {}", d[11]);
    println!("  mutator TOTALS: src_calls {} ({} KiB), desc_hits {}, tbl_op {}, leaf_raw {}, leaf_mdl {}",
        d[12], d[13] / 1024, d[23], d[20], d[21], d[22]);
    println!(
        "                  raw_ok tbl/idx {}/{}, raw_fb {}/{}, model_leaf {}, splits {}",
        d[14], d[16], d[15], d[17], d[18], d[19]
    );

    // UPDATE round.
    let b = snapshot();
    let t0 = std::time::Instant::now();
    for i in 1..=m as i64 {
        db.execute(
            "UPDATE t SET b = ? WHERE id = ?",
            [
                Value::Integer(i.wrapping_mul(104729).rem_euclid(n)),
                Value::Integer(i),
            ],
        )
        .unwrap();
    }
    let upd_dur = t0.elapsed();
    let a = snapshot();
    let d: Vec<u64> = a.iter().zip(b.iter()).map(|(x, y)| x - y).collect();
    println!(
        "UPDATE  total {:>7.1} us/commit  (wall over {m})",
        upd_dur.as_micros() as f64 / m as f64
    );
    println!(
        "  can_splice {:>7.1}  guard_probe {:>7.1}  append_fast {:>7.1}  append_slow {:>7.1}",
        d[0] as f64 / m as f64 / 1000.0,
        d[1] as f64 / m as f64 / 1000.0,
        d[2] as f64 / m as f64 / 1000.0,
        d[3] as f64 / m as f64 / 1000.0
    );
    println!(
        "  publish_ns {:>7.1} (count {})",
        d[4] as f64 / m as f64 / 1000.0,
        d[5] / m as u64
    );
    println!(
        "  phases: mutate {:>7.1}  splice {:>7.1}  tail {:>7.1}  commit {:>7.1}  ckpt {:>7.1}",
        d[6] as f64 / m as f64 / 1000.0,
        d[7] as f64 / m as f64 / 1000.0,
        d[8] as f64 / m as f64 / 1000.0,
        d[9] as f64 / m as f64 / 1000.0,
        d[10] as f64 / m as f64 / 1000.0
    );
    println!("  reader file opens this round: {}", d[11]);
    println!("  mutator TOTALS: src_calls {} ({} KiB), desc_hits {}, tbl_op {}, leaf_raw {}, leaf_mdl {}",
        d[12], d[13] / 1024, d[23], d[20], d[21], d[22]);
    println!(
        "                  raw_ok tbl/idx {}/{}, raw_fb {}/{}, model_leaf {}, splits {}",
        d[14], d[16], d[15], d[17], d[18], d[19]
    );
    let _ = commit_timer_snapshot();
    let _ = (
        READER_OVERLAY_HITS.load(Relaxed),
        READER_CACHE_HITS.load(Relaxed),
        READER_FILE_READS.load(Relaxed),
    );
}
