//! A whole-table UPDATE of an indexed column with ~800-byte keys (four
//! per 4 KiB leaf) rebalances index leaves with their siblings on
//! overflow. Under an interior page with no right-most pointer (right=0:
//! the LAST cell's child is the right edge, and that cell's key is not an
//! upper bound of its child) the rebalance used to re-insert the last
//! cell as the group's bounding separator — sorted by its stale key it
//! landed before the new separators, the right edge came first, and the
//! index read "entries out of order" / "wrong # of entries". Seeded
//! (deterministic) blobs; without the fix several seeds below fail
//! within a few UPDATEs.

use rustqlite::{Database, Value};
use std::sync::atomic::{AtomicU64, Ordering};

static STATE: AtomicU64 = AtomicU64::new(1);

struct SeededBlob;

impl rustqlite::plugin::ScalarFunction for SeededBlob {
    fn name(&self) -> &str {
        "seeded_blob"
    }
    fn call(&self, _ctx: &rustqlite::plugin::FnCtx, args: &[Value]) -> rustqlite::Result<Value> {
        let n = match args.first() {
            Some(Value::Integer(n)) => *n as usize,
            _ => 0,
        };
        let mut x = STATE.load(Ordering::Relaxed);
        let mut v = Vec::with_capacity(n);
        for _ in 0..n {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            v.push((x >> 24) as u8);
        }
        STATE.store(x, Ordering::Relaxed);
        Ok(Value::Blob(v))
    }
}

fn run(seed: u64, iters: usize) -> Option<(usize, String)> {
    STATE.store(
        seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1,
        Ordering::Relaxed,
    );
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::open(dir.path().join("t.db")).unwrap();
    db.create_function(SeededBlob).unwrap();
    db.execute("CREATE TABLE t1(a, b)", ()).unwrap();
    db.execute("CREATE INDEX i1 ON t1(a, b)", ()).unwrap();
    db.execute(
        "INSERT INTO t1 VALUES(seeded_blob(400), seeded_blob(400))",
        (),
    )
    .unwrap();
    for _ in 0..8 {
        db.execute(
            "INSERT INTO t1 SELECT seeded_blob(400), seeded_blob(400) FROM t1",
            (),
        )
        .unwrap();
    }
    for i in 0..iters {
        let len = 399 - (i % 2);
        db.execute(&format!("UPDATE t1 SET a = seeded_blob({len})"), ())
            .unwrap();
        let r = format!("{:?}", db.query("PRAGMA integrity_check", ()).unwrap());
        if r != "[[Text(\"ok\")]]" {
            return Some((i, r));
        }
    }
    None
}

#[test]
fn seeded_update_storms_keep_index_order() {
    for seed in 0..40 {
        assert_eq!(run(seed, 12), None, "seed {seed}");
    }
}
