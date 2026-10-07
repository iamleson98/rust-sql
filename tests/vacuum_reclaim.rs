//! VACUUM reclaim regression: a DENSE bulk load minus an INTERLEAVED
//! mass delete (`id % 10 < 4`) is the shape between the fast paths'
//! two existing guards — no leaf is empty (nothing detaches, the
//! freelist stays 0) and no leaf is hollow (every page keeps ~60% of
//! its rows, above the half-page line) — yet the file as a whole
//! wastes ~40% of its leaf bytes, and VACUUM's contract (like
//! SQLite's own row-level rebuild) is to re-flow that slack away.
//! Run 37597765597 (macOS, mega_scale_marathon M6) drew exactly this
//! shape: "freelist=0 pages (of 217) | VACUUM 2ms -> 888864 bytes
//! (reclaimed 0.0%)" — the page-level fast path kept every page whole
//! and reclaimed nothing. The waste budget in
//! `trees_eligible_for_page_copy` declines such files to the row-level
//! rebuild; both journal-mode shapes (the WAL in-place install and the
//! rollback-journal image copy) must reclaim.
use rustqlite::Database;

/// The marathon's exact row generator bits (tests/mega_scale.rs).
fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

fn db_bytes(path: &std::path::Path) -> u64 {
    let mut n = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    for suffix in ["-wal", "-journal"] {
        let s = format!("{}{suffix}", path.display());
        n += std::fs::metadata(&s).map(|m| m.len()).unwrap_or(0);
    }
    n
}

/// Build 20k rows densely (batched multi-VALUES INSERTs — append-filled
/// leaves), create the k index, mass-delete 40% interleaved, then refill
/// the tail leaves past the hollow line (the marathon's M4/M5 churn did
/// this on the failing draw). Result: every page keeps >= 50% of its
/// rows, no page empties, the freelist stays 0, yet the file wastes
/// ~35-40% of its leaf bytes — the fast-path trap shape.
fn dense_minus_interleaved_delete(path: &std::path::Path, wal: bool) {
    let mut db = Database::open(path).unwrap();
    if wal {
        let mode: String = db.query("PRAGMA journal_mode = WAL", []).unwrap()[0][0]
            .as_text()
            .to_string();
        assert_eq!(mode, "wal");
    } else {
        let mode: String = db.query("PRAGMA journal_mode", []).unwrap()[0][0]
            .as_text()
            .to_string();
        assert_ne!(mode, "wal", "the non-WAL shape must actually be non-WAL");
    }
    db.execute(
        "CREATE TABLE events (id INTEGER PRIMARY KEY, k INTEGER, note TEXT)",
        [],
    )
    .unwrap();
    db.execute("CREATE INDEX ix_events_k ON events (k)", [])
        .unwrap();
    // The marathon's exact row shape: k = id % 1_000_000 (sequential
    // at this scale — an append-shaped k index), note = 'n' + 8 hex
    // digits, batched multi-VALUES INSERTs of 5000 rows.
    let rows: u64 = 20_000;
    let batch: u64 = 5_000;
    let mut next: u64 = 0;
    while next < rows {
        let n = batch.min(rows - next);
        let mut sql = String::with_capacity(48 * n as usize + 64);
        sql.push_str("INSERT INTO events (id, k, note) VALUES ");
        for j in 0..n {
            if j > 0 {
                sql.push(',');
            }
            let i = next + j + 1;
            sql.push_str(&format!(
                "({}, {}, 'n{:08x}')",
                i,
                i % 1_000_000,
                (splitmix64(i) >> 20) & 0xffff_ffff
            ));
        }
        db.execute(&sql, []).unwrap();
        next += n;
    }
    // The marathon M6 statement (uncapped: cap == rows).
    db.execute(
        &format!("DELETE FROM events WHERE id <= {rows} AND id % 10 < 4"),
        [],
    )
    .unwrap();
    // REFILL THE TAILS: the delete leaves the trees' tail leaves under
    // the half-page HOLLOW line (a leaf at ~28% fill declines the fast
    // paths on the EXISTING guard — the marathon's macOS draw had no
    // such leaf: M4/M5 churn had refilled every tail above 50%, which
    // is exactly why its trap shape reached the waste-budget hole). A
    // post-delete suffix INSERT lands in the tail leaves and lifts
    // them past the hollow line, reproducing the macOS precondition:
    // every leaf >= 50% fill, no 0-cell leaf, freelist 0, and the
    // collected intra-page slack still ~40% of the leaf footprint.
    let refill: u64 = 600;
    let mut sql = String::with_capacity(48 * refill as usize + 64);
    sql.push_str("INSERT INTO events (id, k, note) VALUES ");
    for j in 0..refill {
        if j > 0 {
            sql.push(',');
        }
        let i = rows + j + 1;
        sql.push_str(&format!(
            "({}, {}, 'n{:08x}')",
            i,
            i % 1_000_000,
            (splitmix64(i) >> 20) & 0xffff_ffff
        ));
    }
    db.execute(&sql, []).unwrap();
    if wal {
        db.execute("PRAGMA wal_checkpoint(TRUNCATE)", []).unwrap();
    }
    // The trap's preconditions, asserted so a future shape change turns
    // this into a loud failure instead of a silent false-pass.
    let freelist: i64 = db.query("PRAGMA freelist_count", []).unwrap()[0][0].as_integer();
    assert_eq!(freelist, 0, "the trap shape keeps every page live");
    let count: i64 = db.query("SELECT COUNT(*) FROM events", []).unwrap()[0][0].as_integer();
    assert_eq!(count, 12_600, "survivors + tail refill");
    let sum: i64 = db.query("SELECT SUM(k) FROM events", []).unwrap()[0][0].as_integer();
    let k_probe_before: i64 = db
        .query("SELECT COUNT(*) FROM events WHERE k = 105", [])
        .unwrap()[0][0]
        .as_integer();
    assert!(k_probe_before > 0, "the k index probe must be non-empty");

    let pre = db_bytes(path);
    db.execute("VACUUM", []).unwrap();
    if wal {
        db.execute("PRAGMA wal_checkpoint(TRUNCATE)", []).unwrap();
    }
    let post = db_bytes(path);
    let reclaim = 100.0 * (1.0 - post as f64 / pre as f64);
    assert!(
        reclaim >= 25.0,
        "VACUUM reclaimed only {reclaim:.1}% (< 25%) on the \
         dense-minus-interleaved-delete shape ({pre} -> {post} bytes, \
         wal={wal})"
    );
    // Content and index survive the rebuild exactly.
    let count2: i64 = db.query("SELECT COUNT(*) FROM events", []).unwrap()[0][0].as_integer();
    assert_eq!(count2, count);
    let sum2: i64 = db.query("SELECT SUM(k) FROM events", []).unwrap()[0][0].as_integer();
    assert_eq!(sum2, sum);
    let k_probe_after: i64 = db
        .query("SELECT COUNT(*) FROM events WHERE k = 105", [])
        .unwrap()[0][0]
        .as_integer();
    assert_eq!(k_probe_after, k_probe_before);
    let freelist2: i64 = db.query("PRAGMA freelist_count", []).unwrap()[0][0].as_integer();
    assert_eq!(freelist2, 0, "freelist drained by the vacuum");
}

#[test]
fn vacuum_reclaims_interleaved_delete_slack_wal() {
    let dir = tempfile::tempdir().unwrap();
    dense_minus_interleaved_delete(&dir.path().join("vac_wal.db"), true);
}

#[test]
fn vacuum_reclaims_interleaved_delete_slack_rollback() {
    let dir = tempfile::tempdir().unwrap();
    dense_minus_interleaved_delete(&dir.path().join("vac_rb.db"), false);
}
