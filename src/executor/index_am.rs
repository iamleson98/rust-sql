//! Specialized index access methods (the PostgreSQL `USING` clause,
//! borrowed): a GIN-style **inverted index** for full-text search and a
//! GiST-borrowed **spatial grid index** for geometry.
//!
//! Both kinds reuse the engine's single B+tree AM as their physical
//! store — the "AM" is the KEY DISCIPLINE, not a new page format:
//!
//! * **Inverted** (`USING gin(to_tsvector('english', body))` or
//!   `USING gin(tsv)`): one B+tree entry **per tsvector lexeme**, key =
//!   `Text(lexeme)` order key, value = rowid. A term's postings live in
//!   one contiguous run, so `lookup_index` collects them with a single
//!   seek. Query-time candidate generation evaluates the tsquery's
//!   boolean structure over the postings (AND = intersect, OR = union,
//!   NOT = "unknown" sentinel) and yields a rowid **superset**; the
//!   original `@@` conjunct stays in the residual Filter, so soundness
//!   only requires no false negatives — never exactness.
//!
//! * **Spatial** (`USING gist(geom [, resolution])`): one B+tree entry
//!   **per grid cell covered by the geometry's bounding box**, key =
//!   `(level, cell_x, cell_y)` compound integer key, value = rowid.
//!   Level 0 cells are `resolution` wide; a geometry whose bbox covers
//!   more than [`SPATIAL_MAX_CELLS`] level-0 cells is indexed at a
//!   coarser power-of-two level instead (a huge polygon occupies few
//!   big cells; a point occupies exactly one level-0 cell), keeping
//!   entry counts bounded without losing the covering property. A
//!   query window scans every level's rectangle as ONE lexicographic
//!   range — the compound key `(level, cx, cy)` makes each level's
//!   plane contiguous — and the window rectangle is expanded around the
//!   query point until the k-th best true distance proves no geometry
//!   outside the window can beat it (PostGIS's KNN `<->` contract).
//!
//! Soundness invariants (both kinds):
//! 1. Every row whose value matches a query is GUARANTEED to be in the
//!    candidate set (no false negatives) — the residual filter refines.
//! 2. Write-path maintenance is centralized in
//!    [`crate::executor::insert_index_entry`] /
//!    [`crate::executor::delete_index_entry`], which loop over
//!    [`index_entry_keys`] — one entry for btree indexes, many for the
//!    specialized kinds.

use std::collections::HashSet;

use crate::error::{Error, Result};
use crate::executor::fts::{parse_tsquery, parse_tsvector, TsConfig, TsQuery};
use crate::executor::geo::{parse_geometry, planar_distance};
use crate::schema::{Index, IndexKind, Table};
use crate::storage::btree::Btree;
use crate::types::Value;

/// Maximum number of cells one geometry's bbox may occupy at its chosen
/// level. Beyond this the geometry escalates to a coarser level.
pub const SPATIAL_MAX_CELLS: u64 = 256;
/// Coarsest grid level (cells of `resolution * 2^MAX_LEVEL` edge).
pub const SPATIAL_MAX_LEVEL: i64 = 16;
/// KNN expansion rounds before the completion sweep. Round r's window
/// half-extent is `resolution * 2^r`; round 40 at resolution 0.01 spans
/// ~1.1e10 coordinate units — past any real dataset, and the sweep
/// after it guarantees termination regardless.
pub const KNN_MAX_ROUNDS: u32 = 40;

// ============================================================================
// Entry keys (write path)
// ============================================================================

/// The B+tree keys one row contributes to an index — one for ordinary
/// btree indexes, one PER LEXEME (inverted) or PER COVERED CELL
/// (spatial). Empty for NULL values (no entries, like SQLite's
/// NULL-index exemption). Errors on unparseable tsvector/geometry text
/// in an inverted/spatial-indexed value — PostgreSQL's GIN raises on
/// invalid input too, and silently skipping would make the row
/// invisible to index scans (a false negative).
pub fn index_entry_keys(index: &Index, table: &Table, row: &[Value]) -> Result<Vec<Vec<u8>>> {
    match &index.kind {
        IndexKind::Btree => {
            // The ordinary single-key path (collation folding included).
            Ok(vec![crate::executor::encode_index_key(index, table, row)])
        }
        IndexKind::Inverted => {
            let v = indexed_value(index, table, row)?;
            if v.is_null() {
                return Ok(Vec::new());
            }
            let text = v.as_text();
            let vec = parse_tsvector(&text).map_err(|e| {
                Error::constraint(format!("index {}: invalid tsvector value: {e}", index.name))
            })?;
            Ok(vec
                .lexemes
                .keys()
                .map(|lex| Value::Text(lex.as_str().into()).encode_order_key())
                .collect())
        }
        IndexKind::Spatial { resolution } => {
            let v = indexed_value(index, table, row)?;
            if v.is_null() {
                return Ok(Vec::new());
            }
            let text = v.as_text();
            let g = parse_geometry(&text).map_err(|e| {
                Error::constraint(format!("index {}: invalid geometry value: {e}", index.name))
            })?;
            let (xmin, ymin, xmax, ymax) = g.geom.bbox();
            if !(xmin.is_finite() && ymin.is_finite() && xmax.is_finite() && ymax.is_finite()) {
                return Err(Error::constraint(format!(
                    "index {}: geometry has non-finite coordinates",
                    index.name
                )));
            }
            Ok(cells_for_bbox(xmin, ymin, xmax, ymax, *resolution)
                .into_iter()
                .map(|(level, cx, cy)| encode_spatial_key(level, cx, cy))
                .collect())
        }
    }
}

/// Evaluate an index's single indexed expression/column against a row
/// (the specialized kinds are single-column by construction). Follows
/// `encode_index_key`'s convention: index expressions are evaluated
/// with no parameter bindings (SQLite index expressions are
/// parameter-free by construction).
fn indexed_value(index: &Index, table: &Table, row: &[Value]) -> Result<Value> {
    let col = index
        .columns
        .first()
        .ok_or_else(|| Error::corruption(format!("index {}: no columns", index.name)))?;
    if let Some(e) = &col.expr {
        let named = std::collections::HashMap::new();
        return crate::executor::eval_row(e, row, &table.col_names, &[], &named);
    }
    Ok(table
        .find_column(&col.name)
        .and_then(|p| row.get(p).cloned())
        .unwrap_or(Value::Null))
}

// ============================================================================
// Spatial cell math
// ============================================================================

/// floor division that behaves for negatives (cell indices of negative
/// coordinates must round DOWN: -0.01 / 0.01 = -1, not 0).
fn floor_div(x: f64, s: f64) -> i64 {
    (x / s).floor() as i64
}

/// The cells a bbox covers at its chosen level: the smallest level j
/// (finest grid) whose covered-cell count fits
/// [`SPATIAL_MAX_CELLS`]. Points always land at level 0 (one cell).
/// Returns `(level, cell_x, cell_y)` triples.
pub fn cells_for_bbox(
    xmin: f64,
    ymin: f64,
    xmax: f64,
    ymax: f64,
    res: f64,
) -> Vec<(i64, i64, i64)> {
    let res = if res.is_finite() && res > 0.0 {
        res
    } else {
        0.01
    };
    for level in 0..=SPATIAL_MAX_LEVEL {
        let s = res * ((1u64 << level) as f64);
        let cx0 = floor_div(xmin, s);
        let cx1 = floor_div(xmax, s);
        let cy0 = floor_div(ymin, s);
        let cy1 = floor_div(ymax, s);
        let nx = (cx1 - cx0 + 1).max(0) as u64;
        let ny = (cy1 - cy0 + 1).max(0) as u64;
        if nx.saturating_mul(ny) <= SPATIAL_MAX_CELLS {
            let mut out = Vec::with_capacity((nx * ny) as usize);
            for cx in cx0..=cx1 {
                for cy in cy0..=cy1 {
                    out.push((level, cx, cy));
                }
            }
            return out;
        }
    }
    // Unreachable for finite bboxes (level MAX_LEVEL cells are
    // res * 2^16 wide; any finite bbox fits in a 2x2 there) — but keep
    // the fallback total: clamp to the bbox's min-corner cell at the
    // coarsest level rather than emitting nothing (which would be a
    // false negative).
    let s = res * ((1u64 << SPATIAL_MAX_LEVEL) as f64);
    vec![(SPATIAL_MAX_LEVEL, floor_div(xmin, s), floor_div(ymin, s))]
}

/// Compound order key for one spatial entry: `(level, cx, cy)` —
/// three INTEGER order keys concatenated, so each level's plane is one
/// contiguous lexicographic range and a cell rectangle
/// `[cx0..cx1] x [cy0..cy1]` at one level is the single range
/// `[key(level,cx0,cy0) .. key(level,cx1,cy1)]` (a SUPERSET of the
/// rectangle — middle cx columns include out-of-window cy entries,
/// filtered by the caller before any row fetch).
pub fn encode_spatial_key(level: i64, cx: i64, cy: i64) -> Vec<u8> {
    let mut out = Vec::with_capacity(30);
    Value::Integer(level).encode_order_key_into(&mut out);
    Value::Integer(cx).encode_order_key_into(&mut out);
    Value::Integer(cy).encode_order_key_into(&mut out);
    out
}

/// Decode `(level, cx, cy)` from a stored spatial index cell key.
/// Returns None when the key is not a 3-integer compound key.
fn decode_spatial_key(key: &[u8]) -> Option<(i64, i64, i64)> {
    let mut vals = [0i64; 3];
    let mut pos = 0usize;
    for slot in vals.iter_mut() {
        if pos >= key.len() || key[pos] != 0x01 {
            return None;
        }
        // INTEGER order key: [0x01] + 8-byte big-endian order key
        // (double_order_key's sign-flip trick, inverted here). Our
        // levels/cells are small integers, so the double round-trip is
        // exact.
        if pos + 9 > key.len() {
            return None;
        }
        let bytes: [u8; 8] = key[pos + 1..pos + 9].try_into().ok()?;
        let ok = u64::from_be_bytes(bytes);
        let fbits = if ok >> 63 == 1 {
            ok & 0x7FFF_FFFF_FFFF_FFFF
        } else {
            !ok
        };
        let f = f64::from_bits(fbits);
        if !f.is_finite() || f < i64::MIN as f64 || f >= i64::MAX as f64 {
            return None;
        }
        *slot = f as i64;
        pos += 9;
    }
    Some((vals[0], vals[1], vals[2]))
}

/// Collect the rowids of all entries whose covered cell lies inside the
/// rectangle `[x0..x1] x [y0..y1]` (coordinate units), across ALL grid
/// levels. One range scan per level; the per-cell y-range check happens
/// on the decoded key (the lexicographic range is a band superset).
/// Duplicates (a geometry covering cells at only one level never
/// repeats, but expansion rounds rescan) are the caller's concern —
/// `out` may already hold rowids; this appends.
pub fn window_rowids(
    bt: &mut Btree,
    resolution: f64,
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
    out: &mut Vec<i64>,
) -> Result<()> {
    let res = if resolution.is_finite() && resolution > 0.0 {
        resolution
    } else {
        0.01
    };
    for level in 0..=SPATIAL_MAX_LEVEL {
        let s = res * ((1u64 << level) as f64);
        let cx0 = floor_div(x0, s);
        let cx1 = floor_div(x1, s);
        let cy0 = floor_div(y0, s);
        let cy1 = floor_div(y1, s);
        let start = encode_spatial_key(level, cx0, cy0);
        let end = encode_spatial_key(level, cx1, cy1);
        bt.scan_index_from(&start, |rowid, cell_key| {
            // Stop once past this level's band (keys past `end` belong to
            // coarser levels / far cells).
            if cell_key > end.as_slice() {
                return false;
            }
            if let Some((_, cx, cy)) = decode_spatial_key(cell_key) {
                if cx >= cx0 && cx <= cx1 && cy >= cy0 && cy <= cy1 {
                    out.push(rowid);
                }
            }
            true
        })?;
    }
    Ok(())
}

/// Euclidean distance from point `(px, py)` to the rectangle's nearest
/// point (0 when inside) — the KNN expansion's lower bound: every
/// geometry disjoint from the rectangle is at least this far.
pub fn point_rect_distance(px: f64, py: f64, x0: f64, y0: f64, x1: f64, y1: f64) -> f64 {
    let dx = if px < x0 {
        x0 - px
    } else if px > x1 {
        px - x1
    } else {
        0.0
    };
    let dy = if py < y0 {
        y0 - py
    } else if py > y1 {
        py - y1
    } else {
        0.0
    };
    (dx * dx + dy * dy).sqrt()
}

// ============================================================================
// Inverted-index query algebra (FTS candidate generation)
// ============================================================================

/// A candidate rowid set with an "everything / unknown" sentinel.
/// `All` propagates through `Not` and `Or` — when it survives to the
/// top, the caller falls back to a full table scan (some satisfying
/// rows might contain no positive lexeme, e.g. `!a | b`).
#[derive(Debug)]
enum CandSet {
    All,
    Rows(Vec<i64>),
}

/// The postings of one exact term: entries whose key is exactly
/// `Text(term)` (the encoded key's trailing 0x00 terminator makes
/// `lookup_index`'s prefix test exact for NUL-free lexemes).
fn postings_exact(bt: &mut Btree, term: &str, out: &mut Vec<i64>) -> Result<()> {
    let key = Value::Text(term.into()).encode_order_key();
    bt.lookup_index_into(&key, out)?;
    Ok(())
}

/// The postings of a PREFIX term (`run:*`): every entry whose term
/// starts with `run`. `scan_index_from` positions at the first key >=
/// the prefix (terminator-less probe) and iterates ascending; the scan
/// stops at the first term outside the prefix family. The collected
/// rowids are TERM-major (btree order), so they are sorted + deduped
/// here — a document holding two prefix-family lexemes ('postgres',
/// 'postgresql') contributes two entries, and the set-algebra
/// intersection/union below requires ascending, duplicate-free inputs.
fn postings_prefix(bt: &mut Btree, prefix: &str, out: &mut Vec<i64>) -> Result<()> {
    let mut probe = Vec::with_capacity(prefix.len() + 1);
    probe.push(0x02);
    probe.extend_from_slice(prefix.as_bytes());
    let pfx = prefix.as_bytes();
    bt.scan_index_from(&probe, |rowid, cell_key| {
        // Stored keys are [0x02][term bytes][0x00]; the term bytes start
        // at offset 1. Ascending order guarantees the first key that
        // doesn't start with the prefix ends the family.
        if cell_key.first() != Some(&0x02) {
            return false;
        }
        let term = &cell_key[1..];
        term.starts_with(pfx).then(|| out.push(rowid)).is_some()
    })?;
    out.sort_unstable();
    out.dedup();
    Ok(())
}

/// Evaluate a tsquery's boolean structure over the inverted index,
/// producing a rowid SUPERSET of the matching rows:
/// * `Lex` → its postings (exact or prefix)
/// * `And` → intersection (both terms must be present)
/// * `Or` → union
/// * `Not(x)` → `All` (unknown — the residual refines)
/// * `Phrase` → intersection (every phrase term must be present)
///
/// `All` collapses `Or` to `All` and passes through `And`, so a plan is
/// only produced when positive lexemes dominate the formula — exactly
/// PostgreSQL's "GIN can't evaluate negation" restriction.
fn eval_candidates(bt: &mut Btree, q: &TsQuery, scratch: &mut Vec<i64>) -> Result<CandSet> {
    match q {
        TsQuery::Not(_) => Ok(CandSet::All),
        TsQuery::Lex { word, prefix } => {
            scratch.clear();
            if *prefix {
                postings_prefix(bt, word, scratch)?;
            } else {
                postings_exact(bt, word, scratch)?;
            }
            Ok(CandSet::Rows(std::mem::take(scratch)))
        }
        TsQuery::And(a, b) => {
            let (ra, rb) = (
                eval_candidates(bt, a, scratch)?,
                eval_candidates(bt, b, scratch)?,
            );
            Ok(match (ra, rb) {
                (CandSet::All, x) | (x, CandSet::All) => x,
                (CandSet::Rows(mut l), CandSet::Rows(r)) => {
                    intersect_sorted(&mut l, &r);
                    CandSet::Rows(l)
                }
            })
        }
        TsQuery::Or(a, b) => {
            let (ra, rb) = (
                eval_candidates(bt, a, scratch)?,
                eval_candidates(bt, b, scratch)?,
            );
            Ok(match (ra, rb) {
                (CandSet::All, _) | (_, CandSet::All) => CandSet::All,
                (CandSet::Rows(mut l), CandSet::Rows(r)) => {
                    merge_sorted_dedup(&mut l, &r);
                    CandSet::Rows(l)
                }
            })
        }
        TsQuery::Phrase(elems) => {
            // Every phrase element must be present (positions are the
            // residual's job) — intersection, with All-propagation.
            let mut acc = CandSet::All;
            for e in elems {
                let re = eval_candidates(bt, e, scratch)?;
                acc = match (acc, re) {
                    (CandSet::All, x) | (x, CandSet::All) => x,
                    (CandSet::Rows(mut l), CandSet::Rows(r)) => {
                        intersect_sorted(&mut l, &r);
                        CandSet::Rows(l)
                    }
                };
            }
            Ok(acc)
        }
    }
}

/// In-place sorted intersection (`l` becomes `l ∩ r`).
fn intersect_sorted(l: &mut Vec<i64>, r: &[i64]) {
    let mut i = 0usize;
    let mut j = 0usize;
    let mut w = 0usize;
    while i < l.len() && j < r.len() {
        match l[i].cmp(&r[j]) {
            std::cmp::Ordering::Equal => {
                l[w] = l[i];
                w += 1;
                i += 1;
                j += 1;
            }
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
        }
    }
    l.truncate(w);
}

/// In-place sorted union with dedup (`l` becomes `l ∪ r`, deduped).
fn merge_sorted_dedup(l: &mut Vec<i64>, r: &[i64]) {
    let mut merged = Vec::with_capacity(l.len() + r.len());
    let (mut i, mut j) = (0usize, 0usize);
    while i < l.len() || j < r.len() {
        let take_l = j >= r.len() || (i < l.len() && l[i] <= r[j]);
        let v = if take_l {
            let v = l[i];
            i += 1;
            v
        } else {
            let v = r[j];
            j += 1;
            v
        };
        if merged.last() != Some(&v) {
            merged.push(v);
        }
    }
    *l = merged;
}

/// Candidate rowids for `tsvector @@ tsquery` via the inverted index:
/// `None` = the query's positive lexemes can't bound the result
/// (fall back to a full scan); `Some(rows)` = a deduped, sorted
/// SUPERSET of the matching rowids (the residual `@@` refines).
pub fn inverted_candidates(bt: &mut Btree, tsquery_text: &str) -> Result<Option<Vec<i64>>> {
    // The query text is the RENDERED form (already stemmed by
    // to_tsquery et al.) — parse with the Simple dictionary so stored
    // lexemes are not re-stemmed (same rule as eval_match_op).
    let q = parse_tsquery(TsConfig::Simple, tsquery_text)
        .map_err(|e| Error::runtime(format!("invalid tsquery: {e}")))?;
    let mut scratch = Vec::new();
    match eval_candidates(bt, &q, &mut scratch)? {
        CandSet::All => Ok(None),
        CandSet::Rows(rows) => Ok(Some(rows)),
    }
}

// ============================================================================
// KNN driver
// ============================================================================

/// Plan the next KNN window: a square of half-extent
/// `resolution * 2^round` cells centered on the query point
/// (`round` 0 = the point's own level-0 cell). Geometric growth keeps
/// total scanned area proportional to the final window.
pub fn knn_window(resolution: f64, round: u32, px: f64, py: f64) -> (f64, f64, f64, f64) {
    let res = if resolution.is_finite() && resolution > 0.0 {
        resolution
    } else {
        0.01
    };
    let half = res * ((1u64 << round.min(63) as u64) as f64);
    (px - half, py - half, px + half, py + half)
}

/// Collect the rowids inside `rect` that were not seen in any earlier
/// round (`seen` is updated).
pub fn knn_collect_new(
    bt: &mut Btree,
    resolution: f64,
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
    seen: &mut HashSet<i64>,
) -> Result<Vec<i64>> {
    let mut found = Vec::new();
    window_rowids(bt, resolution, x0, y0, x1, y1, &mut found)?;
    let mut fresh = Vec::with_capacity(found.len());
    for rid in found {
        if seen.insert(rid) {
            fresh.push(rid);
        }
    }
    Ok(fresh)
}

/// True distance between a stored geometry text and the query point
/// geometry — EXACTLY the `<->` operator's metric (`planar_distance`),
/// so index ordering and residual ordering can never disagree.
pub fn knn_true_distance(geom_text: &str, point_text: &str) -> Result<f64> {
    let a = parse_geometry(geom_text).map_err(|e| Error::runtime(format!("{e} (KNN geometry)")))?;
    let b = parse_geometry(point_text).map_err(|e| Error::runtime(format!("{e} (KNN point)")))?;
    Ok(planar_distance(&a.geom, &b.geom))
}

// ============================================================================
// Unit tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floor_div_negatives() {
        assert_eq!(floor_div(-0.01, 0.01), -1);
        assert_eq!(floor_div(-0.001, 0.01), -1);
        assert_eq!(floor_div(0.0, 0.01), 0);
        assert_eq!(floor_div(0.01, 0.01), 1);
        assert_eq!(floor_div(-1.0, 0.5), -2);
    }

    #[test]
    fn cells_point_single_level0() {
        let cells = cells_for_bbox(1.0, 2.0, 1.0, 2.0, 0.01);
        assert_eq!(cells, vec![(0, 100, 200)]);
    }

    #[test]
    fn cells_small_bbox_level0() {
        // 0.03 x 0.01 at res 0.01 -> 4 x 2 = 8 cells at level 0
        let cells = cells_for_bbox(0.0, 0.0, 0.03, 0.01, 0.01);
        assert_eq!(cells.len(), 8);
        assert!(cells.iter().all(|&(l, _, _)| l == 0));
        assert!(cells.contains(&(0, 0, 0)));
        assert!(cells.contains(&(0, 3, 1)));
    }

    #[test]
    fn cells_negative_coords() {
        // (-0.02..0.01) x (0..0) -> 4 x 1 cells, x in {-2,-1,0,1}
        let cells = cells_for_bbox(-0.02, 0.0, 0.01, 0.0, 0.01);
        assert_eq!(cells.len(), 4);
        assert!(cells.contains(&(0, -2, 0)));
        assert!(cells.contains(&(0, -1, 0)));
        assert!(cells.contains(&(0, 0, 0)));
        assert!(cells.contains(&(0, 1, 0)));
    }

    #[test]
    fn cells_huge_bbox_escalates_level() {
        // 100 x 100 degrees at res 0.01 = 10001x10001 cells at level 0.
        // Level j cells are 0.01*2^j wide; need <= 256 total:
        // 0.01*2^j * 16 >= 100 -> 2^j >= 625 -> j = 10 (1024):
        // 100 / 10.24 = 10x10 = 100 cells.
        let cells = cells_for_bbox(0.0, 0.0, 100.0, 100.0, 0.01);
        assert!(cells.len() <= SPATIAL_MAX_CELLS as usize);
        assert!(cells.len() >= 64);
        assert!(cells.iter().all(|&(l, _, _)| l >= 9));
    }

    #[test]
    fn cells_whole_world_fits() {
        // lon/lat whole-earth bbox must terminate at some level <= MAX.
        let cells = cells_for_bbox(-180.0, -90.0, 180.0, 90.0, 0.01);
        assert!(cells.len() <= SPATIAL_MAX_CELLS as usize);
        assert!(!cells.is_empty());
    }

    #[test]
    fn spatial_key_roundtrip() {
        for &(l, cx, cy) in &[
            (0i64, 0i64, 0i64),
            (3, -5, 17),
            (16, i32::MIN as i64, i32::MAX as i64),
        ] {
            let key = encode_spatial_key(l, cx, cy);
            assert_eq!(decode_spatial_key(&key), Some((l, cx, cy)));
        }
        // A text key is not a spatial key.
        assert_eq!(
            decode_spatial_key(&Value::Text("x".into()).encode_order_key()),
            None
        );
    }

    #[test]
    fn point_rect_distance_cases() {
        let d = point_rect_distance(0.0, 0.0, 1.0, 1.0, 2.0, 2.0);
        assert!((d - std::f64::consts::SQRT_2).abs() < 1e-12);
        assert_eq!(point_rect_distance(1.5, 1.5, 1.0, 1.0, 2.0, 2.0), 0.0);
        assert_eq!(point_rect_distance(0.0, 1.5, 1.0, 1.0, 2.0, 2.0), 1.0);
    }

    #[test]
    fn intersect_and_merge() {
        let mut l = vec![1, 3, 5, 7];
        intersect_sorted(&mut l, &[3, 4, 5, 6]);
        assert_eq!(l, vec![3, 5]);
        let mut u = vec![1, 3, 5];
        merge_sorted_dedup(&mut u, &[3, 4, 5, 9]);
        assert_eq!(u, vec![1, 3, 4, 5, 9]);
        // dedup of identical single-term unions
        let mut v = vec![2, 2];
        merge_sorted_dedup(&mut v, &[2]);
        assert_eq!(v, vec![2]);
    }

    #[test]
    fn knn_window_growth() {
        let (x0, _, x1, _) = knn_window(0.01, 0, 5.0, 5.0);
        assert!((x1 - x0 - 0.02).abs() < 1e-12);
        let (x0, _, x1, _) = knn_window(0.01, 3, 5.0, 5.0);
        assert!((x1 - x0 - 0.16).abs() < 1e-12);
    }

    #[test]
    fn knn_true_distance_matches_operator_metric() {
        // (3,4) from origin -> 5, exactly like <-> / ST_Distance.
        let d = knn_true_distance("POINT(3 4)", "POINT(0 0)").unwrap();
        assert!((d - 5.0).abs() < 1e-12);
    }
}
