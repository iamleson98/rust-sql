//! Isolated parallel-SUM memory cost: build the file OUT of process, then
//! this process only opens + queries. Vary worker count via PRAGMA.

use rustqlite::Database;

fn mem() -> (f64, f64) {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let mut rss = 0.0;
    let mut hwm = 0.0;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            rss = rest
                .trim()
                .trim_end_matches("kB")
                .trim()
                .parse::<f64>()
                .unwrap()
                / 1024.0;
        }
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            hwm = rest
                .trim()
                .trim_end_matches("kB")
                .trim()
                .parse::<f64>()
                .unwrap()
                / 1024.0;
        }
    }
    (rss, hwm)
}

fn show(stage: &str) {
    let (rss, hwm) = mem();
    println!("{stage:44} rss={rss:6.1}MB hwm={hwm:6.1}MB");
}

fn main() {
    let path = "/tmp/s17q.rq.db";
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).cloned().unwrap_or_else(|| "parallel".into());

    show("start (fresh process)");
    let mut db = Database::open(path).unwrap();
    show("after open");
    if mode == "serial" {
        db.execute("PRAGMA parallel_scan=0", []).unwrap();
    }
    for i in 0..3 {
        let out = db.query("SELECT SUM(val) FROM t", []).unwrap();
        if i == 0 {
            println!("sum = {:?}", out[0][0]);
        }
        show(&format!("after SUM #{i} ({mode})"));
        println!(
            "    cache_pages={} misses={} hits={}",
            db.pager().cache_size(),
            db.pager().cache_misses(),
            db.pager().cache_hits()
        );
    }
    // A COUNT too (index-free walk).
    let out = db.query("SELECT COUNT(*), SUM(score) FROM t", []).unwrap();
    println!("count = {:?}", out[0][0]);
    show(&format!("after COUNT+SUM ({mode})"));
    std::thread::sleep(std::time::Duration::from_millis(50));
    show("final");
}
