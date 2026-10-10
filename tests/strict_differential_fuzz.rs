//! STRICT differential fuzzing against the bundled real SQLite.
//!
//! The older `sql_fuzz` harness compares INTEGER and REAL as equal when
//! numerically close and draws from a tiny grammar — which hides exactly
//! the class of bug a SQLite-compatible engine must not have: a result of
//! the wrong *storage class* (`1` vs `1.0`, `'1'` vs `1`), a different
//! affinity decision, a different NULL / three-valued-logic answer.
//!
//! This harness is deliberately unforgiving:
//!
//! * values must match by storage class AND value (REAL compared bit-exact
//!   modulo `-0.0 == 0.0`; `typeof()` divergence is a failure);
//! * success/failure must agree (one engine erroring while the other
//!   answers is a failure);
//! * statement order: rows compare as a sorted multiset unless the query
//!   carries a total ORDER BY, in which case the exact sequence must match;
//! * DML statements are followed by a full-table content compare, so a
//!   wrong UPDATE/DELETE/INSERT surfaces even when it reports success.
//!
//! The grammar covers literals of every storage class (incl. i64 edges,
//! numeric-looking TEXT, BLOBs), column affinities (INTEGER / TEXT / REAL /
//! NUMERIC / BLOB / none), arithmetic incl. overflow, bit ops, string
//! concatenation, comparisons across classes, `IS [NOT] [DISTINCT FROM]`,
//! `BETWEEN`, `IN`, `LIKE`/`GLOB` with escapes, `CASE` (both forms),
//! `CAST`, `COLLATE`, ~40 scalar functions, aggregates with
//! DISTINCT/FILTER, GROUP BY/HAVING, DISTINCT, compound selects, scalar /
//! EXISTS / IN subqueries, joins, and UPDATE/DELETE/INSERT…SELECT.
//!
//! Reproduce:  STRICT_FUZZ_SEED=<n> STRICT_FUZZ_ITERS=<n> cargo test --test strict_differential_fuzz -- --nocapture

use rustqlite::{Database, Value};

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add(0x2545_F491_4F6C_DD1D)
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
        lo + (self.next_u64() % ((hi - lo + 1) as u64)) as i64
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.next_u64() % 100 < pct
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[self.below(xs.len())]
    }
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

// ---------------------------------------------------------------- schema --

/// Tables: (name, columns). Every column name is globally unique so an
/// unqualified reference in a join is never ambiguous.
const SCHEMA: &[&str] = &[
    "CREATE TABLE t1(a INTEGER, b TEXT, c REAL, d NUMERIC, e BLOB, f)",
    "CREATE TABLE t2(id INTEGER PRIMARY KEY, g INTEGER, h TEXT COLLATE NOCASE, i REAL)",
    "CREATE TABLE t3(j TEXT PRIMARY KEY, k INTEGER, l) WITHOUT ROWID",
    "CREATE INDEX t1_a ON t1(a)",
    "CREATE INDEX t1_bc ON t1(b, c)",
    "CREATE INDEX t2_g ON t2(g)",
    "CREATE INDEX t2_h ON t2(h)",
    "CREATE INDEX t3_k ON t3(k)",
];

const T1_COLS: &[&str] = &["a", "b", "c", "d", "e", "f"];
const T2_COLS: &[&str] = &["id", "g", "h", "i"];
const T3_COLS: &[&str] = &["j", "k", "l"];

fn int_lit(rng: &mut Rng) -> String {
    match rng.below(12) {
        0 => "9223372036854775807".into(),
        1 => "-9223372036854775808".into(),
        2 => "9223372036854775806".into(),
        3 => "-9223372036854775807".into(),
        4 => "4294967296".into(),
        5 => "2147483648".into(),
        6 => rng.range(-3, 3).to_string(),
        7 => "0".into(),
        _ => rng.range(-100, 100).to_string(),
    }
}

fn real_lit(rng: &mut Rng) -> String {
    match rng.below(14) {
        0 => "0.0".into(),
        1 => "-0.0".into(),
        2 => "1e308".into(),
        3 => "-1e308".into(),
        4 => "1.5e-7".into(),
        5 => "9.2233720368547758e18".into(),
        6 => "-9.2233720368547758e18".into(),
        7 => "2.5".into(),
        8 => "-2.5".into(),
        9 => "0.1".into(),
        10 => "3.0".into(),
        11 => "1e20".into(),
        _ => format!("{}.{}", rng.range(-50, 50), rng.range(0, 99)),
    }
}

fn text_lit(rng: &mut Rng) -> String {
    rng.pick(&[
        "''",
        "'abc'",
        "'ABC'",
        "'Abc'",
        "'12'",
        "' 12'",
        "'12 '",
        "'12abc'",
        "'1.5'",
        "'1e3'",
        "'-0'",
        "'0x1F'",
        "'+7'",
        "'9223372036854775808'",
        "'-9223372036854775809'",
        "'3.0'",
        "'a%b'",
        "'a_b'",
        "'%'",
        "'_'",
        "'é'",
        "'É'",
        "'ß'",
        "'x y'",
        "' '",
        "'abc  '",
        "'NULL'",
        "'.5'",
        "'5.'",
        "'1e400'",
        "'Inf'",
        "'0'",
        "'00012'",
    ])
    .to_string()
}

fn blob_lit(rng: &mut Rng) -> String {
    rng.pick(&[
        "x''",
        "x'00'",
        "x'41'",
        "x'616263'",
        "x'3132'",
        "x'00410042'",
        "x'c3a9'",
    ])
    .to_string()
}

fn literal(rng: &mut Rng) -> String {
    match rng.below(10) {
        0 => "NULL".into(),
        1..=3 => int_lit(rng),
        4 | 5 => real_lit(rng),
        6..=8 => text_lit(rng),
        _ => blob_lit(rng),
    }
}

fn row_value(rng: &mut Rng) -> String {
    // Data values: weighted toward small, collision-friendly values so
    // joins / GROUP BY / DISTINCT see duplicates.
    match rng.below(12) {
        0 | 1 => "NULL".into(),
        2..=4 => rng.range(-5, 5).to_string(),
        5 => int_lit(rng),
        6 => format!("{}.{}", rng.range(-5, 5), rng.pick(&["0", "5", "25"])),
        7 => real_lit(rng),
        8 | 9 => text_lit(rng),
        10 => format!("'{}'", rng.range(-5, 5)),
        _ => blob_lit(rng),
    }
}

// ------------------------------------------------------------ expressions --

struct Ctx<'a> {
    cols: &'a [&'a str],
    /// Allow aggregate calls (only in projections / HAVING of aggregate
    /// queries).
    agg: bool,
    /// Allow subqueries.
    subq: bool,
}

const SCALAR_FNS_1: &[&str] = &[
    "abs",
    "length",
    "lower",
    "upper",
    "typeof",
    "hex",
    "quote",
    "trim",
    "ltrim",
    "rtrim",
    "unicode",
    "sign",
    "round",
    "octet_length",
    "likely",
    "unlikely",
    "soundex_disabled",
];

fn expr(rng: &mut Rng, ctx: &Ctx, depth: u32) -> String {
    if depth == 0 || rng.chance(25) {
        return if rng.chance(55) && !ctx.cols.is_empty() {
            rng.pick(ctx.cols).to_string()
        } else {
            literal(rng)
        };
    }
    let d = depth - 1;
    match rng.below(30) {
        0 | 1 => format!(
            "({} {} {})",
            expr(rng, ctx, d),
            rng.pick(&["+", "-", "*", "/", "%"]),
            expr(rng, ctx, d)
        ),
        2 => format!("({} || {})", expr(rng, ctx, d), expr(rng, ctx, d)),
        3 => format!(
            "({} {} {})",
            expr(rng, ctx, d),
            rng.pick(&["&", "|", "<<", ">>"]),
            expr(rng, ctx, d)
        ),
        4..=6 => format!(
            "({} {} {})",
            expr(rng, ctx, d),
            rng.pick(&["=", "==", "!=", "<>", "<", "<=", ">", ">="]),
            expr(rng, ctx, d)
        ),
        7 => format!(
            "({} {} {})",
            expr(rng, ctx, d),
            rng.pick(&["IS", "IS NOT", "IS DISTINCT FROM", "IS NOT DISTINCT FROM"]),
            expr(rng, ctx, d)
        ),
        8 => format!(
            "({} {} {})",
            expr(rng, ctx, d),
            rng.pick(&["AND", "OR"]),
            expr(rng, ctx, d)
        ),
        9 => format!(
            "({} {})",
            rng.pick(&["NOT", "-", "+", "~"]),
            expr(rng, ctx, d)
        ),
        10 => format!(
            "({} {}BETWEEN {} AND {})",
            expr(rng, ctx, d),
            if rng.chance(30) { "NOT " } else { "" },
            expr(rng, ctx, d),
            expr(rng, ctx, d)
        ),
        11 => {
            let n = 1 + rng.below(4);
            let items: Vec<String> = (0..n).map(|_| expr(rng, ctx, d.min(1))).collect();
            format!(
                "({} {}IN ({}))",
                expr(rng, ctx, d),
                if rng.chance(30) { "NOT " } else { "" },
                items.join(", ")
            )
        }
        12 => format!(
            "({} {} {})",
            expr(rng, ctx, d),
            rng.pick(&["LIKE", "NOT LIKE", "GLOB", "NOT GLOB"]),
            match rng.below(3) {
                0 => rng
                    .pick(&[
                        "'a%'", "'%b%'", "'_bc'", "'%'", "'1%'", "'A_C'", "'[a-c]*'", "'*1*'",
                        "'?'", "'%é%'"
                    ])
                    .to_string(),
                _ => expr(rng, ctx, d),
            }
        ),
        13 => format!(
            "({} LIKE {} ESCAPE '\\')",
            expr(rng, ctx, d),
            rng.pick(&["'a\\%b'", "'\\_%'", "'%\\%'", "'a\\_b'"])
        ),
        14 => format!(
            "({} {})",
            expr(rng, ctx, d),
            rng.pick(&["IS NULL", "IS NOT NULL", "NOTNULL", "ISNULL"])
        ),
        15 | 16 => {
            let n = 1 + rng.below(3);
            let mut s = String::from("CASE");
            let simple = rng.chance(40);
            if simple {
                s.push(' ');
                s.push_str(&expr(rng, ctx, d));
            }
            for _ in 0..n {
                s.push_str(&format!(
                    " WHEN {} THEN {}",
                    expr(rng, ctx, d),
                    expr(rng, ctx, d)
                ));
            }
            if rng.chance(60) {
                s.push_str(&format!(" ELSE {}", expr(rng, ctx, d)));
            }
            s.push_str(" END");
            s
        }
        17 | 18 => format!(
            "CAST({} AS {})",
            expr(rng, ctx, d),
            rng.pick(&[
                "INTEGER",
                "REAL",
                "TEXT",
                "BLOB",
                "NUMERIC",
                "INT",
                "FLOAT",
                "VARCHAR(5)",
                "DECIMAL"
            ])
        ),
        19 => format!(
            "({} COLLATE {})",
            expr(rng, ctx, d),
            rng.pick(&["NOCASE", "BINARY", "RTRIM"])
        ),
        20..=22 => {
            let f = rng.pick(SCALAR_FNS_1);
            let f = if f == "soundex_disabled" { "length" } else { f };
            format!("{}({})", f, expr(rng, ctx, d))
        }
        23 => match rng.below(14) {
            0 => format!("coalesce({}, {})", expr(rng, ctx, d), expr(rng, ctx, d)),
            1 => format!("ifnull({}, {})", expr(rng, ctx, d), expr(rng, ctx, d)),
            2 => format!("nullif({}, {})", expr(rng, ctx, d), expr(rng, ctx, d)),
            3 => format!(
                "iif({}, {}, {})",
                expr(rng, ctx, d),
                expr(rng, ctx, d),
                expr(rng, ctx, d)
            ),
            4 => format!("substr({}, {})", expr(rng, ctx, d), expr(rng, ctx, d)),
            5 => format!(
                "substr({}, {}, {})",
                expr(rng, ctx, d),
                int_lit_small(rng),
                int_lit_small(rng)
            ),
            6 => format!(
                "replace({}, {}, {})",
                expr(rng, ctx, d),
                expr(rng, ctx, d),
                expr(rng, ctx, d)
            ),
            7 => format!("instr({}, {})", expr(rng, ctx, d), expr(rng, ctx, d)),
            8 => format!("round({}, {})", expr(rng, ctx, d), rng.range(-2, 4)),
            9 => format!("min({}, {})", expr(rng, ctx, d), expr(rng, ctx, d)),
            10 => format!(
                "max({}, {}, {})",
                expr(rng, ctx, d),
                expr(rng, ctx, d),
                expr(rng, ctx, d)
            ),
            11 => format!(
                "trim({}, {})",
                expr(rng, ctx, d),
                rng.pick(&["'a'", "' 1'", "'xy'", "''"])
            ),
            12 => format!("concat({}, {})", expr(rng, ctx, d), expr(rng, ctx, d)),
            _ => format!(
                "concat_ws({}, {}, {})",
                rng.pick(&["','", "''", "NULL"]),
                expr(rng, ctx, d),
                expr(rng, ctx, d)
            ),
        },
        24 => match rng.below(6) {
            0 => format!("printf('%d', {})", expr(rng, ctx, d)),
            1 => format!(
                "printf('%s|%s', {}, {})",
                expr(rng, ctx, d),
                expr(rng, ctx, d)
            ),
            2 => format!("printf('%.2f', {})", expr(rng, ctx, d)),
            3 => format!("printf('%5s', {})", expr(rng, ctx, d)),
            4 => format!("printf('%x', {})", expr(rng, ctx, d)),
            _ => format!("printf('%q', {})", expr(rng, ctx, d)),
        },
        25 if ctx.subq => match rng.below(4) {
            0 => format!(
                "(SELECT max(g) FROM t2 WHERE g < {})",
                expr(rng, ctx, d.min(1))
            ),
            1 => format!(
                "EXISTS (SELECT 1 FROM t3 WHERE k = {})",
                expr(rng, ctx, d.min(1))
            ),
            2 => format!("({} IN (SELECT g FROM t2))", expr(rng, ctx, d)),
            _ => format!("({} NOT IN (SELECT k FROM t3))", expr(rng, ctx, d)),
        },
        26 if ctx.agg => agg_call(rng, ctx, d),
        27 if ctx.agg => agg_call(rng, ctx, d),
        _ => format!(
            "({} {} {})",
            expr(rng, ctx, d),
            rng.pick(&["+", "=", "<", "||", "AND"]),
            expr(rng, ctx, d)
        ),
    }
}

fn int_lit_small(rng: &mut Rng) -> String {
    rng.range(-3, 5).to_string()
}

fn agg_call(rng: &mut Rng, ctx: &Ctx, d: u32) -> String {
    let inner = Ctx {
        cols: ctx.cols,
        agg: false,
        subq: false,
    };
    let arg = expr(rng, &inner, d.min(2));
    let distinct = if rng.chance(20) { "DISTINCT " } else { "" };
    let filter = if rng.chance(15) {
        format!(" FILTER (WHERE {})", expr(rng, &inner, 1))
    } else {
        String::new()
    };
    match rng.below(9) {
        0 => format!("count({}{}){}", distinct, arg, filter),
        1 => format!("count(*){}", filter),
        2 => format!("sum({}{}){}", distinct, arg, filter),
        3 => format!("total({}{}){}", distinct, arg, filter),
        4 => format!("avg({}{}){}", distinct, arg, filter),
        5 => format!("min({}){}", arg, filter),
        6 => format!("max({}){}", arg, filter),
        // group_concat's element ORDER follows the chosen scan (an index
        // walk in SQLite, a table walk here) — wrap in length(), which is
        // order-independent but still pins separators and NULL handling.
        7 => format!("octet_length(group_concat({}{}){})", distinct, arg, filter),
        _ => format!("octet_length(group_concat({}, '|'){})", arg, filter),
    }
}

// --------------------------------------------------------------- queries --

#[derive(Default)]
struct Query {
    sql: String,
    /// True when the ORDER BY is total over the output, so row SEQUENCE is
    /// part of the contract.
    ordered: bool,
    /// Output columns that are a top-level `min(x)` / `max(x)` aggregate:
    /// among several EQUAL candidates (`0` / `0.0`, `'a'` / `'A'` under
    /// NOCASE) SQLite returns the first its plan reads — and it reads a
    /// table through a covering index whenever one exists — so only the
    /// equality class is a contract there (see `unspecified_only`).
    minmax_cols: Vec<usize>,
}

fn table_for(rng: &mut Rng) -> (&'static str, &'static [&'static str]) {
    match rng.below(3) {
        0 => ("t1", T1_COLS),
        1 => ("t2", T2_COLS),
        _ => ("t3", T3_COLS),
    }
}

fn gen_query(rng: &mut Rng) -> Query {
    match rng.below(10) {
        // Plain projection + filter.
        0..=3 => {
            let (t, cols) = table_for(rng);
            let ctx = Ctx {
                cols,
                agg: false,
                subq: true,
            };
            let n = 1 + rng.below(3);
            let proj: Vec<String> = (0..n).map(|_| expr(rng, &ctx, 3)).collect();
            let mut sql = format!(
                "SELECT {}{} FROM {}",
                if rng.chance(15) { "DISTINCT " } else { "" },
                proj.join(", "),
                t
            );
            if rng.chance(75) {
                sql.push_str(&format!(" WHERE {}", expr(rng, &ctx, 3)));
            }
            // A total order: every projected column as a key (ties are
            // then identical rows, so the sequence is fully determined).
            let ordered = rng.chance(35);
            if ordered {
                let keys: Vec<String> = (1..=n)
                    .map(|i| format!("{}{}", i, if rng.chance(30) { " DESC" } else { "" }))
                    .collect();
                sql.push_str(&format!(" ORDER BY {}", keys.join(", ")));
                if rng.chance(30) {
                    sql.push_str(&format!(
                        " LIMIT {} OFFSET {}",
                        rng.range(0, 6),
                        rng.range(0, 3)
                    ));
                }
            }
            Query {
                sql,
                ordered,
                ..Default::default()
            }
        }
        // Aggregate, optionally grouped.
        4..=5 => {
            let (t, cols) = table_for(rng);
            let plain = Ctx {
                cols,
                agg: false,
                subq: false,
            };
            let aggc = Ctx {
                cols,
                agg: true,
                subq: false,
            };
            let grouped = rng.chance(60);
            let key = expr(rng, &plain, 2);
            let n = 1 + rng.below(3);
            let mut proj: Vec<String> = (0..n).map(|_| agg_call(rng, &aggc, 2)).collect();
            if grouped {
                proj.insert(0, key.clone());
            }
            let minmax_cols: Vec<usize> = proj
                .iter()
                .enumerate()
                .filter(|(i, p)| {
                    (!grouped || *i > 0) && (p.starts_with("min(") || p.starts_with("max("))
                })
                .map(|(i, _)| i)
                .collect();
            let mut sql = format!("SELECT {} FROM {}", proj.join(", "), t);
            if rng.chance(50) {
                sql.push_str(&format!(" WHERE {}", expr(rng, &plain, 2)));
            }
            if grouped {
                sql.push_str(&format!(" GROUP BY {}", key));
                if rng.chance(30) {
                    // HAVING over aggregates only: a bare column there
                    // reads an ARBITRARY row of the group (SQLite: the last
                    // one its plan visited) — plan-dependent, not a
                    // semantic contract to compare.
                    let lit = Ctx {
                        cols: &[],
                        agg: false,
                        subq: false,
                    };
                    sql.push_str(&format!(
                        " HAVING {} {} {}",
                        agg_call(rng, &aggc, 2),
                        rng.pick(&["=", "<>", "<", ">=", "AND", "OR", "IS NOT"]),
                        expr(rng, &lit, 1)
                    ));
                }
            }
            Query {
                sql,
                ordered: false,
                minmax_cols,
            }
        }
        // Join.
        6 => {
            let ctx = Ctx {
                cols: &["a", "b", "c", "d", "f", "id", "g", "h", "i"],
                agg: false,
                subq: false,
            };
            let kind = rng.pick(&["JOIN", "LEFT JOIN", "CROSS JOIN", "INNER JOIN"]);
            let on = match rng.below(4) {
                0 => "t1.a = t2.g".to_string(),
                1 => "t1.b = t2.h".to_string(),
                2 => format!("t1.a = t2.id AND {}", expr(rng, &ctx, 2)),
                _ => expr(rng, &ctx, 2),
            };
            let on_clause = if kind == "CROSS JOIN" {
                String::new()
            } else {
                format!(" ON {}", on)
            };
            let proj: Vec<String> = (0..2).map(|_| expr(rng, &ctx, 2)).collect();
            let mut sql = format!(
                "SELECT {} FROM t1 {} t2{}",
                proj.join(", "),
                kind,
                on_clause
            );
            if rng.chance(50) {
                sql.push_str(&format!(" WHERE {}", expr(rng, &ctx, 2)));
            }
            Query {
                sql,
                ordered: false,
                ..Default::default()
            }
        }
        // Compound select.
        7 => {
            let op = rng.pick(&["UNION", "UNION ALL", "INTERSECT", "EXCEPT"]);
            let c1 = Ctx {
                cols: T1_COLS,
                agg: false,
                subq: false,
            };
            let c2 = Ctx {
                cols: T2_COLS,
                agg: false,
                subq: false,
            };
            let sql = format!(
                "SELECT {}, {} FROM t1 WHERE {} {} SELECT {}, {} FROM t2 WHERE {}",
                expr(rng, &c1, 2),
                expr(rng, &c1, 1),
                expr(rng, &c1, 2),
                op,
                expr(rng, &c2, 2),
                expr(rng, &c2, 1),
                expr(rng, &c2, 2)
            );
            Query {
                sql,
                ordered: false,
                ..Default::default()
            }
        }
        // Constant expression (no table): the evaluator in isolation.
        8 => {
            let ctx = Ctx {
                cols: &[],
                agg: false,
                subq: false,
            };
            let n = 1 + rng.below(3);
            let proj: Vec<String> = (0..n).map(|_| expr(rng, &ctx, 4)).collect();
            Query {
                sql: format!("SELECT {}", proj.join(", ")),
                ordered: true,
                ..Default::default()
            }
        }
        // Typeof probe: storage class of a projected expression.
        _ => {
            let (t, cols) = table_for(rng);
            let ctx = Ctx {
                cols,
                agg: false,
                subq: false,
            };
            let e = expr(rng, &ctx, 3);
            Query {
                sql: format!("SELECT typeof({e}), quote({e}) FROM {t}"),
                ordered: false,
                ..Default::default()
            }
        }
    }
}

fn gen_dml(rng: &mut Rng) -> (String, &'static str) {
    let (t, cols) = table_for(rng);
    // No subqueries in DML: one that reads the statement's own target
    // table has no fixed answer in SQLite — it may see the statement's
    // earlier row changes through a live index (`UPDATE t2 SET g = (x IN
    // (SELECT g FROM t2))` probes t2_g as it is being rewritten), so the
    // result depends on the plan, not on SQL semantics.
    let ctx = Ctx {
        cols,
        agg: false,
        subq: false,
    };
    let settable: &[&str] = match t {
        "t1" => T1_COLS,
        "t2" => &["g", "h", "i"],
        _ => &["k", "l"],
    };
    let sql = match rng.below(4) {
        0 => {
            let c = rng.pick(settable);
            let mut s = format!("UPDATE {} SET {} = {}", t, c, expr(rng, &ctx, 2));
            if rng.chance(80) {
                s.push_str(&format!(" WHERE {}", expr(rng, &ctx, 2)));
            }
            s
        }
        1 => format!("DELETE FROM {} WHERE {}", t, expr(rng, &ctx, 2)),
        2 => {
            let mut vals: Vec<String> = cols.iter().map(|_| row_value(rng)).collect();
            // t2's explicit rowid stays small: once the largest rowid is
            // i64::MAX, SQLite picks RANDOM rowids for later inserts — no
            // deterministic answer exists to compare.
            if t == "t2" {
                vals[0] = if rng.chance(20) {
                    "NULL".into()
                } else {
                    rng.range(-5, 40).to_string()
                };
            }
            let conflict = rng.pick(&["", "OR IGNORE ", "OR REPLACE "]);
            format!("INSERT {}INTO {} VALUES ({})", conflict, t, vals.join(", "))
        }
        _ => {
            // INSERT ... SELECT from the same table (self-copy, transformed).
            let c = rng.pick(settable);
            match t {
                // `c == "a"` would name `a` twice: insert one column then
                // (the old `t1(a, a)` -> `t1(a)` rewrite left TWO values
                // for one column — a statement both engines reject).
                "t1" if c == "a" => format!(
                    "INSERT INTO t1(a) SELECT {} FROM t1 WHERE {} ORDER BY rowid LIMIT 3",
                    expr(rng, &ctx, 1),
                    expr(rng, &ctx, 2)
                ),
                "t1" => format!(
                    "INSERT INTO t1(a, {c}) SELECT {}, {} FROM t1 WHERE {} ORDER BY rowid LIMIT 3",
                    expr(rng, &ctx, 1),
                    expr(rng, &ctx, 1),
                    expr(rng, &ctx, 2)
                ),
                "t2" => format!(
                    "INSERT INTO t2(g, h) SELECT {}, {} FROM t2 WHERE {} ORDER BY id LIMIT 3",
                    expr(rng, &ctx, 1),
                    expr(rng, &ctx, 1),
                    expr(rng, &ctx, 2)
                ),
                _ => format!(
                    "INSERT OR IGNORE INTO t3(j, k) SELECT {} || 'x', {} FROM t3 WHERE {} ORDER BY j LIMIT 3",
                    expr(rng, &ctx, 1),
                    expr(rng, &ctx, 1),
                    expr(rng, &ctx, 2)
                ),
            }
        }
    };
    (sql, t)
}

// ------------------------------------------------------------- execution --

#[derive(Clone, PartialEq)]
enum V {
    Null,
    Int(i64),
    Real(u64),
    /// Raw bytes: SQLite TEXT can carry invalid UTF-8 (`CAST(x'ff' AS TEXT)`).
    Text(Vec<u8>),
    Blob(Vec<u8>),
}

impl std::fmt::Debug for V {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            V::Null => write!(f, "NULL"),
            V::Int(i) => write!(f, "{i}"),
            V::Real(b) => write!(f, "{:?}r", f64::from_bits(*b)),
            V::Text(t) => match std::str::from_utf8(t) {
                Ok(s) => write!(f, "{s:?}"),
                Err(_) => write!(f, "text{t:02x?}"),
            },
            V::Blob(b) => write!(f, "x{b:02x?}"),
        }
    }
}

fn real_bits(x: f64) -> u64 {
    if x == 0.0 {
        0
    } else {
        x.to_bits()
    }
}

fn from_ours(v: &Value) -> V {
    match v {
        Value::Null => V::Null,
        Value::Integer(i) => V::Int(*i),
        Value::Real(r) => V::Real(real_bits(*r)),
        Value::Text(t) => V::Text(t.as_bytes().to_vec()),
        Value::Blob(b) => V::Blob(b.clone()),
    }
}

fn from_sqlite(v: rusqlite::types::ValueRef<'_>) -> V {
    use rusqlite::types::ValueRef as R;
    match v {
        R::Null => V::Null,
        R::Integer(i) => V::Int(i),
        R::Real(r) => V::Real(real_bits(r)),
        R::Text(t) => V::Text(t.to_vec()),
        R::Blob(b) => V::Blob(b.to_vec()),
    }
}

fn sort_key(v: &V) -> (u8, String) {
    match v {
        V::Null => (0, String::new()),
        V::Int(i) => (1, format!("{:020}", *i as i128 + (1i128 << 64))),
        V::Real(b) => (2, format!("{:020}", b)),
        V::Text(t) => (3, String::from_utf8_lossy(t).into_owned()),
        V::Blob(b) => (4, format!("{:?}", b)),
    }
}

type Rows = Vec<Vec<V>>;

fn sqlite_rows(conn: &rusqlite::Connection, sql: &str) -> Result<Rows, String> {
    let mut stmt = conn.prepare(sql).map_err(|e| e.to_string())?;
    let n = stmt.column_count();
    let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    loop {
        match rows.next() {
            Ok(Some(r)) => {
                let mut row = Vec::with_capacity(n);
                for i in 0..n {
                    row.push(from_sqlite(r.get_ref(i).map_err(|e| e.to_string())?));
                }
                out.push(row);
            }
            Ok(None) => break,
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(out)
}

fn our_rows(db: &Database, sql: &str) -> Result<Rows, String> {
    let rows = db.query(sql, []).map_err(|e| e.to_string())?;
    Ok(rows
        .iter()
        .map(|r| r.iter().map(from_ours).collect())
        .collect())
}

fn normalize(mut rows: Rows, ordered: bool) -> Rows {
    if !ordered {
        rows.sort_by(|a, b| {
            let ka: Vec<_> = a.iter().map(sort_key).collect();
            let kb: Vec<_> = b.iter().map(sort_key).collect();
            ka.cmp(&kb)
        });
    }
    rows
}

struct Divergence {
    seed: u64,
    setup: Vec<String>,
    sql: String,
    ours: Result<Rows, String>,
    theirs: Result<Rows, String>,
}

impl std::fmt::Display for Divergence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "---- divergence (seed {}) ----", self.seed)?;
        for s in &self.setup {
            writeln!(f, "  {};", s)?;
        }
        writeln!(f, "  QUERY: {}", self.sql)?;
        let show = |r: &Result<Rows, String>| match r {
            Ok(rows) if rows.len() > 12 => format!("{} rows, first: {:?}", rows.len(), &rows[..12]),
            Ok(rows) => format!("{:?}", rows),
            Err(e) => format!("ERROR {}", e),
        };
        writeln!(f, "  ours   = {}", show(&self.ours))?;
        writeln!(f, "  sqlite = {}", show(&self.theirs))
    }
}

/// SQLite's equality class of a value: numbers by numeric value (`1` and
/// `1.0` compare equal), TEXT case-folded when a NOCASE comparison is in
/// play. Used ONLY to recognize the two answers SQL leaves unspecified
/// (see `unspecified_only`).
fn eq_class(v: &V, nocase: bool, rtrim: bool) -> V {
    match v {
        V::Real(b) => {
            let f = f64::from_bits(*b);
            // Exactly an i64 (-2^63 included, 2^63 not): SQLite compares
            // such a REAL equal to that INTEGER (sqlite3IntFloatCompare).
            if f.fract() == 0.0
                && (-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&f)
            {
                V::Int(f as i64)
            } else {
                v.clone()
            }
        }
        V::Text(t) if nocase || rtrim => {
            let mut end = t.len();
            while rtrim && end > 0 && t[end - 1] == b' ' {
                end -= 1;
            }
            let t = &t[..end];
            V::Text(if nocase {
                t.to_ascii_lowercase()
            } else {
                t.to_vec()
            })
        }
        _ => v.clone(),
    }
}

/// Do two DIFFERENT answers differ only where SQL leaves the answer
/// unspecified?
///
/// * WHICH of several equal-but-distinguishable values (`1` vs `1.0`,
///   `'a'` vs `'A'` under NOCASE) represents a DISTINCT / UNION /
///   INTERSECT / EXCEPT / GROUP BY group — SQLite's own pick depends on
///   its plan (first row for DISTINCT, last insert for UNION's ephemeral
///   index, ...);
/// * the ORDER of rows whose ORDER BY keys tie under the same equality —
///   and, under LIMIT / OFFSET, WHICH tied rows fall inside the window;
/// * the representative a top-level `min()` / `max()` returns.
///
/// Anything else — a row count, a value outside the equality class, a
/// storage class that is not one of the tied candidates, an order that
/// differs on non-tied keys — is still a divergence. Every acceptance is
/// COUNTED and reported (strict_differential_vs_sqlite prints the total).
fn unspecified_only(
    sql: &str,
    ordered: bool,
    minmax_cols: &[usize],
    ours: &Rows,
    theirs: &Rows,
) -> bool {
    if ours.len() != theirs.len() {
        return false;
    }
    let up = sql.to_ascii_uppercase();
    if !ordered && float_sum_order_only(&up, ours, theirs) {
        return true;
    }
    let dedups = ["DISTINCT", "UNION", "INTERSECT", "EXCEPT", "GROUP BY"]
        .iter()
        .any(|k| up.contains(k));
    if !dedups && !ordered && minmax_cols.is_empty() {
        return false;
    }
    // RTRIM: trailing spaces do not count (`'abc'` ties `'abc  '`).
    let rtrim = up.contains("RTRIM");
    let nocase = up.contains("NOCASE")
        || sql
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .any(|tok| tok == "h");
    // Which output columns hold a group REPRESENTATIVE: every column of a
    // DISTINCT / compound row (the whole row is the dedup key); for a
    // plain GROUP BY only the key (the generator projects it first) —
    // aggregate values stay strictly compared, so a storage-class bug in
    // sum()/avg()/... can never hide behind this relaxation.
    let all_cols = ordered
        || ["DISTINCT", "UNION", "INTERSECT", "EXCEPT"]
            .iter()
            .any(|k| up.contains(k));
    let class = |rows: &Rows| -> Rows {
        rows.iter()
            .map(|r| {
                r.iter()
                    .enumerate()
                    .map(|(i, v)| {
                        if all_cols || (dedups && i == 0) || minmax_cols.contains(&i) {
                            eq_class(v, nocase, rtrim)
                        } else {
                            v.clone()
                        }
                    })
                    .collect()
            })
            .collect()
    };
    let (co, ct) = (class(ours), class(theirs));
    if ordered {
        // Same sequence of equality classes; when no dedup is involved
        // the two answers must hold the very same rows (only tied rows
        // may trade places) — unless a LIMIT / OFFSET window cuts through
        // a run of tied rows: which of them land inside it follows the
        // scan order (SQLite's top-N sorter keeps the first-inserted
        // ties, and it may scan a covering index where we scan the
        // table), so only the per-position equality classes are fixed.
        let windowed = up.contains(" LIMIT ") || up.contains(" OFFSET ");
        co == ct
            && (dedups
                || windowed
                || normalize(ours.clone(), false) == normalize(theirs.clone(), false))
    } else {
        normalize(co, false) == normalize(ct, false)
    }
}

/// Floating-point SUMMATION ORDER is plan-dependent (SQLite may walk a
/// covering index where we walk the table): in a sum / total / avg query,
/// REAL results may differ in the last bits (KBN compensation shrinks but
/// does not erase order effects), and an IEEE overflow can land as ±inf
/// in one order and NaN — SQLite's NULL — in another. Everything else
/// (storage classes, non-REAL values, larger differences) stays strict.
fn float_sum_order_only(up: &str, a: &Rows, b: &Rows) -> bool {
    if !["SUM(", "TOTAL(", "AVG("].iter().any(|k| up.contains(k)) || a.len() != b.len() {
        return false;
    }
    let close = |x: &V, y: &V| -> bool {
        match (x, y) {
            (V::Real(p), V::Real(q)) => {
                let (p, q) = (f64::from_bits(*p), f64::from_bits(*q));
                p == q
                    || (p.is_finite()
                        && q.is_finite()
                        && (p - q).abs() <= 1e-9 * p.abs().max(q.abs()))
            }
            (V::Real(p), V::Null) | (V::Null, V::Real(p)) => f64::from_bits(*p).is_infinite(),
            _ => x == y,
        }
    };
    let (sa, sb) = (normalize(a.clone(), false), normalize(b.clone(), false));
    let differs = sa != sb;
    differs
        && sa
            .iter()
            .zip(&sb)
            .all(|(ra, rb)| ra.len() == rb.len() && ra.iter().zip(rb).all(|(x, y)| close(x, y)))
}

/// Error-vs-success divergences that are NOT correctness bugs: SQLite
/// limits / messages we don't pin here. Kept intentionally tiny.
fn both_ok_or_both_err(a: &Result<Rows, String>, b: &Result<Rows, String>) -> bool {
    a.is_ok() == b.is_ok()
}

/// Queries whose answers matched only modulo `unspecified_only`.
static UNSPECIFIED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// Queries compared in total.
static COMPARED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// Queries whose answer differed from SQLite's but equals SQLite's own
/// answer under a `NOT INDEXED` plan (see `plan_variant_matches`).
static PLAN_DEPENDENT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// `sql` with `NOT INDEXED` after every table reference in a FROM / JOIN
/// (the fuzzer never aliases tables): the same query under a plan hint
/// SQLite treats as semantics-preserving. `None` when nothing changed.
fn not_indexed_variant(sql: &str) -> Option<String> {
    let b = sql.as_bytes();
    let mut out = String::with_capacity(sql.len() + 32);
    let mut i = 0;
    let mut changed = false;
    while i < b.len() {
        let rest = &sql[i..];
        let kw = ["FROM ", "JOIN "].iter().find(|k| rest.starts_with(**k));
        let boundary = i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_');
        if let (Some(kw), true) = (kw, boundary) {
            let after = &rest[kw.len()..];
            if let Some(t) = ["t1", "t2", "t3"].iter().find(|t| after.starts_with(**t)) {
                let next = after.as_bytes().get(t.len()).copied();
                if !next.is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'.') {
                    out.push_str(kw);
                    out.push_str(t);
                    out.push_str(" NOT INDEXED");
                    i += kw.len() + t.len();
                    changed = true;
                    continue;
                }
            }
        }
        let ch = rest.chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    changed.then_some(out)
}

/// Is our (differing) answer SQLite's own answer under another plan?
/// Which rows a scan visits first decides unspecified things — the
/// representative SQLite keeps for a DISTINCT / GROUP BY class, a
/// `group_concat` without ORDER BY, which term raises first — and SQLite
/// picks covering indexes freely. If forcing table scans (`NOT INDEXED`)
/// makes SQLite produce exactly our answer, ours is one SQLite itself
/// gives for this query; a real bug differs from both plans.
fn plan_variant_matches(
    theirs: &rusqlite::Connection,
    q: &Query,
    ours: &Result<Rows, String>,
) -> bool {
    let Some(alt) = not_indexed_variant(&q.sql) else {
        return false;
    };
    let c = sqlite_rows(theirs, &alt).map(|r| normalize(r, q.ordered));
    match (ours, &c) {
        (Ok(x), Ok(y)) => x == y || unspecified_only(&alt, q.ordered, &q.minmax_cols, x, y),
        (Err(_), Err(_)) => true,
        _ => false,
    }
}

fn run_seed(seed: u64, stmts_per_case: usize) -> Vec<Divergence> {
    let mut rng = Rng::new(seed);
    let ours = std::cell::RefCell::new(Database::open_in_memory().expect("open ours"));
    let theirs = rusqlite::Connection::open_in_memory().expect("open sqlite");
    let mut setup: Vec<String> = Vec::new();
    let mut out = Vec::new();

    for s in SCHEMA {
        ours.borrow_mut().execute(s, []).expect("schema ours");
        theirs.execute(s, []).expect("schema sqlite");
        setup.push(s.to_string());
    }
    // Seed data.
    let n1 = 4 + rng.below(14);
    for _ in 0..n1 {
        let vals: Vec<String> = T1_COLS.iter().map(|_| row_value(&mut rng)).collect();
        let s = format!("INSERT INTO t1 VALUES ({})", vals.join(", "));
        let a = ours.borrow_mut().execute(&s, []).map_err(|e| e.to_string());
        let b = theirs
            .execute(&s, [])
            .map(|_| ())
            .map_err(|e| e.to_string());
        if a.is_ok() != b.is_ok() {
            out.push(Divergence {
                seed,
                setup: setup.clone(),
                sql: s.clone(),
                ours: a.map(|_| vec![]),
                theirs: b.map(|_| vec![]),
            });
        }
        setup.push(s);
    }
    let n2 = 3 + rng.below(10);
    for _ in 0..n2 {
        let s = format!(
            "INSERT INTO t2(g, h, i) VALUES ({}, {}, {})",
            row_value(&mut rng),
            row_value(&mut rng),
            row_value(&mut rng)
        );
        let a = ours.borrow_mut().execute(&s, []).map_err(|e| e.to_string());
        let b = theirs
            .execute(&s, [])
            .map(|_| ())
            .map_err(|e| e.to_string());
        if a.is_ok() != b.is_ok() {
            out.push(Divergence {
                seed,
                setup: setup.clone(),
                sql: s.clone(),
                ours: a.map(|_| vec![]),
                theirs: b.map(|_| vec![]),
            });
        }
        setup.push(s);
    }
    let n3 = 3 + rng.below(10);
    for _ in 0..n3 {
        let s = format!(
            "INSERT OR IGNORE INTO t3 VALUES ({}, {}, {})",
            text_lit(&mut rng),
            row_value(&mut rng),
            row_value(&mut rng)
        );
        let a = ours.borrow_mut().execute(&s, []).map_err(|e| e.to_string());
        let b = theirs
            .execute(&s, [])
            .map(|_| ())
            .map_err(|e| e.to_string());
        if a.is_ok() != b.is_ok() {
            out.push(Divergence {
                seed,
                setup: setup.clone(),
                sql: s.clone(),
                ours: a.map(|_| vec![]),
                theirs: b.map(|_| vec![]),
            });
        }
        setup.push(s);
    }

    for _ in 0..stmts_per_case {
        if rng.chance(20) {
            let (sql, table) = gen_dml(&mut rng);
            let a = ours
                .borrow_mut()
                .execute(&sql, [])
                .map_err(|e| e.to_string());
            let b = theirs
                .execute(&sql, [])
                .map(|_| ())
                .map_err(|e| e.to_string());
            let full = format!("SELECT * FROM {}", table);
            let ra = our_rows(&ours.borrow(), &full).map(|r| normalize(r, false));
            let rb = sqlite_rows(&theirs, &full).map(|r| normalize(r, false));
            if a.is_ok() != b.is_ok() || ra != rb {
                out.push(Divergence {
                    seed,
                    setup: setup.clone(),
                    sql: format!("{}  -- then {}", sql, full),
                    ours: a.and(ra),
                    theirs: b.and(rb),
                });
                // State has diverged; stop this seed.
                return out;
            }
            setup.push(sql);
        } else {
            let q = gen_query(&mut rng);
            COMPARED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let a = our_rows(&ours.borrow(), &q.sql).map(|r| normalize(r, q.ordered));
            let b = sqlite_rows(&theirs, &q.sql).map(|r| normalize(r, q.ordered));
            let same = match (&a, &b) {
                (Ok(x), Ok(y)) if x == y => true,
                (Ok(x), Ok(y)) if unspecified_only(&q.sql, q.ordered, &q.minmax_cols, x, y) => {
                    UNSPECIFIED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    true
                }
                (Ok(_), Ok(_)) => false,
                // sum()'s "integer overflow" fires when the RUNNING total
                // leaves i64 — whether it does depends on the summation
                // order, i.e. on the plan (SQLite may walk an index).
                (Ok(_), Err(e)) | (Err(e), Ok(_))
                    if e.contains("integer overflow")
                        && q.sql.to_ascii_uppercase().contains("SUM(") =>
                {
                    UNSPECIFIED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    true
                }
                _ => both_ok_or_both_err(&a, &b),
            };
            let same = same || {
                let p = plan_variant_matches(&theirs, &q, &a);
                if p {
                    PLAN_DEPENDENT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                p
            };
            if !same {
                out.push(Divergence {
                    seed,
                    setup: setup.clone(),
                    sql: q.sql,
                    ours: a,
                    theirs: b,
                });
            }
        }
    }
    out
}

#[test]
fn strict_differential_vs_sqlite() {
    let base = env_u64("STRICT_FUZZ_SEED", 1);
    let iters = env_u64("STRICT_FUZZ_ITERS", 60);
    let per = env_u64("STRICT_FUZZ_STMTS", 40) as usize;
    let max_report = env_u64("STRICT_FUZZ_MAX_REPORT", 25) as usize;
    let mut all = Vec::new();
    for s in base..base + iters {
        let r = std::panic::catch_unwind(|| run_seed(s, per));
        match r {
            Ok(d) => all.extend(d),
            Err(_) => panic!(
                "engine PANICKED on seed {s} (rerun with STRICT_FUZZ_SEED={s} STRICT_FUZZ_ITERS=1)"
            ),
        }
    }
    for d in all.iter().take(max_report) {
        eprintln!("{}", d);
    }
    eprintln!(
        "strict fuzz: {} queries compared, {} matched only modulo an unspecified representative / tie order, {} matched SQLite's own answer under a NOT INDEXED plan, {} divergences",
        COMPARED.load(std::sync::atomic::Ordering::Relaxed),
        UNSPECIFIED.load(std::sync::atomic::Ordering::Relaxed),
        PLAN_DEPENDENT.load(std::sync::atomic::Ordering::Relaxed),
        all.len()
    );
    assert!(
        all.is_empty(),
        "{} divergences vs SQLite (first {} shown above)",
        all.len(),
        max_report.min(all.len())
    );
}
