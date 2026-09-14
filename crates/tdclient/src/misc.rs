//! Port of `td/telegram/misc.cpp`: `clean_input_string` — the
//! server-parity string sanitizer applied to user-provided strings before
//! they reach the handlers (UTF-8 check, control-character replacement,
//! the 35 000-byte length clamp, and the directional-mark replacement).

use tdutils::utf8::{check_utf8, is_utf8_character_first_code_unit};

/// `LENGTH_LIMIT` — the server-side string limit.
const LENGTH_LIMIT: usize = 35000;

/// `replace_offending_characters` (misc.cpp:62-77):
/// `(U+200F|U+200E){N}(U+200F|U+200E)` -> `(U+200C){N}$2`.
fn replace_offending_characters(s: &mut [u8]) {
    // The C++ reads up to two bytes past `pos` unconditionally (safe on
    // NUL-terminated strings); out-of-bounds reads are modeled as 0.
    fn at(s: &[u8], i: usize) -> u8 {
        s.get(i).copied().unwrap_or(0)
    }
    let mut pos = 0usize;
    while pos < s.len() {
        if at(s, pos) == 0xe2
            && at(s, pos + 1) == 0x80
            && (at(s, pos + 2) == 0x8e || at(s, pos + 2) == 0x8f)
        {
            while at(s, pos + 3) == 0xe2
                && at(s, pos + 4) == 0x80
                && (at(s, pos + 5) == 0x8e || at(s, pos + 5) == 0x8f)
            {
                s[pos + 2] = 0x8c;
                pos += 3;
            }
            pos += 2;
        }
        pos += 1;
    }
}

/// `td::clean_input_string` (misc.cpp:79-161): valid UTF-8 only; control
/// characters 0-9, 11, 12 and 14-32 become spaces, `\r` is dropped, `\n`
/// survives; the UTF-8 line/paragraph separators U+2028-U+202A and the
/// combining-char vertical lines U+0333/U+033F/U+030A are removed; the
/// result is clamped to `LENGTH_LIMIT` bytes at a character boundary.
pub fn clean_input_string(s: &str) -> Option<String> {
    if !check_utf8(s.as_bytes()) {
        return None;
    }

    let str_bytes = s.as_bytes();
    let str_size = str_bytes.len();
    let mut out: Vec<u8> = Vec::with_capacity(str_size);

    let mut pos = 0usize;
    while pos < str_size {
        let c = str_bytes[pos];
        match c {
            // Control characters become spaces; '\n' (10) is allowed and
            // '\r' (13) is dropped by its own arm below.
            0..=9 | 11 | 12 | 14..=32 => out.push(b' '),
            b'\r' => {}
            _ => {
                // remove \xe2\x80[\xa8-\xae]
                if c == 0xe2 && pos + 2 < str_size {
                    let next = str_bytes[pos + 1];
                    if next == 0x80 {
                        let next2 = str_bytes[pos + 2];
                        if (0xa8..=0xae).contains(&next2) {
                            pos += 3;
                            continue;
                        }
                    }
                }
                // remove vertical lines \xcc[\xb3\xbf\x8a]
                if c == 0xcc && pos + 1 < str_size {
                    let next = str_bytes[pos + 1];
                    if next == 0xb3 || next == 0xbf || next == 0x8a {
                        pos += 2;
                        continue;
                    }
                }
                out.push(c);
            }
        }
        pos += 1;
        // The C++ clamps once a first-code-unit byte would exceed the limit.
        if out.len() >= LENGTH_LIMIT - 3 && is_utf8_character_first_code_unit(out[out.len() - 1]) {
            out.pop();
            break;
        }
    }

    replace_offending_characters(&mut out);

    // The sanitizer never breaks UTF-8: removals are whole characters and
    // the clamp cuts at a first-code-unit boundary.
    Some(String::from_utf8(out).expect("clean_input_string preserves UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean(s: &str) -> Option<String> {
        clean_input_string(s)
    }

    #[test]
    fn passes_plain_and_unicode_through() {
        assert_eq!(clean("hello").unwrap(), "hello");
        assert_eq!(clean("привет 🦀").unwrap(), "привет 🦀");
        assert_eq!(clean("").unwrap(), "");
    }

    #[test]
    fn control_characters_become_spaces() {
        assert_eq!(clean("a\u{1}b").unwrap(), "a b");
        assert_eq!(clean("a\u{9}b").unwrap(), "a b");
        assert_eq!(clean("a\u{b}b").unwrap(), "a b");
        assert_eq!(clean("a\u{20}b").unwrap(), "a b");
        // \n allowed, \r dropped
        assert_eq!(clean("a\nb").unwrap(), "a\nb");
        assert_eq!(clean("a\r\nb").unwrap(), "a\nb");
    }

    #[test]
    fn line_separators_and_vertical_lines_removed() {
        assert_eq!(clean("a\u{2028}b").unwrap(), "ab");
        assert_eq!(clean("a\u{2029}b").unwrap(), "ab");
        assert_eq!(clean("a\u{202a}b").unwrap(), "ab");
        // U+0333 (combining double low line) and friends
        assert_eq!(clean("a\u{333}b").unwrap(), "ab");
        assert_eq!(clean("a\u{33f}b").unwrap(), "ab");
        assert_eq!(clean("a\u{30a}b").unwrap(), "ab");
        // U+202B is inside the removed \xe2\x80[\xa8-\xae] range
        assert_eq!(clean("a\u{202b}b").unwrap(), "ab");
    }

    #[test]
    fn directional_marks_are_replaced() {
        // Two RLMs in a row: the interior ones become ZWNJ.
        assert_eq!(clean("a\u{200f}\u{200f}b").unwrap(), "a\u{200c}\u{200f}b");
        // LRM behaves the same.
        assert_eq!(
            clean("a\u{200e}\u{200e}\u{200e}b").unwrap(),
            "a\u{200c}\u{200c}\u{200e}b"
        );
    }

    #[test]
    fn utf8_guard_is_the_td_byte_check() {
        // Strings are valid UTF-8 by construction in Rust, so the guard can
        // only fire through the byte-level check it delegates to
        // (td::check_utf8): 0xc3 must start a two-byte sequence.
        assert!(!tdutils::utf8::check_utf8(&[0x41, 0xc3, 0x28]));
        assert!(tdutils::utf8::check_utf8(&[0x41, 0xc3, 0xa9]));
    }

    #[test]
    fn length_clamp_at_character_boundary() {
        let long = "б".repeat(30000); // 2 bytes each
        let cleaned = clean(&long).unwrap();
        assert!(cleaned.len() <= LENGTH_LIMIT);
        assert!(cleaned.chars().all(|c| c == 'б'));
        // ASCII clamps to LENGTH_LIMIT - 4 bytes: the check pops the byte
        // that reached the LENGTH_LIMIT - 3 watermark (the C++ `new_size--`).
        let ascii = "a".repeat(40000);
        assert_eq!(clean(&ascii).unwrap().len(), LENGTH_LIMIT - 4);
    }
}
