//! Port of `tdtl`: TL scheme lexer/parser producing the scheme AST.
//!
//! S06 ported the lexer (`td/generate/tl-parser/tl-parser.c`'s `parse_lex`),
//! verified by diffing token dumps against the C original for all five
//! schemes (`conformance/tl_oracle/`). S07 adds the parser on top:
//!
//! - [`parse_tree`]: the recursive-descent stage (`tl_parse_lex`),
//! - [`program`]: the program builder (`tl_parse`) producing [`TlProgram`],
//!   with constructor ids assigned by CRC32 of the canonical combinator
//!   string (`tl_count_combinator_name`),
//! - [`tlo`]: the `.tlo` serialization stage (`write_types`), byte-identical
//!   to the C original for all five schemes.

pub mod crc32;
pub mod lexer;
pub mod parse_tree;
pub mod program;
pub mod tlo;

pub use lexer::{CurLex, FLAG_MAGIC_HASH, FLAG_NAMESPACE_DOT, LexType, Lexer};
pub use program::{
    Act, CTData, CTNode, CType, TlConstructor, TlParseError, TlProgram, TlType, TlVar,
};
