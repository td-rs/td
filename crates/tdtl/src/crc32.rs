//! CRC-32 used for TL constructor-id assignment, matching td's
//! `td/generate/tl-parser/crc32.c` (`compute_crc32`): reflected polynomial
//! 0xedb88320, initial value 0xffffffff, final xor 0xffffffff — i.e. the
//! standard CRC-32/ISO-HDLC also used by zlib.

const fn build_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xedb88320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

static TABLE: [u32; 256] = build_table();

/// Mirrors `compute_crc32(data, len)`.
pub fn compute_crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &b in data {
        crc = TABLE[((crc ^ b as u32) & 0xff) as usize] ^ (crc >> 8);
    }
    crc ^ 0xffff_ffff
}

#[cfg(test)]
mod tests {
    use super::compute_crc32;

    #[test]
    fn known_vectors() {
        // Standard CRC-32/ISO-HDLC check values.
        assert_eq!(compute_crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(compute_crc32(b""), 0);
        // The canonical form of the `vector` constructor (`vector#1cb5c415
        // {t:Type} # [ t ] = Vector t`): braces and the magic are stripped,
        // fields become `name:type` — its on-the-wire id is 0x1cb5c415.
        assert_eq!(
            compute_crc32(b"vector t:Type # [ t ] = Vector t"),
            0x1cb5_c415
        );
    }
}
