//! The change-group — SQLite's `sqlite3_changegroup`
//! (`sqlite3changegroup_new/_schema/_add/_add_change/_output/_delete`)
//! and `sqlite3changeset_concat` (add + add + output).
//!
//! A group folds changesets together row-by-row through
//! `sessionChangeMerge`'s coalescing matrix, storing each entry's RAW
//! record bytes (for UPDATE entries: old-record + new-record). The
//! output iterates tables in FIRST-SEEN order and each table's hash
//! buckets head-first — the same ordering discipline as the capture
//! side, so concatenated output is byte-stable.

use super::codec::*;
use super::iter::ChangesetIter;

/// One grouped change: the raw record blob (UPDATE = old||new).
pub(crate) struct GroupEntry {
    pub(crate) op: u8,
    pub(crate) indirect: bool,
    pub(crate) record: Vec<u8>,
}

/// One grouped table.
pub(crate) struct GroupTable {
    pub(crate) name: String,
    pub(crate) n_col: usize,
    /// The changeset's PK array verbatim (positions; 0 = non-PK).
    pub(crate) ab_pk: Vec<u8>,
    pub(crate) b_patch: bool,
    pub(crate) buckets: Vec<Vec<usize>>,
    pub(crate) entries: Vec<Option<GroupEntry>>,
    pub(crate) n_entry: usize,
}

impl GroupTable {
    fn new(name: &str, n_col: usize, ab_pk: Vec<u8>, b_patch: bool) -> Self {
        GroupTable {
            name: name.to_string(),
            n_col,
            ab_pk,
            b_patch,
            buckets: Vec::new(),
            entries: Vec::new(),
            n_entry: 0,
        }
    }
}

/// The change group.
pub struct ChangeGroup {
    pub(crate) tables: Vec<GroupTable>,
    /// Set from the first added changeset; mixing changesets and
    /// patchsets is an error.
    pub(crate) b_patch: Option<bool>,
    /// Rebase mode (0xFF "replaced" markers in merges).
    b_rebase: bool,
}

impl Default for ChangeGroup {
    fn default() -> Self {
        Self::new()
    }
}

impl ChangeGroup {
    pub fn new() -> Self {
        ChangeGroup {
            tables: Vec::new(),
            b_patch: None,
            b_rebase: false,
        }
    }

    /// A rebaser's backing group (`sessionChangesetToHash(.., bRebase=1)`).
    pub(crate) fn new_rebase() -> Self {
        ChangeGroup {
            tables: Vec::new(),
            b_patch: None,
            b_rebase: true,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.tables.iter().all(|t| t.n_entry == 0)
    }

    /// `sqlite3changegroup_add` — fold one changeset/patchset in.
    pub fn add(&mut self, cs: &[u8]) -> Result<(), &'static str> {
        let mut iter = ChangesetIter::new(cs)?;
        while iter.next()? {
            self.add_one(
                iter.table(),
                iter.n_col(),
                iter.pk(),
                iter.patchset(),
                iter.op(),
                iter.indirect(),
                iter.raw_records(),
            )?;
        }
        Ok(())
    }

    /// `sessionOneChangeToHash` — one change from an iterator.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn add_one(
        &mut self,
        table: &str,
        n_col: usize,
        ab_pk_cs: &[u8],
        b_patchset: bool,
        op: u8,
        indirect: bool,
        raw: &[u8],
    ) -> Result<(), &'static str> {
        if self.b_patch.is_none() {
            self.b_patch = Some(b_patchset);
        } else if self.b_patch != Some(b_patchset) {
            return Err("cannot mix changesets and patchsets");
        }
        // Find or create the table (first-seen order = output order).
        let ti = match self
            .tables
            .iter()
            .position(|t| t.name.eq_ignore_ascii_case(table))
        {
            Some(i) => i,
            None => {
                let t = GroupTable::new(table, n_col, ab_pk_cs.to_vec(), b_patchset);
                self.tables.push(t);
                self.tables.len() - 1
            }
        };
        // Compatibility (sessionChangesetCheckCompat).
        let t = &self.tables[ti];
        if t.ab_pk.len() < n_col {
            return Err("primary key mismatch");
        }
        for (i, &cs_pk) in ab_pk_cs.iter().enumerate().take(n_col) {
            if (t.ab_pk[i] != 0) != (cs_pk != 0) {
                return Err("primary key mismatch");
            }
        }
        // sessionUpdateFind semantics: a plain-record model for merges.
        let t = &mut self.tables[ti];
        if t.buckets.is_empty() || t.n_entry >= t.buckets.len() / 2 {
            let n_new = if t.buckets.is_empty() {
                256
            } else {
                t.buckets.len() * 2
            };
            let mut new_buckets: Vec<Vec<usize>> = vec![Vec::new(); n_new];
            for chain in t.buckets.iter_mut() {
                for &id in chain.iter() {
                    let e = t.entries[id].as_ref().unwrap();
                    let b_pk_only = e.op == OP_DELETE && t.b_patch;
                    let ih =
                        super::state::record_pk_hash(&t.ab_pk, &e.record, b_pk_only, n_new as u32);
                    new_buckets[ih].insert(0, id);
                }
            }
            t.buckets = new_buckets;
        }
        let b_pk_only = op == OP_DELETE && b_patchset;
        let h = super::state::record_pk_hash(&t.ab_pk, raw, b_pk_only, t.buckets.len() as u32);
        // Look for the same PK.
        let mut existing: Option<usize> = None;
        for &id in t.buckets[h].iter() {
            let e = t.entries[id].as_ref().unwrap();
            if change_equal(
                &t.ab_pk,
                n_col,
                &e.record,
                b_pk_only_entry(e.op, t.b_patch),
                raw,
                b_pk_only,
            ) {
                existing = Some(id);
                break;
            }
        }
        match existing {
            None => {
                let entry = GroupEntry {
                    op,
                    indirect,
                    record: raw.to_vec(),
                };
                let id = t.entries.len();
                t.entries.push(Some(entry));
                t.buckets[h].insert(0, id);
                t.n_entry += 1;
            }
            Some(id) => {
                let merged = {
                    let e = t.entries[id].as_ref().unwrap();
                    change_merge(
                        &t.ab_pk,
                        n_col,
                        self.b_rebase,
                        t.b_patch,
                        e,
                        op,
                        indirect,
                        raw,
                    )
                };
                match merged {
                    Some((mop, mindirect, mrec)) => {
                        if let Some(m) = mrec {
                            t.entries[id] = Some(GroupEntry {
                                op: mop,
                                indirect: mindirect,
                                record: m,
                            });
                        } else {
                            // The merged change is a no-op: drop the entry.
                            let slot = t.entries[id].take();
                            if slot.is_some() {
                                t.n_entry -= 1;
                            }
                            t.buckets[h].retain(|&x| x != id);
                        }
                    }
                    None => {
                        // Unsupported combination — keep the existing
                        // entry (discard op2).
                    }
                }
            }
        }
        Ok(())
    }

    /// `sqlite3changegroup_output` — the folded changeset/patchset.
    pub fn output(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for t in &self.tables {
            if t.n_entry == 0 {
                continue;
            }
            out.push(if t.b_patch { b'P' } else { b'T' });
            put_varint(&mut out, t.n_col as u64);
            for &p in &t.ab_pk {
                out.push(p);
            }
            out.extend_from_slice(t.name.as_bytes());
            out.push(0);
            for chain in &t.buckets {
                for &id in chain {
                    if let Some(e) = t.entries.get(id).and_then(|e| e.as_ref()) {
                        out.push(e.op);
                        out.push(e.indirect as u8);
                        out.extend_from_slice(&e.record);
                    }
                }
            }
        }
        out
    }
}

fn b_pk_only_entry(op: u8, b_patch: bool) -> bool {
    op == OP_DELETE && b_patch
}

fn change_equal(
    ab_pk: &[u8],
    n_col: usize,
    left: &[u8],
    b_left_pk_only: bool,
    right: &[u8],
    b_right_pk_only: bool,
) -> bool {
    let mut a1 = left;
    let mut a2 = right;
    for (_i, &pk) in ab_pk.iter().enumerate().take(n_col) {
        if pk != 0 {
            let n1 = serial_len(a1, 0);
            let n2 = serial_len(a2, 0);
            if n1 != n2 || a1[..n1] != a2[..n2] {
                return false;
            }
            a1 = &a1[n1..];
            a2 = &a2[n2..];
        } else {
            if !b_left_pk_only {
                let n1 = serial_len(a1, 0);
                a1 = &a1[n1..];
            }
            if !b_right_pk_only {
                let n2 = serial_len(a2, 0);
                a2 = &a2[n2..];
            }
        }
    }
    true
}

/// `sessionChangeMerge` (changeset/patchset + the rebase variant).
/// Returns `Some((op, indirect, Option<record>))` — `None` for the
/// "unsupported, discard op2" combinations; a `None` record drops the
/// entry (merged to a no-op).
#[allow(clippy::too_many_arguments)]
fn change_merge(
    ab_pk: &[u8],
    n_col: usize,
    b_rebase: bool,
    b_patchset: bool,
    exist: &GroupEntry,
    op2: u8,
    b_indirect: bool,
    a_rec: &[u8],
) -> Option<(u8, bool, Option<Vec<u8>>)> {
    let op1 = exist.op;
    if b_rebase {
        // The rebase-hash merge (0xFF markers).
        if op1 == OP_DELETE && exist.indirect {
            return Some((exist.op, exist.indirect, Some(exist.record.clone())));
        }
        let mut out = Vec::with_capacity(exist.record.len() + a_rec.len());
        let b_indirect_merged = b_indirect || exist.indirect;
        let mut a1: &[u8] = &exist.record;
        let mut a2: &[u8] = a_rec;
        for (_i, &pk) in ab_pk.iter().enumerate().take(n_col) {
            let n1 = serial_len(a1, 0);
            let n2 = serial_len(a2, 0);
            if a1[0] == TYPE_REPLACED || (pk == 0 && b_indirect) {
                out.push(TYPE_REPLACED);
            } else if a2[0] == TYPE_UNDEF {
                out.extend_from_slice(&a1[..n1]);
            } else {
                out.extend_from_slice(&a2[..n2]);
            }
            a1 = &a1[n1..];
            a2 = &a2[n2..];
        }
        return Some((op2, b_indirect_merged, Some(out)));
    }

    // Plain coalescing matrix.
    if (op1 == OP_INSERT && op2 == OP_INSERT)
        || (op1 == OP_UPDATE && op2 == OP_INSERT)
        || (op1 == OP_DELETE && op2 == OP_UPDATE)
        || (op1 == OP_DELETE && op2 == OP_DELETE)
    {
        // Unsupported: keep the existing change, discard op2.
        return None;
    }
    if op1 == OP_INSERT && op2 == OP_DELETE {
        // INSERT + DELETE → no change at all.
        return Some((op1, false, None));
    }
    let b_indirect_merged = b_indirect && exist.indirect;
    if op1 == OP_INSERT {
        // INSERT + UPDATE → INSERT (the update's new values win).
        let mut a1: &[u8] = a_rec;
        if !b_patchset {
            a1 = &a_rec[skip_record(a_rec, 0, n_col)..];
        }
        let mut out = Vec::with_capacity(exist.record.len() + a_rec.len());
        merge_record(&mut out, n_col, &exist.record, a1);
        Some((OP_INSERT, b_indirect_merged, Some(out)))
    } else if op1 == OP_DELETE {
        // DELETE + INSERT → UPDATE.
        if b_patchset {
            Some((OP_UPDATE, b_indirect_merged, Some(a_rec.to_vec())))
        } else {
            let mut out = Vec::with_capacity(exist.record.len() + a_rec.len());
            let ok = merge_update(
                &mut out,
                ab_pk,
                n_col,
                b_patchset,
                &exist.record,
                None,
                a_rec,
                None,
            );
            if ok {
                Some((OP_UPDATE, b_indirect_merged, Some(out)))
            } else {
                Some((OP_UPDATE, b_indirect_merged, None))
            }
        }
    } else if op2 == OP_UPDATE {
        // UPDATE + UPDATE → UPDATE.
        let mut a1: &[u8] = &exist.record;
        let mut a2: &[u8] = a_rec;
        if !b_patchset {
            a1 = &a1[skip_record(a1, 0, n_col)..];
            a2 = &a2[skip_record(a2, 0, n_col)..];
        }
        let mut out = Vec::with_capacity(exist.record.len() + a_rec.len());
        let ok = merge_update(
            &mut out,
            ab_pk,
            n_col,
            b_patchset,
            a_rec,
            Some(&exist.record),
            a1,
            Some(a2),
        );
        if ok {
            Some((OP_UPDATE, b_indirect_merged, Some(out)))
        } else {
            Some((OP_UPDATE, b_indirect_merged, None))
        }
    } else {
        // UPDATE + DELETE → DELETE (the ORIGINAL old values).
        let mut out = Vec::with_capacity(exist.record.len() + a_rec.len());
        if b_patchset {
            out.extend_from_slice(a_rec);
            Some((OP_DELETE, b_indirect_merged, Some(out)))
        } else {
            merge_record(&mut out, n_col, a_rec, &exist.record);
            Some((OP_DELETE, b_indirect_merged, Some(out)))
        }
    }
}

/// `sessionMergeRecord`: per column, aRight's value when defined, else
/// aLeft's.
fn merge_record(out: &mut Vec<u8>, n_col: usize, a_left: &[u8], a_right: &[u8]) {
    let mut a1: &[u8] = a_left;
    let mut a2: &[u8] = a_right;
    for _ in 0..n_col {
        let n1 = serial_len(a1, 0);
        let n2 = serial_len(a2, 0);
        if a2[0] != 0 {
            out.extend_from_slice(&a2[..n2]);
        } else {
            out.extend_from_slice(&a1[..n1]);
        }
        a1 = &a1[n1..];
        a2 = &a2[n2..];
    }
}

/// `sessionMergeUpdate` — returns false when the merged UPDATE changes
/// nothing (the caller drops the entry).
fn merge_update(
    out: &mut Vec<u8>,
    ab_pk: &[u8],
    n_col: usize,
    b_patchset: bool,
    a_old1: &[u8],
    a_old2: Option<&[u8]>,
    a_new1: &[u8],
    a_new2: Option<&[u8]>,
) -> bool {
    // Per-column mergeValue: (one, two) — two wins when defined.
    fn merge_value<'a>(a1: &mut &'a [u8], a2: &mut Option<&'a [u8]>) -> &'a [u8] {
        let n1 = serial_len(a1, 0);
        let chosen: &[u8] = if let Some(two) = *a2 {
            let n2 = serial_len(two, 0);
            if two[0] != 0 {
                let r = &two[..n2];
                *a1 = &a1[n1..];
                *a2 = Some(&two[n2..]);
                return r;
            }
            *a2 = Some(&two[n2..]);
            &a1[..n1]
        } else {
            &a1[..n1]
        };
        *a1 = &a1[n1..];
        chosen
    }
    if !b_patchset {
        let mut o1 = a_old1;
        let mut o2 = a_old2;
        let mut n1 = a_new1;
        let mut n2 = a_new2;
        let mut b_required = false;
        let mut old_out = Vec::with_capacity(a_old1.len() + a_new1.len());
        for (_i, &pk) in ab_pk.iter().enumerate().take(n_col) {
            let a_old = merge_value(&mut o1, &mut o2).to_vec();
            let a_new = merge_value(&mut n1, &mut n2).to_vec();
            if pk != 0 || a_old != a_new {
                if pk == 0 {
                    b_required = true;
                }
                old_out.extend_from_slice(&a_old);
            } else {
                old_out.push(0);
            }
        }
        if !b_required {
            return false;
        }
        out.extend_from_slice(&old_out);
    }
    // new.* vector.
    let mut o1 = a_old1;
    let mut o2 = a_old2;
    let mut n1 = a_new1;
    let mut n2 = a_new2;
    for (_i, &pk) in ab_pk.iter().enumerate().take(n_col) {
        let a_old = merge_value(&mut o1, &mut o2).to_vec();
        let a_new = merge_value(&mut n1, &mut n2).to_vec();
        if !b_patchset && (pk != 0 || a_old == a_new) {
            out.push(0);
        } else {
            out.extend_from_slice(&a_new);
        }
    }
    true
}

/// `sqlite3changeset_concat(A, B)` — the changeset that applies A then B.
pub fn concat(a: &[u8], b: &[u8]) -> Result<Vec<u8>, &'static str> {
    let mut grp = ChangeGroup::new();
    grp.add(a)?;
    grp.add(b)?;
    Ok(grp.output())
}

/// Extra iterator accessors for the group path: the raw record bytes of
/// the current change (op/indirect stripped).
impl<'a> ChangesetIter<'a> {
    pub(crate) fn raw_records(&self) -> &[u8] {
        &self.raw
    }
}
