//! Dumps the tdtl scheme program (types, constructors, functions with their
//! ids) for a TL scheme file, in the exact format produced by the C oracle
//! (`conformance/tl_oracle/dump_scheme.c`) — the S07 differential check:
//!
//! ```text
//! types=<T> constructors=<C> functions=<F>
//! type name=<08x> params=<n> params_types=<ll> flags=<f> ctors=<k> id=<id> print=<print_id>
//! ctor name=<08x> id=<id> real=<real_id|-> type=<type_id|->
//! func name=<08x> id=<id> real=<real_id|-> type=<type_id|->
//! ```
//!
//! Types iterate byte-sorted by id (the C treap in-order walk, like
//! `write_types`), constructors in per-type array order, functions
//! byte-sorted by id.

use std::io::{self, BufWriter, Write};

use tdtl::TlProgram;

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(path), None) = (args.next(), args.next()) else {
        eprintln!("usage: tl-scheme-dump <scheme.tl>");
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
    let stdout = io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    write_program(&mut out, &program).unwrap();
}

fn write_program<W: Write>(out: &mut W, p: &TlProgram) -> io::Result<()> {
    writeln!(
        out,
        "types={} constructors={} functions={}",
        p.total_types, p.total_ctors, p.total_funcs
    )?;
    for &tid in p.type_ids.values() {
        let t = &p.types[tid];
        writeln!(
            out,
            "type name={:08x} params={} params_types={} flags={} ctors={} id={} print={}",
            t.name,
            t.params_num,
            t.params_types,
            t.flags,
            t.constructors.len(),
            t.id,
            t.print_id
        )?;
        for &c in &t.constructors {
            write_ctor(out, p, c)?;
        }
    }
    for &c in p.func_ids.values() {
        write_ctor(out, p, c)?;
    }
    Ok(())
}

fn write_ctor<W: Write>(out: &mut W, p: &TlProgram, c: usize) -> io::Result<()> {
    let c = &p.ctors[c];
    writeln!(
        out,
        "ctor name={:08x} id={} real={} type={}",
        c.name,
        c.id,
        c.real_id.as_deref().unwrap_or("-"),
        c.ty.map(|t| p.types[t].id.as_str()).unwrap_or("-")
    )
}
