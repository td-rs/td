//! CRC32 — the same polynomial tdlib uses for TL constructor ids
//! (`td/generate/tl-parser/crc32.c`), backed by `crc32fast`.

/// `td::crc32`: standard CRC-32 (IEEE 802.3, reflected, poly `0xEDB88320`).
pub fn crc32(data: &[u8]) -> u32 {
    crc32fast::hash(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_tl_constructor_ids() {
        // Canonical constructor strings verified against the C++ oracle in S07.
        assert_eq!(crc32(b"vector t:Type # [ t ] = Vector t"), 0x1CB5_C415);
        assert_eq!(crc32(b"int128 4*[ int ] = Int128"), 0x84CC_F7B7);
        // Standard CRC-32 check value.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }
}
