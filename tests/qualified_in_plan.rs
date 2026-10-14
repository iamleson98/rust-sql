//! Table-qualified IN-lists drive the index multi-seek plans.
//!
//! sea-orm / sqlx / every ORM emits table-qualified columns
//! (`"trip_session"."schedule_id" IN (?, ?, ...)`). The IN-list
//! access-path selection (`try_index_in` / `try_rowid_in`) used to match
//! only UNQUALIFIED references, so every ORM fan-out query planned as a
//! scan + per-row linear membership walk: measured on a real workload,
//! 117k rows x 1,871 members ~ 30 s and multi-GiB of transient
//! allocations per statement (iamleson98/rust-be-template#75). These
//! tests pin the fix: qualified references that resolve to the scanned
//! table (by name, or by FROM alias) drive `IndexIn` / `RowidIn` just
//! like unqualified ones — and results stay row-for-row identical.

use rustqlite::{Database, Value};

fn plan(db: &Database, sql: &str) -> Vec<String> {
    db.query(&format!("EXPLAIN QUERY PLAN {sql}"), [])
        .unwrap()
        .into_iter()
        .map(|r| match &r[3] {
            Value::Text(t) => t.to_string(),
            v => panic!("detail column: {v:?}"),
        })
        .collect()
}

fn one_col(db: &Database, sql: &str) -> Vec<String> {
    db.query(sql, [])
        .unwrap()
        .into_iter()
        .map(|r| match &r[0] {
            Value::Text(t) => t.to_string(),
            Value::Integer(i) => i.to_string(),
            Value::Real(f) => format!("{f}"),
            v => panic!("value column: {v:?}"),
        })
        .collect()
}

fn setup() -> Database {
    let mut db = Database::open_in_memory().unwrap();
    for sql in [
        "CREATE TABLE trip_session (\
             id TEXT PRIMARY KEY, schedule_id TEXT NOT NULL, \
             departure_date TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'scheduled', \
             available_seats INTEGER NOT NULL DEFAULT 0)",
        "CREATE INDEX TripSession_scheduleId_idx ON trip_session (schedule_id)",
        "CREATE UNIQUE INDEX TripSession_schedule_date_uniq \
             ON trip_session (schedule_id, departure_date)",
        "CREATE INDEX TripSession_date_status_idx ON trip_session (departure_date, status)",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT, n INTEGER)",
        "CREATE INDEX tv ON t (v)",
    ] {
        db.execute(sql, []).unwrap();
    }
    for i in 0..56 {
        // (schedule_id, departure_date) unique: schedule s{i%7} gets its
        // own date per occurrence — 8 dates x 7 schedules = 56 rows.
        db.execute(
            &format!(
                "INSERT INTO trip_session VALUES ('t{:03}', 's{}', '2026-10-{:02}', 'scheduled', {})",
                i,
                i % 7,
                16 + i / 7,
                i % 5
            ),
            [],
        )
        .unwrap();
        db.execute(
            &format!("INSERT INTO t VALUES ({i}, 'v{}', {i})", i % 4),
            [],
        )
        .unwrap();
    }
    db
}

#[test]
fn qualified_in_drives_index_in() {
    let db = setup();
    // The sea-orm shape: every column qualified with the table name.
    let sql = "SELECT \"trip_session\".\"id\" FROM \"trip_session\" \
               WHERE \"trip_session\".\"schedule_id\" IN ('s0', 's1', 's2', 's3') \
               AND \"trip_session\".\"departure_date\" = '2026-10-16' \
               AND \"trip_session\".\"available_seats\" >= 1 \
               AND \"trip_session\".\"status\" = 'scheduled' \
               LIMIT 2000";
    let p = plan(&db, sql);
    assert!(
        p.iter()
            .any(|s| s.contains("USING INDEX TripSession_scheduleId_idx (4 IN values)")),
        "qualified IN must drive IndexIn, got {p:?}"
    );
    // Results identical to the unqualified form.
    let unq = one_col(
        &db,
        "SELECT id FROM trip_session WHERE schedule_id IN ('s0', 's1', 's2', 's3') \
         AND departure_date = '2026-10-16' AND available_seats >= 1 \
         AND status = 'scheduled' LIMIT 2000",
    );
    let q = one_col(&db, sql);
    assert_eq!(unq, q);
    // date 2026-10-16 = i in 0..7: schedules s0..s6 once each; seats =
    // i%5 excludes i=0 (s0). s1..s3 survive: i = 1, 2, 3.
    assert_eq!(
        unq,
        vec!["t001".to_string(), "t002".to_string(), "t003".to_string()]
    );
}

#[test]
fn qualified_in_with_from_alias_drives_index_in() {
    let db = setup();
    // Aliased FROM: the qualifier must be the alias.
    let sql = "SELECT ts.id FROM trip_session AS ts \
               WHERE ts.schedule_id IN ('s0', 's5') AND ts.status = 'scheduled'";
    let p = plan(&db, sql);
    assert!(
        p.iter()
            .any(|s| s.contains("USING INDEX TripSession_scheduleId_idx (2 IN values)")),
        "alias-qualified IN must drive IndexIn, got {p:?}"
    );
    // A table-name qualifier under an alias does not even resolve —
    // SQLite rejects `trip_session.x` when the FROM item is aliased to
    // `ts`, and so does the engine's name resolution.
    let wrong = "SELECT ts.id FROM trip_session AS ts \
                 WHERE \"trip_session\".\"schedule_id\" IN ('s0', 's5')";
    assert!(
        db.query(wrong, []).is_err(),
        "mismatched qualifier must not resolve"
    );
}

#[test]
fn qualified_rowid_in_drives_rowid_in() {
    let db = setup();
    let sql = "SELECT \"t\".\"n\" FROM \"t\" WHERE \"t\".\"id\" IN (3, 9, 40)";
    let p = plan(&db, sql);
    assert!(
        p.iter()
            .any(|s| s.contains("INTEGER PRIMARY KEY (rowid IN 3 values)")),
        "qualified rowid IN must drive the multi-seek, got {p:?}"
    );
    assert_eq!(one_col(&db, sql), vec!["3", "9", "40"]);
    // rowid pseudo-column qualified.
    assert_eq!(
        one_col(&db, "SELECT n FROM t WHERE \"t\".\"rowid\" IN (1, 2)"),
        vec!["1", "2"]
    );
}

#[test]
fn qualified_in_with_parameters() {
    let db = setup();
    // Bound parameters — exactly what sqlx binds for sea-orm.
    let rows: Vec<String> = db
        .query(
            "SELECT \"trip_session\".\"id\" FROM \"trip_session\" \
             WHERE \"trip_session\".\"schedule_id\" IN (?, ?, ?) \
             AND \"trip_session\".\"status\" = 'scheduled' ORDER BY \"trip_session\".\"id\"",
            vec![
                Value::Text("s3".into()),
                Value::Text("s4".into()),
                Value::Text("s5".into()),
            ],
        )
        .unwrap()
        .into_iter()
        .map(|r| match &r[0] {
            Value::Text(t) => t.to_string(),
            v => panic!("{v:?}"),
        })
        .collect();
    let expect: Vec<String> = (0..56)
        .filter(|i| [3usize, 4, 5].contains(&(i % 7)))
        .map(|i| format!("t{i:03}"))
        .collect();
    assert_eq!(rows, expect);
}

#[test]
fn qualified_not_in_stays_residual() {
    let db = setup();
    // NOT IN never drives the seek path (SQLite evaluates it as an
    // expression) — qualification must not change that.
    let p = plan(
        &db,
        "SELECT id FROM trip_session WHERE \"trip_session\".\"schedule_id\" NOT IN ('s0')",
    );
    assert!(
        !p.iter().any(|s| s.contains("IN values")),
        "NOT IN must stay residual, got {p:?}"
    );
    let got = one_col(
        &db,
        "SELECT id FROM trip_session WHERE \"trip_session\".\"schedule_id\" NOT IN ('s0')",
    );
    assert_eq!(got.len(), 56 - 8); // 8 rows per schedule id
}

#[test]
fn qualified_in_on_collated_index_still_gated() {
    let mut db = Database::open_in_memory().unwrap();
    for sql in [
        "CREATE TABLE c (k TEXT COLLATE NOCASE)",
        "CREATE INDEX ck ON c (k)",
    ] {
        db.execute(sql, []).unwrap();
    }
    db.execute("INSERT INTO c VALUES ('Apple'), ('banana')", [])
        .unwrap();
    // The NOCASE index serves the NOCASE comparison — qualified or not.
    let p = plan(
        &db,
        "SELECT \"c\".\"k\" FROM \"c\" WHERE \"c\".\"k\" IN ('APPLE')",
    );
    assert!(
        p.iter().any(|s| s.contains("USING INDEX ck (1 IN values)")),
        "collated qualified IN must drive IndexIn, got {p:?}"
    );
    assert_eq!(
        one_col(
            &db,
            "SELECT \"c\".\"k\" FROM \"c\" WHERE \"c\".\"k\" IN ('APPLE')"
        ),
        vec!["Apple"]
    );
}

#[test]
fn qualified_in_matches_sqlite_results() {
    // End-to-end equivalence on a fan-out shape with residual filters,
    // ORDER BY and aggregate-free projection: compare against values
    // computed in Rust (the SQLite ground truth for this tiny set).
    let db = setup();
    let got: Vec<String> = db
        .query(
            "SELECT \"t\".\"id\" FROM \"t\" \
             WHERE \"t\".\"v\" IN ('v0', 'v2') AND \"t\".\"n\" > 10 ORDER BY \"t\".\"id\"",
            [],
        )
        .unwrap()
        .into_iter()
        .map(|r| match &r[0] {
            Value::Integer(i) => i.to_string(),
            v => panic!("{v:?}"),
        })
        .collect();
    let expect: Vec<String> = (11..56)
        .filter(|i| i % 4 == 0 || i % 4 == 2)
        .map(|i| i.to_string())
        .collect();
    assert_eq!(got, expect);
}

#[test]
fn dml_updates_through_index_in() {
    // UPDATE whose WHERE drives the IndexIn multi-seek on a TEXT-PK
    // (no rowid-alias) table: the scan must carry the hidden rowid
    // slot so the UPDATE general path can address rows. Before the
    // fix this errored with "UPDATE on a table without INTEGER PRIMARY
    // KEY" — qualified OR unqualified, i.e. a latent engine bug that
    // the sea-orm shape (always qualified) then hit on every bulk
    // seat hold/release UPDATE once qualified INs started driving
    // IndexIn.
    let mut db = Database::open_in_memory().unwrap();
    for sql in [
        "CREATE TABLE seat_inventory (\
             id TEXT PRIMARY KEY, trip_session_id TEXT NOT NULL, \
             seat_id TEXT NOT NULL, status TEXT NOT NULL)",
        "CREATE INDEX seatinv_seatid_idx ON seat_inventory (seat_id)",
        "CREATE INDEX seatinv_tsid_idx ON seat_inventory (trip_session_id)",
    ] {
        db.execute(sql, []).unwrap();
    }
    for (id, seat) in [("r1", "s1"), ("r2", "s2"), ("r3", "s3")] {
        db.execute(
            &format!("INSERT INTO seat_inventory VALUES ('{id}', 't1', '{seat}', 'available')"),
            [],
        )
        .unwrap();
    }
    // The sea-orm shape: every column qualified.
    db.execute(
        "UPDATE \"seat_inventory\" SET \"status\" = 'held' \
         WHERE \"seat_inventory\".\"trip_session_id\" = 't1' \
         AND \"seat_inventory\".\"seat_id\" IN ('s1', 's3') \
         AND \"seat_inventory\".\"status\" = 'available'",
        [],
    )
    .unwrap();
    let statuses: Vec<String> = db
        .query("SELECT status FROM seat_inventory ORDER BY id", [])
        .unwrap()
        .into_iter()
        .map(|r| r[0].as_text().to_string())
        .collect();
    assert_eq!(statuses, vec!["held", "available", "held"], "both IN members must update");

    // DELETE through the same shape.
    db.execute(
        "DELETE FROM \"seat_inventory\" WHERE \"seat_inventory\".\"seat_id\" IN ('s2')",
        [],
    )
    .unwrap();
    let left: Vec<String> = db
        .query("SELECT id FROM seat_inventory ORDER BY id", [])
        .unwrap()
        .into_iter()
        .map(|r| r[0].as_text().to_string())
        .collect();
    assert_eq!(left, vec!["r1", "r3"], "the deleted member must be gone");
}

#[test]
fn select_output_over_index_in_has_no_extra_column() {
    // The hidden rowid slot rides IndexIn rows for DML, but the SELECT
    // projection must never leak it: explicit column lists bind by
    // name, and `SELECT *` star expansion filters hidden slots.
    let db = setup();
    let rows = db
        .query(
            "SELECT \"trip_session\".\"id\" FROM \"trip_session\" \
             WHERE \"trip_session\".\"schedule_id\" IN ('s1', 's2')",
            [],
        )
        .unwrap();
    assert!(rows.iter().all(|r| r.len() == 1));
    let star = db
        .query(
            "SELECT * FROM trip_session WHERE schedule_id IN ('s1', 's2') LIMIT 2",
            [],
        )
        .unwrap();
    // trip_session has 5 columns; the hidden slot never appears.
    assert!(star.iter().all(|r| r.len() == 5));
}
