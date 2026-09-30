//! Changeset inversion — SQLite's `sqlite3changeset_invert`
//! (`sessionChangesetInvert`).
//!
//! A buffer-level transform:
//! * table headers ('T') pass through;
//! * INSERT ↔ DELETE (records copied verbatim);
//! * UPDATE stays UPDATE with the records transformed — the inverted
//!   old.* takes PK columns from the original old.* and the rest from
//!   the original new.*; the inverted new.* takes every original old.*
//!   column except the PK columns, which become undefined.

use super::codec::SessVal;
use super::codec::*;

/// `sqlite3changeset_invert`.
pub fn invert(cs: &[u8]) -> Result<Vec<u8>, &'static str> {
    let mut out: Vec<u8> = Vec::with_capacity(cs.len());
    let mut pos = 0usize;
    let mut n_col: usize = 0;
    let mut ab_pk: Vec<bool> = Vec::new();
    while pos < cs.len() {
        let e = cs[pos];
        match e {
            b'T' | b'P' => {
                if e == b'P' {
                    // A patchset may not be inverted (SQLite raises
                    // SQLITE_CORRUPT on the first change read).
                    return Err("corrupt changeset");
                }
                pos += 1;
                let (n, used) = get_varint(cs, pos);
                let nc = n as usize;
                if nc == 0 || pos + used + nc > cs.len() {
                    return Err("corrupt changeset");
                }
                ab_pk = cs[pos + used..pos + used + nc]
                    .iter()
                    .map(|p| *p != 0)
                    .collect();
                n_col = nc;
                // Copy the header through.
                let name_end = cs[pos + used + nc..]
                    .iter()
                    .position(|b| *b == 0)
                    .ok_or("corrupt changeset")?;
                let hdr_end = pos + used + nc + name_end + 1;
                out.extend_from_slice(&cs[pos - 1..hdr_end]);
                pos = hdr_end;
            }
            OP_INSERT | OP_DELETE => {
                let flipped = if e == OP_INSERT { OP_DELETE } else { OP_INSERT };
                let indirect = cs[pos + 1];
                pos += 2;
                // One record of n_col fields.
                let rec_start = pos;
                for _ in 0..n_col {
                    let l = serial_len(cs, pos);
                    if pos + l > cs.len() {
                        return Err("corrupt changeset");
                    }
                    pos += l;
                }
                out.push(flipped);
                out.push(indirect);
                out.extend_from_slice(&cs[rec_start..pos]);
            }
            OP_UPDATE => {
                let indirect = cs[pos + 1];
                pos += 2;
                // old record then new record.
                let old_start = pos;
                let (old_fields, old_len) = parse_record(cs, pos, n_col)?;
                pos += old_len;
                let new_start = pos;
                let (new_fields, new_len) = parse_record(cs, pos, n_col)?;
                pos += new_len;
                out.push(OP_UPDATE);
                out.push(indirect);
                // Inverted old.*: PK from original old, others from new.
                for (i, f) in old_fields.iter().enumerate() {
                    if ab_pk.get(i).copied().unwrap_or(false) {
                        let l = serial_len(cs, old_start); // recomputed below
                        let _ = l;
                        append_field(&mut out, f);
                    } else {
                        append_field(&mut out, &new_fields[i]);
                    }
                }
                // Inverted new.*: original old values, PK → undefined.
                for (i, f) in old_fields.iter().enumerate() {
                    if ab_pk.get(i).copied().unwrap_or(false) {
                        out.push(TYPE_UNDEF);
                    } else {
                        append_field(&mut out, f);
                    }
                }
                let _ = (new_start, new_len);
            }
            _ => return Err("corrupt changeset"),
        }
    }
    Ok(out)
}

/// Append one parsed field back in serialized form. Undefined and
/// replaced stay as-is (invert only sees well-formed changesets).
fn append_field(out: &mut Vec<u8>, f: &SessVal) {
    match f {
        SessVal::Undef => out.push(TYPE_UNDEF),
        SessVal::Replaced => out.push(TYPE_REPLACED),
        SessVal::Null => out.push(TYPE_NULL),
        SessVal::Int(v) => {
            out.push(TYPE_INT);
            put_i64(out, *v);
        }
        SessVal::Real(v) => {
            out.push(TYPE_REAL);
            put_i64(out, v.to_bits() as i64);
        }
        SessVal::Text(t) => {
            out.push(TYPE_TEXT);
            put_varint(out, t.len() as u64);
            out.extend_from_slice(t);
        }
        SessVal::Blob(b) => {
            out.push(TYPE_BLOB);
            put_varint(out, b.len() as u64);
            out.extend_from_slice(b);
        }
    }
}
