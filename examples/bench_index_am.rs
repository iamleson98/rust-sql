//! Benchmark: the specialized index access methods vs their fallbacks.
//!
//! * **GIN inverted index** (`USING gin(to_tsvector('english', body))`):
//!   a term query over 100k documents, full scan (parse every stored
//!   tsvector + evaluate `@@` per row) vs the inverted scan (parse the
//!   query once, seek the term's postings, fetch only candidates).
//! * **Spatial grid index** (`USING gist(geom, 1.0)`): KNN
//!   `ORDER BY geom <-> p LIMIT 10` over a 100k-point cloud, brute
//!   force (fetch + distance every row, sort) vs the expanding-window
//!   KNN scan.
//!
//! Both phases assert answer equality between the indexed and unindexed
//! paths — the benchmark only counts time when the two agree.

use std::time::Instant;

use rustqlite::{Database, Value};

const N_DOCS: usize = 100_000;
const N_POINTS: usize = 100_000;

fn lcg(s: &mut u64) -> f64 {
    *s = s
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    ((*s >> 11) as f64) / ((1u64 << 53) as f64)
}

fn ids(rows: &[Vec<Value>]) -> Vec<i64> {
    let mut v: Vec<i64> = rows.iter().map(|r| r[0].as_integer()).collect();
    v.sort_unstable();
    v
}

fn main() {
    let mut db = Database::open_in_memory().unwrap();

    // ================= GIN inverted index (full-text search) =================
    println!(
        "=== GIN inverted index: {} docs, rare-term queries ===",
        N_DOCS
    );
    db.execute("CREATE TABLE docs(id INTEGER PRIMARY KEY, body TEXT)", [])
        .unwrap();
    let t_insert = Instant::now();
    for chunk_start in (0..N_DOCS).step_by(500) {
        let mut sql = String::from("INSERT INTO docs VALUES ");
        let mut args: Vec<Value> = Vec::new();
        for i in chunk_start..(chunk_start + 500).min(N_DOCS) {
            if i > chunk_start {
                sql.push(',');
            }
            sql.push_str("(?, ?)");
            args.push(Value::Integer(i as i64));
            args.push(Value::Text(
                format!(
                    "document {} discusses engines storage retrieval indexes \
                     and the ranking of tokens tok{} across corpora",
                    i,
                    i % 1000
                )
                .into(),
            ));
        }
        db.execute(&sql, args).unwrap();
    }
    println!(
        "insert:            {:>8.1} ms",
        t_insert.elapsed().as_secs_f64() * 1e3
    );

    let terms = ["tok417", "tok83", "tok999"];
    // -- full scan (no index yet) --
    let t_scan = Instant::now();
    let mut full_results: Vec<Vec<i64>> = Vec::new();
    for term in terms {
        let rows = db
            .query(
                &format!(
                    "SELECT id FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', '{}')",
                    term
                ),
                [],
            )
            .unwrap();
        full_results.push(ids(&rows));
    }
    let full_ms = t_scan.elapsed().as_secs_f64() * 1e3 / terms.len() as f64;
    println!(
        "full scan:         {:>8.1} ms/query ({} rows)",
        full_ms,
        full_results[0].len()
    );

    // -- build the inverted index --
    let t_build = Instant::now();
    db.execute(
        "CREATE INDEX docs_gin ON docs USING gin(to_tsvector('english', body))",
        [],
    )
    .unwrap();
    println!(
        "gin build:         {:>8.1} ms",
        t_build.elapsed().as_secs_f64() * 1e3
    );

    // -- indexed queries --
    let plan = db
        .query(
            "EXPLAIN QUERY PLAN SELECT id FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', 'tok417')",
            [],
        )
        .unwrap();
    let plan_text: String = plan.iter().map(|r| r[3].as_text().to_string()).collect();
    assert!(
        plan_text.contains("INVERTED"),
        "plan must use the inverted scan: {plan_text}"
    );

    let t_gin = Instant::now();
    for (ti, term) in terms.iter().enumerate() {
        let rows = db
            .query(
                &format!(
                    "SELECT id FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', '{}')",
                    term
                ),
                [],
            )
            .unwrap();
        assert_eq!(
            ids(&rows),
            full_results[ti],
            "indexed results must equal full-scan results"
        );
    }
    let gin_ms = t_gin.elapsed().as_secs_f64() * 1e3 / terms.len() as f64;
    println!("gin scan:          {:>8.1} ms/query", gin_ms);
    println!("speedup:           {:>8.1}x", full_ms / gin_ms.max(1e-9));

    // ================= GIST spatial grid (KNN) =================
    println!(
        "\n=== GIST spatial grid: {} points in [0,1000)^2, ORDER BY <-> LIMIT 10 ===",
        N_POINTS
    );
    db.execute("CREATE TABLE p(id INTEGER PRIMARY KEY, geom TEXT)", [])
        .unwrap();
    let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
    let t_pins = Instant::now();
    for chunk_start in (0..N_POINTS).step_by(500) {
        let mut sql = String::from("INSERT INTO p VALUES ");
        let mut args: Vec<Value> = Vec::new();
        for i in chunk_start..(chunk_start + 500).min(N_POINTS) {
            if i > chunk_start {
                sql.push(',');
            }
            sql.push_str("(?, ?)");
            args.push(Value::Integer(i as i64));
            args.push(Value::Text(
                format!(
                    "POINT({:.3} {:.3})",
                    lcg(&mut seed) * 1000.0,
                    lcg(&mut seed) * 1000.0
                )
                .into(),
            ));
        }
        db.execute(&sql, args).unwrap();
    }
    println!(
        "insert:            {:>8.1} ms",
        t_pins.elapsed().as_secs_f64() * 1e3
    );

    let origins = [(123.45, 678.9), (500.0, 500.0), (13.37, 42.0)];
    // -- brute force (no index: fetch + distance every row, sort) --
    let t_brute = Instant::now();
    let mut brute_results: Vec<(Vec<i64>, Vec<f64>)> = Vec::new();
    for (ox, oy) in origins {
        let rows = db
            .query(
                &format!(
                    "SELECT id, geom <-> ST_Point({}, {}) AS d FROM p ORDER BY geom <-> ST_Point({}, {}) LIMIT 10",
                    ox, oy, ox, oy
                ),
                [],
            )
            .unwrap();
        brute_results.push((
            rows.iter().map(|r| r[0].as_integer()).collect(),
            rows.iter().map(|r| r[1].as_real()).collect(),
        ));
    }
    let brute_ms = t_brute.elapsed().as_secs_f64() * 1e3 / origins.len() as f64;
    println!("brute force:       {:>8.1} ms/query", brute_ms);

    // -- build the spatial grid index --
    let t_build = Instant::now();
    db.execute("CREATE INDEX p_gix ON p USING gist(geom, 1.0)", [])
        .unwrap();
    println!(
        "gist build:        {:>8.1} ms",
        t_build.elapsed().as_secs_f64() * 1e3
    );

    // -- indexed KNN --
    let plan = db
        .query(
            "EXPLAIN QUERY PLAN SELECT id FROM p ORDER BY geom <-> ST_Point(500, 500) LIMIT 10",
            [],
        )
        .unwrap();
    let plan_text: String = plan.iter().map(|r| r[3].as_text().to_string()).collect();
    assert!(
        plan_text.contains("KNN"),
        "plan must use the KNN scan: {plan_text}"
    );

    let t_knn = Instant::now();
    for (oi, (ox, oy)) in origins.iter().enumerate() {
        let rows = db
            .query(
                &format!(
                    "SELECT id, geom <-> ST_Point({}, {}) AS d FROM p ORDER BY geom <-> ST_Point({}, {}) LIMIT 10",
                    ox, oy, ox, oy
                ),
                [],
            )
            .unwrap();
        let (want_ids, want_d) = &brute_results[oi];
        let got_ids: Vec<i64> = rows.iter().map(|r| r[0].as_integer()).collect();
        let got_d: Vec<f64> = rows.iter().map(|r| r[1].as_real()).collect();
        let mut pairs: Vec<(f64, i64)> =
            got_d.iter().zip(&got_ids).map(|(d, i)| (*d, *i)).collect();
        pairs.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap().then(a.1.cmp(&b.1)));
        let mut want_pairs: Vec<(f64, i64)> =
            want_d.iter().zip(want_ids).map(|(d, i)| (*d, *i)).collect();
        want_pairs.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap().then(a.1.cmp(&b.1)));
        for (g, w) in pairs.iter().zip(&want_pairs) {
            assert!(
                (g.0 - w.0).abs() < 1e-9,
                "distance mismatch: {g:?} vs {w:?}"
            );
            assert_eq!(g.1, w.1, "KNN ids must match brute force");
        }
    }
    let knn_ms = t_knn.elapsed().as_secs_f64() * 1e3 / origins.len() as f64;
    println!("knn scan:          {:>8.1} ms/query", knn_ms);
    println!("speedup:           {:>8.1}x", brute_ms / knn_ms.max(1e-9));
}
