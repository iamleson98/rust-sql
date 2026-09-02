//! Inline (small-string-optimized) text storage for `Value::Text`.
//!
//! ## Why
//!
//! SQLite's VDBE materializes TEXT columns as pointers *into the page
//! buffer* — a `sqlite3_column_text` call costs zero allocations. Our
//! engine decodes every projected column into an owned `Value`, so a
//! `String` heap allocation per TEXT value used to ride along with every
//! decoded row: a 5000-row range scan paid 5000 malloc/free pairs just
//! for short names like `name1234`, and short strings dominate OLTP rows.
//!
//! `Text` keeps the SAME size as `String` (24 bytes on 64-bit) while
//! storing strings of up to 23 bytes **inline** — no heap involvement at
//! all. Longer strings spill to a heap `String` exactly as before. The
//! layout follows the classic `compact_str` scheme:
//!
//! ```text
//! little-endian, 24-byte union
//! inline: [ payload .. payload+23 ][ len (0..=23, top bit CLEAR) ]
//! heap:   [ ptr, len, pad, marker=0x80 ] — a manually-managed allocation
//! ```
//!
//! The discriminant is byte #23: the inline length (top bit clear) or the
//! heap marker (top bit set). The heap variant manages its own buffer via
//! the global allocator, so nothing depends on `String`'s internal layout.

use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::ops::Deref;

#[cfg(target_pointer_width = "64")]
/// Maximum byte length stored inline (23 bytes of payload + 1 length byte
/// = 24 bytes, exactly `size_of::<String>()`).
pub const INLINE_CAP: usize = 23;

/// Byte #23 doubles as the discriminant: top bit set = heap variant.
const HEAP_MARKER: u8 = 0x80;

#[repr(C)]
pub struct Text {
    repr: Repr,
}

/// Inline variant: `bytes[..len]` hold the string, `bytes[23]` = len
/// (0..=23, top bit clear).
#[repr(C)]
union Repr {
    inline: [u8; 24],
    heap: std::mem::ManuallyDrop<HeapRepr>,
}

/// Heap variant: a manually-managed `len`-byte allocation. `marker`
/// occupies byte #23 so both variants can read the discriminant from the
/// same offset without relying on `String`'s internal memory layout.
#[repr(C)]
struct HeapRepr {
    ptr: std::ptr::NonNull<u8>, // bytes 0-7
    len: u64,                   // bytes 8-15
    _pad: [u8; 7],              // bytes 16-22
    marker: u8,                 // byte 23 = 0x80
}

// Text owns plain heap bytes — as shareable as String.
unsafe impl Send for Text {}
unsafe impl Sync for Text {}

impl Text {
    /// Build from a `&str`, copying short strings inline.
    #[inline]
    pub fn new(s: &str) -> Text {
        let bytes = s.as_bytes();
        if bytes.len() <= INLINE_CAP {
            let mut buf = [0u8; 24];
            buf[..bytes.len()].copy_from_slice(bytes);
            buf[23] = bytes.len() as u8;
            Text {
                repr: Repr { inline: buf },
            }
        } else {
            Text::from_bytes_heap(bytes)
        }
    }

    /// Take ownership of heap bytes (caller must not reuse them).
    #[inline]
    fn from_bytes_heap(bytes: &[u8]) -> Text {
        let len = bytes.len();
        debug_assert!(len > INLINE_CAP);
        // SAFETY: len > 0 (checked above) and len < isize::MAX for any
        // realistic text value; alignment 1 needs no adjustment.
        let layout = unsafe { std::alloc::Layout::from_size_align_unchecked(len, 1) };
        // SAFETY: global allocator, layout has non-zero size.
        let ptr = unsafe { std::alloc::alloc(layout) };
        assert!(!ptr.is_null(), "Text: out of memory");
        // SAFETY: ptr is valid for `len` writes.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, len) };
        Text {
            repr: Repr {
                heap: std::mem::ManuallyDrop::new(HeapRepr {
                    ptr: std::ptr::NonNull::new(ptr).unwrap(),
                    len: len as u64,
                    _pad: [0; 7],
                    marker: HEAP_MARKER,
                }),
            },
        }
    }

    /// Build from UTF-8 bytes, copying short strings inline with ZERO
    /// allocation. This is the row-codec decode hot path.
    #[inline]
    pub fn from_utf8(bytes: &[u8]) -> Result<Text, std::str::Utf8Error> {
        // Validate first, then copy — never store invalid UTF-8.
        std::str::from_utf8(bytes)?;
        // SAFETY: just validated above.
        Ok(unsafe { Text::from_utf8_unchecked(bytes) })
    }

    /// Build from UTF-8 bytes that the caller guarantees are valid.
    /// (All storage-layer TEXT payloads were validated on write.)
    ///
    /// # Safety
    /// `bytes` must be valid UTF-8.
    #[inline]
    pub unsafe fn from_utf8_unchecked(bytes: &[u8]) -> Text {
        debug_assert!(std::str::from_utf8(bytes).is_ok());
        Text::new(std::str::from_utf8_unchecked(bytes))
    }

    /// Lossy UTF-8 decode (invalid sequences become U+FFFD), matching the
    /// previous `String::from_utf8_lossy(...).into_owned()` behavior.
    #[inline]
    pub fn from_utf8_lossy(bytes: &[u8]) -> Text {
        match std::str::from_utf8(bytes) {
            Ok(_) => Text::new(unsafe { std::str::from_utf8_unchecked(bytes) }),
            Err(_) => Text::new(&String::from_utf8_lossy(bytes)),
        }
    }

    /// Byte length.
    #[inline]
    pub fn len(&self) -> usize {
        // SAFETY: both variants expose a length at a fixed offset; reading
        // the u8 discriminant first selects the correct one.
        unsafe {
            if self.is_inline() {
                self.repr.inline[23] as usize
            } else {
                self.repr.heap.len as usize
            }
        }
    }

    /// Empty check.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether the payload is stored inline (no heap allocation).
    #[inline]
    pub fn is_inline(&self) -> bool {
        // SAFETY: reading a u8 through the inline view is valid for any
        // bit pattern of the union.
        unsafe { self.repr.inline[23] & HEAP_MARKER == 0 }
    }

    /// Borrow the string contents.
    #[inline]
    pub fn as_str(&self) -> &str {
        // SAFETY: every constructor stores valid UTF-8 in both variants.
        unsafe {
            if self.is_inline() {
                let len = self.repr.inline[23] as usize;
                let bytes = std::slice::from_raw_parts(self.repr.inline.as_ptr(), len);
                std::str::from_utf8_unchecked(bytes)
            } else {
                let h = &self.repr.heap;
                let bytes = std::slice::from_raw_parts(h.ptr.as_ptr(), h.len as usize);
                std::str::from_utf8_unchecked(bytes)
            }
        }
    }

    /// Borrow the raw bytes.
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        self.as_str().as_bytes()
    }

    /// Consume and convert to a heap `String`. Short strings allocate
    /// here; long strings move their existing buffer without copying.
    #[inline]
    pub fn into_string(self) -> String {
        let inline = self.is_inline();
        if inline {
            let s = self.as_str().to_owned();
            // Prevent the Drop impl from running on the (inline) repr.
            std::mem::forget(self);
            s
        } else {
            // SAFETY: heap variant holds a live allocation of exactly
            // `len` bytes from the global allocator, valid UTF-8.
            let s = unsafe {
                let h = &self.repr.heap;
                String::from_utf8_unchecked(Vec::from_raw_parts(
                    h.ptr.as_ptr(),
                    h.len as usize,
                    h.len as usize,
                ))
            };
            std::mem::forget(self);
            s
        }
    }

    /// Cheaply clone: inline variants copy 24 bytes, heap variants copy
    /// the payload into a fresh allocation (same as `String::clone`).
    #[inline]
    pub fn duplicate(&self) -> Text {
        if self.is_inline() {
            // SAFETY: reading the u8 view of the union is always valid.
            let buf = unsafe { self.repr.inline };
            Text {
                repr: Repr { inline: buf },
            }
        } else {
            Text::from_bytes_heap(self.as_bytes())
        }
    }

    /// Heap allocation layout matching what `from_bytes_heap` created.
    #[inline]
    fn heap_layout(len: usize) -> std::alloc::Layout {
        // SAFETY: heap variants always have len > INLINE_CAP > 0 and
        // len < isize::MAX.
        unsafe { std::alloc::Layout::from_size_align_unchecked(len, 1) }
    }
}

impl Clone for Text {
    #[inline]
    fn clone(&self) -> Text {
        self.duplicate()
    }
}

impl Drop for Text {
    #[inline]
    fn drop(&mut self) {
        if !self.is_inline() {
            // SAFETY: the heap variant's buffer came from the global
            // allocator with exactly this layout.
            unsafe {
                let h = &self.repr.heap;
                std::alloc::dealloc(h.ptr.as_ptr(), Self::heap_layout(h.len as usize));
            }
        }
    }
}

impl Deref for Text {
    type Target = str;
    #[inline]
    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<str> for Text {
    #[inline]
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<[u8]> for Text {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl std::borrow::Borrow<str> for Text {
    #[inline]
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl From<&str> for Text {
    #[inline]
    fn from(s: &str) -> Text {
        Text::new(s)
    }
}

impl From<&String> for Text {
    #[inline]
    fn from(s: &String) -> Text {
        Text::new(s)
    }
}

impl From<String> for Text {
    #[inline]
    fn from(s: String) -> Text {
        // Copies into our own inline/heap storage; the source String then
        // frees its (possibly larger) buffer.
        Text::new(&s)
    }
}

impl From<Text> for String {
    #[inline]
    fn from(t: Text) -> String {
        t.into_string()
    }
}

impl From<char> for Text {
    #[inline]
    fn from(c: char) -> Text {
        Text::new(c.encode_utf8(&mut [0u8; 4]))
    }
}

impl PartialEq for Text {
    #[inline]
    fn eq(&self, other: &Text) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for Text {}

impl PartialEq<str> for Text {
    #[inline]
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for Text {
    #[inline]
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl PartialEq<String> for Text {
    #[inline]
    fn eq(&self, other: &String) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Hash for Text {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_str().hash(state)
    }
}

impl PartialOrd for Text {
    #[inline]
    fn partial_cmp(&self, other: &Text) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Text {
    #[inline]
    fn cmp(&self, other: &Text) -> Ordering {
        self.as_str().cmp(other.as_str())
    }
}

impl fmt::Display for Text {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for Text {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl Default for Text {
    #[inline]
    fn default() -> Text {
        Text::new("")
    }
}

impl Text {
    /// Concatenate (used by `||`). Stays inline when the result fits.
    pub fn concat(&self, other: &Text) -> Text {
        let a = self.as_str();
        let b = other.as_str();
        if a.len() + b.len() <= INLINE_CAP {
            let mut buf = [0u8; 24];
            buf[..a.len()].copy_from_slice(a.as_bytes());
            buf[a.len()..a.len() + b.len()].copy_from_slice(b.as_bytes());
            buf[23] = (a.len() + b.len()) as u8;
            Text {
                repr: Repr { inline: buf },
            }
        } else {
            let mut s = String::with_capacity(a.len() + b.len());
            s.push_str(a);
            s.push_str(b);
            Text::new(&s)
        }
    }
}

// ---------------------------------------------------------------------------
// Portable fallback (non-64-bit targets): a plain newtype over String.
// ---------------------------------------------------------------------------

#[cfg(not(target_pointer_width = "64"))]
#[derive(Clone, Default)]
pub struct Text(pub String);

#[cfg(not(target_pointer_width = "64"))]
impl Text {
    pub fn new(s: &str) -> Text {
        Text(s.to_owned())
    }
    pub fn from_utf8(bytes: &[u8]) -> Result<Text, std::str::Utf8Error> {
        Ok(Text(std::str::from_utf8(bytes)?.to_owned()))
    }
    pub unsafe fn from_utf8_unchecked(bytes: &[u8]) -> Text {
        Text(String::from_utf8_lossy(bytes).into_owned())
    }
    pub fn from_utf8_lossy(bytes: &[u8]) -> Text {
        Text(String::from_utf8_lossy(bytes).into_owned())
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn is_inline(&self) -> bool {
        true
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
    pub fn into_string(self) -> String {
        self.0
    }
    pub fn duplicate(&self) -> Text {
        self.clone()
    }
    pub fn concat(&self, other: &Text) -> Text {
        Text(format!("{}{}", self.0, other.0))
    }
}

#[cfg(not(target_pointer_width = "64"))]
impl Deref for Text {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

#[cfg(not(target_pointer_width = "64"))]
impl From<&str> for Text {
    fn from(s: &str) -> Text {
        Text(s.to_owned())
    }
}

#[cfg(not(target_pointer_width = "64"))]
impl From<String> for Text {
    fn from(s: String) -> Text {
        Text(s)
    }
}

#[cfg(not(target_pointer_width = "64"))]
impl From<Text> for String {
    fn from(t: Text) -> String {
        t.0
    }
}

#[cfg(not(target_pointer_width = "64"))]
impl PartialEq for Text {
    fn eq(&self, other: &Text) -> bool {
        self.0 == other.0
    }
}

#[cfg(not(target_pointer_width = "64"))]
impl Eq for Text {}

#[cfg(not(target_pointer_width = "64"))]
impl Hash for Text {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state)
    }
}

#[cfg(not(target_pointer_width = "64"))]
impl PartialOrd for Text {
    fn partial_cmp(&self, other: &Text) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(not(target_pointer_width = "64"))]
impl Ord for Text {
    fn cmp(&self, other: &Text) -> Ordering {
        self.0.cmp(&other.0)
    }
}

#[cfg(not(target_pointer_width = "64"))]
impl fmt::Display for Text {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(not(target_pointer_width = "64"))]
impl fmt::Debug for Text {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_is_string_sized() {
        assert_eq!(std::mem::size_of::<Text>(), std::mem::size_of::<String>());
    }

    #[test]
    fn inline_short_strings() {
        for s in ["", "a", "user500", "name1234", "01234567890123456789012"] {
            let t = Text::new(s);
            assert!(t.is_inline(), "{s:?} should be inline");
            assert_eq!(t.as_str(), s);
            assert_eq!(t.len(), s.len());
        }
        // 23 bytes is the inline boundary
        let s23 = "01234567890123456789012";
        assert_eq!(s23.len(), 23);
        assert!(Text::new(s23).is_inline());
        // 24 bytes spills
        let s24 = "012345678901234567890123";
        assert_eq!(s24.len(), 24);
        assert!(!Text::new(s24).is_inline());
        assert_eq!(Text::new(s24).as_str(), s24);
    }

    #[test]
    fn roundtrip_heap_and_inline() {
        let short = Text::new("abc");
        let long = Text::new(&"x".repeat(1000));
        assert_eq!(short.clone().as_str(), "abc");
        assert_eq!(long.clone().as_str(), &"x".repeat(1000));
        assert_eq!(short.clone().into_string(), "abc");
        assert_eq!(long.clone().into_string(), "x".repeat(1000));
        // Drop of both variants must be clean (run under Miri for real
        // verification).
        drop(Text::new("inline drop"));
        drop(Text::new("heap drop heap drop heap drop!"));
    }

    #[test]
    fn from_utf8_paths() {
        let t = Text::from_utf8(b"valid utf8").unwrap();
        assert_eq!(t.as_str(), "valid utf8");
        assert!(t.is_inline());
        let long_bytes: Vec<u8> = (0..500).map(|i| b'a' + (i % 26) as u8).collect();
        let t2 = Text::from_utf8(&long_bytes).unwrap();
        assert!(!t2.is_inline());
        assert_eq!(t2.len(), 500);
        assert!(Text::from_utf8(&[0xff, 0xfe]).is_err());
        // Lossy keeps byte length-ish, replaces bad seqs
        let l = Text::from_utf8_lossy(&[b'a', 0xff, b'b']);
        assert_eq!(l.as_str(), "a\u{fffd}b");
    }

    #[test]
    fn equality_ordering_hash() {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::Hasher as _;
        let a = Text::new("hello");
        let b = Text::new(&format!("hel{}", "lo"));
        let c = Text::new("world");
        assert_eq!(a, b);
        assert!(a < c);
        assert_eq!(a.cmp(&b), Ordering::Equal);

        let mut h1 = DefaultHasher::new();
        let mut h2 = DefaultHasher::new();
        a.hash(&mut h1);
        b.hash(&mut h2);
        assert_eq!(h1.finish(), h2.finish());

        assert!(a == "hello");
        assert!(a == *"hello");
        assert_eq!(a, Text::from("hello".to_string()));
    }

    #[test]
    fn concat_stays_inline() {
        let a = Text::new("foo");
        let b = Text::new("bar");
        let c = a.concat(&b);
        assert!(c.is_inline());
        assert_eq!(c.as_str(), "foobar");

        let big = Text::new(&"y".repeat(40));
        let d = big.concat(&big);
        assert!(!d.is_inline());
        assert_eq!(d.len(), 80);
    }

    #[test]
    fn from_string_inlines_short() {
        let owned = "short".to_string();
        let t = Text::from(owned);
        assert!(t.is_inline());
        assert_eq!(t.as_str(), "short");

        let owned_long =
            "a fairly long string that definitely overflows the inline capacity".to_string();
        let t2 = Text::from(owned_long.clone());
        assert!(!t2.is_inline());
        assert_eq!(t2.as_str(), owned_long);
    }

    #[test]
    fn empty_and_default() {
        let t = Text::default();
        assert!(t.is_empty());
        assert_eq!(t.as_str(), "");
        let t2 = Text::new("");
        assert!(t2.is_inline());
        assert!(t2.is_empty());
    }

    #[test]
    fn utf8_multibyte_inline() {
        // Multi-byte chars count BYTES toward the inline capacity.
        let s = "日本語テスト"; // 18 bytes
        assert_eq!(s.len(), 18);
        let t = Text::new(s);
        assert!(t.is_inline());
        assert_eq!(t.as_str(), s);
        assert_eq!(t.chars().count(), 6);
    }
}
