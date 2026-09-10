//! SQLite 100-byte database header — fileformat2 §1.2.

/// The canonical 16-byte magic string.
pub const MAGIC: &[u8; 16] = b"SQLite format 3\0";

/// File header fields the interop layer cares about.
#[derive(Clone, Debug)]
pub struct FileHeaderInfo {
    pub page_size: u32,
    /// Bytes of reserved space at the end of every page (usually 0).
    pub reserved: u8,
    /// Write version: 1 = legacy (rollback), 2 = WAL.
    pub write_version: u8,
    pub read_version: u8,
    pub change_counter: u32,
    /// Database size in pages. May be 0 or stale (WAL or pre-commit).
    pub db_size_pages: u32,
    pub freelist_head: u32,
    pub freelist_count: u32,
    pub schema_cookie: u32,
    pub schema_format: u32,
    pub largest_root_btree: u32,
    /// 1 = UTF-8, 2 = UTF-16le, 3 = UTF-16be.
    pub text_encoding: u32,
    pub user_version: u32,
    pub application_id: u32,
    pub version_valid_for: u32,
    pub sqlite_version: u32,
}

impl FileHeaderInfo {
    /// Parse the 100-byte header. `first_page` is the full first page
    /// (or at least its first 100 bytes).
    pub fn parse(first_page: &[u8]) -> Result<Self, String> {
        if first_page.len() < 100 {
            return Err("file too small for SQLite header".into());
        }
        if &first_page[0..16] != MAGIC {
            return Err("not a SQLite format 3 file".into());
        }
        let be16 = |o: usize| u16::from_be_bytes([first_page[o], first_page[o + 1]]) as u32;
        let be32 = |o: usize| {
            u32::from_be_bytes([
                first_page[o],
                first_page[o + 1],
                first_page[o + 2],
                first_page[o + 3],
            ])
        };
        let mut page_size = be16(16);
        if page_size == 1 {
            page_size = 65536;
        }
        if !page_size.is_power_of_two() || page_size < 512 {
            return Err(format!("invalid page size {page_size}"));
        }
        Ok(Self {
            page_size,
            reserved: first_page[20],
            write_version: first_page[18],
            read_version: first_page[19],
            change_counter: be32(24),
            db_size_pages: be32(28),
            freelist_head: be32(32),
            freelist_count: be32(36),
            schema_cookie: be32(40),
            schema_format: be32(44),
            largest_root_btree: be32(52),
            text_encoding: be32(56),
            user_version: be32(60),
            application_id: be32(68),
            version_valid_for: be32(92),
            sqlite_version: be32(96),
        })
    }

    /// Usable page size (page size minus reserved bytes).
    pub fn usable(&self) -> u32 {
        self.page_size - self.reserved as u32
    }
}

/// Build a complete 100-byte header for a freshly written database.
#[allow(clippy::too_many_arguments)]
pub fn build_header(
    page_size: u32,
    db_size_pages: u32,
    change_counter: u32,
    schema_cookie: u32,
    user_version: u32,
    application_id: u32,
) -> [u8; 100] {
    build_header_enc(
        false,
        page_size,
        db_size_pages,
        change_counter,
        schema_cookie,
        user_version,
        application_id,
        1,
        0,
        0,
    )
}

/// [`build_header`] with an explicit text encoding (1/2/3 — see
/// fileformat2 header field 56), auto-vacuum largest-root page (field
/// 52: non-zero = pointer-map pages exist) and incremental-vacuum flag
/// (field 64: 1 = INCREMENTAL mode).
#[allow(clippy::too_many_arguments)]
pub fn build_header_enc(
    journal_wal: bool,
    page_size: u32,
    db_size_pages: u32,
    change_counter: u32,
    schema_cookie: u32,
    user_version: u32,
    application_id: u32,
    text_encoding: u32,
    largest_root_btree: u32,
    incremental_vacuum: u32,
) -> [u8; 100] {
    let mut h = [0u8; 100];
    h[0..16].copy_from_slice(MAGIC);
    // Page size: big-endian u16; 65536 encoded as 1.
    let ps = if page_size == 65536 {
        1u16
    } else {
        page_size as u16
    };
    h[16..18].copy_from_slice(&ps.to_be_bytes());
    // Write/read version: 2/2 = persistent WAL (SQLite reports wal
    // even with no sidecar); 1/1 = legacy rollback journal mode.
    h[18] = if journal_wal { 2 } else { 1 };
    h[19] = h[18];
    // Reserved bytes per page: 0 (dense pages).
    h[20] = 0;
    // Payload fractions (fixed by the format).
    h[21] = 64;
    h[22] = 32;
    h[23] = 32;
    h[24..28].copy_from_slice(&change_counter.to_be_bytes());
    h[28..32].copy_from_slice(&db_size_pages.to_be_bytes());
    // Empty freelist.
    h[32..36].copy_from_slice(&0u32.to_be_bytes());
    h[36..40].copy_from_slice(&0u32.to_be_bytes());
    h[40..44].copy_from_slice(&schema_cookie.to_be_bytes());
    // Schema format 4 (modern: DESC indexes, boolean... default since 3.3).
    h[44..48].copy_from_slice(&4u32.to_be_bytes());
    // Default page cache size: 0 (unspecified).
    h[48..52].copy_from_slice(&0u32.to_be_bytes());
    // Largest root b-tree: 0 = no auto-vacuum pointer-map pages;
    // non-zero = the largest root page (auto-vacuum capable).
    h[52..56].copy_from_slice(&largest_root_btree.to_be_bytes());
    // Text encoding: 1 = UTF-8, 2 = UTF-16le, 3 = UTF-16be.
    h[56..60].copy_from_slice(&text_encoding.to_be_bytes());
    h[60..64].copy_from_slice(&user_version.to_be_bytes());
    // Incremental vacuum mode: 0 = FULL auto-vacuum, 1 = INCREMENTAL.
    h[64..68].copy_from_slice(&incremental_vacuum.to_be_bytes());
    h[68..72].copy_from_slice(&application_id.to_be_bytes());
    // 72..92 reserved zeros.
    // Version-valid-for must equal the change counter.
    h[92..96].copy_from_slice(&change_counter.to_be_bytes());
    // SQLITE_VERSION_NUMBER of the writer (3.53.1 form: 3053001).
    h[96..100].copy_from_slice(&3_053_001u32.to_be_bytes());
    h
}

/// True when the buffer begins with the SQLite magic (cheap sniff).
pub fn has_magic(buf: &[u8]) -> bool {
    buf.len() >= 16 && &buf[0..16] == MAGIC
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        let h = build_header(4096, 42, 7, 3, 99, 12345);
        let info = FileHeaderInfo::parse(&h).unwrap();
        assert_eq!(info.page_size, 4096);
        assert_eq!(info.db_size_pages, 42);
        assert_eq!(info.change_counter, 7);
        assert_eq!(info.schema_cookie, 3);
        assert_eq!(info.schema_format, 4);
        assert_eq!(info.text_encoding, 1);
        assert_eq!(info.user_version, 99);
        assert_eq!(info.application_id, 12345);
        assert_eq!(info.version_valid_for, 7);
        assert_eq!(info.freelist_head, 0);
        assert_eq!(info.largest_root_btree, 0);
    }

    #[test]
    fn header_auto_vacuum_fields() {
        // FULL: largest root non-zero, incremental 0.
        let h = build_header_enc(false, 4096, 9, 1, 1, 0, 0, 1, 9, 0);
        let info = FileHeaderInfo::parse(&h).unwrap();
        assert_eq!(info.largest_root_btree, 9);
        assert_eq!(h[64..68], 0u32.to_be_bytes());
        // INCREMENTAL: flag byte set.
        let h = build_header_enc(false, 4096, 9, 1, 1, 0, 0, 1, 9, 1);
        assert_eq!(h[64..68], 1u32.to_be_bytes());
        assert_eq!(FileHeaderInfo::parse(&h).unwrap().largest_root_btree, 9);
    }

    #[test]
    fn page_size_64k_encoding() {
        let h = build_header(65536, 1, 1, 1, 0, 0);
        assert_eq!(h[16], 0);
        assert_eq!(h[17], 1);
        let info = FileHeaderInfo::parse(&h).unwrap();
        assert_eq!(info.page_size, 65536);
    }

    #[test]
    fn rejects_non_sqlite() {
        assert!(FileHeaderInfo::parse(&[0u8; 100]).is_err());
        assert!(!has_magic(&[0u8; 32]));
        assert!(has_magic(b"SQLite format 3\0xxxx"));
    }
}
