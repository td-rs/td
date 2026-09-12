//! Port of `tdtl`: TL scheme lexer/parser producing the scheme AST.
//!
//! S06 provides the lexer — a faithful port of the C tl-parser lexer
//! (`td/generate/tl-parser/tl-parser.c`), verified by diffing token dumps
//! against the C original for all five schemes (`conformance/tl_oracle/`).
//! S07 will add the parser on top.

mod lexer;

pub use lexer::{CurLex, FLAG_MAGIC_HASH, FLAG_NAMESPACE_DOT, LexType, Lexer};
