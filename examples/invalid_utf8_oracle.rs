//! Oracle probe: invalid-UTF-8 TEXT through LIKE / GLOB, printf, char /
//! unicode, and the JSON functions — engine vs bundled SQLite 3.53.4.
//! Prints one line per case; DIVERGE lines are the work list.
use rustqlite::Database;

fn cases() -> Vec<(&'static str, String)> {
    let raw = |hex: &str| format!("CAST(x'{hex}' AS TEXT)");
    let mut v: Vec<(&'static str, String)> = Vec::new();
    let vals: Vec<(String, String)> = vec![
        (raw("ff"), "ff".into()),
        (raw("c3"), "c3".into()),
        (raw("e262"), "e262".into()),
        (raw("e282"), "e282".into()),
        (raw("41ff"), "41ff".into()),
        (raw("80"), "80".into()),
        (raw("c3a9"), "c3a9".into()),
    ];
    for (val, name) in &vals {
        let pats: Vec<String> = vec![
            "'%'".into(),
            "'_'".into(),
            "'__'".into(),
            "'a%'".into(),
            "'%a'".into(),
            "'z'".into(),
            raw("ff"),
            format!("{} || '%'", raw("ff")),
            format!("'%' || {}", raw("ff")),
            format!("'_' || {}", raw("ff")),
            "'a*b'".into(),
            "'[a-f]*'".into(),
            "'*[!x]'".into(),
            "'a!%' ESCAPE '!'".into(),
            "'ab'".into(),
            "'AB'".into(),
        ];
        for p in &pats {
            v.push(("LIKE", format!("SELECT hex({val} LIKE {p})")));
            v.push(("GLOB", format!("SELECT hex({val} GLOB {p})")));
        }
        v.push(("LIKE", format!("SELECT hex({val} LIKE {val})")));
        v.push(("GLOB", format!("SELECT hex({val} GLOB {val})")));
        v.push(("LIKE", format!("SELECT hex('a' LIKE {val})")));
        v.push(("GLOB", format!("SELECT hex('a*b' GLOB {val})")));
        v.push(("GLOB", format!("SELECT hex({val} GLOB '[a-f]*')")));
        v.push(("GLOB", format!("SELECT hex({val} GLOB '*[!x]')")));
        let _ = name;
    }
    // ---- printf ----
    let pvals: Vec<String> = vec![
        raw("ff"),
        raw("c3"),
        raw("41ff42"),
        raw("e282"),
        format!("{} || char(0) || {}", raw("41"), raw("42")),
    ];
    for val in &pvals {
        v.push(("printf", format!("SELECT hex(printf('%s', {val}))")));
        v.push(("printf", format!("SELECT hex(printf('<%q>', {val}))")));
        v.push(("printf", format!("SELECT hex(printf('%.*s', 2, {val}))")));
        v.push(("printf", format!("SELECT hex(format('%w', {val}))")));
        v.push(("printf", format!("SELECT length(printf('%s', {val}))")));
    }
    v.push((
        "printf",
        "SELECT hex(printf(CAST(x'2573' AS TEXT), 'x'))".to_string(),
    ));
    // ---- char / unicode ----
    for val in [
        raw("ff"),
        raw("c3"),
        raw("e282"),
        raw("80"),
        raw("c3a9"),
        raw("e262"),
    ] {
        v.push((
            "unicode",
            format!("SELECT unicode({val}), unicode(substr({val}, 2)), length({val})"),
        ));
    }
    v.push((
        "char",
        "SELECT hex(char(0xC3)), hex(char(0x10FFFF)), hex(char(0x110000))".to_string(),
    ));
    // ---- JSON ----
    for val in [
        raw("22ff22"),
        raw("22e28222"),
        raw("22c3a922"),
        raw("ff"),
        raw("e262"),
    ] {
        v.push(("json", format!("SELECT json_quote({val})")));
        v.push(("json", format!("SELECT hex(json_quote({val}))")));
        v.push(("json", format!("SELECT hex(json({val}))")));
        v.push(("json", format!("SELECT json_valid({val})")));
        v.push(("json", format!("SELECT hex(json_array({val}))")));
        v.push((
            "json",
            format!("SELECT hex(json_extract(json_array({val}), '$[0]'))"),
        ));
        v.push((
            "json",
            format!("SELECT hex(json_array_text(json_array({val})))"),
        ));
    }
    v.push((
        "json",
        format!(
            "SELECT hex(json_insert('{{\"a\":1}}', '$.b', {}))",
            raw("ff")
        ),
    ));
    v.push((
        "json",
        format!(
            "SELECT hex(json_extract(json_insert('{{\"a\":1}}', '$.b', {}), '$.b'))",
            raw("ff")
        ),
    ));
    v.push((
        "json",
        format!("SELECT hex(json_group_array({}))", raw("ff")),
    ));
    v.push((
        "json",
        format!("SELECT hex(json_object('k', {}))", raw("ff")),
    ));
    v.push((
        "json",
        format!("SELECT hex(json_array_length(json_array({})))", raw("ff")),
    ));
    v
}

fn fmt_rows(rows: &[Vec<rustqlite::Value>]) -> String {
    let mut out = String::new();
    for r in rows {
        if !out.is_empty() {
            out.push('|');
        }
        for (i, c) in r.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            match c {
                rustqlite::Value::Null => out.push('~'),
                rustqlite::Value::Integer(i) => out.push_str(&i.to_string()),
                rustqlite::Value::Real(f) => out.push_str(&format!("{f:?}")),
                rustqlite::Value::Text(t) => out.push_str(
                    &t.as_bytes()
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect::<String>(),
                ),
                rustqlite::Value::Blob(b) => {
                    out.push_str(&b.iter().map(|x| format!("{x:02x}")).collect::<String>())
                }
            }
        }
    }
    out
}

fn main() {
    let cs = cases();
    let db = Database::open_in_memory().unwrap();

    use rusqlite::Connection;
    let rc = Connection::open_in_memory().unwrap();

    let mut n_ok = 0;
    let mut n_bad = 0;
    let mut n_err_pair = 0;
    for (fam, sql) in &cs {
        let eng: Result<String, String> = match db.query(sql, []) {
            Ok(rows) => Ok(fmt_rows(&rows)),
            Err(e) => Err(format!("ERR: {}", e)),
        };
        let sq: Result<String, String> = (|| {
            let mut stmt = rc.prepare(sql).map_err(|e| format!("ERR: {e}"))?;
            let mut out = String::new();
            let mut rows = stmt.query([]).map_err(|e| format!("ERR: {e}"))?;
            loop {
                let r = match rows.next() {
                    Ok(Some(r)) => r,
                    Ok(None) => break,
                    Err(e) => return Err(format!("ERR: {e}")),
                };
                if !out.is_empty() {
                    out.push('|');
                }
                for i in 0..r.as_ref().column_count() {
                    if i > 0 {
                        out.push(',');
                    }
                    use rusqlite::types::ValueRef;
                    match r.get_ref(i).unwrap() {
                        ValueRef::Null => out.push('~'),
                        ValueRef::Integer(v) => out.push_str(&v.to_string()),
                        ValueRef::Real(v) => out.push_str(&format!("{v:?}")),
                        ValueRef::Text(t) => {
                            out.push_str(&t.iter().map(|b| format!("{b:02x}")).collect::<String>())
                        }
                        ValueRef::Blob(t) => {
                            out.push_str(&t.iter().map(|b| format!("{b:02x}")).collect::<String>())
                        }
                    }
                }
            }
            Ok(out)
        })();
        match (eng, sq) {
            (Ok(a), Ok(b)) => {
                if a == b {
                    n_ok += 1;
                } else {
                    n_bad += 1;
                    println!("DIVERGE {fam}: {sql}\n  engine={a}\n  sqlite={b}");
                }
            }
            (Err(a), Err(b)) => {
                let norm = |s: &str| {
                    let s = s.to_lowercase();
                    if s.contains("json") || s.contains("malformed") {
                        "malformed"
                    } else if s.contains("escape") {
                        "escape"
                    } else {
                        "other"
                    }
                };
                if norm(&a) == norm(&b) {
                    n_err_pair += 1;
                } else {
                    n_bad += 1;
                    println!("DIVERGE(err) {fam}: {sql}\n  engine={a}\n  sqlite={b}");
                }
            }
            (a, b) => {
                n_bad += 1;
                println!("DIVERGE(mixed) {fam}: {sql}\n  engine={a:?}\n  sqlite={b:?}");
            }
        }
    }
    println!(
        "\n== {n_ok} match, {n_err_pair} both-error, {n_bad} DIVERGE of {} ==",
        cs.len()
    );
}
