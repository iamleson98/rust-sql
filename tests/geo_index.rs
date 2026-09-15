//! End-to-end tests for the GiST-borrowed spatial grid index
//! (`CREATE INDEX ... USING gist(geom [, resolution])`) — KNN
//! `<->` acceleration and ST_DWithin range scans.
//!
//! Coverage strategy: KNN results are cross-checked against a brute
//! force ORDER BY computed WITHOUT the spatial index (identical twin
//! table), with the rowid tiebreak made explicit on both sides.

use rustqlite::{Database, Value};

fn db() -> Database {
    Database::open_in_memory().unwrap()
}

fn ids(db: &Database, sql: &str) -> Vec<i64> {
    let rows = db.query(sql, []).unwrap();
    rows.iter()
        .map(|r| match r.first() {
            Some(Value::Integer(i)) => *i,
            v => panic!("expected INTEGER id, got {v:?}"),
        })
        .collect()
}

/// Deterministic PRNG (xorshift64*) for reproducible point clouds.
struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed | 1)
    }
    fn next_f64(&mut self, lo: f64, hi: f64) -> f64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        let v = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
        // Use the high bits; map to [lo, hi).
        let unit = (v >> 11) as f64 / (1u64 << 53) as f64;
        lo + unit * (hi - lo)
    }
}

/// Two identical tables of random points: one gist-indexed, one not.
fn point_cloud(n: usize, seed: u64) -> (Database, Database) {
    let mut rng = Rng::new(seed);
    let mut values: Vec<String> = Vec::with_capacity(n);
    for i in 0..n {
        let x = rng.next_f64(-1.0, 1.0);
        let y = rng.next_f64(-1.0, 1.0);
        values.push(format!("({}, 'POINT({x:.6} {y:.6})')", i + 1));
    }
    let ddl = "CREATE TABLE pts(id INTEGER PRIMARY KEY, geom TEXT)";
    let insert = format!("INSERT INTO pts VALUES {}", values.join(","));

    let mut indexed = db();
    indexed.execute(ddl, []).unwrap();
    indexed.execute(&insert, []).unwrap();
    indexed
        .execute("CREATE INDEX pts_gix ON pts USING gist(geom, 0.01)", [])
        .unwrap();

    let mut plain = db();
    plain.execute(ddl, []).unwrap();
    plain.execute(&insert, []).unwrap();
    (indexed, plain)
}

/// KNN parity: the indexed window scan must return exactly the k
/// nearest rows, in ascending distance order with rowid tiebreaks —
/// the same order the brute-force sort produces with an explicit
/// distance-then-rowid ordering.
fn knn_of(db: &Database, table_alias_expr: &str, px: f64, py: f64, k: i64) -> Vec<i64> {
    let sql = format!(
        "SELECT id FROM pts {table_alias_expr} ORDER BY geom <-> ST_Point({px}, {py}), id LIMIT {k}"
    );
    ids(db, &sql)
}

#[test]
fn spatial_knn_matches_brute_force() {
    let (indexed, plain) = point_cloud(300, 0xC0FFEE);
    for (px, py, k) in [
        (0.0, 0.0, 5),
        (0.5, -0.5, 7),
        (-0.9, 0.9, 1),
        (0.123, 0.456, 12),
        (2.0, 2.0, 3),  // outside the cloud
        (-3.0, 0.1, 4), // far outside
    ] {
        let a = knn_of(&indexed, "", px, py, k);
        let b = knn_of(&plain, "", px, py, k);
        assert_eq!(a, b, "KNN({px},{py},{k}): indexed {a:?} vs brute {b:?}");
        assert_eq!(a.len(), k as usize, "KNN({px},{py},{k}) must return k rows");
    }
}

#[test]
fn spatial_knn_larger_cloud_many_k() {
    let (indexed, plain) = point_cloud(1000, 0xBADC0DE);
    for k in [1, 2, 10, 25, 50] {
        let a = knn_of(&indexed, "", 0.05, 0.05, k);
        let b = knn_of(&plain, "", 0.05, 0.05, k);
        assert_eq!(a, b, "k={k}: {a:?} vs {b:?}");
    }
}

#[test]
fn spatial_knn_with_residual_filter() {
    // k nearest among rows passing a WHERE: the node applies the
    // residual during collection and stops on the k-th PASSING
    // distance.
    let (indexed, plain) = point_cloud(400, 0x5EED);
    for k in [1, 3, 10] {
        let a = ids(
            &indexed,
            &format!(
                "SELECT id FROM pts WHERE id % 3 = 1 ORDER BY geom <-> ST_Point(0.2, 0.2), id LIMIT {k}"
            ),
        );
        let b = ids(
            &plain,
            &format!(
                "SELECT id FROM pts WHERE id % 3 = 1 ORDER BY geom <-> ST_Point(0.2, 0.2), id LIMIT {k}"
            ),
        );
        assert_eq!(a, b, "filtered KNN k={k}: {a:?} vs {b:?}");
        assert_eq!(a.len(), k as usize);
    }
}

#[test]
fn spatial_knn_k_larger_than_table() {
    // Fewer rows than k: the expansion must terminate via the
    // completion sweep and return every (passing) row.
    let (indexed, plain) = point_cloud(10, 0xABCDEF);
    let a = knn_of(&indexed, "", 0.0, 0.0, 25);
    let b = knn_of(&plain, "", 0.0, 0.0, 25);
    assert_eq!(a, b);
    assert_eq!(a.len(), 10);
}

#[test]
fn spatial_knn_k_larger_than_passing() {
    let (indexed, plain) = point_cloud(50, 0xFACE);
    let a = ids(
        &indexed,
        "SELECT id FROM pts WHERE id > 45 ORDER BY geom <-> ST_Point(0, 0), id LIMIT 20",
    );
    let b = ids(
        &plain,
        "SELECT id FROM pts WHERE id > 45 ORDER BY geom <-> ST_Point(0, 0), id LIMIT 20",
    );
    assert_eq!(a, b);
    assert_eq!(a.len(), 5);
}

#[test]
fn spatial_knn_st_distance_form_and_swapped_operands() {
    let (indexed, plain) = point_cloud(200, 0xD00D);
    // ST_Distance spelling.
    let a = ids(
        &indexed,
        "SELECT id FROM pts ORDER BY ST_Distance(geom, ST_Point(0.3, -0.3)), id LIMIT 6",
    );
    let b = ids(
        &plain,
        "SELECT id FROM pts ORDER BY ST_Distance(geom, ST_Point(0.3, -0.3)), id LIMIT 6",
    );
    assert_eq!(a, b);
    // Swapped operands: point <-> geom.
    let c = ids(
        &indexed,
        "SELECT id FROM pts ORDER BY ST_Point(0.3, -0.3) <-> geom, id LIMIT 6",
    );
    assert_eq!(c, b);
}

#[test]
fn spatial_knn_null_point_semantics() {
    // NULL point: all distances NULL; the node degenerates to the first
    // k residual-passing rows in rowid order (SQLite's stable-sort
    // behavior over equal NULL keys).
    let (indexed, plain) = point_cloud(30, 0x1DEA);
    let a = ids(
        &indexed,
        "SELECT id FROM pts ORDER BY geom <-> NULL, id LIMIT 4",
    );
    let b = ids(
        &plain,
        "SELECT id FROM pts ORDER BY geom <-> NULL, id LIMIT 4",
    );
    assert_eq!(a, b);
}

#[test]
fn spatial_knn_write_maintenance() {
    let (mut indexed, mut plain) = point_cloud(100, 0x7EA5);

    // INSERT a very-near point: it must displace the old nearest.
    indexed
        .execute("INSERT INTO pts VALUES (100001, 'POINT(0.001 0.001)')", [])
        .unwrap();
    plain
        .execute("INSERT INTO pts VALUES (100001, 'POINT(0.001 0.001)')", [])
        .unwrap();
    let a = knn_of(&indexed, "", 0.0, 0.0, 3);
    let b = knn_of(&plain, "", 0.0, 0.0, 3);
    assert_eq!(a, b);
    assert_eq!(a[0], 100001, "the inserted near point must be first");

    // UPDATE moves it far away.
    indexed
        .execute("UPDATE pts SET geom = 'POINT(5 5)' WHERE id = 100001", [])
        .unwrap();
    plain
        .execute("UPDATE pts SET geom = 'POINT(5 5)' WHERE id = 100001", [])
        .unwrap();
    let a = knn_of(&indexed, "", 0.0, 0.0, 3);
    let b = knn_of(&plain, "", 0.0, 0.0, 3);
    assert_eq!(a, b);
    assert_ne!(a[0], 100001);

    // DELETE removes it from candidates entirely.
    indexed
        .execute("DELETE FROM pts WHERE id = 100001", [])
        .unwrap();
    plain
        .execute("DELETE FROM pts WHERE id = 100001", [])
        .unwrap();
    let a = knn_of(&indexed, "", 0.0, 0.0, 3);
    let b = knn_of(&plain, "", 0.0, 0.0, 3);
    assert_eq!(a, b);
    assert!(!a.contains(&100001));
}

#[test]
fn spatial_knn_non_point_geometries() {
    // Polygons and linestrings: bbox cells cover them, and the true
    // `<->` distance (not the bbox) orders the output.
    let mut indexed = db();
    indexed
        .execute("CREATE TABLE zones(id INTEGER PRIMARY KEY, geom TEXT)", [])
        .unwrap();
    indexed
        .execute(
            "INSERT INTO zones VALUES
                (1, 'POLYGON((0.10 0.10, 0.20 0.10, 0.20 0.20, 0.10 0.20, 0.10 0.10))'),
                (2, 'LINESTRING(0.30 0, 0.30 0.5)'),
                (3, 'POINT(0.05 0)'),
                (4, 'POLYGON((9 9, 9.5 9, 9.5 9.5, 9 9.5, 9 9))')",
            [],
        )
        .unwrap();
    indexed
        .execute("CREATE INDEX zones_gix ON zones USING gist(geom, 0.01)", [])
        .unwrap();

    let mut plain = db();
    plain
        .execute("CREATE TABLE zones(id INTEGER PRIMARY KEY, geom TEXT)", [])
        .unwrap();
    plain
        .execute(
            "INSERT INTO zones VALUES
                (1, 'POLYGON((0.10 0.10, 0.20 0.10, 0.20 0.20, 0.10 0.20, 0.10 0.10))'),
                (2, 'LINESTRING(0.30 0, 0.30 0.5)'),
                (3, 'POINT(0.05 0)'),
                (4, 'POLYGON((9 9, 9.5 9, 9.5 9.5, 9 9.5, 9 9))')",
            [],
        )
        .unwrap();

    for (qx, qy, k) in [(0.0, 0.0, 4), (0.15, 0.15, 2), (0.31, 0.25, 2)] {
        let a = ids(
            &indexed,
            &format!(
                "SELECT id FROM zones ORDER BY geom <-> ST_Point({}, {}), id LIMIT {}",
                qx, qy, k
            ),
        );
        let b = ids(
            &plain,
            &format!(
                "SELECT id FROM zones ORDER BY geom <-> ST_Point({}, {}), id LIMIT {}",
                qx, qy, k
            ),
        );
        assert_eq!(
            a, b,
            "KNN over mixed geometries at ({qx}, {qy}): {a:?} vs {b:?}"
        );
    }
}

#[test]
fn spatial_dwithin_range_scan() {
    let (indexed, plain) = point_cloud(400, 0x9EED);
    for r in [0.05, 0.2, 0.8] {
        let sql = format!("SELECT id FROM pts WHERE ST_DWithin(geom, ST_Point(0.1, -0.1), {r})");
        let mut a = ids(&indexed, &sql);
        let mut b = ids(&plain, &sql);
        a.sort_unstable();
        b.sort_unstable();
        assert_eq!(a, b, "ST_DWithin r={r}: {} vs {} rows", a.len(), b.len());
    }
}

#[test]
fn spatial_dwithin_swapped_and_comparison_form() {
    let (indexed, plain) = point_cloud(200, 0x4B1D);
    // Swapped arguments.
    let a = ids(
        &indexed,
        "SELECT id FROM pts WHERE ST_DWithin(ST_Point(0.2, 0.2), geom, 0.15)",
    );
    let b = ids(
        &plain,
        "SELECT id FROM pts WHERE ST_DWithin(ST_Point(0.2, 0.2), geom, 0.15)",
    );
    let mut a2 = a.clone();
    let mut b2 = b.clone();
    a2.sort_unstable();
    b2.sort_unstable();
    assert_eq!(a2, b2);

    // `<-> < r` comparison form.
    let c = ids(
        &indexed,
        "SELECT id FROM pts WHERE geom <-> ST_Point(0.2, 0.2) < 0.15",
    );
    let d = ids(
        &plain,
        "SELECT id FROM pts WHERE geom <-> ST_Point(0.2, 0.2) < 0.15",
    );
    let mut c2 = c.clone();
    let mut d2 = d.clone();
    c2.sort_unstable();
    d2.sort_unstable();
    assert_eq!(c2, d2);
    assert_eq!(a2, c2, "ST_DWithin and <-> < r must agree");
}

#[test]
fn spatial_dwithin_with_other_predicates() {
    let (indexed, plain) = point_cloud(300, 0x2EED);
    let a = ids(
        &indexed,
        "SELECT id FROM pts WHERE ST_DWithin(geom, ST_Point(0, 0), 0.3) AND id % 2 = 0",
    );
    let b = ids(
        &plain,
        "SELECT id FROM pts WHERE ST_DWithin(geom, ST_Point(0, 0), 0.3) AND id % 2 = 0",
    );
    let mut a2 = a.clone();
    let mut b2 = b.clone();
    a2.sort_unstable();
    b2.sort_unstable();
    assert_eq!(a2, b2);
}

#[test]
fn spatial_null_geometries_excluded_from_knn() {
    // NULL geometry rows: not indexed, never emitted by the KNN node
    // (PostgreSQL's NULLS-LAST KNN semantics — the documented
    // divergence from SQLite's NULLS-FIRST sort rule).
    let mut indexed = db();
    indexed
        .execute("CREATE TABLE pts(id INTEGER PRIMARY KEY, geom TEXT)", [])
        .unwrap();
    indexed
        .execute(
            "INSERT INTO pts VALUES (1, NULL), (2, 'POINT(0.5 0.5)'), (3, NULL), (4, 'POINT(0.1 0.1)')",
            [],
        )
        .unwrap();
    indexed
        .execute("CREATE INDEX p_gix ON pts USING gist(geom)", [])
        .unwrap();
    let got = ids(
        &indexed,
        "SELECT id FROM pts ORDER BY geom <-> ST_Point(0, 0), id LIMIT 3",
    );
    assert_eq!(
        got,
        vec![4, 2],
        "NULL-geometry rows are excluded (PG semantics)"
    );
}

#[test]
fn spatial_persistence_round_trip() {
    let path = std::env::temp_dir().join("rustqlite_gist_test.db");
    let _ = std::fs::remove_file(&path);
    {
        let mut d = Database::open(&path).unwrap();
        d.execute("CREATE TABLE p(id INTEGER PRIMARY KEY, geom TEXT)", [])
            .unwrap();
        d.execute(
            "INSERT INTO p VALUES (1,'POINT(0 0)'), (2,'POINT(3 4)'), (3,'POINT(10 10)')",
            [],
        )
        .unwrap();
        d.execute("CREATE INDEX p_gix ON p USING gist(geom)", [])
            .unwrap();
    }
    {
        let d = Database::open(&path).unwrap();
        // (3,4) is 2.24 from (2,2); (0,0) is 2.83; (10,10) is 11.3.
        assert_eq!(
            ids(
                &d,
                "SELECT id FROM p ORDER BY geom <-> ST_Point(2, 2), id LIMIT 1"
            ),
            vec![2]
        );
        assert_eq!(
            ids(
                &d,
                "SELECT id FROM p WHERE ST_DWithin(geom, ST_Point(0, 0), 5)"
            ),
            vec![1, 2]
        );
        // Writes maintain the reopened index.
        let mut d2 = Database::open(&path).unwrap();
        d2.execute("INSERT INTO p VALUES (4, 'POINT(2.1 2.1)')", [])
            .unwrap();
        assert_eq!(
            ids(
                &d2,
                "SELECT id FROM p ORDER BY geom <-> ST_Point(2, 2), id LIMIT 1"
            ),
            vec![4]
        );
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn spatial_explain_shows_knn_and_spatial_scan() {
    let (d, _) = point_cloud(50, 0xE1E1);
    let knn = format!(
        "{:?}",
        d.query(
            "EXPLAIN SELECT id FROM pts ORDER BY geom <-> ST_Point(0, 0) LIMIT 3",
            []
        )
        .unwrap()
    );
    assert!(knn.contains("KNN"), "EXPLAIN must show the KNN scan: {knn}");
    assert!(
        knn.contains("pts_gix"),
        "EXPLAIN must name the gist index: {knn}"
    );

    let range = format!(
        "{:?}",
        d.query(
            "EXPLAIN SELECT id FROM pts WHERE ST_DWithin(geom, ST_Point(0, 0), 0.1)",
            []
        )
        .unwrap()
    );
    assert!(
        range.contains("SPATIAL"),
        "EXPLAIN must show the spatial scan: {range}"
    );
}

#[test]
fn spatial_validation_and_errors() {
    let mut d = db();
    d.execute("CREATE TABLE t(geom TEXT, other TEXT)", [])
        .unwrap();
    // UNIQUE rejected.
    assert!(d
        .execute("CREATE UNIQUE INDEX ix ON t USING gist(geom)", [])
        .is_err());
    // Too many columns.
    assert!(d
        .execute("CREATE INDEX ix ON t USING gist(geom, other)", [])
        .is_err());
    // Resolution must be a positive literal.
    assert!(d
        .execute("CREATE INDEX ix ON t USING gist(geom, -0.5)", [])
        .is_err());
    assert!(d
        .execute("CREATE INDEX ix ON t USING gist(geom, other)", [])
        .is_err());
    // Garbage geometry fails the write (strictness, like GIN).
    d.execute("CREATE INDEX ix ON t USING gist(geom)", [])
        .unwrap();
    assert!(d
        .execute("INSERT INTO t VALUES ('banana', NULL)", [])
        .is_err());
    // NULL geometry is fine.
    d.execute("INSERT INTO t VALUES (NULL, NULL)", []).unwrap();
}

#[test]
fn spatial_knn_alias_qualified() {
    let (indexed, plain) = point_cloud(150, 0xA11CE);
    let a = knn_of(&indexed, "AS p", -0.2, 0.3, 5);
    let b = knn_of(&plain, "AS p", -0.2, 0.3, 5);
    assert_eq!(a, b);
    assert_eq!(a.len(), 5);
}

#[test]
fn spatial_default_resolution_and_custom() {
    // Default resolution (0.01) vs an explicit coarser one (0.25):
    // identical results, different internal grids.
    let mut fine = db();
    let mut coarse = db();
    for d in [&mut fine, &mut coarse] {
        d.execute("CREATE TABLE pts(id INTEGER PRIMARY KEY, geom TEXT)", [])
            .unwrap();
        d.execute(
            "INSERT INTO pts VALUES (1,'POINT(0.02 0.02)'), (2,'POINT(0.12 0.12)'), (3,'POINT(-0.3 -0.3)')",
            [],
        )
        .unwrap();
    }
    fine.execute("CREATE INDEX f ON pts USING gist(geom)", [])
        .unwrap();
    coarse
        .execute("CREATE INDEX c ON pts USING gist(geom, 0.25)", [])
        .unwrap();
    let a = ids(
        &fine,
        "SELECT id FROM pts ORDER BY geom <-> ST_Point(0, 0), id LIMIT 2",
    );
    let b = ids(
        &coarse,
        "SELECT id FROM pts ORDER BY geom <-> ST_Point(0, 0), id LIMIT 2",
    );
    assert_eq!(a, b);
    assert_eq!(a, vec![1, 2]);
}
