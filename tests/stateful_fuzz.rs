//! Stateful random-workload differential fuzz (SQLite `dbsqlfuzz`-class):
//! generate a random schema, then a long random script of DML + transaction
//! choreography + DDL, execute it against BOTH engines statement by
//! statement, and verify after every step that
//!
//!   1. success/failure agrees (a statement SQLite accepts must work here,
//!      and vice versa),
//!   2. a FAILED statement changes nothing in either engine (statement
//!      atomicity — the class of bug where half a statement lands),
//!   3. the full visible state of every user table (rowid-ordered row
//!      dumps) is IDENTICAL after every step,
//!   4. random query probes (aggregates, GROUP BY, joins, DISTINCT) agree
//!      row-for-row,
//!   5. `PRAGMA integrity_check` stays `ok` on our side throughout.
//!
//! Everything is seeded: `RUSTQLITE_STATEFUL_SEED`. On divergence the
//! failing script is printed verbatim for replay. Iterations are env-tuned
//! (`RUSTQLITE_STATEFUL_CASES` / `RUSTQLITE_STATEFUL_OPS`) so the default
//! matrix stays fast while the dedicated CI fuzz job runs it hard.

use rusqlite::types::Value as Sv;
use rustqlite::{Database, Value};

// ---------------------------------------------------------------------------
// Seeded RNG (xorshift64*, same shape as tests/sql_fuzz.rs)
// ---------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
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

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

// ---------------------------------------------------------------------------
// Cross-engine value comparison (numeric cross-type tolerance, like the
// structured fuzz — read-back semantic equality is the contract)
// ---------------------------------------------------------------------------

fn values_match(a: &Value, b: &Sv) -> bool {
    match (a, b) {
        (Value::Null, Sv::Null) => true,
        (Value::Integer(x), Sv::Integer(y)) => x == y,
        (Value::Integer(x), Sv::Real(y)) => (*x as f64 - y).abs() <= 1e-9 * y.abs().max(1.0),
        (Value::Real(x), Sv::Integer(y)) => (*x - *y as f64).abs() <= 1e-9 * x.abs().max(1.0),
        (Value::Real(x), Sv::Real(y)) => {
            // `x == y` first: ±inf pairs must match (inf - inf = NaN made
            // the epsilon test reject them — a FALSE divergence on
            // overflow-to-inf sums), and exact equality is free.
            (x.is_nan() && y.is_nan())
                || x == y
                || (x - y).abs() <= 1e-9 * x.abs().max(y.abs()).max(1.0)
        }
        (Value::Text(x), Sv::Text(y)) => x.as_str() == y,
        (Value::Blob(x), Sv::Blob(y)) => x == y,
        _ => false,
    }
}

fn rows_match(where_: &str, ours: &[Vec<Value>], theirs: &[Vec<Sv>]) -> Result<(), String> {
    if ours.len() != theirs.len() {
        return Err(format!(
            "{}: row count {} vs sqlite {}",
            where_,
            ours.len(),
            theirs.len()
        ));
    }
    for (i, (r1, r2)) in ours.iter().zip(theirs.iter()).enumerate() {
        if r1.len() != r2.len() {
            return Err(format!(
                "{}: col count row {}: {} vs {}",
                where_,
                i,
                r1.len(),
                r2.len()
            ));
        }
        for (j, (v1, v2)) in r1.iter().zip(r2.iter()).enumerate() {
            if !values_match(v1, v2) {
                return Err(format!(
                    "{}: row {} col {}: ours={:?} sqlite={:?}",
                    where_, i, j, v1, v2
                ));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Statement routing + execution on both engines
// ---------------------------------------------------------------------------

fn is_queryish(sql: &str) -> bool {
    let head = sql.trim_start().to_ascii_uppercase();
    head.starts_with("SELECT")
        || head.starts_with("WITH")
        || head.starts_with("VALUES")
        || head.starts_with("PRAGMA")
}

/// Run one statement on our engine; Err = the engine rejected it.
fn our_run(db: &mut Database, sql: &str) -> Result<Vec<Vec<Value>>, String> {
    if is_queryish(sql) {
        db.query(sql, []).map_err(|e| e.to_string())
    } else {
        db.execute(sql, [])
            .map(|_| Vec::new())
            .map_err(|e| e.to_string())
    }
}

/// Run one statement on SQLite; Err = SQLite rejected it.
fn sq_run(conn: &rusqlite::Connection, sql: &str) -> Result<Vec<Vec<Sv>>, String> {
    let mut stmt = conn.prepare(sql).map_err(|e| format!("prepare: {}", e))?;
    let ncols = stmt.column_count();
    if ncols == 0 {
        return stmt
            .execute([])
            .map(|_| Vec::new())
            .map_err(|e| format!("execute: {}", e));
    }
    let mut rows = stmt.query([]).map_err(|e| format!("query: {}", e))?;
    let mut out = Vec::new();
    // A mid-iteration ERROR (e.g. sum()'s "integer overflow") is an
    // error, not end-of-rows: `while let Ok(Some)` silently truncated
    // at the failure and made a failing SQLite query look successful.
    loop {
        match rows.next() {
            Ok(Some(row)) => {
                let mut r = Vec::with_capacity(ncols);
                for i in 0..ncols {
                    r.push(row.get(i).unwrap_or(Sv::Null));
                }
                out.push(r);
            }
            Ok(None) => break,
            Err(e) => return Err(format!("query: {}", e)),
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Random schema generation
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Col {
    name: String,
    ty: &'static str,
}

#[derive(Clone)]
struct Table {
    name: String,
    cols: Vec<Col>,
    /// INTEGER PRIMARY KEY rowid-alias column index, if any.
    alias_pk: Option<usize>,
    /// WITHOUT ROWID: primary key column indices (dump orders by these).
    worowid_pk: Option<Vec<usize>>,
    /// ALTER ADD COLUMN budget.
    alters_left: usize,
}

struct Schema {
    tables: Vec<Table>,
    indexes: Vec<String>,
    triggers: Vec<String>,
    allow_triggers: bool,
}

const COL_NAMES: &[&str] = &["a", "b", "c", "d", "e", "f"];
const TYPES: &[&str] = &["INTEGER", "REAL", "TEXT", "BLOB"];

fn gen_col_def(rng: &mut Rng, used: &mut Vec<String>) -> Col {
    let base = COL_NAMES[rng.below(COL_NAMES.len())];
    let name = if used.iter().any(|u| u == base) {
        let n = used.len() + 1;
        used.push(format!("{base}{n}"));
        format!("{base}{n}")
    } else {
        used.push(base.to_string());
        base.to_string()
    };
    Col {
        name,
        ty: TYPES[rng.below(TYPES.len())],
    }
}

fn gen_schema(rng: &mut Rng, _audit: bool) -> Schema {
    let allow_triggers = rng.chance(60);
    let n_tables = 1 + rng.below(3);
    let mut tables = Vec::new();
    for t in 0..n_tables {
        let shape = rng.below(10);
        let ncols = 2 + rng.below(4);
        let mut used: Vec<String> = Vec::new();
        let mut cols = Vec::new();
        let mut alias_pk = None;
        let mut worowid_pk = None;
        let mut ddl_cols = String::new();
        for i in 0..ncols {
            let c = gen_col_def(rng, &mut used);
            let mut def = format!("{} {}", c.name, c.ty);
            if rng.chance(12) {
                def.push_str(" NOT NULL");
            }
            if rng.chance(10) {
                let lit = match c.ty {
                    "INTEGER" => rng.range(-5, 5).to_string(),
                    "REAL" => format!("{:.1}", rng.range(-9, 9) as f64 / 2.0),
                    "TEXT" => format!("'d{}'", rng.below(9)),
                    _ => "x'00'".to_string(),
                };
                def.push_str(&format!(" DEFAULT {}", lit));
            }
            if rng.chance(8) {
                def.push_str(" UNIQUE");
            }
            if rng.chance(6) {
                def.push_str(&format!(" CHECK ({} >= -1000000)", c.name));
            }
            if i > 0 {
                ddl_cols.push_str(", ");
            }
            ddl_cols.push_str(&def);
            cols.push(c);
        }
        // Table shape: alias PK / WITHOUT ROWID PK / plain rowid table.
        let mut ddl_tail = String::new();
        if shape < 4 {
            // INTEGER PRIMARY KEY alias: fold into the first INTEGER col
            // (or add one).
            let idx = cols
                .iter()
                .position(|c| c.ty == "INTEGER")
                .unwrap_or_else(|| {
                    cols.push(Col {
                        name: "pk".to_string(),
                        ty: "INTEGER",
                    });
                    cols.len() - 1
                });
            alias_pk = Some(idx);
            let name = cols[idx].name.clone();
            // rebuild DDL with the alias clause on that column
            ddl_cols = cols
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    let mut def = format!("{} {}", c.name, c.ty);
                    if i == idx {
                        def.push_str(" PRIMARY KEY");
                        if rng.chance(15) {
                            def.push_str(" AUTOINCREMENT");
                        }
                    }
                    def
                })
                .collect::<Vec<_>>()
                .join(", ");
            let _ = name;
        } else if shape < 6 {
            // WITHOUT ROWID: single- or double-column PK. The keywords are
            // REQUIRED: `CREATE TABLE t (... PRIMARY KEY (f))` without them
            // is a ROWID table whose INTEGER pk column is the rowid ALIAS
            // (SQLite quirk) — NULL-pk inserts then auto-allocate rowids,
            // and once the max hits i64::MAX both engines go RANDOM,
            // which the harness model (worowid_pk = skip rowid checks)
            // can never verify. Three deep-sweep seeds died on this.
            let k = 1 + rng.below(2);
            let mut pk = Vec::new();
            for _ in 0..k {
                pk.push(rng.below(cols.len()));
            }
            pk.sort();
            pk.dedup();
            worowid_pk = Some(pk.clone());
            let pk_list = pk
                .iter()
                .map(|&i| cols[i].name.clone())
                .collect::<Vec<_>>()
                .join(", ");
            ddl_tail = format!(", PRIMARY KEY ({}) WITHOUT ROWID", pk_list);
        } else if shape == 6 {
            // Rowid alias via the SEPARATE-clause form: `f INTEGER, ...,
            // PRIMARY KEY (f)` — SQLite treats the INTEGER pk column as
            // the rowid alias (lang_createtable: "the column is an alias
            // for the rowid"). Same model as the column-level alias
            // shape: rowid dumps, arming checks, the i64::MAX guard.
            let idx = cols
                .iter()
                .position(|c| c.ty == "INTEGER")
                .unwrap_or_else(|| {
                    cols.push(Col {
                        name: "pk".to_string(),
                        ty: "INTEGER",
                    });
                    cols.len() - 1
                });
            alias_pk = Some(idx);
            let name = cols[idx].name.clone();
            ddl_tail = format!(", PRIMARY KEY ({})", name);
        }
        let name = format!("t{t}");
        let ddl = format!("CREATE TABLE {} ({}{})", name, ddl_cols, ddl_tail);
        tables.push(Table {
            name,
            cols,
            alias_pk,
            worowid_pk,
            alters_left: 3,
        });
        SCHEMA_DDL.with(|c| c.borrow_mut().push(ddl));
    }
    // The audit table always exists (trigger bodies write to it).
    {
        let ddl = "CREATE TABLE audit (id INTEGER PRIMARY KEY, note TEXT)";
        tables.push(Table {
            name: "audit".to_string(),
            cols: vec![
                Col {
                    name: "id".to_string(),
                    ty: "INTEGER",
                },
                Col {
                    name: "note".to_string(),
                    ty: "TEXT",
                },
            ],
            alias_pk: Some(0),
            worowid_pk: None,
            alters_left: 0,
        });
        SCHEMA_DDL.with(|c| c.borrow_mut().push(ddl.to_string()));
    }
    Schema {
        tables,
        indexes: Vec::new(),
        triggers: Vec::new(),
        allow_triggers,
    }
}

thread_local! {
    static SCHEMA_DDL: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

// ---------------------------------------------------------------------------
// Random value generation (edge-flavored)
// ---------------------------------------------------------------------------

const EXTREME_REALS: &[&str] = &[
    "1e308",
    "-1e308",
    "9.2233720368547758e18",
    "-9.2233720368547758e18",
    "2.2250738585072014e-308",
    "5e-324",
    "0.0",
    "-0.0",
    "1.5",
    "-2.25",
];

fn gen_int_lit(rng: &mut Rng) -> String {
    match rng.below(20) {
        0 => "0".into(),
        1 => "1".into(),
        2 => (-1).to_string(),
        3 => i64::MAX.to_string(),
        4 => i64::MIN.to_string(),
        5 => (1i64 << 53).to_string(),
        6 => (-(1i64 << 53)).to_string(),
        7 => (1i64 << 62).to_string(),
        _ => rng.range(-60, 60).to_string(),
    }
}

fn gen_real_lit(rng: &mut Rng) -> String {
    if rng.chance(35) {
        EXTREME_REALS[rng.below(EXTREME_REALS.len())].to_string()
    } else {
        format!("{:.3}", rng.range(-999, 999) as f64 / 7.0)
    }
}

const TEXT_POOL: &[&str] = &[
    "alpha",
    "beta",
    "gamma",
    "delta",
    "zeta",
    "café",
    "naïve",
    "日本語",
    "🦀🚀",
    "line\nbreak",
    "tab\there",
    "NULL",
    "0",
    "x'y",
    "  padded  ",
];

fn gen_text_lit(rng: &mut Rng) -> String {
    if rng.chance(6) {
        return "''".into();
    }
    let s = if rng.chance(8) {
        "L".repeat(120 + rng.below(80))
    } else {
        TEXT_POOL[rng.below(TEXT_POOL.len())].to_string()
    };
    format!("'{}'", s.replace('\'', "''"))
}

fn gen_blob_lit(rng: &mut Rng) -> String {
    match rng.below(6) {
        0 => "x''".into(),
        1 => "x'DEADBEEF'".into(),
        2 => "x'00FF00'".into(),
        3 => "x'0000000000'".into(),
        _ => {
            let n = 1 + rng.below(40);
            let mut hex = String::with_capacity(n * 2);
            for _ in 0..n {
                hex.push_str(&format!("{:02X}", rng.below(256) as u8));
            }
            format!("x'{hex}'")
        }
    }
}

fn gen_lit_for(rng: &mut Rng, ty: &str) -> String {
    if rng.chance(14) {
        return "NULL".into();
    }
    match ty {
        "INTEGER" => gen_int_lit(rng),
        "REAL" => gen_real_lit(rng),
        "TEXT" => gen_text_lit(rng),
        _ => gen_blob_lit(rng),
    }
}

/// Literal for a specific column of a table. The rowid-alias column
/// never receives exactly i64::MAX: a table whose top rowid is MAX arms
/// SQLite's random-rowid lottery for later auto-assignments — both
/// engines then pick (correctly) DIFFERENT random rowids, and the exact
/// state comparison can only flake. MAX - 1 still exercises the extreme
/// alias values; the lottery itself is covered by dedicated engine tests.
fn gen_lit_for_col(rng: &mut Rng, t: &Table, i: usize) -> String {
    let lit = gen_lit_for(rng, t.cols[i].ty);
    if Some(i) == t.alias_pk && lit == i64::MAX.to_string() {
        return (i64::MAX - 1).to_string();
    }
    lit
}

fn gen_any_lit(rng: &mut Rng) -> String {
    match rng.below(4) {
        0 => gen_int_lit(rng),
        1 => gen_real_lit(rng),
        2 => gen_text_lit(rng),
        _ => gen_blob_lit(rng),
    }
}

// ---------------------------------------------------------------------------
// Failure reporting: full script on any divergence
// ---------------------------------------------------------------------------

struct Ctx {
    seed: u64,
    case: usize,
    script: Vec<String>,
}

fn fail(ctx: &Ctx, stmt_idx: usize, stmt: &str, msg: &str) -> String {
    let mut out = format!(
        "\n=== STATEFUL FUZZ DIVERGENCE ===\nseed={} case={} stmt #{}: {}\n  SQL: {}\n--- script ---\n",
        ctx.seed, ctx.case, stmt_idx, msg, stmt
    );
    for (i, s) in ctx.script.iter().enumerate() {
        out.push_str(&format!("{:4}: {}\n", i, s));
    }
    out.push_str("--- end script ---");
    out
}

// ---------------------------------------------------------------------------
// Predicate generation over a table's columns (+ rowid)
// ---------------------------------------------------------------------------

fn gen_predicate(rng: &mut Rng, t: &Table) -> String {
    if t.worowid_pk.is_none() && rng.chance(25) {
        return format!(
            "rowid BETWEEN {} AND {}",
            rng.range(-5, 80),
            rng.range(-5, 120)
        );
    }
    let c = &t.cols[rng.below(t.cols.len())].name;
    match rng.below(5) {
        0 => format!("{} = {}", c, gen_any_lit(rng)),
        1 => format!("{} IS NULL", c),
        2 => format!(
            "{} IN ({}, {}, {})",
            c,
            gen_any_lit(rng),
            gen_any_lit(rng),
            gen_any_lit(rng)
        ),
        3 => format!("{} > {}", c, gen_any_lit(rng)),
        _ => format!("{} IS NOT NULL AND {} <> {}", c, c, gen_any_lit(rng)),
    }
}

// ---------------------------------------------------------------------------
// Operation generation (one per iteration; transaction choreography is
// interleaved as its own statements)
// ---------------------------------------------------------------------------

enum Op {
    Stmt(String),
}

fn gen_insert(rng: &mut Rng, t: &Table) -> String {
    let nrows = 1 + rng.below(4);
    // Column subset: sometimes the alias id (with edge ids), never the
    // WITHOUT-ROWID PK omissions (all cols required there for simplicity:
    // always list every column for worowid tables).
    let list_all = t.worowid_pk.is_some() || rng.chance(40);
    let col_idxs: Vec<usize> = if list_all {
        (0..t.cols.len()).collect()
    } else {
        let k = 1 + rng.below(t.cols.len());
        let mut v: Vec<usize> = (0..t.cols.len()).collect();
        // keep stable random subset
        for i in (1..v.len()).rev() {
            let j = rng.below(i + 1);
            v.swap(i, j);
        }
        v.truncate(k);
        v.sort();
        if t.alias_pk.is_some() && rng.chance(55) {
            if let Some(pos) = v.iter().position(|&i| Some(i) == t.alias_pk) {
                v.remove(pos);
            }
        }
        v
    };
    let names = col_idxs
        .iter()
        .map(|&i| t.cols[i].name.clone())
        .collect::<Vec<_>>()
        .join(", ");
    let mut rows_sql = String::new();
    for r in 0..nrows {
        if r > 0 {
            rows_sql.push_str(", ");
        }
        let mut vals = String::new();
        for (k, &i) in col_idxs.iter().enumerate() {
            if k > 0 {
                vals.push_str(", ");
            }
            vals.push_str(&gen_lit_for_col(rng, t, i));
        }
        rows_sql.push_str(&format!("({})", vals));
    }
    format!("INSERT INTO {} ({}) VALUES {}", t.name, names, rows_sql)
}

fn gen_conflict_insert(rng: &mut Rng, t: &Table) -> Option<String> {
    // Only for alias-PK tables (id known to be a conflict key).
    let idc = t.alias_pk?;
    let id = rng.range(1, 50);
    let cols: Vec<usize> = (0..t.cols.len()).filter(|&i| i != idc).collect();
    if cols.is_empty() {
        return None;
    }
    let pick = cols[rng.below(cols.len())];
    let names = (0..t.cols.len())
        .map(|i| t.cols[i].name.clone())
        .collect::<Vec<_>>()
        .join(", ");
    let mut vals = Vec::new();
    for i in 0..t.cols.len() {
        vals.push(if i == idc {
            id.to_string()
        } else {
            gen_lit_for(rng, t.cols[i].ty)
        });
    }
    match rng.below(4) {
        0 => Some(format!(
            "INSERT OR IGNORE INTO {} ({}) VALUES ({})",
            t.name,
            names,
            vals.join(", ")
        )),
        1 => Some(format!(
            "INSERT OR REPLACE INTO {} ({}) VALUES ({})",
            t.name,
            names,
            vals.join(", ")
        )),
        2 => Some(format!(
            "INSERT INTO {} ({}) VALUES ({}) ON CONFLICT({}) DO NOTHING",
            t.name,
            names,
            vals.join(", "),
            t.cols[idc].name
        )),
        _ => {
            let cname = t.cols[pick].name.clone();
            Some(format!(
                "INSERT INTO {} ({}) VALUES ({}) ON CONFLICT({}) DO UPDATE SET {} = excluded.{}",
                t.name,
                names,
                vals.join(", "),
                t.cols[idc].name,
                cname,
                cname
            ))
        }
    }
}

fn gen_update(rng: &mut Rng, t: &Table) -> String {
    let k = 1 + rng.below(2.min(t.cols.len()));
    let mut idxs: Vec<usize> = (0..t.cols.len()).collect();
    for i in (1..idxs.len()).rev() {
        let j = rng.below(i + 1);
        idxs.swap(i, j);
    }
    idxs.truncate(k);
    idxs.sort();
    let sets = idxs
        .iter()
        .map(|&i| format!("{} = {}", t.cols[i].name, gen_lit_for_col(rng, t, i)))
        .collect::<Vec<_>>()
        .join(", ");
    if rng.chance(5) {
        format!("UPDATE {} SET {}", t.name, sets)
    } else {
        format!(
            "UPDATE {} SET {} WHERE {}",
            t.name,
            sets,
            gen_predicate(rng, t)
        )
    }
}

fn gen_delete(rng: &mut Rng, t: &Table) -> String {
    if rng.chance(5) {
        format!("DELETE FROM {}", t.name)
    } else {
        format!("DELETE FROM {} WHERE {}", t.name, gen_predicate(rng, t))
    }
}

fn gen_op(rng: &mut Rng, sc: &mut Schema, idx_counter: &mut usize) -> Option<Op> {
    // DML weight table. DDL ops are only emitted outside transactions
    // (checked by caller via Op::BeginTxn sequencing).
    let t = &sc.tables[rng.below(sc.tables.len())];
    let pick = rng.below(100);
    if pick < 32 {
        return Some(Op::Stmt(gen_insert(rng, t)));
    }
    if pick < 40 {
        if let Some(sql) = gen_conflict_insert(rng, t) {
            return Some(Op::Stmt(sql));
        }
        return Some(Op::Stmt(gen_insert(rng, t)));
    }
    if pick < 56 {
        return Some(Op::Stmt(gen_update(rng, t)));
    }
    if pick < 68 {
        return Some(Op::Stmt(gen_delete(rng, t)));
    }
    if pick < 72 && sc.indexes.len() < 6 {
        let t2 = &sc.tables[rng.below(sc.tables.len())];
        let c = &t2.cols[rng.below(t2.cols.len())].name;
        let name = format!("ix{}", *idx_counter);
        *idx_counter += 1;
        let shape = rng.below(4);
        let sql = match shape {
            0 => format!("CREATE INDEX {} ON {}({})", name, t2.name, c),
            1 => format!("CREATE INDEX {} ON {}({} DESC)", name, t2.name, c),
            2 => format!(
                "CREATE UNIQUE INDEX {} ON {}({}) WHERE {} IS NOT NULL",
                name, t2.name, c, c
            ),
            _ => format!("CREATE INDEX {} ON {}({}, rowid)", name, t2.name, c),
        };
        sc.indexes.push(name);
        return Some(Op::Stmt(sql));
    }
    if pick < 75 && !sc.indexes.is_empty() {
        let i = rng.below(sc.indexes.len());
        let name = sc.indexes.remove(i);
        return Some(Op::Stmt(format!("DROP INDEX {}", name)));
    }
    if pick < 78 {
        // ALTER TABLE ADD COLUMN on a table with budget.
        let ti = rng.below(sc.tables.len());
        if sc.tables[ti].alters_left == 0 {
            return None;
        }
        sc.tables[ti].alters_left -= 1;
        let t = &sc.tables[ti];
        let n = format!("added{}", 4 - sc.tables[ti].alters_left);
        let ty = TYPES[rng.below(TYPES.len())];
        let def = match ty {
            "INTEGER" => format!("{} INTEGER DEFAULT {}", n, rng.range(-5, 5)),
            "REAL" => format!("{} REAL DEFAULT {:.1}", n, rng.range(-9, 9) as f64 / 2.0),
            "TEXT" => format!("{} TEXT DEFAULT 'x{}'", n, rng.below(9)),
            _ => format!("{} BLOB DEFAULT x'01'", n),
        };
        return Some(Op::Stmt(format!(
            "ALTER TABLE {} ADD COLUMN {}",
            t.name, def
        )));
    }
    if pick < 81 && sc.allow_triggers && sc.triggers.len() < 2 {
        let ti = rng.below(sc.tables.len());
        let tname = sc.tables[ti].name.clone();
        let name = format!("trg{}", *idx_counter);
        *idx_counter += 1;
        let evt = match rng.below(3) {
            0 => format!("AFTER INSERT ON {}", tname),
            1 => format!("AFTER DELETE ON {}", tname),
            _ => format!("AFTER UPDATE ON {}", tname),
        };
        let note = gen_text_lit(rng);
        let sql = format!(
            "CREATE TRIGGER {} {} BEGIN INSERT INTO audit (note) VALUES ({}); END",
            name, evt, note
        );
        sc.triggers.push(name);
        return Some(Op::Stmt(sql));
    }
    if pick < 83 && !sc.triggers.is_empty() {
        let i = rng.below(sc.triggers.len());
        let name = sc.triggers.remove(i);
        return Some(Op::Stmt(format!("DROP TRIGGER {}", name)));
    }
    if pick < 85 {
        return Some(Op::Stmt("ANALYZE".into()));
    }
    // Random query probe (SELECT): does not change state; the caller
    // compares results.
    let t2 = &sc.tables[rng.below(sc.tables.len())];
    let probe = gen_probe(rng, sc, t2);
    Some(Op::Stmt(probe))
}

/// Deterministic (fully ordered) random query probe.
fn gen_probe(rng: &mut Rng, sc: &Schema, t: &Table) -> String {
    let c0 = &t.cols[rng.below(t.cols.len())].name;
    let c1 = t.cols[rng.below(t.cols.len())].name.clone();
    match rng.below(5) {
        0 => format!(
            "SELECT count(*), ifnull(min({}), 0), ifnull(max({}), 0) FROM {}",
            c0, c1, t.name
        ),
        1 => format!(
            "SELECT {}, count(*), ifnull(sum({}), 0) FROM {} GROUP BY {} ORDER BY 1, 2, 3",
            c0,
            t.cols[rng.below(t.cols.len())].name,
            t.name,
            c0
        ),
        2 => format!(
            "SELECT DISTINCT {} FROM {} WHERE {} ORDER BY 1",
            c0,
            t.name,
            gen_predicate(rng, t)
        ),
        3 => {
            if sc.tables.len() > 1 {
                let u = &sc.tables[rng.below(sc.tables.len())];
                let uc = &u.cols[rng.below(u.cols.len())].name;
                if u.worowid_pk.is_none() {
                    format!(
                        "SELECT a.rowid, b.rowid FROM {} a JOIN {} b ON a.{} = b.{} ORDER BY 1, 2 LIMIT 200",
                        t.name, u.name, c0, uc
                    )
                } else {
                    format!("SELECT rowid, {} FROM {} ORDER BY 1 LIMIT 100", c0, t.name)
                }
            } else {
                format!("SELECT rowid, {} FROM {} ORDER BY 1 LIMIT 100", c0, t.name)
            }
        }
        _ => format!(
            "SELECT typeof({}), {} FROM {} ORDER BY rowid LIMIT 80",
            c0, c1, t.name
        ),
    }
}

// ---------------------------------------------------------------------------
// State comparison
// ---------------------------------------------------------------------------

fn dump_order(t: &Table) -> String {
    match &t.worowid_pk {
        Some(pk) => pk
            .iter()
            .map(|&i| t.cols[i].name.clone())
            .collect::<Vec<_>>()
            .join(", "),
        None => "rowid".to_string(),
    }
}

fn dump_sql(t: &Table) -> String {
    match &t.worowid_pk {
        Some(_) => format!("SELECT * FROM {} ORDER BY {}", t.name, dump_order(t)),
        None => format!("SELECT rowid, * FROM {} ORDER BY rowid LIMIT 5000", t.name),
    }
}

fn count_sql(t: &Table) -> String {
    format!("SELECT count(*) FROM {}", t.name)
}

// ---------------------------------------------------------------------------
// Main driver
// ---------------------------------------------------------------------------

/// Execute one statement on both engines; returns (our_ok, sqlite_ok).
/// Query-ish statements get an immediate row-level comparison.
fn exec_stmt(
    ours: &mut Database,
    oracle: &rusqlite::Connection,
    ctx_cell: &std::cell::RefCell<Ctx>,
    sql: String,
) -> Result<(bool, bool), String> {
    let idx = {
        let mut c = ctx_cell.borrow_mut();
        c.script.push(sql.clone());
        c.script.len() - 1
    };
    let our_res = our_run(ours, &sql);
    let sq_res = sq_run(oracle, &sql);
    let our_ok = our_res.is_ok();
    let sq_ok = sq_res.is_ok();
    if our_ok != sq_ok {
        return Err(format!(
            "success divergence: ours={:?} sqlite={:?}",
            our_res.err(),
            sq_res.err()
        ));
    }
    if is_queryish(&sql) && !sql.to_ascii_uppercase().contains("PRAGMA") {
        if let (Ok(a), Ok(b)) = (&our_res, &sq_res) {
            rows_match(
                &format!("case {} stmt {}", ctx_cell.borrow().case, idx),
                a,
                b,
            )
            .map_err(|e| format!("{} [query probe: {}]", e, sql))?;
        }
    }
    Ok((our_ok, sq_ok))
}

/// Convenience wrapper: execute + on failure, report with the full script.
fn exec_checked(
    ours: &mut Database,
    oracle: &rusqlite::Connection,
    ctx_cell: &std::cell::RefCell<Ctx>,
    sql: &str,
) -> Result<bool, String> {
    let (our_ok, _sq_ok) = exec_stmt(ours, oracle, ctx_cell, sql.to_string()).map_err(|e| {
        let c = ctx_cell.borrow();
        let i = c.script.len().saturating_sub(1);
        let s = c.script.last().cloned().unwrap_or_default();
        fail(&c, i, &s, &e)
    })?;
    Ok(our_ok)
}

fn run_case(seed: u64, case: usize, n_ops: usize) -> Result<(), String> {
    let mut rng = Rng::new(seed ^ (case as u64).wrapping_mul(0x51ED_2701));
    SCHEMA_DDL.with(|c| c.borrow_mut().clear());
    let mut sc = gen_schema(&mut rng, true);
    let ddl: Vec<String> = SCHEMA_DDL.with(|c| c.borrow().clone());

    let mut ours = Database::open_in_memory().unwrap();
    let oracle = rusqlite::Connection::open_in_memory().unwrap();

    let ctx_cell: std::cell::RefCell<Ctx> = std::cell::RefCell::new(Ctx {
        seed,
        case,
        script: Vec::new(),
    });

    // Schema setup: both engines must accept the generated DDL.
    for ddl_stmt in ddl {
        exec_checked(&mut ours, &oracle, &ctx_cell, &ddl_stmt)?;
    }

    // Transaction state machine.
    let mut in_txn = false;
    let mut txn_ops_left = 0usize;
    let mut idx_counter = 0usize;
    let mut op_i = 0usize;
    // Set when the random-rowid regime arms: the armed table's rowids are
    // engine-local from that point, so the final dump skips just it.
    let mut skip_final: Option<String> = None;
    while op_i < n_ops {
        // Auto-close an open transaction.
        if in_txn && txn_ops_left == 0 {
            let sql = if rng.chance(70) { "COMMIT" } else { "ROLLBACK" };
            exec_checked(&mut ours, &oracle, &ctx_cell, sql)?;
            in_txn = false;
            op_i += 1;
            continue;
        }
        // Open a transaction sometimes.
        if !in_txn && rng.chance(8) {
            exec_checked(&mut ours, &oracle, &ctx_cell, "BEGIN")?;
            in_txn = true;
            txn_ops_left = 3 + rng.below(5);
            op_i += 1;
            continue;
        }
        // Savepoint choreography inside transactions.
        if in_txn && rng.chance(12) {
            let seq: Vec<&str> = if rng.chance(50) {
                vec!["SAVEPOINT sp1", "RELEASE sp1"]
            } else {
                vec!["SAVEPOINT sp1", "ROLLBACK TO sp1", "RELEASE sp1"]
            };
            for s in seq {
                exec_checked(&mut ours, &oracle, &ctx_cell, s)?;
            }
            op_i += 1;
            continue;
        }
        // Generate an op; DDL only outside transactions.
        let op = loop {
            match gen_op(&mut rng, &mut sc, &mut idx_counter) {
                Some(Op::Stmt(s)) => {
                    let head = s.trim_start().to_ascii_uppercase();
                    let is_ddl = head.starts_with("CREATE")
                        || head.starts_with("DROP")
                        || head.starts_with("ALTER")
                        || head.starts_with("ANALYZE");
                    if !in_txn || !is_ddl {
                        break s;
                    }
                }
                None => {}
            }
        };
        let our_ok = exec_checked(&mut ours, &oracle, &ctx_cell, &op)?;
        if in_txn {
            txn_ops_left -= 1;
        }
        // Full state dump policy: after failures (statement atomicity!),
        // after DDL, periodically, and at case end; count-compare otherwise.
        let head = op.trim_start().to_ascii_uppercase();
        let was_ddl =
            head.starts_with("CREATE") || head.starts_with("DROP") || head.starts_with("ALTER");
        let n = ctx_cell.borrow().script.len();
        // Invariant: index/table consistency must survive every statement.
        if let Some(verdict) = integrity_ok(&mut ours) {
            return Err(fail(
                &ctx_cell.borrow(),
                n - 1,
                &op,
                &format!("integrity_check: {}", verdict),
            ));
        }
        // Random-rowid regime: once any table's max rowid reaches i64::MAX,
        // the next implicit-rowid INSERT allocates a RANDOM rowid in BOTH
        // engines (SQLite OP_NewRowid semantics) — including a LATER tuple
        // of the very statement that armed it. The values are engine-local
        // by design and can never agree differentially, so end the case
        // cleanly (everything verified so far is still rigorous) instead of
        // reporting a false divergence.
        if let Some(armed_table) = random_rowid_armed(&mut ours, &sc) {
            // The arming statement may itself have completed a random
            // allocation in a later tuple — the armed table's rowids are
            // already engine-local. Skip only that table in the final dump;
            // every other table stays under full verification.
            eprintln!(
                "stateful fuzz: case {} ended early at op {}: random-rowid regime armed on {} (max rowid = i64::MAX)",
                case, op_i, armed_table
            );
            skip_final = Some(armed_table);
            break;
        }
        if !our_ok || was_ddl || n % 5 == 0 || op_i + 1 == n_ops {
            compare_full_state(&mut ours, &oracle, &sc, &ctx_cell, n - 1, &op, None)?;
        } else {
            compare_counts(&mut ours, &oracle, &sc, &ctx_cell, n - 1, &op)?;
        }
        op_i += 1;
    }
    // Close any dangling transaction and dump final state.
    if in_txn {
        exec_checked(&mut ours, &oracle, &ctx_cell, "COMMIT")?;
    }
    compare_full_state(
        &mut ours,
        &oracle,
        &sc,
        &ctx_cell,
        usize::MAX,
        "<final>",
        skip_final.as_deref(),
    )?;
    // Our engine must still pass integrity_check (defense in depth; the
    // per-op gate above should have caught any violation already).
    if let Some(verdict) = integrity_ok(&mut ours) {
        let n = ctx_cell.borrow().script.len();
        return Err(fail(
            &ctx_cell.borrow(),
            n.saturating_sub(1),
            "<final>",
            &format!("integrity_check: {}", verdict),
        ));
    }
    Ok(())
}

/// True when any rowid table's max rowid has reached i64::MAX — the point
/// after which implicit-rowid INSERTs allocate RANDOM rowids (in both this
/// engine and SQLite), making further differential rowid verification
/// impossible by design.
fn random_rowid_armed(ours: &mut Database, sc: &Schema) -> Option<String> {
    for t in &sc.tables {
        if t.worowid_pk.is_some() {
            continue; // WITHOUT ROWID tables have no rowid
        }
        let sql = format!("SELECT ifnull(max(rowid), 0) FROM {}", t.name);
        if let Ok(rows) = ours.query(&sql, ()) {
            if let Some(v) = rows.first().and_then(|r| r.first()) {
                if v.as_integer() == i64::MAX {
                    return Some(t.name.clone());
                }
            }
        }
    }
    None
}

/// Invariant gate: our engine must pass `PRAGMA integrity_check`. Checked
/// after EVERY generated op so a violation pinpoints the offending statement.
fn integrity_ok(ours: &mut Database) -> Option<String> {
    let rows = ours.query("PRAGMA integrity_check", []).ok()?;
    let verdict = rows
        .first()
        .and_then(|r| r.first())
        .map(|v| v.as_text())
        .unwrap_or_default();
    if verdict == "ok" {
        None
    } else {
        Some(verdict)
    }
}

fn compare_counts(
    ours: &mut Database,
    oracle: &rusqlite::Connection,
    sc: &Schema,
    ctx_cell: &std::cell::RefCell<Ctx>,
    stmt_idx: usize,
    stmt: &str,
) -> Result<(), String> {
    for t in &sc.tables {
        let a = our_run(ours, &count_sql(t));
        let b = sq_run(oracle, &count_sql(t));
        match (a, b) {
            (Ok(a), Ok(b)) => {
                let av = a.first().and_then(|r| r.first()).and_then(|v| match v {
                    Value::Integer(i) => Some(*i),
                    _ => None,
                });
                let bv = b.first().and_then(|r| r.first()).and_then(|v| match v {
                    Sv::Integer(i) => Some(*i),
                    _ => None,
                });
                if av != bv {
                    let c = ctx_cell.borrow();
                    return Err(fail(
                        &c,
                        stmt_idx,
                        stmt,
                        &format!("count({}) = {:?} vs sqlite {:?}", t.name, av, bv),
                    ));
                }
            }
            (Err(_), Err(_)) => {}
            (a, b) => {
                let c = ctx_cell.borrow();
                return Err(fail(
                    &c,
                    stmt_idx,
                    stmt,
                    &format!("count({}) divergence: {:?} vs {:?}", t.name, a, b),
                ));
            }
        }
    }
    Ok(())
}

fn compare_full_state(
    ours: &mut Database,
    oracle: &rusqlite::Connection,
    sc: &Schema,
    ctx_cell: &std::cell::RefCell<Ctx>,
    stmt_idx: usize,
    stmt: &str,
    skip_table: Option<&str>,
) -> Result<(), String> {
    for t in &sc.tables {
        if skip_table == Some(t.name.as_str()) {
            continue; // random-rowid regime: rowids are engine-local
        }
        let a = our_run(ours, &dump_sql(t));
        let b = sq_run(oracle, &dump_sql(t));
        match (a, b) {
            (Ok(a), Ok(b)) => {
                let label = format!("state({})", t.name);
                rows_match(&label, &a, &b).map_err(|e| {
                    let c = ctx_cell.borrow();
                    fail(&c, stmt_idx, stmt, &e)
                })?;
            }
            (Err(x), Err(_)) => {
                // Both rejected the dump (e.g. table renamed away by a
                // fuzzed DDL path) — agreement.
                let _ = x;
            }
            (a, b) => {
                let c = ctx_cell.borrow();
                return Err(fail(
                    &c,
                    stmt_idx,
                    stmt,
                    &format!(
                        "state dump divergence on {}: ours={:?} sqlite={:?}",
                        t.name,
                        a.err(),
                        b.err()
                    ),
                ));
            }
        }
    }
    Ok(())
}

#[test]
fn stateful_random_workload_matches_sqlite() {
    let cases = env_u64("RUSTQLITE_STATEFUL_CASES", 6);
    let n_ops = env_u64("RUSTQLITE_STATEFUL_OPS", 140);
    let seed = env_u64("RUSTQLITE_STATEFUL_SEED", 0x00C0_FFEE_F00D_BA5E);
    let start = std::time::Instant::now();
    // Wall-clock guard: dev-profile CI runners can be 10x slower than a
    // laptop; the contract needs coverage, not a specific iteration count.
    const BUDGET: std::time::Duration = std::time::Duration::from_secs(90);
    for case in 0..cases {
        if start.elapsed() > BUDGET {
            eprintln!(
                "stateful fuzz: time budget exhausted after case {} of {}",
                case, cases
            );
            break;
        }
        if let Err(msg) = run_case(seed, case as usize, n_ops as usize) {
            panic!("{}", msg);
        }
    }
}

/// Regression pins from the 2026-09 fresh-seed campaign (40 cases x 400
/// ops per seed, far beyond the default seed's sweep). Each (seed, case)
/// pair below once DIVERGED from real SQLite and is now fixed; a case is
/// independently seeded (`seed ^ case * PHI`), so pinning the exact case
/// re-verifies the whole fixed script on every push. The engine classes
/// covered: rowid-allocation cache poisoning after rowid-moving
/// UPDATE/DELETE (raise_max_rowid_lc), parallel fused/SELECTIVE/compiled
/// SUM sticky-overflow decline, upsert DO UPDATE secondary-UNIQUE
/// enforcement, rowid IN-list boundary-REAL seek-vs-numeric planning,
/// boundary-value hash joins (Real(-2^63) vs Integer(i64::MIN)) with the
/// unique-index-probe join direction, and AFTER UPDATE trigger vs index
/// maintenance ordering (undo-journal duplication window).
#[test]
fn stateful_fuzz_fresh_seed_pins() {
    let pins: [(u64, usize); 6] = [
        (777001, 3),  // auto rowids 28.. vs SQLite 40.. after a rowid move
        (777001, 39), // parallel slice-local [2^62, 2^62+8] false overflow
        (777023, 1),  // upsert DO UPDATE left two rows sharing a UNIQUE key
        (777023, 28), // `id IN (blob, text, -2^63e18)` boundary REAL match
        (777101, 3),  // Real(-2^63) = Integer(i64::MIN) join pair dropped
        (777101, 36), // "index entries out of order or duplicated" on undo
    ];
    for (seed, case) in pins {
        if let Err(msg) = run_case(seed, case, 400) {
            panic!("seed {seed} case {case}: {msg}");
        }
    }
}
