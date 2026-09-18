//! MILLION-RECORD differential: the file-format support, stressed at
//! scale and compared against real SQLite end-to-end.
//!
//! The user-facing contract: "create millions of records and compare
//! this with sqlite too." Concretely, this suite
//!
//!   1. BUILDS the same deterministic dataset (events + users) on BOTH
//!      engines, FILE-BACKED: the engine through `open_sqlite_format`
//!      (the SQLite disk format — the interop file), real SQLite
//!      through rusqlite. Same rowids, same values, same indexes.
//!   2. Compares ANSWERS on a battery that touches every access shape:
//!      full-table aggregates, GROUP BY at scale, index point probes
//!      (sampled), range scans, ORDER BY (both directions, tie-broken),
//!      LIKE prefix scans, multi-column filters, IN lists.
//!   3. CHURNS at scale: band UPDATEs, band DELETEs, re-INSERT of the
//!      deleted range, then re-verifies the whole battery — on both
//!      engines, statement-for-statement.
//!   4. Cross-verifies the FILES: real SQLite opens the ENGINE-written
//!      file (integrity_check + counts + probes), and the engine opens
//!      the SQLITE-written file. Two-way file-format support at scale.
//!
//! Env knobs (CI cranks these; defaults keep `cargo test` quick):
//!   MILLION_ROWS  — rows in the events table (default 150_000)
//!   MILLION_BATCH — multi-VALUES rows per INSERT statement (default 500)

use rusqlite::Connection;
use rustqlite::Value;
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Deterministic data (splitmix64 per rowid — same values on both engines,
// no materialized dataset: millions of rows, zero RSS).
// ---------------------------------------------------------------------------

fn splitmix64(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

const KINDS: [&str; 5] = ["click", "view", "buy", "refund", "login"];

/// One events row by rowid: (user_id, kind, amount, note).
fn gen_event(i: i64) -> (i64, &'static str, f64, String) {
    let h = splitmix64(i as u64);
    let user_id = (h % 100_000) as i64;
    let kind = KINDS[((h >> 17) % 5) as usize];
    // Two exact decimal places: f64 arithmetic stays deterministic when
    // both engines accumulate in the same (rowid) order.
    let amount = ((h >> 32) % 1_000_000) as f64 / 100.0;
    let note = format!("n{:08x}", (h >> 20) & 0xffff_ffff);
    (user_id, kind, amount, note)
}

/// One users row by number: (email, name, n). Emails are unique TEXT
/// keys (a TEXT PRIMARY KEY unique index at scale).
fn gen_user(i: i64) -> (String, String, i64) {
    let h = splitmix64((i as u64).wrapping_mul(3));
    (
        format!("u{:07}@x.com", i),
        format!("name{:x}", h & 0xffff),
        (h % 1_000_000) as i64,
    )
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

// ---------------------------------------------------------------------------
// Differential helpers (same conventions as tests/index_stress.rs).
// ---------------------------------------------------------------------------

fn clean(s: &str) -> String {
    let s = s.strip_prefix("error: ").unwrap_or(s);
    if let Some(pos) = s.find(" in INSERT ") {
        if s[pos..].contains(" at offset ") {
            return s[..pos].to_string();
        }
    }
    s.to_string()
}

/// Execute one statement on BOTH engines; errors must match exactly.
fn both(db: &mut rustqlite::Database, rc: &Connection, sql: &str) {
    let e1 = db.execute(sql, ()).map_err(|e| e.to_string());
    let e2 = rc.execute_batch(sql).map_err(|e| clean(&e.to_string()));
    assert_eq!(
        e1, e2,
        "engine/SQLite disagree on: {sql}\n  engine: {e1:?}\n  sqlite: {e2:?}"
    );
}

fn sv(v: rusqlite::types::ValueRef<'_>) -> Value {
    match v {
        rusqlite::types::ValueRef::Null => Value::Null,
        rusqlite::types::ValueRef::Integer(v) => Value::Integer(v),
        rusqlite::types::ValueRef::Real(v) => Value::Real(v),
        rusqlite::types::ValueRef::Text(t) => {
            Value::Text(String::from_utf8_lossy(t).to_string().into())
        }
        rusqlite::types::ValueRef::Blob(b) => Value::Blob(b.to_vec()),
    }
}

fn engine_rows(db: &rustqlite::Database, sql: &str) -> Vec<Value> {
    db.query(sql, ())
        .unwrap_or_else(|e| panic!("engine query failed: {sql}: {e}"))
        .into_iter()
        .flatten()
        .collect()
}

fn sqlite_rows(rc: &Connection, sql: &str) -> Vec<Value> {
    let mut out = Vec::new();
    let mut stmt = rc.prepare(sql).expect("sqlite prepare");
    let n = stmt.column_count();
    let mut rows = stmt.query([]).unwrap();
    while let Ok(Some(r)) = rows.next() {
        for i in 0..n {
            out.push(sv(r.get_ref(i).unwrap()));
        }
    }
    out
}

/// Run one query on BOTH engines and compare every returned value.
fn q_both(db: &rustqlite::Database, rc: &Connection, sql: &str) {
    let r1 = engine_rows(db, sql);
    let r2 = sqlite_rows(rc, sql);
    assert_eq!(r1, r2, "engine/SQLite rows disagree on: {sql}");
}

fn temp_path(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "rsql_million_{}_{}_{}",
        name,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            % 1_000_000
    ));
    let _ = std::fs::remove_file(&p);
    let _ = std::fs::remove_file(rustqlite::storage::sqlitefmt::reader::wal_path_of(&p));
    p
}

// ---------------------------------------------------------------------------
// The suite.
// ---------------------------------------------------------------------------

#[test]
fn million_record_differential() {
    let rows = env_u64("MILLION_ROWS", 150_000) as i64;
    let batch = env_u64("MILLION_BATCH", 500).min(rows as u64).max(1) as i64;
    let users = (rows / 40).max(1);

    let engine_path = temp_path("engine");
    let sqlite_path = temp_path("sqlite");

    let mut db = rustqlite::Database::open_sqlite_format(&engine_path).unwrap();
    let rc = Connection::open(&sqlite_path).unwrap();

    // ---- Schema on both engines --------------------------------------
    let schema = [
        "CREATE TABLE events (id INTEGER PRIMARY KEY, user_id INT, kind TEXT, amount REAL, note TEXT)",
        "CREATE INDEX ie_user ON events(user_id)",
        "CREATE INDEX ie_kind ON events(kind)",
        "CREATE TABLE users (email TEXT PRIMARY KEY, name TEXT, n INT)",
    ];
    for s in schema {
        both(&mut db, &rc, s);
    }

    let t0 = std::time::Instant::now();
    let phase = |name: &str| {
        if std::env::var_os("MILLION_TIMING").is_some() {
            eprintln!("[million] {name}: {:?} (rows={rows})", t0.elapsed());
        }
    };

    // ---- Build phase: same deterministic rows on both engines --------
    // Engine: multi-VALUES INSERT batches inside explicit transactions
    // (SQLite's own bulk-load practice — one commit per chunk).
    // SQLite: a PREPARED statement with bound parameters (its own
    // fastest bulk path). The DATA is identical; only the ingestion
    // path differs — each engine exercises its native bulk-load shape.
    build_events(&mut db, &rc, 1, rows, batch);
    build_users(&mut db, &rc, users);

    phase("build");
    // ---- The answer battery -------------------------------------------
    battery(&db, &rc, rows, users);
    phase("battery1");

    // ---- Churn at scale ------------------------------------------------
    // A band UPDATE, a band DELETE, and a re-INSERT of exactly the
    // deleted rows — the whole battery re-verified after each phase.
    let lo = rows / 4;
    let hi = lo + rows / 10;
    both(
        &mut db,
        &rc,
        "UPDATE events SET amount = amount + 1, note = note || 'x' WHERE id BETWEEN {lo} AND {hi}"
            .replace("{lo}", &lo.to_string())
            .replace("{hi}", &hi.to_string())
            .as_str(),
    );
    phase("churn-update");
    battery(&db, &rc, rows, users);
    phase("battery3");

    both(
        &mut db,
        &rc,
        "DELETE FROM events WHERE id BETWEEN {lo} AND {hi}"
            .replace("{lo}", &lo.to_string())
            .replace("{hi}", &hi.to_string())
            .as_str(),
    );
    phase("churn-delete");
    battery(&db, &rc, rows, users);
    phase("battery4");

    // Re-insert the deleted band with the SAME generator values.
    build_events(&mut db, &rc, lo, hi, batch);
    battery(&db, &rc, rows, users);
    phase("battery5");

    // User churn: UPDATE by point probes on the TEXT PK, DELETE + re-add.
    for j in [0, users / 2, users - 1] {
        let (e, _, _) = gen_user(j);
        both(
            &mut db,
            &rc,
            &format!("UPDATE users SET n = n + 1 WHERE email = '{e}'"),
        );
    }
    q_both(&db, &rc, "SELECT count(*), sum(n) FROM users");

    phase("user-churn");
    // ---- Two-way FILE verification ------------------------------------
    // Close the engine (clean WAL fold), then real SQLite opens the
    // ENGINE-written file and fully verifies it.
    drop(db);
    {
        let v = Connection::open(&engine_path).unwrap();
        let ic: String = v
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ic, "ok", "real SQLite integrity_check on engine file");
        let n: i64 = v
            .query_row("SELECT count(*) FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, rows, "SQLite count on engine file");
        let kinds: i64 = v
            .query_row(
                "SELECT count(*) FROM events WHERE kind = 'buy' AND user_id BETWEEN 1000 AND 2000",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let engine_kinds = engine_rows(
            &rustqlite::Database::open(&engine_path).unwrap(),
            "SELECT count(*) FROM events WHERE kind = 'buy' AND user_id BETWEEN 1000 AND 2000",
        );
        assert_eq!(
            Value::Integer(kinds),
            engine_kinds[0].clone(),
            "engine re-read of its own file agrees with SQLite"
        );
    }

    // The engine opens the SQLITE-written file and answers the same
    // battery shape (from_sqlite_file path).
    {
        let fdb = rustqlite::Database::open(&sqlite_path).unwrap();
        assert_eq!(fdb.disk_format(), "sqlite");
        let c = engine_rows(&fdb, "SELECT count(*) FROM events");
        assert_eq!(c, vec![Value::Integer(rows)]);
        let g = engine_rows(
            &fdb,
            "SELECT kind, count(*) FROM events GROUP BY kind ORDER BY kind",
        );
        let g2 = sqlite_rows(
            &rc,
            "SELECT kind, count(*) FROM events GROUP BY kind ORDER BY kind",
        );
        assert_eq!(g, g2, "engine reads SQLite's file: GROUP BY at scale");
        // TEXT-PK unique probes through the foreign file. The churn
        // phase bumped n for users 0, users/2 and users-1 — the expected
        // values must carry those +1s.
        for j in [0, users / 3, users - 1] {
            let (e, name, n) = gen_user(j);
            let bumped = matches!(j, 0) || j == users / 2 || j == users - 1;
            let n = n + if bumped { 1 } else { 0 };
            let got = engine_rows(
                &fdb,
                &format!("SELECT name, n FROM users WHERE email = '{e}'"),
            );
            assert_eq!(
                got,
                vec![Value::Text(name.into()), Value::Integer(n)],
                "TEXT PK point probe through SQLite's file"
            );
        }
    }

    phase("file-verify");
    // Cleanup (big files).
    let _ = std::fs::remove_file(&engine_path);
    let _ = std::fs::remove_file(rustqlite::storage::sqlitefmt::reader::wal_path_of(
        &engine_path,
    ));
    let _ = std::fs::remove_file(&sqlite_path);
    let _ = std::fs::remove_file(rustqlite::storage::sqlitefmt::reader::wal_path_of(
        &sqlite_path,
    ));
}

/// Load events rows `[lo, hi]` into BOTH engines, each through its own
/// native bulk path, inside chunked explicit transactions.
fn build_events(db: &mut rustqlite::Database, rc: &Connection, lo: i64, hi: i64, batch: i64) {
    if lo > hi {
        return;
    }
    // Big explicit transactions: the SQLite-format commit boundary pays
    // an O(db) image rebuild (documented engine cost; SQLite's own
    // bulk-load advice is exactly this — batch work in BEGIN..COMMIT).
    // 50k rows per transaction keeps the load linear-time.
    let tx_rows = (batch * 100).max(batch);
    let mut stmt = rc
        .prepare("INSERT INTO events VALUES (?1, ?2, ?3, ?4, ?5)")
        .unwrap();
    let mut i = lo;
    while i <= hi {
        let t = (i + tx_rows - 1).min(hi);
        db.execute("BEGIN", ()).unwrap();
        rc.execute_batch("BEGIN").unwrap();
        let mut j = i;
        while j <= t {
            let h = (j + batch - 1).min(t);
            let mut sql = String::with_capacity(56 * (h - j + 1) as usize);
            sql.push_str("INSERT INTO events VALUES ");
            for id in j..=h {
                let (u, k, a, n) = gen_event(id);
                if id > j {
                    sql.push(',');
                }
                sql.push_str(&format!("({id}, {u}, '{k}', {a:.2}, '{n}')"));
            }
            db.execute(&sql, ()).unwrap();
            j = h + 1;
        }
        for id in i..=t {
            let (u, k, a, n) = gen_event(id);
            stmt.execute(rusqlite::params![id, u, k, a, n]).unwrap();
        }
        db.execute("COMMIT", ()).unwrap();
        rc.execute_batch("COMMIT").unwrap();
        i = t + 1;
    }
}

/// Load the users table into BOTH engines (TEXT PRIMARY KEY at scale).
fn build_users(db: &mut rustqlite::Database, rc: &Connection, users: i64) {
    let mut stmt = rc.prepare("INSERT INTO users VALUES (?1, ?2, ?3)").unwrap();
    let mut i = 0i64;
    while i < users {
        let hi = (i + 2000).min(users);
        db.execute("BEGIN", ()).unwrap();
        rc.execute_batch("BEGIN").unwrap();
        let mut sql = String::from("INSERT INTO users VALUES ");
        for j in i..hi {
            let (e, name, n) = gen_user(j);
            if j > i {
                sql.push(',');
            }
            sql.push_str(&format!("('{e}', '{name}', {n})"));
        }
        db.execute(&sql, ()).unwrap();
        for j in i..hi {
            let (e, name, n) = gen_user(j);
            stmt.execute(rusqlite::params![e, name, n]).unwrap();
        }
        db.execute("COMMIT", ()).unwrap();
        rc.execute_batch("COMMIT").unwrap();
        i = hi;
    }
}

/// The engine-vs-SQLite answer battery (stable orders, tie-broken).
fn battery(db: &rustqlite::Database, rc: &Connection, rows: i64, users: i64) {
    // Whole-table aggregates.
    q_both(
        db,
        rc,
        "SELECT count(*), count(note), min(user_id), max(user_id), min(amount), max(amount) FROM events",
    );
    // GROUP BY at scale (ordered, with per-group sum).
    q_both(
        db,
        rc,
        "SELECT kind, count(*), sum(amount), min(amount), max(amount) FROM events GROUP BY kind ORDER BY kind",
    );
    // Sampled point probes through the user_id index.
    for probe in [0i64, 50_000 / 2, 50_000, 999_999 % 100_000, 42] {
        q_both(
            db,
            rc,
            &format!("SELECT count(*) FROM events WHERE user_id = {probe}"),
        );
        q_both(
            db,
            rc,
            &format!(
                "SELECT id, kind, amount FROM events WHERE user_id = {probe} ORDER BY id LIMIT 25"
            ),
        );
    }
    // Range scans over the INTEGER PRIMARY KEY.
    let lo = rows / 5;
    let hi = lo + rows / 20;
    q_both(
        db,
        rc,
        &format!("SELECT count(*), sum(amount) FROM events WHERE id BETWEEN {lo} AND {hi}"),
    );
    // ORDER BY both directions over the whole table (tie-broken).
    q_both(
        db,
        rc,
        "SELECT id, amount, note FROM events ORDER BY amount DESC, id LIMIT 40",
    );
    q_both(
        db,
        rc,
        "SELECT id, note FROM events ORDER BY note ASC, id DESC LIMIT 40",
    );
    // LIKE prefix scan (sampled real prefixes from the generator).
    for pfx in ["n0000", "n00ff", "n0f0f"] {
        q_both(
            db,
            rc,
            &format!("SELECT count(*) FROM events WHERE note LIKE '{pfx}%'"),
        );
    }
    // Multi-column filter + GROUP BY over a band.
    q_both(
        db,
        rc,
        &format!(
            "SELECT kind, count(*) FROM events WHERE id BETWEEN {lo} AND {hi} AND user_id >= 50000 GROUP BY kind ORDER BY kind"
        ),
    );
    // IN list probes.
    q_both(
        db,
        rc,
        &format!(
            "SELECT count(*) FROM events WHERE user_id IN ({}, {}, {}, {})",
            lo % 100_000,
            (lo + 1) % 100_000,
            (lo + 2) % 100_000,
            (lo + 3) % 100_000
        ),
    );
    // users: TEXT PK at scale — count, ordered walk, point probes,
    // range over the unique key.
    q_both(db, rc, "SELECT count(*), sum(n) FROM users");
    q_both(
        db,
        rc,
        "SELECT email, name, n FROM users ORDER BY email LIMIT 60",
    );
    q_both(
        db,
        rc,
        "SELECT email, name, n FROM users ORDER BY email DESC LIMIT 60",
    );
    for j in [0, users / 2, users - 1, users / 7] {
        let (e, name, n) = gen_user(j);
        q_both(
            db,
            rc,
            &format!("SELECT name, n FROM users WHERE email = '{e}'"),
        );
        assert_eq!(
            engine_rows(db, &format!("SELECT name FROM users WHERE email = '{e}'")),
            vec![Value::Text(name.clone().into())],
            "point probe value check"
        );
        let _ = n;
    }
    q_both(
        db,
        rc,
        &format!(
            "SELECT count(*) FROM users WHERE email BETWEEN 'u{:07}@x.com' AND 'u{:07}@x.com'",
            users / 4,
            users / 2
        ),
    );
}
