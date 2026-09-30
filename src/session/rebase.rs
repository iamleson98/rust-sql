//! The changeset rebaser — SQLite's `sqlite3_rebaser`
//! (`sqlite3rebaser_create/_configure/_rebase/_delete`,
//! `sessionRebase`, `sessionAppendRecordMerge`,
//! `sessionAppendPartialUpdate`).
//!
//! A rebase blob (apply_v2's output) records the OMIT/REPLACE decisions
//! a remote node made while applying OUR changeset's parent. Rebasing
//! rewrites our local changeset so it applies cleanly on top of the
//! remote's resolution:
//!
//!   local op    rebase op     rebased output
//!   ----------  ------------  --------------------------------------
//!   INSERT      INSERT        UPDATE (old=rebase, new=local) — unless
//!                             the rebase entry is indirect (dropped)
//!   UPDATE      DELETE        INSERT (local new, rebase fills)
//!   UPDATE      INSERT        partial UPDATE: fields the remote
//!                             replaced (0xFF) drop out; the old values
//!                             the remote set become the local old
//!   DELETE      INSERT        DELETE (rebase record, local old fills)
//!   DELETE      DELETE        (dropped)
//!   other pairs —             the change passes through unchanged

use super::codec::*;
use super::group::ChangeGroup;
use super::iter::ChangesetIter;

/// `sqlite3rebaser_create`.
pub struct Rebaser {
    grp: ChangeGroup,
}

impl Default for Rebaser {
    fn default() -> Self {
        Self::new()
    }
}

impl Rebaser {
    pub fn new() -> Self {
        Rebaser {
            grp: ChangeGroup::new_rebase(),
        }
    }

    /// `sqlite3rebaser_configure` — load a rebase blob.
    pub fn configure(&mut self, rebase_blob: &[u8]) -> Result<(), &'static str> {
        self.grp.add(rebase_blob)
    }

    /// `sqlite3rebaser_rebase` — rebase a changeset (patchsets are
    /// rejected, like SQLite).
    pub fn rebase(&self, changeset: &[u8]) -> Result<Vec<u8>, &'static str> {
        let mut iter = ChangesetIter::new(changeset)?;
        let mut out: Vec<u8> = Vec::new();
        // The current table's rebase entries (if any).
        let mut cur_table: Option<(String, usize, Vec<u8>, Vec<RawEntryRef>)> = None;
        while iter.next()? {
            let table_changed = cur_table
                .as_ref()
                .map(|(n, _, _, _)| n != iter.table())
                .unwrap_or(true);
            if table_changed {
                // Table header passes through.
                out.push(if iter.patchset() { b'P' } else { b'T' });
                put_varint(&mut out, iter.n_col() as u64);
                out.extend_from_slice(iter.pk());
                out.extend_from_slice(iter.table().as_bytes());
                out.push(0);
                if iter.patchset() {
                    return Err("patchset may not be rebased");
                }
                let entries = self.grp.table_entries(iter.table());
                cur_table = Some((
                    iter.table().to_string(),
                    iter.n_col(),
                    iter.pk().to_vec(),
                    entries,
                ));
            }
            let (n_col, ab_pk, entries) = match &cur_table {
                Some((_, n, pk, e)) => (*n, pk.clone(), e),
                None => unreachable!("table header always precedes changes"),
            };
            let raw = iter.raw_records().to_vec();
            let op = iter.op();
            let indirect = iter.indirect();
            // Find the rebase entry for this row (PK equality).
            let hit = find_rebase_entry(&ab_pk, n_col, entries, &raw);
            let mut done = false;
            if let Some(entry) = hit {
                match op {
                    OP_INSERT => {
                        if entry.op == OP_INSERT {
                            done = true;
                            if !entry.indirect {
                                out.push(OP_UPDATE);
                                out.push(indirect as u8);
                                out.extend_from_slice(&entry.record);
                                out.extend_from_slice(&raw);
                            }
                        }
                    }
                    OP_UPDATE => {
                        done = true;
                        if entry.op == OP_DELETE {
                            if !entry.indirect {
                                let new_off = skip_record(&raw, 0, n_col);
                                out.push(OP_INSERT);
                                out.push(indirect as u8);
                                append_record_merge(
                                    &mut out,
                                    n_col,
                                    &raw[new_off..],
                                    &entry.record,
                                );
                            }
                        } else {
                            append_partial_update(
                                &mut out,
                                n_col,
                                &ab_pk,
                                indirect,
                                &raw,
                                &entry.record,
                            );
                        }
                    }
                    _ => {
                        // DELETE
                        done = true;
                        if entry.op == OP_INSERT {
                            out.push(OP_DELETE);
                            out.push(indirect as u8);
                            append_record_merge(&mut out, n_col, &entry.record, &raw);
                        }
                    }
                }
            }
            if !done {
                out.push(op);
                out.push(indirect as u8);
                out.extend_from_slice(&raw);
            }
        }
        Ok(out)
    }
}

/// A view of one rebase entry.
pub(crate) struct RawEntryRef {
    op: u8,
    indirect: bool,
    record: Vec<u8>,
}

impl ChangeGroup {
    /// The rebase entries of one table (name-matched).
    pub(crate) fn table_entries(&self, name: &str) -> Vec<RawEntryRef> {
        self.tables
            .iter()
            .find(|t| t.name.eq_ignore_ascii_case(name))
            .map(|t| {
                let mut out = Vec::new();
                for chain in &t.buckets {
                    for &id in chain {
                        if let Some(e) = t.entries.get(id).and_then(|e| e.as_ref()) {
                            out.push(RawEntryRef {
                                op: e.op,
                                indirect: e.indirect,
                                record: e.record.clone(),
                            });
                        }
                    }
                }
                out
            })
            .unwrap_or_default()
    }
}

fn find_rebase_entry<'e>(
    ab_pk: &[u8],
    n_col: usize,
    entries: &'e [RawEntryRef],
    raw: &[u8],
) -> Option<&'e RawEntryRef> {
    entries
        .iter()
        .find(|&e| pk_records_equal(ab_pk, n_col, &e.record, raw))
        .map(|v| v as _)
}

/// PK-field equality of two single records (`sessionChangeEqual`,
/// pk-only sides).
fn pk_records_equal(ab_pk: &[u8], n_col: usize, a: &[u8], b: &[u8]) -> bool {
    let mut a1: &[u8] = a;
    let mut a2: &[u8] = b;
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
            let n1 = serial_len(a1, 0);
            let n2 = serial_len(a2, 0);
            a1 = &a1[n1..];
            a2 = &a2[n2..];
        }
    }
    true
}

/// `sessionAppendRecordMerge`: copy of a1 except undefined fields fall
/// back to a2.
fn append_record_merge(out: &mut Vec<u8>, n_col: usize, a1: &[u8], a2: &[u8]) {
    let mut x1: &[u8] = a1;
    let mut x2: &[u8] = a2;
    for _ in 0..n_col {
        let n1 = serial_len(x1, 0);
        let n2 = serial_len(x2, 0);
        if x1[0] == TYPE_UNDEF || x1[0] == TYPE_REPLACED {
            out.extend_from_slice(&x2[..n2]);
        } else {
            out.extend_from_slice(&x1[..n1]);
        }
        x1 = &x1[n1..];
        x2 = &x2[n2..];
    }
}

/// `sessionAppendPartialUpdate` — rebase a local UPDATE against a remote
/// UPDATE (the rebase entry's single record). Fields the remote
/// REPLACED (0xFF) drop out of the local change; old values the remote
/// set become the local old values. Emits nothing when no local
/// modification survives.
fn append_partial_update(
    out: &mut Vec<u8>,
    n_col: usize,
    ab_pk: &[u8],
    indirect: bool,
    a_rec: &[u8],
    a_change: &[u8],
) {
    let mut a1: &[u8] = a_rec; // walks old.* then new.*
    let mut a2: &[u8] = a_change;
    let mut buf: Vec<u8> = Vec::with_capacity(a_rec.len() + a_change.len());
    buf.push(OP_UPDATE);
    buf.push(indirect as u8);
    let mut b_data = false;
    // old.*: per column — PK or remote-undefined → keep local old
    // (marking bData when a real value); remote-value → the remote value
    // becomes the local old; remote-REPLACED + undefined local → 0x00.
    for &pk in ab_pk.iter().take(n_col) {
        let n1 = serial_len(a1, 0);
        let n2 = serial_len(a2, 0);
        if pk != 0 || a2[0] == TYPE_UNDEF {
            if pk == 0 && a1[0] != TYPE_UNDEF && a1[0] != TYPE_REPLACED {
                b_data = true;
            }
            buf.extend_from_slice(&a1[..n1]);
        } else if a2[0] != TYPE_REPLACED && a1[0] != TYPE_UNDEF && a1[0] != TYPE_REPLACED {
            b_data = true;
            buf.extend_from_slice(&a2[..n2]);
        } else {
            buf.push(TYPE_UNDEF);
        }
        a1 = &a1[n1..];
        a2 = &a2[n2..];
    }
    if !b_data {
        return; // the whole change touched only replaced fields
    }
    // new.*: per column — PK or remote not-REPLACED → local new; else 0.
    let mut a2: &[u8] = a_change;
    for (_i, &pk) in ab_pk.iter().enumerate().take(n_col) {
        let n1 = serial_len(a1, 0);
        let n2 = serial_len(a2, 0);
        if pk != 0 || a2[0] != TYPE_REPLACED {
            buf.extend_from_slice(&a1[..n1]);
        } else {
            buf.push(TYPE_UNDEF);
        }
        a1 = &a1[n1..];
        a2 = &a2[n2..];
    }
    out.extend_from_slice(&buf);
}
