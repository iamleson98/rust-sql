//! The changeset/patchset iterator — SQLite's `sqlite3_changeset_iter`
//! (`sqlite3changeset_start[_v2]` / `_next` / `_op` / `_pk` / `_old` /
//! `_new` / `_finalize`), including the INVERT and patchset semantics of
//! `sessionChangesetNextOne`.
//!
//! The iterator pre-parses each change's old.*/new.* records into
//! per-column slots (`apValue[]` in the C), with `None` for "undefined"
//! (0x00) fields. `raw` accessors expose the serialized record bytes for
//! the buffer-transform passes (changegroup / rebase), mirroring the
//! iterator's `paRec`/`nRec` mode.

pub(crate) use super::codec::*;

/// Start flags (`sqlite3changeset_start_v2`).
pub const CHANGESETSTART_INVERT: u32 = 0x0002;

/// The iteration error type — a sticky message (SQLite error codes
/// collapse to "an error occurred" text in our engine mapping).
pub type IterError = &'static str;

/// The input buffer: borrowed (apply / group paths) or owned (the C-ABI
/// iterator, which must own its copy).
enum IterData<'a> {
    Borrowed(&'a [u8]),
    Owned(Vec<u8>),
}

impl<'a> std::ops::Deref for IterData<'a> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            IterData::Borrowed(b) => b,
            IterData::Owned(v) => v,
        }
    }
}

/// One changeset iterator. All parsing is bounds-checked (corrupt
/// changesets error, never panic).
pub struct ChangesetIter<'a> {
    data: IterData<'a>,
    pos: usize,
    /// Byte offset where the CURRENT change's records start (after the
    /// op/indirect bytes) — `iCurrent`; `end` is `iNext`.
    cur: usize,
    end: usize,
    pub(crate) b_patchset: bool,
    b_invert: bool,
    // Current table header.
    pub(crate) z_tab: String,
    pub(crate) n_col: usize,
    pub(crate) ab_pk: Vec<u8>,
    // Current change.
    pub(crate) op: u8,
    pub(crate) indirect: bool,
    /// Old-side values (None = undefined). After the patchset PK shift,
    /// PK fields live here for patchset UPDATEs too.
    pub(crate) old: Vec<Option<SessVal>>,
    /// New-side values (None = undefined).
    pub(crate) new: Vec<Option<SessVal>>,
    /// The serialized record bytes of the current change (buffer mode).
    pub(crate) raw: Vec<u8>,
}

impl<'a> ChangesetIter<'a> {
    /// `sqlite3changeset_start`.
    pub fn new(data: &'a [u8]) -> Result<Self, IterError> {
        Self::start(data, 0)
    }

    /// `sqlite3changeset_start` over an OWNED buffer (the C-ABI
    /// iterator).
    pub fn new_owned(data: Vec<u8>) -> Result<ChangesetIter<'static>, IterError> {
        Self::start_owned(data, 0)
    }

    /// `sqlite3changeset_start_v2` — honors CHANGESETSTART_INVERT.
    pub fn start(data: &'a [u8], flags: u32) -> Result<Self, IterError> {
        Self::start_data(IterData::Borrowed(data), flags)
    }

    /// `sqlite3changeset_start_v2` over an owned buffer.
    pub fn start_owned(data: Vec<u8>, flags: u32) -> Result<ChangesetIter<'static>, IterError> {
        Self::start_data(IterData::Owned(data), flags)
    }

    fn start_data<'b>(data: IterData<'b>, flags: u32) -> Result<ChangesetIter<'b>, IterError> {
        Ok(ChangesetIter {
            data,
            pos: 0,
            cur: 0,
            end: 0,
            b_patchset: false,
            b_invert: flags & CHANGESETSTART_INVERT != 0,
            z_tab: String::new(),
            n_col: 0,
            ab_pk: Vec::new(),
            op: 0,
            indirect: false,
            old: Vec::new(),
            new: Vec::new(),
            raw: Vec::new(),
        })
    }

    /// Advance to the next change. `Ok(false)` = DONE.
    ///
    /// (Deliberately NOT the `Iterator::next` shape: a changeset walk
    /// is fallible and done-signalled, and the C ABI's
    /// `sqlite3changeset_next` returns SQLITE_ROW/SQLITE_DONE codes.)
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<bool, IterError> {
        self.old.clear();
        self.new.clear();
        self.raw.clear();
        if self.pos >= self.data.len() {
            return Ok(false);
        }
        let mut op = self.data[self.pos];
        self.pos += 1;
        // Table headers: 'T' (changeset) / 'P' (patchset).
        while op == b'T' || op == b'P' {
            self.b_patchset = op == b'P';
            self.read_tblhdr()?;
            if self.pos >= self.data.len() {
                return Ok(false);
            }
            op = self.data[self.pos];
            self.pos += 1;
        }
        if self.z_tab.is_empty() || (self.b_patchset && self.b_invert) {
            // First record is not a table header — corrupt. (A patchset
            // may not be inverted.)
            return Err("corrupt changeset");
        }
        if op != OP_INSERT && op != OP_DELETE && op != OP_UPDATE {
            return Err("corrupt changeset");
        }
        self.op = op;
        if self.pos >= self.data.len() {
            return Err("corrupt changeset");
        }
        self.indirect = self.data[self.pos] != 0;
        self.pos += 1;
        self.cur = self.pos;

        // Parse the records. In INVERT mode the arrays swap and
        // INSERT/DELETE flip (the UPDATE record order stays old-then-new
        // in the blob; the parsed halves swap roles).
        let (mut op_old, mut op_new) = (Vec::new(), Vec::new());
        if self.b_patchset {
            if op == OP_DELETE {
                // PK fields only.
                let n_pk = self.ab_pk.iter().filter(|p| **p != 0).count();
                let (vals, used) = parse_record(&self.data, self.pos, n_pk)?;
                self.pos += used;
                op_old = vals.into_iter().map(Some).collect();
                // Expand to full width in PK positions.
                let mut full = vec![None; self.n_col];
                let mut k = 0;
                for (i, &pk) in self.ab_pk.iter().enumerate() {
                    if pk != 0 {
                        full[i] = op_old[k].take();
                        k += 1;
                    }
                }
                op_old = full;
            } else {
                // INSERT: full record (new side). UPDATE: one record
                // with PK + modified fields (new side).
                let (vals, used) = parse_record(&self.data, self.pos, self.n_col)?;
                self.pos += used;
                op_new = vals.into_iter().map(Some).collect();
            }
        } else if op == OP_UPDATE {
            let (o, used1) = parse_record(&self.data, self.pos, self.n_col)?;
            self.pos += used1;
            let (n, used2) = parse_record(&self.data, self.pos, self.n_col)?;
            self.pos += used2;
            op_old = o.into_iter().map(Some).collect();
            op_new = n.into_iter().map(Some).collect();
        } else if op == OP_DELETE {
            let (o, used) = parse_record(&self.data, self.pos, self.n_col)?;
            self.pos += used;
            op_old = o.into_iter().map(Some).collect();
        } else {
            // INSERT
            let (n, used) = parse_record(&self.data, self.pos, self.n_col)?;
            self.pos += used;
            op_new = n.into_iter().map(Some).collect();
        }
        self.end = self.pos;
        self.raw = self.data[self.cur..self.end].to_vec();

        if self.b_patchset && op == OP_UPDATE {
            // Shift the PK fields from new.* to old.* (all PK and
            // modified fields are in the single record).
            for i in 0..self.n_col {
                if self.ab_pk[i] != 0 {
                    op_old[i] = op_new[i].take();
                    if op_old[i].is_none() {
                        return Err("corrupt changeset");
                    }
                }
            }
        }

        if self.b_invert {
            std::mem::swap(&mut op_old, &mut op_new);
            if op == OP_INSERT {
                self.op = OP_DELETE;
            } else if op == OP_DELETE {
                self.op = OP_INSERT;
            }
        } else if op == OP_UPDATE {
            // old.* fields that are neither PK nor present in new.* are
            // dropped (the historical rebaser-quirk tolerance).
            for i in 0..self.n_col {
                if self.ab_pk[i] == 0 && op_new[i].is_none() {
                    op_old[i] = None;
                }
            }
        }

        self.old = op_old;
        self.new = op_new;
        Ok(true)
    }

    fn read_tblhdr(&mut self) -> Result<(), IterError> {
        if self.pos >= self.data.len() {
            return Err("corrupt changeset");
        }
        let (n_col, used) = get_varint(&self.data, self.pos);
        self.pos += used;
        let n_col = n_col as usize;
        if n_col == 0 || self.pos + n_col > self.data.len() {
            return Err("corrupt changeset");
        }
        self.ab_pk = self.data[self.pos..self.pos + n_col].to_vec();
        self.pos += n_col;
        let name_end = self.data[self.pos..]
            .iter()
            .position(|&b| b == 0)
            .ok_or("corrupt changeset")?;
        let name = std::str::from_utf8(&self.data[self.pos..self.pos + name_end])
            .map_err(|_| "corrupt changeset")?;
        self.z_tab = name.to_string();
        self.pos += name_end + 1;
        self.n_col = n_col;
        Ok(())
    }

    pub fn op(&self) -> u8 {
        self.op
    }
    pub fn table(&self) -> &str {
        &self.z_tab
    }
    pub fn n_col(&self) -> usize {
        self.n_col
    }
    pub fn pk(&self) -> &[u8] {
        &self.ab_pk
    }
    pub fn indirect(&self) -> bool {
        self.indirect
    }
    pub fn patchset(&self) -> bool {
        self.b_patchset
    }

    /// `sqlite3changeset_old` — `None` for undefined fields (SQLite
    /// hands back a NULL sqlite3_value).
    pub fn old(&self, i: usize) -> Option<&SessVal> {
        self.old.get(i).and_then(|v| v.as_ref())
    }

    /// `sqlite3changeset_new` (the value accessor; `new` is the
    /// constructor).
    pub fn new_val(&self, i: usize) -> Option<&SessVal> {
        self.new.get(i).and_then(|v| v.as_ref())
    }
}
