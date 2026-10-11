//! Dual-engine oracle: the rowid-alias IN / boundary REAL decision map.
//! Runs every shape through the engine AND bundled SQLite; prints only
//! divergences. The class: SQLite's rowid-IN seek (OP_SeekRowid, ticket
//! #3922 — a boundary REAL member matches nothing) vs the general
//! comparison (matches i64::MIN), chosen by SQLite's cost model — table
//! size, alias-index presence, member count, extra AND/OR terms.
use rustqlite::Database;

fn val_hex(v: &rustqlite::Value) -> String {
    use rustqlite::Value as V;
    match v {
        V::Null => "~".into(),
        V::Integer(i) => i.to_string(),
        V::Real(f) => format!("{f:?}"),
        V::Text(t) => t.as_bytes().iter().map(|b| format!("{b:02x}")).collect(),
        V::Blob(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
    }
}

fn sq_val_hex(v: rusqlite::types::ValueRef<'_>) -> String {
    use rusqlite::types::ValueRef as V;
    match v {
        V::Null => "~".into(),
        V::Integer(i) => i.to_string(),
        V::Real(f) => format!("{f:?}"),
        V::Text(t) => t.iter().map(|b| format!("{b:02x}")).collect(),
        V::Blob(t) => t.iter().map(|b| format!("{b:02x}")).collect(),
    }
}

fn eng_rows(db: &Database, q: &str) -> Result<String, String> {
    let mut out = String::new();
    match db.query(q, []) {
        Ok(got) => {
            for r in got {
                if !out.is_empty() {
                    out.push(',');
                }
                out.push_str(&val_hex(&r[0]));
            }
            Ok(out)
        }
        Err(e) => Err(format!("{e}")),
    }
}

fn sq_rows(rc: &rusqlite::Connection, q: &str) -> Result<String, String> {
    let mut stmt = rc.prepare(q).map_err(|e| format!("{e}"))?;
    let mut rows = stmt.query([]).map_err(|e| format!("{e}"))?;
    let mut out = String::new();
    loop {
        match rows.next() {
            Ok(Some(r)) => {
                if !out.is_empty() {
                    out.push(',');
                }
                out.push_str(&sq_val_hex(r.get_ref(0).map_err(|e| format!("{e}"))?));
            }
            Ok(None) => break,
            Err(e) => return Err(format!("{e}")),
        }
    }
    Ok(out)
}

fn short(r: &Result<String, String>) -> String {
    match r {
        Ok(s) => {
            if s.len() > 40 {
                let boundary = s.contains("-9223372036854775808");
                let n = s.split(',').count();
                format!("[{n} rows, boundary={boundary}]")
            } else {
                s.clone()
            }
        }
        Err(e) => format!("ERR:{e}"),
    }
}

fn main() {
    let b = "-9223372036854775808.0";
    let setups: Vec<(&str, Vec<&str>)> = vec![
        ("no-index", vec![]),
        ("plain-idx", vec!["CREATE INDEX t_id ON t(id)"]),
        ("uniq-idx", vec!["CREATE UNIQUE INDEX t_id ON t(id)"]),
        (
            "idx-a-nostats",
            vec!["CREATE INDEX t_id ON t(id)", "ANALYZE t"],
        ),
        ("big-plain-idx", vec!["CREATE INDEX t_id ON t(id)", "BIG"]),
        ("big-no-index", vec!["BIG"]),
    ];
    let shapes: Vec<(&str, String)> = vec![
        ("eq-literal", format!("SELECT id FROM t WHERE id = {b}")),
        ("in1-literal", format!("SELECT id FROM t WHERE id IN ({b})")),
        (
            "in2-literal",
            format!("SELECT id FROM t WHERE id IN ({b}, 2)"),
        ),
        (
            "in3-literal",
            format!("SELECT id FROM t WHERE id IN ({b}, 2, 3)"),
        ),
        (
            "in1-cast",
            format!("SELECT id FROM t WHERE id IN (CAST({b} AS REAL))"),
        ),
        (
            "in2-cast",
            format!("SELECT id FROM t WHERE id IN (CAST({b} AS REAL), 2)"),
        ),
        (
            "in2-text-real",
            "SELECT id FROM t WHERE id IN ('-9223372036854775808.0', 2)".to_string(),
        ),
        (
            "in1-text-real",
            "SELECT id FROM t WHERE id IN ('-9223372036854775808.0')".to_string(),
        ),
        (
            "in-or",
            format!("SELECT id FROM t WHERE id IN ({b}, 1) OR v = 'z'"),
        ),
        (
            "or-in",
            format!("SELECT id FROM t WHERE v = 'z' OR id IN ({b}, 1)"),
        ),
        (
            "or-in-idx",
            format!("SELECT id FROM t WHERE a = 1 OR id IN ({b}, 3)"),
        ),
        (
            "in2-and",
            format!("SELECT id FROM t WHERE id IN ({b}, 2) AND v = 'y'"),
        ),
        (
            "in2-and-idx2",
            format!("SELECT id FROM t WHERE id IN ({b}, 3) AND a = 3"),
        ),
        (
            "in2-and-aliasidx",
            format!("SELECT id FROM t WHERE id IN ({b}, 3) AND a > 0"),
        ),
        (
            "count-in2",
            format!("SELECT count(*) FROM t WHERE id IN ({b}, 2)"),
        ),
        (
            "distinct-in2",
            format!("SELECT DISTINCT a FROM t WHERE id IN ({b}, 3) ORDER BY 1"),
        ),
        (
            "upd-in2",
            format!("SELECT count(*) FROM t WHERE id IN ({b}, 2)"),
        ),
        (
            "distinct-in1",
            format!("SELECT DISTINCT a FROM t WHERE id IN ({b})"),
        ),
        (
            "distinct-in2a",
            format!("SELECT DISTINCT a FROM t WHERE id IN ({b}, 3)"),
        ),
        (
            "distinct-in2k",
            format!("SELECT DISTINCT k FROM t WHERE id IN ({b}, 3)"),
        ),
        (
            "distinct-in2v",
            format!("SELECT DISTINCT v FROM t WHERE id IN ({b}, 3)"),
        ),
        (
            "order-in2",
            format!("SELECT a FROM t WHERE id IN ({b}, 3) ORDER BY a"),
        ),
        (
            "order-id-in2",
            format!("SELECT a FROM t WHERE id IN ({b}, 3) ORDER BY id"),
        ),
        (
            "max-in2",
            format!("SELECT max(a) FROM t WHERE id IN ({b}, 3)"),
        ),
        (
            "in2-cast-neg",
            format!("SELECT id FROM t WHERE id IN (0.0 - {b}, 3)"),
        ),
        (
            "in1-cast-sub",
            format!("SELECT id FROM t WHERE id IN ({b} * 1)"),
        ),
        (
            "or-and-mix",
            format!("SELECT id FROM t WHERE (id IN ({b}, 3) AND k > 0) OR v = 'z'"),
        ),
    ];
    let mut n_ok = 0;
    let mut n_bad = 0;
    for (sname, idxs) in &setups {
        let big = idxs.contains(&"BIG");
        let mut db = Database::open_in_memory().unwrap();
        let rc = rusqlite::Connection::open_in_memory().unwrap();
        let setup = |db: &mut Database, rc: &rusqlite::Connection| {
            db.execute(
                "CREATE TABLE t(id INTEGER PRIMARY KEY, a INT, k INT, v TEXT)",
                [],
            )
            .unwrap();
            db.execute("CREATE INDEX t_a ON t(a)", []).unwrap();
            rc.execute_batch("CREATE TABLE t(id INTEGER PRIMARY KEY, a INT, k INT, v TEXT); CREATE INDEX t_a ON t(a);")
                .unwrap();
            for s in idxs {
                if *s == "BIG" {
                    continue;
                }
                db.execute(s, []).unwrap();
                rc.execute_batch(s).unwrap();
            }
            let n = if big { 4000u32 } else { 5u32 };
            let mut ins = String::from("INSERT INTO t VALUES (-9223372036854775808, 3, 30, 'min')");
            for i in 1..n {
                ins.push_str(&format!(", ({i}, {}, {}, 'r{}')", i % 7, i % 97, i));
            }
            db.execute(&ins, []).unwrap();
            rc.execute_batch(&ins).unwrap();
            if idxs.contains(&"ANALYZE t") {
                // stats only for t (not t_id — index-after-analyze shape)
                db.execute("ANALYZE", []).unwrap();
                rc.execute_batch("ANALYZE;").unwrap();
                // drop t_id stats to emulate index-after-analyze? keep simple: full analyze
            } else if !big {
                db.execute("ANALYZE", []).unwrap();
                rc.execute_batch("ANALYZE;").unwrap();
            }
        };
        setup(&mut db, &rc);
        println!("--- {sname} ---");
        for (qn, q) in &shapes {
            let mut e = eng_rows(&db, q);
            let mut s = sq_rows(&rc, q);
            if !q.contains("ORDER BY") {
                for r in [&mut e, &mut s] {
                    let mut parts: Vec<String> = r
                        .as_ref()
                        .map(|x| x.split(',').map(str::to_string).collect())
                        .unwrap_or_default();
                    parts.sort();
                    *r = Ok(parts.join(","));
                }
            }
            if e == s {
                n_ok += 1;
                println!("{qn:18} MATCH {}", short(&e));
            } else {
                n_bad += 1;
                println!("{qn:18} ENG {} | SQ {}", short(&e), short(&s));
            }
        }
    }
    println!("== {n_ok} match, {n_bad} DIVERGE ==");
}
