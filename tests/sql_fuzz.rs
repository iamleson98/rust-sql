//! SQL fuzz testing — modeled on §4.1 of https://www.sqlite.org/testing.html
//!
//! SQLite fuzzes with four independent engines (AFL, OSS-Fuzz, dbsqlfuzz,
//! fuzzcheck). The two strategies reproduced here are the ones that are
//! effective without coverage-guided infrastructure:
//!
//! 1. **Mutation fuzz**: take a corpus of valid SQL programs, mutate the
//!    bytes (flip, truncate, splice, duplicate, swap chunks), and feed the
//!    result to the engine. The engine must NEVER panic, hang, or corrupt
//!    itself: every input either executes or returns a graceful `Err`.
//!    This is what fuzzershell.c does for SQLite.
//!
//! 2. **Structured random SQL**: generate syntactically valid but
//!    semantically wild statements (random expressions, random column
//!    mixes, random WHERE clauses over a seeded random table). Run each
//!    statement against BOTH rustqlite and SQLite; when both succeed the
//!    results must match, and when one errors the other must too (for
//!    syntax errors). This mirrors dbsqlfuzz's dual-run checking.
//!
//! Determinism: every iteration is driven by an explicit PRNG seed
//! (RUSTQLITE_FUZZ_SEED), so any failure reproduces exactly — SQLite keeps
//! seed corpora for the same reason (fuzzcheck reruns "interesting" cases).
//!
//! Run with:
//!     cargo test --test sql_fuzz                # default budget
//!     RUSTQLITE_FUZZ_ITERS=200000 cargo test --test sql_fuzz -- --nocapture
//!     RUSTQLITE_FUZZ_SEED=12345 cargo test --test sql_fuzz   # reproduce

use rustqlite::{Database, Value};

// ===========================================================================
// Deterministic PRNG (xorshift64*) — no dependency on rand so failures are
// reproducible across machines and rand versions.
// ===========================================================================

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407)
            | 1)
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
        if n == 0 {
            0
        } else {
            (self.next_u64() % n as u64) as usize
        }
    }
    fn range(&mut self, lo: i64, hi: i64) -> i64 {
        lo + (self.next_u64() % (hi - lo + 1) as u64) as i64
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.next_u64() % 100 < pct
    }
}

fn env_iters(default: u64) -> u64 {
    std::env::var("RUSTQLITE_FUZZ_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn env_seed() -> u64 {
    std::env::var("RUSTQLITE_FUZZ_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0x5EED_CAFE_F00D_0001)
}

// ===========================================================================
// 1. Mutation fuzz: no panics, no hangs, graceful errors only.
// ===========================================================================

/// Corpus of valid SQL programs exercising a broad feature surface.
/// (Deliberately overlapping with the differential suite but shorter —
/// mutation fuzz needs variety, not depth.)
const CORPUS: &[&str] = &[
    "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val REAL)",
    "INSERT INTO t (name, val) VALUES ('a', 1.5), ('b', 2)",
    "SELECT id, name FROM t WHERE val > 1 ORDER BY name DESC LIMIT 5",
    "UPDATE t SET val = val + 1 WHERE id IN (1, 2, 3)",
    "DELETE FROM t WHERE name IS NULL OR val < 0",
    "SELECT COUNT(*), SUM(val), MIN(name), MAX(id) FROM t GROUP BY name HAVING COUNT(*) > 1",
    "CREATE INDEX idx_val ON t(val DESC) WHERE val IS NOT NULL",
    "SELECT a.id, b.name FROM t a JOIN t b ON a.id = b.id - 1 WHERE a.val <> b.val",
    "SELECT * FROM t WHERE name LIKE 'a%' OR name GLOB '*b' ESCAPE '!'",
    "WITH c AS (SELECT id * 2 AS x FROM t) SELECT x FROM c UNION ALL SELECT -x FROM c ORDER BY 1",
    "SELECT CASE WHEN val > 0 THEN 'pos' WHEN val < 0 THEN 'neg' ELSE 'zero' END, COALESCE(NULL, name, '') FROM t",
    "INSERT OR REPLACE INTO t (id, name) SELECT id + 100, name FROM t WHERE val BETWEEN 1 AND 10",
    "SELECT substr(name, 2, 3), length(name), upper(name), abs(val), round(val, 2) FROM t",
    "SELECT id FROM t WHERE id = ? OR id = ?",
    "BEGIN; UPDATE t SET val = 0; SELECT count(*) FROM t; COMMIT",
    "SELECT DISTINCT name FROM t EXCEPT SELECT name FROM t WHERE val IS NULL",
    "CREATE VIEW v AS SELECT name, val * 2 AS dv FROM t WHERE val > 0",
    "SELECT json('{\"a\": [1, 2, {\"b\": null}]}') -> 'a' -> 2 ->> 'b'",
    "SELECT datetime('2024-02-29 12:34:56', '+1 month', 'weekday 3')",
    "SELECT quote(x'DEADBEEF'), hex('hi'), typeof(1), typeof(1.0), typeof('x'), typeof(NULL)",
    "ALTER TABLE t ADD COLUMN extra TEXT DEFAULT 'x'",
    "SELECT (SELECT max(val) FROM t) - (SELECT min(val) FROM t) AS spread FROM t LIMIT 1",
];

/// Apply one random mutation to a byte string.
fn mutate(rng: &mut Rng, src: &str) -> String {
    let mut bytes = src.as_bytes().to_vec();
    if bytes.is_empty() {
        return String::new();
    }
    let n_mut = 1 + rng.below(3);
    for _ in 0..n_mut {
        let pos = rng.below(bytes.len());
        match rng.below(7) {
            // bit flip
            0 => bytes[pos] ^= 1 << (rng.below(8) as u32),
            // random byte replacement
            1 => bytes[pos] = rng.next_u64() as u8,
            // truncate
            2 => bytes.truncate(pos),
            // duplicate a chunk
            3 => {
                let len = (1 + rng.below(16)).min(bytes.len() - pos);
                let chunk = bytes[pos..pos + len].to_vec();
                bytes.extend_from_slice(&chunk);
            }
            // delete a chunk
            4 => {
                let len = (1 + rng.below(16)).min(bytes.len() - pos);
                bytes.drain(pos..pos + len);
            }
            // insert junk token
            5 => {
                let junk =
                    [b'\'', b'"', b';', b'-', b'(', b')', b'?', b'@', b'\0', 0xFF][rng.below(10)];
                bytes.insert(pos, junk);
            }
            // swap two bytes
            _ => {
                let pos2 = rng.below(bytes.len());
                bytes.swap(pos, pos2);
            }
        }
        if bytes.is_empty() {
            break;
        }
    }
    // The engine takes &str: byte-level mutations may produce invalid UTF-8.
    // That is itself a fuzz input — use lossy conversion to keep feeding it.
    String::from_utf8_lossy(&bytes).into_owned()
}

#[test]
fn mutation_fuzz_never_panics() {
    let iters = env_iters(20_000);
    let mut rng = Rng::new(env_seed());
    let mut db = Database::open_in_memory().unwrap();
    // Seed the database with a valid schema so mutated statements have
    // something real to bind against (most mutations still reference t).
    db.execute(CORPUS[0], []).unwrap();
    db.execute(CORPUS[1], []).unwrap();

    let mut executed = 0usize;
    let mut rejected = 0usize;
    for i in 0..iters {
        let src = CORPUS[rng.below(CORPUS.len())];
        let sql = mutate(&mut rng, src);
        // The contract: execute() returns Ok or Err. It must not panic,
        // not abort, and must leave the DB usable for the next iteration.
        match db.execute(&sql, []) {
            Ok(()) => executed += 1,
            Err(_) => rejected += 1,
        }
        // Periodically verify the engine is still alive and correct.
        // NOTE: the corpus contains DDL (CREATE TABLE/INDEX/VIEW, ALTER
        // TABLE), so a mutation may have legitimately redefined schema
        // objects — `t` may no longer be a table at all. The invariants
        // that must hold regardless of schema state:
        //   1. a schema-INDEPENDENT statement must still work perfectly
        //      (SELECT 1 — pure execution-path sanity),
        //   2. schema-dependent statements must not panic (harness
        //      catches panics); graceful Err is acceptable when the
        //      schema was legitimately mutated (SQLite's fuzzershell
        //      likewise only demands "no crash, no hang").
        if i % 512 == 511 {
            let rows = db
                .query("SELECT 1", [])
                .expect("engine state corrupted: SELECT 1 failed after fuzz input");
            assert_eq!(
                rows.first().and_then(|r| r.first()),
                Some(&Value::Integer(1)),
                "SELECT 1 returned garbage {:?} after fuzz input {:?}",
                rows.first(),
                sql
            );
            // When the COUNT target still exists and the query succeeds,
            // the answer must be a coherent non-negative integer.
            if let Ok(rows) = db.query("SELECT COUNT(*) FROM t", []) {
                let n = match rows.first().and_then(|r| r.first()) {
                    Some(Value::Integer(n)) => *n,
                    other => panic!(
                        "COUNT(*) returned non-integer {:?} after fuzz input {:?}",
                        other, sql
                    ),
                };
                assert!(
                    n >= 0,
                    "COUNT(*) returned negative {} after fuzz input {:?}",
                    n,
                    sql
                );
            }
        }
    }
    eprintln!(
        "mutation_fuzz_never_panics: {} iterations, {} executed, {} rejected (graceful errors)",
        iters, executed, rejected
    );
}

/// Fuzz the *parameter* path too: arbitrary Value combinations against
/// prepared statements. Params must never cause type-confusion panics.
#[test]
fn parameter_fuzz_never_panics() {
    let iters = env_iters(5_000);
    let mut rng = Rng::new(env_seed() ^ 0xBEEF);
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, s TEXT, r REAL, b BLOB)",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO t (s, r, b) VALUES (?, ?, ?)",
        [
            Value::Text("seed".into()),
            Value::Real(1.0),
            Value::Blob(vec![1, 2, 3]),
        ],
    )
    .unwrap();

    let odd_values: Vec<Value> = vec![
        Value::Integer(i64::MIN),
        Value::Integer(i64::MAX),
        Value::Real(f64::NAN),
        Value::Real(f64::INFINITY),
        Value::Real(-f64::INFINITY),
        Value::Real(-0.0),
        Value::Real(f64::MIN_POSITIVE),
        Value::Text("".into()),
        Value::Text("\0\0\0".into()),
        Value::Text("日本語🚀🔥".into()),
        Value::Blob(vec![]),
        Value::Blob((0u8..=255).collect()),
        Value::Blob(vec![0xFF; 100_000]),
        Value::Null,
    ];

    for i in 0..iters {
        let a = if rng.chance(70) {
            odd_values[rng.below(odd_values.len())].clone()
        } else {
            Value::Integer(rng.range(i64::MIN / 4, i64::MAX / 4))
        };
        let b = if rng.chance(70) {
            odd_values[rng.below(odd_values.len())].clone()
        } else {
            Value::Real(rng.next_u64() as f64)
        };
        let c = odd_values[rng.below(odd_values.len())].clone();

        // Every statement type that binds parameters.
        let _ = db.execute(
            "INSERT INTO t (s, r, b) VALUES (?, ?, ?)",
            [a.clone(), b.clone(), c.clone()],
        );
        let _ = db.query(
            "SELECT * FROM t WHERE s = ? OR r = ? OR b = ?",
            [a.clone(), b.clone(), c.clone()],
        );
        let _ = db.query(
            "SELECT id + ? FROM t WHERE id BETWEEN ? AND ?",
            [a.clone(), b.clone(), c.clone()],
        );
        let _ = db.query("SELECT substr(s, ?, ?) FROM t LIMIT ?", [a, b, c]);
        if i % 256 == 255 {
            // Sanity: the seed row must always be findable.
            let rows = db
                .query("SELECT COUNT(*) FROM t", [])
                .expect("parameter fuzz corrupted engine state");
            assert!(
                matches!(rows.first().and_then(|r| r.first()), Some(Value::Integer(n)) if *n >= 1)
            );
        }
    }
}

// ===========================================================================
// 2. Structured random SQL, differentially verified against SQLite.
//    (dbsqlfuzz-style: same statement, two engines, same answer.)
// ===========================================================================

fn run_on_sqlite(
    conn: &rusqlite::Connection,
    sql: &str,
) -> Option<Vec<Vec<rusqlite::types::Value>>> {
    use rusqlite::types::Value as Sv;
    let mut stmt = match conn.prepare(sql) {
        Ok(s) => s,
        Err(_) => return None,
    };
    let ncols = stmt.column_count();
    if ncols == 0 {
        return match stmt.execute([]) {
            Ok(_) => Some(Vec::new()),
            Err(_) => None,
        };
    }
    let mut rows = stmt.query([]).ok()?;
    let mut out = Vec::new();
    while let Ok(Some(row)) = rows.next() {
        let mut r = Vec::with_capacity(ncols);
        for i in 0..ncols {
            let v: Sv = row.get(i).unwrap_or(Sv::Null);
            r.push(v);
        }
        out.push(r);
    }
    Some(out)
}

fn values_match(a: &Value, b: &rusqlite::types::Value) -> bool {
    use rusqlite::types::Value as Sv;
    match (a, b) {
        (Value::Null, Sv::Null) => true,
        (Value::Integer(x), Sv::Integer(y)) => x == y,
        (Value::Integer(x), Sv::Real(y)) => (*x as f64 - y).abs() <= 1e-9 * y.abs().max(1.0),
        (Value::Real(x), Sv::Integer(y)) => (*x - *y as f64).abs() <= 1e-9 * x.abs().max(1.0),
        (Value::Real(x), Sv::Real(y)) => {
            // NaN matches NaN; otherwise relative-tolerance compare.
            (x.is_nan() && y.is_nan()) || (x - y).abs() <= 1e-9 * x.abs().max(y.abs()).max(1.0)
        }
        (Value::Text(x), Sv::Text(y)) => x.as_str() == y,
        (Value::Blob(x), Sv::Blob(y)) => x == y,
        _ => false,
    }
}

const COLS: &[&str] = &["a", "b", "c"];
const OPS: &[&str] = &["+", "-", "*", "/", "%"];
const CMPS: &[&str] = &["=", "<>", "<", "<=", ">", ">="];

/// Generate a random scalar expression over columns a/b/c.
fn gen_expr(rng: &mut Rng, depth: u32) -> String {
    if depth == 0 || rng.chance(40) {
        match rng.below(6) {
            0 => COLS[rng.below(COLS.len())].to_string(),
            1 => rng.range(-1000, 1000).to_string(),
            2 => format!("{:.3}", rng.range(-100, 100) as f64 / 7.0),
            3 => "NULL".to_string(),
            4 => format!("'s{}'", rng.below(20)),
            _ => rng.range(0, 2).to_string(),
        }
    } else {
        let op = OPS[rng.below(OPS.len())];
        let lhs = gen_expr(rng, depth - 1);
        let rhs = gen_expr(rng, depth - 1);
        format!("({} {} {})", lhs, op, rhs)
    }
}

fn gen_predicate(rng: &mut Rng) -> String {
    let cmp = CMPS[rng.below(CMPS.len())];
    match rng.below(4) {
        0 => format!("{} {} {}", COLS[rng.below(3)], cmp, gen_expr(rng, 1)),
        1 => format!(
            "{} IS {}",
            COLS[rng.below(3)],
            if rng.chance(50) { "NULL" } else { "NOT NULL" }
        ),
        2 => format!(
            "{} BETWEEN {} AND {}",
            COLS[rng.below(3)],
            gen_expr(rng, 0),
            gen_expr(rng, 0)
        ),
        _ => format!(
            "{} IN ({}, {}, {})",
            COLS[rng.below(3)],
            rng.range(-5, 5),
            rng.range(-5, 5),
            rng.range(-5, 5)
        ),
    }
}

/// Random but always-valid SQL data generation: both engines get the SAME
/// data so the differential comparison is apples-to-apples.
#[test]
fn structured_random_sql_matches_sqlite() {
    let iters = env_iters(300);
    let mut rng = Rng::new(env_seed() ^ 0xD1FF);

    for case_no in 0..iters {
        let n_rows = 1 + rng.below(40);
        // Generate the data once, materialize as INSERTs both engines run.
        let mut inserts = String::new();
        for i in 0..n_rows {
            let a = if rng.chance(10) {
                "NULL".into()
            } else {
                rng.range(-50, 50).to_string()
            };
            let b = if rng.chance(10) {
                "NULL".into()
            } else {
                format!("{:.2}", rng.range(-99, 99) as f64 / 3.0)
            };
            let c = if rng.chance(10) {
                "NULL".into()
            } else {
                format!("'t{}'", rng.below(8))
            };
            if i > 0 {
                inserts.push_str(", ");
            }
            inserts.push_str(&format!("({}, {}, {})", a, b, c));
        }

        let setup = format!(
            "CREATE TABLE d (a INTEGER, b REAL, c TEXT); INSERT INTO d VALUES {}",
            inserts
        );
        // A random final SELECT with different shapes.
        let select = match rng.below(6) {
            0 => format!(
                "SELECT {} FROM d WHERE {} ORDER BY 1, 2, 3 LIMIT {}",
                COLS[rng.below(3)],
                gen_predicate(&mut rng),
                rng.below(50)
            ),
            1 => "SELECT c, COUNT(*), SUM(a), AVG(b), MIN(a), MAX(b) FROM d GROUP BY c ORDER BY c"
                .to_string(),
            2 => "SELECT DISTINCT a FROM d ORDER BY a".to_string(),
            3 => format!(
                "SELECT {} FROM d WHERE {} ORDER BY 1 DESC",
                gen_expr(&mut rng, 2),
                gen_predicate(&mut rng)
            ),
            4 => "SELECT a, b FROM d UNION SELECT b, a FROM d ORDER BY 1, 2".to_string(),
            _ => format!(
                "SELECT CASE WHEN {} THEN a ELSE b END FROM d ORDER BY 1",
                gen_predicate(&mut rng)
            ),
        };

        // ---- rustqlite ----
        let mut ours = Database::open_in_memory().unwrap();
        let ours_result: Option<Vec<Vec<Value>>> = (|| {
            for stmt in setup.split(';') {
                let s = stmt.trim();
                if !s.is_empty() {
                    ours.execute(s, []).ok()?;
                }
            }
            ours.query(&select, []).ok()
        })();

        // ---- SQLite oracle ----
        let oracle_conn = rusqlite::Connection::open_in_memory().unwrap();
        let oracle_result = (|| {
            for stmt in setup.split(';') {
                let s = stmt.trim();
                if !s.is_empty() {
                    run_on_sqlite(&oracle_conn, s)?;
                }
            }
            run_on_sqlite(&oracle_conn, &select)
        })();

        // Both engines must agree on success/failure, and on rows.
        match (&ours_result, &oracle_result) {
            (Some(our_rows), Some(their_rows)) => {
                assert_eq!(
                    our_rows.len(),
                    their_rows.len(),
                    "case {}: row-count divergence for {:?} (setup rows: {})",
                    case_no,
                    select,
                    n_rows
                );
                for (i, (r1, r2)) in our_rows.iter().zip(their_rows.iter()).enumerate() {
                    assert_eq!(
                        r1.len(),
                        r2.len(),
                        "case {}: column count differs at row {}",
                        case_no,
                        i
                    );
                    for (j, (v1, v2)) in r1.iter().zip(r2.iter()).enumerate() {
                        assert!(
                            values_match(v1, v2),
                            "case {}: value divergence at row {} col {}: ours={:?} sqlite={:?} | SQL: {:?}",
                            case_no, i, j, v1, v2, select
                        );
                    }
                }
            }
            (None, None) => { /* both failed — agreement */ }
            (ours, theirs) => {
                panic!(
                    "case {}: success/failure divergence for {:?}: ours={:?} sqlite={:?}",
                    case_no,
                    select,
                    ours.as_ref().map(|r| r.len()),
                    theirs.as_ref().map(|r| r.len())
                );
            }
        }
    }
}
