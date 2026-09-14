//! Port of `td/utils/utf8.{h,cpp}`: the byte-level UTF-8 checks the
//! Telegram layer leans on (`check_utf8` allows the C++ "read the NUL
//! terminator past the end" shape — out-of-bounds reads are modeled as
//! reading 0, which reproduces both the accept-at-exact-EOF and the
//! reject-truncated-multibyte outcomes).

/// `td::is_utf8_character_first_code_unit` (utf8.h).
#[inline]
pub fn is_utf8_character_first_code_unit(c: u8) -> bool {
    (c & 0xc0) != 0x80
}

/// `td::check_utf8` (utf8.cpp).
pub fn check_utf8(s: &[u8]) -> bool {
    // Reads one byte, modeling the C++ read-past-end as NUL.
    fn at(data: &[u8], pos: usize) -> u8 {
        data.get(pos).copied().unwrap_or(0)
    }
    let data_end = s.len();
    let mut pos = 0usize;
    loop {
        let a = at(s, pos);
        pos += 1;
        if a & 0x80 == 0 {
            // The C++ pointer has already been incremented, so the exact-EOF
            // check is `data == data_end + 1`.
            if pos == data_end + 1 {
                return true;
            }
            continue;
        }

        if a & 0x40 == 0 {
            return false;
        }

        let b = at(s, pos);
        pos += 1;
        if b & 0xc0 != 0x80 {
            return false;
        }
        if a & 0x20 == 0 {
            if a & 0x1e == 0 {
                return false;
            }
            continue;
        }

        let c = at(s, pos);
        pos += 1;
        if c & 0xc0 != 0x80 {
            return false;
        }
        if a & 0x10 == 0 {
            let x = ((a & 0x0f) as u32) << 6 | ((b & 0x20) as u32);
            if x == 0 || x == 0x360 {
                return false; // surrogates
            }
            continue;
        }

        let d = at(s, pos);
        pos += 1;
        if d & 0xc0 != 0x80 {
            return false;
        }
        if a & 0x08 == 0 {
            let t = ((a & 0x07) as u32) << 6 | ((b & 0x30) as u32);
            if t == 0 || t >= 0x110 {
                return false; // end of unicode
            }
            continue;
        }

        return false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_utf8_matches_cpp_outcomes() {
        assert!(check_utf8(b""));
        assert!(check_utf8(b"plain ascii"));
        assert!(check_utf8("привет".as_bytes()));
        assert!(check_utf8("🦀\u{10ffff}".as_bytes()));
        // Truncated multibyte sequences: the boundary byte reads as NUL and
        // fails the continuation-byte check.
        assert!(!check_utf8(&[0xc3]));
        assert!(!check_utf8(&[0xe2, 0x82]));
        assert!(!check_utf8(&[0xf0, 0x9f, 0x92]));
        // Overlong encodings and surrogate/cp-range violations.
        assert!(!check_utf8(&[0xc0, 0x80])); // (a & 0x1e) == 0
        assert!(!check_utf8(&[0xe0, 0x80, 0x80])); // x == 0
        assert!(!check_utf8(&[0xed, 0xa0, 0x80])); // surrogate D800
        assert!(!check_utf8(&[0xf4, 0x90, 0x80, 0x80])); // > 0x10ffff
        // Lone continuation bytes.
        assert!(!check_utf8(&[0x80]));
        assert!(!check_utf8(&[0xbf]));
        // Five-byte lead byte (a & 0x08 != 0 after the 4-byte chain).
        assert!(!check_utf8(&[0xf8, 0x80, 0x80, 0x80, 0x80]));
    }

    #[test]
    fn first_code_unit_predicate() {
        assert!(is_utf8_character_first_code_unit(b'a'));
        assert!(is_utf8_character_first_code_unit(0xc3));
        assert!(!is_utf8_character_first_code_unit(0x80));
        assert!(!is_utf8_character_first_code_unit(0xbf));
    }
}
