//! Dumps the tdtl lexer's token stream for a TL scheme file, one line per
//! token, in the exact format produced by the C oracle
//! (`conformance/tl_oracle/dump_tokens.c`) — the S06 differential check:
//!
//! ```text
//! #<idx> type=lex_xxx len=<len> flags=<flags> line=<line> line_pos=<pos> text=<escaped>
//! ```

use std::io::{self, BufWriter, Write};

use tdtl::{LexType, Lexer};

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(path), None) = (args.next(), args.next()) else {
        eprintln!("usage: tl-token-dump <scheme.tl>");
        std::process::exit(2);
    };
    let text = std::fs::read(&path).unwrap_or_else(|e| {
        eprintln!("{path}: {e}");
        std::process::exit(1);
    });
    let stdout = io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    let mut lexer = Lexer::new(text);
    // Mirror tl_parse_lex: the first token is pre-fetched when none was read.
    if lexer.lex.ty == LexType::None {
        lexer.parse_lex();
    }
    let mut idx = 0usize;
    loop {
        idx += 1;
        let lex = lexer.lex;
        let token = lexer.token_text();
        write!(
            out,
            "#{idx} type={} len={} flags={} line={} line_pos={} text=",
            lex.ty.name(),
            lex.len,
            lex.flags,
            lexer.line(),
            lexer.line_pos()
        )
        .unwrap();
        if token.is_empty() {
            write!(out, "-").unwrap();
        } else {
            for &byte in token {
                write_escaped(&mut out, byte).unwrap();
            }
        }
        writeln!(out).unwrap();
        if matches!(lex.ty, LexType::Eof | LexType::Error) {
            break;
        }
        lexer.parse_lex();
    }
}

/// Must stay byte-identical to `esc` in the C oracle.
fn write_escaped<W: Write>(out: &mut W, byte: u8) -> io::Result<()> {
    match byte {
        0x21..=0x7e => {
            if byte == b'\\' {
                out.write_all(b"\\\\")
            } else {
                out.write_all(&[byte])
            }
        }
        _ => write!(out, "\\x{byte:02x}"),
    }
}
