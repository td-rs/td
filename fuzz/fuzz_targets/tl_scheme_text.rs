//! Fuzz target: the TL scheme text lexer/parser (`tdtl`) — arbitrary bytes
//! through `tl_parse_lex`, which parses the WHOLE program in one call (the
//! C `tl_parse` contract; it returns `None` on lexer error or parse
//! failure). Malformed scheme text must error, never crash.

#![no_main]

use libfuzzer_sys::fuzz_target;
use tdtl::lexer::Lexer;
use tdtl::parse_tree::tl_parse_lex;

fuzz_target!(|data: &[u8]| {
    let mut lexer = Lexer::new(data.to_vec());
    let _ = tl_parse_lex(&mut lexer);
});
