//! Port of `td/telegram/misc.cpp`: `clean_input_string` — the
//! server-parity string sanitizer applied to user-provided strings before
//! they reach the handlers (UTF-8 check, control-character replacement,
//! the 35 000-byte length clamp, and the directional-mark replacement) —
//! plus the name/username/bio helpers the profile-editing surfaces share
//! (`clean_name`, `strip_empty_characters`, `is_allowed_username`).

use tdutils::utf8::{check_utf8, is_utf8_character_first_code_unit};

/// `LENGTH_LIMIT` — the server-side string limit.
const LENGTH_LIMIT: usize = 35000;

/// `replace_offending_characters` (misc.cpp:62-77):
/// `(U+200F|U+200E){N}(U+200F|U+200E)` -> `(U+200C){N}$2`.
pub(crate) fn replace_offending_characters(s: &mut [u8]) {
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

/// The `get_emoji_fingerprint` table (misc.cpp:324-373) — the C++
/// `vector<Slice>` becomes the `&str` array (extracted from the source,
/// 333 entries).
const EMOJI_FINGERPRINTS: [&str; 333] = [
    "😉", "😍", "😛", "😭", "😱", "😡", "😎", "😴", "😵", "😈", "😬", "😇", "😏", "👮", "👷", "💂",
    "👶", "👨", "👩", "👴", "👵", "😻", "😽", "🙀", "👺", "🙈", "🙉", "🙊", "💀", "👽", "💩", "🔥",
    "💥", "💤", "👂", "👀", "👃", "👅", "👄", "👍", "👎", "👌", "👊", "✌", "✋", "👐", "👆", "👇",
    "👉", "👈", "🙏", "👏", "💪", "🚶", "🏃", "💃", "👫", "👪", "👬", "👭", "💅", "🎩", "👑", "👒",
    "👟", "👞", "👠", "👕", "👗", "👖", "👙", "👜", "👓", "🎀", "💄", "💛", "💙", "💜", "💚", "💍",
    "💎", "🐶", "🐺", "🐱", "🐭", "🐹", "🐰", "🐸", "🐯", "🐨", "🐻", "🐷", "🐮", "🐗", "🐴", "🐑",
    "🐘", "🐼", "🐧", "🐥", "🐔", "🐍", "🐢", "🐛", "🐝", "🐜", "🐞", "🐌", "🐙", "🐚", "🐟", "🐬",
    "🐋", "🐐", "🐊", "🐫", "🍀", "🌹", "🌻", "🍁", "🌾", "🍄", "🌵", "🌴", "🌳", "🌞", "🌚", "🌙",
    "🌎", "🌋", "⚡", "☔", "❄", "⛄", "🌀", "🌈", "🌊", "🎓", "🎆", "🎃", "👻", "🎅", "🎄", "🎁",
    "🎈", "🔮", "🎥", "📷", "💿", "💻", "☎", "📡", "📺", "📻", "🔉", "🔔", "⏳", "⏰", "⌚", "🔒",
    "🔑", "🔎", "💡", "🔦", "🔌", "🔋", "🚿", "🚽", "🔧", "🔨", "🚪", "🚬", "💣", "🔫", "🔪", "💊",
    "💉", "💰", "💵", "💳", "✉", "📫", "📦", "📅", "📁", "✂", "📌", "📎", "✒", "✏", "📐", "📚",
    "🔬", "🔭", "🎨", "🎬", "🎤", "🎧", "🎵", "🎹", "🎻", "🎺", "🎸", "👾", "🎮", "🃏", "🎲", "🎯",
    "🏈", "🏀", "⚽", "⚾", "🎾", "🎱", "🏉", "🎳", "🏁", "🏇", "🏆", "🏊", "🏄", "☕", "🍼", "🍺",
    "🍷", "🍴", "🍕", "🍔", "🍟", "🍗", "🍱", "🍚", "🍜", "🍡", "🍳", "🍞", "🍩", "🍦", "🎂", "🍰",
    "🍪", "🍫", "🍭", "🍯", "🍎", "🍏", "🍊", "🍋", "🍒", "🍇", "🍉", "🍓", "🍑", "🍌", "🍐", "🍍",
    "🍆", "🍅", "🌽", "🏡", "🏥", "🏦", "⛪", "🏰", "⛺", "🏭", "🗻", "🗽", "🎠", "🎡", "⛲", "🎢",
    "🚢", "🚤", "⚓", "🚀", "✈", "🚁", "🚂", "🚋", "🚎", "🚌", "🚙", "🚗", "🚕", "🚛", "🚨", "🚔",
    "🚒", "🚑", "🚲", "🚠", "🚜", "🚦", "⚠", "🚧", "⛽", "🎰", "🗿", "🎪", "🎭", "🇯🇵", "🇰🇷", "🇩🇪",
    "🇨🇳", "🇺🇸", "🇫🇷", "🇪🇸", "🇮🇹", "🇷🇺", "🇬🇧", "1⃣", "2⃣", "3⃣", "4⃣", "5⃣", "6⃣", "7⃣", "8⃣", "9⃣", "0⃣",
    "🔟", "❗", "❓", "♥", "♦", "💯", "🔗", "🔱", "🔴", "🔵", "🔶", "🔷",
];

/// `get_emoji_fingerprint` (misc.cpp:323-378) — the emoji selected by the
/// low 63 bits of the number.
fn get_emoji_fingerprint(num: u64) -> String {
    EMOJI_FINGERPRINTS[((num & 0x7FFF_FFFF_FFFF_FFFF) % EMOJI_FINGERPRINTS.len() as u64) as usize]
        .to_string()
}

/// `get_emoji_fingerprints` (misc.cpp:380-387) — the four-emojis call
/// fingerprint over the big-endian 64-bit words of a 32-byte buffer.
pub fn get_emoji_fingerprints(buffer: &[u8; 32]) -> Vec<String> {
    let mut result = Vec::with_capacity(4);
    for i in 0..4 {
        let num = u64::from_be_bytes(buffer[8 * i..8 * i + 8].try_into().unwrap());
        result.push(get_emoji_fingerprint(num));
    }
    result
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

/// `td::is_space` (tdutils/misc.h:107).
fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\r' | b'\n' | 0 | 0x0b)
}

/// `td::validate_bot_language_code` (misc.cpp:395-406): empty or a
/// two-letter a-z code.
pub(crate) fn validate_bot_language_code(
    language_code: &str,
) -> std::result::Result<(), tdutils::status::Status> {
    if language_code.is_empty() {
        return Ok(());
    }
    let bytes = language_code.as_bytes();
    if bytes.len() == 2 && bytes.iter().all(|c| c.is_ascii_lowercase()) {
        return Ok(());
    }
    Err(tdutils::status::Status::error(
        400,
        "Invalid language code specified",
    ))
}

/// `td::trim` (tdutils/misc.h:133) over the shared whitespace set; the
/// ASCII gate keeps the byte-based C++ semantics (`is_space` is defined on
/// bytes only, so no multi-byte character can be trimmed).
pub(crate) fn trim(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_ascii() && is_space(c as u8))
}

/// `td::to_lower` (tdutils/misc.h:64-85) — ASCII-only.
pub(crate) fn to_lower_ascii(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_uppercase() {
                (c as u8 + 32) as char
            } else {
                c
            }
        })
        .collect()
}

/// `check_currency_amount` (misc.cpp:390-393) — the C++ `MAX_AMOUNT`
/// gate of 9999'9999'9999.
pub(crate) fn check_currency_amount(amount: i64) -> bool {
    const MAX_AMOUNT: i64 = 999_999_999_999;
    (-MAX_AMOUNT..=MAX_AMOUNT).contains(&amount)
}

#[allow(dead_code)] // the C++ to_lower twin; the ascii fast-path is the live one
fn to_lower(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_uppercase() {
                (c as u8 - b'A' + b'a') as char
            } else {
                c
            }
        })
        .collect()
}

/// `td::utf8_truncate` (tdutils/utf8.h:70): cut to `length` Unicode
/// characters at a codepoint boundary.
fn utf8_truncate(s: &str, length: usize) -> &str {
    if s.len() > length {
        let mut remaining = length;
        for (i, b) in s.bytes().enumerate() {
            if is_utf8_character_first_code_unit(b) {
                if remaining == 0 {
                    return &s[..i];
                }
                remaining -= 1;
            }
        }
    }
    s
}

/// `td::replace_with_spaces` (tdutils/misc.cpp:22-28) — a byte search,
/// so only ASCII characters make sense here.
pub fn replace_with_spaces(s: &mut String, characters: &str) {
    // only b' ' is ever written, so the UTF-8 invariant is kept
    for c in unsafe { s.as_mut_vec() } {
        if characters.as_bytes().contains(c) {
            *c = b' ';
        }
    }
}

/// `td::clean_name` (misc.cpp:24-46): strip empty characters, collapse
/// space/nbsp/`\n` runs to single spaces and trim.
pub fn clean_name(s: &str, max_length: usize) -> String {
    let str = strip_empty_characters(s, max_length, false);
    let bytes = str.as_bytes();
    let mut new_len = 0usize;
    let mut is_previous_space = false;
    let mut out = vec![0u8; bytes.len()];
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b' ' || bytes[i] == b'\n' {
            if !is_previous_space {
                out[new_len] = b' ';
                new_len += 1;
                is_previous_space = true;
            }
            i += 1;
            continue;
        }
        if bytes[i] == 0xC2 && i + 1 < bytes.len() && bytes[i + 1] == 0xA0 {
            // &nbsp;
            if !is_previous_space {
                out[new_len] = b' ';
                new_len += 1;
                is_previous_space = true;
            }
            i += 2;
            continue;
        }
        out[new_len] = bytes[i];
        new_len += 1;
        is_previous_space = false;
        i += 1;
    }
    String::from_utf8_lossy(&out[..new_len])
        .trim_matches(|c: char| c.is_ascii() && is_space(c as u8))
        .to_owned()
}

/// `td::strip_empty_characters` (misc.cpp:163-253): replace the
/// known space-like Unicode sequences with plain spaces, drop
/// leading/trailing "empty" characters and clamp to `max_length`
/// Unicode characters.
pub fn strip_empty_characters(s: &str, max_length: usize, strip_rtlo: bool) -> String {
    // 3-byte UTF-8 space-like sequences, plus the U+F3xx
    // supplementary-plane form handled by the 0xF3 arm.
    const SPACE_CHARACTERS: [&[u8]; 19] = [
        b"\xe1\x9a\x80", // U+1680
        b"\xe1\xa0\x8e", // U+180E
        b"\xe2\x80\x80", // U+2000
        b"\xe2\x80\x81", // U+2001
        b"\xe2\x80\x82", // U+2002
        b"\xe2\x80\x83", // U+2003
        b"\xe2\x80\x84", // U+2004
        b"\xe2\x80\x85", // U+2005
        b"\xe2\x80\x86", // U+2006
        b"\xe2\x80\x87", // U+2007
        b"\xe2\x80\x88", // U+2008
        b"\xe2\x80\x89", // U+2009
        b"\xe2\x80\x8a", // U+200A
        b"\xe2\x80\xae", // U+202E
        b"\xe2\x80\xaf", // U+202F
        b"\xe2\x81\x9f", // U+205F
        b"\xe2\xa0\x80", // U+2800
        b"\xe3\x80\x80", // U+3000
        b"\xef\xbf\xbc", // U+FFFC
    ];
    let can_be_first = |b: u8| SPACE_CHARACTERS.iter().any(|sc| sc[0] == b) || b == 0xF3;

    let bytes = s.as_bytes();
    // replace all occurrences of space characters with a space
    let mut i = 0usize;
    while i < bytes.len() && !can_be_first(bytes[i]) {
        i += 1;
    }
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    out.extend_from_slice(&bytes[..i]);
    while i < bytes.len() {
        if can_be_first(bytes[i]) && i + 3 <= bytes.len() {
            if bytes[i] == 0xF3 {
                if bytes[i + 1] == 0xA0 && (bytes[i + 2] & 0xFE) == 0x80 && i + 4 <= bytes.len() {
                    out.push(b' ');
                    i += 4;
                    continue;
                }
            } else {
                let mut found = false;
                for space_ch in SPACE_CHARACTERS {
                    if space_ch[0] == bytes[i]
                        && space_ch[1] == bytes[i + 1]
                        && space_ch[2] == bytes[i + 2]
                    {
                        if bytes[i + 2] != 0xAE
                            || bytes[i + 1] != 0x80
                            || bytes[i] != 0xE2
                            || strip_rtlo
                        {
                            found = true;
                        }
                        break;
                    }
                }
                if found {
                    out.push(b' ');
                    i += 3;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    let replaced = String::from_utf8_lossy(&out).into_owned();
    let trimmed = utf8_truncate(trim(&replaced), max_length);
    // check if there is some non-empty character, empty characters:
    // ZERO WIDTH SPACE/NON-JOINER/JOINER, the LRM/RLM/RLTO marks, the
    // BYTE ORDER MARK, the NO-BREAK SPACE
    let bytes = trimmed.as_bytes();
    let mut i = 0usize;
    loop {
        if i == bytes.len() {
            // if all characters are empty, return an empty string
            return String::new();
        }
        if bytes[i] == b' ' || bytes[i] == b'\n' {
            i += 1;
            continue;
        }
        if bytes[i] == 0xE2 && i + 2 < bytes.len() && bytes[i + 1] == 0x80 {
            let next = bytes[i + 2];
            if (0x8B..=0x8F).contains(&next) || next == 0xAE {
                i += 3;
                continue;
            }
        }
        if bytes[i] == 0xEF && i + 2 < bytes.len() && bytes[i + 1] == 0xBB && bytes[i + 2] == 0xBF {
            i += 3;
            continue;
        }
        if bytes[i] == 0xC2 && i + 1 < bytes.len() && bytes[i + 1] == 0xA0 {
            i += 2;
            continue;
        }
        break;
    }
    trimmed.to_owned()
}

/// `td::is_empty_string` (misc.cpp:256-258).
pub fn is_empty_string(s: &str) -> bool {
    strip_empty_characters(s, s.len(), false).is_empty()
}

/// `td::is_valid_username` (misc.cpp:260-282).
pub fn is_valid_username(username: &str) -> bool {
    let bytes = username.as_bytes();
    if bytes.is_empty() || bytes.len() > 32 {
        return false;
    }
    if !is_alpha(bytes[0]) {
        return false;
    }
    for &c in bytes {
        if !is_alpha(c) && !c.is_ascii_digit() && c != b'_' {
            return false;
        }
    }
    if bytes[bytes.len() - 1] == b'_' {
        return false;
    }
    for i in 1..bytes.len() {
        if bytes[i - 1] == b'_' && bytes[i] == b'_' {
            return false;
        }
    }
    true
}

/// `td::is_alpha` (tdutils/misc.h:113): `c | 0x20` in `'a'..='z'`.
fn is_alpha(c: u8) -> bool {
    let c = c | 0x20;
    c.is_ascii_lowercase()
}

/// `td::is_allowed_username` (misc.cpp:284-303): a valid username of at
/// least 5 characters without the reserved prefixes.
pub fn is_allowed_username(username: &str) -> bool {
    if !is_valid_username(username) {
        return false;
    }
    if username.len() < 5 {
        return false;
    }
    let username_lowered = to_lower_ascii(username);
    for prefix in [
        "admin",
        "telegram",
        "support",
        "security",
        "settings",
        "contacts",
        "service",
        "telegraph",
    ] {
        if username_lowered.starts_with(prefix) {
            return false;
        }
    }
    true
}

/// `td::clean_username` (misc.cpp:52-56): remove all dots, ASCII-lowercase,
/// trim the C whitespace set from both ends.
pub fn clean_username(str: &str) -> String {
    let without_dots: String = str.chars().filter(|&c| c != '.').collect();
    let trimmed = without_dots.trim_matches(|c| matches!(c, ' ' | '\t' | '\n' | '\r'));
    trimmed.to_ascii_lowercase()
}

/// `td::get_valid_short_usernames` (MessageEntity.cpp:1347-1350) — the
/// mention-usable usernames shorter than the 4-character minimum. The data
/// table is standalone; the rest of MessageEntity arrives with M.8.
pub fn get_valid_short_usernames() -> &'static [&'static str] {
    &["gif", "nft", "pic", "ufc", "vid"]
}

#[cfg(test)]
mod misc_helper_tests {
    use super::{
        check_currency_amount, clean_name, clean_username, get_valid_short_usernames,
        is_allowed_username, is_valid_username, replace_with_spaces, strip_empty_characters,
    };

    #[test]
    fn currency_amount_bounds_match_the_cpp_limit() {
        // misc.cpp:390-393 — inclusive ±9999'9999'9999.
        assert!(check_currency_amount(0));
        assert!(check_currency_amount(9999_9999_9999));
        assert!(check_currency_amount(-9999_9999_9999));
        assert!(!check_currency_amount(9999_9999_9999 + 1));
        assert!(!check_currency_amount(-9999_9999_9999 - 1));
    }

    #[test]
    fn clean_username_strips_dots_lowercases_and_trims() {
        // misc.cpp:52-56.
        assert_eq!(clean_username("Ab.C..D"), "abcd");
        assert_eq!(clean_username("  User\tname \n"), "user\tname");
        assert_eq!(clean_username(""), "");
    }

    #[test]
    fn valid_short_usernames_match_the_cpp_table() {
        assert_eq!(
            get_valid_short_usernames(),
            &["gif", "nft", "pic", "ufc", "vid"]
        );
    }

    #[test]
    fn clean_name_collapses_and_trims() {
        // misc.cpp:24-46 — space/`\n`/nbsp runs collapse to one space.
        assert_eq!(clean_name("  Alice   Liddell\n\n", 64), "Alice Liddell");
        assert_eq!(clean_name("a\u{a0}b", 64), "a b", "nbsp becomes a space");
        assert_eq!(
            clean_name("\u{a0}\u{1680}x", 64),
            "x",
            "leading empty characters go away"
        );
        assert_eq!(clean_name("", 64), "");
    }

    #[test]
    fn strip_empty_replaces_and_clamps() {
        // The fixed space-like table becomes plain spaces; the final
        // trim drops the leading one, exactly like the C++ order.
        assert_eq!(
            strip_empty_characters("\u{2002}a\u{3000}b", 64, false),
            "a b"
        );
        // U+202E (RTL override) is kept unless strip_rtlo; the
        // converted leading space is trimmed either way.
        assert_eq!(strip_empty_characters("\u{202e}a", 64, false), "\u{202e}a");
        assert_eq!(strip_empty_characters("\u{202e}a", 64, true), "a");
        // The clamp counts Unicode characters, not bytes.
        assert_eq!(strip_empty_characters("абвгд", 3, false), "абв");
        // All-empty input collapses to nothing.
        assert_eq!(strip_empty_characters("\u{a0}\u{200b}", 64, false), "");
    }

    #[test]
    fn username_validity_rules() {
        // is_valid_username: alpha start, [A-Za-z0-9_], no trailing or
        // doubled underscores, 1..=32 bytes.
        assert!(is_valid_username("durov"));
        assert!(is_valid_username("D_1x"));
        assert!(!is_valid_username("1durov"), "must start with a letter");
        assert!(!is_valid_username("a__b"), "doubled underscore");
        assert!(!is_valid_username("ab_"), "trailing underscore");
        assert!(!is_valid_username("ab-cd"), "dash is not allowed");
        assert!(!is_valid_username(&"a".repeat(33)));
        // is_allowed_username adds the 5-character floor and the
        // reserved prefixes.
        assert!(!is_allowed_username("abcd"), "too short");
        assert!(!is_allowed_username("telegram_fan"), "reserved prefix");
        assert!(
            !is_allowed_username("TeLeGram"),
            "reserved prefix, case-folded"
        );
        assert!(
            !is_allowed_username("telegram2"),
            "the prefix check is starts_with"
        );
        assert!(is_allowed_username("durov"));
    }

    #[test]
    fn replace_with_spaces_hits_every_match() {
        let mut s = "a\nb\nc".to_owned();
        replace_with_spaces(&mut s, "\n");
        assert_eq!(s, "a b c");
    }
}

/// `search_strings_by_prefix` (misc.cpp:406-417): the `Hints`-backed prefix
/// search behind the synchronous `searchStringsByPrefix` request. Keys are
/// the string positions (empty strings enter as `" "`, a searchable
/// stand-in), ratings equal the positions so ties answer in input order.
pub fn search_strings_by_prefix(
    strings: Vec<String>,
    query: &str,
    limit: i32,
    return_all_for_empty_query: bool,
) -> (i32, Vec<i32>) {
    let mut hints = tdutils::hints::Hints::default();
    for (index, string) in strings.iter().enumerate() {
        hints.add(index as i64, if string.is_empty() { " " } else { string });
        hints.set_rating(index as i64, index as i64);
    }
    let (total_count, keys) = hints.search(query, limit, return_all_for_empty_query);
    (
        total_count as i32,
        keys.into_iter().map(|key| key as i32).collect(),
    )
}

/// `td::to_integer<int64>` (tdutils/td/utils/misc.h:157-183) — the signed
/// decimal parse: an optional leading `-`, digits until the first
/// non-digit, wrapping overflow exactly like the C++ (the two's-complement
/// negation dance), 0 on an empty/minus-only input.
pub fn to_integer_i64(str: &str) -> i64 {
    let mut integer_value: u64 = 0;
    let mut is_negative = false;
    let bytes = str.as_bytes();
    let mut begin = 0usize;
    if !bytes.is_empty() && bytes[begin] == b'-' {
        is_negative = true;
        begin += 1;
    }
    while begin < bytes.len() && bytes[begin].is_ascii_digit() {
        integer_value = integer_value
            .wrapping_mul(10)
            .wrapping_add(u64::from(bytes[begin] - b'0'));
        begin += 1;
    }
    if integer_value > i64::MAX as u64 {
        // Use ~x + 1 instead of -x to suppress the C++ MSVC warning the
        // comment mentions; the wrap is the C++ behavior.
        integer_value = (!integer_value).wrapping_add(1);
        is_negative = !is_negative;
        if integer_value > i64::MAX as u64 {
            return i64::MIN;
        }
    }
    if is_negative {
        (integer_value as i64).wrapping_neg()
    } else {
        integer_value as i64
    }
}

/// `td::to_double` (misc.h:220-224, `atof` semantics) — parse the leading
/// `double` (optional whitespace/sign, digits with an optional fraction
/// and exponent), stopping at the first invalid character; 0.0 when
/// nothing parses.
pub fn to_double(str: &str) -> f64 {
    let trimmed = str.trim_start();
    let bytes = trimmed.as_bytes();
    let mut end = 0usize;
    if end < bytes.len() && (bytes[end] == b'+' || bytes[end] == b'-') {
        end += 1;
    }
    let mut seen_digits = false;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
        seen_digits = true;
    }
    if end < bytes.len() && bytes[end] == b'.' {
        end += 1;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
            seen_digits = true;
        }
    }
    if !seen_digits {
        return 0.0;
    }
    if end < bytes.len() && (bytes[end] == b'e' || bytes[end] == b'E') {
        let mut exp_end = end + 1;
        if exp_end < bytes.len() && (bytes[exp_end] == b'+' || bytes[exp_end] == b'-') {
            exp_end += 1;
        }
        let exp_digits_start = exp_end;
        while exp_end < bytes.len() && bytes[exp_end].is_ascii_digit() {
            exp_end += 1;
        }
        if exp_end > exp_digits_start {
            end = exp_end;
        }
    }
    trimmed[..end].parse::<f64>().unwrap_or(0.0)
}

/// `td::is_base64url_characters` (base64.cpp:205-212, the `is_url`
/// instantiation) — every byte is in the URL-safe base64 alphabet; padding
/// is not allowed (unlike `is_base64url`).
pub fn is_base64url_characters(input: &[u8]) -> bool {
    input
        .iter()
        .all(|c| c.is_ascii_alphanumeric() || *c == b'-' || *c == b'_')
}

/// `td::add_to_top` (tdutils/td/utils/algorithm.h:115-134) — move `value`
/// to the front of `v`, evicting the last element past `max_size`.
pub fn add_to_top<T: PartialEq + Clone>(v: &mut Vec<T>, max_size: usize, value: T) {
    let size = v.len();
    let mut i = 0usize;
    while i < size && v[i] != value {
        i += 1;
    }
    if i == size {
        if size < max_size || i == 0 {
            // `i` stays at the pushed element's index
            v.push(value.clone());
        } else {
            // the last element is evicted by the shift below
            i -= 1;
        }
    }
    while i > 0 {
        v[i] = v[i - 1].clone();
        i -= 1;
    }
    v[0] = value;
}

#[cfg(test)]
mod search_strings_by_prefix_tests {
    use super::search_strings_by_prefix;

    #[test]
    fn prefix_matches_answer_in_input_order() {
        let strings = vec![
            "Delta".to_owned(),
            "charlie".to_owned(),
            "Charlie".to_owned(),
            "bravo".to_owned(),
        ];
        let (total, positions) = search_strings_by_prefix(strings, "char", 10, false);
        assert_eq!(total, 2);
        assert_eq!(positions, vec![1, 2]);
    }

    #[test]
    fn limit_cuts_but_total_count_survives() {
        let strings: Vec<String> = ["ab1", "ab2", "ab3", "ab4"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let (total, positions) = search_strings_by_prefix(strings, "ab", 2, false);
        assert_eq!(total, 4);
        assert_eq!(positions, vec![0, 1]);
    }

    #[test]
    fn empty_query_is_returned_only_on_request() {
        let strings = vec!["one".to_owned(), String::new(), " ".to_owned()];
        let (total, positions) = search_strings_by_prefix(strings.clone(), "", 10, true);
        assert_eq!(total, 3);
        assert_eq!(positions, vec![0, 1, 2]);
        let (total, positions) = search_strings_by_prefix(strings, "", 10, false);
        assert_eq!(total, 0);
        assert!(positions.is_empty());
    }

    #[test]
    fn no_match_answers_zero() {
        let strings = vec!["alpha".to_owned(), "beta".to_owned()];
        let (total, positions) = search_strings_by_prefix(strings, "gamma", 10, false);
        assert_eq!(total, 0);
        assert!(positions.is_empty());
    }
}
