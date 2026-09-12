//! Faithful port of the TL scheme lexer from td's tl-parser
//! (`td/generate/tl-parser/tl-parser.c`: `nextch`, `parse_lex`, `parse_error`).
//!
//! The port preserves the C behavior exactly, including its quirks, because
//! plan step S06 verifies it by diffing token dumps against the C original for
//! all five schemes (`conformance/tl_oracle/verify.sh`):
//!
//! - `nextch` freezes at the last byte and reports EOF as `\0` from then on;
//!   a `\0` inside the file also acts as EOF;
//! - whitespace is any byte `<= 32` (C compares a signed `char`, which also
//!   makes bytes `>= 0x80` whitespace; schemes are pure ASCII, so the u8
//!   comparison here matches on all real inputs);
//! - `//` comments run to end of line; a lone `/` is silently swallowed (the
//!   C code consumes it while probing for the second `/`);
//! - an unterminated `//` comment makes the C code loop forever at EOF; we
//!   stop at EOF instead — the only deliberate divergence, unreachable for
//!   well-formed input;
//! - on `lex_eof`/`lex_error` the previous token's `flags` (eof) and `len`
//!   and `start` (both) are left stale, exactly like the C `parse.lex`.

/// Token kinds, mirroring `enum lex_type` in tl-parser.h.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LexType {
    Error,
    Char,
    TripleMinus,
    UcIdent,
    LcIdent,
    Eof,
    Final,
    New,
    None,
    Num,
    Empty,
}

impl LexType {
    /// The C enum name; the shared token-dump format uses these strings.
    pub fn name(self) -> &'static str {
        match self {
            LexType::Error => "lex_error",
            LexType::Char => "lex_char",
            LexType::TripleMinus => "lex_triple_minus",
            LexType::UcIdent => "lex_uc_ident",
            LexType::LcIdent => "lex_lc_ident",
            LexType::Eof => "lex_eof",
            LexType::Final => "lex_final",
            LexType::New => "lex_new",
            LexType::None => "lex_none",
            LexType::Num => "lex_num",
            LexType::Empty => "lex_empty",
        }
    }
}

/// The current token, mirroring `struct curlex`. `start` is the token's byte
/// offset in the source (the C `ptr`); on `Error`/`Eof` it keeps its stale
/// value just like the C code keeps a stale or sentinel `ptr`.
#[derive(Debug, Clone, Copy)]
pub struct CurLex {
    pub start: usize,
    pub len: usize,
    pub ty: LexType,
    pub flags: i32,
}

/// Set on a namespaced identifier (`a.b.c`, the C `flags |= 1`).
pub const FLAG_NAMESPACE_DOT: i32 = 1;
/// Set on an identifier with a magic-number suffix (`name#hhhhhhhh`, the C
/// `flags |= 2`).
pub const FLAG_MAGIC_HASH: i32 = 2;

/// Lexer state, bundling the C globals `struct parse parse; char curch;`.
pub struct Lexer {
    text: Vec<u8>,
    pos: usize,
    len: usize,
    line: i32,
    line_pos: i32,
    pub lex: CurLex,
    curch: u8,
    last_error_pos: i32,
    last_error_line: i32,
    last_error_line_pos: i32,
    last_error: Option<String>,
}

impl Lexer {
    /// Mirrors `tl_init_parse_file` plus the `load_parse` bootstrap that
    /// `tl_parse_lex` performs (sets `curch` to the first byte).
    pub fn new(text: Vec<u8>) -> Lexer {
        let len = text.len();
        Lexer {
            lex: CurLex {
                start: 0,
                len: 0,
                ty: LexType::None,
                flags: 0,
            },
            curch: text.first().copied().unwrap_or(0),
            text,
            pos: 0,
            len,
            line: 0,
            line_pos: 0,
            last_error_pos: 0,
            last_error_line: 0,
            last_error_line_pos: 0,
            last_error: None,
        }
    }

    /// Current byte offset (the C `parse.pos`).
    pub fn pos(&self) -> usize {
        self.pos
    }

    /// Current line, 0-based (the C `parse.line`).
    pub fn line(&self) -> i32 {
        self.line
    }

    /// Current position in line, 0-based (the C `parse.line_pos`).
    pub fn line_pos(&self) -> i32 {
        self.line_pos
    }

    /// The full source text.
    pub fn text(&self) -> &[u8] {
        &self.text
    }

    /// The current token's bytes; empty for `Error`/`Eof`/`None`.
    pub fn token_text(&self) -> &[u8] {
        if self.lex.len == 0 || matches!(self.lex.ty, LexType::Error | LexType::Eof | LexType::None)
        {
            &[]
        } else {
            &self.text[self.lex.start..self.lex.start + self.lex.len]
        }
    }

    /// Mirrors `nextch`: advances only while `pos < len - 1` (signed math in
    /// C, so an empty input never advances); at the last byte the position
    /// freezes and `curch` becomes 0.
    fn nextch(&mut self) -> u8 {
        if (self.pos as isize) < self.len as isize - 1 {
            self.pos += 1;
            self.curch = self.text[self.pos];
        } else {
            self.curch = 0;
        }
        if self.curch == b'\n' {
            self.line += 1;
            self.line_pos = 0;
        } else if self.curch != 0 {
            self.line_pos += 1;
        }
        self.curch
    }

    /// Mirrors `parse_error`: only the furthest error is kept.
    fn parse_error(&mut self, error: &str) {
        if (self.pos as i32) > self.last_error_pos {
            self.last_error_pos = self.pos as i32;
            self.last_error_line = self.line;
            self.last_error_line_pos = self.line_pos;
            self.last_error = Some(error.to_owned());
        }
    }

    /// Mirrors `tl_print_parse_error` (line/pos reported 1-based).
    pub fn print_parse_error(&self) -> String {
        format!(
            "Error near line {} pos {}: `{}`",
            self.last_error_line + 1,
            self.last_error_line_pos + 1,
            self.last_error.as_deref().unwrap_or("")
        )
    }

    /// Scans the next token into `self.lex`, mirroring `parse_lex`.
    pub fn parse_lex(&mut self) -> &CurLex {
        loop {
            while self.curch != 0 && is_whitespace(self.curch) {
                self.nextch();
            }
            if self.curch == b'/' && self.nextch() == b'/' {
                // C: `while (nextch() != 10);` — spins forever at EOF on an
                // unterminated comment; we additionally stop at EOF.
                loop {
                    let c = self.nextch();
                    if c == b'\n' || c == 0 {
                        break;
                    }
                }
                self.nextch();
            } else {
                break;
            }
        }
        if self.curch == 0 {
            // Deliberately not resetting flags here (C quirk: eof inherits
            // the previous token's flags).
            self.lex.len = 0;
            self.lex.ty = LexType::Eof;
            return &self.lex;
        }
        let p = self.pos;
        self.lex.flags = 0;
        match self.curch {
            b'-' => {
                // C short-circuits: the second nextch runs only if the first
                // returned '-'.
                if self.nextch() != b'-' || self.nextch() != b'-' {
                    self.parse_error("Can not parse triple minus");
                    self.lex.ty = LexType::Error;
                } else {
                    self.lex.start = p;
                    self.lex.len = 3;
                    self.lex.ty = LexType::TripleMinus;
                    self.nextch();
                }
            }
            b':' | b';' | b'(' | b')' | b'[' | b']' | b'{' | b'}' | b'=' | b'#' | b'?' | b'%'
            | b'<' | b'>' | b'+' | b',' | b'*' | b'_' | b'!' | b'.' => {
                self.nextch();
                self.lex.start = p;
                self.lex.len = 1;
                self.lex.ty = LexType::Char;
            }
            b'A'..=b'Z' => {
                while is_ident_char(self.nextch()) {}
                self.lex.start = p;
                self.lex.len = self.pos - p;
                let token = &self.text[p..p + self.lex.len];
                self.lex.ty = if self.lex.len == 5 && token == b"Final" {
                    LexType::Final
                } else if self.lex.len == 3 && token == b"New" {
                    LexType::New
                } else if self.lex.len == 5 && token == b"Empty" {
                    LexType::Empty
                } else {
                    LexType::UcIdent
                };
            }
            b'a'..=b'z' => {
                while is_ident_char(self.nextch()) {}
                // `ident.` with a non-letter after the dot: the dot is not
                // part of the identifier (C peeks text[pos + 1]; an
                // out-of-range peek reads as 0, which is not a letter).
                if self.curch == b'.'
                    && !self.text.get(self.pos + 1).copied().is_some_and(is_letter)
                {
                    self.lex.start = p;
                    self.lex.len = self.pos - p;
                    self.lex.ty = LexType::LcIdent;
                    return &self.lex;
                }
                while self.curch == b'.' {
                    self.lex.flags |= FLAG_NAMESPACE_DOT;
                    self.nextch();
                    if is_uletter(self.curch) {
                        while is_ident_char(self.nextch()) {}
                        self.lex.start = p;
                        self.lex.len = self.pos - p;
                        self.lex.ty = LexType::UcIdent;
                        return &self.lex;
                    }
                    if is_lletter(self.curch) {
                        while is_ident_char(self.nextch()) {}
                    } else {
                        self.parse_error("Expected letter");
                        self.lex.ty = LexType::Error;
                        return &self.lex;
                    }
                }
                if self.curch == b'#' {
                    self.lex.flags |= FLAG_MAGIC_HASH;
                    // Eight hex digits, or the short form: at least five
                    // digits followed by a space (the C `ok == 2` path).
                    let mut short_form = false;
                    for i in 0..8 {
                        if !is_hexdigit(self.nextch()) {
                            if self.curch == b' ' && i >= 5 {
                                short_form = true;
                                break;
                            }
                            self.parse_error("Hex digit expected");
                            self.lex.ty = LexType::Error;
                            return &self.lex;
                        }
                    }
                    if !short_form {
                        self.nextch();
                    }
                }
                self.lex.start = p;
                self.lex.len = self.pos - p;
                self.lex.ty = LexType::LcIdent;
            }
            b'0'..=b'9' => {
                while is_digit(self.nextch()) {}
                self.lex.start = p;
                self.lex.len = self.pos - p;
                self.lex.ty = LexType::Num;
            }
            _ => {
                self.parse_error("Unknown lexem");
                self.lex.ty = LexType::Error;
            }
        }
        &self.lex
    }
}

/// C `is_whitespace(c) { return c <= 32; }` — note this covers all C0 control
/// characters, not just space/tab/newline.
fn is_whitespace(c: u8) -> bool {
    c <= 32
}

fn is_uletter(c: u8) -> bool {
    c.is_ascii_uppercase()
}

fn is_lletter(c: u8) -> bool {
    c.is_ascii_lowercase()
}

fn is_letter(c: u8) -> bool {
    is_uletter(c) || is_lletter(c)
}

fn is_digit(c: u8) -> bool {
    c.is_ascii_digit()
}

/// Lowercase hex only, exactly like the C `is_hexdigit`.
fn is_hexdigit(c: u8) -> bool {
    is_digit(c) || (b'a'..=b'f').contains(&c)
}

fn is_ident_char(c: u8) -> bool {
    is_digit(c) || is_letter(c) || c == b'_'
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lexes `src` to completion, returning `(type, flags, text)` triples.
    /// Real scheme files end with a newline; most tests append one so they
    /// avoid the exact-EOF quirk (see `exact_eof_quirks_match_c`).
    fn lex(src: &str) -> Vec<(LexType, i32, Vec<u8>)> {
        let mut src = src.to_owned();
        if !src.ends_with('\n') {
            src.push('\n');
        }
        lex_raw(&src)
    }

    /// Lexes `src` exactly as given, no trailing newline added.
    fn lex_raw(src: &str) -> Vec<(LexType, i32, Vec<u8>)> {
        let mut lexer = Lexer::new(src.as_bytes().to_vec());
        let mut out = Vec::new();
        if lexer.lex.ty == LexType::None {
            lexer.parse_lex();
        }
        loop {
            out.push((lexer.lex.ty, lexer.lex.flags, lexer.token_text().to_vec()));
            if matches!(lexer.lex.ty, LexType::Eof | LexType::Error) {
                break;
            }
            lexer.parse_lex();
        }
        out
    }

    #[test]
    fn keywords_and_uc_idents() {
        let tokens = lex("Final New Empty Foo Fina NewX");
        assert_eq!(
            tokens
                .iter()
                .map(|(ty, _, text)| (*ty, text.clone()))
                .collect::<Vec<_>>(),
            vec![
                (LexType::Final, b"Final".to_vec()),
                (LexType::New, b"New".to_vec()),
                (LexType::Empty, b"Empty".to_vec()),
                (LexType::UcIdent, b"Foo".to_vec()),
                (LexType::UcIdent, b"Fina".to_vec()),
                (LexType::UcIdent, b"NewX".to_vec()),
                (LexType::Eof, Vec::new()),
            ]
        );
    }

    #[test]
    fn magic_hash_identifier() {
        let tokens = lex("boolFalse#bc799737");
        assert_eq!(
            tokens[0],
            (
                LexType::LcIdent,
                FLAG_MAGIC_HASH,
                b"boolFalse#bc799737".to_vec()
            )
        );
    }

    #[test]
    fn magic_hash_short_form_with_space() {
        // Five hex digits then a space: the short `ok == 2` path; the token
        // includes the digits but not the space.
        let tokens = lex("foo#12345 x");
        assert_eq!(
            tokens[0],
            (LexType::LcIdent, FLAG_MAGIC_HASH, b"foo#12345".to_vec())
        );
        // The space is skipped afterwards; `x` is the next token.
        assert_eq!(tokens[1], (LexType::LcIdent, 0, b"x".to_vec()));
    }

    #[test]
    fn magic_hash_rejects_bad_hex() {
        let tokens = lex("foo#12g34567");
        assert_eq!(tokens.last().unwrap().0, LexType::Error);
    }

    #[test]
    fn namespaced_identifiers() {
        let tokens = lex("mtproto.foo#12345678 Bar");
        assert_eq!(
            tokens[0],
            (
                LexType::LcIdent,
                FLAG_NAMESPACE_DOT | FLAG_MAGIC_HASH,
                b"mtproto.foo#12345678".to_vec()
            )
        );
        // Namespace continuing into an uppercase part yields lex_uc_ident,
        // ending at the next dot; the dot and the rest lex separately.
        let tokens = lex("my.NS.Ident");
        assert_eq!(
            tokens[0],
            (LexType::UcIdent, FLAG_NAMESPACE_DOT, b"my.NS".to_vec())
        );
        assert_eq!(tokens[1], (LexType::Char, 0, b".".to_vec()));
        assert_eq!(tokens[2], (LexType::UcIdent, 0, b"Ident".to_vec()));
    }

    #[test]
    fn trailing_dot_is_separate_token() {
        let tokens = lex("foo. bar");
        assert_eq!(tokens[0], (LexType::LcIdent, 0, b"foo".to_vec()));
        assert_eq!(tokens[1], (LexType::Char, 0, b".".to_vec()));
        assert_eq!(tokens[2], (LexType::LcIdent, 0, b"bar".to_vec()));
    }

    #[test]
    fn triple_minus_and_chars() {
        let tokens = lex("---types---");
        assert_eq!(tokens[0], (LexType::TripleMinus, 0, b"---".to_vec()));
        assert_eq!(tokens[1], (LexType::LcIdent, 0, b"types".to_vec()));
        assert_eq!(tokens[2], (LexType::TripleMinus, 0, b"---".to_vec()));
        let tokens = lex("{x:int}");
        let chars: Vec<u8> = tokens
            .iter()
            .filter(|(ty, _, _)| *ty == LexType::Char)
            .map(|(_, _, text)| text[0])
            .collect();
        assert_eq!(chars.as_slice(), b"{:}");
    }

    #[test]
    fn numbers_and_comments() {
        let tokens = lex("123 // comment\n456");
        assert_eq!(tokens[0], (LexType::Num, 0, b"123".to_vec()));
        assert_eq!(tokens[1], (LexType::Num, 0, b"456".to_vec()));
        assert_eq!(tokens[2].0, LexType::Eof);
    }

    #[test]
    fn lone_slash_is_swallowed() {
        // C consumes the '/' while probing for '//'; it never becomes a token.
        // When a token character follows directly, lexing just continues...
        let tokens = lex("a /b");
        assert_eq!(
            tokens.iter().map(|(_, _, t)| t.clone()).collect::<Vec<_>>(),
            vec![b"a".to_vec(), b"b".to_vec(), Vec::new()]
        );
        // ...but whitespace after the swallowed '/' is NOT re-skipped (the
        // skip already ran before the probe), so the space hits the switch's
        // default arm and the C lexer errors out. Mirror that exactly.
        let tokens = lex("a / b");
        assert_eq!(tokens[0].1, 0);
        assert_eq!(tokens[0].2, b"a".to_vec());
        assert_eq!(tokens[1].0, LexType::Error);
    }

    #[test]
    fn unterminated_comment_stops_at_eof() {
        let tokens = lex("a // no newline");
        assert_eq!(tokens[0], (LexType::LcIdent, 0, b"a".to_vec()));
        assert_eq!(tokens[1].0, LexType::Eof);
    }

    #[test]
    fn unknown_character_is_error() {
        let tokens = lex("@");
        assert_eq!(tokens[0].0, LexType::Error);
    }

    #[test]
    fn bad_triple_minus_is_error() {
        let tokens = lex("- x");
        assert_eq!(tokens.last().unwrap().0, LexType::Error);
    }

    #[test]
    fn eof_inherits_previous_flags() {
        // C quirk: the eof token keeps the last real token's flags. This
        // input ends exactly after the magic digits, which also exercises the
        // exact-EOF truncation below.
        let tokens = lex_raw("foo#12345678");
        assert_eq!(
            tokens[0],
            (LexType::LcIdent, FLAG_MAGIC_HASH, b"foo#1234567".to_vec())
        );
        assert_eq!(tokens[1].0, LexType::Eof);
        assert_eq!(tokens[1].1, FLAG_MAGIC_HASH);
    }

    #[test]
    fn exact_eof_quirks_match_c() {
        // nextch freezes at the last byte, so a token ending exactly at EOF
        // loses its last character, and the keyword check runs on the
        // truncated text — exactly like the C lexer.
        let tokens = lex_raw("NewX");
        assert_eq!(tokens[0], (LexType::New, 0, b"New".to_vec()));
        let tokens = lex_raw("123");
        assert_eq!(tokens[0], (LexType::Num, 0, b"12".to_vec()));
    }

    #[test]
    fn line_tracking() {
        // The lookahead char after a token is already consumed, so the
        // position reported for a token includes it: after `a` the newline
        // has been read (line 1, pos 0).
        let mut lexer = Lexer::new(b"a\nbb\n\nc".to_vec());
        lexer.parse_lex(); // a (lookahead consumed the newline)
        assert_eq!((lexer.line(), lexer.line_pos()), (1, 0));
        lexer.parse_lex(); // bb (lookahead consumed its newline)
        assert_eq!((lexer.line(), lexer.line_pos()), (2, 0));
        lexer.parse_lex(); // c (lookahead hit EOF, position frozen)
        assert_eq!((lexer.line(), lexer.line_pos()), (3, 1));
    }

    #[test]
    fn empty_input() {
        assert_eq!(lex(""), vec![(LexType::Eof, 0, Vec::new())]);
    }
}
