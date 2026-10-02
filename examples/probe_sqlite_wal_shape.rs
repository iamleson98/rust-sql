//! EXPERIMENT: does real SQLite's WAL-mode growth commit carry a
//! page-1 frame? (Our splicer writes one — "the size-field refresh".)
use rusqlite::Connection;

fn main() {
    let path = "/tmp/sqlite_wal_probe.db";
    for ext in ["", "-wal", "-shm", "-journal"] {
        let _ = std::fs::remove_file(format!("{path}{ext}"));
    }
    let mut con = Connection::open(path).unwrap();
    con.pragma_update(None, "journal_mode", "WAL").unwrap();
    con.pragma_update(None, "synchronous", "OFF").unwrap();
    con.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b INTEGER, pad TEXT)",
        [],
    )
    .unwrap();
    con.execute("CREATE INDEX ib ON t(b)", []).unwrap();
    // Bulk: grow the file inside one txn.
    let tx = con.transaction().unwrap();
    for i in 1..=5000i64 {
        tx.execute(
            "INSERT INTO t(a, b, pad) VALUES (?1, ?2, ?3)",
            rusqlite::params![
                format!("a{i}"),
                i * 7919 % 5000,
                "p".repeat(i as usize % 40)
            ],
        )
        .unwrap();
    }
    tx.commit().unwrap();
    // ONE autocommit insert that GROWS the file.
    con.execute("INSERT INTO t(a, b, pad) VALUES ('grow', 1, 'x')", [])
        .unwrap();
    // Dump the WAL frames WITHOUT checkpointing: open a second reader?
    // Simpler: parse the sidecar bytes directly.
    let wal = std::fs::read(format!("{path}-wal")).unwrap_or_default();
    let ps = 4096usize;
    let fs = 24 + ps;
    let n = wal.len() / fs;
    println!("wal bytes {}, frames {}", wal.len(), n);
    let mut commits = 0usize;
    let mut page1_frames = 0usize;
    let mut growth_frames: Vec<(usize, u32, u32)> = Vec::new(); // (frame idx, pgno, db_size)
    for i in 0..n {
        let off = 32 + i * fs;
        if off + 24 > wal.len() {
            break;
        }
        let pgno = u32::from_be_bytes([wal[off], wal[off + 1], wal[off + 2], wal[off + 3]]);
        let dbsz = u32::from_be_bytes([wal[off + 4], wal[off + 5], wal[off + 6], wal[off + 7]]);
        if dbsz != 0 {
            commits += 1;
        }
        if pgno == 1 {
            page1_frames += 1;
        }
        growth_frames.push((i, pgno, dbsz));
    }
    println!("commits: {commits}, page-1 frames: {page1_frames}");
    // Show the tail: the last 12 frames (the growth commit).
    let tail = growth_frames.len().saturating_sub(12);
    for (i, pgno, dbsz) in &growth_frames[tail..] {
        println!("frame {i}: page {pgno}, commit dbsize={dbsz}");
    }
    // FULL page-1 map: every page-1 frame + every commit marker, and
    // whether the dbsize MOVED at that commit (growth without page 1).
    println!(
        "--- page-1 frames at: {:?}",
        growth_frames
            .iter()
            .filter(|f| f.1 == 1)
            .map(|f| f.0)
            .collect::<Vec<_>>()
    );
    for (i, pgno, dbsz) in &growth_frames {
        if *dbsz != 0 {
            println!("commit at frame {i} (last page {pgno}): dbsize={dbsz}");
        }
    }
    drop(con);
}
