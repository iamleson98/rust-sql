//! INDEXING intensive stress — differential against real SQLite, plus
//! engine-internal cross-validation at scale.
//!
//! SQLite's own methodology (TH3's index cross-check + the
//! `index-verification` scripts): every indexed query must agree with
//! the un-indexed scan of the same table, and with real SQLite on the
//! same data. This suite drives that contract through:
//!
//!   1. The index flavor zoo (single, multi-col, DESC, expression,
//!      partial, COLLATE NOCASE, UNIQUE) over mixed-type data.
//!   2. Churn correctness: interleaved insert/update/delete storms on
//!      indexed columns with periodic full-state agreement + unique
//!      violation parity (exact messages).
//!   3. Cross-validation at scale: a big table (env-gated) where a
//!      battery of predicates is answered BOTH via the index and via a
//!      no-index clone — the two engine answers must match exactly.
//!   4. Index metadata parity: index_list / index_info / index_xinfo
//!      (PRAGMA + TVF forms), sqlite_master visibility, DROP/CREATE
//!      cycles.
//!   5. Overflow keys: multi-KB indexed values churned through the
//!      overflow-page machinery.
//!   6. Randomized seeded workloads: random DDL (create/drop indexes
//!      mid-stream), random DML, random probes, integrity_check each
//!      phase.
//!
//! Env knobs (CI cranks these; defaults keep `cargo test` quick):
//!   INDEX_STRESS_ROWS   — rows in the scale phases   (default 4000)
//!   INDEX_STRESS_SEEDS  — randomized workload seeds  (default 8)
//!   INDEX_STRESS_OPS    — ops per randomized seed    (default 150)

use rusqlite::Connection;

// ---------------------------------------------------------------------------
// Differential helpers (same conventions as tests/without_rowid_pk.rs).
// ---------------------------------------------------------------------------

fn clean(s: &str) -> String {
    let s = s.strip_prefix("error: ").unwrap_or(s);
    if let Some(pos) = s.find(" in SELECT ") {
        if s[pos..].contains(" at offset ") {
            return s[..pos].to_string();
        }
    }
    s.to_string()
}

fn both(db: &mut rustqlite::Database, rc: &Connection, sql: &str) {
    let e1 = db.execute(sql, ()).map_err(|e| e.to_string());
    let e2 = rc.execute_batch(sql).map_err(|e| clean(&e.to_string()));
    assert_eq!(
        e1, e2,
        "engine/SQLite disagree on: {sql}\n  engine: {e1:?}\n  sqlite: {e2:?}"
    );
}

fn sv(v: rusqlite::types::ValueRef<'_>) -> rustqlite::Value {
    match v {
        rusqlite::types::ValueRef::Null => rustqlite::Value::Null,
        rusqlite::types::ValueRef::Integer(v) => rustqlite::Value::Integer(v),
        rusqlite::types::ValueRef::Real(v) => rustqlite::Value::Real(v),
        rusqlite::types::ValueRef::Text(t) => {
            rustqlite::Value::Text(String::from_utf8_lossy(t).to_string().into())
        }
        rusqlite::types::ValueRef::Blob(b) => rustqlite::Value::Blob(b.to_vec()),
    }
}

fn engine_rows(db: &rustqlite::Database, sql: &str) -> Vec<rustqlite::Value> {
    db.query(sql, ())
        .unwrap_or_else(|e| panic!("engine query failed: {sql}: {e}"))
        .into_iter()
        .flatten()
        .collect()
}

fn sqlite_rows(rc: &Connection, sql: &str) -> Vec<rustqlite::Value> {
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

fn q_both(db: &rustqlite::Database, rc: &Connection, sql: &str) {
    let r1 = engine_rows(db, sql);
    let r2 = sqlite_rows(rc, sql);
    assert_eq!(r1, r2, "engine/SQLite rows disagree on: {sql}");
}

fn fresh() -> (rustqlite::Database, Connection) {
    (
        rustqlite::Database::open_in_memory().unwrap(),
        Connection::open_in_memory().unwrap(),
    )
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_ints(db: &rustqlite::Database, sql: &str) -> Vec<i64> {
    engine_rows(db, sql)
        .into_iter()
        .map(|v| match v {
            rustqlite::Value::Integer(i) => i,
            other => panic!("non-int from {sql}: {other:?}"),
        })
        .collect()
}

/// Deterministic xorshift RNG (same family as tests/stateful_fuzz.rs).
struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed | 1)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

// ===========================================================================
// 1. Index flavor zoo
// ===========================================================================

#[test]
fn index_flavor_zoo() {
    let (mut db, rc) = fresh();
    both(&mut db, &rc, "CREATE TABLE ix (a INT, b TEXT, c REAL)");
    for sql in [
        "CREATE INDEX ixa ON ix (a)",
        "CREATE UNIQUE INDEX ixb ON ix (b)",
        "CREATE INDEX ixc ON ix (a DESC, b ASC)",
        "CREATE INDEX ixp ON ix (a) WHERE b IS NOT NULL",
        "CREATE INDEX ixe ON ix (a * 2)",
        "CREATE INDEX ixco ON ix (b COLLATE NOCASE)",
        "CREATE INDEX ixm ON ix (b, c, a)",
    ] {
        both(&mut db, &rc, sql);
    }
    // Mixed-type rows: numbers, text, NULLs, a blob — the storage-class
    // ordering corners the index must respect.
    both(
        &mut db,
        &rc,
        "INSERT INTO ix VALUES (1, 'x', 1.5), (2, 'y', 2.5), (NULL, 'z', NULL), (5, 'X', 0.5), ('7', 'w', 7.25)",
    );
    // The query battery: equality, ranges, IN, LIKE prefix, ORDER BY,
    // MIN/MAX, GROUP BY — every one answered through (or declined by)
    // each index flavor.
    for sql in [
        "SELECT a FROM ix WHERE a = 5",
        "SELECT a FROM ix WHERE a = '7'",
        "SELECT a FROM ix WHERE a IN (1, 2, 5)",
        "SELECT * FROM ix WHERE a BETWEEN 1 AND 5",
        "SELECT b FROM ix WHERE b = 'x'",
        "SELECT b FROM ix WHERE b = 'X'", // NOCASE vs BINARY
        "SELECT b FROM ix WHERE b LIKE 'x%'",
        "SELECT a FROM ix ORDER BY a",
        "SELECT a FROM ix ORDER BY a DESC",
        "SELECT b, c FROM ix ORDER BY b, c",
        "SELECT min(a), max(a) FROM ix",
        "SELECT min(b), max(b) FROM ix",
        "SELECT b, count(*) FROM ix GROUP BY b ORDER BY b",
        "SELECT a * 2 FROM ix WHERE a * 2 = 4", // expression index probe
        "SELECT count(*) FROM ix WHERE b IS NOT NULL", // partial domain
        "SELECT count(*) FROM ix WHERE b IS NOT NULL AND a > 0",
        "SELECT count(*) FROM ix WHERE b IS NULL",
    ] {
        q_both(&db, &rc, sql);
    }
    // Collated index: case-insensitive equality finds both 'x' and 'X'.
    q_both(
        &db,
        &rc,
        "SELECT count(*) FROM ix WHERE b = 'x' COLLATE NOCASE",
    );
    // NULL keys: indexed column NULLs are indexed (findable).
    q_both(&db, &rc, "SELECT c FROM ix WHERE a IS NULL");
    // DROP + recreate on live data.
    both(&mut db, &rc, "DROP INDEX ixa");
    q_both(&db, &rc, "SELECT a FROM ix WHERE a = 5");
    both(&mut db, &rc, "CREATE INDEX ixa ON ix (a)");
    q_both(&db, &rc, "SELECT a FROM ix WHERE a = 5");
    // Duplicated CREATE / unknown DROP: error parity.
    both(&mut db, &rc, "CREATE INDEX ixa ON ix (a)");
    both(&mut db, &rc, "DROP INDEX nosuch");
    both(&mut db, &rc, "CREATE INDEX nosuch ON nosuchtable (x)");
    // Integrity after the whole zoo.
    q_both(&db, &rc, "PRAGMA integrity_check");
}

// ===========================================================================
// 2. Churn correctness with unique-violation parity
// ===========================================================================

#[test]
fn unique_index_churn() {
    let n = env_u64("INDEX_STRESS_ROWS", 3000);
    let (mut db, rc) = fresh();
    both(&mut db, &rc, "CREATE TABLE t (a INT, b TEXT, v REAL)");
    both(&mut db, &rc, "CREATE UNIQUE INDEX iu_ab ON t (a, b)");
    both(&mut db, &rc, "CREATE INDEX iv ON t (v)");
    // Seeded storm over a colliding key space: inserts (plain, OR
    // IGNORE, OR REPLACE, upsert), updates that move indexed keys,
    // deletes — the engine must match SQLite's state after every phase.
    let mut rng = Rng::new(0xA11C_E55E_0000_0001);
    for i in 0..n {
        let a = (rng.below(60)) as i64;
        let b = format!("k{:02}", rng.below(40));
        let v = (rng.next_u64() % 10000) as f64 / 8.0;
        let stmt = match i % 9 {
            0 => format!("INSERT INTO t VALUES ({a}, '{b}', {v})"),
            1 => format!("INSERT OR IGNORE INTO t VALUES ({a}, '{b}', {v})"),
            2 => format!("INSERT OR REPLACE INTO t VALUES ({a}, '{b}', {v})"),
            3 => format!(
                "INSERT INTO t VALUES ({a}, '{b}', {v}) ON CONFLICT (a, b) DO UPDATE SET v = v + 1"
            ),
            4 => format!("UPDATE t SET a = a + 100 WHERE b = '{b}'"),
            5 => format!("UPDATE t SET b = 'zz{i}' WHERE a = {a}"),
            6 => format!("DELETE FROM t WHERE a = {a} AND b = '{b}'"),
            7 => format!("UPDATE t SET v = v * 2 WHERE a = {a}"),
            _ => format!("DELETE FROM t WHERE v > {v}"),
        };
        both(&mut db, &rc, &stmt);
        if i % 97 == 0 {
            q_both(&db, &rc, "SELECT count(*) FROM t");
            q_both(&db, &rc, "SELECT min(a), max(a) FROM t");
        }
    }
    // Final full-state agreement (stable orders over each index).
    q_both(&db, &rc, "SELECT a, b, v FROM t ORDER BY a, b");
    q_both(&db, &rc, "SELECT v FROM t ORDER BY v");
    q_both(&db, &rc, "PRAGMA integrity_check");
    // Every violation message is exact.
    both(&mut db, &rc, "INSERT INTO t SELECT a, b, v FROM t LIMIT 1");
    // Failed multi-row batch leaves no partial state.
    both(
        &mut db,
        &rc,
        "INSERT INTO t VALUES (9999, 'fresh', 1), (10000, 'fresh2', 2), (9999, 'fresh', 3)",
    );
    q_both(&db, &rc, "SELECT count(*) FROM t WHERE a >= 9999");
    q_both(&db, &rc, "PRAGMA integrity_check");
}

// ===========================================================================
// 3. Cross-validation at scale (indexed vs un-indexed, engine-internal)
// ===========================================================================

#[test]
fn index_cross_validation_at_scale() {
    let n = env_u64("INDEX_STRESS_ROWS", 4000) as i64;
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (a INT, b TEXT, c REAL)", ())
        .unwrap();
    db.execute("CREATE INDEX i_a ON t (a)", ()).unwrap();
    db.execute("CREATE INDEX i_b ON t (b)", ()).unwrap();
    db.execute("CREATE INDEX i_ab ON t (a, b)", ()).unwrap();
    // Deterministic mixed-type data: 1/10 NULLs, some text-numbers.
    let mut rng = Rng::new(0xC0FF_EE00_0000_1234);
    for i in 0..n {
        let a = if i % 10 == 0 {
            "NULL".to_string()
        } else if i % 7 == 0 {
            format!("'{}'", i) // text number — ordering corner
        } else {
            format!("{}", (rng.next_u64() % 1_000_000) as i64)
        };
        let b = if i % 13 == 0 {
            "NULL".to_string()
        } else {
            format!("'s{:05}'", (rng.next_u64() % 5000) as i64)
        };
        let c = format!("{:.3}", (rng.next_u64() % 1_000_000) as f64 / 64.0);
        db.execute(&format!("INSERT INTO t VALUES ({a}, {b}, {c})"), ())
            .unwrap();
    }
    // The SAME data in a no-index clone (identical rowids).
    db.execute("CREATE TABLE u AS SELECT rowid AS rid, * FROM t", ())
        .unwrap();

    // Battery: for every predicate, the indexed answer (table t) must
    // equal the no-index answer (table u) — the TH3 cross-check.
    let probes: Vec<String> = [
        "SELECT count(*) FROM {t} WHERE a = {v}",
        "SELECT count(*) FROM {t} WHERE a BETWEEN {lo} AND {hi}",
        "SELECT count(*) FROM {t} WHERE b = '{s}'",
        "SELECT count(*) FROM {t} WHERE b LIKE 's123%'",
        "SELECT count(*) FROM {t} WHERE b BETWEEN 's0100' AND 's2000'",
        "SELECT count(*) FROM {t} WHERE a IS NULL",
        "SELECT count(*) FROM {t} WHERE b IS NULL",
        "SELECT count(*) FROM {t} WHERE a IN ({v}, {v2}, 999999)",
    ]
    .iter()
    .map(|p| p.to_string())
    .collect();
    let mut rr = Rng::new(0x5EED_ABCD_0000_0009);
    for probe in &probes {
        for _ in 0..40 {
            let v = (rr.next_u64() % 1_000_000) as i64;
            let v2 = (rr.next_u64() % 1_000_000) as i64;
            let lo = (rr.next_u64() % 900_000) as i64;
            let hi = lo + (rr.next_u64() % 100_000) as i64;
            let s = format!("s{:05}", rr.below(5000));
            let sql_t = probe
                .replace("{t}", "t")
                .replace("{v2}", &v2.to_string())
                .replace("{lo}", &lo.to_string())
                .replace("{hi}", &hi.to_string())
                .replace("{s}", &s)
                .replace("{v}", &v.to_string());
            let sql_u = sql_t.replace("FROM t", "FROM u");
            let a = env_ints(&db, &sql_t);
            let b = env_ints(&db, &sql_u);
            assert_eq!(a, b, "indexed vs un-indexed disagree: {sql_t}");
        }
    }
    // ORDER BY agreement (rowid order stable in both).
    let a = env_ints(
        &db,
        "SELECT rowid FROM t WHERE a BETWEEN 100 AND 200 ORDER BY a",
    );
    let b = env_ints(
        &db,
        "SELECT rid FROM u WHERE a BETWEEN 100 AND 200 ORDER BY a",
    );
    assert_eq!(a, b);
    // Aggregates.
    for sql in [
        "SELECT min(a), max(a), count(a) FROM t WHERE b LIKE 's1%'",
        "SELECT b, count(*) FROM t WHERE a BETWEEN 0 AND 500000 GROUP BY b ORDER BY count(*) DESC, b LIMIT 20",
    ] {
        let sql_u = sql.replace(" rowid ", " rid ").replace("FROM t", "FROM u");
        let r1 = engine_rows(&db, sql);
        let r2 = engine_rows(&db, &sql_u);
        assert_eq!(r1, r2, "aggregate cross-check: {sql}");
    }
    // Engine integrity + COUNT via the (previously split-prone) index.
    db.execute("PRAGMA integrity_check", ()).unwrap();
    // Churn after the big load: delete a middle band, reinsert, re-check.
    db.execute("DELETE FROM t WHERE a BETWEEN 400000 AND 600000", ())
        .unwrap();
    db.execute(
        "INSERT INTO t SELECT a, b, c FROM u WHERE a BETWEEN 400000 AND 600000",
        (),
    )
    .unwrap();
    let a = env_ints(&db, "SELECT count(*) FROM t");
    let b = env_ints(&db, "SELECT count(*) FROM u");
    assert_eq!(a, b, "churn preserves counts");
    for v in [0, 100000, 500000, 900000, 999999] {
        let a = env_ints(&db, &format!("SELECT count(*) FROM t WHERE a = {v}"));
        let b = env_ints(&db, &format!("SELECT count(*) FROM u WHERE a = {v}"));
        assert_eq!(a, b, "post-churn point probe {v}");
    }
}

// ===========================================================================
// 4. Metadata parity (PRAGMA + TVF forms)
// ===========================================================================

#[test]
fn index_metadata_parity() {
    let (mut db, rc) = fresh();
    both(
        &mut db,
        &rc,
        "CREATE TABLE t (a INT, b TEXT, c REAL, d INT UNIQUE)",
    );
    both(&mut db, &rc, "CREATE INDEX i_a ON t (a)");
    both(&mut db, &rc, "CREATE INDEX i_desc ON t (a DESC, b)");
    both(&mut db, &rc, "CREATE INDEX i_part ON t (c) WHERE a > 0");
    both(&mut db, &rc, "CREATE INDEX i_expr ON t (lower(b))");
    both(&mut db, &rc, "CREATE UNIQUE INDEX i_u ON t (b, c)");
    // sqlite_master: the six index rows, names + rootpage order stable.
    q_both(
        &db,
        &rc,
        "SELECT type, name, tbl_name FROM sqlite_master WHERE type = 'index' ORDER BY name",
    );
    // index_list via PRAGMA and via TVF — identical metadata.
    q_both(&db, &rc, "PRAGMA index_list('t')");
    q_both(
        &db,
        &rc,
        "SELECT name, \"unique\", origin, partial FROM pragma_index_list('t') ORDER BY name",
    );
    // index_info on every flavor (seqno/cid/name).
    for idx in ["i_a", "i_desc", "i_part", "i_u", "sqlite_autoindex_t_1"] {
        q_both(&db, &rc, &format!("PRAGMA index_info('{idx}')"));
        q_both(
            &db,
            &rc,
            &format!("SELECT seqno, cid, name FROM pragma_index_info('{idx}')"),
        );
    }
    // index_xinfo (adds desc/coll/key + the rowid row).
    for idx in ["i_a", "i_desc", "i_u"] {
        q_both(&db, &rc, &format!("PRAGMA index_xinfo('{idx}')"));
    }
    // Rebuild: ANALYZE populates sqlite_stat1 identically.
    both(
        &mut db,
        &rc,
        "INSERT INTO t VALUES (1, 'x', 1.5, 10), (2, 'y', 2.5, 20)",
    );
    both(&mut db, &rc, "ANALYZE");
    q_both(&db, &rc, "SELECT idx, stat FROM sqlite_stat1 ORDER BY idx");
}

// ===========================================================================
// 5. Overflow keys — multi-KB indexed values
// ===========================================================================

#[test]
fn index_overflow_churn() {
    let n = env_u64("INDEX_STRESS_ROWS", 300);
    let (mut db, rc) = fresh();
    both(&mut db, &rc, "CREATE TABLE t (k TEXT, v INT)");
    both(&mut db, &rc, "CREATE INDEX ik ON t (k)");
    both(&mut db, &rc, "CREATE UNIQUE INDEX iu ON t (k)");
    // 4 KB keys: every entry's key spills to overflow chains.
    let pad = "p".repeat(4096);
    for i in 0..n {
        let k = format!("{pad}-{i:06}");
        both(&mut db, &rc, &format!("INSERT INTO t VALUES ('{k}', {i})"));
    }
    // Point probes on overflow keys.
    for i in [0, n / 2, n - 1, 17, 33] {
        let k = format!("{pad}-{i:06}");
        q_both(&db, &rc, &format!("SELECT v FROM t WHERE k = '{k}'"));
        q_both(&db, &rc, &format!("SELECT count(*) FROM t WHERE k = '{k}'"));
    }
    // Ranged scan across overflow keys (ordering by the FULL key).
    let mid = format!("{pad}-{:06}", n / 2);
    q_both(
        &db,
        &rc,
        &format!("SELECT count(*) FROM t WHERE k > '{mid}'"),
    );
    q_both(&db, &rc, "SELECT count(*) FROM t");
    // Churn: replace a band (same-size in-place updates), delete a band,
    // re-insert shorter keys — the chains must stay consistent.
    both(
        &mut db,
        &rc,
        &format!("UPDATE t SET k = '{pad}-{n:06}' WHERE v = 0"),
    ); // unique clash expected? no: fresh suffix
    q_both(&db, &rc, "SELECT count(*) FROM t WHERE k = 'pppp'");
    both(&mut db, &rc, "DELETE FROM t WHERE v % 5 = 0");
    q_both(&db, &rc, "SELECT count(*) FROM t");
    for i in 0..n / 4 {
        let k = format!("short-{i:04}");
        both(
            &mut db,
            &rc,
            &format!("INSERT INTO t VALUES ('{k}', {})", 10_000 + i),
        );
    }
    q_both(&db, &rc, "SELECT count(*) FROM t WHERE k LIKE 'short-%'");
    q_both(&db, &rc, "SELECT count(*) FROM t");
    q_both(&db, &rc, "PRAGMA integrity_check");
    // Final ordered walk: the FULL key ordering (overflow reassembly).
    q_both(&db, &rc, "SELECT v FROM t ORDER BY k LIMIT 25");
    q_both(&db, &rc, "SELECT v FROM t ORDER BY k DESC LIMIT 25");
}

// ===========================================================================
// 6. Randomized seeded workloads (DDL mid-stream + DML + probes)
// ===========================================================================

#[test]
fn index_randomized_workload() {
    let seeds = env_u64("INDEX_STRESS_SEEDS", 8);
    let ops = env_u64("INDEX_STRESS_OPS", 150);
    for s in 0..seeds {
        let mut rng =
            Rng::new(0xDEAD_BEEF_0000_0000u64.wrapping_add(s.wrapping_mul(0x9E37_79B9_7F4A_7C15)));
        let (mut db, rc) = fresh();
        // Base table with a couple of columns and a small key space.
        both(&mut db, &rc, "CREATE TABLE t (a INT, b TEXT, v REAL)");
        both(&mut db, &rc, "INSERT INTO t VALUES (1, 'k1', 1.0)");
        let mut idx_counter = 0u64;
        let mut live: Vec<String> = Vec::new();
        for i in 0..ops {
            let op = rng.below(12);
            let a = (rng.below(25)) as i64;
            let b = format!("k{}", rng.below(20));
            let stmt = match op {
                0..=2 => format!("INSERT INTO t VALUES ({a}, '{b}', {a}.5)"),
                3 => format!("INSERT OR IGNORE INTO t VALUES ({a}, '{b}', {a}.5)"),
                4 => format!("UPDATE t SET v = v + 1 WHERE a = {a}"),
                5 => format!("UPDATE t SET a = {a} WHERE b = '{b}'"),
                6 => format!("DELETE FROM t WHERE a = {a}"),
                7 => {
                    // Random DDL: create a new index flavor.
                    idx_counter += 1;
                    let name = format!("x{idx_counter}");
                    let flavor = match rng.below(5) {
                        0 => format!("CREATE INDEX {name} ON t (a)"),
                        1 => format!("CREATE INDEX {name} ON t (a, b)"),
                        2 => format!("CREATE INDEX {name} ON t (b DESC, v)"),
                        3 => format!("CREATE UNIQUE INDEX {name} ON t (v)"),
                        _ => format!("CREATE INDEX {name} ON t (v) WHERE a IS NOT NULL"),
                    };
                    live.push(name);
                    flavor
                }
                8 if !live.is_empty() => {
                    let name = live.remove(rng.below(live.len() as u64) as usize);
                    format!("DROP INDEX {name}")
                }
                9 => format!("SELECT count(*) FROM t WHERE a = {a}"),
                10 => "SELECT min(a), max(a), count(*) FROM t".to_string(),
                _ => format!("SELECT v FROM t WHERE b = '{b}' AND a = {a}"),
            };
            // Read probes are checked differential; writes too (error
            // parity on unique clashes).
            if op == 9 || op == 10 || op == 11 {
                q_both(&db, &rc, &stmt);
            } else {
                both(&mut db, &rc, &stmt);
            }
            if i % 40 == 0 {
                q_both(&db, &rc, "SELECT count(*) FROM t");
                q_both(&db, &rc, "PRAGMA integrity_check");
            }
        }
        // Final state + every live index consistent.
        q_both(&db, &rc, "SELECT a, b, v FROM t ORDER BY a, b, v");
        q_both(&db, &rc, "PRAGMA integrity_check");
        q_both(
            &db,
            &rc,
            "SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = 't' ORDER BY name",
        );
    }
}

// ===========================================================================
// 7. Multi-page index + split storm (the IndexCount regression class)
// ===========================================================================

#[test]
fn index_split_storm_counts() {
    // The exact shape that found the stale-root IndexCount bug: enough
    // rows to split every index several times, then COUNT + point probes
    // across the WHOLE key range (before, across, and after every split
    // boundary).
    let n = env_u64("INDEX_STRESS_ROWS", 3000) as i64;
    let (mut db, rc) = fresh();
    both(&mut db, &rc, "CREATE TABLE t (k TEXT, v INT)");
    both(&mut db, &rc, "CREATE INDEX ik ON t (k)");
    both(&mut db, &rc, "CREATE UNIQUE INDEX iu ON t (v)");
    for i in 0..n {
        both(
            &mut db,
            &rc,
            &format!("INSERT INTO t VALUES ('k{:06}', {})", i * 7 % n, i),
        );
    }
    // COUNT + lookup + range for keys spanning the full range — forward
    // AND reverse scan order (the hint cache differs).
    for i in (0..n).step_by((n / 64).max(1) as usize) {
        let k = format!("k{:06}", i * 7 % n);
        q_both(&db, &rc, &format!("SELECT count(*) FROM t WHERE k = '{k}'"));
    }
    for i in (0..n).rev().step_by((n / 64).max(1) as usize) {
        let k = format!("k{:06}", i * 7 % n);
        q_both(&db, &rc, &format!("SELECT count(*) FROM t WHERE k = '{k}'"));
    }
    for (lo, hi) in [
        (0, n / 4),
        (n / 4, n / 2),
        (n / 2, 3 * n / 4),
        (3 * n / 4, n),
    ] {
        q_both(
            &db,
            &rc,
            &format!(
                "SELECT count(*) FROM t WHERE k BETWEEN 'k{:06}' AND 'k{:06}'",
                lo * 7 % n,
                hi * 7 % n
            ),
        );
    }
    // The unique index through the same storm.
    for i in (0..n).step_by((n / 32).max(1) as usize) {
        q_both(&db, &rc, &format!("SELECT k FROM t WHERE v = {i}"));
    }
    q_both(&db, &rc, "PRAGMA integrity_check");
    // Reopen (native format): the split tree survives.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("split.db");
    {
        let mut fdb = rustqlite::Database::open(&path).unwrap();
        fdb.execute("CREATE TABLE t (k TEXT, v INT)", ()).unwrap();
        fdb.execute("CREATE INDEX ik ON t (k)", ()).unwrap();
        for i in 0..n {
            fdb.execute(
                &format!("INSERT INTO t VALUES ('k{:06}', {i})", i * 7 % n),
                (),
            )
            .unwrap();
        }
    }
    let fdb2 = rustqlite::Database::open(&path).unwrap();
    let c = env_ints(&fdb2, "SELECT count(*) FROM t WHERE k = 'k000042'");
    assert_eq!(c, vec![1], "post-reopen index lookup");
    let c2 = env_ints(&fdb2, "SELECT count(*) FROM t");
    assert_eq!(c2, vec![n]);
}
