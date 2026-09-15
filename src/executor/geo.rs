//! PostgreSQL/PostGIS-style geospatial functions over WKT TEXT.
//!
//! Geometries are stored as well-known TEXT in this engine (there is no
//! dedicated geometry storage class yet — same layering decision as the
//! FTS module: richer types live above the row store). The canonical
//! forms round-trip through [`render`]:
//!
//! * `POINT(x y)` — longitude first when used as geography (SRID 4326)
//! * `LINESTRING(x y, x y, …)`
//! * `POLYGON((x y, …), (hole …))` — first ring exterior, rest holes
//! * optional PostGIS EWKT SRID prefix: `SRID=4326;POINT(-73.98 40.77)`
//!
//! The Postgres-borrowed surface:
//!
//! * constructors/accessors: `ST_Point`, `ST_MakePoint`,
//!   `ST_GeomFromText`, `ST_SetSRID`, `ST_SRID`, `ST_X`, `ST_Y`,
//!   `ST_GeometryType`, `ST_NPoints`, `ST_IsValid`, `ST_AsText`,
//!   `ST_AsGeoJSON`
//! * measurements: `ST_Distance` (planar), `ST_DistanceSphere`
//!   (haversine, mean-Earth radius or a custom 3rd argument),
//!   `ST_DistanceSpheroid` (Vincenty inverse on WGS84),
//!   `ST_Area` (shoelace, holes subtracted), `ST_Length` (linestring
//!   path), `ST_Perimeter` (ring sum)
//! * predicates: `ST_DWithin` (planar; 4-arg form = geography meters),
//!   `ST_Contains`, `ST_Within`, `ST_Intersects`
//! * shapes: `ST_MakeEnvelope`, `ST_Envelope`, `ST_Expand`, `ST_Centroid`
//! * the KNN operator `a <-> b` (planar minimum distance — see
//!   [`eval_distance_op`]; `ORDER BY geom <-> point` is the classic
//!   nearest-neighbor shape)
//!
//! Documented divergences from PostGIS (v1 scope):
//! * 2D only — Z/M coordinates are rejected, no curve/geometry-collection
//!   types, no reprojection (SRIDs are carried, never transformed).
//! * geography functions interpret `POINT(lon lat)` in DEGREES and return
//!   METERS; the sphere radius is the IUGG mean 6371008.8 m unless a
//!   custom radius is passed.
//! * `ST_DistanceSpheroid` falls back to the sphere formula for
//!   near-antipodal points where Vincenty fails to converge.
//! * `ST_Intersects` between polygons uses boundary intersection or
//!   containment; `ST_Contains` on linestrings checks vertices only.
//! * No spatial index yet — `ST_DWithin`/`<->` filter by full scan; the
//!   index-access-method architecture that would accelerate them is on
//!   the roadmap.

use crate::error::{Error, Result};
use crate::types::Value;

/// PostGIS-style coordinate number format: shortest round-trip
/// representation, integral values WITHOUT a trailing `.0`
/// (`ST_AsText` prints `POINT(3 4)`, not `POINT(3.0 4.0)`; GeoJSON
/// prints `[1,2]`, not `[1.0,2.0]`). Deliberately diverges from the
/// SQLite `REAL→TEXT %!.17g` convention used by [`crate::types::format_real`] —
/// WKT/GeoJSON are PostGIS formats, not SQLite ones.
fn fmt_num(f: f64) -> String {
    if f.is_nan() || f.is_infinite() {
        // match the guardrails of the SQLite formatter for non-finites
        if f.is_nan() {
            return String::new();
        }
        return if f > 0.0 { "Inf" } else { "-Inf" }.to_string();
    }
    if f.trunc() == f && f.abs() < 9.007_199_254_740_992e15 {
        return format!("{}", f as i64);
    }
    format!("{f}") // Rust Display = shortest round-trip repr
}

fn fmt_coord(x: f64, y: f64) -> String {
    format!("{} {}", fmt_num(x), fmt_num(y))
}

/// IUGG mean Earth radius in meters (PostGIS's ST_DistanceSphere default
/// sphere, documented in its source as the mean radius).
const EARTH_RADIUS_M: f64 = 6371008.8;
/// WGS84 ellipsoid (Vincenty).
const WGS84_A: f64 = 6378137.0;
const WGS84_F: f64 = 1.0 / 298.257223563;

// ============================================================================
// Geometry model
// ============================================================================

#[derive(Clone, Debug, PartialEq)]
pub enum Geometry {
    Point(f64, f64),
    LineString(Vec<(f64, f64)>),
    /// Rings: exterior first, holes after.
    Polygon(Vec<Vec<(f64, f64)>>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Geo {
    pub srid: i64,
    pub geom: Geometry,
}

impl Geometry {
    fn kind(&self) -> &'static str {
        match self {
            Geometry::Point(..) => "ST_Point",
            Geometry::LineString(..) => "ST_LineString",
            Geometry::Polygon(..) => "ST_Polygon",
        }
    }

    fn n_points(&self) -> i64 {
        match self {
            Geometry::Point(..) => 1,
            Geometry::LineString(v) => v.len() as i64,
            Geometry::Polygon(rings) => rings.iter().map(|r| r.len() as i64).sum(),
        }
    }

    /// All vertices, in order.
    fn vertices(&self) -> Vec<(f64, f64)> {
        match self {
            Geometry::Point(x, y) => vec![(*x, *y)],
            Geometry::LineString(v) => v.clone(),
            Geometry::Polygon(rings) => {
                let mut out = Vec::new();
                for r in rings {
                    out.extend(r.iter().copied());
                }
                out
            }
        }
    }

    /// Boundary edges (segments). Points have none.
    fn edges(&self) -> Vec<((f64, f64), (f64, f64))> {
        match self {
            Geometry::Point(..) => Vec::new(),
            Geometry::LineString(v) => v.windows(2).map(|w| (w[0], w[1])).collect(),
            Geometry::Polygon(rings) => rings_edges(rings),
        }
    }

    pub(crate) fn bbox(&self) -> (f64, f64, f64, f64) {
        // (xmin, ymin, xmax, ymax)
        let v = self.vertices();
        let mut xmin = f64::INFINITY;
        let mut ymin = f64::INFINITY;
        let mut xmax = f64::NEG_INFINITY;
        let mut ymax = f64::NEG_INFINITY;
        for (x, y) in v {
            xmin = xmin.min(x);
            ymin = ymin.min(y);
            xmax = xmax.max(x);
            ymax = ymax.max(y);
        }
        (xmin, ymin, xmax, ymax)
    }
}

// ============================================================================
// WKT parsing / rendering
// ============================================================================

struct WktScanner<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> WktScanner<'a> {
    fn skip_ws(&mut self) {
        while self.i < self.b.len() && (self.b[self.i] as char).is_ascii_whitespace() {
            self.i += 1;
        }
    }

    fn eat(&mut self, c: u8) -> Result<()> {
        self.skip_ws();
        match self.b.get(self.i) {
            Some(&x) if x == c => {
                self.i += 1;
                Ok(())
            }
            _ => Err(Error::runtime("parse error - invalid geometry")),
        }
    }

    fn try_eat(&mut self, c: u8) -> bool {
        self.skip_ws();
        if self.b.get(self.i) == Some(&c) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn number(&mut self) -> Result<f64> {
        self.skip_ws();
        let start = self.i;
        while self.i < self.b.len()
            && (self.b[self.i].is_ascii_digit()
                || matches!(self.b[self.i], b'.' | b'-' | b'+' | b'e' | b'E'))
        {
            self.i += 1;
        }
        if start == self.i {
            return Err(Error::runtime("parse error - invalid geometry"));
        }
        let s = std::str::from_utf8(&self.b[start..self.i])
            .map_err(|_| Error::runtime("parse error - invalid geometry"))?;
        s.parse::<f64>()
            .map_err(|_| Error::runtime("parse error - invalid geometry"))
    }

    /// A coordinate pair: `x y`.
    fn coord(&mut self) -> Result<(f64, f64)> {
        let x = self.number()?;
        let y = self.number()?;
        Ok((x, y))
    }

    fn word(&mut self) -> String {
        self.skip_ws();
        let start = self.i;
        while self.i < self.b.len() && (self.b[self.i] as char).is_ascii_alphabetic() {
            self.i += 1;
        }
        String::from_utf8_lossy(&self.b[start..self.i]).to_uppercase()
    }

    fn done(&mut self) -> bool {
        self.skip_ws();
        self.i >= self.b.len()
    }
}

/// Parse `POINT(x y)`, `LINESTRING(...)`, `POLYGON((...),(...))` with an
/// optional `SRID=<n>;` prefix, any case, whitespace-tolerant.
pub fn parse_geometry(s: &str) -> Result<Geo> {
    let mut s = s.trim();
    let mut srid = 0i64;
    // EWKT prefix: SRID=4326;
    if let Some(rest) = s.strip_prefix("SRID=") {
        let Some(semi) = rest.find(';') else {
            return Err(Error::runtime(
                "parse error - invalid geometry (SRID without ';')",
            ));
        };
        srid = rest[..semi]
            .trim()
            .parse::<i64>()
            .map_err(|_| Error::runtime("parse error - invalid geometry (bad SRID)"))?;
        s = rest[semi + 1..].trim_start();
    }
    let mut sc = WktScanner {
        b: s.as_bytes(),
        i: 0,
    };
    let tag = sc.word();
    let geom = match tag.as_str() {
        "POINT" => {
            sc.eat(b'(')?;
            let (x, y) = sc.coord()?;
            // reject trailing junk inside the parens (e.g. a Z coordinate)
            let closing = sc.try_eat(b')');
            if !closing {
                return Err(Error::runtime(
                    "parse error - invalid geometry (only 2D points are supported)",
                ));
            }
            Geometry::Point(x, y)
        }
        "LINESTRING" => {
            sc.eat(b'(')?;
            let mut pts = Vec::new();
            loop {
                pts.push(sc.coord()?);
                if !sc.try_eat(b',') {
                    break;
                }
            }
            sc.eat(b')')?;
            if pts.len() < 2 {
                return Err(Error::runtime(
                    "parse error - invalid geometry (linestring needs >= 2 points)",
                ));
            }
            Geometry::LineString(pts)
        }
        "POLYGON" => {
            sc.eat(b'(')?;
            let mut rings = Vec::new();
            loop {
                sc.eat(b'(')?;
                let mut ring = Vec::new();
                loop {
                    ring.push(sc.coord()?);
                    if !sc.try_eat(b',') {
                        break;
                    }
                }
                sc.eat(b')')?;
                if ring.len() < 3 {
                    return Err(Error::runtime(
                        "parse error - invalid geometry (polygon ring needs >= 3 points)",
                    ));
                }
                rings.push(ring);
                if !sc.try_eat(b',') {
                    break;
                }
            }
            sc.eat(b')')?;
            Geometry::Polygon(rings)
        }
        "" => {
            return Err(Error::runtime(
                "parse error - invalid geometry (empty input)",
            ))
        }
        other => {
            return Err(Error::runtime(format!(
                "parse error - unsupported geometry type \"{other}\" (supported: POINT, LINESTRING, POLYGON)"
            )))
        }
    };
    if !sc.done() {
        return Err(Error::runtime(
            "parse error - invalid geometry (trailing input)",
        ));
    }
    Ok(Geo { srid, geom })
}

/// Canonical rendering (uppercase tags like PostGIS `ST_AsText`; the SRID
/// prefix is only emitted when nonzero, EWKT-style).
pub fn render(g: &Geo) -> String {
    let body = match &g.geom {
        Geometry::Point(x, y) => format!("POINT({})", fmt_coord(*x, *y)),
        Geometry::LineString(v) => {
            let pts: Vec<String> = v.iter().map(|(x, y)| fmt_coord(*x, *y)).collect();
            format!("LINESTRING({})", pts.join(", "))
        }
        Geometry::Polygon(rings) => {
            let rs: Vec<String> = rings
                .iter()
                .map(|r| {
                    let pts: Vec<String> = r.iter().map(|(x, y)| fmt_coord(*x, *y)).collect();
                    format!("({})", pts.join(", "))
                })
                .collect();
            format!("POLYGON({})", rs.join(", "))
        }
    };
    if g.srid != 0 {
        format!("SRID={};{}", g.srid, body)
    } else {
        body
    }
}

// ============================================================================
// Planar predicates
// ============================================================================

const EPS: f64 = 1e-12;

/// Is point `p` on segment `ab`?
fn on_segment(px: f64, py: f64, ax: f64, ay: f64, bx: f64, by: f64) -> bool {
    let cross = (bx - ax) * (py - ay) - (by - ay) * (px - ax);
    if cross.abs() > EPS {
        return false;
    }
    px >= ax.min(bx) - EPS
        && px <= ax.max(bx) + EPS
        && py >= ay.min(by) - EPS
        && py <= ay.max(by) + EPS
}

/// Point-segment distance.
fn point_seg_dist(px: f64, py: f64, ax: f64, ay: f64, bx: f64, by: f64) -> f64 {
    let dx = bx - ax;
    let dy = by - ay;
    let len2 = dx * dx + dy * dy;
    if len2 <= 0.0 {
        return ((px - ax).powi(2) + (py - ay).powi(2)).sqrt();
    }
    let t = (((px - ax) * dx + (py - ay) * dy) / len2).clamp(0.0, 1.0);
    let cx = ax + t * dx;
    let cy = ay + t * dy;
    ((px - cx).powi(2) + (py - cy).powi(2)).sqrt()
}

/// Do segments a1a2 and b1b2 properly share any point?
fn segments_intersect(
    a1x: f64,
    a1y: f64,
    a2x: f64,
    a2y: f64,
    b1x: f64,
    b1y: f64,
    b2x: f64,
    b2y: f64,
) -> bool {
    let d = |ax: f64, ay: f64, bx: f64, by: f64, px: f64, py: f64| -> f64 {
        (bx - ax) * (py - ay) - (by - ay) * (px - ax)
    };
    let d1 = d(b1x, b1y, b2x, b2y, a1x, a1y);
    let d2 = d(b1x, b1y, b2x, b2y, a2x, a2y);
    let d3 = d(a1x, a1y, a2x, a2y, b1x, b1y);
    let d4 = d(a1x, a1y, a2x, a2y, b2x, b2y);
    if ((d1 > 0.0 && d2 < 0.0) || (d1 < 0.0 && d2 > 0.0))
        && ((d3 > 0.0 && d4 < 0.0) || (d3 < 0.0 && d4 > 0.0))
    {
        return true;
    }
    // collinear overlap cases
    (d1.abs() <= EPS && on_segment(a1x, a1y, b1x, b1y, b2x, b2y))
        || (d2.abs() <= EPS && on_segment(a2x, a2y, b1x, b1y, b2x, b2y))
        || (d3.abs() <= EPS && on_segment(b1x, b1y, a1x, a1y, a2x, a2y))
        || (d4.abs() <= EPS && on_segment(b2x, b2y, a1x, a1y, a2x, a2y))
}

/// Segment-segment distance (0 when intersecting).
fn seg_seg_dist(
    a1x: f64,
    a1y: f64,
    a2x: f64,
    a2y: f64,
    b1x: f64,
    b1y: f64,
    b2x: f64,
    b2y: f64,
) -> f64 {
    if segments_intersect(a1x, a1y, a2x, a2y, b1x, b1y, b2x, b2y) {
        return 0.0;
    }
    point_seg_dist(a1x, a1y, b1x, b1y, b2x, b2y)
        .min(point_seg_dist(a2x, a2y, b1x, b1y, b2x, b2y))
        .min(point_seg_dist(b1x, b1y, a1x, a1y, a2x, a2y))
        .min(point_seg_dist(b2x, b2y, a1x, a1y, a2x, a2y))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RingPos {
    Inside,
    Outside,
    Boundary,
}

/// Even-odd ray casting for one ring.
fn point_in_ring(px: f64, py: f64, ring: &[(f64, f64)]) -> RingPos {
    let n = ring.len();
    if n < 3 {
        return RingPos::Outside;
    }
    // close the ring implicitly for the edge walk
    let closed = |i: usize| {
        let a = ring[i % n];
        let b = ring[(i + 1) % n];
        (a, b)
    };
    for i in 0..n {
        let ((ax, ay), (bx, by)) = closed(i);
        if on_segment(px, py, ax, ay, bx, by) {
            return RingPos::Boundary;
        }
    }
    let mut inside = false;
    for i in 0..n {
        let ((ax, ay), (bx, by)) = closed(i);
        if (ay > py) != (by > py) {
            let x_int = ax + (py - ay) * (bx - ax) / (by - ay);
            if px < x_int {
                inside = !inside;
            }
        }
    }
    if inside {
        RingPos::Inside
    } else {
        RingPos::Outside
    }
}

/// Point vs polygon: holes subtract; boundary wins.
fn point_in_polygon(px: f64, py: f64, rings: &[Vec<(f64, f64)>]) -> RingPos {
    let Some((ext, holes)) = rings.split_first() else {
        return RingPos::Outside;
    };
    match point_in_ring(px, py, ext) {
        RingPos::Outside => RingPos::Outside,
        RingPos::Boundary => RingPos::Boundary,
        RingPos::Inside => {
            for h in holes {
                match point_in_ring(px, py, h) {
                    RingPos::Inside => return RingPos::Outside,
                    RingPos::Boundary => return RingPos::Boundary,
                    RingPos::Outside => {}
                }
            }
            RingPos::Inside
        }
    }
}

/// A planar coordinate.
type Pt = (f64, f64);

/// A directed line segment (its two endpoints).
type Seg = (Pt, Pt);

/// Edges of a ring set, closing each ring implicitly (last → first).
fn rings_edges(rings: &[Vec<Pt>]) -> Vec<Seg> {
    let mut out = Vec::new();
    for r in rings {
        if r.len() >= 2 {
            let mut pts = r.clone();
            if r[0] != r[r.len() - 1] {
                pts.push(r[0]);
            }
            out.extend(pts.windows(2).map(|w| (w[0], w[1])));
        }
    }
    out
}

/// Do the two edge sets share any point (proper crossing or touching)?
fn edge_sets_intersect(ea: &[Seg], eb: &[Seg]) -> bool {
    for ((a1x, a1y), (a2x, a2y)) in ea.iter().copied() {
        for ((b1x, b1y), (b2x, b2y)) in eb.iter().copied() {
            if segments_intersect(a1x, a1y, a2x, a2y, b1x, b1y, b2x, b2y) {
                return true;
            }
        }
    }
    false
}

/// OGC contains: `outer` contains `g` — boundary touch does NOT count.
fn geo_contains(outer: &Geometry, g: &Geometry) -> bool {
    match (outer, g) {
        (Geometry::Polygon(rings), Geometry::Point(x, y)) => {
            point_in_polygon(*x, *y, rings) == RingPos::Inside
        }
        (Geometry::Polygon(rings), Geometry::LineString(v)) => v
            .iter()
            .all(|(x, y)| point_in_polygon(*x, *y, rings) == RingPos::Inside),
        (Geometry::Polygon(a), Geometry::Polygon(b)) => {
            // all vertices of b strictly inside a, and boundaries disjoint
            let b_inside = b.iter().all(|r| {
                r.iter()
                    .all(|(x, y)| point_in_polygon(*x, *y, a) == RingPos::Inside)
            });
            b_inside && {
                let ea = rings_edges(a);
                let eb = rings_edges(b);
                !edge_sets_intersect(&ea, &eb)
            }
        }
        // a point or linestring contains nothing (no area)
        _ => false,
    }
}

/// Do the two geometries' boundaries share any point?
fn boundaries_intersect(a: &Geometry, b: &Geometry) -> bool {
    let ea = a.edges();
    let eb = b.edges();
    if edge_sets_intersect(&ea, &eb) {
        return true;
    }
    // point-on-boundary cases (a point's "edge set" is empty)
    if let Geometry::Point(x, y) = a {
        for ((b1x, b1y), (b2x, b2y)) in eb.iter().copied() {
            if on_segment(*x, *y, b1x, b1y, b2x, b2y) {
                return true;
            }
        }
    }
    if let Geometry::Point(x, y) = b {
        for ((a1x, a1y), (a2x, a2y)) in ea.iter().copied() {
            if on_segment(*x, *y, a1x, a1y, a2x, a2y) {
                return true;
            }
        }
    }
    false
}

/// OGC intersects: shared space OR boundary touch OR containment either
/// way.
fn geo_intersects(a: &Geometry, b: &Geometry) -> bool {
    if geo_contains(a, b) || geo_contains(b, a) {
        return true;
    }
    match (a, b) {
        (Geometry::Point(x1, y1), Geometry::Point(x2, y2)) => {
            (x1 - x2).abs() <= EPS && (y1 - y2).abs() <= EPS
        }
        (Geometry::Point(x, y), Geometry::Polygon(rings))
        | (Geometry::Polygon(rings), Geometry::Point(x, y)) => {
            // boundary counts as intersecting (unlike contains)
            point_in_polygon(*x, *y, rings) != RingPos::Outside
        }
        (Geometry::Point(x, y), Geometry::LineString(v))
        | (Geometry::LineString(v), Geometry::Point(x, y)) => v
            .windows(2)
            .any(|w| on_segment(*x, *y, w[0].0, w[0].1, w[1].0, w[1].1)),
        _ => boundaries_intersect(a, b),
    }
}

// ============================================================================
// Planar measurements
// ============================================================================

/// Minimum planar distance between two geometries (PostGIS ST_Distance
/// semantics: 0 when they intersect/overlap).
pub fn planar_distance(a: &Geometry, b: &Geometry) -> f64 {
    if let (Geometry::Point(x1, y1), Geometry::Point(x2, y2)) = (a, b) {
        return ((x1 - x2).powi(2) + (y1 - y2).powi(2)).sqrt();
    }
    if geo_intersects(a, b) {
        return 0.0;
    }
    // point vs anything: min distance to the other's boundary/vertices
    if let Geometry::Point(x, y) = a {
        return geom_point_dist(*x, *y, b);
    }
    if let Geometry::Point(x, y) = b {
        return geom_point_dist(*x, *y, a);
    }
    // edge set vs edge set
    let ea = a.edges();
    let eb = b.edges();
    let mut best = f64::INFINITY;
    for ((a1x, a1y), (a2x, a2y)) in ea.iter().copied() {
        for ((b1x, b1y), (b2x, b2y)) in eb.iter().copied() {
            best = best.min(seg_seg_dist(a1x, a1y, a2x, a2y, b1x, b1y, b2x, b2y));
        }
    }
    if best.is_finite() {
        best
    } else {
        // both edge-less and non-point (cannot happen today) — fall back
        // to vertex-pair minimum
        let mut best = f64::INFINITY;
        for (x1, y1) in a.vertices() {
            for (x2, y2) in b.vertices() {
                best = best.min(((x1 - x2).powi(2) + (y1 - y2).powi(2)).sqrt());
            }
        }
        best
    }
}

fn geom_point_dist(px: f64, py: f64, g: &Geometry) -> f64 {
    let mut best = f64::INFINITY;
    for ((ax, ay), (bx, by)) in g.edges() {
        best = best.min(point_seg_dist(px, py, ax, ay, bx, by));
    }
    // linestrings/polygons always have edges; a bare point reaches here
    // only via the intersect early-return above
    if best.is_finite() {
        best
    } else {
        match g {
            Geometry::Point(x, y) => ((px - x).powi(2) + (py - y).powi(2)).sqrt(),
            _ => best,
        }
    }
}

/// Shoelace area of one ring (absolute value).
fn ring_area(ring: &[(f64, f64)]) -> f64 {
    let n = ring.len();
    if n < 3 {
        return 0.0;
    }
    let mut acc = 0.0;
    for i in 0..n {
        let (x1, y1) = ring[i];
        let (x2, y2) = ring[(i + 1) % n];
        acc += x1 * y2 - x2 * y1;
    }
    (acc / 2.0).abs()
}

/// Polygon area (exterior minus holes).
fn polygon_area(rings: &[Vec<(f64, f64)>]) -> f64 {
    let Some((ext, holes)) = rings.split_first() else {
        return 0.0;
    };
    let mut a = ring_area(ext);
    for h in holes {
        a -= ring_area(h);
    }
    a.max(0.0)
}

fn ring_perimeter(ring: &[(f64, f64)]) -> f64 {
    let n = ring.len();
    if n < 2 {
        return 0.0;
    }
    let mut p = 0.0;
    for i in 0..n {
        let (x1, y1) = ring[i];
        let (x2, y2) = ring[(i + 1) % n];
        p += ((x2 - x1).powi(2) + (y2 - y1).powi(2)).sqrt();
    }
    p
}

/// Open-path length of a linestring: consecutive segments only — unlike
/// [`ring_perimeter`] there is NO closing edge back to the first point
/// (PostGIS: `ST_Length('LINESTRING(0 0, 3 4)')` is 5, not 10).
fn path_length(pts: &[(f64, f64)]) -> f64 {
    pts.windows(2)
        .map(|w| {
            let (x1, y1) = w[0];
            let (x2, y2) = w[1];
            ((x2 - x1).powi(2) + (y2 - y1).powi(2)).sqrt()
        })
        .sum()
}

/// Area-weighted centroid of a polygon (single ring, holes ignored —
/// documented divergence).
fn polygon_centroid(rings: &[Vec<(f64, f64)>]) -> (f64, f64) {
    let Some((ext, _)) = rings.split_first() else {
        return (0.0, 0.0);
    };
    let n = ext.len();
    if n < 3 {
        return (0.0, 0.0);
    }
    let mut cx = 0.0;
    let mut cy = 0.0;
    let mut a = 0.0;
    for i in 0..n {
        let (x1, y1) = ext[i];
        let (x2, y2) = ext[(i + 1) % n];
        let cross = x1 * y2 - x2 * y1;
        cx += (x1 + x2) * cross;
        cy += (y1 + y2) * cross;
        a += cross;
    }
    if a.abs() < EPS {
        // degenerate: average the vertices
        let (sx, sy): (f64, f64) = ext
            .iter()
            .fold((0.0, 0.0), |(ax, ay), (x, y)| (ax + x, ay + y));
        return (sx / n as f64, sy / n as f64);
    }
    (cx / (3.0 * a), cy / (3.0 * a))
}

// ============================================================================
// Sphere / spheroid distances (degrees in, meters out)
// ============================================================================

fn to_rad(deg: f64) -> f64 {
    deg * std::f64::consts::PI / 180.0
}

/// Haversine great-circle distance on a sphere of `radius` meters.
/// Inputs are (lon, lat) in degrees.
pub fn haversine_m(lon1: f64, lat1: f64, lon2: f64, lat2: f64, radius: f64) -> f64 {
    let (la1, la2) = (to_rad(lat1), to_rad(lat2));
    let dlat = to_rad(lat2 - lat1);
    let dlon = to_rad(lon2 - lon1);
    let a = (dlat / 2.0).sin().powi(2) + la1.cos() * la2.cos() * (dlon / 2.0).sin().powi(2);
    let c = 2.0 * a.sqrt().clamp(0.0, 1.0).asin();
    radius * c
}

/// Vincenty inverse on the WGS84 ellipsoid. Returns None when the
/// iteration fails to converge (near-antipodal points).
#[allow(unused_assignments)] // cos2_sigma_m is recomputed post-loop
pub fn vincenty_m(lon1: f64, lat1: f64, lon2: f64, lat2: f64) -> Option<f64> {
    let a = WGS84_A;
    let f = WGS84_F;
    let b = (1.0 - f) * a;
    let l = to_rad(lon2 - lon1);
    let (la1, la2) = (to_rad(lat1), to_rad(lat2));
    let u1 = ((1.0 - f) * la1.tan()).atan();
    let u2 = ((1.0 - f) * la2.tan()).atan();
    let (su1, cu1) = (u1.sin(), u1.cos());
    let (su2, cu2) = (u2.sin(), u2.cos());
    let mut lambda = l;
    let mut sin_sigma;
    let mut cos_sigma;
    let mut sigma;
    let mut cos2_sigma_m = 0.0;
    let mut sin_alpha;
    let mut cos2_alpha;
    for _ in 0..200 {
        let sl = lambda.sin();
        let cl = lambda.cos();
        sin_sigma = ((cu2 * sl).powi(2) + (cu1 * su2 - su1 * cu2 * cl).powi(2)).sqrt();
        if sin_sigma.abs() < 1e-15 {
            return Some(0.0); // coincident points
        }
        cos_sigma = su1 * su2 + cu1 * cu2 * cl;
        sigma = sin_sigma.atan2(cos_sigma);
        sin_alpha = cu1 * cu2 * sl / sin_sigma;
        cos2_alpha = 1.0 - sin_alpha.powi(2);
        cos2_sigma_m = if cos2_alpha > 1e-12 {
            cos_sigma - 2.0 * su1 * su2 / cos2_alpha
        } else {
            0.0 // equatorial line
        };
        let c = f / 16.0 * cos2_alpha * (4.0 + f * (4.0 - 3.0 * cos2_alpha));
        let lambda_prev = lambda;
        // Vincenty (1975) eq. 3-10: the OUTER factor is sin_alpha (odd in
        // lambda), NOT sin_sigma — using sin_sigma loses the sign of the
        // longitude difference and skews the fixed point (~74 m on the
        // classic Flinders–Buninyong vector, plus a mirror asymmetry).
        lambda = l
            + (1.0 - c)
                * f
                * sin_alpha
                * (sigma
                    + c * sin_sigma
                        * (cos2_sigma_m + c * cos_sigma * (-1.0 + 2.0 * cos2_sigma_m.powi(2))));
        if (lambda - lambda_prev).abs() < 1e-12 {
            break;
        }
        if lambda.abs() > std::f64::consts::PI {
            return None; // failed to converge
        }
    }
    // recompute final sigma values for the distance formula
    let sl = lambda.sin();
    let cl = lambda.cos();
    sin_sigma = ((cu2 * sl).powi(2) + (cu1 * su2 - su1 * cu2 * cl).powi(2)).sqrt();
    cos_sigma = su1 * su2 + cu1 * cu2 * cl;
    if sin_sigma.abs() < 1e-15 {
        return Some(0.0);
    }
    sigma = sin_sigma.atan2(cos_sigma);
    let sin_alpha = cu1 * cu2 * sl / sin_sigma;
    let cos2_alpha = 1.0 - sin_alpha.powi(2);
    cos2_sigma_m = if cos2_alpha > 1e-12 {
        cos_sigma - 2.0 * su1 * su2 / cos2_alpha
    } else {
        0.0
    };
    let u2 = cos2_alpha * (a * a - b * b) / (b * b);
    let big_a = 1.0 + u2 / 16384.0 * (4096.0 + u2 * (-768.0 + u2 * (320.0 - 175.0 * u2)));
    let big_b = u2 / 1024.0 * (256.0 + u2 * (-128.0 + u2 * (74.0 - 47.0 * u2)));
    let delta_sigma = big_b
        * sin_sigma
        * (cos2_sigma_m
            + big_b / 4.0
                * (cos_sigma * (-1.0 + 2.0 * cos2_sigma_m.powi(2))
                    - big_b / 6.0
                        * cos2_sigma_m
                        * (-3.0 + 4.0 * sin_sigma.powi(2))
                        * (-3.0 + 4.0 * cos2_sigma_m.powi(2))));
    Some(b * big_a * (sigma - delta_sigma))
}

// ============================================================================
// SQL-facing helpers
// ============================================================================

fn geo_arg(args: &[Value], i: usize) -> Result<Option<Geo>> {
    match args.get(i) {
        Some(v) if !v.is_null() => parse_geometry(&v.as_text()).map(Some),
        _ => Ok(None),
    }
}

fn point_arg(args: &[Value], i: usize) -> Result<Option<(f64, f64)>> {
    match args.get(i) {
        Some(v) if !v.is_null() => {
            let g = parse_geometry(&v.as_text())?;
            match g.geom {
                Geometry::Point(x, y) => Ok(Some((x, y))),
                _ => Err(Error::runtime(format!(
                    "{} only accepts points, got {}",
                    fn_name_hint(args),
                    g.geom.kind()
                ))),
            }
        }
        _ => Ok(None),
    }
}

/// Best-effort context for error messages (the dispatch name is not
/// threaded through every helper; the message stays useful either way).
fn fn_name_hint(_args: &[Value]) -> &'static str {
    "ST_X/ST_Y"
}

fn real_arg(args: &[Value], i: usize) -> Result<Option<f64>> {
    match args.get(i) {
        Some(v) if !v.is_null() => Ok(Some(v.as_real())),
        _ => Ok(None),
    }
}

fn int_arg(args: &[Value], i: usize) -> Result<Option<i64>> {
    match args.get(i) {
        Some(v) if !v.is_null() => Ok(Some(v.as_integer())),
        _ => Ok(None),
    }
}

fn bool_arg(args: &[Value], i: usize) -> bool {
    matches!(args.get(i), Some(v) if v.as_integer() != 0)
}

fn make_envelope(xmin: f64, ymin: f64, xmax: f64, ymax: f64, srid: i64) -> Geo {
    Geo {
        srid,
        geom: Geometry::Polygon(vec![vec![
            (xmin, ymin),
            (xmax, ymin),
            (xmax, ymax),
            (xmin, ymax),
            (xmin, ymin),
        ]]),
    }
}

/// The `<->` KNN operator: planar minimum distance between two geometries
/// (NULL on either side → NULL).
pub fn eval_distance_op(l: &Value, r: &Value) -> Result<Value> {
    if l.is_null() || r.is_null() {
        return Ok(Value::Null);
    }
    let a = parse_geometry(&l.as_text())
        .map_err(|e| Error::runtime(format!("{e} (left operand of <->)")))?;
    let b = parse_geometry(&r.as_text())
        .map_err(|e| Error::runtime(format!("{e} (right operand of <->)")))?;
    Ok(Value::Real(planar_distance(&a.geom, &b.geom)))
}

/// Dispatch table for geospatial functions. Returns `Ok(None)` when the
/// name is not a geo function (the caller tries the next family).
pub fn call_geo_function(name: &str, args: &[Value]) -> Result<Option<Value>> {
    let out = match name {
        // ---- constructors ----
        "st_point" | "st_makepoint" => {
            let (x, y) = match (real_arg(args, 0)?, real_arg(args, 1)?) {
                (Some(x), Some(y)) => (x, y),
                _ => return Ok(Some(Value::Null)),
            };
            let srid = int_arg(args, 2)?.unwrap_or(0);
            Value::Text(
                render(&Geo {
                    srid,
                    geom: Geometry::Point(x, y),
                })
                .into(),
            )
        }
        "st_geomfromtext" => {
            let Some(wkt) = args.first().map(|v| v.as_text()) else {
                return Ok(Some(Value::Null));
            };
            if args.first().is_some_and(|v| v.is_null()) {
                return Ok(Some(Value::Null));
            }
            let mut g = parse_geometry(&wkt)?;
            if let Some(srid) = int_arg(args, 1)? {
                g.srid = srid;
            }
            Value::Text(render(&g).into())
        }
        "st_setsrid" => {
            let Some(g) = geo_arg(args, 0)? else {
                return Ok(Some(Value::Null));
            };
            let srid = int_arg(args, 1)?.unwrap_or(0);
            Value::Text(render(&Geo { srid, geom: g.geom }).into())
        }
        // ---- accessors ----
        "st_srid" => {
            let Some(g) = geo_arg(args, 0)? else {
                return Ok(Some(Value::Null));
            };
            Value::Integer(g.srid)
        }
        "st_x" => match point_arg(args, 0)? {
            Some((x, _)) => Value::Real(x),
            None => Value::Null,
        },
        "st_y" => match point_arg(args, 0)? {
            Some((_, y)) => Value::Real(y),
            None => Value::Null,
        },
        "st_geometrytype" => match geo_arg(args, 0)? {
            Some(g) => Value::Text(g.geom.kind().into()),
            None => Value::Null,
        },
        "st_npoints" => match geo_arg(args, 0)? {
            Some(g) => Value::Integer(g.geom.n_points()),
            None => Value::Null,
        },
        "st_isvalid" => match geo_arg(args, 0)? {
            Some(g) => Value::Integer(i64::from(is_valid(&g.geom))),
            None => Value::Null,
        },
        "st_astext" => match geo_arg(args, 0)? {
            Some(g) => Value::Text(render(&g).into()),
            None => Value::Null,
        },
        "st_asgeojson" => match geo_arg(args, 0)? {
            Some(g) => Value::Text(as_geojson(&g.geom).into()),
            None => Value::Null,
        },
        // ---- measurements ----
        "st_distance" => {
            let (Some(a), Some(b)) = (geo_arg(args, 0)?, geo_arg(args, 1)?) else {
                return Ok(Some(Value::Null));
            };
            Value::Real(planar_distance(&a.geom, &b.geom))
        }
        "st_distancesphere" => {
            let (Some(pa), Some(pb)) = (point_arg(args, 0)?, point_arg(args, 1)?) else {
                return Ok(Some(Value::Null));
            };
            let radius = real_arg(args, 2)?.unwrap_or(EARTH_RADIUS_M);
            Value::Real(haversine_m(pa.0, pa.1, pb.0, pb.1, radius))
        }
        "st_distancespheroid" => {
            let (Some(pa), Some(pb)) = (point_arg(args, 0)?, point_arg(args, 1)?) else {
                return Ok(Some(Value::Null));
            };
            // near-antipodal fallback: sphere on the mean radius
            let d = vincenty_m(pa.0, pa.1, pb.0, pb.1)
                .unwrap_or_else(|| haversine_m(pa.0, pa.1, pb.0, pb.1, EARTH_RADIUS_M));
            Value::Real(d)
        }
        "st_area" => match geo_arg(args, 0)? {
            Some(g) => Value::Real(match &g.geom {
                Geometry::Polygon(rings) => polygon_area(rings),
                _ => 0.0,
            }),
            None => Value::Null,
        },
        "st_length" => match geo_arg(args, 0)? {
            Some(g) => Value::Real(match &g.geom {
                Geometry::LineString(v) => path_length(v),
                _ => 0.0, // PG: only linear features have length
            }),
            None => Value::Null,
        },
        "st_perimeter" => match geo_arg(args, 0)? {
            Some(g) => Value::Real(match &g.geom {
                Geometry::Polygon(rings) => rings.iter().map(|r| ring_perimeter(r)).sum::<f64>(),
                _ => 0.0,
            }),
            None => Value::Null,
        },
        // ---- predicates ----
        "st_dwithin" => {
            let (Some(a), Some(b)) = (geo_arg(args, 0)?, geo_arg(args, 1)?) else {
                return Ok(Some(Value::Null));
            };
            let Some(d) = real_arg(args, 2)? else {
                return Ok(Some(Value::Null));
            };
            let hit = if args.len() >= 4 {
                // geography form: meters, sphere or spheroid
                let (pa, pb) = (point_of(&a.geom), point_of(&b.geom));
                match (pa, pb) {
                    (Some(pa), Some(pb)) => {
                        if bool_arg(args, 3) {
                            let dd = vincenty_m(pa.0, pa.1, pb.0, pb.1).unwrap_or_else(|| {
                                haversine_m(pa.0, pa.1, pb.0, pb.1, EARTH_RADIUS_M)
                            });
                            dd <= d
                        } else {
                            haversine_m(pa.0, pa.1, pb.0, pb.1, EARTH_RADIUS_M) <= d
                        }
                    }
                    _ => planar_distance(&a.geom, &b.geom) <= d,
                }
            } else {
                planar_distance(&a.geom, &b.geom) <= d
            };
            Value::Integer(i64::from(hit))
        }
        "st_contains" => {
            let (Some(a), Some(b)) = (geo_arg(args, 0)?, geo_arg(args, 1)?) else {
                return Ok(Some(Value::Null));
            };
            Value::Integer(i64::from(geo_contains(&a.geom, &b.geom)))
        }
        "st_within" => {
            let (Some(a), Some(b)) = (geo_arg(args, 0)?, geo_arg(args, 1)?) else {
                return Ok(Some(Value::Null));
            };
            Value::Integer(i64::from(geo_contains(&b.geom, &a.geom)))
        }
        "st_intersects" => {
            let (Some(a), Some(b)) = (geo_arg(args, 0)?, geo_arg(args, 1)?) else {
                return Ok(Some(Value::Null));
            };
            Value::Integer(i64::from(geo_intersects(&a.geom, &b.geom)))
        }
        // ---- shapes ----
        "st_makeenvelope" => {
            let (Some(xmin), Some(ymin), Some(xmax), Some(ymax)) = (
                real_arg(args, 0)?,
                real_arg(args, 1)?,
                real_arg(args, 2)?,
                real_arg(args, 3)?,
            ) else {
                return Ok(Some(Value::Null));
            };
            let srid = int_arg(args, 4)?.unwrap_or(0);
            Value::Text(render(&make_envelope(xmin, ymin, xmax, ymax, srid)).into())
        }
        "st_envelope" => {
            let Some(g) = geo_arg(args, 0)? else {
                return Ok(Some(Value::Null));
            };
            let (xmin, ymin, xmax, ymax) = g.geom.bbox();
            Value::Text(render(&make_envelope(xmin, ymin, xmax, ymax, g.srid)).into())
        }
        "st_expand" => {
            let Some(g) = geo_arg(args, 0)? else {
                return Ok(Some(Value::Null));
            };
            let Some(d) = real_arg(args, 1)? else {
                return Ok(Some(Value::Null));
            };
            let (xmin, ymin, xmax, ymax) = g.geom.bbox();
            Value::Text(
                render(&make_envelope(
                    xmin - d,
                    ymin - d,
                    xmax + d,
                    ymax + d,
                    g.srid,
                ))
                .into(),
            )
        }
        "st_centroid" => match geo_arg(args, 0)? {
            Some(g) => {
                let (x, y) = match &g.geom {
                    Geometry::Point(x, y) => (*x, *y),
                    Geometry::Polygon(rings) => polygon_centroid(rings),
                    Geometry::LineString(v) => {
                        let n = v.len().max(1);
                        let sx: f64 = v.iter().map(|p| p.0).sum();
                        let sy: f64 = v.iter().map(|p| p.1).sum();
                        (sx / n as f64, sy / n as f64)
                    }
                };
                Value::Text(
                    render(&Geo {
                        srid: g.srid,
                        geom: Geometry::Point(x, y),
                    })
                    .into(),
                )
            }
            None => Value::Null,
        },
        _ => return Ok(None),
    };
    Ok(Some(out))
}

fn point_of(g: &Geometry) -> Option<(f64, f64)> {
    match g {
        Geometry::Point(x, y) => Some((*x, *y)),
        _ => None,
    }
}

fn is_valid(g: &Geometry) -> bool {
    match g {
        Geometry::Point(x, y) => x.is_finite() && y.is_finite(),
        Geometry::LineString(v) => {
            v.len() >= 2 && v.iter().all(|(x, y)| x.is_finite() && y.is_finite())
        }
        Geometry::Polygon(rings) => {
            rings.iter().all(|r| {
                r.len() >= 3 && {
                    // rings should be closed (first == last) per OGC, but we
                    // accept open rings and close implicitly — validity only
                    // requires >= 3 DISTINCT vertices
                    let distinct = r
                        .windows(2)
                        .any(|w| (w[0].0 - w[1].0).abs() > EPS || (w[0].1 - w[1].1).abs() > EPS);
                    distinct && r.iter().all(|(x, y)| x.is_finite() && y.is_finite())
                }
            })
        }
    }
}

fn as_geojson(g: &Geometry) -> String {
    fn coord(x: f64, y: f64) -> String {
        format!("[{},{}]", fmt_num(x), fmt_num(y))
    }
    match g {
        Geometry::Point(x, y) => {
            format!("{{\"type\":\"Point\",\"coordinates\":{}}}", coord(*x, *y))
        }
        Geometry::LineString(v) => {
            let cs: Vec<String> = v.iter().map(|(x, y)| coord(*x, *y)).collect();
            format!(
                "{{\"type\":\"LineString\",\"coordinates\":[{}]}}",
                cs.join(",")
            )
        }
        Geometry::Polygon(rings) => {
            let rs: Vec<String> = rings
                .iter()
                .map(|r| {
                    let cs: Vec<String> = r.iter().map(|(x, y)| coord(*x, *y)).collect();
                    format!("[{}]", cs.join(","))
                })
                .collect();
            format!(
                "{{\"type\":\"Polygon\",\"coordinates\":[{}]}}",
                rs.join(",")
            )
        }
    }
}

// ============================================================================
// Unit tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn pt(x: f64, y: f64) -> String {
        render(&Geo {
            srid: 0,
            geom: Geometry::Point(x, y),
        })
    }

    #[test]
    fn parse_and_roundtrip() {
        for s in [
            "POINT(1 2)",
            "point( 1.5   -2.5 )",
            "LINESTRING(0 0, 1 1, 2 0)",
            "POLYGON((0 0, 4 0, 4 4, 0 4, 0 0))",
            "POLYGON((0 0, 4 0, 4 4, 0 4, 0 0), (1 1, 2 1, 2 2, 1 2, 1 1))",
            "SRID=4326;POINT(-73.98 40.77)",
        ] {
            let g = parse_geometry(s).unwrap();
            let r = render(&g);
            let g2 = parse_geometry(&r).unwrap();
            assert_eq!(g, g2, "round trip failed for {s} -> {r}");
        }
        let g = parse_geometry("SRID=4326;POINT(-73.98 40.77)").unwrap();
        assert_eq!(g.srid, 4326);
        assert!(render(&g).starts_with("SRID=4326;POINT("));
        // case-insensitive tags
        assert_eq!(
            parse_geometry("PoInT( 3 4 )").unwrap(),
            parse_geometry("POINT(3 4)").unwrap()
        );
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(parse_geometry("").is_err());
        assert!(parse_geometry("POINT(1)").is_err());
        assert!(parse_geometry("POINT(1 2 3)").is_err()); // Z rejected
        assert!(parse_geometry("TRIANGLE((0 0, 1 0, 0 1))").is_err());
        assert!(parse_geometry("POLYGON((0 0, 1 0))").is_err()); // < 3 pts
        assert!(parse_geometry("SRID=4326POINT(1 2)").is_err());
        assert!(parse_geometry("POINT(1 2) junk").is_err());
    }

    #[test]
    fn planar_point_distance() {
        let a = parse_geometry(&pt(0.0, 0.0)).unwrap();
        let b = parse_geometry(&pt(3.0, 4.0)).unwrap();
        assert!((planar_distance(&a.geom, &b.geom) - 5.0).abs() < 1e-9);
    }

    #[test]
    fn point_to_polygon_distance() {
        // unit square
        let sq = parse_geometry("POLYGON((0 0, 1 0, 1 1, 0 1, 0 0))").unwrap();
        let inside = parse_geometry(&pt(0.5, 0.5)).unwrap();
        let outside = parse_geometry(&pt(3.0, 0.5)).unwrap();
        let on_edge = parse_geometry(&pt(1.0, 0.5)).unwrap();
        assert!(planar_distance(&inside.geom, &sq.geom).abs() < 1e-12);
        assert!((planar_distance(&outside.geom, &sq.geom) - 2.0).abs() < 1e-12);
        assert!(planar_distance(&on_edge.geom, &sq.geom).abs() < 1e-12);
    }

    #[test]
    fn contains_and_intersects() {
        let sq = parse_geometry("POLYGON((0 0, 10 0, 10 10, 0 10, 0 0))").unwrap();
        let inner = parse_geometry(&pt(5.0, 5.0)).unwrap();
        let edge = parse_geometry(&pt(0.0, 5.0)).unwrap();
        let outer = parse_geometry(&pt(-1.0, 5.0)).unwrap();
        assert!(geo_contains(&sq.geom, &inner.geom));
        assert!(!geo_contains(&sq.geom, &edge.geom)); // boundary: not contains
        assert!(geo_intersects(&sq.geom, &edge.geom)); // ...but intersects
        assert!(!geo_intersects(&sq.geom, &outer.geom));
        // hole semantics
        let donut =
            parse_geometry("POLYGON((0 0, 10 0, 10 10, 0 10, 0 0), (4 4, 6 4, 6 6, 4 6, 4 4))")
                .unwrap();
        let in_hole = parse_geometry(&pt(5.0, 5.0)).unwrap();
        let in_ring = parse_geometry(&pt(2.0, 2.0)).unwrap();
        assert!(!geo_contains(&donut.geom, &in_hole.geom));
        assert!(geo_contains(&donut.geom, &in_ring.geom));
    }

    #[test]
    fn linestring_distance_and_length() {
        let line = parse_geometry("LINESTRING(0 0, 0 10)").unwrap();
        let p = parse_geometry(&pt(3.0, 5.0)).unwrap();
        assert!((planar_distance(&line.geom, &p.geom) - 3.0).abs() < 1e-12);
        // linestring length is the OPEN path (no closing edge): 3-4-5 leg
        assert!((path_length(&[(0.0, 0.0), (3.0, 4.0)]) - 5.0).abs() < 1e-12);
        assert!((path_length(&[(0.0, 0.0), (3.0, 4.0), (3.0, 10.0)]) - 11.0).abs() < 1e-12);
        // ...while a 2-point RING closes back: 5 + 5
        assert!((ring_perimeter(&[(0.0, 0.0), (3.0, 4.0)]) - 10.0).abs() < 1e-12);
    }

    #[test]
    fn area_and_perimeter() {
        let sq = parse_geometry("POLYGON((0 0, 4 0, 4 4, 0 4, 0 0))").unwrap();
        let Geometry::Polygon(rings) = &sq.geom else {
            unreachable!()
        };
        assert!((polygon_area(rings) - 16.0).abs() < 1e-9);
        assert!((rings.iter().map(|r| ring_perimeter(r)).sum::<f64>() - 16.0).abs() < 1e-9);
        // with a 2x2 hole: 16 - 4
        let donut = parse_geometry("POLYGON((0 0, 4 0, 4 4, 0 4, 0 0), (1 1, 3 1, 3 3, 1 3, 1 1))")
            .unwrap();
        let Geometry::Polygon(r2) = &donut.geom else {
            unreachable!()
        };
        assert!((polygon_area(r2) - 12.0).abs() < 1e-9);
    }

    #[test]
    fn centroid_unit_square() {
        let sq = parse_geometry("POLYGON((0 0, 2 0, 2 2, 0 2, 0 0))").unwrap();
        let Geometry::Polygon(rings) = &sq.geom else {
            unreachable!()
        };
        let (cx, cy) = polygon_centroid(rings);
        assert!((cx - 1.0).abs() < 1e-9 && (cy - 1.0).abs() < 1e-9);
    }

    #[test]
    fn haversine_known_distance() {
        // JFK (-73.7781, 40.6413) to LHR (-0.1276, 51.5053) ≈ 5540-5570 km
        // depending on radius; on the IUGG mean sphere ~5554 km.
        let d = haversine_m(-73.7781, 40.6413, -0.1276, 51.5053, EARTH_RADIUS_M);
        assert!((d / 1000.0 - 5554.0).abs() < 30.0, "got {d} m");
        // zero distance
        assert!(haversine_m(1.0, 2.0, 1.0, 2.0, EARTH_RADIUS_M).abs() < 1e-9);
    }

    #[test]
    fn vincenty_known_distance() {
        // The classic Vincenty test vector: Flinders Peak to Buninyong
        // (Australia), geodesic inverse = 54,972.271 m.
        // Flinders Peak: (144.42486789, -37.95103342)
        // Buninyong:     (143.92649553, -37.65282114)
        // (Buninyong's longitude is 143.9264955 — NOT 143.926355, which is
        // a mis-transcription that circulates and lands ~10 m off.)
        let d = vincenty_m(144.42486789, -37.95103342, 143.92649553, -37.65282114);
        let d = d.expect("vincenty should converge for the classic vector");
        assert!(
            (d - 54972.271).abs() < 0.5,
            "got {d} m, expected ~54972.271 m"
        );
        // mirror symmetry: negating BOTH longitudes must not move the
        // distance (the ellipsoid is symmetric across the meridian plane)
        let dm = vincenty_m(-144.42486789, -37.95103342, -143.92649553, -37.65282114)
            .expect("mirror case converges");
        assert!((dm - d).abs() < 1e-6, "mirror {dm} vs {d}");
        // coincident points
        assert_eq!(vincenty_m(1.0, 2.0, 1.0, 2.0), Some(0.0));
    }

    #[test]
    fn distance_op_null_and_error() {
        assert_eq!(
            eval_distance_op(&Value::Null, &Value::Text("POINT(1 2)".into())).unwrap(),
            Value::Null
        );
        assert!(eval_distance_op(
            &Value::Text("garbage".into()),
            &Value::Text("POINT(1 2)".into())
        )
        .is_err());
    }

    #[test]
    fn geo_dispatch_shapes() {
        // ST_Point + ST_X/ST_Y
        let p = call_geo_function("st_point", &[Value::Real(1.5), Value::Real(-2.5)])
            .unwrap()
            .unwrap();
        assert_eq!(p, Value::Text("POINT(1.5 -2.5)".into()));
        let x = call_geo_function("st_x", std::slice::from_ref(&p))
            .unwrap()
            .unwrap();
        assert_eq!(x, Value::Real(1.5));
        // unknown name → None
        assert!(call_geo_function("nope", &[]).unwrap().is_none());
        // ST_DWithin planar
        let a = Value::Text(pt(0.0, 0.0).into());
        let b = Value::Text(pt(3.0, 4.0).into());
        let hit = call_geo_function("st_dwithin", &[a.clone(), b.clone(), Value::Real(5.0)])
            .unwrap()
            .unwrap();
        assert_eq!(hit, Value::Integer(1));
        let miss = call_geo_function("st_dwithin", &[a, b, Value::Real(4.99)])
            .unwrap()
            .unwrap();
        assert_eq!(miss, Value::Integer(0));
    }

    #[test]
    fn st_distance_sphere_dispatch() {
        let jfk = Value::Text("SRID=4326;POINT(-73.7781 40.6413)".into());
        let lhr = Value::Text("SRID=4326;POINT(-0.1276 51.5053)".into());
        let d = call_geo_function("st_distancesphere", &[jfk.clone(), lhr.clone()])
            .unwrap()
            .unwrap();
        match d {
            Value::Real(m) => assert!((m / 1000.0 - 5554.0).abs() < 30.0, "{m}"),
            other => panic!("expected real, got {other:?}"),
        }
        let ds = call_geo_function("st_distancespheroid", &[jfk, lhr])
            .unwrap()
            .unwrap();
        match ds {
            Value::Real(m) => assert!(
                (m / 1000.0 - 5567.0).abs() < 30.0,
                "spheroid {m} should be ~5567 km"
            ),
            other => panic!("expected real, got {other:?}"),
        }
    }

    #[test]
    fn envelope_expand_make() {
        let env = call_geo_function(
            "st_makeenvelope",
            &[
                Value::Real(0.0),
                Value::Real(0.0),
                Value::Real(2.0),
                Value::Real(2.0),
            ],
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            env,
            Value::Text("POLYGON((0 0, 2 0, 2 2, 0 2, 0 0))".into())
        );
        let exp = call_geo_function("st_expand", &[env.clone(), Value::Real(1.0)])
            .unwrap()
            .unwrap();
        match exp {
            Value::Text(s) => {
                let g = parse_geometry(&s).unwrap();
                let Geometry::Polygon(rings) = g.geom else {
                    unreachable!()
                };
                assert!((polygon_area(&rings) - 16.0).abs() < 1e-9); // 4x4 now
            }
            other => panic!("{other:?}"),
        }
        // st_envelope of a polygon = its bbox
        let tri = Value::Text("POLYGON((1 1, 5 1, 3 4, 1 1))".into());
        let e = call_geo_function("st_envelope", &[tri]).unwrap().unwrap();
        match e {
            Value::Text(s) => {
                let g = parse_geometry(&s).unwrap();
                let (xmin, ymin, xmax, ymax) = g.geom.bbox();
                assert!((xmin - 1.0).abs() < 1e-9 && (xmax - 5.0).abs() < 1e-9);
                assert!((ymin - 1.0).abs() < 1e-9 && (ymax - 4.0).abs() < 1e-9);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn asgeojson_shape() {
        let g = parse_geometry("POINT(1 2)").unwrap();
        assert_eq!(
            as_geojson(&g.geom),
            "{\"type\":\"Point\",\"coordinates\":[1,2]}"
        );
        // fractional values keep their decimals; integral ones trim
        let f = parse_geometry("POINT(1.5 2)").unwrap();
        assert_eq!(
            as_geojson(&f.geom),
            "{\"type\":\"Point\",\"coordinates\":[1.5,2]}"
        );
        assert_eq!(render(&f), "POINT(1.5 2)");
        assert_eq!(render(&parse_geometry("POINT(3 4)").unwrap()), "POINT(3 4)");
    }

    #[test]
    fn knn_ordering_shape() {
        // the classic ORDER BY geom <-> point nearest-neighbor pattern
        let origin = Value::Text(pt(0.0, 0.0).into());
        let mut pts: Vec<(String, f64)> = (1..=5)
            .map(|i| {
                (
                    pt(i as f64, i as f64),
                    (i as f64) * std::f64::consts::SQRT_2,
                )
            })
            .collect();
        pts.sort_by(|a, b| {
            let da = eval_distance_op(&Value::Text(a.0.clone().into()), &origin).unwrap();
            let db = eval_distance_op(&Value::Text(b.0.clone().into()), &origin).unwrap();
            da.as_real().partial_cmp(&db.as_real()).unwrap()
        });
        // nearest-first ordering
        assert!(pts.windows(2).all(|w| w[0].1 <= w[1].1));
    }
}
