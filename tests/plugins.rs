//! Plugin system tests: user-defined scalar functions, aggregates,
//! collations, virtual tables (read + writable), and page codecs —
//! the static Rust registration path (`Database::create_*`).

use rustqlite::plugin::codec::XorCodec;
use rustqlite::plugin::vtab::{
    IndexInfo, ModuleCaps, UpdateOp, VirtualTable, VirtualTableCursor, VirtualTableModule,
    VtabConstraint, VtabConstraintOp,
};
use rustqlite::plugin::{AggCtx, AggState, AggregateFunction, Collation, FnCtx, ScalarFunction};
use rustqlite::types::Value;
use rustqlite::{Database, StepResult};
use std::cmp::Ordering;

// ---------------------------------------------------------------------------
// Scalar functions
// ---------------------------------------------------------------------------

struct Rot13;
impl ScalarFunction for Rot13 {
    fn name(&self) -> &str {
        "rot13"
    }
    fn arity(&self) -> rustqlite::plugin::Arity {
        rustqlite::plugin::Arity::Exact(1)
    }
    fn deterministic(&self) -> bool {
        true
    }
    fn call(&self, _ctx: &FnCtx, args: &[Value]) -> rustqlite::Result<Value> {
        let s = args.get(0).map(|v| v.as_text()).unwrap_or_default();
        let out: String = s
            .chars()
            .map(|c| match c {
                'a'..='z' => char::from(b'a' + ((c as u8 - b'a' + 13) % 26)),
                'A'..='Z' => char::from(b'A' + ((c as u8 - b'A' + 13) % 26)),
                other => other,
            })
            .collect();
        Ok(Value::Text(out.into()))
    }
}

#[test]
fn scalar_function_basic() {
    let mut db = Database::open_in_memory().unwrap();
    db.create_function(Rot13).unwrap();
    let rows = db.query("SELECT rot13('hello')", []).unwrap();
    assert_eq!(rows[0][0].as_text(), "uryyb");
    // Double application round-trips.
    let rows = db
        .query("SELECT rot13(rot13('Uryyb, Jbeyq!'))", [])
        .unwrap();
    assert_eq!(rows[0][0].as_text(), "Uryyb, Jbeyq!");
}

#[test]
fn scalar_function_in_where_and_with_params() {
    let mut db = Database::open_in_memory().unwrap();
    db.create_function(Rot13).unwrap();
    db.execute("CREATE TABLE t (word TEXT)", []).unwrap();
    db.execute(
        "INSERT INTO t (word) VALUES ('apple'), ('uryyb'), ('banana')",
        [],
    )
    .unwrap();
    // rot13('hello') = 'uryyb' — the WHERE clause finds it.
    let rows = db
        .query(
            "SELECT word FROM t WHERE word = rot13(?)",
            vec![Value::Text("hello".into())],
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0].as_text(), "uryyb");
}

#[test]
fn scalar_function_arity_enforced() {
    let mut db = Database::open_in_memory().unwrap();
    db.create_function(Rot13).unwrap();
    // rot13 declared with Arity::Exact(1): two args must error, not
    // silently misbehave.
    let err = db.query("SELECT rot13('a', 'b')", []).unwrap_err();
    assert!(err.to_string().contains("wrong number of arguments"));
}

#[test]
fn builtin_cannot_be_shadowed() {
    let mut db = Database::open_in_memory().unwrap();
    struct FakeAbs;
    impl ScalarFunction for FakeAbs {
        fn name(&self) -> &str {
            "abs"
        }
        fn call(&self, _ctx: &FnCtx, _args: &[Value]) -> rustqlite::Result<Value> {
            Ok(Value::Integer(42))
        }
    }
    let err = db.create_function(FakeAbs).unwrap_err();
    assert!(err.to_string().contains("built-in"));
}

// ---------------------------------------------------------------------------
// Aggregates
// ---------------------------------------------------------------------------

struct Median;
impl AggregateFunction for Median {
    fn name(&self) -> &str {
        "median"
    }
    fn arity(&self) -> rustqlite::plugin::Arity {
        rustqlite::plugin::Arity::Exact(1)
    }
    fn init(&self) -> Box<dyn AggState> {
        Box::new(MedianState { vals: Vec::new() })
    }
}

struct MedianState {
    vals: Vec<f64>,
}

impl AggState for MedianState {
    fn step(&mut self, _ctx: &AggCtx, args: &[Value]) -> rustqlite::Result<()> {
        if let Some(v) = args.first() {
            if !v.is_null() {
                self.vals.push(v.as_real());
            }
        }
        Ok(())
    }
    fn value(&self) -> rustqlite::Result<Value> {
        if self.vals.is_empty() {
            return Ok(Value::Null);
        }
        let mut v = self.vals.clone();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let n = v.len();
        let med = if n % 2 == 1 {
            v[n / 2]
        } else {
            (v[n / 2 - 1] + v[n / 2]) / 2.0
        };
        Ok(Value::Real(med))
    }
}

#[test]
fn aggregate_median_ungrouped() {
    let mut db = Database::open_in_memory().unwrap();
    db.create_aggregate(Median).unwrap();
    db.execute("CREATE TABLE t (x INTEGER)", []).unwrap();
    db.execute("INSERT INTO t (x) VALUES (1), (2), (3), (4), (100)", [])
        .unwrap();
    let rows = db.query("SELECT median(x) FROM t", []).unwrap();
    assert_eq!(rows[0][0].as_real(), 3.0);
}

#[test]
fn aggregate_median_grouped_and_empty() {
    let mut db = Database::open_in_memory().unwrap();
    db.create_aggregate(Median).unwrap();
    db.execute("CREATE TABLE t (g TEXT, x INTEGER)", [])
        .unwrap();
    db.execute(
        "INSERT INTO t (g, x) VALUES ('a', 1), ('a', 2), ('a', 100), ('b', 10), ('b', 20)",
        [],
    )
    .unwrap();
    let rows = db
        .query("SELECT g, median(x) FROM t GROUP BY g ORDER BY g", [])
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0].as_text(), "a");
    assert_eq!(rows[0][1].as_real(), 2.0);
    assert_eq!(rows[1][1].as_real(), 15.0);

    // Empty input with no GROUP BY → one row (SQLite semantics).
    let rows = db
        .query("SELECT median(x) FROM t WHERE g = 'zzz'", [])
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0][0].is_null());
}

#[test]
fn aggregate_mixed_with_builtin() {
    let mut db = Database::open_in_memory().unwrap();
    db.create_aggregate(Median).unwrap();
    db.execute("CREATE TABLE t (x INTEGER)", []).unwrap();
    db.execute("INSERT INTO t (x) VALUES (1), (2), (3)", [])
        .unwrap();
    let rows = db
        .query("SELECT count(*), median(x), sum(x) FROM t", [])
        .unwrap();
    assert_eq!(rows[0][0].as_integer(), 3);
    assert_eq!(rows[0][1].as_real(), 2.0);
    assert_eq!(rows[0][2].as_integer(), 6);
}

// ---------------------------------------------------------------------------
// Collations
// ---------------------------------------------------------------------------

#[test]
fn nocase_builtin_order_by() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (name TEXT)", []).unwrap();
    db.execute(
        "INSERT INTO t (name) VALUES ('apple'), ('Banana'), ('cherry'), ('Date')",
        [],
    )
    .unwrap();
    let rows = db
        .query("SELECT name FROM t ORDER BY name COLLATE NOCASE", [])
        .unwrap();
    let got: Vec<String> = rows.iter().map(|r| r[0].as_text()).collect();
    assert_eq!(got, vec!["apple", "Banana", "cherry", "Date"]);

    // Without the collation: uppercase sorts before lowercase (BINARY).
    let rows = db.query("SELECT name FROM t ORDER BY name", []).unwrap();
    let got: Vec<String> = rows.iter().map(|r| r[0].as_text()).collect();
    assert_eq!(got, vec!["Banana", "Date", "apple", "cherry"]);
}

#[test]
fn nocase_comparison_operators() {
    let db = Database::open_in_memory().unwrap();
    let rows = db
        .query("SELECT 'HELLO' = 'hello' COLLATE NOCASE", [])
        .unwrap();
    assert_eq!(rows[0][0].as_integer(), 1);
    let rows = db
        .query("SELECT 'HELLO' < 'hello' COLLATE NOCASE", [])
        .unwrap();
    assert_eq!(rows[0][0].as_integer(), 0);
}

#[test]
fn rtrim_collation() {
    let db = Database::open_in_memory().unwrap();
    let rows = db.query("SELECT 'ab  ' = 'ab' COLLATE RTRIM", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 1);
}

struct ReverseColl;
impl Collation for ReverseColl {
    fn name(&self) -> &str {
        "REVERSE"
    }
    fn compare(&self, a: &str, b: &str) -> Ordering {
        let ra: String = a.chars().rev().collect();
        let rb: String = b.chars().rev().collect();
        ra.cmp(&rb)
    }
}

#[test]
fn custom_collation_registration() {
    let mut db = Database::open_in_memory().unwrap();
    db.create_collation(ReverseColl).unwrap();
    db.execute("CREATE TABLE t (w TEXT)", []).unwrap();
    db.execute(
        "INSERT INTO t (w) VALUES ('ab'), ('ba'), ('ca'), ('ac')",
        [],
    )
    .unwrap();
    // REVERSE sorts by the reversed string: ab->ba, ba->ab, ca->ac, ac->ca
    // order: ab(ba), ac(ca), ba(ab), ca(ac).
    let rows = db
        .query("SELECT w FROM t ORDER BY w COLLATE REVERSE", [])
        .unwrap();
    let got: Vec<String> = rows.iter().map(|r| r[0].as_text()).collect();
    assert_eq!(got, vec!["ba", "ca", "ab", "ac"]);
}

// ---------------------------------------------------------------------------
// Virtual tables
// ---------------------------------------------------------------------------

/// generate-series style module: rows 0..=n with best_index handling of
/// `n >= ?`.
struct SeriesModule;

impl VirtualTableModule for SeriesModule {
    fn name(&self) -> &str {
        "series"
    }
    fn caps(&self) -> u32 {
        ModuleCaps::EPHEMERAL
    }
    fn create(&self, table: &str, args: &[String]) -> rustqlite::Result<Box<dyn VirtualTable>> {
        let n: i64 = args.first().and_then(|a| a.parse().ok()).unwrap_or(10);
        let _ = table;
        Ok(Box::new(SeriesTable { end: n }))
    }
    fn connect(&self, table: &str, args: &[String]) -> rustqlite::Result<Box<dyn VirtualTable>> {
        self.create(table, args)
    }
}

struct SeriesTable {
    end: i64,
}

impl VirtualTable for SeriesTable {
    fn columns(&self) -> Vec<(String, String)> {
        vec![
            ("n".into(), "INTEGER".into()),
            ("label".into(), "TEXT".into()),
        ]
    }

    fn best_index(&self, constraints: &[VtabConstraint]) -> rustqlite::Result<IndexInfo> {
        let mut info = IndexInfo::full_scan(constraints.len());
        for (i, c) in constraints.iter().enumerate() {
            if c.column == Some(0)
                && matches!(
                    c.op,
                    VtabConstraintOp::Eq | VtabConstraintOp::Ge | VtabConstraintOp::Gt
                )
            {
                info.handled[i] = true;
            }
        }
        info.idx_num = 1;
        info.estimated_rows = self.end;
        info.estimated_cost = 10.0;
        Ok(info)
    }

    fn open(&self) -> rustqlite::Result<Box<dyn VirtualTableCursor>> {
        Ok(Box::new(SeriesCursor {
            current: 0,
            end: self.end,
        }))
    }
}

struct SeriesCursor {
    current: i64,
    end: i64,
}

impl VirtualTableCursor for SeriesCursor {
    fn filter(
        &mut self,
        idx_num: usize,
        _idx_str: Option<&str>,
        args: &[Value],
    ) -> rustqlite::Result<()> {
        self.current = 0;
        if idx_num == 1 {
            if let Some(v) = args.first() {
                self.current = v.as_integer();
            }
        }
        Ok(())
    }
    fn next(&mut self) -> rustqlite::Result<()> {
        self.current += 1;
        Ok(())
    }
    fn eof(&self) -> bool {
        self.current > self.end
    }
    fn column(&self, i: usize) -> rustqlite::Result<Value> {
        match i {
            0 => Ok(Value::Integer(self.current)),
            _ => Ok(Value::Text(format!("row-{}", self.current).into())),
        }
    }
    fn rowid(&self) -> rustqlite::Result<i64> {
        Ok(self.current)
    }
}

#[test]
fn vtab_scan_and_projection() {
    let mut db = Database::open_in_memory().unwrap();
    db.create_module(SeriesModule).unwrap();
    db.execute("CREATE VIRTUAL TABLE s USING series(5)", [])
        .unwrap();
    let rows = db.query("SELECT n FROM s", []).unwrap();
    let got: Vec<i64> = rows.iter().map(|r| r[0].as_integer()).collect();
    assert_eq!(got, vec![0, 1, 2, 3, 4, 5]);

    // Projection of both columns.
    let rows = db.query("SELECT n, label FROM s WHERE n < 2", []).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1][1].as_text(), "row-1");
}

#[test]
fn vtab_with_rows_and_join() {
    let mut db = Database::open_in_memory().unwrap();
    db.create_module(SeriesModule).unwrap();
    db.execute("CREATE VIRTUAL TABLE s USING series(3)", [])
        .unwrap();
    // WHERE with residual + handled constraints.
    let rows = db
        .query("SELECT n FROM s WHERE n >= 2 AND label <> 'x'", [])
        .unwrap();
    let got: Vec<i64> = rows.iter().map(|r| r[0].as_integer()).collect();
    assert_eq!(got, vec![2, 3]);
    // Joins treat the vtab like any table.
    db.execute("CREATE TABLE m (k INTEGER, v TEXT)", [])
        .unwrap();
    db.execute("INSERT INTO m (k, v) VALUES (1, 'one'), (2, 'two')", [])
        .unwrap();
    let rows = db
        .query(
            "SELECT s.n, m.v FROM s JOIN m ON s.n = m.k ORDER BY s.n",
            [],
        )
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][1].as_text(), "one");
}

#[test]
fn vtab_if_not_exists_and_namespace_conflict() {
    let mut db = Database::open_in_memory().unwrap();
    db.create_module(SeriesModule).unwrap();
    db.execute("CREATE TABLE taken (x INTEGER)", []).unwrap();
    let err = db
        .execute("CREATE VIRTUAL TABLE taken USING series(2)", [])
        .unwrap_err();
    assert!(err.to_string().contains("already exists"));
    db.execute(
        "CREATE VIRTUAL TABLE IF NOT EXISTS taken USING series(2)",
        [],
    )
    .unwrap(); // no error
}

#[test]
fn vtab_create_without_module_fails() {
    let mut db = Database::open_in_memory().unwrap();
    let err = db
        .execute("CREATE VIRTUAL TABLE s USING nosuchmod(1)", [])
        .unwrap_err();
    assert!(err.to_string().contains("no such module"));
}

#[test]
fn vtab_drop_table() {
    let mut db = Database::open_in_memory().unwrap();
    db.create_module(SeriesModule).unwrap();
    db.execute("CREATE VIRTUAL TABLE s USING series(2)", [])
        .unwrap();
    db.execute("DROP TABLE s", []).unwrap();
    let err = db.query("SELECT * FROM s", []).unwrap_err();
    assert!(err.to_string().contains("table"));
}

#[test]
fn vtab_persist_and_reconnect_via_create_module() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v.db");
    {
        let mut db = Database::open(&path).unwrap();
        db.create_module(SeriesModule).unwrap();
        db.execute("CREATE VIRTUAL TABLE s USING series(4)", [])
            .unwrap();
        db.execute("CREATE TABLE plain (id INTEGER PRIMARY KEY, x INTEGER)", [])
            .unwrap();
        db.execute("INSERT INTO plain (x) VALUES (10)", []).unwrap();
    }
    // Reopen WITHOUT the module: the schema row loads as a PENDING vtab
    // (empty columns); queries over it fail with "no such module".
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("INSERT INTO plain (x) VALUES (20)", []).unwrap();
        let rows = db.query("SELECT x FROM plain", []).unwrap();
        assert_eq!(rows.len(), 2);
        let err = db.query("SELECT n FROM s", []).unwrap_err();
        assert!(err.to_string().contains("no such module"));
        // Register the module → pending vtabs connect, queries work.
        db.create_module(SeriesModule).unwrap();
        let rows = db.query("SELECT n FROM s", []).unwrap();
        let got: Vec<i64> = rows.iter().map(|r| r[0].as_integer()).collect();
        assert_eq!(got, vec![0, 1, 2, 3, 4]);
    }
}

/// A writable kv module: INSERT/UPDATE/DELETE through xUpdate.
struct KvModule;

impl VirtualTableModule for KvModule {
    fn name(&self) -> &str {
        "kv"
    }
    fn caps(&self) -> u32 {
        ModuleCaps::WRITABLE | ModuleCaps::EPHEMERAL
    }
    fn create(&self, _table: &str, _args: &[String]) -> rustqlite::Result<Box<dyn VirtualTable>> {
        Ok(Box::new(KvTable {
            rows: Vec::new(),
            next_rowid: 1,
        }))
    }
}

struct KvTable {
    rows: Vec<(i64, String, String)>,
    next_rowid: i64,
}

impl VirtualTable for KvTable {
    fn columns(&self) -> Vec<(String, String)> {
        vec![("k".into(), "TEXT".into()), ("v".into(), "TEXT".into())]
    }
    fn best_index(&self, constraints: &[VtabConstraint]) -> rustqlite::Result<IndexInfo> {
        let mut info = IndexInfo::full_scan(constraints.len());
        for (i, c) in constraints.iter().enumerate() {
            if c.column == Some(0) && c.op == VtabConstraintOp::Eq {
                info.handled[i] = true;
            }
        }
        info.idx_num = 2;
        info.estimated_rows = 100;
        info.estimated_cost = 5.0;
        Ok(info)
    }
    fn open(&self) -> rustqlite::Result<Box<dyn VirtualTableCursor>> {
        // Cursors iterate a snapshot (rows cloned) — simple and safe for
        // the DML flow, which collects matching rows before xUpdate.
        Ok(Box::new(KvCursor {
            rows: self.rows.clone(),
            pos: 0,
        }))
    }
    fn update(&mut self, ops: Vec<UpdateOp>) -> rustqlite::Result<Vec<Option<i64>>> {
        let mut out = Vec::new();
        for op in ops {
            match (op.old_rowid, op.new_rowid, &op.columns) {
                // INSERT
                (None, _, cols) if !cols.is_empty() => {
                    let k = cols[0].clone().map(|v| v.as_text()).unwrap_or_default();
                    let v = cols[1].clone().map(|v| v.as_text()).unwrap_or_default();
                    // Upsert on key.
                    if let Some(slot) = self.rows.iter_mut().find(|r| r.1 == k) {
                        slot.2 = v;
                        out.push(Some(slot.0));
                    } else {
                        let rid = self.next_rowid;
                        self.next_rowid += 1;
                        self.rows.push((rid, k, v));
                        out.push(Some(rid));
                    }
                }
                // DELETE
                (Some(rid), None, cols) if cols.is_empty() => {
                    self.rows.retain(|r| r.0 != rid);
                    out.push(None);
                }
                // UPDATE
                (Some(rid), Some(_), cols) => {
                    if let Some(slot) = self.rows.iter_mut().find(|r| r.0 == rid) {
                        if let Some(Some(k)) = cols.first() {
                            slot.1 = k.as_text();
                        }
                        if let Some(Some(v)) = cols.get(1) {
                            slot.2 = v.as_text();
                        }
                    }
                    out.push(None);
                }
                _ => out.push(None),
            }
        }
        Ok(out)
    }
}

struct KvCursor {
    rows: Vec<(i64, String, String)>,
    pos: usize,
}

impl VirtualTableCursor for KvCursor {
    fn filter(
        &mut self,
        idx_num: usize,
        _s: Option<&str>,
        args: &[Value],
    ) -> rustqlite::Result<()> {
        if idx_num == 2 {
            if let Some(v) = args.first() {
                let k = v.as_text();
                self.pos = 0;
                self.rows.retain(|r| r.1 == k);
            }
        }
        Ok(())
    }
    fn next(&mut self) -> rustqlite::Result<()> {
        self.pos += 1;
        Ok(())
    }
    fn eof(&self) -> bool {
        self.pos >= self.rows.len()
    }
    fn column(&self, i: usize) -> rustqlite::Result<Value> {
        Ok(match i {
            0 => Value::Text(self.rows[self.pos].1.clone().into()),
            _ => Value::Text(self.rows[self.pos].2.clone().into()),
        })
    }
    fn rowid(&self) -> rustqlite::Result<i64> {
        Ok(self.rows[self.pos].0)
    }
}

#[test]
fn vtab_insert_update_delete() {
    let mut db = Database::open_in_memory().unwrap();
    db.create_module(KvModule).unwrap();
    db.execute("CREATE VIRTUAL TABLE kvstore USING kv()", [])
        .unwrap();
    db.execute(
        "INSERT INTO kvstore (k, v) VALUES ('a', '1'), ('b', '2')",
        [],
    )
    .unwrap();
    let rows = db.query("SELECT k, v FROM kvstore ORDER BY k", []).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][1].as_text(), "1");

    // Point lookup through best_index.
    let rows = db.query("SELECT v FROM kvstore WHERE k = 'b'", []).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0].as_text(), "2");

    // UPDATE via xUpdate.
    db.execute("UPDATE kvstore SET v = '20' WHERE k = 'b'", [])
        .unwrap();
    let rows = db.query("SELECT v FROM kvstore WHERE k = 'b'", []).unwrap();
    assert_eq!(rows[0][0].as_text(), "20");

    // DELETE via xUpdate.
    db.execute("DELETE FROM kvstore WHERE k = 'a'", []).unwrap();
    let rows = db.query("SELECT k FROM kvstore", []).unwrap();
    assert_eq!(rows.len(), 1);

    // Upsert-style reinsert.
    db.execute("INSERT INTO kvstore (k, v) VALUES ('b', '99')", [])
        .unwrap();
    let rows = db.query("SELECT v FROM kvstore", []).unwrap();
    assert_eq!(rows[0][0].as_text(), "99");
}

#[test]
fn vtab_readonly_module_rejects_writes() {
    let mut db = Database::open_in_memory().unwrap();
    db.create_module(SeriesModule).unwrap();
    db.execute("CREATE VIRTUAL TABLE s USING series(2)", [])
        .unwrap();
    let err = db.execute("INSERT INTO s (n) VALUES (5)", []).unwrap_err();
    assert!(err.to_string().contains("read-only"));
}

#[test]
fn vtab_aggregate_over_scan() {
    let mut db = Database::open_in_memory().unwrap();
    db.create_module(SeriesModule).unwrap();
    db.execute("CREATE VIRTUAL TABLE s USING series(10)", [])
        .unwrap();
    let rows = db
        .query("SELECT count(*), sum(n), min(n), max(n) FROM s", [])
        .unwrap();
    assert_eq!(rows[0][0].as_integer(), 11);
    assert_eq!(rows[0][1].as_integer(), 55);
    assert_eq!(rows[0][2].as_integer(), 0);
    assert_eq!(rows[0][3].as_integer(), 10);
}

#[test]
fn vtab_through_streaming_statement() {
    let mut db = Database::open_in_memory().unwrap();
    db.create_module(SeriesModule).unwrap();
    db.execute("CREATE VIRTUAL TABLE s USING series(100)", [])
        .unwrap();
    let mut stmt = db.prepare("SELECT n FROM s").unwrap();
    let mut count = 0;
    let mut last = -1;
    while stmt.step().unwrap() == StepResult::Row {
        last = stmt.column_int(0);
        count += 1;
    }
    assert_eq!(count, 101);
    assert_eq!(last, 100);
}

// ---------------------------------------------------------------------------
// Page codecs
// ---------------------------------------------------------------------------

#[test]
fn codec_xor_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("coded.db");
    {
        let mut db = Database::open(&path).unwrap();
        db.create_codec(XorCodec::new(0x5A)).unwrap();
        db.execute("PRAGMA codec = xor", []).unwrap();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x TEXT)", [])
            .unwrap();
        for i in 0..100 {
            db.execute(
                "INSERT INTO t (x) VALUES (?)",
                vec![Value::Text(format!("value-{}", i).into())],
            )
            .unwrap();
        }
        db.flush().unwrap();
    }
    // The raw file must NOT contain the plain text (XOR obfuscated).
    let raw = std::fs::read(&path).unwrap();
    assert!(!raw.windows(6).any(|w| w == b"value-"));
    // Plain open refuses with the marker error.
    let err = match Database::open(&path) {
        Err(e) => e,
        Ok(_) => panic!("plain open of a coded file must fail"),
    };
    assert!(err.to_string().contains("codec"), "{}", err);
    // open_with_codec round-trips.
    let db = Database::open_with_codec(&path, XorCodec::new(0x5A)).unwrap();
    let rows = db.query("SELECT count(*) FROM t", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 100);
    let rows = db.query("SELECT x FROM t WHERE id = 42", []).unwrap();
    assert_eq!(rows[0][0].as_text(), "value-41"); // rowid 42 holds value-41 (rowids start at 1)
}

#[test]
fn codec_wrong_key_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("coded.db");
    {
        let mut db = Database::open(&path).unwrap();
        db.create_codec(XorCodec::new(0x5A)).unwrap();
        db.execute("PRAGMA codec = xor", []).unwrap();
        db.execute("CREATE TABLE t (x TEXT)", []).unwrap();
        db.execute("INSERT INTO t (x) VALUES ('hello')", [])
            .unwrap();
        db.flush().unwrap();
    }
    // The codec NAME matches ("xor"), so the marker check passes — the
    // wrong key then decodes garbage and the schema load fails with a
    // corruption/format error. Any error is the correct outcome: the
    // database must not open as if it were valid.
    let opened = Database::open_with_codec(&path, XorCodec::new(0x11));
    assert!(opened.is_err(), "wrong-key decode must fail somewhere");
}

#[test]
fn codec_pragma_read_form() {
    let mut db = Database::open_in_memory().unwrap();
    db.create_codec(XorCodec::new(0x5A)).unwrap();
    let rows = db.query("PRAGMA codec", []).unwrap();
    assert_eq!(rows[0][0].as_text(), "none");
    db.execute("PRAGMA codec = xor", []).unwrap();
    let rows = db.query("PRAGMA codec", []).unwrap();
    assert_eq!(rows[0][0].as_text(), "xor");
    db.execute("PRAGMA codec = none", []).unwrap();
    let rows = db.query("PRAGMA codec", []).unwrap();
    assert_eq!(rows[0][0].as_text(), "none");
}

// ---------------------------------------------------------------------------
// Introspection
// ---------------------------------------------------------------------------

#[test]
fn plugin_registry_introspection() {
    let mut db = Database::open_in_memory().unwrap();
    db.create_function(Rot13).unwrap();
    db.create_aggregate(Median).unwrap();
    let reg = db.plugin_registry();
    let names = reg.function_names();
    assert!(names.contains(&"rot13".to_string()));
    assert!(names.contains(&"median".to_string()));
}
