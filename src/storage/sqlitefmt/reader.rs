//! SQLite-format file READER: parses a real SQLite `.db` (and its WAL
//! sidecar, when present) into engine values.
//!
//! Scope of milestone 1: table b-trees (rowid + WITHOUT ROWID) are fully
//! decoded; index b-trees are NOT read (the engine rebuilds every index
//! from the table rows during load). Freelist pages are never touched;
//! pointer-map (auto-vacuum) pages are likewise irrelevant because only
//! b-tree pages named by `sqlite_schema` rootpages are visited.

use std::collections::HashMap;
use std::path::Path;

use super::header::FileHeaderInfo;
use super::record::{decode_record_enc, TextEnc};
use super::varint::read_varint;
use crate::types::value::Value;

pub(crate) const PAGE_INTERIOR_INDEX: u8 = 0x02;
pub(crate) const PAGE_INTERIOR_TABLE: u8 = 0x05;
pub(crate) const PAGE_LEAF_INDEX: u8 = 0x0a;
pub(crate) const PAGE_LEAF_TABLE: u8 = 0x0d;

/// One `sqlite_schema` row, in creation (rowid) order.
#[derive(Clone, Debug)]
pub struct SchemaRow {
    pub rowid: i64,
    /// "table" | "index" | "view" | "trigger"
    pub kind: String,
    pub name: String,
    pub tbl_name: String,
    pub rootpage: u32,
    pub sql: Option<String>,
}

/// A decoded table row: `(rowid, values)`. WITHOUT ROWID tables carry
/// `rowid = 0` and the full record (PK columns first).
#[derive(Clone, Debug)]
pub struct RawRow {
    pub rowid: i64,
    pub values: Vec<Value>,
}

/// A parsed SQLite database image.
#[derive(Clone, Debug, Default)]
pub struct SqliteDbImage {
    pub page_size: u32,
    pub user_version: u32,
    pub application_id: u32,
    /// File text encoding (header field 56): UTF-8 / UTF-16le / UTF-16be.
    /// Every TEXT value — records AND sqlite_schema — was read in it.
    pub text_enc: TextEnc,
    pub schema: Vec<SchemaRow>,
    /// Table rows keyed by table name (lowercased).
    pub table_rows: HashMap<String, Vec<RawRow>>,
    /// `sqlite_sequence` contents (table name lowercased -> seq).
    pub sequences: HashMap<String, i64>,
    /// True when a `-wal` sidecar contributed frames to the read view.
    pub had_wal: bool,
    /// Auto-vacuum mode of the source file (header 52/64): 0 = none,
    /// 1 = FULL, 2 = INCREMENTAL. Preserved across engine rewrites so
    /// the output file keeps its pointer-map structure.
    pub auto_vacuum: u8,
}

/// A page view over the file + applied WAL frames.
struct PageSource {
    data: Vec<u8>,
    page_size: u32,
    /// Pages overridden by WAL frames (page number -> page bytes).
    overrides: HashMap<u32, Vec<u8>>,
    n_pages: u32,
}

impl PageSource {
    fn page(&self, n: u32) -> Result<&[u8], String> {
        if n == 0 || n > self.n_pages {
            return Err(format!("page {n} out of range ({} pages)", self.n_pages));
        }
        if let Some(p) = self.overrides.get(&n) {
            return Ok(p);
        }
        let off = (n as usize - 1) * self.page_size as usize;
        let end = off + self.page_size as usize;
        if end > self.data.len() {
            return Err(format!("page {n} truncated in file"));
        }
        Ok(&self.data[off..end])
    }
}

/// Read a SQLite file (plus WAL sidecar when present) into an image.
pub fn read_sqlite_file(path: &Path) -> Result<SqliteDbImage, String> {
    let data = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    if data.len() < 100 {
        return Err("file smaller than the 100-byte SQLite header".into());
    }
    let hdr = FileHeaderInfo::parse(&data)?;
    let text_enc = TextEnc::from_u32(hdr.text_encoding)?;
    // Auto-vacuum mode: enabled iff largest-root (header 52) is set;
    // INCREMENTAL when the incremental flag (header 64) is set.
    let auto_vacuum: u8 = if hdr.largest_root_btree != 0 {
        if data[64..68] != [0, 0, 0, 0] {
            2
        } else {
            1
        }
    } else {
        0
    };
    if !(hdr.reserved as u32) < hdr.page_size && hdr.reserved != 0 {
        return Err(format!("invalid reserved-bytes-per-page {}", hdr.reserved));
    }
    let file_pages = (data.len() / hdr.page_size as usize) as u32;
    // Trust the header's size only when it is sane vs the file.
    let n_pages = if hdr.db_size_pages != 0 && hdr.db_size_pages <= file_pages {
        hdr.db_size_pages
    } else {
        file_pages
    };

    // --- WAL sidecar: apply the last committed frames on top.
    let mut overrides: HashMap<u32, Vec<u8>> = HashMap::new();
    let mut had_wal = false;
    let wal_path = wal_path_of(path);
    if let Ok(wal) = std::fs::read(&wal_path) {
        if wal.len() >= 32 {
            match apply_wal(&wal, hdr.page_size, n_pages, &mut overrides) {
                Ok(applied) => had_wal = applied,
                Err(e) => {
                    // An unreadable WAL is ignored: the main file view is
                    // still a consistent pre-WAL snapshot (the writer
                    // failed before any commit frame reached the log).
                    let _ = e;
                }
            }
        }
    }

    let src = PageSource {
        data,
        page_size: hdr.page_size,
        overrides,
        n_pages,
    };

    let usable = hdr.usable();
    let mut image = SqliteDbImage {
        page_size: hdr.page_size,
        user_version: hdr.user_version,
        application_id: hdr.application_id,
        text_enc,
        schema: Vec::new(),
        table_rows: HashMap::new(),
        sequences: HashMap::new(),
        had_wal,
        auto_vacuum,
    };

    // --- Walk the schema b-tree (root page 1, header at offset 100).
    // sqlite_schema text (names, SQL) is stored in the FILE's encoding.
    let schema_rows = walk_table_tree(&src, usable, 1, true)?;
    for (rowid, payload) in schema_rows {
        let mut vals = decode_record_enc(&payload, 5, text_enc)
            .map_err(|e| format!("sqlite_schema row: {e}"))?;
        if vals.len() < 5 {
            return Err("sqlite_schema row shorter than 5 columns".into());
        }
        let kind = take_text(&mut vals[0])?;
        let name = take_text(&mut vals[1])?;
        let tbl_name = take_text(&mut vals[2])?;
        let rootpage = match &vals[3] {
            Value::Integer(i) => *i as u32,
            _ => 0,
        };
        let sql = match vals.swap_remove(4) {
            Value::Null => None,
            Value::Text(t) => Some(t.as_str().to_string()),
            other => return Err(format!("sqlite_schema.sql has bad type: {other:?}")),
        };
        image.schema.push(SchemaRow {
            rowid,
            kind,
            name,
            tbl_name,
            rootpage,
            sql,
        });
    }

    // --- Read every ordinary table's rows.
    for row in &image.schema {
        if row.kind != "table" {
            continue;
        }
        let name_lc = row.name.to_ascii_lowercase();
        if row.rootpage == 0 {
            // CREATE VIRTUAL TABLE entries have no b-tree.
            continue;
        }
        if name_lc == "sqlite_sequence" {
            // Read as data (below), but routed into `sequences`.
            let rows = walk_table_tree(&src, usable, row.rootpage, false)?;
            let mut seqs = Vec::new();
            for (_, payload) in rows {
                let vals = decode_record_enc(&payload, 2, text_enc)
                    .map_err(|e| format!("sqlite_sequence row: {e}"))?;
                if vals.len() == 2 {
                    if let (Value::Text(n), Value::Integer(s)) = (&vals[0], &vals[1]) {
                        seqs.push((n.as_str().to_ascii_lowercase(), *s));
                    }
                }
            }
            image.sequences.extend(seqs);
            continue;
        }
        if name_lc.starts_with("sqlite_stat") || name_lc.starts_with("sqlite_") {
            // sqlite_stat1/4: read as an ordinary queryable table.
        }
        let rows = walk_table_tree(&src, usable, row.rootpage, false)?;
        let mut out = Vec::with_capacity(rows.len());
        for (rowid, payload) in rows {
            // Column count is unknown here; decode with the payload's own
            // column count (the record header defines it). The api layer
            // re-normalizes to the catalog's column count.
            let n = record_column_count(&payload);
            let vals = decode_record_enc(&payload, n, text_enc)
                .map_err(|e| format!("table {} row {}: {e}", row.name, rowid))?;
            out.push(RawRow {
                rowid,
                values: vals,
            });
        }
        image.table_rows.insert(name_lc, out);
    }

    Ok(image)
}

fn take_text(v: &mut Value) -> Result<String, String> {
    match v {
        Value::Text(t) => Ok(t.as_str().to_string()),
        other => Err(format!("expected TEXT, got {other:?}")),
    }
}

/// Number of values a record payload carries (its serial-type count).
fn record_column_count(payload: &[u8]) -> usize {
    let (header_size, mut off) = match read_varint(payload, 0) {
        Some(x) => x,
        None => return 0,
    };
    let header_size = header_size as usize;
    let mut n = 0usize;
    while off < header_size.min(payload.len()) {
        match read_varint(payload, off) {
            Some((_, used)) => {
                off += used;
                n += 1;
            }
            None => break,
        }
    }
    n
}

/// Walk a TABLE b-tree in rowid order, returning `(rowid, payload)` for
/// every leaf cell, with overflow chains assembled.
fn walk_table_tree(
    src: &PageSource,
    usable: u32,
    root: u32,
    is_schema: bool,
) -> Result<Vec<(i64, Vec<u8>)>, String> {
    let mut out = Vec::new();
    walk_table_page(src, usable, root, is_schema, &mut out)?;
    Ok(out)
}

/// Recursive in-order walk of one page.
fn walk_table_page(
    src: &PageSource,
    usable: u32,
    page_no: u32,
    header_offset: bool,
    out: &mut Vec<(i64, Vec<u8>)>,
) -> Result<(), String> {
    let page = src.page(page_no)?;
    let hoff: usize = if header_offset { 100 } else { 0 };
    if page.len() < hoff + 8 {
        return Err(format!("page {page_no} shorter than its b-tree header"));
    }
    let ptype = page[hoff];
    let n_cells = u16::from_be_bytes([page[hoff + 3], page[hoff + 4]]) as usize;
    match ptype {
        PAGE_LEAF_TABLE => {
            let cp_arr = hoff + 8;
            for i in 0..n_cells {
                let cp = read_cp(page, cp_arr + i * 2)?;
                let (payload_len, used) = read_varint(page, cp)
                    .ok_or_else(|| format!("page {page_no} cell {i}: varint payload"))?;
                let (rowid, used2) = read_varint(page, cp + used)
                    .ok_or_else(|| format!("page {page_no} cell {i}: varint rowid"))?;
                let payload = assemble_payload(
                    src,
                    usable,
                    page,
                    cp + used + used2,
                    payload_len as usize,
                    false,
                )?;
                out.push((rowid, payload));
            }
            Ok(())
        }
        PAGE_INTERIOR_TABLE => {
            let cp_arr = hoff + 12;
            for i in 0..n_cells {
                let cp = read_cp(page, cp_arr + i * 2)?;
                if cp + 4 > page.len() {
                    return Err(format!("page {page_no} interior cell {i} truncated"));
                }
                let child =
                    u32::from_be_bytes([page[cp], page[cp + 1], page[cp + 2], page[cp + 3]]);
                walk_table_page(src, usable, child, false, out)?;
            }
            let right = u32::from_be_bytes([
                page[hoff + 8],
                page[hoff + 9],
                page[hoff + 10],
                page[hoff + 11],
            ]);
            walk_table_page(src, usable, right, false, out)
        }
        PAGE_LEAF_INDEX | PAGE_INTERIOR_INDEX => {
            // A WITHOUT ROWID table stored as an index b-tree. Its cells
            // carry records directly (no rowid key).
            walk_index_as_table(src, usable, page_no, ptype, hoff, n_cells, out)
        }
        other => Err(format!(
            "page {page_no}: unexpected b-tree page type 0x{other:02x}"
        )),
    }
}

/// WITHOUT ROWID tables live in index-shaped b-trees: every cell payload
/// is a full record (PK columns first).
fn walk_index_as_table(
    src: &PageSource,
    usable: u32,
    page_no: u32,
    ptype: u8,
    hoff: usize,
    n_cells: usize,
    out: &mut Vec<(i64, Vec<u8>)>,
) -> Result<(), String> {
    let page = src.page(page_no)?;
    let interior = ptype == PAGE_INTERIOR_INDEX;
    let cp_arr = hoff + if interior { 12 } else { 8 };
    for i in 0..n_cells {
        let cp = read_cp(page, cp_arr + i * 2)?;
        let mut body = cp;
        if interior {
            if cp + 4 > page.len() {
                return Err(format!("page {page_no} index cell {i} truncated"));
            }
            let child = u32::from_be_bytes([page[cp], page[cp + 1], page[cp + 2], page[cp + 3]]);
            walk_table_page(src, usable, child, false, out)?;
            body = cp + 4;
        }
        let (payload_len, used) = read_varint(page, body)
            .ok_or_else(|| format!("page {page_no} index cell {i}: varint"))?;
        let payload = assemble_payload(src, usable, page, body + used, payload_len as usize, true)?;
        // rowid is meaningless for WITHOUT ROWID entries: 0.
        out.push((0, payload));
    }
    if interior {
        let right = u32::from_be_bytes([
            page[hoff + 8],
            page[hoff + 9],
            page[hoff + 10],
            page[hoff + 11],
        ]);
        walk_table_page(src, usable, right, false, out)?;
    }
    Ok(())
}

fn read_cp(page: &[u8], off: usize) -> Result<usize, String> {
    if off + 2 > page.len() {
        return Err(format!("cell pointer at {off} out of page"));
    }
    Ok(u16::from_be_bytes([page[off], page[off + 1]]) as usize)
}

/// Assemble a cell payload, following the overflow chain when the local
/// prefix is short. `index_page` selects the index-cell local formula.
#[allow(clippy::too_many_arguments)]
fn assemble_payload(
    src: &PageSource,
    usable: u32,
    page: &[u8],
    body: usize,
    total: usize,
    index_page: bool,
) -> Result<Vec<u8>, String> {
    let u = usable as usize;
    let x = if index_page {
        ((u - 12) * 64 / 255) - 23
    } else {
        u - 35
    };
    if total <= x {
        if body + total > page.len() {
            return Err("cell body truncated in page".into());
        }
        return Ok(page[body..body + total].to_vec());
    }
    let m = ((u - 12) * 32 / 255) - 23;
    let k = m + ((total - m) % (u - 4));
    let local = if k <= x { k } else { m };
    if body + local + 4 > page.len() {
        return Err("overflow cell prefix truncated".into());
    }
    let mut payload = Vec::with_capacity(total);
    payload.extend_from_slice(&page[body..body + local]);
    let mut next = u32::from_be_bytes([
        page[body + local],
        page[body + local + 1],
        page[body + local + 2],
        page[body + local + 3],
    ]);
    let mut remaining = total - local;
    let mut guard = 0u32;
    while next != 0 && remaining > 0 {
        guard += 1;
        if guard > 1_000_000 {
            return Err("overflow chain too long (loop?)".into());
        }
        let opage = src.page(next)?;
        if opage.len() < 4 {
            return Err(format!("overflow page {next} too small"));
        }
        let take = (u - 4).min(remaining);
        payload.extend_from_slice(&opage[4..4 + take]);
        remaining -= take;
        next = u32::from_be_bytes([opage[0], opage[1], opage[2], opage[3]]);
    }
    if remaining != 0 {
        return Err("overflow chain ended before payload was complete".into());
    }
    Ok(payload)
}

/// Apply the last-committed frames of a SQLite WAL file to `overrides`.
/// Returns whether any frame was applied. Checksums are NOT verified
/// (the reader trusts a cleanly-closed WAL; a torn tail simply stops the
/// replay at the previous commit boundary).
fn apply_wal(
    wal: &[u8],
    page_size: u32,
    _n_pages: u32,
    overrides: &mut HashMap<u32, Vec<u8>>,
) -> Result<bool, String> {
    let magic = u32::from_be_bytes([wal[0], wal[1], wal[2], wal[3]]);
    if magic != 0x377f0682 && magic != 0x377f0683 {
        return Err("bad WAL magic".into());
    }
    let wal_page_size = u32::from_be_bytes([wal[8], wal[9], wal[10], wal[11]]);
    if wal_page_size != page_size {
        return Err("WAL page size mismatch".into());
    }
    let frame_size = 24 + page_size as usize;
    let mut off = 32usize;
    // Frames between commit markers are speculative; only replay pages
    // from a prefix ending in a commit frame.
    let mut pending: HashMap<u32, Vec<u8>> = HashMap::new();
    let mut applied_any = false;
    while off + frame_size <= wal.len() {
        let hdr = &wal[off..off + 24];
        let page_no = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
        let db_size = u32::from_be_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
        // Salt check: frames from a previous WAL generation (after a
        // checkpoint reset) stop the replay.
        let salt1 = u32::from_be_bytes([hdr[8], hdr[9], hdr[10], hdr[11]]);
        let file_salt1 = u32::from_be_bytes([wal[16], wal[17], wal[18], wal[19]]);
        if salt1 != file_salt1 {
            break;
        }
        let frame = &wal[off + 24..off + frame_size];
        if page_no == 0 {
            break; // corrupt frame
        }
        pending.insert(page_no, frame.to_vec());
        if db_size != 0 {
            // Commit frame: the pending set becomes durable.
            overrides.extend(pending.drain());
            applied_any = true;
        }
        off += frame_size;
    }
    Ok(applied_any)
}

/// `<path>-wal` sidecar path.
pub fn wal_path_of(path: &Path) -> std::path::PathBuf {
    let mut p = path.as_os_str().to_os_string();
    p.push("-wal");
    std::path::PathBuf::from(p)
}

/// Quick magic sniff used by `Database::open`.
pub fn is_sqlite_file(path: &Path) -> bool {
    use std::io::Read;
    let mut buf = [0u8; 16];
    match std::fs::File::open(path) {
        Ok(mut f) => match f.read(&mut buf) {
            Ok(16) => super::header::has_magic(&buf),
            _ => false,
        },
        Err(_) => false,
    }
}
