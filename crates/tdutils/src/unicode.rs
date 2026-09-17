//! Port of the `tdutils/td/utils/unicode.cpp` character-preparation functions
//! used by the search machinery (M.4.4, `Hints`): `prepare_search_character`
//! and `remove_diacritics`. The backing tables live in the generated
//! [`crate::unicode_tables`] module (byte-extracted from tdlib 1.8.67).
//!
//! The C++ `to_lower_table`/`to_lower_ranges` pair (behind `unicode_to_lower`,
//! consumed by `utf8_to_lower` and the misc string helpers) has no M.4.4
//! consumer and is not carried yet; add it with its first user.

use crate::unicode_simple_category_tables::{
    SIMPLE_CATEGORY_JUMP_POS, SIMPLE_CATEGORY_RANGES, SIMPLE_CATEGORY_TABLE,
};
use crate::unicode_tables::{
    PREPARE_SEARCH_CHARACTER_RANGES, PREPARE_SEARCH_CHARACTER_TABLE, WITHOUT_DIACRITICS_RANGES,
    WITHOUT_DIACRITICS_TABLE,
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

/// `td::UnicodeSimpleCategory` (unicode.h:13) — the discriminants are the
/// table codes; `Separator` = 4.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UnicodeSimpleCategory {
    Unknown,
    Letter,
    DecimalNumber,
    Number,
    Separator,
}

fn simple_category_from_code(code: u32) -> UnicodeSimpleCategory {
    match code {
        0 => UnicodeSimpleCategory::Unknown,
        1 => UnicodeSimpleCategory::Letter,
        2 => UnicodeSimpleCategory::DecimalNumber,
        3 => UnicodeSimpleCategory::Number,
        4 => UnicodeSimpleCategory::Separator,
        // the committed tables only encode 0-4
        _ => panic!("bad simple-category code {code}"),
    }
}

/// C++ `get_unicode_simple_category` (unicode.cpp:1281-1294): the direct
/// 128-entry table, then the jump-point-accelerated walk over the
/// pair-encoded `(base << 5 | category)` ranges ending in the
/// `0xFFFFFFFF` sentinel; the transform `(code << 5) + 30` keeps the
/// walk's `<=` comparison equivalent to "greatest base not exceeding the
/// code".
pub fn get_unicode_simple_category(code: u32) -> UnicodeSimpleCategory {
    if code < 128 {
        return simple_category_from_code(SIMPLE_CATEGORY_TABLE[code as usize] as u32);
    }
    let jump_pos_index = if code <= 0x20000 {
        (code >> 7) as usize
    } else {
        ((0x20000 >> 7) - (0x20000 >> 16) + (code >> 16)) as usize
    };
    let mut it = SIMPLE_CATEGORY_JUMP_POS[jump_pos_index] as usize;
    let transformed = (code << 5) + 30;
    while SIMPLE_CATEGORY_RANGES[it] <= transformed {
        it += 1;
    }
    simple_category_from_code(SIMPLE_CATEGORY_RANGES[it - 1] & 31)
}
