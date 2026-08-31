//! Malformed-database fuzzing — modeled on §4.2 of
//! https://www.sqlite.org/testing.html
//!
//! SQLite's malformed-DB tests "first build a well-formed database file,
//! then add corruption by changing one or more bytes in the file by some
//! means other than SQLite. Then SQLite is used to read the database ...
//! The malformed database tests verify that SQLite finds the file format
//! errors and reports them using the SQLITE_CORRUPT return code without
//! overflowing buffers, dereferencing NULL pointers, or performing other
//! unwholesome actions."
//!
//! rustqlite must satisfy the same contract: opening and fully reading a
//! corrupted file either succeeds (when the corruption hit unused bytes or
//! payload data) or returns a graceful `Err`. It must never panic, never
//! abort, never loop forever, and never return garbage that crashes a
//! subsequent write.
//!
//! The corruption sweep here is deterministic (seeded PRNG) and covers:
//!   - single/multi byte flips at random offsets (data AND structure)
//!   - targeted strikes at structural regions (header, page headers,
//!     cell-pointer arrays) — the "interesting" cases per the SQLite page
//!   - truncation at every page boundary
//!   - zero-filled tails, huge absurd page counts, garbage-only files
//!   - WAL-file corruption next to a valid DB (compound failure, §3.4)
//!
//! Run with:
//!     cargo test --test db_corrupt_fuzz
//!     RUSTQLITE_FUZZ_ITERS=5000 RUSTQLITE_FUZZ_SEED=7 cargo test --test db_corrupt_fuzz -- --nocapture

use rustqlite::{Database, Value};
use std::io::Write;

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407) | 1)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(2685821657736338717)
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.next_u64() % n as u64) as usize }
    }
}

fn env_iters(default: usize) -> usize {
    std::env::var("RUSTQLITE_FUZZ_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn env_seed() -> u64 {
    std::env::var("RUSTQLITE_FUZZ_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0xC0FF_EE00_0000_0042)
}

/// Build a well-formed database file with:
///   - a table with an INTEGER PRIMARY KEY (rowid btree)
///   - a text column and a blob column (full codec coverage)
///   - enough rows to force interior pages (multi-level btree)
///   - a secondary index (index btree corruption paths)
///   - a WITHOUT ROWID table? (if supported — best effort)
fn build_valid_db(path: &std::path::Path) -> (usize, usize) {
    let mut db = Database::open(path).expect("build fresh db");
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, s TEXT, r REAL, b BLOB)", [])
        .unwrap();
    db.execute("CREATE INDEX idx_s ON t(s)", []).unwrap();
    for i in 1..=2000i64 {
        db.execute(
            "INSERT INTO t (s, r, b) VALUES (?, ?, ?)",
            [
                Value::Text(format!("value-{:04}", i * 7).into()),
                Value::Real(i as f64 / 3.0),
                Value::Blob(vec![(i % 256) as u8; (i % 32) as usize]),
            ],
        )
        .unwrap();
    }
    db.flush().unwrap();
    let n_rows = db.query("SELECT COUNT(*) FROM t", []).unwrap().len();
    let committed = db
        .query("SELECT COUNT(*) FROM t", [])
        .unwrap()[0][0]
        .clone();
    let file_len = std::fs::metadata(path).unwrap().len() as usize;
    (file_len, if let Value::Integer(n) = committed { n as usize } else { n_rows })
}

/// Read every page of every table + index: a full structural walk. The
/// contract is Ok or Err — never a panic.
fn full_read(db: &mut Database) -> Result<(), String> {
    let tables = db.catalog_ref().all_tables();
    let indexes = db.catalog_ref().all_indexes();
    for (name, _t) in &tables {
        let sql = format!("SELECT * FROM {}", name);
        let rows = db
            .query(&sql, [])
            .map_err(|e| format!("full read of {} failed: {}", name, e))?;
        let _ = rows.len();
    }
    for (name, idx) in &indexes {
        // Force an index walk via a range-ish predicate.
        let sql = format!(
            "SELECT rowid FROM {} WHERE {} IS NOT NULL ORDER BY {}",
            idx.table, idx.columns[0].name, idx.columns[0].name
        );
        let _ = db.query(&sql, []);
        let _ = name;
    }
    // Also exercise count/aggregate paths over the primary key.
    for (name, _t) in &tables {
        let _ = db.query(&format!("SELECT COUNT(*), MIN(rowid), MAX(rowid) FROM {}", name), []);
    }
    Ok(())
}

/// After any corruption outcome, a surviving connection (or a fresh open)
/// must remain usable for WRITES: corruption must not poison the writer
/// into producing an invalid new file state on the next INSERT.
fn write_still_works_or_errors(path: &std::path::Path) {
    match Database::open(path) {
        Ok(mut db) => {
            // If the DB opened, a write either succeeds or fails gracefully.
            let _ = db.execute("INSERT INTO t (s, r, b) VALUES ('post-corruption', 1.0, X'00')", []);
            let _ = db.execute("DELETE FROM t WHERE s = 'post-corruption'", []);
            let _ = db.flush();
        }
        Err(_) => { /* graceful rejection is acceptable */ }
    }
}

fn corrupt_and_verify(path: &std::path::Path, original: &[u8], corrupted: &[u8], label: &str) {
    assert_ne!(Some(corrupted), Some(original), "{}: mutation was a no-op", label);
    std::fs::write(path, corrupted).unwrap();
    // The core contract: no panic, no abort, no infinite loop (test harness
    // imposes the time limit).
    if std::env::var("RUSTQLITE_CORRUPT_TRACE").is_ok() {
        eprintln!("[corrupt] {} open...", label);
    }
    let mut outcome = "error";
    if let Ok(mut db) = Database::open(path) {
        if std::env::var("RUSTQLITE_CORRUPT_TRACE").is_ok() {
            eprintln!("[corrupt] {} read1...", label);
        }
        if full_read(&mut db).is_ok() {
            outcome = "readable";
        } else {
            outcome = "corruption-detected";
        }
        if std::env::var("RUSTQLITE_CORRUPT_TRACE").is_ok() {
            eprintln!("[corrupt] {} read2...", label);
        }
        // Verify a second read pass gives a CONSISTENT answer (reads must
        // not mutate/repair state in a way that changes results mid-session).
        let second = full_read(&mut db);
        if std::env::var("RUSTQLITE_CORRUPT_TRACE").is_ok() {
            eprintln!("[corrupt] {} write...", label);
        }
        let first_ok = outcome == "readable";
        assert_eq!(
            second.is_ok(),
            first_ok,
            "{}: read outcome flipped between passes ({} -> {})",
            label,
            outcome,
            if second.is_ok() { "readable" } else { "corruption-detected" }
        );
    }
    write_still_works_or_errors(path);
    let _ = outcome;
}

#[test]
fn malformed_database_files_never_panic() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("corrupt.db");
    let (file_len, committed_rows) = build_valid_db(&db_path);
    let original = std::fs::read(&db_path).unwrap();
    assert!(file_len > 4096, "test DB suspiciously small: {} bytes", file_len);
    assert_eq!(committed_rows, 2000);

    let iters = env_iters(600);
    let mut rng = Rng::new(env_seed());

    for i in 0..iters {
        let mut bytes = original.clone();
        // 1–8 random byte strikes anywhere in the file.
        let strikes = 1 + rng.below(8);
        for _ in 0..strikes {
            let pos = rng.below(bytes.len());
            bytes[pos] = rng.next_u64() as u8;
        }
        corrupt_and_verify(&db_path, &original, &bytes, &format!("random-strike-{}", i));
    }

    // Restore the pristine file for the structural sweeps below.
    std::fs::write(&db_path, &original).unwrap();
}

/// Targeted structural corruption: the SQLite page says the "interesting
/// cases are when bytes of the file that define database structure get
/// changed". Strike each structural region of every page in turn.
#[test]
fn structural_corruption_never_panics() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("structural.db");
    let _ = build_valid_db(&db_path);
    let original = std::fs::read(&db_path).unwrap();

    // Read the page size from OUR file header: u32 LE at bytes 8..12
    // (see src/storage/page.rs FileHeader). The previous code read
    // SQLite's layout (u16 BE at 16..18) — which in our header is the
    // database-size-in-pages field, producing a bogus "page size" like
    // 4864 and misplacing every structural strike.
    assert!(original.len() >= 12, "db file too small to hold a header");
    let page_size = u32::from_le_bytes(original[8..12].try_into().unwrap()) as usize;
    assert!(
        page_size >= 512 && page_size.is_power_of_two(),
        "bad page size {}",
        page_size
    );

    let n_pages = original.len().div_ceil(page_size);
    for page in 0..n_pages {
        let base = page * page_size;
        // Structural regions of a b-tree page: the 8-byte header (type,
        // first freeblock, ncells, cell content offset, fragmented bytes,
        // right-most pointer) and the cell pointer array right after it.
        for off in [0usize, 1, 3, 5, 7, 8, 9, 10, 12] {
            if base + off >= original.len() {
                continue;
            }
            let mut bytes = original.clone();
            // Corrupt with several distinct values: 0x00 (zeroed), 0xFF
            // (max), and a random-ish pattern each catch different classes
            // of missing validation. Skip strikes that would be no-ops
            // (the byte already holds that value) — e.g. striking 0x00 on
            // page 0's page-size field, whose low byte is already 0x00.
            for &v in [0x00u8, 0xFF, 0x7F].iter() {
                if original[base + off] == v {
                    continue;
                }
                bytes[base + off] = v;
                corrupt_and_verify(&db_path, &original, &bytes, &format!("page{} hdr+{} = {:#x}", page, off, v));
            }
        }
    }

    // File-header strikes: page-size field, page-count, schema cookie,
    // text encoding, format versions. Each must be caught or tolerated.
    for (off, name) in [
        (16usize, "page-size"),
        (18, "reserved-bytes"),
        (19, "payload-fract"),
        (20, "payload-max"),
        (21, "payload-min"),
        (28, "db-size-pages"),
        (32, "schema-cookie"),
        (36, "schema-format"),
        (44, "text-encoding"),
        (56, "format-write-version"),
    ] {
        if off + 4 <= original.len() {
            // Skip strikes that would be no-ops — e.g. zeroing reserved
            // bytes that are already zero (reserved-bytes on a fresh file).
            if original[off..off + 4] != [0xFF, 0xFF, 0xFF, 0xFF] {
                let mut bytes = original.clone();
                bytes[off..off + 4].copy_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);
                corrupt_and_verify(&db_path, &original, &bytes, &format!("file-header {} = 0xFFFFFFFF", name));
            }
            if original[off..off + 4] != [0x00, 0x00, 0x00, 0x00] {
                let mut bytes = original.clone();
                bytes[off..off + 4].copy_from_slice(&[0x00, 0x00, 0x00, 0x00]);
                corrupt_and_verify(&db_path, &original, &bytes, &format!("file-header {} = 0", name));
            }
        }
    }

    std::fs::write(&db_path, &original).unwrap();
}

/// Truncation, zero-fill, and garbage files.
#[test]
fn truncated_and_garbage_files_fail_gracefully() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("trunc.db");
    let _ = build_valid_db(&db_path);
    let original = std::fs::read(&db_path).unwrap();
    // Our header: page_size is u32 LE at bytes 8..12 (see FileHeader).
    let page_size = u32::from_le_bytes(original[8..12].try_into().unwrap()) as usize;
    assert!(page_size >= 512 && page_size.is_power_of_two(), "bad page size {}", page_size);

    // Truncate at every page boundary (and a few mid-page points). The
    // cut must be strictly shorter than the file — truncating to the full
    // length is a no-op mutation.
    for cut in (page_size..original.len()).step_by(page_size) {
        let bytes = original[..cut].to_vec();
        corrupt_and_verify(&db_path, &original, &bytes, &format!("truncate-{}", cut));
    }
    // A few mid-page cuts too.
    for cut in [page_size + page_size / 2, original.len() - page_size / 2] {
        if cut < original.len() {
            let bytes = original[..cut].to_vec();
            corrupt_and_verify(&db_path, &original, &bytes, &format!("truncate-mid-{}", cut));
        }
    }

    // Zero out the tail from various points.
    for i in 1..6 {
        let cut = original.len() - i * page_size;
        let mut bytes = original.clone();
        bytes[cut..].fill(0);
        corrupt_and_verify(&db_path, &original, &bytes, &format!("zero-tail-{}", cut));
    }

    // Pure garbage files of assorted shapes.
    let garbage_files: Vec<(String, Vec<u8>)> = vec![
        ("empty".into(), Vec::new()),
        ("one-byte".into(), vec![0x53]),
        // Our engine's magic is "RSQLDB01" (page.rs DB_MAGIC) — files that
        // carry a magic but nothing else exercise the header-validation
        // path; a foreign magic (SQLite's) exercises the magic check.
        ("sqlite-magic-only".into(), b"SQLite format 3\0".to_vec()),
        ("our-magic-only".into(), b"RSQLDB01".to_vec()),
        (
            "header-then-garbage".into(),
            [b"RSQLDB01".as_slice(), [0xAA; 512].as_slice()].concat(),
        ),
        ("all-zeros".into(), vec![0u8; 8192]),
        ("all-ff".into(), vec![0xFF; 8192]),
        ("random".into(), (0..8192).map(|i| (i * 31 + 7) as u8).collect()),
        (
            "huge-page-count".into(),
            {
                let mut b = original.clone();
                // Claim 0x7FFFFFFF pages in OUR header field (bytes
                // 16..20, u32 LE — see FileHeader).
                b[16..20].copy_from_slice(&0x7FFF_FFFFu32.to_le_bytes());
                b
            },
        ),
        (
            "huge-page-count-be".into(),
            {
                // Also strike the field with a BE-encoded value: catches
                // any parser that accidentally reads big-endian.
                let mut b = original.clone();
                b[16..20].copy_from_slice(&0x7FFF_FFFFu32.to_be_bytes());
                b
            },
        ),
    ];
    for (name, bytes) in garbage_files {
        corrupt_and_verify(&db_path, &original, &bytes, &format!("garbage-{}", name));
    }

    // Non-UTF8 blob content corrupted mid-payload: must stay readable or
    // error — but never panic (payload corruption changes DATA, not layout).
    {
        let mut bytes = original.clone();
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xFF;
        corrupt_and_verify(&db_path, &original, &bytes, "payload-mid");
    }

    std::fs::write(&db_path, &original).unwrap();
}

/// Compound failure (§3.4): corrupt the WAL file next to an otherwise
/// valid database. The engine must reject bad frames by checksum and must
/// never apply a torn frame.
#[test]
fn corrupted_wal_is_rejected_or_ignored() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("wal.db");
    let wal_path = tmp.path().join("wal.db-wal");

    // Build a DB, then enable WAL-ish mode via PRAGMA and write enough to
    // leave multiple frames in the WAL.
    let mut db = Database::open(&db_path).unwrap();
    db.execute("PRAGMA journal_mode = WAL", []).unwrap();
    db.execute("CREATE TABLE w (id INTEGER PRIMARY KEY, v TEXT)", []).unwrap();
    for i in 1..=500i64 {
        db.execute("INSERT INTO w (v) VALUES (?)", [Value::Text(format!("row{}", i).into())]).unwrap();
    }
    db.flush().unwrap();
    drop(db);

    let wal = std::fs::read(&wal_path).unwrap_or_default();
    if wal.is_empty() {
        // WAL checkpointed already or mode not persisted — the compound
        // scenario can't be constructed; verify the plain DB still opens.
        let db = Database::open(&db_path).expect("db must open without wal");
        assert_eq!(db.query("SELECT COUNT(*) FROM w", []).unwrap().len(), 1);
        return;
    }

    // Corrupt each 4 KiB-aligned chunk of the WAL in turn.
    let mut rng = Rng::new(env_seed() ^ 0xFEED);
    let n_chunks = wal.len().div_ceil(1024).max(1);
    for chunk in 0..n_chunks {
        let mut bytes = wal.clone();
        let base = chunk * 1024;
        let end = (base + 1024).min(bytes.len());
        for b in &mut bytes[base..end] {
            *b = rng.next_u64() as u8;
        }
        std::fs::write(&wal_path, &bytes).unwrap();
        // Contract: open + read must not panic; committed data before the
        // WAL is either visible (WAL discarded) or the WAL content that
        // validates is visible — never garbage.
        if let Ok(db2) = Database::open(&db_path) {
            let rows = db2.query("SELECT COUNT(*) FROM w", []);
            if let Ok(rows) = rows {
                if let Some(Value::Integer(n)) = rows.first().and_then(|r| r.first()) {
                    // 500 rows committed pre-WAL-corruption... any count in
                    // [0, 500] is a defensible recovery state; >500 or <0
                    // would mean garbage was applied.
                    assert!(
                        (0..=500).contains(n),
                        "corrupt WAL applied garbage: COUNT(*)={} (chunk {})",
                        n,
                        chunk
                    );
                }
            }
        }
    }

    // Restore an all-zero WAL: must be treated as empty (checksum reject).
    std::fs::write(&wal_path, vec![0u8; wal.len()]).unwrap();
    if let Ok(db3) = Database::open(&db_path) {
        let _ = db3.query("SELECT COUNT(*) FROM w", []);
    }
}

/// A corrupt file must never crash the CLI-style batch path either: run a
/// representative workload (DDL + DML + SELECT) against progressively more
/// damaged files.
#[test]
fn corrupt_file_survives_full_workload() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("workload.db");
    let _ = build_valid_db(&db_path);
    let original = std::fs::read(&db_path).unwrap();

    let mut rng = Rng::new(env_seed() ^ 0x9999);
    for round in 0..40 {
        let mut bytes = original.clone();
        // Progressively heavier corruption: round r hits r+1 spots.
        for _ in 0..=round {
            let pos = rng.below(bytes.len());
            bytes[pos] = rng.next_u64() as u8;
        }
        std::fs::write(&db_path, &bytes).unwrap();
        if let Ok(mut db) = Database::open(&db_path) {
            // Every one of these must be Ok or Err — never a panic.
            let _ = db.execute("CREATE TABLE IF NOT EXISTS z (x INTEGER)", []);
            let _ = db.execute("INSERT INTO z VALUES (1)", []);
            let _ = db.execute("UPDATE z SET x = 2 WHERE x = 1", []);
            let _ = db.query("SELECT * FROM t ORDER BY id LIMIT 100", []);
            let _ = db.query("SELECT s, COUNT(*) FROM t GROUP BY s HAVING COUNT(*) > 0 LIMIT 10", []);
            let _ = db.query("SELECT * FROM t a JOIN t b ON a.id = b.id - 1 LIMIT 10", []);
            let _ = db.execute("DELETE FROM z WHERE x = 2", []);
            let _ = db.flush();
        }
    }
    std::fs::write(&db_path, &original).unwrap();
    // The pristine restore must open and read perfectly again.
    let db = Database::open(&db_path).expect("restored pristine file must open");
    let count = db.query("SELECT COUNT(*) FROM t", []).unwrap();
    assert!(matches!(count.first().and_then(|r| r.first()), Some(Value::Integer(n)) if *n == 2000));
    let _ = std::io::stdout().flush();
}
