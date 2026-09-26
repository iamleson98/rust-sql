//! Repro probe (KNOWN ENGINE CORNER, pre-existing, native-format too):
//! an INSERT OR REPLACE workload with ~2.5 KB blobs eventually asks a
//! leaf to split with no feasible byte-aware point
//! (`src/storage/btree.rs` byte_aware_mid) — reproducible on the NATIVE
//! container, i.e. independent of the SQLite-format splice work. Kept
//! as the minimal reproducer for the follow-up ledger.
//! Run: `cargo run --example probe_native_fuzz` → "NATIVE failed at
//! round 8: corruption: leaf page 24 cannot split".

use rustqlite::{Database, Value};

fn main() {
    let mut lcg = 0x1234_5678u64;
    let mut rnd = || {
        lcg = lcg
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (lcg >> 33) as i64
    };
    // Native file container.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("native.db");
    let mut db = Database::open(&path).unwrap();
    db.execute(
        "CREATE TABLE m(id INTEGER PRIMARY KEY, a TEXT, b INTEGER, blob BLOB)",
        [],
    )
    .unwrap();
    db.execute("CREATE INDEX ixm ON m(b)", []).unwrap();
    db.execute(
        "CREATE TABLE w(k TEXT PRIMARY KEY, v INT) WITHOUT ROWID",
        [],
    )
    .unwrap();
    let mut ok_until = 0;
    'outer: for round in 0..12 {
        for _ in 0..25 {
            let op = rnd().rem_euclid(4);
            let id = 1 + rnd().rem_euclid(300);
            match op {
                0 => {
                    let blob_len = (rnd().rem_euclid(2500)) as usize;
                    let blob: Vec<u8> = (0..blob_len).map(|i| (i % 251) as u8).collect();
                    let r = db.execute(
                        "INSERT OR REPLACE INTO m(id, a, b, blob) VALUES (?, ?, ?, ?)",
                        [
                            Value::Integer(id),
                            Value::Text(format!("s{id}-{round}").into()),
                            Value::Integer(rnd()),
                            Value::Blob(blob.into()),
                        ],
                    );
                    if let Err(e) = r {
                        println!("NATIVE failed at round {round}: {e}");
                        break 'outer;
                    }
                }
                1 => {
                    db.execute("DELETE FROM m WHERE id = ?", [Value::Integer(id)])
                        .unwrap();
                }
                2 => {
                    db.execute(
                        "INSERT OR REPLACE INTO w(k, v) VALUES (?, ?)",
                        [Value::Text(format!("k{id}").into()), Value::Integer(round)],
                    )
                    .unwrap();
                }
                _ => {
                    db.execute("UPDATE m SET b = b + 1 WHERE id < ?", [Value::Integer(id)])
                        .unwrap();
                }
            }
            ok_until += 1;
        }
    }
    println!("native container survived {ok_until} ops");
    drop(db);
}
