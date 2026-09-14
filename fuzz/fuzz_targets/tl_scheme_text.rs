//! Fuzz target: the TL scheme text lexer/parser (`tdtl`) — arbitrary bytes
//! through the lexer and the parse-tree builder (the codegen front end;
//! malformed scheme text must error, never crash).

#![no_main]

use libfuzzer_sys::fuzz_target;
use tdtl::lexer::Lexer;
use tdtl::parse_tree::tl_parse_lex;

fuzz_target!(|data: &[u8]| {
    let mut lexer = Lexer::new(data.to_vec());
    // Drain the token stream; `tl_parse_lex` recurses per construct, so a
    // deeply nested input explores the parser depth paths.
    while let Some(_tree) = tl_parse_lex(&mut lexer) {}
});
