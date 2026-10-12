//! Port of the `tdutils/td/utils/unicode.cpp` character-preparation functions
//! used by the search machinery (M.4.4, `Hints`): `prepare_search_character`
//! and `remove_diacritics`. The backing tables live in the generated
//! `crate::unicode_tables` module (byte-extracted from tdlib 1.8.67).

use crate::unicode_tables::{
    PREPARE_SEARCH_CHARACTER_RANGES, PREPARE_SEARCH_CHARACTER_TABLE, TO_LOWER_RANGES,
    TO_LOWER_TABLE, UNICODE_SIMPLE_CATEGORY_JUMP_POS, UNICODE_SIMPLE_CATEGORY_RANGES,
    UNICODE_SIMPLE_CATEGORY_TABLE, WITHOUT_DIACRITICS_RANGES, WITHOUT_DIACRITICS_TABLE,
};

/// The C++ `TABLE_SIZE`: codes below it hit the direct tables, the rest the
/// range binary search.
const TABLE_SIZE: u32 = 1280;

/// C++ `binary_search_ranges`: locate `(range_begin, transformation)` over the
/// pair-encoded `ranges` array and apply the transformation to `code`.
///
/// The odd entries encode the replacement: a negative `t` is an offset form
/// (`code - range_begin + !t`), a value `<= 0x10ffff` is the replacement
/// itself, and the `0x200000 + k` forms map `code` to
/// `code & !1` / `code | 1` / `(code - 1) | 1`.
fn binary_search_ranges(ranges: &[i32], code: u32) -> u32 {
    if code > 0x10ffff {
        return 0;
    }
    let code_int = code as i32;
    let mut l = 0usize;
    let mut r = ranges.len();
    while l < r {
        // The C++ pair-wise midpoint; every array here is long enough that
        // `m + 1` stays in bounds (the C++ reads the same slots).
        let m = ((l + r + 2) >> 2) << 1;
        debug_assert!(m + 1 < ranges.len(), "binary_search_ranges out of bounds");
        if ranges[m] <= code_int {
            l = m;
        } else {
            r = m - 2;
        }
    }

    let t = ranges[l + 1];
    if t < 0 {
        let value = code as i64 - ranges[l] as i64 + (!t) as i64;
        return value as u32;
    }
    if t <= 0x10ffff {
        return t as u32;
    }
    match t - 0x200000 {
        0 => code & !1,
        1 => code | 1,
        2 => (code - 1) | 1,
        // C++ LOG(FATAL); unreachable with the committed tables.
        _ => panic!("binary_search_ranges: bad transformation {t} for code {code}"),
    }
}

/// C++ `prepare_search_character`: map a code point to its search form —
/// lowercased, punctuation and symbols collapsed to `' '` (word separator) or
/// dropped (`0`).
pub fn prepare_search_character(code: u32) -> u32 {
    if code < TABLE_SIZE {
        PREPARE_SEARCH_CHARACTER_TABLE[code as usize] as u32
    } else {
        binary_search_ranges(PREPARE_SEARCH_CHARACTER_RANGES, code)
    }
}

/// C++ `remove_diacritics`: map a code point to its base (non-combining)
/// form; combining marks and modifiers map to themselves and are dropped by
/// the caller (`utf8_get_search_words`).
pub fn remove_diacritics(code: u32) -> u32 {
    if code < TABLE_SIZE {
        WITHOUT_DIACRITICS_TABLE[code as usize] as u32
    } else {
        binary_search_ranges(WITHOUT_DIACRITICS_RANGES, code)
    }
}

/// C++ `unicode_to_lower` (unicode.cpp:1345-1352): the single-code-point
/// lowercase mapping behind `utf8_to_lower`.
pub fn unicode_to_lower(code: u32) -> u32 {
    if code < TABLE_SIZE {
        TO_LOWER_TABLE[code as usize] as u32
    } else {
        binary_search_ranges(TO_LOWER_RANGES, code)
    }
}

/// `td::UnicodeSimpleCategory` (tdutils/unicode.h:13).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnicodeSimpleCategory {
    Unknown,
    Letter,
    DecimalNumber,
    Number,
    Separator,
}

fn simple_category_from_byte(byte: u8) -> UnicodeSimpleCategory {
    match byte {
        0 => UnicodeSimpleCategory::Unknown,
        1 => UnicodeSimpleCategory::Letter,
        2 => UnicodeSimpleCategory::DecimalNumber,
        3 => UnicodeSimpleCategory::Number,
        4 => UnicodeSimpleCategory::Separator,
        _ => unreachable!("unicode_simple_category_table carries only 0..=4"),
    }
}

/// `td::get_unicode_simple_category` (tdutils/unicode.cpp:1281-1294): the
/// ASCII table below 128, then a jump-indexed scan over the encoded
/// `(code << 5) | category` range list.
pub fn get_unicode_simple_category(code: u32) -> UnicodeSimpleCategory {
    if code < 128 {
        return simple_category_from_byte(UNICODE_SIMPLE_CATEGORY_TABLE[code as usize]);
    }
    let jump_pos_index = if code <= 0x20000 {
        (code >> 7) as usize
    } else {
        ((0x20000 >> 7) - (0x20000 >> 16) + (code >> 16)) as usize
    };
    let mut idx = UNICODE_SIMPLE_CATEGORY_JUMP_POS[jump_pos_index] as usize;
    let encoded = (code << 5) + 30;
    while UNICODE_SIMPLE_CATEGORY_RANGES[idx] <= encoded {
        idx += 1;
    }
    simple_category_from_byte((UNICODE_SIMPLE_CATEGORY_RANGES[idx - 1] & 31) as u8)
}

#[cfg(test)]
mod simple_category_tests {
    use super::*;

    /// Pins verified against the extracted table (the C++ algorithm, run in
    /// lockstep): ASCII classes, a non-ASCII Nd, a No, an astral letter,
    /// the plane-2 boundary and the hashtag-special characters.
    #[test]
    fn categories_match_unicode() {
        use UnicodeSimpleCategory::*;
        assert_eq!(get_unicode_simple_category(b'a' as u32), Letter);
        assert_eq!(get_unicode_simple_category(b'Z' as u32), Letter);
        assert_eq!(get_unicode_simple_category(b'7' as u32), DecimalNumber);
        assert_eq!(get_unicode_simple_category(b'_' as u32), Unknown);
        assert_eq!(get_unicode_simple_category(b' ' as u32), Separator);
        assert_eq!(
            get_unicode_simple_category(0x0660),
            DecimalNumber,
            "Arabic-Indic zero"
        );
        assert_eq!(
            get_unicode_simple_category(0x00B2),
            Number,
            "superscript two"
        );
        assert_eq!(get_unicode_simple_category(0x05D0), Letter, "alef");
        assert_eq!(get_unicode_simple_category(0x1F42D), Unknown, "emoji is So");
        assert_eq!(
            get_unicode_simple_category(0x0305),
            Unknown,
            "combining marks are Unknown here"
        );
        assert_eq!(get_unicode_simple_category(0x20000), Letter, "plane-2 CJK");
        assert_eq!(get_unicode_simple_category(0x20001), Letter);
        assert_eq!(get_unicode_simple_category(0x200C), Unknown, "ZWJ");
        assert_eq!(get_unicode_simple_category(0x00B7), Unknown, "middle dot");
        assert_eq!(get_unicode_simple_category(0xD800), Unknown, "surrogate");
        assert_eq!(get_unicode_simple_category(0x10FFFF), Unknown);
    }

    /// Full-range parity pin for the Unicode 18.0 tables: FNV-1a over every
    /// code point 0..=0x110000 through each reader, matching a standalone
    /// g++ probe compiled from the verbatim arrays + readers of
    /// tdutils/td/utils/unicode.cpp @ 42e6a5259 (the probe lives in the
    /// 2026-09-28 upstream-sync session notes; regenerate by extracting the
    /// table blocks and the four functions verbatim).
    #[test]
    fn unicode18_tables_match_the_cpp_probe_checksums() {
        fn fnv1a(init: u64, values: impl Iterator<Item = u64>) -> u64 {
            values.fold(init, |h, v| (h ^ v).wrapping_mul(1099511628211))
        }
        const INIT: u64 = 1469598103934665603;

        let cat = fnv1a(
            INIT,
            (0..=0x110000u32).map(|code| get_unicode_simple_category(code) as u64),
        );
        assert_eq!(cat, 1443554385117807666, "get_unicode_simple_category");

        let prep = fnv1a(INIT, (0..=0x110000u32).map(prepare_search_character_u64));
        assert_eq!(prep, 3098223115303020054, "prepare_search_character");

        let low = fnv1a(INIT, (0..=0x110000u32).map(unicode_to_lower_u64));
        assert_eq!(low, 8290289319935536834, "unicode_to_lower");

        let dia = fnv1a(INIT, (0..=0x110000u32).map(remove_diacritics_u64));
        assert_eq!(dia, 3213732956951812771, "remove_diacritics");
    }

    fn prepare_search_character_u64(code: u32) -> u64 {
        prepare_search_character(code) as u64
    }
    fn unicode_to_lower_u64(code: u32) -> u64 {
        unicode_to_lower(code) as u64
    }
    fn remove_diacritics_u64(code: u32) -> u64 {
        remove_diacritics(code) as u64
    }
}
