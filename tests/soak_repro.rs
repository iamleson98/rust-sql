//! LOCAL TRIAGE HARNESS — the S4 residual index-corruption hunt.
//!
//! NOT part of CI: every test here is `#[ignore]`d. Manual use:
//!
//! ```text
//! # 1. build the 1M-row seed once (~1 min):
//! cargo test --release --test soak_repro -- --ignored --nocapture build_seed
//!
//! # 2. iterate soak attempts against fresh copies until the corruption
//! #    reproduces (each attempt dumps full diagnostics + archives the
//! #    failed db files to /home/z/my-project/soak_fail/<attempt>/):
//! cargo test --release --test soak_repro -- --ignored --nocapture soak
//! ```
//!
//! Recipe (from the Task-39/40 shrink hunt): 1M-row seed + cache_size=100
//! reproduces ~1/4 of runs with the deterministic signature "index
//! entries out of order or duplicated at rowid 25817" (the k=380139
//! neighborhood — `gen_row(25816)`).

use rustqlite::{Database, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier, Mutex, MutexGuard};

// ============================================================
// Deterministic data (identical to limit_stress.rs)
// ============================================================

fn splitmix64(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn gen_row(i: u64) -> (i64, String) {
    let h = splitmix64(i + 1);
    let k = (h % 1_000_000) as i64;
    let note = format!("n{:08x}", (h >> 20) & 0xffff_ffff);
    (k, note)
}

/// Seed row count (env-scalable for debug-box triage runs; the
/// 1M-row release shape is the default and what CI-adjacent hunting
/// uses). The corruption anchor (rowid 25817) exists at any scale
/// >= 25817.
fn seed_rows() -> u64 {
    env_u64("SEED_ROWS", 1_000_000).max(25_817)
}
const SEED_DIR: &str = "/home/z/my-project/soak_seed";
const FAIL_DIR: &str = "/home/z/my-project/soak_fail";

/// The corruption anchor: seed rowid 25817 -> k = gen_row(25816).0.
const K_ANCHOR: i64 = 380_139;
const WRITER_ID_BASE: i64 = 1_000_000_000;
const N_WRITERS: usize = 4;
const TXNS: i64 = 40;
const PER_TXN: i64 = 25;

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}

static SERIAL: Mutex<()> = Mutex::new(());
fn serial() -> MutexGuard<'static, ()> {
    match SERIAL.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

fn writer_ids() -> impl Iterator<Item = i64> {
    (0..N_WRITERS as i64).flat_map(move |tid| {
        (0..TXNS).flat_map(move |t| {
            (0..PER_TXN).map(move |j| WRITER_ID_BASE + tid * 10_000_000 + t * PER_TXN + j + 1)
        })
    })
}

// ============================================================
// Seed builder
// ============================================================

#[test]
#[ignore]
fn build_seed() {
    let _g = serial();
    let _ = std::fs::create_dir_all(SEED_DIR);
    let path = std::path::Path::new(SEED_DIR).join("seed.db");
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("db-wal"));
    let start = std::time::Instant::now();
    let mut db = Database::open(&path).unwrap();
    db.execute("PRAGMA journal_mode = WAL", []).unwrap();
    db.execute(
        "CREATE TABLE events (id INTEGER PRIMARY KEY, k INTEGER, note TEXT)",
        [],
    )
    .unwrap();
    db.execute("CREATE INDEX ix_events_k ON events (k)", [])
        .unwrap();
    let batch = env_u64("SEED_BATCH", 25_000);
    let mut next_id: u64 = 0;
    let mut k_sum = 0i64;
    while next_id < seed_rows() {
        let n = batch.min(seed_rows() - next_id);
        let mut sql = String::with_capacity(64 * n as usize + 64);
        sql.push_str("INSERT INTO events (id, k, note) VALUES ");
        for j in 0..n {
            let (k, note) = gen_row(next_id + j);
            k_sum += k;
            if j > 0 {
                sql.push(',');
            }
            sql.push_str(&format!("({}, {}, '{}')", next_id + j + 1, k, note));
        }
        db.execute("BEGIN", []).unwrap();
        db.execute(&sql, []).unwrap();
        db.execute("COMMIT", []).unwrap();
        next_id += n;
    }
    drop(db);
    println!(
        "[seed] {} rows, k_sum={k_sum}, {:?}",
        seed_rows(),
        start.elapsed()
    );
    // what sidecars exist after a clean close?
    for e in std::fs::read_dir(SEED_DIR).unwrap() {
        let e = e.unwrap();
        println!(
            "[seed] file: {} = {} bytes",
            e.path().display(),
            e.metadata().unwrap().len()
        );
    }
    std::fs::write(
        std::path::Path::new(SEED_DIR).join("k_sum.txt"),
        k_sum.to_string(),
    )
    .unwrap();
}

fn copy_seed_into(dir: &std::path::Path) -> std::path::PathBuf {
    let dst = dir.join("soak.db");
    for e in std::fs::read_dir(SEED_DIR).unwrap() {
        let e = e.unwrap();
        let name = e.file_name().into_string().unwrap();
        if name == "k_sum.txt" {
            continue;
        }
        let renamed = name.replace("seed.db", "soak.db");
        std::fs::copy(e.path(), dir.join(&renamed)).unwrap();
    }
    dst
}

// ============================================================
// The soak (S4 verbatim shape + cache_size=100)
// ============================================================

fn run_soak_attempt(dir: &std::path::Path, attempt: usize) -> bool {
    let path = copy_seed_into(dir);
    let db = {
        let mut d = Database::open(&path).unwrap();
        d.execute("PRAGMA journal_mode = WAL", []).unwrap();
        d.execute("PRAGMA synchronous = NORMAL", []).unwrap();
        d.execute("PRAGMA cache_size = 100", []).unwrap();
        Arc::new(d)
    };
    let committed = Arc::new(AtomicU64::new(0));
    let read_ops = Arc::new(AtomicU64::new(0));
    let writers_live = Arc::new(AtomicU64::new(N_WRITERS as u64));
    let seed_rows = seed_rows() as i64;
    let mut handles = Vec::new();
    let barrier = Arc::new(Barrier::new(N_WRITERS + 2));

    for tid in 0..N_WRITERS as i64 {
        let db = Arc::clone(&db);
        let barrier = Arc::clone(&barrier);
        let committed = Arc::clone(&committed);
        let writers_live = Arc::clone(&writers_live);
        handles.push(std::thread::spawn(move || {
            Database::set_conn_identity((tid + 1) as u64);
            barrier.wait();
            'txns: for t in 0..TXNS {
                for _attempt in 0..32 {
                    if let Err(e) = db.begin_concurrent_transaction() {
                        let msg = e.to_string().to_lowercase();
                        assert!(
                            msg.contains("snapshot") || msg.contains("busy"),
                            "unexpected BEGIN error: {e}"
                        );
                        continue;
                    }
                    let mut stmt =
                        match db.prepare("INSERT INTO events (id, k, note) VALUES (?, ?, ?)") {
                            Ok(s) => s,
                            Err(e) => {
                                let msg = e.to_string().to_lowercase();
                                assert!(
                                    msg.contains("snapshot") || msg.contains("busy"),
                                    "unexpected PREPARE error in soak: {e}"
                                );
                                let _ = db.rollback_concurrent_transaction();
                                continue;
                            }
                        };
                    let mut ok = true;
                    for j in 0..PER_TXN {
                        let id = WRITER_ID_BASE + tid * 10_000_000 + t * PER_TXN + j + 1;
                        let (k, note) = gen_row(id as u64);
                        stmt.bind(1, Value::Integer(id)).unwrap();
                        stmt.bind(2, Value::Integer(k)).unwrap();
                        stmt.bind(3, Value::Text(note.into())).unwrap();
                        if let Err(e) = stmt.step() {
                            let msg = e.to_string().to_lowercase();
                            assert!(
                                msg.contains("snapshot") || msg.contains("busy"),
                                "unexpected INSERT error in soak: {e}"
                            );
                            ok = false;
                            break;
                        }
                        stmt.reset();
                    }
                    drop(stmt);
                    if !ok {
                        let _ = db.rollback_concurrent_transaction();
                        continue;
                    }
                    match db.commit_concurrent_transaction() {
                        Ok(()) => {
                            committed.fetch_add(PER_TXN as u64, Ordering::Relaxed);
                            continue 'txns;
                        }
                        Err(e) => {
                            let msg = e.to_string().to_lowercase();
                            assert!(
                                msg.contains("snapshot") || msg.contains("busy"),
                                "unexpected COMMIT error in soak: {e}"
                            );
                        }
                    }
                }
                panic!("writer {tid} could not commit txn {t} in 32 OCC attempts");
            }
            writers_live.fetch_sub(1, Ordering::Relaxed);
            Database::set_conn_identity(0);
        }));
    }

    for _r in 0..2u64 {
        let db = Arc::clone(&db);
        let barrier = Arc::clone(&barrier);
        let read_ops = Arc::clone(&read_ops);
        let writers_live = Arc::clone(&writers_live);
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            let mut last_n = 0i64;
            while writers_live.load(Ordering::Relaxed) > 0 {
                match db.query(
                    "SELECT count(*), sum(k) FROM events WHERE id <= ?",
                    [Value::Integer(seed_rows)],
                ) {
                    Ok(got) => {
                        let n = got[0][0].as_integer();
                        assert!(n >= last_n, "reader saw count go BACKWARD: {n} < {last_n}");
                        assert_eq!(
                            n, seed_rows,
                            "seed range mutated under writer pressure (dirty read?)"
                        );
                        last_n = n;
                        read_ops.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => panic!("reader error under write pressure: {e}"),
                }
                match db.query(
                    "SELECT count(*) FROM events WHERE k BETWEEN 1000 AND 1099",
                    [],
                ) {
                    Ok(_) => {
                        read_ops.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => panic!("range-probe reader error under write pressure: {e}"),
                }
            }
        }));
    }

    for h in handles {
        if let Err(e) = h.join() {
            std::panic::resume_unwind(e);
        }
    }
    let committed_total = committed.load(Ordering::Relaxed);
    println!(
        "[attempt {attempt}] committed={committed_total} (want {}) read_ops={}",
        read_ops.load(Ordering::Relaxed),
        N_WRITERS as u64 * TXNS as u64 * PER_TXN as u64
    );

    // ---- Final exactness battery.
    let final_n: i64 = db.query("SELECT count(*) FROM events", []).unwrap()[0][0].as_integer();
    assert_eq!(
        final_n,
        seed_rows + committed_total as i64,
        "lost rows in the soak"
    );

    let integrity_ok = db
        .query("PRAGMA integrity_check", [])
        .unwrap()
        .iter()
        .all(|r| r.iter().any(|c| c.as_text() == "ok"));
    if integrity_ok {
        // reopen check as well — the corruption may only surface after
        // WAL recovery.
        drop(db);
        let db2 = Database::open(&path).unwrap();
        let ok2 = db2
            .query("PRAGMA integrity_check", [])
            .unwrap()
            .iter()
            .all(|r| r.iter().any(|c| c.as_text() == "ok"));
        println!("[attempt {attempt}] integrity ok (reopen: {ok2})");
        return ok2;
    }

    // ---- FAILURE: archive the failed db files FIRST (offline forensics),
    //      then dump diagnostics.
    let arch = std::path::Path::new(FAIL_DIR).join(format!("attempt{attempt}"));
    let _ = std::fs::remove_dir_all(&arch);
    let _ = std::fs::create_dir_all(&arch);
    for e in std::fs::read_dir(dir).unwrap() {
        let e = e.unwrap();
        let _ = std::fs::copy(e.path(), arch.join(e.file_name()));
    }
    println!(
        "\n=========== CORRUPTION REPRODUCED (attempt {attempt}; archived to {arch:?}) ==========="
    );
    eprintln!("---- integrity_check ----");
    for r in db.query("PRAGMA integrity_check", []).unwrap() {
        eprintln!("INTEGRITY: {:?}", r);
    }
    eprintln!("---- dbstat: index tree shape ----");
    if let Ok(inv) = db.query(
        "SELECT pagetype, count(*), sum(ncell), min(ncell), max(ncell) \
         FROM dbstat WHERE name = 'ix_events_k' GROUP BY pagetype",
        [],
    ) {
        for r in &inv {
            eprintln!("IX SHAPE {:?}", r);
        }
    }
    if let Ok(inv) = db.query(
        "SELECT pagetype, count(*), sum(ncell) FROM dbstat WHERE name = 'events' GROUP BY pagetype",
        [],
    ) {
        for r in &inv {
            eprintln!("TBL SHAPE {:?}", r);
        }
    }
    eprintln!("---- planner route for the window probe ----");
    if let Ok(plan) = db.query(
        "EXPLAIN SELECT k, id FROM events WHERE k BETWEEN 379900 AND 380400 ORDER BY k, id",
        [],
    ) {
        for r in plan.iter().take(12) {
            eprintln!("PLAN {:?}", r);
        }
    }
    eprintln!(
        "---- k-window [{}, {}] through the query planner (index order) ----",
        K_ANCHOR - 250,
        K_ANCHOR + 250
    );
    let actual: Vec<(i64, i64)> = db
        .query(
            "SELECT k, id FROM events WHERE k BETWEEN ? AND ? ORDER BY k, id",
            [
                Value::Integer(K_ANCHOR - 250),
                Value::Integer(K_ANCHOR + 250),
            ],
        )
        .unwrap()
        .into_iter()
        .map(|r| (r[0].as_integer(), r[1].as_integer()))
        .collect();
    // Expected multiset: seed ids 1..=1M + all writer ids.
    let mut expected: Vec<(i64, i64)> = Vec::new();
    for i in 1..=seed_rows as u64 {
        let (k, _) = gen_row(i - 1);
        if (K_ANCHOR - 250..=K_ANCHOR + 250).contains(&k) {
            expected.push((k, i as i64));
        }
    }
    for id in writer_ids() {
        let (k, _) = gen_row(id as u64);
        if (K_ANCHOR - 250..=K_ANCHOR + 250).contains(&k) {
            expected.push((k, id));
        }
    }
    expected.sort_unstable();
    // duplicate (k,id) pairs in ACTUAL = index double-entries.
    let mut seen = std::collections::HashSet::new();
    let mut dup_in_actual: Vec<(i64, i64)> = Vec::new();
    for p in &actual {
        if !seen.insert(*p) {
            dup_in_actual.push(*p);
        }
    }
    let exp_set: std::collections::HashSet<(i64, i64)> = expected.iter().copied().collect();
    let act_set: std::collections::HashSet<(i64, i64)> = actual.iter().copied().collect();
    let missing: Vec<(i64, i64)> = exp_set.difference(&act_set).copied().collect();
    let extra: Vec<(i64, i64)> = act_set.difference(&exp_set).copied().collect();
    eprintln!(
        "window rows: expected-set {} actual {} | dup-pairs-in-actual {} missing-from-actual {} extra-in-actual {}",
        exp_set.len(),
        act_set.len(),
        dup_in_actual.len(),
        missing.len(),
        extra.len()
    );
    eprintln!(
        "duplicate pairs in actual (first 20): {:?}",
        &dup_in_actual[..dup_in_actual.len().min(20)]
    );
    let mut missing = missing;
    missing.sort_unstable();
    eprintln!(
        "missing pairs (first 20): {:?}",
        &missing[..missing.len().min(20)]
    );
    eprintln!(
        "extra pairs (first 20): {:?}",
        &extra[..extra.len().min(20)]
    );
    // monotonocity witness of the actual sequence:
    let mut order_breaks = 0usize;
    for w in actual.windows(2) {
        if w[0] >= w[1] {
            if order_breaks < 8 {
                eprintln!("ORDER BREAK at {:?} -> {:?}", w[0], w[1]);
            }
            order_breaks += 1;
        }
    }
    eprintln!("order breaks in actual window scan: {order_breaks}");

    false
}

#[test]
#[ignore]
fn soak() {
    let _g = serial();
    let iters = env_u64("REPRO_ITERS", 12) as usize;
    let mut hits = 0usize;
    for attempt in 1..=iters {
        let dir = tempfile::tempdir().unwrap();
        let ok = run_soak_attempt(dir.path(), attempt);
        if !ok {
            hits += 1;
            break; // full diagnostics already dumped; stop for triage
        }
    }
    assert_eq!(
        hits, 0,
        "corruption reproduced — diagnostics above, files in {FAIL_DIR}"
    );
}

// ============================================================
// Raw page-level dump of the archived corrupted db
// ============================================================

fn decode_varint(buf: &[u8]) -> Option<(u64, usize)> {
    if buf.is_empty() {
        return None;
    }
    let mut v: u64 = 0;
    for i in 0..9 {
        if i >= buf.len() {
            return None;
        }
        let b = buf[i];
        if i == 8 {
            v = (v << 8) | b as u64;
            return Some((v, 9));
        }
        v = (v << 7) | (b & 0x7F) as u64;
        if b & 0x80 == 0 {
            return Some((v, i + 1));
        }
    }
    Some((v, 9))
}

/// (page id, cells in stored order, right-most pointer) — one interior.
type InteriorPages = Vec<(u32, Vec<(u32, i64)>, u32)>;

/// Page header info read raw from the file bytes.
struct RawPage<'a> {
    id: u32,
    ty: u8,
    n_cells: usize,
    right: u32,
    data: &'a [u8],
}

fn read_page(file: &[u8], id: u32, psz: usize) -> RawPage<'_> {
    let off = id as usize * psz;
    let hdr = if id == 0 { 100 } else { 0 };
    let p = &file[off..off + psz];
    RawPage {
        id,
        ty: p[hdr],
        n_cells: u16::from_be_bytes([p[hdr + 4], p[hdr + 5]]) as usize,
        right: u32::from_be_bytes([p[hdr + 8], p[hdr + 9], p[hdr + 10], p[hdr + 11]]),
        data: p,
    }
}

fn cell_at<'a>(page: &'a RawPage<'a>, i: usize) -> Option<&'a [u8]> {
    let hdr = if page.id == 0 { 100 } else { 0 };
    let base = hdr + 12;
    let p = base + i * 2;
    let ptr = u16::from_be_bytes([page.data[p], page.data[p + 1]]) as usize;
    // cells are page-relative from the END of the page (content start to
    // page end); slice to the page end — decoders consume what they need.
    Some(&page.data[ptr..page.data.len().min(ptr + psz_max())])
}

fn psz_max() -> usize {
    4096
}

#[test]
#[ignore]
fn page_dump() {
    let _g = serial();
    let arch = std::path::Path::new(FAIL_DIR)
        .join(std::env::var("ARCH").unwrap_or_else(|_| "attempt4".into()));
    let work = tempfile::tempdir().unwrap();
    for e in std::fs::read_dir(&arch).unwrap() {
        let e = e.unwrap();
        let name = e.file_name().into_string().unwrap();
        std::fs::copy(e.path(), work.path().join(&name)).unwrap();
    }
    let path = work.path().join("soak.db");

    // Resolve the events root, then close cleanly so the main file holds
    // the whole recovered state (checkpoint on close).
    let events_root;
    {
        let db = Database::open(&path).unwrap();
        let rows = db
            .query(
                "SELECT pageno FROM dbstat WHERE name = 'events' AND path = '/'",
                [],
            )
            .unwrap();
        events_root = rows[0][0].as_integer() as u32;
        println!("events root = {events_root}");
    }

    let file = std::fs::read(&path).unwrap();
    let psz = 4096usize;

    // DFS over interior pages; each interior page prints its cells in
    // STORED order. Collect leaf ranges in DFS visit order.
    fn walk(
        file: &[u8],
        psz: usize,
        pid: u32,
        depth: usize,
        visit: &mut Vec<(u32, i64, i64, usize)>, // (page, first, last, n)
        interiors: &mut InteriorPages,
    ) {
        let page = read_page(file, pid, psz);
        match page.ty {
            0x0D => {
                // LeafTable: cells [rowid varint][plen varint][payload]
                let mut first: Option<i64> = None;
                let mut last: i64 = 0;
                for i in 0..page.n_cells {
                    let cell = cell_at(&page, i).unwrap();
                    if let Some((rid, _)) = decode_varint(cell) {
                        let rid = rid as i64;
                        if first.is_none() {
                            first = Some(rid);
                        }
                        last = rid;
                    }
                }
                visit.push((pid, first.unwrap_or(0), last, page.n_cells));
            }
            0x05 => {
                // InteriorTable: cells [child u32][key varint], right at +8
                let mut cells: Vec<(u32, i64)> = Vec::new();
                for i in 0..page.n_cells {
                    let cell = cell_at(&page, i).unwrap();
                    if cell.len() < 4 {
                        continue;
                    }
                    let child = u32::from_be_bytes([cell[0], cell[1], cell[2], cell[3]]);
                    if let Some((k, _)) = decode_varint(&cell[4..]) {
                        cells.push((child, k as i64));
                    }
                }
                interiors.push((pid, cells.clone(), page.right));
                // validate separator monotonicity
                for w in cells.windows(2) {
                    if w[1].1 <= w[0].1 {
                        println!("!!! INTERIOR {pid} (depth {depth}) CELL ORDER VIOLATION: {:?} right={}", cells, page.right);
                        break;
                    }
                }
                for (child, _) in &cells {
                    walk(file, psz, *child, depth + 1, visit, interiors);
                }
                if page.right != 0 {
                    walk(file, psz, page.right, depth + 1, visit, interiors);
                }
            }
            other => println!("page {pid}: unexpected type {other:#x}"),
        }
    }

    let mut visits: Vec<(u32, i64, i64, usize)> = Vec::new();
    let mut interiors: InteriorPages = Vec::new();
    walk(&file, psz, events_root, 0, &mut visits, &mut interiors);

    println!(
        "events tree: {} leaves, {} interiors",
        visits.len(),
        interiors.len()
    );
    // Leaf range order along the DFS visit order: report any regression.
    for w in visits.windows(2) {
        if w[1].1 <= w[0].2 {
            println!(
                "!!! LEAF ORDER BREAK in DFS: page {} [{},{}] then page {} [{},{}]",
                w[0].0, w[0].1, w[0].2, w[1].0, w[1].1, w[1].2
            );
        }
    }
    // Show the leaves holding the writer-row boundary region.
    println!("--- leaves overlapping writer-row region (last cells >= 1000000000) ---");
    for (pid, first, last, n) in &visits {
        if *last >= 1_000_000_000 {
            println!("leaf {pid}: [{first}..{last}] n={n}");
        }
    }
    // Show every interior page whose cells reference the leaves above.
    let writer_leaves: std::collections::HashSet<u32> = visits
        .iter()
        .filter(|(_, _, last, _)| *last >= 1_000_000_000)
        .map(|(pid, _, _, _)| *pid)
        .collect();
    println!("--- interior cells referencing writer leaves ---");
    for (pid, cells, right) in &interiors {
        let hits: Vec<&(u32, i64)> = cells
            .iter()
            .filter(|(c, _)| writer_leaves.contains(c))
            .collect();
        if !hits.is_empty() || writer_leaves.contains(right) {
            println!(
                "interior {pid} ({} cells, right={right}): {:?} | writer-hits {:?}",
                cells.len(),
                cells,
                hits
            );
        }
    }
}

#[test]
#[ignore]
fn probe() {
    let _g = serial();
    let arch = std::path::PathBuf::from(FAIL_DIR)
        .join(std::env::var("ARCH").unwrap_or_else(|_| "attempt4".into()));
    let work = tempfile::tempdir().unwrap();
    for e in std::fs::read_dir(&arch).unwrap() {
        let e = e.unwrap();
        let name = e.file_name().into_string().unwrap();
        std::fs::copy(e.path(), work.path().join(&name)).unwrap();
    }
    let path = work.path().join("soak.db");
    let db = Database::open(&path).unwrap();

    // ---- Index-side ground truth: total cells in the index tree.
    let ix_cells: i64 = db
        .query(
            "SELECT sum(ncell) FROM dbstat WHERE name = 'ix_events_k'",
            [],
        )
        .unwrap()[0][0]
        .as_integer();
    let tbl_cells: i64 = db
        .query("SELECT sum(ncell) FROM dbstat WHERE name = 'events'", [])
        .unwrap()[0][0]
        .as_integer();
    println!("index tree total cells (DFS): {ix_cells} (complete = 1004000)");
    println!("table tree total cells (DFS): {tbl_cells} (complete = 1004000)");
    let ix_leaf_cells: i64 = db
        .query(
            "SELECT sum(ncell) FROM dbstat WHERE name = 'ix_events_k' AND pagetype = 'leaf'",
            [],
        )
        .unwrap()[0][0]
        .as_integer();
    let tbl_leaf_cells: i64 = db
        .query(
            "SELECT sum(ncell) FROM dbstat WHERE name = 'events' AND pagetype = 'leaf'",
            [],
        )
        .unwrap()[0][0]
        .as_integer();
    println!("index LEAF cells: {ix_leaf_cells} (complete = 1004000)");
    println!("table LEAF cells: {tbl_leaf_cells} (complete = 1004000)");
    // duplicate-pair check through the index chain walk
    let dup: i64 = db
        .query(
            "SELECT count(*) FROM (SELECT k, id FROM events WHERE k BETWEEN 0 AND 999999 GROUP BY k, id HAVING count(*) > 1)",
            [],
        )
        .unwrap()[0][0]
        .as_integer();
    println!("duplicate (k,id) pairs via index walk: {dup}");
    let chain_count: i64 = db
        .query(
            "SELECT count(*) FROM events WHERE k BETWEEN 0 AND 999999",
            [],
        )
        .unwrap()[0][0]
        .as_integer();
    println!("k-range chain-walk count: {chain_count} (complete = 1004000)");

    // ---- A: probes BEFORE any integrity run
    let pre: Vec<(i64, i64)> = [1000000361i64, 1000000375, 1020000001]
        .iter()
        .map(|id| {
            (
                *id,
                db.query(
                    "SELECT count(*) FROM events WHERE id = ?",
                    [Value::Integer(*id)],
                )
                .unwrap()[0][0]
                    .as_integer(),
            )
        })
        .collect();
    println!("A. probes BEFORE integrity: {:?}", pre);

    // ---- B: integrity_check, then the SAME probes again
    let _ = db.query("PRAGMA integrity_check", []).unwrap();
    let post: Vec<(i64, i64)> = [1000000361i64, 1000000375, 1020000001]
        .iter()
        .map(|id| {
            (
                *id,
                db.query(
                    "SELECT count(*) FROM events WHERE id = ?",
                    [Value::Integer(*id)],
                )
                .unwrap()[0][0]
                    .as_integer(),
            )
        })
        .collect();
    println!("B. probes AFTER integrity:  {:?}", post);

    // ---- C: fresh open, page_count/freelist first, then probes
    drop(db);
    let db = Database::open(&path).unwrap();
    let _ = db.query("PRAGMA page_count", []).unwrap();
    let _ = db.query("PRAGMA freelist_count", []).unwrap();
    let post2: Vec<(i64, i64)> = [1000000361i64, 1000000375, 1020000001]
        .iter()
        .map(|id| {
            (
                *id,
                db.query(
                    "SELECT count(*) FROM events WHERE id = ?",
                    [Value::Integer(*id)],
                )
                .unwrap()[0][0]
                    .as_integer(),
            )
        })
        .collect();
    println!("C. probes AFTER page pragmas: {:?}", post2);

    for id in [
        1000000361i64,
        1000000370,
        1000000375,
        1000000376,
        1000000442,
        1000000500,
        1020000001,
        1020000025,
    ] {
        let n: i64 = db
            .query(
                "SELECT count(*) FROM events WHERE id = ?",
                [Value::Integer(id)],
            )
            .unwrap()[0][0]
            .as_integer();
        println!("point probe id={id}: {n}");
    }
    println!("--- EXPLAIN point probe ---");
    for r in db
        .query(
            "EXPLAIN SELECT count(*) FROM events WHERE id = 1000000361",
            [],
        )
        .unwrap()
    {
        println!("PLAN {:?}", r);
    }
    // rowid-range window crossing the stale boundary
    let rows: Vec<i64> = db
        .query(
            "SELECT id FROM events WHERE id BETWEEN 1000000355 AND 1000000385 ORDER BY id",
            [],
        )
        .unwrap()
        .into_iter()
        .map(|r| r[0].as_integer())
        .collect();
    println!("BETWEEN 355..385: {:?}", rows);
    let rows2: Vec<i64> = db
        .query(
            "SELECT id FROM events WHERE id BETWEEN 1019999995 AND 1020000030 ORDER BY id",
            [],
        )
        .unwrap()
        .into_iter()
        .map(|r| r[0].as_integer())
        .collect();
    println!("BETWEEN 1019999995..1020000030: {:?}", rows2);
    // index-side: are tid2 txn0 k-entries findable?
    let (k, _) = gen_row(1020000001u64);
    let hits: Vec<i64> = db
        .query("SELECT id FROM events WHERE k = ?", [Value::Integer(k)])
        .unwrap()
        .into_iter()
        .map(|r| r[0].as_integer())
        .collect();
    println!("k-probe for 1020000001 (k={k}): {:?}", hits);
}

#[test]
#[ignore]
fn forensics() {
    let _g = serial();
    let arch = std::path::Path::new(FAIL_DIR)
        .join(std::env::var("ARCH").unwrap_or_else(|_| "attempt4".into()));
    let work = tempfile::tempdir().unwrap();
    for e in std::fs::read_dir(&arch).unwrap() {
        let e = e.unwrap();
        let name = e.file_name().into_string().unwrap();
        std::fs::copy(e.path(), work.path().join(&name)).unwrap();
    }
    let path = work.path().join("soak.db");
    let db = Database::open(&path).unwrap();

    // ---- 0. integrity (post-recovery) + page/freelist shape.
    for r in db.query("PRAGMA integrity_check", []).unwrap() {
        eprintln!("INTEGRITY: {:?}", r[0].as_text());
    }
    for pragma in ["PRAGMA page_count", "PRAGMA freelist_count"] {
        eprintln!(
            "{pragma} = {}",
            db.query(pragma, []).unwrap()[0][0].as_integer()
        );
    }

    // ---- 1. missing-writer-row census (table tree probe per id).
    let mut missing: Vec<i64> = Vec::new();
    let mut present: usize = 0;
    for id in writer_ids() {
        let n: i64 = db
            .query(
                "SELECT count(*) FROM events WHERE id = ?",
                [Value::Integer(id)],
            )
            .unwrap()[0][0]
            .as_integer();
        if n == 0 {
            missing.push(id);
        } else {
            present += 1;
        }
    }
    eprintln!(
        "writer rows present in table: {present}, missing: {}",
        missing.len()
    );
    // group by (tid, txn)
    let mut by_txn: std::collections::BTreeMap<(i64, i64), Vec<i64>> = Default::default();
    for id in &missing {
        let off = id - WRITER_ID_BASE;
        let tid = off / 10_000_000;
        let rest = off % 10_000_000;
        let t = (rest - 1) / PER_TXN;
        by_txn.entry((tid, t)).or_default().push(*id);
    }
    for ((tid, t), ids) in &by_txn {
        eprintln!(
            "  missing tid={tid} txn={t}: {}/{} rows: first={:?} last={:?}",
            ids.len(),
            PER_TXN,
            ids.first(),
            ids.last()
        );
    }

    // ---- 2. index presence of the missing rows.
    let mut in_index = 0usize;
    let mut not_in_index = 0usize;
    for id in &missing {
        let (k, _) = gen_row(*id as u64);
        let hits: Vec<i64> = db
            .query("SELECT id FROM events WHERE k = ?", [Value::Integer(k)])
            .unwrap()
            .into_iter()
            .map(|r| r[0].as_integer())
            .collect();
        if hits.contains(id) {
            in_index += 1;
        } else {
            not_in_index += 1;
        }
    }
    eprintln!(
        "missing-from-table rows: still in index = {in_index}, also gone from index = {not_in_index}"
    );

    // ---- 3. the raw writer-range scan order (no ORDER BY: tree order).
    let seq: Vec<i64> = db
        .query("SELECT id FROM events WHERE id > 999000000", [])
        .unwrap()
        .into_iter()
        .map(|r| r[0].as_integer())
        .collect();
    eprintln!("writer-range scan returned {} rows", seq.len());
    let mut breaks = 0usize;
    for i in 0..seq.len().saturating_sub(1) {
        if seq[i + 1] < seq[i] {
            if breaks < 10 {
                let lo = i.saturating_sub(3);
                let hi = (i + 5).min(seq.len());
                eprintln!("ORDER BREAK #{breaks} at pos {i}: ...{:?}...", &seq[lo..hi]);
            }
            breaks += 1;
        }
    }
    eprintln!("total order breaks in writer-range scan: {breaks}");

    // ---- 4. sequence vs expected: which writer rows does the scan MISS?
    let seq_set: std::collections::HashSet<i64> = seq.iter().copied().collect();
    let mut scan_missing: Vec<i64> = writer_ids().filter(|id| !seq_set.contains(id)).collect();
    scan_missing.sort_unstable();
    eprintln!(
        "scan-missing writer ids ({}): first 25 = {:?}",
        scan_missing.len(),
        &scan_missing[..scan_missing.len().min(25)]
    );

    // ---- 5. neighborhoods: for a few missing ids + the break area, show
    //         which ids exist nearby in the table (leaf-level view).
    for probe in scan_missing.iter().take(3) {
        let rows: Vec<i64> = db
            .query(
                "SELECT id FROM events WHERE id BETWEEN ? AND ?",
                [Value::Integer(probe - 3), Value::Integer(probe + 3)],
            )
            .unwrap()
            .into_iter()
            .map(|r| r[0].as_integer())
            .collect();
        eprintln!("neighborhood of missing {probe}: {:?}", rows);
    }

    // ---- 6. dbstat: the table tree's page shape (any short/odd leaves).
    if let Ok(inv) = db.query(
        "SELECT pageno, pagetype, ncell FROM dbstat WHERE name = 'events' ORDER BY pageno",
        [],
    ) {
        let mut leaves = 0usize;
        let mut short_leaves = 0usize;
        for r in &inv {
            let pt = r[1].as_text();
            let nc = r[2].as_integer();
            if pt == "leaf" {
                leaves += 1;
                if nc < 20 {
                    short_leaves += 1;
                    eprintln!("SHORT LEAF: {:?}", (r[0].as_integer(), pt, nc));
                }
            }
        }
        eprintln!(
            "events tree: {} pages, {} leaves, {} short(<20 cells)",
            inv.len(),
            leaves,
            short_leaves
        );
    }
}
