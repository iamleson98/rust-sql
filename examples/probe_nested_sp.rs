//! Nested-savepoint rollback repro probe (the preimage round's find).
//!
//! Shape: BEGIN -> SAVEPOINT a -> mass UPDATE (index splits) -> SAVEPOINT b
//! -> mass UPDATE (more splits) -> ROLLBACK TO a -> verify. Fails with
//! "page N out of range" corruption on master (9cd03e6, pre-spill).
//!
//! Usage: probe_nested_sp [rows] [band1] [band2]

use rustqlite::Database;

fn main() {
    // Libtest runs each test on its own thread — replicate that here
    // (thread-identity machinery may be the discriminator).
    std::thread::Builder::new()
        .name("probe-thread".into())
        .spawn(run)
        .unwrap()
        .join()
        .unwrap();
}

fn run() {
    let rows: i64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(12000);
    let band1: i64 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(rows / 2);
    let band2: i64 = std::env::args()
        .nth(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(rows);
    println!("[probe] rows={rows} band1={band1} band2={band2}");

    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::open(dir.path().join("p.db")).unwrap();
    db.execute("PRAGMA journal_mode = WAL", []).unwrap();
    db.execute("PRAGMA synchronous = NORMAL", []).unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, k INTEGER)", [])
        .unwrap();
    db.execute("CREATE INDEX ix_t_k ON t (k)", []).unwrap();

    let mut next = 1i64;
    while next <= rows {
        let n = (rows - next + 1).min(2000);
        let mut sql = String::from("INSERT INTO t (id, k) VALUES ");
        for i in 0..n {
            if i > 0 {
                sql.push(',');
            }
            sql.push_str(&format!("({}, {})", next + i, (next + i) % 97));
        }
        db.execute(&sql, []).unwrap();
        next += n;
    }
    let base = state(&db);
    println!("[probe] base state: {base:?}");

    db.execute("BEGIN", []).unwrap();
    db.execute("SAVEPOINT a", []).unwrap();
    db.execute(&format!("UPDATE t SET k = k + 1 WHERE id <= {band1}"), [])
        .unwrap();
    println!("[probe] after upd1: {:?}", state(&db));
    db.execute("SAVEPOINT b", []).unwrap();
    db.execute(&format!("UPDATE t SET k = k + 2 WHERE id <= {band2}"), [])
        .unwrap();
    println!("[probe] after upd2: {:?}", state(&db));

    db.execute("ROLLBACK TO a", []).unwrap();
    // EXACT test sequence: no intervening query — straight to state().
    let s = state(&db);
    println!("[probe] after ROLLBACK TO a (no intervening query): {s:?}");
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| state(&db))) {
        Ok(s) => {
            println!("[probe] after ROLLBACK TO a: {s:?}");
            if s == base {
                println!("[probe] VERDICT: PASS (state restored)");
            } else {
                println!("[probe] VERDICT: WRONG STATE {s:?} != base {base:?}");
            }
        }
        Err(_) => {
            println!("[probe] VERDICT: PANIC during state query (corruption)");
        }
    }
}

fn state(db: &Database) -> (i64, i64, i64) {
    let row = db
        .query("SELECT count(*), sum(k), coalesce(max(k), -1) FROM t", [])
        .unwrap();
    let r = &row[0];
    (r[0].as_integer(), r[1].as_integer(), r[2].as_integer())
}
