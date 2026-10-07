//! Focused sweep-cost probe: apply the band-shape op multiset through the
//! raw sorted-sweep APIs on a realistic 1M-entry cyclic index tree and
//! break the cost into delete-pass / insert-pass, vs the per-op API.
//! Run: cargo run --release --example probe_sb -- [touched]

use rustqlite::storage::{Btree, Pager};

fn main() {
    let touched: u64 = std::env::args()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(200_000);
    let cycle: i64 = std::env::args()
        .nth(3)
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_000_000);
    let shift = 500_000i64;
    let dir = tempfile::tempdir().unwrap();

    let run_on = |file: &str, sweep: bool| -> (f64, f64, u32) {
        let path = dir.path().join(file);
        let psz: u32 = std::env::args()
            .nth(2)
            .and_then(|v| v.parse().ok())
            .unwrap_or(4096);
        let pager = Pager::open(&path, psz as usize).unwrap();
        let mut bt = Btree::create(&pager, true).unwrap();
        let interleaved = std::env::args().nth(4).map(|v| v == "i").unwrap_or(false);
        if interleaved {
            // The engine's shape: k = id % cycle, ids 1..=cycle*2 — every
            // key visited TWICE in id order (built through the per-op
            // path like the executor's INSERT loop, append hints decline).
            for id in 1..=cycle * 2 {
                let kb = (id % cycle).to_be_bytes();
                bt.insert_index(&kb, id).unwrap();
            }
        } else {
            let mut hint = None;
            for k in 0..cycle {
                let kb = k.to_be_bytes();
                hint = bt.insert_index_append_hinted(&kb, k, hint).unwrap();
            }
        }
        let mk_key = |v: i64| -> [u8; 8] { v.rem_euclid(cycle).to_be_bytes() };
        let mut dels: Vec<(Vec<u8>, i64)> = Vec::with_capacity(touched as usize);
        let mut ins: Vec<(Vec<u8>, i64)> = Vec::with_capacity(touched as usize);
        for i in 1..=touched as i64 {
            dels.push((mk_key(i).to_vec(), i));
            ins.push((mk_key(i + shift).to_vec(), i));
        }
        dels.sort_unstable_by(|a, b| (&a.0[..], a.1).cmp(&(&b.0[..], b.1)));
        ins.sort_unstable_by(|a, b| (&a.0[..], a.1).cmp(&(&b.0[..], b.1)));
        let t0 = std::time::Instant::now();
        if sweep {
            bt.delete_index_sorted(&dels).unwrap();
        } else {
            for d in &dels {
                let _ = bt.delete_index(&d.0, d.1).unwrap();
            }
        }
        let del_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let t1 = std::time::Instant::now();
        if sweep {
            bt.insert_index_sorted(&ins).unwrap();
        } else {
            for i in &ins {
                bt.insert_index(&i.0, i.1).unwrap();
            }
        }
        let ins_ms = t1.elapsed().as_secs_f64() * 1000.0;
        (del_ms, ins_ms, pager.n_pages())
    };

    let (d1, i1, p1) = run_on("perop.db", false);
    let (d2, i2, p2) = run_on("sweep.db", true);
    println!("touched={touched} on a {cycle}-entry cyclic index:");
    println!(
        "  per-op:  delete {d1:8.1} ms ({:.2} us/row)   insert {i1:8.1} ms ({:.2} us/row)   pages={p1}",
        d1 * 1000.0 / touched as f64,
        i1 * 1000.0 / touched as f64
    );
    println!(
        "  sweep:   delete {d2:8.1} ms ({:.2} us/row)   insert {i2:8.1} ms ({:.2} us/row)   pages={p2}",
        d2 * 1000.0 / touched as f64,
        i2 * 1000.0 / touched as f64
    );
    println!(
        "  speedup: delete {:.2}x  insert {:.2}x",
        d1 / d2.max(1e-9),
        i1 / i2.max(1e-9)
    );
}
