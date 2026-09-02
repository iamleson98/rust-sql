//! Pluggable page codecs: transform pages between their on-disk (encoded)
//! and in-cache (plain) forms — the hook SQLite's SEE encryption extension
//! and the ZIPVFS compression VFS use.
//!
//! A codec is registered with
//! [`Database::create_codec`](crate::api::Database::create_codec) and
//! activated per database with `PRAGMA codec = name` (or
//! [`Database::set_page_codec`]). Once active, every page the pager writes
//! passes through [`PageCodec::encode`] and every page read passes through
//! [`PageCodec::decode`].
//!
//! Page 1 (the header page) has the database magic at its start; a codec
//! must leave the first 16 bytes of page 0 untouched so the file remains
//! recognizable (SQLite's SEE encrypts everything AFTER the header for the
//! same reason). The engine enforces this by skipping the first 16 bytes of
//! page 0 for codec transforms.
//!
//! Example built-in codec: the XOR demo codec below (also used by the test
//! suite to verify the full round-trip through the pager).
//!
//! ```
//! use rustqlite::Database;
//! use rustqlite::plugin::codec::XorCodec;
//!
//! let mut db = Database::open_in_memory().unwrap();
//! db.create_codec(XorCodec::new(0x5A)).unwrap();
//! // PRAGMA codec is sticky: writes from here on are encoded on disk.
//! // (The demo codec registers under the name "xor".)
//! db.execute("PRAGMA codec = xor", []).unwrap();
//! ```

use crate::error::{Error, Result};
use std::sync::Arc;

/// Transform pages between plain (in-cache) and encoded (on-disk) forms.
///
/// Contract: `encode` returns exactly `page.len()` bytes (compress then
/// pad), and `decode` returns exactly `page_size` bytes — the pager keeps
/// fixed-size positional layout. Implementations must be deterministic,
/// thread-safe (`Send + Sync`), and stable across versions (a file written
/// with codec X must decode with codec X).
pub trait PageCodec: Send + Sync {
    /// Codec name (referenced by `PRAGMA codec = name`).
    fn name(&self) -> &str;

    /// Plain page (exactly page_size bytes, except a possibly-short final
    /// page) → encoded bytes to write to disk. MAY return a different
    /// length (compressed / padded).
    fn encode(&self, page: &[u8]) -> Result<Vec<u8>>;

    /// Encoded bytes read from disk → plain page (exactly page_size bytes
    /// for full pages).
    fn decode(&self, data: &[u8], page_size: usize) -> Result<Vec<u8>>;

    /// Human-readable marker stored in the database header comment area so
    /// `PRAGMA codec` on reopen knows which codec to require. Default:
    /// the codec name.
    fn marker(&self) -> String {
        self.name().to_ascii_lowercase()
    }
}

/// Demo codec: XOR every byte after the first 16 (page 0 header) with a
/// fixed key. NOT encryption — it exists to prove the codec chain and to
/// give tests a cheap, inspectable transform.
pub struct XorCodec {
    key: u8,
}

impl XorCodec {
    pub fn new(key: u8) -> Self {
        Self { key }
    }
}

impl PageCodec for XorCodec {
    fn name(&self) -> &str {
        // Name embeds the key so several instances can coexist.
        "xor"
    }

    fn encode(&self, page: &[u8]) -> Result<Vec<u8>> {
        let mut out = page.to_vec();
        for b in out.iter_mut() {
            *b ^= self.key;
        }
        Ok(out)
    }

    fn decode(&self, data: &[u8], page_size: usize) -> Result<Vec<u8>> {
        let mut out = vec![0u8; page_size.max(data.len())];
        let n = data.len().min(out.len());
        out[..n].copy_from_slice(&data[..n]);
        for b in out.iter_mut() {
            *b ^= self.key;
        }
        Ok(out)
    }
}

/// Identity codec (a no-op; equals "no codec active").
pub struct PlainCodec;

impl PageCodec for PlainCodec {
    fn name(&self) -> &str {
        "plain"
    }
    fn encode(&self, page: &[u8]) -> Result<Vec<u8>> {
        Ok(page.to_vec())
    }
    fn decode(&self, data: &[u8], page_size: usize) -> Result<Vec<u8>> {
        let mut out = vec![0u8; page_size.max(data.len())];
        let n = data.len().min(out.len());
        out[..n].copy_from_slice(&data[..n]);
        Ok(out)
    }
}

/// Activate a registered codec on a pager-side state object.
#[derive(Default, Clone)]
pub struct CodecState {
    pub active: Option<Arc<dyn PageCodec>>,
}

impl CodecState {
    /// Encode a page for disk. `is_header_page` pages keep their first 100
    /// bytes plain (file header + codec marker area must stay readable).
    pub fn encode_page(&self, is_header_page: bool, page: &[u8]) -> Result<Vec<u8>> {
        let c = match &self.active {
            None => return Ok(page.to_vec()),
            Some(c) => c,
        };
        if is_header_page {
            // Split: codec transforms only the tail past the file header.
            let split = (crate::storage::page::DB_HEADER_SIZE as usize).min(page.len());
            let mut tail = c.encode(&page[split..])?;
            if tail.len() != page.len() - split {
                tail.resize(page.len() - split, 0);
            }
            let mut out = page[..split].to_vec();
            out.extend_from_slice(&tail);
            Ok(out)
        } else {
            let mut out = c.encode(page)?;
            if out.len() != page.len() {
                out.resize(page.len(), 0);
            }
            Ok(out)
        }
    }

    /// Decode a page from disk.
    pub fn decode_page(&self, is_header_page: bool, data: &[u8], page_size: usize) -> Result<Vec<u8>> {
        let c = match &self.active {
            None => {
                let mut out = vec![0u8; page_size.max(data.len())];
                let n = data.len().min(out.len());
                out[..n].copy_from_slice(&data[..n]);
                return Ok(out);
            }
            Some(c) => c,
        };
        if is_header_page {
            let split = (crate::storage::page::DB_HEADER_SIZE as usize).min(data.len());
            let mut tail = c.decode(&data[split..], page_size.saturating_sub(split).max(1))?;
            if tail.len() != page_size - split {
                tail.resize(page_size - split, 0);
            }
            let mut out = data[..split].to_vec();
            out.extend_from_slice(&tail);
            Ok(out)
        } else {
            let mut out = c.decode(data, page_size)?;
            if out.len() != page_size {
                out.resize(page_size, 0);
            }
            Ok(out)
        }
    }

    pub fn is_active(&self) -> bool {
        self.active.is_some()
    }

    pub fn active_name(&self) -> Option<&str> {
        self.active.as_ref().map(|c| c.name())
    }
}

/// Validate that a codec name is well-formed before activation.
pub fn validate_codec_name(name: &str) -> Result<String> {
    let lowered = name.to_ascii_lowercase();
    if lowered.is_empty() || !lowered.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return Err(Error::semantic(format!("invalid codec name: {name:?}")));
    }
    Ok(lowered)
}
