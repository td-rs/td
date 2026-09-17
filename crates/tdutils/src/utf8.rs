//! Port of `td/utils/utf8.{h,cpp}`: the byte-level UTF-8 checks the
//! Telegram layer leans on (`check_utf8` allows the C++ "read the NUL
//! terminator past the end" shape — out-of-bounds reads are modeled as
//! reading 0, which reproduces both the accept-at-exact-EOF and the
//! reject-truncated-multibyte outcomes), plus the search-word splitter used
//! by `Hints` (M.4.4).

use crate::unicode::{prepare_search_character, remove_diacritics};

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

/// `td::next_utf8_unsafe` (utf8.cpp): decode one code point from the front of
/// `bytes`, returning it with the number of bytes consumed. Like the C++, the
/// continuation bytes are masked without validation; the C++ "unsafe" reads
/// past the slice end become panics here (every caller iterates a full
/// buffer, as the C++ end-based loops do).
pub fn next_utf8_unsafe(bytes: &[u8]) -> (u32, usize) {
    let a = bytes[0];
    if a & 0x80 == 0 {
        (a as u32, 1)
    } else if a & 0x20 == 0 {
        (((a & 0x1f) as u32) << 6 | (bytes[1] & 0x3f) as u32, 2)
    } else if a & 0x10 == 0 {
        (
            ((a & 0x0f) as u32) << 12 | ((bytes[1] & 0x3f) as u32) << 6 | (bytes[2] & 0x3f) as u32,
            3,
        )
    } else if a & 0x08 == 0 {
        (
            ((a & 0x07) as u32) << 18
                | ((bytes[1] & 0x3f) as u32) << 12
                | ((bytes[2] & 0x3f) as u32) << 6
                | (bytes[3] & 0x3f) as u32,
            4,
        )
    } else {
        // C++ UNREACHABLE().
        panic!("next_utf8_unsafe: invalid leading byte {a:#x}");
    }
}

/// `td::append_utf8_character` (utf8.cpp): append the UTF-8 encoding of
/// `code` to `out`.
pub fn append_utf8_character(out: &mut Vec<u8>, code: u32) {
    if code <= 0x7f {
        out.push(code as u8);
    } else if code <= 0x7ff {
        out.push((0xc0 | (code >> 6)) as u8);
        out.push((0x80 | (code & 0x3f)) as u8);
    } else if code <= 0xffff {
        out.push((0xe0 | (code >> 12)) as u8);
        out.push((0x80 | ((code >> 6) & 0x3f)) as u8);
        out.push((0x80 | (code & 0x3f)) as u8);
    } else {
        out.push((0xf0 | (code >> 18)) as u8);
        out.push((0x80 | ((code >> 12) & 0x3f)) as u8);
        out.push((0x80 | ((code >> 6) & 0x3f)) as u8);
        out.push((0x80 | (code & 0x3f)) as u8);
    }
}

/// `td::utf8_get_search_words` (utf8.cpp): split into lowercased search words
/// — punctuation and symbols become word separators (`prepare_search_character`
/// maps them to `' '` or `0`), diacritics are stripped, and casing is folded.
pub fn utf8_get_search_words(s: &str) -> Vec<String> {
    let bytes = s.as_bytes();
    let mut words: Vec<Vec<u8>> = Vec::new();
    let mut in_word = false;
    let mut word: Vec<u8> = Vec::new();
    let mut pos = 0usize;
    while pos < bytes.len() {
        let (code, used) = next_utf8_unsafe(&bytes[pos..]);
        pos += used;

        let code = prepare_search_character(code);
        if code == 0 {
            continue;
        }
        if code == b' ' as u32 {
            if in_word {
                words.push(std::mem::take(&mut word));
                in_word = false;
            }
        } else {
            in_word = true;
            let code = remove_diacritics(code);
            append_utf8_character(&mut word, code);
        }
    }
    if in_word {
        words.push(word);
    }
    words
        .into_iter()
        .map(|word| String::from_utf8(word).expect("search words are valid UTF-8 for a &str input"))
        .collect()
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

/// `td::utf8_utf16_length` (utf8.cpp:159-164): the length in UTF-16 code
/// units — the offset unit of every message entity.
pub fn utf8_utf16_length(s: &str) -> usize {
    s.bytes().fold(0, |acc, c| {
        acc + usize::from(is_utf8_character_first_code_unit(c)) + usize::from((c & 0xf8) == 0xf0)
    })
}

/// `td::utf8_utf16_truncate` (utf8.cpp:166-180): the longest prefix of at
/// most `length` UTF-16 code units, cut at a code-point boundary.
pub fn utf8_utf16_truncate(s: &str, length: usize) -> &str {
    let mut length_left = length;
    for (i, b) in s.bytes().enumerate() {
        if is_utf8_character_first_code_unit(b) {
            if length_left == 0 {
                return &s[..i];
            }
            length_left -= 1;
            if b >= 0xf0 {
                // >= 4 bytes in symbol => surrogate pair. The C++ `size_t`
                // underflows to SIZE_MAX here and the truncation is
                // silently defeated for the rest of the string; preserve
                // that quirk verbatim (a truncate(2) that starts a
                // surrogate pair at its last unit returns the whole input).
                length_left = length_left.wrapping_sub(1);
            }
        }
    }
    s
}

/// `td::utf8_utf16_substr(str, offset)` (utf8.cpp:182-187).
pub fn utf8_utf16_substr(s: &str, offset: usize) -> &str {
    if offset == 0 {
        return s;
    }
    let offset_pos = utf8_utf16_truncate(s, offset).len();
    &s[offset_pos..]
}

/// `td::utf8_utf16_substr(str, offset, length)` (utf8.cpp:189-191).
pub fn utf8_utf16_substr_range(s: &str, offset: usize, length: usize) -> &str {
    utf8_utf16_truncate(utf8_utf16_substr(s, offset), length)
}

/// `td::utf8_length` (utf8.h:23-29): the length in Unicode characters.
pub fn utf8_length(s: &str) -> usize {
    s.bytes().fold(0, |acc, c| {
        acc + usize::from(is_utf8_character_first_code_unit(c))
    })
}

/// `td::utf8_truncate` (utf8.h:76-89): the longest prefix of at most
/// `length` Unicode characters.
pub fn utf8_truncate(s: &str, length: usize) -> &str {
    let mut length_left = length;
    for (i, b) in s.bytes().enumerate() {
        if is_utf8_character_first_code_unit(b) {
            if length_left == 0 {
                return &s[..i];
            }
            length_left -= 1;
        }
    }
    s
}

/// `td::utf8_substr(str, offset)` (utf8.h:92-97).
pub fn utf8_substr(s: &str, offset: usize) -> &str {
    if offset == 0 {
        return s;
    }
    let offset_pos = utf8_truncate(s, offset).len();
    &s[offset_pos..]
}

/// `td::utf8_substr(str, offset, length)` (utf8.h:99-101).
pub fn utf8_substr_range(s: &str, offset: usize, length: usize) -> &str {
    utf8_truncate(utf8_substr(s, offset), length)
}

/// `td::utf8_to_lower` (utf8.cpp:104-113): per-code-point `unicode_to_lower`
/// re-encoded back into UTF-8.
pub fn utf8_to_lower(s: &str) -> String {
    let mut result = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut pos = 0usize;
    while pos < bytes.len() {
        let (code, width) = next_utf8_unsafe(&bytes[pos..]);
        append_utf8_character(&mut result, crate::unicode::unicode_to_lower(code));
        pos += width;
    }
    // The mapping only lowers code points, so the re-encoding stays valid UTF-8.
    String::from_utf8(result).unwrap_or_default()
}

#[cfg(test)]
mod utf16_tests {
    use super::*;

    /// utf8.cpp:159-164 — ASCII counts one, 4-byte characters two; 2-byte
    /// BMP characters (here Cyrillic) are one UTF-16 code unit each (the
    /// C++ per-byte sum: the lead byte is a first code unit, the
    /// continuation byte is not).
    #[test]
    fn utf16_length_counts_surrogate_pairs() {
        assert_eq!(utf8_utf16_length("abc"), 3);
        assert_eq!(utf8_utf16_length("аб"), 2);
        assert_eq!(utf8_utf16_length("\u{1F42D}"), 2, "a surrogate pair");
        assert_eq!(utf8_utf16_length("a\u{1F42D}b"), 4);
        assert_eq!(utf8_utf16_length(""), 0);
    }

    /// utf8.cpp:166-191 — truncation at code-point boundaries and the
    /// substr compositions. `truncate(s, 2)` returns the whole input: the
    /// second code unit of the surrogate pair underflows the C++ `size_t`
    /// and the truncation is defeated (pinned C++ quirk, kept verbatim).
    #[test]
    fn utf16_truncate_and_substr() {
        let s = "a\u{1F42D}bc";
        assert_eq!(utf8_utf16_truncate(s, 3), "a\u{1F42D}");
        assert_eq!(utf8_utf16_truncate(s, 2), s);
        assert_eq!(utf8_utf16_truncate(s, 1), "a");
        assert_eq!(utf8_utf16_truncate(s, 10), s);
        assert_eq!(utf8_utf16_substr(s, 1), "\u{1F42D}bc");
        assert_eq!(utf8_utf16_substr_range(s, 1, 3), "\u{1F42D}b");
        assert_eq!(utf8_utf16_substr(s, 0), s);
    }
}
