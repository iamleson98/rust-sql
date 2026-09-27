//! PROBE: real SQLite's WAL frame discipline per autocommit commit —
//! what pages enter the sidecar, whether page 1 rides every commit,
//! what the change counter / in-header db size do in WAL mode, and what
//! the checkpointed main file's header ends up carrying. Our engine runs
//! the identical scenario so the diff is side by side.
//!
//! Run: `cargo run --release --example probe_wal_discipline`
use rusqlite::params;
use std::path::Path;

fn parse_wal(wal: &[u8]) -> Vec<(usize, Vec<u32>, u32)> {
    // Returns (commit_idx, pgnos, commit_db_size) — one entry per commit.
    if wal.len() < 32 {
        return Vec::new();
    }
    let ps = u32::from_be_bytes([wal[8], wal[9], wal[10], wal[11]]) as usize;
    let fsz = 24 + ps;
    let mut out = Vec::new();
    let mut pgnos: Vec<u32> = Vec::new();
    let mut off = 32usize;
    while off + fsz <= wal.len() {
        let pgno = u32::from_be_bytes([wal[off], wal[off + 1], wal[off + 2], wal[off + 3]]);
        let dbsz = u32::from_be_bytes([wal[off + 4], wal[off + 5], wal[off + 6], wal[off + 7]]);
        pgnos.push(pgno);
        if dbsz != 0 {
            out.push((out.len(), std::mem::take(&mut pgnos), dbsz));
        }
        off += fsz;
    }
    out
}

fn hdr(path: &Path) -> String {
    let d = std::fs::read(path).unwrap();
    let u32be = |o: usize| u32::from_be_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]]);
    format!(
        "counter={} size={} free_head={} free_n={} ver_valid={}",
        u32be(24),
        u32be(28),
        u32be(32),
        u32be(36),
        u32be(92)
    )
}

fn clean(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    for s in ["-wal", "-shm", "-journal"] {
        let _ = std::fs::remove_file(format!("{}{s}", path.display()));
    }
}

fn sqlite_round(dir: &Path) {
    let dbp = dir.join("s.db");
    clean(&dbp);
    let c = rusqlite::Connection::open(&dbp).unwrap();
    c.pragma_update(None, "journal_mode", "WAL").unwrap();
    c.pragma_update(None, "synchronous", "OFF").unwrap();
    c.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b INT, pad TEXT)",
        [],
    )
    .unwrap();
    c.execute("CREATE INDEX ib ON t(b)", []).unwrap();
    c.execute_batch("BEGIN").unwrap();
    for i in 1..=2000i64 {
        c.execute(
            "INSERT INTO t(a, b, pad) VALUES (?1, ?2, ?3)",
            params![format!("a{i}"), i, format!("p{i}")],
        )
        .unwrap();
    }
    c.execute_batch("COMMIT").unwrap();
    // Checkpoint the seed so the sidecar starts empty.
    c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
    println!("[sqlite] main hdr after seed: {}", hdr(&dbp));
    let wl = rusqlite_wal_len(&dbp);
    println!("[sqlite] wal len after checkpoint: {wl}");

    for i in 1..=3i64 {
        c.execute(
            "INSERT INTO t(a, b, pad) VALUES (?1, ?2, ?3)",
            params![format!("n{i}"), 10000 + i, format!("q{i}")],
        )
        .unwrap();
        let w = std::fs::read(format!("{}-wal", dbp.display())).unwrap();
        let commits = parse_wal(&w);
        let last = commits.last().unwrap();
        println!(
            "[sqlite] INSERT #{i} commit: {} frames, pages {:?}, db_size {} (page1 in set: {})",
            last.1.len(),
            last.1,
            last.2,
            last.1.contains(&1)
        );
    }
    for i in 1..=3i64 {
        c.execute("UPDATE t SET b = ?1 WHERE id = ?2", params![9000 + i, i])
            .unwrap();
        let w = std::fs::read(format!("{}-wal", dbp.display())).unwrap();
        let commits = parse_wal(&w);
        let last = commits.last().unwrap();
        println!(
            "[sqlite] UPDATE #{i} commit: {} frames, pages {:?}, db_size {} (page1 in set: {})",
            last.1.len(),
            last.1,
            last.2,
            last.1.contains(&1)
        );
    }
    // A growth commit (splits: new tail pages).
    c.execute_batch("BEGIN").unwrap();
    for i in 1..=500i64 {
        c.execute(
            "INSERT INTO t(a, b, pad) VALUES (?1, ?2, ?3)",
            params![format!("g{i}"), 20000 + i, format!("r{i}")],
        )
        .unwrap();
    }
    c.execute_batch("COMMIT").unwrap();
    {
        let w = std::fs::read(format!("{}-wal", dbp.display())).unwrap();
        let commits = parse_wal(&w);
        let last = commits.last().unwrap();
        println!(
            "[sqlite] 500-row growth txn: {} frames, db_size {} (page1 in set: {})",
            last.1.len(),
            last.2,
            last.1.contains(&1)
        );
    }
    println!("[sqlite] main hdr before close: {}", hdr(&dbp));
    drop(c);
    println!("[sqlite] main hdr after close (checkpoint): {}", hdr(&dbp));
    clean(&dbp);
}

fn rusqlite_wal_len(p: &Path) -> u64 {
    std::fs::metadata(format!("{}-wal", p.display()))
        .map(|m| m.len())
        .unwrap_or(0)
}

fn engine_round(dir: &Path) {
    use rustqlite::{Database, Value};
    let dbp = dir.join("e.db");
    clean(&dbp);
    let mut db = Database::open_sqlite_format(&dbp).unwrap();
    db.execute("PRAGMA journal_mode = WAL", []).unwrap();
    db.execute("PRAGMA synchronous = OFF", []).unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b INT, pad TEXT)",
        [],
    )
    .unwrap();
    db.execute("CREATE INDEX ib ON t(b)", []).unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=2000i64 {
        db.execute(
            "INSERT INTO t(a, b, pad) VALUES (?, ?, ?)",
            [
                Value::Text(format!("a{i}").into()),
                Value::Integer(i),
                Value::Text(format!("p{i}").into()),
            ],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    db.execute("PRAGMA wal_checkpoint(TRUNCATE)", []).unwrap();
    println!("[engine] main hdr after seed: {}", hdr(&dbp));
    let wl = rusqlite_wal_len(&dbp);
    println!("[engine] wal len after checkpoint: {wl}");

    for i in 1..=3i64 {
        db.execute(
            "INSERT INTO t(a, b, pad) VALUES (?, ?, ?)",
            [
                Value::Text(format!("n{i}").into()),
                Value::Integer(10000 + i),
                Value::Text(format!("q{i}").into()),
            ],
        )
        .unwrap();
        let w = std::fs::read(format!("{}-wal", dbp.display())).unwrap();
        let commits = parse_wal(&w);
        let last = commits.last().unwrap();
        println!(
            "[engine] INSERT #{i} commit: {} frames, pages {:?}, db_size {} (page1 in set: {})",
            last.1.len(),
            last.1,
            last.2,
            last.1.contains(&1)
        );
    }
    for i in 1..=3i64 {
        db.execute(
            "UPDATE t SET b = ? WHERE id = ?",
            [Value::Integer(9000 + i), Value::Integer(i)],
        )
        .unwrap();
        let w = std::fs::read(format!("{}-wal", dbp.display())).unwrap();
        let commits = parse_wal(&w);
        let last = commits.last().unwrap();
        println!(
            "[engine] UPDATE #{i} commit: {} frames, pages {:?}, db_size {} (page1 in set: {})",
            last.1.len(),
            last.1,
            last.2,
            last.1.contains(&1)
        );
    }
    db.execute("BEGIN", []).unwrap();
    for i in 1..=500i64 {
        db.execute(
            "INSERT INTO t(a, b, pad) VALUES (?, ?, ?)",
            [
                Value::Text(format!("g{i}").into()),
                Value::Integer(20000 + i),
                Value::Text(format!("r{i}").into()),
            ],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    {
        let w = std::fs::read(format!("{}-wal", dbp.display())).unwrap();
        let commits = parse_wal(&w);
        let last = commits.last().unwrap();
        println!(
            "[engine] 500-row growth txn: {} frames, db_size {} (page1 in set: {})",
            last.1.len(),
            last.2,
            last.1.contains(&1)
        );
    }
    println!("[engine] main hdr before close: {}", hdr(&dbp));
    drop(db);
    println!("[engine] main hdr after close (checkpoint): {}", hdr(&dbp));
    // Real SQLite must accept the file.
    let c = rusqlite::Connection::open(&dbp).unwrap();
    let n: i64 = c
        .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
        .unwrap();
    let ic: String = c
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .unwrap();
    println!("[engine] real-SQLite reopen: rows={n} integrity={ic}");
    drop(c);
    clean(&dbp);
}

fn main() {
    let dir = tempfile::tempdir().unwrap();
    sqlite_round(dir.path());
    println!("--------------------------------------------------");
    engine_round(dir.path());
}
