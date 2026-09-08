//! SQLite on-disk varint codec (fileformat2 §1.5).
//!
//! A variable-length integer is 1-9 bytes, big-endian, 7 bits per byte
//! with the high bit clear on the final byte; the 9th byte (when present)
//! contributes a full 8 bits. The result is a signed 64-bit two's
//! complement value. This is byte-identical to the native format's
//! varint, but is kept standalone so the module has no dependencies on
//! the native b-tree code.

/// Read a varint at `off`. Returns `(value, bytes_consumed)`.
#[inline]
pub fn read_varint(buf: &[u8], off: usize) -> Option<(i64, usize)> {
    let mut result: u64 = 0;
    let mut i = 0usize;
    while i < 8 {
        let b = *buf.get(off + i)?;
        result = (result << 7) | u64::from(b & 0x7f);
        i += 1;
        if b & 0x80 == 0 {
            return Some((result as i64, i));
        }
    }
    // 9th byte: all 8 bits
    let b = *buf.get(off + 8)?;
    result = (result << 8) | u64::from(b);
    Some((result as i64, 9))
}

/// Append a varint encoding of `v` to `out`.
#[inline]
pub fn write_varint(out: &mut Vec<u8>, v: i64) {
    let uv = v as u64;
    if uv <= 0x7f {
        out.push(uv as u8);
        return;
    }
    // Values needing more than 56 bits (and all negatives, whose u64
    // bit patterns are huge) use the 9-byte form: eight 7-bit groups
    // covering bits 63..8 (big-endian, continuation bit set), then a
    // final full byte holding bits 7..0.
    if v < 0 || uv >= (1u64 << 56) {
        let mut tmp = [0u8; 9];
        tmp[8] = (uv & 0xff) as u8;
        let mut rest = uv >> 8;
        for i in (0..8).rev() {
            tmp[i] = (rest & 0x7f) as u8 | 0x80;
            rest >>= 7;
        }
        out.extend_from_slice(&tmp);
        return;
    }
    // 2..8 byte form: 7-bit groups, most significant first.
    let mut n = 1u32;
    let mut x = uv >> 7;
    while x > 0 {
        n += 1;
        x >>= 7;
    }
    for i in (0..n).rev() {
        let shift = i * 7;
        let mut byte = ((uv >> shift) & 0x7f) as u8;
        if i > 0 {
            byte |= 0x80;
        }
        out.push(byte);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let cases: &[i64] = &[
            0,
            1,
            -1,
            127,
            128,
            -128,
            300,
            16383,
            16384,
            2097151,
            2097152,
            268435455,
            i32::MAX as i64,
            i32::MIN as i64,
            i64::MAX,
            i64::MIN,
        ];
        for &c in cases {
            let mut buf = Vec::new();
            write_varint(&mut buf, c);
            let (v, n) = read_varint(&buf, 0).unwrap();
            assert_eq!(v, c, "value {c}");
            assert_eq!(n, buf.len(), "len {c}");
        }
    }

    #[test]
    fn sqlite_known_encodings() {
        let mut b = vec![];
        write_varint(&mut b, 127);
        assert_eq!(b, vec![0x7f]);
        let mut b = vec![];
        write_varint(&mut b, 128);
        assert_eq!(b, vec![0x81, 0x00]);
        // 9399 = 0x24B7: high group 73 (0x49|0x80), low group 0x37.
        let mut b = vec![];
        write_varint(&mut b, 9399);
        assert_eq!(b, vec![0xC9, 0x37]);
    }

    #[test]
    fn negative_is_nine_bytes() {
        let mut b = vec![];
        write_varint(&mut b, -1);
        assert_eq!(b.len(), 9);
        let (v, n) = read_varint(&b, 0).unwrap();
        assert_eq!(v, -1);
        assert_eq!(n, 9);
    }
}
