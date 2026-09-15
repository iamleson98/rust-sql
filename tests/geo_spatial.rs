//! End-to-end geospatial SQL (PostGIS-style ST_* functions + `<->`).
//!
//! Covers the SQL surface of `src/executor/geo.rs`: WKT parsing via
//! `ST_GeomFromText`, constructors (`ST_Point`/`ST_MakePoint`), accessors
//! (`ST_X`/`ST_Y`/`ST_GeometryType`/`ST_NPoints`/`ST_IsValid`/`ST_AsText`/
//! `ST_AsGeoJSON`), measurements (`ST_Distance`, `ST_DistanceSphere`,
//! `ST_DistanceSpheroid`, `ST_Area`, `ST_Length`, `ST_Perimeter`),
//! predicates (`ST_DWithin`, `ST_Contains`, `ST_Within`, `ST_Intersects`),
//! shapes (`ST_MakeEnvelope`, `ST_Envelope`, `ST_Expand`, `ST_Centroid`),
//! SRID tagging, the KNN `<->` operator in expressions and ORDER BY, and
//! a realistic nearest-neighbors query against a table with an index.

use rustqlite::{Database, Value};

fn db() -> Database {
    Database::open_in_memory().unwrap()
}

fn one(db: &Database, sql: &str) -> Value {
    let rows = db.query(sql, []).unwrap();
    rows.first()
        .and_then(|r| r.first().cloned())
        .unwrap_or(Value::Null)
}

fn text(db: &Database, sql: &str) -> String {
    match one(db, sql) {
        Value::Text(s) => s.to_string(),
        v => panic!("expected TEXT from `{sql}`, got {v:?}"),
    }
}

fn real(db: &Database, sql: &str) -> f64 {
    match one(db, sql) {
        Value::Real(f) => f,
        v => panic!("expected REAL from `{sql}`, got {v:?}"),
    }
}

fn int(db: &Database, sql: &str) -> i64 {
    match one(db, sql) {
        Value::Integer(i) => i,
        v => panic!("expected INTEGER from `{sql}`, got {v:?}"),
    }
}

#[test]
fn constructors_and_accessors() {
    let d = db();
    assert_eq!(real(&d, "SELECT ST_X(ST_Point(1.5, 2.5))"), 1.5);
    assert_eq!(real(&d, "SELECT ST_Y(ST_MakePoint(1.5, 2.5))"), 2.5);
    assert_eq!(
        text(&d, "SELECT ST_AsText(ST_GeomFromText('POINT(3 4)'))"),
        "POINT(3 4)"
    );
    assert_eq!(
        int(
            &d,
            "SELECT ST_NPoints(ST_GeomFromText('LINESTRING(0 0, 1 1, 2 2)'))"
        ),
        3
    );
    // ST_GeometryType returns the ST_-prefixed name (PostGIS behavior;
    // the unprefixed GEOMETRYTYPE() spelling is not implemented)
    assert_eq!(
        text(
            &d,
            "SELECT ST_GeometryType(ST_GeomFromText('POLYGON((0 0, 1 0, 1 1, 0 0))'))"
        ),
        "ST_Polygon"
    );
    assert_eq!(
        int(
            &d,
            "SELECT ST_IsValid(ST_GeomFromText('LINESTRING(0 0, 1 1)'))"
        ),
        1
    );
    // GeoJSON rendering (PostGIS trims integral .0)
    assert_eq!(
        text(&d, "SELECT ST_AsGeoJSON(ST_GeomFromText('POINT(1 2)'))"),
        "{\"type\":\"Point\",\"coordinates\":[1,2]}"
    );
    // SRID tagging (default 0, set/query round-trip)
    assert_eq!(int(&d, "SELECT ST_SRID(ST_GeomFromText('POINT(1 2)'))"), 0);
    assert_eq!(
        int(
            &d,
            "SELECT ST_SRID(ST_SetSRID(ST_GeomFromText('POINT(1 2)'), 4326))"
        ),
        4326
    );
}

#[test]
fn measurements() {
    let d = db();
    // planar distance
    assert_eq!(
        real(&d, "SELECT ST_Distance(ST_Point(0,0), ST_Point(3,4))"),
        5.0
    );
    // area with a hole: 16 - 4
    let donut = "ST_GeomFromText('POLYGON((0 0, 4 0, 4 4, 0 4, 0 0), (1 1, 3 1, 3 3, 1 3, 1 1))')";
    assert_eq!(real(&d, &format!("SELECT ST_Area({donut})")), 12.0);
    // linestring length is the OPEN path
    assert_eq!(
        real(
            &d,
            "SELECT ST_Length(ST_GeomFromText('LINESTRING(0 0, 3 4)'))"
        ),
        5.0
    );
    // polygon perimeter closes every ring
    assert_eq!(
        real(
            &d,
            "SELECT ST_Perimeter(ST_GeomFromText('POLYGON((0 0, 4 0, 4 4, 0 4, 0 0))'))"
        ),
        16.0
    );
    // centroid of the unit square shifted by 1
    let (cx, cy) = (
        real(
            &d,
            "SELECT ST_X(ST_Centroid(ST_GeomFromText('POLYGON((0 0, 2 0, 2 2, 0 2, 0 0))')))",
        ),
        real(
            &d,
            "SELECT ST_Y(ST_Centroid(ST_GeomFromText('POLYGON((0 0, 2 0, 2 2, 0 2, 0 0))')))",
        ),
    );
    assert!((cx - 1.0).abs() < 1e-9 && (cy - 1.0).abs() < 1e-9);
}

#[test]
fn spheroid_and_sphere_distances() {
    let d = db();
    // Classic Vincenty vector: Flinders Peak -> Buninyong = 54,972.271 m
    let d_m = real(
        &d,
        "SELECT ST_DistanceSpheroid(ST_GeomFromText('POINT(144.42486789 -37.95103342)'),
                                     ST_GeomFromText('POINT(143.92649553 -37.65282114)'))",
    );
    assert!(
        (d_m - 54972.271).abs() < 0.5,
        "vincenty got {d_m} m, expected ~54972.271"
    );
    // JFK -> LHR on the mean sphere: ~5,545-5,555 km depending on radius
    let sph = real(
        &d,
        "SELECT ST_DistanceSphere(ST_GeomFromText('POINT(-73.7781 40.6413)'),
                                  ST_GeomFromText('POINT(-0.1276 51.5053)'))",
    );
    assert!(
        (sph / 1000.0 - 5554.0).abs() < 30.0,
        "haversine got {sph} m"
    );
    // custom radius argument
    let r6 = real(
        &d,
        "SELECT ST_DistanceSphere(ST_Point(0, 0), ST_Point(0, 1), 6371000.0)",
    );
    assert!((r6 - 111_194.9).abs() < 5.0, "got {r6}");
}

#[test]
fn predicates() {
    let d = db();
    let square = "ST_GeomFromText('POLYGON((0 0, 10 0, 10 10, 0 10, 0 0))')";
    assert_eq!(
        int(&d, &format!("SELECT ST_Contains({square}, ST_Point(5, 5))")),
        1
    );
    assert_eq!(
        int(
            &d,
            &format!("SELECT ST_Contains({square}, ST_Point(15, 5))")
        ),
        0
    );
    // hole semantics: point in the hole is NOT contained
    let donut =
        "ST_GeomFromText('POLYGON((0 0, 10 0, 10 10, 0 10, 0 0), (4 4, 6 4, 6 6, 4 6, 4 4))')";
    assert_eq!(
        int(&d, &format!("SELECT ST_Contains({donut}, ST_Point(5, 5))")),
        0
    );
    assert_eq!(
        int(&d, &format!("SELECT ST_Contains({donut}, ST_Point(2, 2))")),
        1
    );
    // ST_Within is ST_Contains with swapped arguments
    assert_eq!(
        int(&d, &format!("SELECT ST_Within(ST_Point(5, 5), {square})")),
        1
    );
    // ST_Intersects: touching counts
    assert_eq!(
        int(
            &d,
            &format!("SELECT ST_Intersects({square}, ST_Point(10, 5))")
        ),
        1
    );
    assert_eq!(
        int(
            &d,
            &format!("SELECT ST_Intersects({square}, ST_Point(20, 5))")
        ),
        0
    );
    // ST_DWithin (comma-separated coordinates — `ST_Point(0 0)` is
    // invalid PostGIS function syntax)
    assert_eq!(
        int(&d, "SELECT ST_DWithin(ST_Point(0, 0), ST_Point(3, 4), 5.0)"),
        1
    );
    assert_eq!(
        int(&d, "SELECT ST_DWithin(ST_Point(0, 0), ST_Point(3, 4), 4.9)"),
        0
    );
}

#[test]
fn shapes() {
    let d = db();
    // MakeEnvelope builds a polygon from bounds
    assert_eq!(
        text(&d, "SELECT ST_AsText(ST_MakeEnvelope(0, 0, 2, 2))"),
        "POLYGON((0 0, 2 0, 2 2, 0 2, 0 0))"
    );
    // Envelope of a diagonal linestring is its bounding box
    assert_eq!(
        text(
            &d,
            "SELECT ST_AsText(ST_Envelope(ST_GeomFromText('LINESTRING(1 2, 5 8)')))"
        ),
        "POLYGON((1 2, 5 2, 5 8, 1 8, 1 2))"
    );
    // Expand grows a geometry's bbox by a delta (returns the expanded
    // envelope polygon)
    assert_eq!(
        text(
            &d,
            "SELECT ST_AsText(ST_Expand(ST_GeomFromText('POINT(1 1)'), 2.0))"
        ),
        "POLYGON((-1 -1, 3 -1, 3 3, -1 3, -1 -1))"
    );
}

#[test]
fn knn_operator_expressions() {
    let d = db();
    assert_eq!(real(&d, "SELECT ST_Point(0, 0) <-> ST_Point(3, 4)"), 5.0);
    // NULL propagation
    assert_eq!(one(&d, "SELECT NULL <-> ST_Point(0, 0)"), Value::Null);
    // malformed WKT raises
    assert!(d.query("SELECT 'garbage' <-> ST_Point(0, 0)", []).is_err());
    // comparison against the distance value
    assert_eq!(int(&d, "SELECT (ST_Point(0, 0) <-> ST_Point(3, 4)) < 6"), 1);
    // point-to-linestring minimum distance
    assert_eq!(
        real(
            &d,
            "SELECT ST_GeomFromText('LINESTRING(0 0, 0 10)') <-> ST_Point(3, 5)"
        ),
        3.0
    );
}

#[test]
fn knn_order_by_nearest_neighbors() {
    // The classic KNN shape: ORDER BY geom <-> origin with an index on the
    // geometry column.
    let mut d = db();
    d.execute(
        "CREATE TABLE cafes(id INTEGER PRIMARY KEY, name TEXT, geom TEXT);
         INSERT INTO cafes VALUES
           (1, 'near',   'POINT(1 1)'),
           (2, 'middle', 'POINT(5 5)'),
           (3, 'far',    'POINT(20 20)'),
           (4, 'nearest','POINT(0.5 0.5)');
         CREATE INDEX cafes_geom ON cafes(geom)",
        [],
    )
    .unwrap();
    let rows = d
        .query(
            "SELECT name FROM cafes
             ORDER BY geom <-> ST_Point(0, 0)
             LIMIT 3",
            [],
        )
        .unwrap();
    let names: Vec<&str> = rows
        .iter()
        .map(|r| match &r[0] {
            Value::Text(s) => s.as_ref(),
            v => panic!("expected TEXT, got {v:?}"),
        })
        .collect();
    assert_eq!(names, vec!["nearest", "near", "middle"]);

    // KNN + filter: within 10 units of the origin
    let n = int(
        &d,
        "SELECT COUNT(*) FROM cafes WHERE (geom <-> ST_Point(0, 0)) < 10",
    );
    assert_eq!(n, 3);
}

#[test]
fn geo_sql_table_workflow() {
    // A realistic mini-workflow: cities with lat/lon POINT WKT, queried
    // by containment in a bounding box and ranked by spheroid distance.
    let mut d = db();
    d.execute(
        "CREATE TABLE cities(name TEXT, geom TEXT);
         INSERT INTO cities VALUES
           ('London',   'POINT(-0.1276 51.5053)'),
           ('Paris',    'POINT(2.3522 48.8566)'),
           ('New York', 'POINT(-73.7781 40.6413)'),
           ('Berlin',   'POINT(13.4050 52.5200)')",
        [],
    )
    .unwrap();
    // European bounding box (rough)
    let n = int(
        &d,
        "SELECT COUNT(*) FROM cities
         WHERE ST_Intersects(
             ST_MakeEnvelope(-10, 40, 20, 60),
             geom)",
    );
    assert_eq!(n, 3, "London, Paris, Berlin in the box");
    // nearest European capital to London (spheroid meters)
    let rows = d
        .query(
            "SELECT name FROM cities
             WHERE name <> 'London'
             ORDER BY ST_DistanceSpheroid(geom, ST_GeomFromText('POINT(-0.1276 51.5053)'))
             LIMIT 1",
            [],
        )
        .unwrap();
    assert_eq!(rows[0][0], Value::Text("Paris".into()));
}

#[test]
fn null_handling() {
    let d = db();
    assert_eq!(
        one(&d, "SELECT ST_Distance(NULL, ST_Point(0, 0))"),
        Value::Null
    );
    assert_eq!(one(&d, "SELECT ST_X(NULL)"), Value::Null);
    assert_eq!(one(&d, "SELECT ST_Area(NULL)"), Value::Null);
    // bad WKT errors (matching ST_GeomFromText strictness)
    assert!(d
        .query("SELECT ST_GeomFromText('NOT A GEOMETRY')", [])
        .is_err());
    // unknown ST_ function still errors as "no such function"
    assert!(d.query("SELECT ST_NoSuchFunction(1)", []).is_err());
}

// ============================================================================
// Second-tier PostGIS surface: OGC topology predicates, accessors,
// affine transforms, ST_MakeLine, ST_ConvexHull.
// ============================================================================

fn pred(db: &Database, name: &str, a: &str, b: &str) -> i64 {
    int(
        db,
        &format!("SELECT {name}(ST_GeomFromText('{a}'), ST_GeomFromText('{b}'))"),
    )
}

#[test]
fn topology_predicates_sql() {
    let d = db();
    // touches: point on edge, T-junction lines, adjacent polygons
    assert_eq!(
        pred(
            &d,
            "ST_Touches",
            "POINT(1 0)",
            "POLYGON((0 0, 2 0, 2 2, 0 2, 0 0))"
        ),
        1
    );
    assert_eq!(
        pred(
            &d,
            "ST_Touches",
            "POINT(1 1)",
            "POLYGON((0 0, 2 0, 2 2, 0 2, 0 0))"
        ),
        0
    );
    assert_eq!(
        pred(
            &d,
            "ST_Touches",
            "LINESTRING(0 0, 2 0)",
            "LINESTRING(1 0, 1 3)"
        ),
        1
    );
    assert_eq!(
        pred(
            &d,
            "ST_Touches",
            "POLYGON((0 0, 1 0, 1 1, 0 1, 0 0))",
            "POLYGON((1 0, 2 0, 2 1, 1 1, 1 0))"
        ),
        1
    );
    // crosses: X lines, line through polygon
    assert_eq!(
        pred(
            &d,
            "ST_Crosses",
            "LINESTRING(0 0, 2 2)",
            "LINESTRING(0 2, 2 0)"
        ),
        1
    );
    assert_eq!(
        pred(
            &d,
            "ST_Crosses",
            "LINESTRING(-1 1, 3 1)",
            "POLYGON((0 0, 2 0, 2 2, 0 2, 0 0))"
        ),
        1
    );
    assert_eq!(
        pred(
            &d,
            "ST_Crosses",
            "LINESTRING(0 5, 3 5)",
            "POLYGON((0 0, 2 0, 2 2, 0 2, 0 0))"
        ),
        0
    );
    // overlaps: partial polygon + collinear line overlap
    assert_eq!(
        pred(
            &d,
            "ST_Overlaps",
            "POLYGON((0 0, 2 0, 2 2, 0 2, 0 0))",
            "POLYGON((1 0, 3 0, 3 2, 1 2, 1 0))"
        ),
        1
    );
    assert_eq!(
        pred(
            &d,
            "ST_Overlaps",
            "LINESTRING(0 0, 2 0)",
            "LINESTRING(1 0, 3 0)"
        ),
        1
    );
    assert_eq!(
        pred(
            &d,
            "ST_Overlaps",
            "LINESTRING(0 0, 4 0)",
            "LINESTRING(1 0, 2 0)"
        ),
        0
    );
    // equals: direction-insensitive lines, rotation/reversal-insensitive rings
    assert_eq!(
        pred(
            &d,
            "ST_Equals",
            "LINESTRING(0 0, 1 1, 2 2)",
            "LINESTRING(2 2, 1 1, 0 0)"
        ),
        1
    );
    assert_eq!(
        pred(
            &d,
            "ST_Equals",
            "POLYGON((0 0, 2 0, 2 2, 0 2, 0 0))",
            "POLYGON((2 2, 0 2, 0 0, 2 0, 2 2))"
        ),
        1
    );
    assert_eq!(pred(&d, "ST_Equals", "POINT(1 2)", "POINT(1 3)"), 0);
    // disjoint is the complement of intersects
    assert_eq!(
        pred(
            &d,
            "ST_Disjoint",
            "POINT(5 5)",
            "POLYGON((0 0, 2 0, 2 2, 0 2, 0 0))"
        ),
        1
    );
    assert_eq!(
        pred(
            &d,
            "ST_Disjoint",
            "POINT(1 1)",
            "POLYGON((0 0, 2 0, 2 2, 0 2, 0 0))"
        ),
        0
    );
    // NULL propagation through the predicate family
    assert_eq!(
        one(&d, "SELECT ST_Touches(NULL, ST_GeomFromText('POINT(0 0)'))"),
        Value::Null
    );
}

#[test]
fn closure_simplicity_accessors_sql() {
    let d = db();
    assert_eq!(
        int(
            &d,
            "SELECT ST_IsClosed(ST_GeomFromText('LINESTRING(0 0, 1 1, 0 0)'))"
        ),
        1
    );
    assert_eq!(
        int(
            &d,
            "SELECT ST_IsClosed(ST_GeomFromText('LINESTRING(0 0, 1 1)'))"
        ),
        0
    );
    // a valid square ring: closed AND simple
    assert_eq!(
        int(
            &d,
            "SELECT ST_IsRing(ST_GeomFromText('LINESTRING(0 0, 1 0, 1 1, 0 1, 0 0)'))"
        ),
        1
    );
    // bowtie: closed but not simple
    assert_eq!(
        int(
            &d,
            "SELECT ST_IsRing(ST_GeomFromText('LINESTRING(0 0, 2 2, 2 0, 0 2, 0 0)'))"
        ),
        0
    );
    // backtrack: closed but not simple
    assert_eq!(
        int(
            &d,
            "SELECT ST_IsRing(ST_GeomFromText('LINESTRING(0 0, 1 0, 0 0)'))"
        ),
        0
    );
    // line accessors
    let ls = "ST_GeomFromText('LINESTRING(0 0, 1 1, 2 2, 3 3)')";
    assert_eq!(
        text(&d, &format!("SELECT ST_AsText(ST_StartPoint({ls}))")),
        "POINT(0 0)"
    );
    assert_eq!(
        text(&d, &format!("SELECT ST_AsText(ST_EndPoint({ls}))")),
        "POINT(3 3)"
    );
    assert_eq!(
        text(&d, &format!("SELECT ST_AsText(ST_PointN({ls}, 2))")),
        "POINT(1 1)"
    );
    assert_eq!(
        text(&d, &format!("SELECT ST_AsText(ST_PointN({ls}, -1))")),
        "POINT(3 3)"
    );
    assert_eq!(int(&d, &format!("SELECT ST_NumPoints({ls})")), 4);
    // type mismatch → NULL
    assert_eq!(
        one(&d, "SELECT ST_NumPoints(ST_GeomFromText('POINT(0 0)'))"),
        Value::Null
    );
    assert_eq!(
        one(&d, "SELECT ST_StartPoint(ST_GeomFromText('POINT(0 0)'))"),
        Value::Null
    );
    // polygon ring accessors
    let poly = "ST_GeomFromText('POLYGON((0 0, 4 0, 4 4, 0 4, 0 0), (1 1, 2 1, 2 2, 1 2, 1 1))')";
    assert_eq!(int(&d, &format!("SELECT ST_NumInteriorRings({poly})")), 1);
    assert_eq!(
        text(&d, &format!("SELECT ST_AsText(ST_ExteriorRing({poly}))")),
        "LINESTRING(0 0, 4 0, 4 4, 0 4, 0 0)"
    );
    assert_eq!(
        text(
            &d,
            &format!("SELECT ST_AsText(ST_InteriorRingN({poly}, 1))")
        ),
        "LINESTRING(1 1, 2 1, 2 2, 1 2, 1 1)"
    );
    assert_eq!(
        one(&d, &format!("SELECT ST_InteriorRingN({poly}, 2)")),
        Value::Null
    );
}

#[test]
fn transforms_and_constructors_sql() {
    let d = db();
    // translate / scale / rotate / reverse
    assert_eq!(
        text(
            &d,
            "SELECT ST_AsText(ST_Translate(ST_GeomFromText('POINT(1 2)'), 10, -1))"
        ),
        "POINT(11 1)"
    );
    assert_eq!(
        text(
            &d,
            "SELECT ST_AsText(ST_Scale(ST_GeomFromText('LINESTRING(1 1, 3 5)'), 2, 0.5))"
        ),
        "LINESTRING(2 0.5, 6 2.5)"
    );
    // rotate a point 90 degrees CCW about (1, 1): (2, 1) -> (1, 2)... wait
    // (2,1) relative to (1,1) is (1,0); rotated 90° CCW -> (0,1) -> (1, 2)
    let r = text(
        &d,
        "SELECT ST_AsText(ST_Rotate(ST_GeomFromText('POINT(2 1)'), 1.5707963267948966, 1, 1))",
    );
    let p = one(&d, &format!("SELECT ST_X(ST_GeomFromText('{r}'))")).as_real();
    let q = one(&d, &format!("SELECT ST_Y(ST_GeomFromText('{r}'))")).as_real();
    assert!((p - 1.0).abs() < 1e-9, "x {p}");
    assert!((q - 2.0).abs() < 1e-9, "y {q}");
    assert_eq!(
        text(
            &d,
            "SELECT ST_AsText(ST_Reverse(ST_GeomFromText('LINESTRING(0 0, 1 1, 2 2)')))"
        ),
        "LINESTRING(2 2, 1 1, 0 0)"
    );
    // SRID survives transforms
    assert_eq!(
        text(
            &d,
            "SELECT ST_AsText(ST_Translate(ST_GeomFromText('SRID=4326;POINT(1 1)'), 1, 1))"
        ),
        "SRID=4326;POINT(2 2)"
    );
    // ST_MakeLine: point-point and line-line
    assert_eq!(
        text(
            &d,
            "SELECT ST_AsText(ST_MakeLine(ST_GeomFromText('POINT(0 0)'), ST_GeomFromText('POINT(1 1)')))"
        ),
        "LINESTRING(0 0, 1 1)"
    );
    assert_eq!(
        text(
            &d,
            "SELECT ST_AsText(ST_MakeLine(ST_GeomFromText('LINESTRING(0 0, 1 1)'), ST_GeomFromText('LINESTRING(2 2, 3 3)')))"
        ),
        "LINESTRING(0 0, 1 1, 2 2, 3 3)"
    );
    // ST_ConvexHull: square cloud → the corner square, closed ring
    assert_eq!(
        text(
            &d,
            "SELECT ST_AsText(ST_ConvexHull(ST_GeomFromText('LINESTRING(0 0, 1 0.5, 2 0, 2 2, 0.5 1, 0 2)')))"
        ),
        "POLYGON((0 0, 2 0, 2 2, 0 2, 0 0))"
    );
    // collinear cloud → the extremes as a LINESTRING
    assert_eq!(
        text(
            &d,
            "SELECT ST_AsText(ST_ConvexHull(ST_GeomFromText('LINESTRING(0 0, 1 1, 2 2, 3 3)')))"
        ),
        "LINESTRING(0 0, 3 3)"
    );
}

#[test]
fn topology_over_table_workflow() {
    // a realistic query combining predicates + transforms + the hull
    let mut d = db();
    d.execute("CREATE TABLE zones(id INTEGER PRIMARY KEY, geom TEXT)", [])
        .unwrap();
    d.execute(
        "INSERT INTO zones VALUES \
         (1, 'POLYGON((0 0, 10 0, 10 10, 0 10, 0 0))'), \
         (2, 'POLYGON((5 5, 15 5, 15 15, 5 15, 5 5))'), \
         (3, 'POLYGON((20 20, 25 20, 25 25, 20 25, 20 20))')",
        [],
    )
    .unwrap();
    // which zones does the (translated) road cross?
    let rows = d
        .query(
            "SELECT id FROM zones \
             WHERE ST_Crosses(ST_Translate(ST_GeomFromText('LINESTRING(-4 7, 13 7)'), 3, 0), geom) \
             ORDER BY id",
            [],
        )
        .unwrap();
    // translated road: LINESTRING(-1 7, 16 7) passes through zones 1 and 2
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], Value::Integer(1));
    assert_eq!(rows[1][0], Value::Integer(2));
    // which zones merely touch zone 3?
    let rows = d
        .query(
            "SELECT a.id, b.id FROM zones a JOIN zones b ON ST_Touches(a.geom, b.geom) \
             WHERE a.id < b.id",
            [],
        )
        .unwrap();
    assert_eq!(rows.len(), 0); // zones 1 and 2 OVERLAP, none merely touch
                               // overlapping pairs
    let rows = d
        .query(
            "SELECT a.id, b.id FROM zones a JOIN zones b ON ST_Overlaps(a.geom, b.geom) \
             WHERE a.id < b.id",
            [],
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Integer(1));
    assert_eq!(rows[0][1], Value::Integer(2));
    // equality after canonicalization differences
    let rows = d
        .query(
            "SELECT id FROM zones WHERE ST_Equals(geom, ST_GeomFromText('POLYGON((0 10, 0 0, 10 0, 10 10, 0 10))'))",
            [],
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Integer(1));
}
