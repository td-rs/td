//! Serializes a TL scheme file to the `.tlo` format (serialized schema V2) —
//! the S08 differential counterpart of `tl-parser -e` / `tlc -e`: the bytes
//! must match td's unmodified `write_types` output for every scheme
//! (`conformance/tl_oracle/verify_tlo.sh`).

use std::io::{self, Write};

use tdtl::{TlProgram, tlo};

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(path), None) = (args.next(), args.next()) else {
        eprintln!("usage: tl-tlo <scheme.tl>");
        std::process::exit(2);
    };
    let text = std::fs::read(&path).unwrap_or_else(|e| {
        eprintln!("{path}: {e}");
        std::process::exit(1);
    });
    let program = TlProgram::parse(text).unwrap_or_else(|e| {
        eprintln!("{path}: {e}");
        std::process::exit(1);
    });
    let bytes = tlo::write_tlo(&program);
    let stdout = io::stdout();
    let mut out = stdout.lock();
    out.write_all(&bytes).unwrap();
    out.flush().unwrap();
}
