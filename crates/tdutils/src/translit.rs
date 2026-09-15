//! Port of `td/utils/translit.{h,cpp}` (tdlib 1.8.67): `get_word_transliterations`,
//! the Latin↔Cyrillic search helper behind `Hints`.
//!
//! Mapping notes:
//! - The C++ `FlatHashMap` simple rules and `vector<pair>` complex rules
//!   become a `match` and a `const` slice — same lookups, zero runtime setup.
//! - `Slice` walking (`ubegin`/`uend`, byte-pointer `pos += rule.first.size()`)
//!   becomes index arithmetic over `&[u8]`, exactly matching the C++ byte
//!   positions.

use crate::utf8::{append_utf8_character, next_utf8_unsafe};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    EnToRu,
    RuToEn,
}

/// C++ `get_en_to_ru_simple_rules` (lowercase ASCII keys, noted per arm).
fn en_to_ru_simple(code: u32) -> Option<&'static str> {
    Some(match code {
        0x61 => "а",  // a
        0x62 => "б",  // b
        0x63 => "к",  // c
        0x64 => "д",  // d
        0x65 => "е",  // e
        0x66 => "ф",  // f
        0x67 => "г",  // g
        0x68 => "х",  // h
        0x69 => "и",  // i
        0x6a => "й",  // j
        0x6b => "к",  // k
        0x6c => "л",  // l
        0x6d => "м",  // m
        0x6e => "н",  // n
        0x6f => "о",  // o
        0x70 => "п",  // p
        0x71 => "к",  // q
        0x72 => "р",  // r
        0x73 => "с",  // s
        0x74 => "т",  // t
        0x75 => "у",  // u
        0x76 => "в",  // v
        0x77 => "в",  // w
        0x78 => "кс", // x
        0x79 => "и",  // y
        0x7a => "з",  // z
        _ => return None,
    })
}

/// C++ `get_en_to_ru_complex_rules`.
const EN_TO_RU_COMPLEX: &[(&str, &str)] = &[
    ("ch", "ч"),
    ("ei", "ей"),
    ("ey", "ей"),
    ("ia", "ия"),
    ("iy", "ий"),
    ("jo", "е"),
    ("ju", "ю"),
    ("ja", "я"),
    ("kh", "х"),
    ("shch", "щ"),
    ("sh", "ш"),
    ("sch", "щ"),
    ("ts", "ц"),
    ("yo", "е"),
    ("yu", "ю"),
    ("ya", "я"),
    ("zh", "ж"),
];

/// C++ `get_ru_to_en_simple_rules`.
fn ru_to_en_simple(code: u32) -> Option<&'static str> {
    Some(match code {
        0x430 => "a",
        0x431 => "b",
        0x432 => "v",
        0x433 => "g",
        0x434 => "d",
        0x435 => "e",
        0x451 => "e",
        0x436 => "zh",
        0x437 => "z",
        0x438 => "i",
        0x439 => "y",
        0x43a => "k",
        0x43b => "l",
        0x43c => "m",
        0x43d => "n",
        0x43e => "o",
        0x43f => "p",
        0x440 => "r",
        0x441 => "s",
        0x442 => "t",
        0x443 => "u",
        0x444 => "f",
        0x445 => "kh",
        0x446 => "ts",
        0x447 => "ch",
        0x448 => "sh",
        0x449 => "sch",
        0x44a => "",
        0x44b => "y",
        0x44c => "",
        0x44d => "e",
        0x44e => "yu",
        0x44f => "ya",
        _ => return None,
    })
}

/// C++ `get_ru_to_en_complex_rules`.
const RU_TO_EN_COMPLEX: &[(&str, &str)] = &[
    ("ий", "y"),
    ("ия", "ia"),
    ("кс", "x"),
    ("yo", "e"),
    ("jo", "e"),
];

/// One direction's rule set: the per-codepoint simple table plus the whole-word complex rules.
type TranslitRules = (
    fn(u32) -> Option<&'static str>,
    &'static [(&'static str, &'static str)],
);

/// C++ `add_word_transliterations`: push the whole-word simple-rule
/// transliteration, then the greedy complex-rule walk (with partial-suffix
/// candidates when `allow_partial`).
fn add_word_transliterations(
    result: &mut Vec<String>,
    word: &str,
    allow_partial: bool,
    direction: Direction,
) {
    let (simple, complex_rules): TranslitRules = if direction == Direction::EnToRu {
        (en_to_ru_simple, EN_TO_RU_COMPLEX)
    } else {
        (ru_to_en_simple, RU_TO_EN_COMPLEX)
    };
    let bytes = word.as_bytes();

    let mut s: Vec<u8> = Vec::new();
    let mut pos = 0usize;
    while pos < bytes.len() {
        let (code, used) = next_utf8_unsafe(&bytes[pos..]);
        pos += used;
        match simple(code) {
            Some(replacement) => s.extend_from_slice(replacement.as_bytes()),
            None => append_utf8_character(&mut s, code),
        }
    }
    if !s.is_empty() {
        result.push(String::from_utf8(s).expect("transliteration of valid UTF-8 stays valid"));
        s = Vec::new();
    }

    pos = 0;
    while pos < bytes.len() {
        let suffix = &bytes[pos..];
        let mut found = false;
        for (first, second) in complex_rules {
            if suffix.starts_with(first.as_bytes()) {
                found = true;
                pos += first.len();
                s.extend_from_slice(second.as_bytes());
                break;
            }
            if allow_partial && first.as_bytes().starts_with(suffix) {
                let mut candidate = s.clone();
                candidate.extend_from_slice(second.as_bytes());
                result
                    .push(String::from_utf8(candidate).expect("transliteration stays valid UTF-8"));
            }
        }
        if found {
            continue;
        }

        let (code, used) = next_utf8_unsafe(&bytes[pos..]);
        pos += used;
        match simple(code) {
            Some(replacement) => s.extend_from_slice(replacement.as_bytes()),
            None => append_utf8_character(&mut s, code),
        }
    }
    if !s.is_empty() {
        result.push(String::from_utf8(s).expect("transliteration stays valid UTF-8"));
    }
}

/// C++ `get_word_transliterations`: both directions, deduplicated
/// (`td::unique` = sort + drop duplicates).
pub fn get_word_transliterations(word: &str, allow_partial: bool) -> Vec<String> {
    let mut result = Vec::new();
    add_word_transliterations(&mut result, word, allow_partial, Direction::EnToRu);
    add_word_transliterations(&mut result, word, allow_partial, Direction::RuToEn);
    result.sort();
    result.dedup();
    result
}
