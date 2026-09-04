// Isolate mimalloc's RSS behavior: growing-Vec (doubling realloc) vs
// many small allocations, at the GROUP BY scale (10k groups).
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

#[derive(Clone, Default)]
struct St {
    a: i64,
    b: f64,
    c: i64,
    d: bool,
    e: bool,
    f: Box<[u8]>,
}

fn main() {
    println!("start: {:.1}MB", cur_mb());

    // Pattern A: ONE flat Vec growing by push (the new grouper).
    let mut v: Vec<St> = Vec::new();
    for _ in 0..10_000 {
        v.push(St::default());
    }
    println!(
        "flat vec 10k x {}B: {:.1}MB",
        std::mem::size_of::<St>(),
        cur_mb()
    );
    drop(v);
    println!("dropped: {:.1}MB", cur_mb());

    // Pattern B: 10k separate Vec<St> of 1 element (the old grouper).
    let mut vs: Vec<Vec<St>> = Vec::with_capacity(10_000);
    for _ in 0..10_000 {
        let mut s = Vec::new();
        s.push(St::default());
        vs.push(s);
    }
    println!("10k x Vec<St>(1): {:.1}MB", cur_mb());
    drop(vs);
    println!("dropped: {:.1}MB", cur_mb());

    pattern_d();

    // Pattern C: repeated resize(new_len) like the grouper's resize call.
    let mut r: Vec<St> = Vec::new();
    for i in 0..10_000 {
        r.resize(i + 1, St::default());
    }
    println!("resize-to-10k: {:.1}MB", cur_mb());
    drop(r);
    println!("dropped: {:.1}MB", cur_mb());
}

// Pattern D: interleaved (the actual grouper pattern): per group:
// keys push + states resize + slot insert (with rehash) + output row.
#[allow(dead_code)]
fn pattern_d() {
    let n: usize = 10_000;
    let mut keys: Vec<i64> = Vec::new();
    let mut states: Vec<St> = Vec::new();
    let mut slots: Vec<(u64, u32)> = Vec::new();
    let mut mask: usize = 0;
    let mut rows: Vec<Vec<i64>> = Vec::new();
    for gi in 0..n {
        keys.push(gi as i64);
        states.resize(gi + 1, St::default());
        let h = (gi as u64).wrapping_mul(0x9E3779B97F4A7C15);
        if gi + 1 > mask * 3 / 4 {
            let new_cap = (slots.len() * 2).max(16).next_power_of_two();
            slots = vec![(0u64, 0u32); new_cap];
            mask = new_cap - 1;
        }
        let mut slot = (h as usize) & mask;
        while slots[slot].1 != 0 {
            slot = (slot + 1) & mask;
        }
        slots[slot] = (h, (gi + 1) as u32);
        let mut row: Vec<i64> = Vec::with_capacity(3);
        row.push(gi as i64);
        row.push(1);
        row.push(2);
        rows.push(row);
    }
    println!(
        "pattern D interleaved: {:.1}MB (keys {} + states {} + slots {} + rows {})",
        cur_mb(),
        keys.capacity() * 8 / 1024,
        states.capacity() * std::mem::size_of::<St>() / 1024,
        slots.capacity() * 16 / 1024,
        rows.capacity() * 24 / 1024
    );
}
