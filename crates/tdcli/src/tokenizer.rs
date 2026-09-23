//! The `cli.cpp` tokenizer family: `td::split`/`td::full_split`
//! (`tdutils/td/utils/misc.h`), the lenient `td::to_integer`, and the `on_cmd`
//! input preprocessing (ANSI escape stripping + control-char removal).
//!
//! Note: the plan's M.14.1 mentions `get_word` (quote-aware word splitting),
//! but the CLI in the tree we build against has no such helper — `on_cmd`
//! splits on the first space via `td::split` and coerces the tail per-command.
//! The port targets the tree, so `split` semantics are the tokenizer.

/// `td::split(str)` — split at the first `delimiter`; `(before, after)` with
/// an empty second element when the delimiter is absent (and vice versa for
/// a leading delimiter). No trimming anywhere.
pub fn split(s: &str, delimiter: char) -> (String, String) {
    match s.find(delimiter) {
        Some(pos) => (
            s[..pos].to_string(),
            s[pos + delimiter.len_utf8()..].to_string(),
        ),
        None => (s.to_string(), String::new()),
    }
}

/// `td::full_split` — split on every `delimiter` (the C++ default max_parts
/// of `numeric_limits<size_t>::max()`; the CLI never passes one). An empty
/// input yields an empty vector like the C++.
#[allow(dead_code)] // the first caller lands in M.14.2 (arg families)
pub fn full_split(s: &str, delimiter: char) -> Vec<String> {
    if s.is_empty() {
        return Vec::new();
    }
    s.split(delimiter).map(str::to_string).collect()
}

/// `td::to_integer<int32>` — the lenient signed scan: optional `-`, digits
/// until the first non-digit, unsigned wraparound with the two's-complement
/// fold (`~x + 1`), `i32::MIN` as the final fallback. No failure mode.
#[allow(dead_code)] // the first caller lands in M.14.2 (get_args coercers)
pub fn to_integer_i32(s: &str) -> i32 {
    to_integer_signed_impl(s)
}

/// `td::to_integer<int64>` — see [`to_integer_i32`].
#[allow(dead_code)] // the first caller lands in M.14.2 (get_args coercers)
pub fn to_integer_i64(s: &str) -> i64 {
    to_integer_signed_impl(s)
}

#[allow(dead_code)] // exercised through the coercers starting in M.14.2
fn to_integer_signed_impl<T>(s: &str) -> T
where
    T: ToInteger,
{
    T::scan(s)
}

#[allow(dead_code)] // exercised through the coercers starting in M.14.2
trait ToInteger: Sized {
    fn scan(s: &str) -> Self;
}

macro_rules! impl_to_integer {
    ($t:ty, $u:ty) => {
        impl ToInteger for $t {
            fn scan(s: &str) -> Self {
                let bytes = s.as_bytes();
                let mut i = 0;
                let mut is_negative = false;
                if i < bytes.len() && bytes[i] == b'-' {
                    is_negative = true;
                    i += 1;
                }
                let mut integer_value: $u = 0;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    integer_value = integer_value
                        .wrapping_mul(10)
                        .wrapping_add((bytes[i] - b'0') as $u);
                    i += 1;
                }
                if integer_value > <$t>::MAX as $u {
                    integer_value = (!integer_value).wrapping_add(1);
                    is_negative = !is_negative;
                    if integer_value > <$t>::MAX as $u {
                        return <$t>::MIN;
                    }
                }
                if is_negative {
                    (integer_value as $t).wrapping_neg()
                } else {
                    integer_value as $t
                }
            }
        }
    };
}

impl_to_integer!(i32, u32);
impl_to_integer!(i64, u64);

/// The `on_cmd` ANSI preprocessing: `cli.cpp` scans for `ESC [` sequences and
/// removes xterm (`...A`-`...Z`) and vt (`...~`) sequences. Ported verbatim,
/// including the C++ string edge cases (reads past the end see `'\0'`, so a
/// truncated sequence is left untouched).
pub fn strip_ansi(mut cmd: String) -> String {
    let mut i = 0;
    loop {
        let bytes = cmd.as_bytes();
        if i >= bytes.len() {
            return cmd;
        }
        if bytes[i] == 27 && bytes.get(i + 1) == Some(&b'[') {
            let mut j = i + 2;
            if matches!(bytes.get(j), Some(c) if matches!(c, b'1'..=b'9')) {
                while bytes.get(j).is_some_and(u8::is_ascii_digit) {
                    j += 1;
                }
            }
            if bytes.get(j).is_some_and(|c| c.is_ascii_uppercase()) {
                // xterm sequence
                cmd = format!("{}{}", &cmd[..i], &cmd[(j + 1).min(cmd.len())..]);
                continue;
            }
            if bytes.get(j) == Some(&b';')
                && matches!(bytes.get(j + 1), Some(c) if matches!(c, b'1'..=b'9'))
            {
                j += 2;
                while bytes.get(j).is_some_and(u8::is_ascii_digit) {
                    j += 1;
                }
            }
            if bytes.get(j) == Some(&b'~') {
                // vt sequence
                cmd = format!("{}{}", &cmd[..i], &cmd[(j + 1).min(cmd.len())..]);
                continue;
            }
        }
        i += 1;
    }
}

/// The `on_cmd` control-char removal: `td::remove_if(cmd, c < 32)` — every
/// byte below 32 is dropped (tab included; DEL stays).
pub fn remove_control_chars(mut cmd: String) -> String {
    cmd.retain(|c| (c as u32) >= 32);
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_first_space() {
        assert_eq!(split("sm 1 2", ' '), ("sm".to_string(), "1 2".to_string()));
        assert_eq!(split("q", ' '), ("q".to_string(), String::new()));
        assert_eq!(
            split("  leading", ' '),
            (String::new(), " leading".to_string())
        );
        assert_eq!(split("a;b", ';'), ("a".to_string(), "b".to_string()));
    }

    #[test]
    fn full_split_semantics() {
        assert!(full_split("", ' ').is_empty());
        assert_eq!(full_split("a b", ' '), vec!["a", "b"]);
        assert_eq!(full_split("a", ' '), vec!["a"]);
    }

    #[test]
    fn to_integer_lenient_scan() {
        assert_eq!(to_integer_i64("123"), 123);
        assert_eq!(to_integer_i64("-123"), -123);
        assert_eq!(to_integer_i64(""), 0);
        assert_eq!(to_integer_i64("+5"), 0);
        assert_eq!(to_integer_i64("42abc"), 42);
        assert_eq!(to_integer_i64("abc"), 0);
        assert_eq!(to_integer_i32("5"), 5);
        assert_eq!(to_integer_i32("-1"), -1);
    }

    #[test]
    fn to_integer_wraparound() {
        assert_eq!(to_integer_i64("9223372036854775808"), i64::MIN);
        assert_eq!(to_integer_i64("9223372036854775809"), i64::MIN + 1);
        assert_eq!(to_integer_i32("4294967295"), -1);
    }

    #[test]
    fn ansi_strip_xterm_and_vt() {
        assert_eq!(strip_ansi("\u{1b}[1Aansi".to_string()), "ansi");
        assert_eq!(strip_ansi("\u{1b}[1;5~vt".to_string()), "vt");
        assert_eq!(strip_ansi("plain".to_string()), "plain");
        assert_eq!(strip_ansi("\u{1b}[".to_string()), "\u{1b}[");
        assert_eq!(strip_ansi("\u{1b}[A".to_string()), "");
    }

    #[test]
    fn control_chars_removed() {
        assert_eq!(remove_control_chars("a\tb".to_string()), "ab");
        assert_eq!(remove_control_chars("a\x01b".to_string()), "ab");
        assert_eq!(remove_control_chars("\u{7f}".to_string()), "\u{7f}");
    }
}
