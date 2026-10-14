//! Blob-key probe: the app binds UUIDs as 16-byte BLOBs (sea-orm), and
//! prod-parity index seeks must drive the IndexIn multi-seek for BOTH
//! text and blob keys. This measures the warmer's existence query and
//! the search fan-out shape with TEXT vs BLOB keys side by side.
//!
//! Run: cargo run --release --example blob_in_probe

use rustqlite::{Database, Value};
use std::time::Instant;

fn mk_blob(i: u32) -> Value {
    // A 16-byte key, UUID-ish.
    let mut b = [0u8; 16];
    b[0] = (i >> 24) as u8;
    b[1] = (i >> 16) as u8;
    b[2] = (i >> 8) as u8;
    b[3] = i as u8;
    b[4] = 0xC0;
    b[5] = 0xFF;
    b[6] = 0xEE;
    b[15] = 0x42;
    Value::Blob(b.to_vec())
}

fn main() {
    let mut db = Database::open_in_memory().unwrap();
    for sql in [
        "CREATE TABLE trip_session (\
             id TEXT PRIMARY KEY, schedule_id BLOB NOT NULL, \
             departure_date TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'scheduled', \
             total_seats INTEGER NOT NULL DEFAULT 0, \
             available_seats INTEGER NOT NULL DEFAULT 0)",
        "CREATE INDEX ts_sched_idx ON trip_session (schedule_id)",
        "CREATE INDEX ts_date_idx ON trip_session (departure_date)",
        "CREATE UNIQUE INDEX ts_sched_date_uniq ON trip_session (schedule_id, departure_date)",
        "CREATE INDEX ts_date_status_idx ON trip_session (departure_date, status)",
    ] {
        db.execute(sql, []).unwrap();
    }

    // 8762 schedules × 13 dates ≈ 114k trips (prod scale).
    let n_sched = 8762u32;
    let dates: [&str; 13] = [
        "2026-10-15",
        "2026-10-16",
        "2026-10-17",
        "2026-10-18",
        "2026-10-19",
        "2026-10-20",
        "2026-10-21",
        "2026-10-22",
        "2026-10-23",
        "2026-10-24",
        "2026-10-25",
        "2026-10-26",
        "2026-10-27",
    ];
    db.execute("BEGIN", []).unwrap();
    let ins = "INSERT INTO trip_session VALUES (?, ?, ?, 'scheduled', 40, 40)";
    let mut n = 0;
    for s in 0..n_sched {
        for d in &dates {
            n += 1;
            db.execute(
                ins,
                [
                    Value::Text(format!("trip-{n:07}").into()),
                    mk_blob(s),
                    Value::Text((*d).into()),
                ],
            )
            .unwrap();
        }
    }
    db.execute("COMMIT", []).unwrap();
    println!("inserted {} trips", n);

    // 500-key IN chunk, qualified, blob keys — the warmer's existence query.
    for (label, use_blob) in [("TEXT", false), ("BLOB", true)] {
        let keys: Vec<Value> = (0..500u32)
            .map(|i| {
                if use_blob {
                    mk_blob(i)
                } else {
                    Value::Text(format!("{:08x}-c0ff-ee00-0000-000000000042", i).into())
                }
            })
            .collect();
        let ph: Vec<String> = (1..=keys.len()).map(|i| format!("?{i}")).collect();
        let sql = format!(
            "SELECT id FROM \"trip_session\" \
             WHERE \"trip_session\".\"schedule_id\" IN ({}) \
             AND \"trip_session\".\"departure_date\" = ?501",
            ph.join(", ")
        );
        let mut params = keys;
        params.push(Value::Text("2026-10-16".into()));
        let t0 = Instant::now();
        let rows = db.query(&sql, params).unwrap();
        println!(
            "{label} qualified-IN(500)+date existence query: {:?} ({} rows)",
            t0.elapsed(),
            rows.len()
        );
    }

    // Unqualified twin (the shape the previous session's bench used).
    let keys: Vec<Value> = (0..500u32).map(mk_blob).collect();
    let ph: Vec<String> = (1..=keys.len()).map(|i| format!("?{i}")).collect();
    let sql = format!(
        "SELECT id FROM trip_session WHERE schedule_id IN ({}) AND departure_date = ?501",
        ph.join(", ")
    );
    let mut params = keys;
    params.push(Value::Text("2026-10-16".into()));
    let t0 = Instant::now();
    let rows = db.query(&sql, params).unwrap();
    println!(
        "BLOB unqualified-IN(500)+date twin: {:?} ({} rows)",
        t0.elapsed(),
        rows.len()
    );
}
