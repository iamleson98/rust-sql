//! Cross-database triggers — TEMP triggers that target (or reference)
//! ATTACHED databases (`src/attach_fire.rs`).
//!
//! Every behavior below was pinned against the real SQLite 3.53.4
//! oracle (the vendored amalgamation, compiled as /tmp oracle — see the
//! worklog) BEFORE being asserted here. The oracle programs:
//!
//! - `CREATE TEMP TRIGGER trg AFTER INSERT ON aux.t2 BEGIN INSERT INTO
//!   main.log VALUES (NEW.a); END;` — allowed, fires per row, NEW/OLD
//!   visible, RAISE aborts the whole statement.
//! - `CREATE TRIGGER trg_b ... BEGIN INSERT INTO log SELECT b FROM
//!   aux.u; END;` (NON-temp, cross-db body) — REJECTED: "trigger trg_b
//!   cannot reference objects in database aux".
//! - TEMP trigger bodies may carry QUALIFIED DML targets (the one place
//!   SQLite allows them); bare body names bind through the connection
//!   search order (temp -> main -> attached — main.same wins over
//!   aux.same).
//! - DETACH leaves the trigger listed but DORMANT forever — a re-ATTACH
//!   (even of the same file) never re-arms it.
//! - `CREATE TEMP TRIGGER ... ON aux.nosuch` errors at CREATE time:
//!   "no such table: aux.nosuch"; a bare ON name that misses
//!   everywhere: "no such table: nosuch".
//! - An unqualified ON name with same-named tables in main and aux
//!   binds MAIN (the search order), never the attached fallback.

use rustqlite::{Database, Value};

fn mem() -> Database {
    Database::open_in_memory().expect("open")
}

fn rows(db: &Database, sql: &str) -> Vec<Vec<String>> {
    let (cols, rs) = db.query_with_columns(sql, []).expect(sql);
    let _ = cols;
    rs.iter()
        .map(|r| {
            r.iter()
                .map(|v| match v {
                    Value::Null => "NULL".to_string(),
                    Value::Integer(i) => i.to_string(),
                    Value::Real(f) => f.to_string(),
                    Value::Text(t) => t.to_string(),
                    Value::Blob(b) => format!("x'{}'", hex(b)),
                })
                .collect()
        })
        .collect()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{:02x}", x)).collect()
}

fn err_of(db: &mut Database, sql: &str) -> String {
    match db.execute(sql, []) {
        Ok(()) => "OK".to_string(),
        Err(e) => e.to_string(),
    }
}

/// The parent + aux fixture every test starts from.
fn fixture() -> Database {
    let mut db = mem();
    db.execute("CREATE TABLE log (x TEXT)", []).unwrap();
    db.execute("ATTACH ':memory:' AS aux", []).unwrap();
    db.execute("CREATE TABLE aux.t2 (a INT, b INT)", [])
        .unwrap();
    db
}

// ---------------------------------------------------------------------------
// Form (a): TEMP triggers ON attached tables (the routed-DML driver)
// ---------------------------------------------------------------------------

#[test]
fn temp_trigger_on_attached_table_fires_all_events() {
    let mut db = fixture();
    db.execute(
        "CREATE TEMP TRIGGER trg_ins AFTER INSERT ON aux.t2 \
         BEGIN INSERT INTO main.log VALUES ('ins:' || NEW.a); END",
        [],
    )
    .unwrap();
    db.execute(
        "CREATE TEMP TRIGGER trg_upd AFTER UPDATE ON aux.t2 \
         BEGIN INSERT INTO main.log VALUES ('upd:' || OLD.a || '->' || NEW.a); END",
        [],
    )
    .unwrap();
    db.execute(
        "CREATE TEMP TRIGGER trg_del AFTER DELETE ON aux.t2 \
         BEGIN INSERT INTO main.log VALUES ('del:' || OLD.a); END",
        [],
    )
    .unwrap();

    db.execute("INSERT INTO aux.t2 VALUES (1, 10)", []).unwrap();
    db.execute("UPDATE aux.t2 SET a = 2 WHERE a = 1", [])
        .unwrap();
    db.execute("DELETE FROM aux.t2 WHERE a = 2", []).unwrap();

    // The oracle's exact output for the same program (3.53.4):
    // ins:1 / upd:1->2 / del:2.
    assert_eq!(
        rows(&db, "SELECT x FROM main.log"),
        vec![vec!["ins:1"], vec!["upd:1->2"], vec!["del:2"]]
    );
    // The write itself landed (and only once).
    assert_eq!(rows(&db, "SELECT count(*) FROM aux.t2"), vec![vec!["0"]]);
}

#[test]
fn temp_trigger_when_guard_and_before_raise() {
    let mut db = fixture();
    db.execute(
        "CREATE TEMP TRIGGER g AFTER INSERT ON aux.t2 WHEN NEW.a > 10 \
         BEGIN INSERT INTO main.log VALUES ('big'); END",
        [],
    )
    .unwrap();
    db.execute("INSERT INTO aux.t2 VALUES (5, 0)", []).unwrap();
    db.execute("INSERT INTO aux.t2 VALUES (50, 0)", []).unwrap();
    assert_eq!(rows(&db, "SELECT x FROM main.log"), vec![vec!["big"]]);

    // BEFORE + RAISE(ABORT) vetoes the write and empties the statement
    // (the oracle: the aux row count stays at the pre-statement value).
    db.execute(
        "CREATE TEMP TRIGGER r1 BEFORE INSERT ON aux.t2 WHEN NEW.a = 99 \
         BEGIN SELECT RAISE(ABORT, 'nope'); END",
        [],
    )
    .unwrap();
    let err = err_of(&mut db, "INSERT INTO aux.t2 VALUES (99, 0)");
    assert!(err.contains("RAISE"), "err: {err}");
    assert_eq!(
        rows(&db, "SELECT count(*) FROM aux.t2"),
        vec![vec!["2"]], // 5 and 50; the vetoed 99 never landed
    );
}

#[test]
fn temp_trigger_statement_atomicity_across_engines() {
    let mut db = fixture();
    db.execute(
        "CREATE TEMP TRIGGER g AFTER INSERT ON aux.t2 \
         BEGIN INSERT INTO main.log VALUES ('g:' || NEW.a); END",
        [],
    )
    .unwrap();
    db.execute(
        "CREATE TEMP TRIGGER boom AFTER INSERT ON aux.t2 WHEN NEW.a = 66 \
         BEGIN SELECT RAISE(ABORT, 'nope'); END",
        [],
    )
    .unwrap();
    let err = err_of(&mut db, "INSERT INTO aux.t2 VALUES (30, 0), (66, 0)");
    assert!(err.contains("RAISE"), "err: {err}");
    // Row 30 AND its trigger write rolled back with the statement
    // (SQLite's multi-database statement journal semantics).
    assert_eq!(rows(&db, "SELECT count(*) FROM aux.t2"), vec![vec!["0"]]);
    assert_eq!(rows(&db, "SELECT count(*) FROM main.log"), vec![vec!["0"]]);
}

#[test]
fn temp_trigger_transaction_rollback_spans_engines() {
    let mut db = fixture();
    db.execute(
        "CREATE TEMP TRIGGER g AFTER INSERT ON aux.t2 \
         BEGIN INSERT INTO main.log VALUES ('tx:' || NEW.a); END",
        [],
    )
    .unwrap();
    db.execute("BEGIN", []).unwrap();
    db.execute("INSERT INTO aux.t2 VALUES (5, 0)", []).unwrap();
    assert_eq!(rows(&db, "SELECT count(*) FROM main.log"), vec![vec!["1"]]);
    db.execute("ROLLBACK", []).unwrap();
    // BOTH engines rolled back (BEGIN/ROLLBACK propagate to attached).
    assert_eq!(rows(&db, "SELECT count(*) FROM aux.t2"), vec![vec!["0"]]);
    assert_eq!(rows(&db, "SELECT count(*) FROM main.log"), vec![vec!["0"]]);
}

#[test]
fn temp_trigger_detach_dormancy() {
    let mut db = fixture();
    db.execute(
        "CREATE TEMP TRIGGER trg AFTER INSERT ON aux.t2 \
         BEGIN INSERT INTO main.log VALUES ('fire'); END",
        [],
    )
    .unwrap();
    db.execute("INSERT INTO aux.t2 VALUES (1, 0)", []).unwrap();
    assert_eq!(rows(&db, "SELECT count(*) FROM main.log"), vec![vec!["1"]]);

    // DETACH is allowed with a live trigger bound to the database (the
    // oracle detaches cleanly); the trigger never fires again — not even
    // after a re-ATTACH of the same database under the same name.
    db.execute("DETACH aux", []).unwrap();
    db.execute("ATTACH ':memory:' AS aux", []).unwrap();
    db.execute("CREATE TABLE aux.t2 (a INT, b INT)", [])
        .unwrap();
    db.execute("INSERT INTO aux.t2 VALUES (2, 0)", []).unwrap();
    assert_eq!(rows(&db, "SELECT count(*) FROM main.log"), vec![vec!["1"]]);

    // DROP TRIGGER still finds the dormant trigger (temp search order).
    db.execute("DROP TRIGGER trg", []).unwrap();
    let err = err_of(&mut db, "DROP TRIGGER trg");
    assert!(err.contains("no such trigger"), "err: {err}");
}

#[test]
fn temp_trigger_create_time_validation() {
    let mut db = fixture();
    // Qualified missing table (the oracle's exact text).
    let err = err_of(
        &mut db,
        "CREATE TEMP TRIGGER bad BEFORE INSERT ON aux.nosuch BEGIN SELECT 1; END",
    );
    assert_eq!(err, "no such table: aux.nosuch");
    // Bare name that misses everywhere.
    let err = err_of(
        &mut db,
        "CREATE TEMP TRIGGER bad2 BEFORE INSERT ON nosuch BEGIN SELECT 1; END",
    );
    assert_eq!(err, "no such table: nosuch");
    // A qualified MAIN table is fine.
    db.execute("CREATE TABLE mt (a INT)", []).unwrap();
    db.execute(
        "CREATE TEMP TRIGGER ok AFTER INSERT ON main.mt BEGIN SELECT 1; END",
        [],
    )
    .unwrap();
}

#[test]
fn temp_trigger_shadow_binding_prefers_main() {
    let mut db = fixture();
    db.execute("CREATE TABLE main.t2 (a INT)", []).unwrap();
    // The bare ON name binds MAIN (temp -> main -> attached), so the
    // trigger fires for main writes and never for aux writes.
    db.execute(
        "CREATE TEMP TRIGGER sh AFTER INSERT ON t2 \
         BEGIN INSERT INTO main.log VALUES ('shadow'); END",
        [],
    )
    .unwrap();
    db.execute("INSERT INTO main.t2 VALUES (1)", []).unwrap();
    assert_eq!(rows(&db, "SELECT count(*) FROM main.log"), vec![vec!["1"]]);
    db.execute("INSERT INTO aux.t2 VALUES (2, 0)", []).unwrap();
    assert_eq!(rows(&db, "SELECT count(*) FROM main.log"), vec![vec!["1"]]);
}

#[test]
fn temp_trigger_bare_body_names_search_order() {
    let mut db = fixture();
    // `audit` exists ONLY in aux: a bare body name binds through the
    // connection search order (main misses, attached answers) — the
    // oracle inserted into MAIN.same when both existed, into the only
    // candidate otherwise.
    db.execute("CREATE TABLE aux.audit (msg TEXT)", []).unwrap();
    db.execute(
        "CREATE TEMP TRIGGER tr AFTER UPDATE ON aux.t2 \
         BEGIN INSERT INTO audit VALUES ('fired'); END",
        [],
    )
    .unwrap();
    db.execute("INSERT INTO aux.t2 VALUES (7, 0)", []).unwrap();
    db.execute("UPDATE aux.t2 SET a = a", []).unwrap();
    assert_eq!(rows(&db, "SELECT msg FROM aux.audit"), vec![vec!["fired"]]);
}

#[test]
fn mixed_insert_into_bound_table_fires_per_row() {
    let mut db = fixture();
    db.execute("CREATE TABLE main.src (v INT)", []).unwrap();
    db.execute("INSERT INTO main.src VALUES (10), (20)", [])
        .unwrap();
    db.execute(
        "CREATE TEMP TRIGGER g AFTER INSERT ON aux.t2 \
         BEGIN INSERT INTO main.log VALUES ('g:' || NEW.a); END",
        [],
    )
    .unwrap();
    // Mixed statement (aux target, main source): the driver materializes
    // the source at the parent and fires per row.
    db.execute("INSERT INTO aux.t2 (a) SELECT v FROM main.src", [])
        .unwrap();
    assert_eq!(
        rows(&db, "SELECT x FROM main.log ORDER BY x"),
        vec![vec!["g:10"], vec!["g:20"]]
    );
    assert_eq!(
        rows(&db, "SELECT a FROM aux.t2 ORDER BY a"),
        vec![vec!["10"], vec!["20"]]
    );
}

#[test]
fn bound_triggers_on_without_rowid_tables() {
    let mut db = fixture();
    db.execute(
        "CREATE TABLE aux.wr (k TEXT PRIMARY KEY, v INT) WITHOUT ROWID",
        [],
    )
    .unwrap();
    db.execute(
        "CREATE TEMP TRIGGER wtr AFTER INSERT ON aux.wr \
         BEGIN INSERT INTO main.log VALUES ('wr:' || NEW.k); END",
        [],
    )
    .unwrap();
    db.execute("INSERT INTO aux.wr VALUES ('x', 1)", [])
        .unwrap();
    assert_eq!(rows(&db, "SELECT x FROM main.log"), vec![vec!["wr:x"]]);
    // Per-row writes target the PK on WITHOUT ROWID tables.
    db.execute("UPDATE aux.wr SET v = 9 WHERE k = 'x'", [])
        .unwrap();
    assert_eq!(
        rows(&db, "SELECT v FROM aux.wr WHERE k = 'x'"),
        vec![vec!["9"]]
    );
    db.execute("DELETE FROM aux.wr WHERE k = 'x'", []).unwrap();
    assert_eq!(rows(&db, "SELECT count(*) FROM aux.wr"), vec![vec!["0"]]);
}

#[test]
fn aux_engine_own_triggers_coexist_with_bound_temp_triggers() {
    let mut db = fixture();
    // The aux engine's own trigger fires natively on the per-row write;
    // the parent's TEMP trigger fires around it.
    db.execute(
        "CREATE TRIGGER aux.own AFTER INSERT ON t2 \
         BEGIN UPDATE t2 SET b = b + 100 WHERE a = NEW.a; END",
        [],
    )
    .unwrap();
    db.execute(
        "CREATE TEMP TRIGGER par AFTER INSERT ON aux.t2 \
         BEGIN INSERT INTO main.log VALUES ('par:' || NEW.a); END",
        [],
    )
    .unwrap();
    db.execute("INSERT INTO aux.t2 VALUES (5, 0)", []).unwrap();
    assert_eq!(
        rows(&db, "SELECT b FROM aux.t2 WHERE a = 5"),
        vec![vec!["100"]] // aux's own trigger applied
    );
    assert_eq!(
        rows(&db, "SELECT x FROM main.log"),
        vec![vec!["par:5"]] // parent temp trigger fired
    );
}

#[test]
fn query_path_dml_on_bound_table_directs_to_execute() {
    // DML with RETURNING through the &self query path cannot run the
    // driver (it re-enters the parent's MUTABLE machinery) — the
    // documented boundary names the fix.
    let mut db = fixture();
    db.execute(
        "CREATE TEMP TRIGGER g AFTER INSERT ON aux.t2 \
         BEGIN INSERT INTO main.log VALUES ('g'); END",
        [],
    )
    .unwrap();
    let err = err_of_q(&db, "INSERT INTO aux.t2 VALUES (1, 1) RETURNING a");
    assert!(
        err.contains("must run through Database::execute"),
        "err: {err}"
    );
    // The execute path handles the same statement fine.
    db.execute("INSERT INTO aux.t2 VALUES (1, 1)", []).unwrap();
    assert_eq!(rows(&db, "SELECT count(*) FROM main.log"), vec![vec!["1"]]);
}

fn err_of_q(db: &Database, sql: &str) -> String {
    match db.query_with_columns(sql, []) {
        Ok(_) => "OK".to_string(),
        Err(e) => e.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Form (b): TEMP triggers on LOCAL tables with foreign refs in the body
// ---------------------------------------------------------------------------

#[test]
fn local_temp_trigger_reads_aux() {
    let mut db = fixture();
    db.execute("CREATE TABLE aux.u (b INT)", []).unwrap();
    db.execute("INSERT INTO aux.u VALUES (55)", []).unwrap();
    db.execute("CREATE TABLE maint (a INT)", []).unwrap();
    // The oracle's program: the body's SELECT reads aux live.
    db.execute(
        "CREATE TEMP TRIGGER tr AFTER INSERT ON main.maint \
         BEGIN INSERT INTO log SELECT b FROM aux.u; END",
        [],
    )
    .unwrap();
    db.execute("INSERT INTO maint VALUES (1)", []).unwrap();
    assert_eq!(rows(&db, "SELECT x FROM main.log"), vec![vec!["55"]]);
    // The read is LIVE per fire: after a new aux row, the next fire
    // selects BOTH aux rows (the oracle's SELECT b FROM aux.u semantics).
    db.execute("INSERT INTO aux.u VALUES (66)", []).unwrap();
    db.execute("INSERT INTO maint VALUES (2)", []).unwrap();
    assert_eq!(
        rows(&db, "SELECT x FROM main.log"),
        vec![vec!["55"], vec!["55"], vec!["66"]]
    );
}

#[test]
fn local_temp_trigger_writes_aux() {
    let mut db = fixture();
    db.execute("CREATE TABLE aux.u (b INT)", []).unwrap();
    db.execute("CREATE TABLE maint (a INT)", []).unwrap();
    db.execute(
        "CREATE TEMP TRIGGER tr2 AFTER INSERT ON maint \
         BEGIN INSERT INTO aux.u VALUES (NEW.a * 10); END",
        [],
    )
    .unwrap();
    db.execute("INSERT INTO maint VALUES (66)", []).unwrap();
    assert_eq!(rows(&db, "SELECT b FROM aux.u"), vec![vec!["660"]]);
}

#[test]
fn local_temp_trigger_when_reads_aux() {
    let mut db = fixture();
    db.execute("CREATE TABLE aux.u (b INT)", []).unwrap();
    db.execute("INSERT INTO aux.u VALUES (100)", []).unwrap();
    db.execute("CREATE TABLE maint (a INT)", []).unwrap();
    db.execute(
        "CREATE TEMP TRIGGER tw AFTER INSERT ON maint \
         WHEN NEW.a > (SELECT max(b) FROM aux.u) \
         BEGIN INSERT INTO main.log VALUES ('big:' || NEW.a); END",
        [],
    )
    .unwrap();
    db.execute("INSERT INTO maint VALUES (50)", []).unwrap();
    assert_eq!(rows(&db, "SELECT count(*) FROM main.log"), vec![vec!["0"]]);
    db.execute("INSERT INTO maint VALUES (200)", []).unwrap();
    assert_eq!(rows(&db, "SELECT x FROM main.log"), vec![vec!["big:200"]]);
}

#[test]
fn local_temp_trigger_statement_atomicity_on_aux() {
    let mut db = fixture();
    db.execute("CREATE TABLE aux.u (b INT)", []).unwrap();
    db.execute("CREATE TABLE maint (a INT NOT NULL)", [])
        .unwrap();
    db.execute(
        "CREATE TEMP TRIGGER w AFTER INSERT ON maint \
         BEGIN INSERT INTO aux.u VALUES (NEW.a); END",
        [],
    )
    .unwrap();
    // A failing multi-row statement must not leave aux writes behind
    // (the spanning aux savepoint opened by the dispatcher).
    let err = err_of(&mut db, "INSERT INTO maint VALUES (1), (2), (NULL)");
    assert!(err.contains("NOT NULL"), "err: {err}");
    assert_eq!(
        rows(&db, "SELECT count(*) FROM aux.u"),
        vec![vec!["0"]],
        "aux writes from trigger bodies must roll back with the statement"
    );
}

#[test]
fn non_temp_cross_db_body_rejected_with_oracle_text() {
    // SQLite 3.53.4: "trigger trg_b cannot reference objects in
    // database aux" — a NON-temp trigger's body may not reference
    // another database at all.
    let mut db = fixture();
    db.execute("CREATE TABLE aux.u (b INT)", []).unwrap();
    let err = err_of(
        &mut db,
        "CREATE TRIGGER trg_b AFTER INSERT ON main.log \
         BEGIN INSERT INTO log SELECT b FROM aux.u; END",
    );
    assert_eq!(
        err,
        "semantic error: trigger trg_b cannot reference objects in database aux"
    );
}

#[test]
fn temp_triggers_may_qualify_dml_targets() {
    // SQLite allows schema-qualified DML targets in TEMP trigger bodies
    // (the one exemption) — and REJECTS them in non-temp bodies.
    let mut db = fixture();
    db.execute(
        "CREATE TEMP TRIGGER q AFTER INSERT ON aux.t2 \
         BEGIN INSERT INTO main.log VALUES ('q'); END",
        [],
    )
    .unwrap();
    db.execute("INSERT INTO aux.t2 VALUES (1, 1)", []).unwrap();
    assert_eq!(rows(&db, "SELECT count(*) FROM main.log"), vec![vec!["1"]]);
    // A non-temp trigger with an AUX-qualified body target (the shape
    // the router sees): SQLite's exact rejection.
    let err = err_of(
        &mut db,
        "CREATE TRIGGER nq AFTER INSERT ON main.log \
         BEGIN INSERT INTO aux.t2 VALUES (1, 1); END",
    );
    assert!(
        err.contains("qualified table names are not allowed"),
        "err: {err}"
    );
    // Divergence note (documented in the module header): the parser
    // normalizes `main.`/`temp.` DML qualifiers away, so a non-temp
    // local trigger body carrying a MAIN-qualified target is treated
    // as its bare form instead of erroring (SQLite rejects it).
}
