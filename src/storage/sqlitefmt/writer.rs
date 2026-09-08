//! SQLite-format file WRITER: builds a complete, dense, `integrity_check`
//! clean SQLite `.db` from decoded engine state.
//!
//! Every b-tree is bulk-built bottom-up. Separators follow SQLite's own
//! invariants (verified empirically against 3.46/3.53 files):
//!
//! * **Table trees**: the parent cell's key is the right-most rowid of
//!   the left child page, *copied* up (rowid bounds, not real entries).
//! * **Index trees** (incl. WITHOUT ROWID tables): the boundary entry is
//!   *removed* from the child level and stored only in the parent cell —
//!   interior cells carry real entries, never duplicated in leaves.
//! * In-order traversal is non-decreasing; left subtree < separator,
//!   right subtree >= separator.
//!
//! Overflow chains follow fileformat2's local-length formulas exactly.
//! The output is fully compact: no freelist, no freeblocks, no pointer
//! maps, reserved bytes per page = 0.
//!
//! Durability: temp file + fsync + atomic rename — a crash leaves either
//! the old or the new file, never a mix. Stale `-wal`/`-shm` sidecars are
//! removed so a subsequent SQLite open reads the fresh image.

use std::path::Path;

use super::header::build_header;
use super::record::encode_record;
use super::varint::write_varint;
use crate::types::value::Value;

/// One output object in creation order (its future sqlite_schema row).
#[derive(Clone, Debug)]
pub enum OutObject {
    /// Rowid table: rows are `(rowid, values)`; `alias` is the column
    /// index stored as NULL in the record (INTEGER PRIMARY KEY).
    Table {
        name: String,
        sql: String,
        alias: Option<usize>,
        rows: Vec<(i64, Vec<Value>)>,
    },
    /// WITHOUT ROWID table: values are full records with the PK columns
    /// first (already reordered and sorted by the caller).
    WithoutRowid {
        name: String,
        sql: String,
        rows: Vec<Vec<Value>>,
    },
    /// Index b-tree (explicit or autoindex). `entries` are complete index
    /// records: key columns followed by the rowid (or the remaining PK
    /// columns for a WITHOUT ROWID table) — the caller composes them.
    /// `desc` / `collations` apply to the KEY columns only.
    Index {
        name: String,
        tbl_name: String,
        sql: Option<String>,
        desc: Vec<bool>,
        collations: Vec<Collation>,
        entries: Vec<Vec<Value>>,
    },
    /// View or trigger: rootpage 0, sql round-tripped.
    Other {
        kind: &'static str,
        name: String,
        tbl_name: String,
        sql: String,
    },
}

/// Collations the writer can order by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Collation {
    Binary,
    NoCase,
}

/// Whole-file build parameters.
#[derive(Clone, Debug)]
pub struct OutDb {
    pub page_size: u32,
    pub user_version: u32,
    pub application_id: u32,
    pub change_counter: u32,
    pub schema_cookie: u32,
    pub objects: Vec<OutObject>,
}

/// Serialize the database to a complete page image.
pub fn build_bytes(db: &OutDb) -> Result<Vec<u8>, String> {
    let page_size = if db.page_size == 0 {
        4096
    } else {
        db.page_size
    };
    if !page_size.is_power_of_two() || !(512..=65536).contains(&page_size) {
        return Err(format!("invalid page size {page_size}"));
    }
    let usable = page_size as usize;
    let mut alloc = PageAllocator::new(page_size);

    let mut schema_cells: Vec<SchemaCell> = Vec::new();
    for obj in &db.objects {
        match obj {
            OutObject::Table {
                name,
                sql,
                alias,
                rows,
            } => {
                let root = build_table_tree(&mut alloc, usable, rows, *alias, None)?;
                schema_cells.push(SchemaCell {
                    rowid: 0,
                    kind: "table".into(),
                    name: name.clone(),
                    tbl_name: name.clone(),
                    root,
                    sql: Some(sql.clone()),
                });
            }
            OutObject::WithoutRowid { name, sql, rows } => {
                let root = build_record_tree(&mut alloc, usable, rows, &[], &[])?;
                schema_cells.push(SchemaCell {
                    rowid: 0,
                    kind: "table".into(),
                    name: name.clone(),
                    tbl_name: name.clone(),
                    root,
                    sql: Some(sql.clone()),
                });
            }
            OutObject::Index {
                name,
                tbl_name,
                sql,
                desc,
                collations,
                entries,
            } => {
                let root = build_record_tree(&mut alloc, usable, entries, desc, collations)?;
                schema_cells.push(SchemaCell {
                    rowid: 0,
                    kind: "index".into(),
                    name: name.clone(),
                    tbl_name: tbl_name.clone(),
                    root,
                    sql: sql.clone(),
                });
            }
            OutObject::Other {
                kind,
                name,
                tbl_name,
                sql,
            } => {
                schema_cells.push(SchemaCell {
                    rowid: 0,
                    kind: (*kind).into(),
                    name: name.clone(),
                    tbl_name: tbl_name.clone(),
                    root: 0,
                    sql: Some(sql.clone()),
                });
            }
        }
    }

    // Schema tree last: its root is FIXED at page 1; children are
    // allocated after every user page.
    for (i, c) in schema_cells.iter_mut().enumerate() {
        c.rowid = (i + 1) as i64;
    }
    build_schema_tree(&mut alloc, usable, &schema_cells)?;

    // Assemble the page image.
    let n_pages = alloc.n_pages();
    let mut image = vec![0u8; n_pages as usize * page_size as usize];
    for (page_no, bytes) in alloc.finished() {
        let off = (page_no as usize - 1) * page_size as usize;
        image[off..off + page_size as usize].copy_from_slice(bytes.as_slice());
    }
    let header = build_header(
        page_size,
        n_pages,
        db.change_counter,
        db.schema_cookie,
        db.user_version,
        db.application_id,
    );
    image[0..100].copy_from_slice(&header);
    Ok(image)
}

/// Remove a sidecar file; when Windows denies removal (open handle
/// without share-delete), truncate it to zero length instead.
fn clear_sidecar(path: &std::path::Path) {
    if std::fs::remove_file(path).is_err() {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(path)
        {
            let _ = f.write_all(&[]);
            let _ = f.sync_all();
        }
    }
}

/// Write atomically (temp file + fsync + rename), with an in-place
/// fallback when Windows denies the rename over a live cross-process
/// handle.
pub fn write_sqlite_file(path: &Path, db: &OutDb) -> Result<(), String> {
    let bytes = build_bytes(db)?;
    let tmp = path.with_extension(format!("rsqltmp{}", std::process::id()));
    {
        use std::io::Write;
        let mut f =
            std::fs::File::create(&tmp).map_err(|e| format!("create {}: {e}", tmp.display()))?;
        f.write_all(&bytes)
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
        f.sync_all()
            .map_err(|e| format!("fsync {}: {e}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path)
        .or_else(|e| {
            // Windows/EXDEV: fall back to remove + rename.
            let _ = std::fs::remove_file(path);
            std::fs::rename(&tmp, path).map_err(|e2| format!("rename: {e} / {e2}"))
        })
        .or_else(|e| {
            // Windows with a live cross-process SQLite connection: the
            // other handle has share read+write but no share-delete, so
            // both rename and remove are denied ("os error 5"). SQLite
            // itself opens the file with share-write, so overwriting the
            // bytes in place succeeds — atomicity is traded for
            // correctness in exactly this edge (single writer).
            let _ = e;
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(path)
                .map_err(|e| format!("in-place open {}: {e}", path.display()))?;
            f.write_all(&bytes)
                .map_err(|e| format!("in-place write {}: {e}", path.display()))?;
            f.sync_all()
                .map_err(|e| format!("in-place fsync {}: {e}", path.display()))?;
            let _ = std::fs::remove_file(&tmp);
            Ok::<(), String>(())
        })?;
    // Stale sidecars would resurrect pre-dump frames. On Windows a live
    // cross-process connection may hold them open (no share-delete), so
    // when removal fails, TRUNCATE in place: a zero-length WAL is an
    // empty WAL to every SQLite reader, and the writer's own close-time
    // checkpoint of an empty WAL is a no-op instead of a resurrection.
    clear_sidecar(&super::reader::wal_path_of(path));
    let mut shm = path.as_os_str().to_os_string();
    shm.push("-shm");
    clear_sidecar(std::path::Path::new(&shm));
    Ok(())
}

// ---------------------------------------------------------------------------
// Page allocation
// ---------------------------------------------------------------------------

struct PageAllocator {
    page_size: u32,
    /// Next sequential page number. Page 1 is reserved for the schema
    /// root; the first dynamic page is 2.
    next: u32,
    pages: Vec<(u32, Vec<u8>)>,
}

impl PageAllocator {
    fn new(page_size: u32) -> Self {
        Self {
            page_size,
            next: 2,
            pages: Vec::new(),
        }
    }

    fn alloc(&mut self) -> u32 {
        let n = self.next;
        self.next += 1;
        n
    }

    /// Total page count including the reserved page 1.
    fn n_pages(&self) -> u32 {
        self.next - 1
    }

    fn store(&mut self, page_no: u32, bytes: Vec<u8>) {
        debug_assert_eq!(bytes.len(), self.page_size as usize);
        self.pages.push((page_no, bytes));
    }

    fn finished(self) -> Vec<(u32, Vec<u8>)> {
        self.pages
    }
}

// ---------------------------------------------------------------------------
// Cell representation
// ---------------------------------------------------------------------------

/// A pending b-tree cell. `head` carries the complete on-page bytes
/// (with a zero 4-byte overflow pointer when `ptr_pos` is set); `tail`
/// holds the payload bytes destined for the overflow chain.
#[derive(Clone)]
struct Cell {
    head: Vec<u8>,
    tail: Vec<u8>,
    /// Position of the 4-byte overflow pointer inside `head`.
    ptr_pos: Option<usize>,
    /// Table trees: the cell's rowid key (used for copied-up separators).
    rowid: i64,
}

/// Table-leaf cell: `[varint P][varint rowid][local payload][u32 next]`.
fn make_table_leaf_cell(usable: usize, rowid: i64, payload: &[u8]) -> Cell {
    let u = usable;
    let total = payload.len();
    let x = u - 35;
    if total <= x {
        let mut head = Vec::with_capacity(total + 18);
        write_varint(&mut head, total as i64);
        write_varint(&mut head, rowid);
        head.extend_from_slice(payload);
        return Cell {
            head,
            tail: Vec::new(),
            ptr_pos: None,
            rowid,
        };
    }
    let m = ((u - 12) * 32 / 255) - 23;
    let k = m + ((total - m) % (u - 4));
    let local = if k <= x { k } else { m };
    let mut head = Vec::with_capacity(local + 20);
    write_varint(&mut head, total as i64);
    write_varint(&mut head, rowid);
    head.extend_from_slice(&payload[..local]);
    let ptr_pos = head.len();
    head.extend_from_slice(&[0, 0, 0, 0]);
    Cell {
        head,
        tail: payload[local..].to_vec(),
        ptr_pos: Some(ptr_pos),
        rowid,
    }
}

/// Index cell (leaf or interior): `[varint P][local record][u32 next]`
/// — interior cells additionally carry a 4-byte left-child pointer
/// prepended at page-write time.
fn make_index_cell(usable: usize, entry: &[Value]) -> Cell {
    let payload = encode_record(entry);
    let u = usable;
    let total = payload.len();
    let x = ((u - 12) * 64 / 255) - 23;
    let head = |local: usize| {
        let mut h = Vec::with_capacity(local + 16);
        write_varint(&mut h, total as i64);
        h.extend_from_slice(&payload[..local]);
        h
    };
    if total <= x {
        let mut h = head(total);
        h.shrink_to_fit();
        return Cell {
            head: h,
            tail: Vec::new(),
            ptr_pos: None,
            rowid: 0,
        };
    }
    let m = ((u - 12) * 32 / 255) - 23;
    let k = m + ((total - m) % (u - 4));
    let local = if k <= x { k } else { m };
    let mut h = head(local);
    let ptr_pos = h.len();
    h.extend_from_slice(&[0, 0, 0, 0]);
    Cell {
        head: h,
        tail: payload[local..].to_vec(),
        ptr_pos: Some(ptr_pos),
        rowid: 0,
    }
}

/// Build the overflow chain for a cell (when it has one), patching the
/// pointer inside `head`. Returns the final on-page cell bytes.
fn finish_cell(alloc: &mut PageAllocator, cell: Cell) -> Vec<u8> {
    let Some(ptr_pos) = cell.ptr_pos else {
        return cell.head;
    };
    if cell.tail.is_empty() {
        return cell.head;
    }
    let page_size = alloc.page_size as usize;
    let cap = page_size - 4;
    let n_chain = cell.tail.len().div_ceil(cap);
    let page_nos: Vec<u32> = (0..n_chain).map(|_| alloc.alloc()).collect();
    let mut head = cell.head;
    head[ptr_pos..ptr_pos + 4].copy_from_slice(&page_nos[0].to_be_bytes());
    for i in 0..n_chain {
        let start = i * cap;
        let end = (start + cap).min(cell.tail.len());
        let next = page_nos.get(i + 1).copied().unwrap_or(0);
        let mut page = vec![0u8; page_size];
        page[0..4].copy_from_slice(&next.to_be_bytes());
        page[4..4 + (end - start)].copy_from_slice(&cell.tail[start..end]);
        alloc.store(page_nos[i], page);
    }
    head
}

// ---------------------------------------------------------------------------
// Tree builders
// ---------------------------------------------------------------------------

/// Build a TABLE b-tree from `(rowid, values)` rows.
fn build_table_tree(
    alloc: &mut PageAllocator,
    usable: usize,
    rows: &[(i64, Vec<Value>)],
    alias: Option<usize>,
    fixed_root: Option<u32>,
) -> Result<u32, String> {
    let mut cells: Vec<Cell> = Vec::with_capacity(rows.len());
    for (rowid, values) in rows {
        let payload = super::record::encode_record_aliased(values, alias);
        cells.push(make_table_leaf_cell(usable, *rowid, &payload));
    }
    if cells.is_empty() {
        return Ok(write_leaf_page(alloc, 0x0d, cells, fixed_root));
    }
    pack_table_tree(alloc, usable, cells, fixed_root)
}

/// Build an INDEX-shaped b-tree from complete records (WITHOUT ROWID
/// tables and index b-trees). Entries are sorted here when `desc` /
/// `collations` are declared (explicit indexes); callers that pre-sort
/// pass empty slices.
fn build_record_tree(
    alloc: &mut PageAllocator,
    usable: usize,
    entries: &[Vec<Value>],
    desc: &[bool],
    collations: &[Collation],
) -> Result<u32, String> {
    let cells: Vec<Cell> = if desc.is_empty() && collations.is_empty() {
        entries.iter().map(|e| make_index_cell(usable, e)).collect()
    } else {
        let mut sorted: Vec<&Vec<Value>> = entries.iter().collect();
        sorted.sort_by(|a, b| compare_index_keys(a, b, desc, collations));
        sorted
            .into_iter()
            .map(|e| make_index_cell(usable, e))
            .collect()
    };
    if cells.is_empty() {
        return Ok(write_leaf_page(alloc, 0x0a, cells, None));
    }
    pack_index_tree(alloc, usable, cells)
}

/// SQLite index-key comparison: NULL < numbers < TEXT < BLOB, then
/// per-column memcmp (or NOCASE), flipped for DESC columns. Trailing
/// columns (the rowid / PK suffix) compare ascending.
pub fn compare_index_keys(
    a: &[Value],
    b: &[Value],
    desc: &[bool],
    collations: &[Collation],
) -> std::cmp::Ordering {
    use std::cmp::Ordering as O;
    let n = a.len().min(b.len());
    for i in 0..n {
        let mut cmp = compare_values(&a[i], &b[i]);
        if let Some(Collation::NoCase) = collations.get(i) {
            if let (Value::Text(x), Value::Text(y)) = (&a[i], &b[i]) {
                cmp = compare_nocase(x.as_str(), y.as_str());
            }
        }
        if i < desc.len() && desc[i] {
            cmp = cmp.reverse();
        }
        if cmp != O::Equal {
            return cmp;
        }
    }
    a.len().cmp(&b.len())
}

fn compare_values(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering as O;
    let class = |v: &Value| match v {
        Value::Null => 0u8,
        Value::Integer(_) | Value::Real(_) => 1,
        Value::Text(_) => 2,
        Value::Blob(_) => 3,
    };
    let (ca, cb) = (class(a), class(b));
    if ca != cb {
        return ca.cmp(&cb);
    }
    match (a, b) {
        (Value::Null, Value::Null) => O::Equal,
        (Value::Integer(x), Value::Integer(y)) => x.cmp(y),
        (Value::Real(x), Value::Real(y)) => x.partial_cmp(y).unwrap_or(O::Equal),
        (Value::Integer(x), Value::Real(y)) => (*x as f64).partial_cmp(y).unwrap_or(O::Equal),
        (Value::Real(x), Value::Integer(y)) => x.partial_cmp(&(*y as f64)).unwrap_or(O::Equal),
        (Value::Text(x), Value::Text(y)) => x.as_str().as_bytes().cmp(y.as_str().as_bytes()),
        (Value::Blob(x), Value::Blob(y)) => x.cmp(y),
        _ => O::Equal,
    }
}

fn compare_nocase(a: &str, b: &str) -> std::cmp::Ordering {
    // ASCII-only fold, matching SQLite's built-in NOCASE collation.
    let ab = a.as_bytes();
    let bb = b.as_bytes();
    let n = ab.len().min(bb.len());
    for i in 0..n {
        let x = ab[i].to_ascii_lowercase();
        let y = bb[i].to_ascii_lowercase();
        if x != y {
            return x.cmp(&y);
        }
    }
    ab.len().cmp(&bb.len())
}

// ---------------------------------------------------------------------------
// Level packing — table trees (separators are rowid bounds, copied up)
// ---------------------------------------------------------------------------

fn pack_table_tree(
    alloc: &mut PageAllocator,
    usable: usize,
    cells: Vec<Cell>,
    fixed_root: Option<u32>,
) -> Result<u32, String> {
    // ---- Leaf level ----
    // When the root is fixed at page 1 and the tree stays a single leaf,
    // that leaf lives at offset 100 — budget the extra header bytes for
    // EVERY leaf decision in that case (only the schema tree).
    let leaf_hdr = if fixed_root.is_some() { 108 } else { 8 };
    let mut leaves: Vec<(Vec<Cell>, i64)> = Vec::new(); // (cells, max rowid)
    let mut current: Vec<Cell> = Vec::new();
    let mut cur_bytes = 0usize;
    let mut max_rowid = 0i64;
    for cell in cells {
        let sz = cell.head.len();
        let ptr_cost = 2 * (current.len() + 1);
        let overhead = leaf_hdr + ptr_cost;
        if !current.is_empty() && cur_bytes + sz + overhead > usable {
            leaves.push((std::mem::take(&mut current), max_rowid));
            cur_bytes = 0;
        }
        max_rowid = cell.rowid;
        current.push(cell);
        cur_bytes += sz;
    }
    if !current.is_empty() {
        leaves.push((current, max_rowid));
    }

    if leaves.len() <= 1 {
        let (cells, _) = leaves.into_iter().next().unwrap_or_default();
        return Ok(write_leaf_page(alloc, 0x0d, cells, fixed_root));
    }

    // Children + separators: separator k (between leaf k and k+1) is the
    // max rowid of leaf k — it stays in the leaf and is COPIED up.
    let n_leaves = leaves.len();
    let mut children: Vec<u32> = Vec::with_capacity(n_leaves);
    let mut seps: Vec<i64> = Vec::with_capacity(n_leaves - 1);
    for (idx, (cells, max_rowid)) in leaves.into_iter().enumerate() {
        // With >1 leaf the root is an interior page (written below), so
        // all leaves are normal pages.
        let page = write_leaf_page(alloc, 0x0d, cells, None);
        children.push(page);
        if idx + 1 < n_leaves {
            seps.push(max_rowid);
        }
    }
    pack_interior_table(alloc, usable, children, seps, fixed_root)
}

fn pack_interior_table(
    alloc: &mut PageAllocator,
    usable: usize,
    children: Vec<u32>,
    seps: Vec<i64>,
    fixed_root: Option<u32>,
) -> Result<u32, String> {
    // Group children so each page fits: child i brings cell
    // (children[i], seps[i]) when i < seps.len(); the group's LAST child
    // is the right-most pointer. Groups must hold >= 2 children (so the
    // page has >= 1 cell); a trailing singleton merges back.
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut cur: Vec<usize> = Vec::new();
    let mut cur_bytes = 0usize;
    for i in 0..children.len() {
        let cell_size = if i < seps.len() {
            4 + varint_space(seps[i])
        } else {
            0
        };
        let ptr_cost = 2 * (cur.len() + usize::from(cell_size > 0));
        let overhead = 12 + ptr_cost;
        if !cur.is_empty() && cur_bytes + cell_size + overhead > usable {
            groups.push(std::mem::take(&mut cur));
            cur_bytes = 0;
        }
        cur.push(i);
        cur_bytes += cell_size;
    }
    if !cur.is_empty() {
        groups.push(cur);
    }
    // Merge a trailing singleton group into its predecessor (an interior
    // page with zero cells is degenerate).
    if groups.len() > 1 {
        if let Some(last) = groups.last() {
            if last.len() == 1 {
                let solo = groups.pop().unwrap();
                groups.last_mut().unwrap().extend(solo);
            }
        }
    }

    // The root is this level ONLY when a single page results.
    let single = groups.len() == 1;
    let mut next_children: Vec<u32> = Vec::new();
    let mut next_seps: Vec<i64> = Vec::new();
    for (gi, group) in groups.iter().enumerate() {
        let root_here = single && gi == 0;
        let fixed = if root_here { fixed_root } else { None };
        let page = write_interior_table_page(
            alloc,
            usable,
            group,
            &children,
            &seps,
            fixed_root.is_some() && root_here,
            fixed,
        )?;
        next_children.push(page);
        // Separator pushed to the parent: the separator FOLLOWING this
        // group's last child (copied up — it also remains this page's
        // last cell key).
        let last_child = *group.last().unwrap();
        if last_child < seps.len() {
            next_seps.push(seps[last_child]);
        }
    }
    if single {
        return Ok(next_children[0]);
    }
    pack_interior_table(alloc, usable, next_children, next_seps, fixed_root)
}

fn varint_space(v: i64) -> usize {
    let mut tmp = Vec::new();
    write_varint(&mut tmp, v);
    tmp.len()
}

// ---------------------------------------------------------------------------
// Level packing — index trees (separators are real entries, pushed up)
// ---------------------------------------------------------------------------

fn pack_index_tree(
    alloc: &mut PageAllocator,
    usable: usize,
    cells: Vec<Cell>,
) -> Result<u32, String> {
    // ---- Leaf level ----
    // On each split the boundary cell (the group's LAST) is REMOVED and
    // becomes the separator; the leaf keeps every cell before it.
    let mut leaves: Vec<Vec<Cell>> = Vec::new();
    let mut seps: Vec<Cell> = Vec::new();
    let mut current: Vec<Cell> = Vec::new();
    let mut cur_bytes = 0usize;
    for cell in cells {
        let sz = cell.head.len();
        let ptr_cost = 2 * (current.len() + 1);
        let overhead = 8 + ptr_cost;
        if !current.is_empty() && cur_bytes + sz + overhead > usable {
            if current.len() >= 2 {
                let sep = current.pop().unwrap();
                seps.push(sep);
            }
            // A lone cell that cannot coexist with the next is impossible
            // after overflow encoding (max local ~ usable/4); if it ever
            // happens, flush the singleton leaf without a separator —
            // detectable below because leaves.len() outgrows seps.
            leaves.push(std::mem::take(&mut current));
            cur_bytes = 0;
        }
        current.push(cell);
        cur_bytes += sz;
    }
    if !current.is_empty() {
        leaves.push(current);
    }
    if leaves.len() > seps.len() + 1 {
        return Err("index leaf packing produced adjacent leaves without a separator".into());
    }

    if leaves.len() == 1 {
        return Ok(write_leaf_page(
            alloc,
            0x0a,
            leaves.into_iter().next().unwrap(),
            None,
        ));
    }

    let mut children: Vec<u32> = Vec::with_capacity(leaves.len());
    for cells in &leaves {
        children.push(write_leaf_page(alloc, 0x0a, cells.clone(), None));
    }
    pack_interior_index(alloc, usable, children, seps)
}

fn pack_interior_index(
    alloc: &mut PageAllocator,
    usable: usize,
    children: Vec<u32>,
    seps: Vec<Cell>,
) -> Result<u32, String> {
    // children[i] is followed by seps[i] (when i < seps.len()). A group
    // [a..=b] becomes a page holding cells (children[a], seps[a]) ..
    // (children[b-1], seps[b-1]) with right-most pointer children[b];
    // seps[b] (the separator FOLLOWING the group) is pushed to the next
    // level and never stored on this page.
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut cur: Vec<usize> = Vec::new();
    let mut cur_bytes = 0usize;
    for i in 0..children.len() {
        let cell_size = if i < seps.len() {
            4 + seps[i].head.len()
        } else {
            0
        };
        let ptr_cost = 2 * (cur.len() + usize::from(cell_size > 0));
        let overhead = 12 + ptr_cost;
        if !cur.is_empty() && cur_bytes + cell_size + overhead > usable {
            groups.push(std::mem::take(&mut cur));
            cur_bytes = 0;
        }
        cur.push(i);
        cur_bytes += cell_size;
    }
    if !cur.is_empty() {
        groups.push(cur);
    }
    // Merge a trailing singleton into the predecessor (page would have
    // zero cells otherwise).
    if groups.len() > 1 {
        if let Some(last) = groups.last() {
            if last.len() == 1 {
                let solo = groups.pop().unwrap();
                groups.last_mut().unwrap().extend(solo);
            }
        }
    }

    let single = groups.len() == 1;
    let mut next_children: Vec<u32> = Vec::new();
    let mut next_seps: Vec<Cell> = Vec::new();
    for group in &groups {
        let page = write_interior_index_page(alloc, usable, group, &children, &seps)?;
        next_children.push(page);
        let last_child = *group.last().unwrap();
        if last_child < seps.len() {
            // This separator follows the group's last child — it belongs
            // to the PARENT, not this page.
            next_seps.push(seps[last_child].clone());
        }
    }
    if single {
        return Ok(next_children[0]);
    }
    pack_interior_index(alloc, usable, next_children, next_seps)
}

// ---------------------------------------------------------------------------
// Page writers
// ---------------------------------------------------------------------------

/// Write a leaf page (table 0x0d or index 0x0a). `page1` places the
/// b-tree header at offset 100 and the page number at 1.
fn write_leaf_page(
    alloc: &mut PageAllocator,
    ptype: u8,
    cells: Vec<Cell>,
    fixed: Option<u32>,
) -> u32 {
    let page_size = alloc.page_size as usize;
    let page1 = fixed == Some(1);
    let hoff: usize = if page1 { 100 } else { 0 };
    let n = cells.len();
    // Resolve overflow chains first (head size is unaffected).
    let final_cells: Vec<Vec<u8>> = cells.into_iter().map(|c| finish_cell(alloc, c)).collect();
    let mut offsets = Vec::with_capacity(n);
    let mut content = page_size;
    for cell in &final_cells {
        content -= cell.len();
        offsets.push(content);
    }
    let page_no = fixed.unwrap_or_else(|| alloc.alloc());
    let mut page = vec![0u8; page_size];
    page[hoff] = ptype;
    page[hoff + 1..hoff + 3].copy_from_slice(&0u16.to_be_bytes()); // first freeblock
    page[hoff + 3..hoff + 5].copy_from_slice(&(n as u16).to_be_bytes());
    // Content start: 0 encodes 65536; otherwise the raw offset.
    let cs = if content >= 65536 {
        0u16
    } else {
        content as u16
    };
    page[hoff + 5..hoff + 7].copy_from_slice(&cs.to_be_bytes());
    page[hoff + 7] = 0; // fragmented free bytes
    for (i, off) in offsets.iter().enumerate() {
        let cp = hoff + 8 + i * 2;
        page[cp..cp + 2].copy_from_slice(&(*off as u16).to_be_bytes());
    }
    for (i, cell) in final_cells.iter().enumerate() {
        page[offsets[i]..offsets[i] + cell.len()].copy_from_slice(cell);
    }
    alloc.store(page_no, page);
    page_no
}

/// Write an interior TABLE page (type 0x05).
fn write_interior_table_page(
    alloc: &mut PageAllocator,
    _usable: usize,
    group: &[usize],
    children: &[u32],
    seps: &[i64],
    page1: bool,
    fixed: Option<u32>,
) -> Result<u32, String> {
    let page_size = alloc.page_size as usize;
    let hoff: usize = if page1 { 100 } else { 0 };
    // Cells for every child except the group's last (right-most ptr).
    let mut cells: Vec<Vec<u8>> = Vec::with_capacity(group.len().saturating_sub(1));
    for &i in &group[..group.len() - 1] {
        let mut c = Vec::with_capacity(4 + 9);
        c.extend_from_slice(&children[i].to_be_bytes());
        write_varint(&mut c, seps[i]);
        cells.push(c);
    }
    let right = children[*group.last().unwrap()];
    let n = cells.len();
    let mut offsets = Vec::with_capacity(n);
    let mut content = page_size;
    for cell in &cells {
        content -= cell.len();
        offsets.push(content);
    }
    let page_no = fixed.unwrap_or_else(|| alloc.alloc());
    let mut page = vec![0u8; page_size];
    page[hoff] = 0x05;
    page[hoff + 1..hoff + 3].copy_from_slice(&0u16.to_be_bytes());
    page[hoff + 3..hoff + 5].copy_from_slice(&(n as u16).to_be_bytes());
    let cs = if content >= 65536 {
        0u16
    } else {
        content as u16
    };
    page[hoff + 5..hoff + 7].copy_from_slice(&cs.to_be_bytes());
    page[hoff + 7] = 0;
    page[hoff + 8..hoff + 12].copy_from_slice(&right.to_be_bytes());
    for (i, off) in offsets.iter().enumerate() {
        let cp = hoff + 12 + i * 2;
        page[cp..cp + 2].copy_from_slice(&(*off as u16).to_be_bytes());
    }
    for (i, cell) in cells.iter().enumerate() {
        page[offsets[i]..offsets[i] + cell.len()].copy_from_slice(cell);
    }
    alloc.store(page_no, page);
    Ok(page_no)
}

/// Write an interior INDEX page (type 0x02). The group's last child is
/// the right-most pointer; every earlier child contributes
/// `(left child, separator entry)` cell.
fn write_interior_index_page(
    alloc: &mut PageAllocator,
    _usable: usize,
    group: &[usize],
    children: &[u32],
    seps: &[Cell],
) -> Result<u32, String> {
    let page_size = alloc.page_size as usize;
    let hoff = 0usize;
    let mut cell_specs: Vec<(usize, &Cell)> = Vec::with_capacity(group.len().saturating_sub(1));
    for &i in &group[..group.len() - 1] {
        cell_specs.push((i, &seps[i]));
    }
    let right = children[*group.last().unwrap()];
    // Materialize cell bytes (4-byte child prefix + separator head with
    // any overflow chain resolved).
    let mut cells: Vec<Vec<u8>> = Vec::with_capacity(cell_specs.len());
    for (i, sep) in cell_specs {
        let mut c = Vec::with_capacity(4 + sep.head.len());
        c.extend_from_slice(&children[i].to_be_bytes());
        c.extend_from_slice(&sep.head);
        // The separator's own overflow chain (if any) must be attached
        // here — clone the cell and finish it against this page context.
        if sep.ptr_pos.is_some() && !sep.tail.is_empty() {
            // Rebuild through finish_cell on a clone.
            let clone = Cell {
                head: c,
                tail: sep.tail.clone(),
                ptr_pos: sep.ptr_pos.map(|p| p + 4),
                rowid: 0,
            };
            cells.push(finish_cell(alloc, clone));
        } else {
            cells.push(c);
        }
    }
    let n = cells.len();
    let mut offsets = Vec::with_capacity(n);
    let mut content = page_size;
    for cell in &cells {
        content -= cell.len();
        offsets.push(content);
    }
    let page_no = alloc.alloc();
    let mut page = vec![0u8; page_size];
    page[0] = 0x02;
    page[1..3].copy_from_slice(&0u16.to_be_bytes());
    page[3..5].copy_from_slice(&(n as u16).to_be_bytes());
    let cs = if content >= 65536 {
        0u16
    } else {
        content as u16
    };
    page[5..7].copy_from_slice(&cs.to_be_bytes());
    page[7] = 0;
    page[8..12].copy_from_slice(&right.to_be_bytes());
    for (i, off) in offsets.iter().enumerate() {
        let cp = hoff + 12 + i * 2;
        page[cp..cp + 2].copy_from_slice(&(*off as u16).to_be_bytes());
    }
    for (i, cell) in cells.iter().enumerate() {
        page[offsets[i]..offsets[i] + cell.len()].copy_from_slice(cell);
    }
    alloc.store(page_no, page);
    Ok(page_no)
}

// ---------------------------------------------------------------------------
// sqlite_schema tree
// ---------------------------------------------------------------------------

struct SchemaCell {
    rowid: i64,
    kind: String,
    name: String,
    tbl_name: String,
    root: u32,
    sql: Option<String>,
}

/// Build the sqlite_schema tree with its root FIXED at page 1.
fn build_schema_tree(
    alloc: &mut PageAllocator,
    usable: usize,
    cells: &[SchemaCell],
) -> Result<u32, String> {
    let mut table_cells: Vec<Cell> = Vec::with_capacity(cells.len());
    for c in cells {
        let sql_val = match &c.sql {
            Some(s) => Value::Text(crate::types::text::Text::from(s.as_str())),
            None => Value::Null,
        };
        let vals = vec![
            Value::Text(crate::types::text::Text::from(c.kind.as_str())),
            Value::Text(crate::types::text::Text::from(c.name.as_str())),
            Value::Text(crate::types::text::Text::from(c.tbl_name.as_str())),
            Value::Integer(c.root as i64),
            sql_val,
        ];
        let payload = encode_record(&vals);
        table_cells.push(make_table_leaf_cell(usable, c.rowid, &payload));
    }
    pack_table_tree(alloc, usable, table_cells, Some(1))
}
