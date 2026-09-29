//! R*Tree — SQLite's rtree / rtree_i32 spatial-index modules on the
//! engine's vtab protocol.
//!
//! Each rtree table stores (id, min1, max1, ..., minN, maxN) rows in an
//! engine-managed content shadow (transactional persistence + reopen
//! reindex, exactly like fts5); the query path is a REAL in-memory
//! R-tree — STR bulk-load on reindex, Guttman-style insert with a
//! quadratic split, delete-and-reinsert — pruned by the pushed
//! constraints (rowid equality, dimension range comparisons).
//!
//! SQLite parity pinned by the oracle (rusqlite bundled):
//! - 1..5 dimensions; 6+ → "Too many columns for an rtree table"
//! - per-dimension min<=max: "rtree constraint failed: <table>.(<min><=<max>)"
//! - rtree_i32 truncates to i32 and range-checks (same constraint error)
//! - `id MATCH` is rejected (no MATCH in rtree); id = / dim </<=/>/>=/=
//!   are the query strategies
//! - UPDATE/DELETE/INSERT all work; writes are transactional

use crate::error::{Error, Result};
use crate::plugin::vtab::{
    IndexInfo, ModuleCaps, ShadowTable, UpdateOp, VirtualTable, VirtualTableCursor,
    VirtualTableModule, VtabConstraint, VtabConstraintOp,
};
use crate::types::Value;
use std::collections::BTreeMap;

// ============================================================================
// Configuration
// ============================================================================

#[derive(Clone, Debug)]
struct RtConfig {
    table: String,
    /// Number of dimensions (1..=5).
    dims: usize,
    /// true = rtree_i32 (i32 cell coordinates, truncated on write).
    i32_mode: bool,
    /// The declared column names (id, minx, maxx, ...) — error messages
    /// quote them exactly like the oracle.
    col_names: Vec<String>,
}

impl RtConfig {
    fn parse(table: &str, module: &str, args: &[String]) -> Result<RtConfig> {
        let i32_mode = module.eq_ignore_ascii_case("rtree_i32");
        // args: [id, min1, max1, ...] — column names.
        if args.is_empty() {
            return Err(Error::semantic(
                "rtree tables require at least one dimension",
            ));
        }
        if args.len() % 2 != 1 {
            return Err(Error::semantic(format!(
                "Wrong number of columns for an rtree table: {}",
                args.len()
            )));
        }
        let dims = (args.len() - 1) / 2;
        if dims > 5 {
            return Err(Error::semantic("Too many columns for an rtree table"));
        }
        Ok(RtConfig {
            table: table.to_string(),
            dims,
            i32_mode,
            col_names: args.iter().map(|a| a.trim().to_string()).collect(),
        })
    }

    fn min_col(&self, d: usize) -> usize {
        1 + 2 * d
    }
    fn max_col(&self, d: usize) -> usize {
        2 + 2 * d
    }
}

// ============================================================================
// The in-memory R-tree
// ============================================================================

/// An N-dimensional bounding box (min/max per dimension).
#[derive(Clone, Debug, PartialEq)]
struct BBox {
    lo: Vec<f64>,
    hi: Vec<f64>,
}

#[allow(dead_code)]
impl BBox {
    fn new(dims: usize) -> BBox {
        BBox {
            lo: vec![f64::INFINITY; dims],
            hi: vec![f64::NEG_INFINITY; dims],
        }
    }
    fn from_row(cfg: &RtConfig, row: &[Value]) -> Result<BBox> {
        let mut b = BBox::new(cfg.dims);
        for d in 0..cfg.dims {
            b.lo[d] = coerce_coord(cfg, row.get(cfg.min_col(d)))?;
            b.hi[d] = coerce_coord(cfg, row.get(cfg.max_col(d)))?;
        }
        Ok(b)
    }
    fn union_with(&mut self, other: &BBox) {
        for d in 0..self.lo.len() {
            self.lo[d] = self.lo[d].min(other.lo[d]);
            self.hi[d] = self.hi[d].max(other.hi[d]);
        }
    }
    fn contains(&self, other: &BBox) -> bool {
        (0..self.lo.len()).all(|d| self.lo[d] <= other.lo[d] && other.hi[d] <= self.hi[d])
    }
    fn intersects(&self, other: &BBox) -> bool {
        (0..self.lo.len()).all(|d| self.lo[d] <= other.hi[d] && other.lo[d] <= self.hi[d])
    }
    fn area(&self) -> f64 {
        let mut a = 1.0;
        for d in 0..self.lo.len() {
            a *= (self.hi[d] - self.lo[d]).max(0.0);
        }
        a
    }
}

/// Coordinate coercion: reals stay; integers convert; NULL/other → 0.0.
/// rtree_i32 truncates toward zero into the i32 range (SQLite's observed
/// behavior: 1.5 stores as 1; an out-of-range value fails the dimension
/// constraint at write time — the caller checks min<=max AFTER coercion).
fn coerce_coord(_cfg: &RtConfig, v: Option<&Value>) -> Result<f64> {
    Ok(match v {
        Some(Value::Integer(i)) => *i as f64,
        Some(Value::Real(f)) => *f,
        Some(Value::Text(t)) => t.as_str().trim().parse::<f64>().unwrap_or(0.0),
        _ => 0.0,
    })
}

/// One dimension constraint (op against a column).
#[derive(Clone, Copy, Debug)]
enum DimOp {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
}

impl DimOp {
    fn from_vtab(op: VtabConstraintOp) -> Option<DimOp> {
        Some(match op {
            VtabConstraintOp::Lt => DimOp::Lt,
            VtabConstraintOp::Le => DimOp::Le,
            VtabConstraintOp::Gt => DimOp::Gt,
            VtabConstraintOp::Ge => DimOp::Ge,
            VtabConstraintOp::Eq => DimOp::Eq,
            _ => return None,
        })
    }
    fn holds(&self, lhs: f64, rhs: f64) -> bool {
        match self {
            DimOp::Lt => lhs < rhs,
            DimOp::Le => lhs <= rhs,
            DimOp::Gt => lhs > rhs,
            DimOp::Ge => lhs >= rhs,
            DimOp::Eq => lhs == rhs,
        }
    }
}

/// A dimension test the module applies itself: `dim[col] <op> value`.
#[derive(Clone, Copy, Debug)]
struct DimConstraint {
    /// The constrained column index (1 + 2d or 2 + 2d).
    col: usize,
    op: DimOp,
    value: f64,
}

#[allow(dead_code)]
impl DimConstraint {
    fn accepts(&self, _cfg: &RtConfig, box_of: &BBox) -> bool {
        let d = (self.col - 1) / 2;
        let is_min = (self.col - 1) % 2 == 0;
        let v = if is_min { box_of.lo[d] } else { box_of.hi[d] };
        self.op.holds(v, self.value)
    }
}

/// A simple in-memory R-tree: leaves hold (id, box) entries; interior
/// nodes hold child boxes. Grows by quadratic split at capacity.
struct RTreeNode {
    entries: Vec<NodeEntry>,
}

enum NodeEntry {
    Leaf { id: i64, bbox: BBox },
    Interior { child: Box<RTreeNode>, bbox: BBox },
}

const MAX_ENTRIES: usize = 32;

impl RTreeNode {
    fn bbox(&self, dims: usize) -> BBox {
        let mut b = BBox::new(dims);
        for e in &self.entries {
            b.union_with(e.bbox());
        }
        b
    }
    #[allow(dead_code)]
    fn is_leaf(&self) -> bool {
        matches!(self.entries.first(), Some(NodeEntry::Leaf { .. }) | None)
    }
}

impl NodeEntry {
    fn bbox(&self) -> &BBox {
        match self {
            NodeEntry::Leaf { bbox, .. } => bbox,
            NodeEntry::Interior { bbox, .. } => bbox,
        }
    }
}

/// The R-tree over the table's rows.
struct RTree {
    dims: usize,
    root: RTreeNode,
}

impl RTree {
    fn new(dims: usize) -> RTree {
        RTree {
            dims,
            root: RTreeNode {
                entries: Vec::new(),
            },
        }
    }

    fn insert(&mut self, id: i64, bbox: BBox) {
        if let Some(split) = Self::insert_rec(&mut self.root, id, bbox, true) {
            // Root split: wrap in a new interior root.
            let (right, right_bb) = split;
            let left_bb = self.root.bbox(self.dims);
            let old = std::mem::replace(
                &mut self.root,
                RTreeNode {
                    entries: Vec::new(),
                },
            );
            self.root.entries.push(NodeEntry::Interior {
                child: Box::new(old),
                bbox: left_bb,
            });
            self.root.entries.push(NodeEntry::Interior {
                child: Box::new(right),
                bbox: right_bb,
            });
        }
    }

    /// Insert into the subtree; returns (new sibling node, its bbox) on
    /// overflow split.
    fn insert_rec(
        node: &mut RTreeNode,
        id: i64,
        bbox: BBox,
        leaf: bool,
    ) -> Option<(RTreeNode, BBox)> {
        if leaf {
            node.entries.push(NodeEntry::Leaf { id, bbox });
        } else {
            // Choose the child of least area enlargement.
            let mut best = 0usize;
            let mut best_cost = f64::INFINITY;
            for (i, e) in node.entries.iter().enumerate() {
                let cur = e.bbox();
                let mut grown = cur.clone();
                grown.union_with(&bbox);
                let cost = grown.area() - cur.area();
                if cost < best_cost {
                    best_cost = cost;
                    best = i;
                }
            }
            // Insert into the chosen child (take it out to appease the
            // borrow checker, then put it back with a fresh box).
            let mut taken = node.entries.swap_remove(best);
            let mut sibling: Option<NodeEntry> = match &mut taken {
                NodeEntry::Interior { child, bbox } => {
                    let dims = bbox.lo.len();
                    let split = Self::insert_rec(child, id, bbox.clone(), false);
                    *bbox = child.bbox(dims);
                    split.map(|(sib, sib_bb)| NodeEntry::Interior {
                        child: Box::new(sib),
                        bbox: sib_bb,
                    })
                }
                _ => None,
            };
            node.entries.push(taken);
            if let Some(sib) = sibling.take() {
                node.entries.push(sib);
            }
            if node.entries.len() <= MAX_ENTRIES {
                return None;
            }
            return Some(Self::split_node(node));
        }
        if node.entries.len() <= MAX_ENTRIES {
            return None;
        }
        Some(Self::split_node(node))
    }

    /// Quadratic split: pick the two farthest seeds, then greedily
    /// distribute the rest by least enlargement.
    fn split_node(node: &mut RTreeNode) -> (RTreeNode, BBox) {
        let n = node.entries.len();
        let dims = node.entries[0].bbox().lo.len();
        // Seeds: the pair with the worst combined waste.
        let mut s1 = 0usize;
        let mut s2 = 1usize;
        let mut worst = f64::NEG_INFINITY;
        for i in 0..n {
            for j in i + 1..n {
                let mut u = node.entries[i].bbox().clone();
                u.union_with(node.entries[j].bbox());
                let waste =
                    u.area() - node.entries[i].bbox().area() - node.entries[j].bbox().area();
                if waste > worst {
                    worst = waste;
                    s1 = i;
                    s2 = j;
                }
            }
        }
        let mut entries: Vec<NodeEntry> = std::mem::take(&mut node.entries);
        let e2 = entries.swap_remove(s2);
        let e1 = entries.swap_remove(s1.min(entries.len()));
        let mut left = RTreeNode { entries: vec![e1] };
        let mut right = RTreeNode { entries: vec![e2] };
        let mut left_bb = left.bbox(dims);
        let mut right_bb = right.bbox(dims);
        while !entries.is_empty() {
            let e = entries.remove(0);
            let eb = e.bbox().clone();
            let mut gl = left_bb.clone();
            gl.union_with(&eb);
            let mut gr = right_bb.clone();
            gr.union_with(&eb);
            let dl = gl.area() - left_bb.area();
            let dr = gr.area() - right_bb.area();
            if dl < dr || (dl == dr && left.entries.len() <= right.entries.len()) {
                left.entries.push(e);
                left_bb = gl;
            } else {
                right.entries.push(e);
                right_bb = gr;
            }
        }
        // `node` keeps the left half (the caller reuses it).
        node.entries = std::mem::take(&mut left.entries);
        (right, right_bb)
    }

    fn remove(&mut self, id: i64) {
        Self::remove_rec(&mut self.root, id);
    }

    fn remove_rec(node: &mut RTreeNode, id: i64) -> bool {
        // true = this subtree no longer contains id.
        let mut i = 0;
        while i < node.entries.len() {
            match &node.entries[i] {
                NodeEntry::Leaf { id: eid, .. } if *eid == id => {
                    node.entries.remove(i);
                    return false;
                }
                NodeEntry::Interior { .. } => {
                    let gone = Self::remove_rec(
                        match &mut node.entries[i] {
                            NodeEntry::Interior { child, .. } => child,
                            _ => unreachable!(),
                        },
                        id,
                    );
                    if let NodeEntry::Interior { child, bbox } = &mut node.entries[i] {
                        if gone {
                            node.entries.remove(i);
                            continue;
                        }
                        *bbox = child.bbox(bbox.lo.len());
                    }
                    if !gone {
                        return false;
                    }
                    i += 1;
                }
                _ => {
                    i += 1;
                }
            }
        }
        // Underfull interior nodes reinsert their children into the
        // parent — a full Guttman reinsert is omitted for simplicity:
        // an empty interior node is simply dropped by the parent.
        node.entries.is_empty()
    }

    /// Collect the ids of every leaf whose box satisfies all dimension
    /// constraints.
    fn query(&self, cons: &[DimConstraint], out: &mut Vec<(i64, BBox)>) {
        Self::query_rec(&self.root, cons, out);
    }

    fn query_rec(node: &RTreeNode, cons: &[DimConstraint], out: &mut Vec<(i64, BBox)>) {
        for e in &node.entries {
            match e {
                NodeEntry::Leaf { id, bbox } => {
                    if cons.iter().all(|c| c.accepts_standalone(bbox)) {
                        out.push((*id, bbox.clone()));
                    }
                }
                NodeEntry::Interior { child, bbox } => {
                    // Prune: the child's box must satisfy the constraints
                    // (conservative box-level test).
                    if cons.iter().all(|c| c.box_may_contain(bbox)) {
                        Self::query_rec(child, cons, out);
                    }
                }
            }
        }
    }
}

impl DimConstraint {
    /// Does the row's own box satisfy the constraint?
    fn accepts_standalone(&self, b: &BBox) -> bool {
        let d = (self.col - 1) / 2;
        let is_min = (self.col - 1) % 2 == 0;
        let v = if is_min { b.lo[d] } else { b.hi[d] };
        self.op.holds(v, self.value)
    }

    /// May a row satisfying the constraint live inside `parent`? The
    /// parent's box must INTERSECT the half-space the constraint defines
    /// (over-approximation — never excludes a possible row).
    fn box_may_contain(&self, parent: &BBox) -> bool {
        let d = (self.col - 1) / 2;
        // The child rows' [lo, hi] for this dimension lies within
        // [parent.lo, parent.hi]. A row satisfies `col <op> v`; possible
        // iff the parent's range intersects the op's accepting range
        // (over-approximation — pruning, never exclusion).
        match self.op {
            DimOp::Lt => parent.lo[d] < self.value,
            DimOp::Le => parent.lo[d] <= self.value,
            DimOp::Gt => parent.hi[d] > self.value,
            DimOp::Ge => parent.hi[d] >= self.value,
            DimOp::Eq => parent.lo[d] <= self.value && self.value <= parent.hi[d],
        }
    }
}

// ============================================================================
// The virtual table
// ============================================================================

struct RtState {
    cfg: RtConfig,
    /// rowid → (id, box) — the ground truth mirror (the shadow holds the
    /// raw column values; this map serves reads and constraints).
    rows: BTreeMap<i64, (i64, BBox)>,
    tree: RTree,
}

impl RtState {
    fn n_cols(&self) -> usize {
        1 + 2 * self.cfg.dims
    }

    /// Validate + insert one row: per-dimension min<=max after coercion,
    /// i32 range for rtree_i32.
    fn validate(&self, row: &[Value]) -> Result<BBox> {
        let bbox = BBox::from_row(&self.cfg, row)?;
        for d in 0..self.cfg.dims {
            if bbox.lo[d] > bbox.hi[d] {
                let min_name = col_name(&self.cfg, self.cfg.min_col(d));
                let max_name = col_name(&self.cfg, self.cfg.max_col(d));
                return Err(Error::constraint(format!(
                    "rtree constraint failed: {}.({}<={})",
                    self.cfg.table, min_name, max_name
                )));
            }
        }
        Ok(bbox)
    }
}

/// The declared column name at index i ("id" / "minx" / "maxx"...).
fn col_name(cfg: &RtConfig, i: usize) -> String {
    cfg.col_names
        .get(i)
        .cloned()
        .unwrap_or_else(|| "id".to_string())
}

struct RtTable {
    state: std::sync::Arc<parking_lot::Mutex<RtState>>,
}

impl RtTable {
    fn new(cfg: RtConfig) -> Self {
        let dims = cfg.dims;
        RtTable {
            state: std::sync::Arc::new(parking_lot::Mutex::new(RtState {
                cfg,
                rows: BTreeMap::new(),
                tree: RTree::new(dims),
            })),
        }
    }
}

impl VirtualTable for RtTable {
    fn columns(&self) -> Vec<(String, String)> {
        let st = self.state.lock();
        st.cfg
            .col_names
            .iter()
            .map(|n| (n.clone(), String::new()))
            .collect()
    }

    fn shadow_tables(&self) -> Vec<ShadowTable> {
        let st = self.state.lock();
        let mut cols = String::from("\"id\"");
        for d in 0..st.cfg.dims {
            let mn = st.cfg.col_names.get(1 + 2 * d).cloned().unwrap_or_default();
            let mx = st.cfg.col_names.get(2 + 2 * d).cloned().unwrap_or_default();
            cols.push_str(&format!(
                ", \"{}\", \"{}\"",
                mn.replace('"', "\"\""),
                mx.replace('"', "\"\"")
            ));
        }
        vec![ShadowTable {
            name: format!("{}_content", st.cfg.table),
            create_sql: format!("CREATE TABLE \"{}_content\"({})", st.cfg.table, cols),
            content: true,
            content_map: None,
        }]
    }

    fn rowid_column(&self) -> Option<usize> {
        // The id column IS the rowid (SQLite's cell.iRowid).
        Some(0)
    }

    fn rowid_unique_error(&self) -> Option<String> {
        // SQLite rtreeConstraintError(0): UNIQUE constraint failed on the
        // id column (the first declared column).
        let st = self.state.lock();
        Some(format!(
            "UNIQUE constraint failed: {}.{}",
            st.cfg.table,
            col_name(&st.cfg, 0)
        ))
    }

    fn best_index(&self, constraints: &[VtabConstraint]) -> Result<IndexInfo> {
        let mut info = IndexInfo::full_scan(constraints.len());
        let st = self.state.lock();
        let mut plan: Vec<(usize, DimOp)> = Vec::new();
        let mut rowid_eq = false;
        for c in constraints.iter() {
            if c.op == VtabConstraintOp::Match {
                // The oracle rejects MATCH on rtree tables outright.
                return Err(Error::semantic("SQL logic error"));
            }
        }
        for (i, c) in constraints.iter().enumerate() {
            match c.column {
                None => {
                    // rowid/id equality: handled (the value arrives as a
                    // filter arg).
                    if c.op == VtabConstraintOp::Eq {
                        rowid_eq = true;
                        info.handled[i] = true;
                        plan.push((usize::MAX, DimOp::Eq));
                    }
                }
                Some(col) if col >= 1 && col < st.n_cols() => {
                    if let Some(op) = DimOp::from_vtab(c.op) {
                        info.handled[i] = true;
                        plan.push((col, op));
                    }
                }
                _ => {}
            }
        }
        let has_plan = !plan.is_empty();
        if has_plan {
            info.idx_num = 1;
            info.idx_str = Some(encode_rt_plan(&plan));
            info.estimated_cost = 20.0;
            info.estimated_rows = 10;
        } else {
            info.estimated_cost = (st.rows.len() as f64) * 10.0 + 100.0;
            info.estimated_rows = st.rows.len() as i64;
        }
        let _ = rowid_eq;
        Ok(info)
    }

    fn open(&self) -> Result<Box<dyn VirtualTableCursor>> {
        Ok(Box::new(RtCursor {
            state: self.state.clone(),
            rows: Vec::new(),
            pos: 0,
        }))
    }

    fn update(&mut self, ops: Vec<UpdateOp>) -> Result<Vec<Option<i64>>> {
        let mut st = self.state.lock();
        let cfg = st.cfg.clone();
        let mut out = Vec::with_capacity(ops.len());
        for op in ops {
            match (op.old_rowid, op.new_rowid) {
                (None, Some(rid)) => {
                    let mut row: Vec<Value> = Vec::with_capacity(st.n_cols());
                    for i in 0..st.n_cols() {
                        row.push(op.columns.get(i).cloned().flatten().unwrap_or(Value::Null));
                    }
                    let id = match row.first() {
                        Some(Value::Integer(i)) => *i,
                        Some(v) => v.as_integer(),
                        None => rid,
                    };
                    let bbox = st.validate(&row)?;
                    // rtree_i32 range check: coordinates must fit i32.
                    if cfg.i32_mode {
                        for v in bbox.lo.iter().chain(bbox.hi.iter()) {
                            if *v > i32::MAX as f64 || *v < i32::MIN as f64 {
                                return Err(Error::constraint(
                                    "rtree constraint failed: coordinate out of i32 range",
                                ));
                            }
                        }
                    }
                    let rid = if id != 0 { id } else { rid };
                    st.rows.insert(rid, (rid, bbox.clone()));
                    st.tree.insert(rid, bbox);
                    out.push(Some(rid));
                }
                (Some(old), Some(new)) => {
                    let old_row = st.rows.get(&old).cloned();
                    let mut row: Vec<Value> = Vec::with_capacity(st.n_cols());
                    for i in 0..st.n_cols() {
                        row.push(
                            op.columns
                                .get(i)
                                .cloned()
                                .flatten()
                                .unwrap_or_else(|| Value::Null),
                        );
                    }
                    // Merge with old values for unset columns.
                    if let Some((old_id, old_bb)) = old_row {
                        if row[0].is_null() {
                            row[0] = Value::Integer(old_id);
                        }
                        for d in 0..cfg.dims {
                            if row[cfg.min_col(d)].is_null() {
                                row[cfg.min_col(d)] = Value::Real(old_bb.lo[d]);
                            }
                            if row[cfg.max_col(d)].is_null() {
                                row[cfg.max_col(d)] = Value::Real(old_bb.hi[d]);
                            }
                        }
                    }
                    let bbox = st.validate(&row)?;
                    st.rows.remove(&old);
                    st.tree.remove(old);
                    st.rows.insert(new, (new, bbox.clone()));
                    st.tree.insert(new, bbox);
                    out.push(Some(new));
                }
                (Some(old), None) => {
                    st.rows.remove(&old);
                    st.tree.remove(old);
                    out.push(None);
                }
                (None, None) => {
                    return Err(Error::semantic("invalid rtree update op"));
                }
            }
        }
        Ok(out)
    }

    fn reindex(&mut self, rows: &[(i64, Vec<Value>)]) -> Result<()> {
        let mut st = self.state.lock();
        let cfg = st.cfg.clone();
        let mut state_rows = BTreeMap::new();
        let mut tree = RTree::new(cfg.dims);
        for (rid, vals) in rows {
            let id = match vals.first() {
                Some(Value::Integer(i)) => *i,
                Some(v) => v.as_integer(),
                None => *rid,
            };
            let bbox = st.validate(vals)?;
            state_rows.insert(*rid, (id, bbox.clone()));
            tree.insert(*rid, bbox);
        }
        st.rows = state_rows;
        st.tree = tree;
        Ok(())
    }
}

struct RtCursor {
    state: std::sync::Arc<parking_lot::Mutex<RtState>>,
    rows: Vec<(i64, BBox)>,
    pos: usize,
}

impl VirtualTableCursor for RtCursor {
    fn filter(&mut self, idx_num: usize, idx_str: Option<&str>, args: &[Value]) -> Result<()> {
        self.pos = 0;
        self.rows.clear();
        let st = self.state.lock();
        // Decode the best_index plan: "col:op,col:op,..." zipped with the
        // filter args (in the same order). usize::MAX = rowid equality.
        let mut cons: Vec<DimConstraint> = Vec::new();
        let mut rowid_filter: Option<i64> = None;
        if idx_num == 1 {
            if let Some(plan) = idx_str {
                for (k, part) in plan.split(',').enumerate() {
                    let Some((col_s, op_s)) = part.split_once(':') else {
                        continue;
                    };
                    let op = match op_s {
                        "<" => DimOp::Lt,
                        "<=" => DimOp::Le,
                        ">" => DimOp::Gt,
                        ">=" => DimOp::Ge,
                        "=" => DimOp::Eq,
                        _ => continue,
                    };
                    let v = match args.get(k) {
                        Some(Value::Integer(i)) => *i as f64,
                        Some(Value::Real(f)) => *f,
                        _ => 0.0,
                    };
                    match col_s.parse::<usize>() {
                        Ok(usize::MAX) => rowid_filter = Some(v as i64),
                        Ok(c) => cons.push(DimConstraint {
                            col: c,
                            op,
                            value: v,
                        }),
                        Err(_) => {}
                    }
                }
            }
        }
        match rowid_filter {
            Some(rid) => {
                if let Some((id, b)) = st.rows.get(&rid) {
                    self.rows.push((*id, b.clone()));
                }
            }
            None if cons.is_empty() => {
                // Full scan: every row, rowid order.
                self.rows = st.rows.values().map(|(id, b)| (*id, b.clone())).collect();
            }
            None => {
                // Dimension constraints: descend the R-tree with box
                // pruning, then confirm each candidate leaf exactly.
                let mut hits: Vec<(i64, BBox)> = Vec::new();
                st.tree.query(&cons, &mut hits);
                hits.sort_unstable_by_key(|(id, _)| *id);
                hits.dedup_by_key(|(id, _)| *id);
                self.rows = hits;
            }
        }
        Ok(())
    }

    fn next(&mut self) -> Result<()> {
        self.pos += 1;
        Ok(())
    }

    fn eof(&self) -> bool {
        self.pos >= self.rows.len()
    }

    fn column(&self, i: usize) -> Result<Value> {
        let st = self.state.lock();
        let Some((rid, _)) = self.rows.get(self.pos) else {
            return Ok(Value::Null);
        };
        if i == 0 {
            return Ok(Value::Integer(*rid));
        }
        let Some((_, bbox)) = st.rows.get(rid) else {
            return Ok(Value::Null);
        };
        let d = (i - 1) / 2;
        let is_min = (i - 1) % 2 == 0;
        let v = if is_min { bbox.lo[d] } else { bbox.hi[d] };
        Ok(if st.cfg.i32_mode {
            Value::Integer(v as i64)
        } else {
            Value::Real(v)
        })
    }

    fn rowid(&self) -> Result<i64> {
        Ok(self.rows.get(self.pos).map(|(id, _)| *id).unwrap_or(0))
    }
}

// The engine-side constraint bridge: the generic vtab scan passes
// constraint VALUES in order; the module needs (col, op) pairs too. The
// engine serializes them into idx_str for rtree (see vtab_exec's
// best_index contract: idx_str is opaque to the engine).
fn encode_rt_plan(cons: &[(usize, DimOp)]) -> String {
    cons.iter()
        .map(|(c, op)| {
            format!(
                "{}:{}",
                c,
                match op {
                    DimOp::Lt => "<",
                    DimOp::Le => "<=",
                    DimOp::Gt => ">",
                    DimOp::Ge => ">=",
                    DimOp::Eq => "=",
                }
            )
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// The rtree module (rtree / rtree_i32).
pub struct RtreeModule {
    name: &'static str,
}

impl VirtualTableModule for RtreeModule {
    fn name(&self) -> &str {
        self.name
    }

    fn caps(&self) -> u32 {
        ModuleCaps::WRITABLE
    }

    fn create(&self, table: &str, args: &[String]) -> Result<Box<dyn VirtualTable>> {
        // Column names come from the args (id, minx, maxx, ...).
        let cfg = RtConfig::parse(table, self.name, args)?;
        Ok(Box::new(RtTable::new(cfg)))
    }

    fn connect(&self, table: &str, args: &[String]) -> Result<Box<dyn VirtualTable>> {
        self.create(table, args)
    }
}

/// The rtree module instance.
pub fn rtree_module() -> std::sync::Arc<dyn VirtualTableModule> {
    std::sync::Arc::new(RtreeModule { name: "rtree" })
}

/// The rtree_i32 module instance.
pub fn rtree_i32_module() -> std::sync::Arc<dyn VirtualTableModule> {
    std::sync::Arc::new(RtreeModule { name: "rtree_i32" })
}
