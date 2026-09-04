// RSS trajectory of the S03 GROUP BY workload: where does the +11MB come
// from? Prints current/peak RSS after each phase (build, per-iteration).
use rustqlite::{Database, Value};

fn cur_mb() -> f64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap();
    for l in s.lines() {
        if let Some(rest) = l.strip_prefix("VmRSS:") {
            let kb: f64 = rest.trim().trim_end_matches(" kB").parse().unwrap();
            return kb / 1024.0;
        }
    }
    0.0
}

fn peak_mb() -> f64 {
    // VmHWM read IN-PROCESS (the subprocess version measured `sh`).
    let s = std::fs::read_to_string("/proc/self/status").unwrap();
    for l in s.lines() {
        if let Some(rest) = l.strip_prefix("VmHWM:") {
            let kb: f64 = rest.trim().trim_end_matches(" kB").parse().unwrap();
            return kb / 1024.0;
        }
    }
    0.0
}

fn main() {
    let rows: i64 = std::env::var("PROBE_ROWS").ok().and_then(|v| v.parse().ok()).unwrap_or(150_000); // scale 0.15 of 1M
    println!("start      : cur {:.1}MB peak {:.1}MB", cur_mb(), peak_mb());
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, val INTEGER)", [])
        .unwrap();
    db.execute("BEGIN", []).unwrap();
    {
        let mut stmt = db
            .prepare("INSERT INTO t (id, val) VALUES (?, ?)")
            .unwrap();
        for i in 1..=rows {
            stmt.bind(1, Value::Integer(i)).unwrap();
            stmt.bind(2, Value::Integer(i as i64 * 7 % std::env::var("PROBE_MOD").ok().and_then(|v| v.parse().ok()).unwrap_or(100_000))).unwrap();
            let _ = stmt.step();
            stmt.reset();
        }
    }
    db.execute("COMMIT", []).unwrap();
    println!("after build: cur {:.1}MB peak {:.1}MB", cur_mb(), peak_mb());


    // Plain scan with filter (S02-like) — isolates scan cost.
    {
        let t = std::time::Instant::now();
        let out = db
            .query("SELECT COUNT(*), SUM(val) FROM t WHERE val > 50", [])
            .unwrap();
        println!(
            "plain scan : {:.1}ms | cur {:.1}MB peak {:.1}MB | {:?}",
            t.elapsed().as_secs_f64() * 1000.0,
            cur_mb(),
            peak_mb(),
            out.first().map(|r| r[1].clone())
        );
        drop(out);
    }

    let sql = "SELECT val/10, COUNT(*) FROM t GROUP BY val/10";
    let sql_small = "SELECT val/10000, COUNT(*) FROM t GROUP BY val/10000"; // ~10 groups
    {
        let t = std::time::Instant::now();
        let out = db.query(sql_small, []).unwrap();
        println!(
            "small grps : {} groups, {:.1}ms | cur {:.1}MB",
            out.len(),
            t.elapsed().as_secs_f64() * 1000.0,
            cur_mb()
        );
    }
    for it in 0..3 {
        let t = std::time::Instant::now();
        let out = db.query(sql, []).unwrap();
        println!(
            "iter {it} done: {} groups, {:.1}ms | cur {:.1}MB peak {:.1}MB",
            out.len(),
            t.elapsed().as_secs_f64() * 1000.0,
            cur_mb(),
            peak_mb()
        );
        drop(out);
    }

    // Streaming variant: same query through prepare/step.
    let mut stmt = db.prepare(sql).unwrap();
    let mut n = 0usize;
    let t = std::time::Instant::now();
    while stmt.step().unwrap() == rustqlite::StepResult::Row {
        n += 1;
    }
    println!(
        "streaming  : {} groups, {:.1}ms | cur {:.1}MB peak {:.1}MB",
        n,
        t.elapsed().as_secs_f64() * 1000.0,
        cur_mb(),
        peak_mb()
    );
}

// Isolate: plain filtered scan (S02-like) on the same DB.
#[allow(dead_code)]
fn phase2() {}
