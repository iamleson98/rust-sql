//! Scale probe: DML (UPDATE/DELETE) whose WHERE drives the qualified-IN
//! IndexIn multi-seek — the shape of the warmer's bulk seat hold/release
//! and of `delete_unsold_seats_of_trips`. Correctness is pinned by
//! tests/qualified_in_plan.rs at 3 rows; this measures 200k-row scale.
//!
//! Run: cargo run --release --example dml_scale_probe

use std::time::Instant;
use rustqlite::{Database, Value};

fn main() {
    let mut db = Database::open_in_memory().unwrap();
    for sql in [
        "CREATE TABLE seat_inventory (\
             id TEXT PRIMARY KEY, \
             trip_session_id TEXT NOT NULL, \
             seat_id TEXT NOT NULL, \
             status TEXT NOT NULL, \
             final_price INTEGER NOT NULL)",
        // Prod-parity indexes.
        "CREATE INDEX seatinv_tsid_idx ON seat_inventory (trip_session_id)",
        "CREATE INDEX seatinv_seatid_idx ON seat_inventory (seat_id)",
    ] {
        db.execute(sql, []).unwrap();
    }

    // 400 trips x 500 seats = 200k rows.
    let n_trips = 400usize;
    let seats_per = 500usize;
    let ins = "INSERT INTO seat_inventory VALUES (?, ?, ?, 'available', 150000)";
    db.execute("BEGIN", []).unwrap();
    let mut t_no = 0u64;
    for t in 0..n_trips {
        let tsid = format!("trip-session-{t:04}");
        for s in 0..seats_per {
            t_no += 1;
            db.execute(
                ins,
                [
                    Value::Text(format!("seat-row-{t_no:07}").into()),
                    Value::Text(tsid.clone().into()),
                    Value::Text(format!("s{s:03}").into()),
                ],
            )
            .unwrap();
        }
    }
    db.execute("COMMIT", []).unwrap();
    println!("inserted {} rows", n_trips * seats_per);

    // ── DELETE: 500-chunk of trip ids, qualified (sea-orm shape) ──
    let chunk: Vec<Value> = (0..n_trips)
        .take(500 % n_trips)
        .map(|t| Value::Text(format!("trip-session-{t:04}").into()))
        .collect();
    let pharms: Vec<String> = (1..=chunk.len()).map(|i| format!("?{i}")).collect();
    let del_q = format!(
        "DELETE FROM \"seat_inventory\" \
         WHERE \"seat_inventory\".\"trip_session_id\" IN ({})",
        pharms.join(", ")
    );
    let t0 = Instant::now();
    db.execute(&del_q, chunk.clone()).unwrap();
    println!("DELETE qualified-IN ({} ids): {:?}", 500 % n_trips, t0.elapsed());

    // ── UPDATE: the seat-hold shape ──
    let upd_q = format!(
        "UPDATE \"seat_inventory\" SET \"status\" = 'held' \
         WHERE \"seat_inventory\".\"trip_session_id\" IN ({}) \
         AND \"seat_inventory\".\"status\" = 'available'",
        pharms.join(", ")
    );
    let t1 = Instant::now();
    db.execute(&upd_q, chunk.clone()).unwrap();
    println!("UPDATE qualified-IN: {:?}", t1.elapsed());

    // ── SELECT twin for comparison (the shape that IS fast) ──
    let sel_q = format!(
        "SELECT count(*) FROM \"seat_inventory\" \
         WHERE \"seat_inventory\".\"trip_session_id\" IN ({})",
        pharms.join(", ")
    );
    let t2 = Instant::now();
    let rows = db.query(&sel_q, chunk).unwrap();
    let n: i64 = rows[0][0].as_integer();
    println!("SELECT count twin: {:?} ({} rows)", t2.elapsed(), n);
}
