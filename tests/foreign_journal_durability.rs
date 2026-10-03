//! Hot-journal crash recovery for SQLite-format files: a rollback
//! journal left by a crashed REAL SQLite writer (or forged to the
//! same spec) is replayed at open, restoring the pre-transaction
//! state byte-exactly — the surviving half of the old foreign
//! durability suite. (The engine's own in-place SQLite-format COMMIT
//! machinery — WAL sidecar, rollback journals, checkpoints — is gone
//! with the live container: a SQLite-format file now loads pending
//! and its first write ADOPTS the path into the native container; see
//! tests/adopt_on_write.rs. What must keep working: OPENING a file
//! whose last writer crashed mid-commit.)

use std::path::{Path, PathBuf};

fn temp_path(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "rsql_fjd_{}_{}_{}",
        name,
        std::process::id(),
        line_id()
    ));
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let _ = std::fs::remove_file(format!("{}{}", p.display(), suffix));
    }
    p
}

/// Distinct path per test even after cleanups (a plain counter).
fn line_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static C: AtomicU64 = AtomicU64::new(0);
    C.fetch_add(1, Ordering::Relaxed)
}

fn cleanup(path: &Path) {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let _ = std::fs::remove_file(format!("{}{}", path.display(), suffix));
    }
}

fn journal_path(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_os_string();
    p.push("-journal");
    PathBuf::from(p)
}

fn count(db: &rustqlite::Database) -> i64 {
    match &db.query("SELECT COUNT(*) FROM t", []).unwrap()[0][0] {
        rustqlite::Value::Integer(n) => *n,
        other => panic!("non-integer count: {other:?}"),
    }
}

/// SQLite's documented journal page-record checksum (fileformat2.html
/// §3), reimplemented from the spec text independently of the engine.
fn spec_page_checksum(page: &[u8], nonce: u32) -> u32 {
    let mut cksum = nonce;
    let mut x = page.len() as i64 - 200;
    while x >= 0 {
        cksum = cksum.wrapping_add(page[x as usize] as u32);
        x -= 200;
    }
    cksum
}

/// Hand-build a SQLite rollback journal per the documented format (28-
/// byte big-endian header zero-padded to one 512-byte sector, then
/// `pgno | old-page | checksum` records).
fn craft_journal(path: &Path, old_pages: &[(u32, Vec<u8>)], initial_db_pages: u32) {
    let nonce = 0x5eed_1234u32;
    let mut j = vec![0u8; 512];
    j[0..8].copy_from_slice(&[0xd9, 0xd5, 0x05, 0xf9, 0x20, 0xa1, 0x63, 0xd7]);
    j[8..12].copy_from_slice(&(old_pages.len() as i32).to_be_bytes());
    j[12..16].copy_from_slice(&nonce.to_be_bytes());
    j[16..20].copy_from_slice(&initial_db_pages.to_be_bytes());
    j[20..24].copy_from_slice(&512u32.to_be_bytes());
    j[24..28].copy_from_slice(&(old_pages[0].1.len() as u32).to_be_bytes());
    for (pgno, page) in old_pages {
        j.extend_from_slice(&pgno.to_be_bytes());
        j.extend_from_slice(page);
        j.extend_from_slice(&spec_page_checksum(page, nonce).to_be_bytes());
    }
    std::fs::write(journal_path(path), &j).unwrap();
}

#[test]
fn hot_rollback_journal_restores_pre_transaction_state() {
    let path = temp_path("hotjournal");

    // State S1, written by REAL SQLite (DELETE journal mode, one file).
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE t(id INTEGER PRIMARY KEY);\n\
             INSERT INTO t VALUES (0);\n\
             INSERT INTO t SELECT i FROM (SELECT 1 AS i UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9 UNION ALL SELECT 10);",
        )
        .unwrap();
    }
    let s1 = std::fs::read(&path).unwrap();

    // State S2: one more committed transaction by real SQLite.
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "INSERT INTO t SELECT i FROM (SELECT 11 AS i UNION ALL SELECT 12 UNION ALL SELECT 13 UNION ALL SELECT 14 UNION ALL SELECT 15 UNION ALL SELECT 16 UNION ALL SELECT 17 UNION ALL SELECT 18 UNION ALL SELECT 19 UNION ALL SELECT 20);",
        )
        .unwrap();
    }
    let s2 = std::fs::read(&path).unwrap();
    assert_ne!(s1, s2, "the S2 transaction must have changed the file");

    // Simulate a crash MID-COMMIT of a third transaction: the journal
    // holds S2 pre-images of the changed pages, and the main file has
    // been partially updated (torn writes — a few pages already carry
    // garbage that is neither S2 nor S3).
    let ps = 4096usize;
    assert!(s2.len() % ps == 0 && s2.len() >= 2 * ps, "page geometry");
    let mut changed: Vec<(u32, Vec<u8>)> = Vec::new();
    for (i, (a, b)) in s1.chunks(ps).zip(s2.chunks(ps)).enumerate() {
        if a != b {
            changed.push(((i + 1) as u32, a.to_vec()));
        }
    }
    assert!(
        !changed.is_empty(),
        "the S2 transaction changed at least one existing page"
    );
    craft_journal(&path, &changed, (s1.len() / ps) as u32);
    // Torn application: smash two pages that the journal covers.
    {
        use std::io::{Seek, Write};
        let mut f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        for (pgno, _) in changed.iter().take(2) {
            f.seek(std::io::SeekFrom::Start((*pgno as u64 - 1) * ps as u64))
                .unwrap();
            f.write_all(&vec![0xA5u8; ps]).unwrap();
        }
    }

    // Reopen with THIS engine: the hot journal replays and the file
    // lands exactly on S1 — the S2 transaction was never committed
    // (its journal deletion never happened), so its rows are GONE, and
    // the torn garbage is repaired from the pre-images.
    let db = rustqlite::Database::open(&path).unwrap();
    assert_eq!(
        count(&db),
        11,
        "hot journal must roll the file back to the pre-transaction state"
    );
    drop(db);
    // ...and the repaired file is what real SQLite sees too.
    let conn = rusqlite::Connection::open(&path).unwrap();
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 11);
    let ic: String = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .unwrap();
    assert_eq!(ic, "ok");
    drop(conn);
    assert!(
        !journal_path(&path).exists(),
        "journal consumed by rollback"
    );
    cleanup(&path);
}
