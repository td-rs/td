//! `tl_writer_rust` (S09 model + S10 binary store + S11 binary parse):
//! emits the Rust data model of a scheme — one struct per constructor, one
//! enum per multi-constructor type, one struct per function plus the umbrella
//! enum — and the TL binary `store`/`store_boxed` and `parse`/`parse_bare`
//! implementations matching td's generated C++ (`TlStorerUnsafe` /
//! `TlBufferParser` semantics from `td/generate/tl_writer_cpp.cpp` +
//! `tdutils/tl_storers.h` / `tl_parsers.h`).
//!
//! Binary layout rules (all verified against td's C++ in S10):
//! - primitives are little-endian memcpy's; `bool` stores the boolTrue/
//!   boolFalse constructor ids;
//! - strings/bytes use the TL framing (1-byte length up to 253, then 0xfe/3
//!   bytes, then 0xff/4+3 bytes) zero-padded to a 4-byte multiple counting
//!   the 1-byte header;
//! - capitalized `Vector<T>` vectors store the `vector#1cb5c415` id, then
//!   the int32 count, then the elements; lower-case `vector<t>` stores just
//!   the count (bare form, `TlStoreVector`);
//! - boxed objects store the 4-byte constructor id first, bare ones
//!   (lower-case references) don't;
//! - the `flags:#` word stores `flags | (?true bools << their bits)`, and
//!   every other conditional field is stored only when its bit is set.
//!
//! Parse semantics mirror `TlBufferParser` exactly: bounds-checked reads
//! ("Not enough data to read"), the `#` word must be non-negative, bools are
//! id-checked ("Bool expected"), vectors validate the count against the
//! remaining length ("Wrong vector length") and re-check the boxed id,
//! string fields get td's `\0`→space + UTF-8 truncation sanitization while
//! bytes stay raw.

use std::collections::BTreeSet;

use tdtl::TlProgram;

use crate::model::{Field, FieldType, Prim, SchemeModel, TypeKind, class_name};

/// Rust keywords and reserved words that cannot appear as plain identifiers.
/// tl field names in the schemes avoid them; anything else becomes `r#name`.
fn is_keyword(name: &str) -> bool {
    const KEYWORDS: &[&str] = &[
        // Strict keywords.
        "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
        "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move",
        "mut", "pub", "ref", "return", "self", "static", "struct", "super", "trait", "true",
        "type", "unsafe", "use", "where", "while",
        // Reserved keywords (raw identifiers still work for these).
        "abstract", "become", "box", "do", "final", "gen", "macro", "override", "priv", "try",
        "typeof", "unsized", "virtual", "yield",
    ];
    KEYWORDS.contains(&name)
}

/// Class/variant naming: td's `gen_class_name` (non-alphanumerics become
/// `_`, case preserved), so Rust names match the C++ classes one-to-one —
/// including case-distinct pairs like type `AiComposeTone` vs its ctor
/// `aiComposeTone`.
fn capitalize(name: &str) -> String {
    crate::model::class_name(name)
}

fn field_ident(name: &str) -> String {
    // `self` cannot be used even as a raw identifier; td's gen_field_name
    // suffixes it too (`self_`).
    if name == "self" {
        "self_".to_owned()
    } else if name == "_" {
        // Anonymous multiplicity args (`int128 4*[ int ]`): legal C++
        // identifiers (`_`), invalid Rust. The only anonymous args in the
        // pinned schemes are the int128/int256 word arrays.
        "words".to_owned()
    } else if is_keyword(name) {
        format!("r#{name}")
    } else {
        name.to_owned()
    }
}

/// The bit mask expression for a conditional's gate. Bit 0 collapses to a
/// bare `1` (a `1 << 0` shift trips clippy's identity_op in the generated
/// code).
fn bit_mask(bit: i32) -> String {
    if bit == 0 {
        "1".to_owned()
    } else {
        format!("1 << {bit}")
    }
}

/// Per-scheme generation options.
pub struct WriterOpts {
    /// The umbrella enum name for the functions section.
    pub umbrella: &'static str,
    /// Emit serde derives (td_api only — the other schemes have no JSON).
    pub serde: bool,
    /// Emit the td_api JSON methods (`to_json*`/`from_json*`, S12) — td_api
    /// only; the other schemes have no JSON surface.
    pub json: bool,
    /// Emit the C++ `to_string` debug surface (`store_debug` +
    /// `to_debug_string`, M.14.2) — td_api only: the CLI prints responses
    /// through td's `TlStorerToString` format, so the port needs the same
    /// dynamic `to_string(const BaseObject&)` surface the C++ CLI reads.
    pub tostring: bool,
    /// Also compile the sample synthesizers outside `cfg(test)`, behind this
    /// cargo feature of the including crate — the S15 replay oracle bins
    /// build with it (`conformance-samples`).
    pub samples_feature: Option<&'static str>,
    /// Parse `string` fields as raw bytes (`Vec<u8>` via `r.bytes()`, the
    /// C++ `TlParser` path) instead of the sanitizing `r.string()` —
    /// td applies `TlBufferParser` sanitization only to telegram_api; the
    /// mtproto/secret/e2e schemes carry binary payloads (pq, dh_prime, g_a)
    /// that must not be mangled (S60 fix).
    pub raw_strings: bool,
    /// Provenance comment.
    pub source: &'static str,
}

/// Renders the whole generated module for a scheme.
pub fn write_rust(p: &TlProgram, m: &SchemeModel, opts: &WriterOpts) -> String {
    let mut out = String::with_capacity(8 << 20);
    out.push_str("// @generated by tl-codegen — DO NOT EDIT.\n");
    out.push_str(&format!("// Source: {}\n\n", opts.source));
    if opts.serde {
        out.push_str("use serde::{Deserialize, Serialize};\n\n");
    }
    out.push_str(SUPPORT_MODULE);
    if opts.json {
        out.push_str(JSON_SUPPORT_MODULE);
    }
    if opts.tostring {
        out.push_str(TOSTRING_SUPPORT_MODULE);
    }

    let mut emitted_names = BTreeSet::new();

    for &tid in p.type_ids.values() {
        match &m.type_kinds[tid] {
            TypeKind::Prim(_) | TypeKind::Vector => {}
            TypeKind::Struct { ctor } => {
                let name = class_name(&p.types[tid].id);
                emit_struct(&mut out, p, m, opts, *ctor, &name);
                emitted_names.insert(name.clone());
            }
            TypeKind::Enum { ctors } => {
                // The `Type` meta type classifies as an empty enum and is
                // never emitted.
                if ctors.is_empty() {
                    continue;
                }
                for &c in ctors {
                    let name = capitalize(&p.ctors[c].id);
                    emit_struct(&mut out, p, m, opts, c, &name);
                    assert!(
                        emitted_names.insert(name.clone()),
                        "duplicate Rust struct name {name}"
                    );
                }
                let enum_name = class_name(&p.types[tid].id);
                let default_ctor = m.breaking_ctor[tid].unwrap_or(ctors[0]);
                emit_enum(&mut out, p, opts, ctors, &enum_name, default_ctor);
                assert!(
                    emitted_names.insert(enum_name.clone()),
                    "enum name {enum_name} collides with a struct"
                );
            }
        }
    }

    for &f in &m.functions {
        let name = capitalize(&p.ctors[f].id);
        emit_struct(&mut out, p, m, opts, f, &name);
        assert!(
            emitted_names.insert(name.clone()),
            "function struct name {name} collides"
        );
    }
    emit_enum(
        &mut out,
        p,
        opts,
        &m.functions,
        opts.umbrella,
        m.safe_function,
    );

    if opts.json {
        emit_object_umbrella(&mut out, p, m, opts);
    }

    out.push_str(&format!(
        "pub const SCHEME_TYPES: usize = {};\n\
         pub const SCHEME_CTORS: usize = {};\n\
         pub const SCHEME_FUNCTIONS: usize = {};\n",
        p.total_types, p.total_ctors, p.total_funcs
    ));
    // The M.11 dispatcher census: every scheme function constructor name, in
    // the treap (ctor-id-sorted) order, for the tdclient census fixture test.
    out.push_str("pub const ALL_FUNCTION_NAMES: &[&str] = &[\n");
    for &f in &m.functions {
        out.push_str(&format!("    {:?},\n", p.ctors[f].id));
    }
    out.push_str("];\n");
    let enums = m
        .type_kinds
        .iter()
        .filter(|k| matches!(k, TypeKind::Enum { ctors } if !ctors.is_empty()))
        .count();
    let struct_kinds = m
        .type_kinds
        .iter()
        .filter(|k| matches!(k, TypeKind::Struct { .. }))
        .count();
    let enum_ctors: usize = m
        .type_kinds
        .iter()
        .map(|k| match k {
            TypeKind::Enum { ctors } => ctors.len(),
            _ => 0,
        })
        .sum();
    // Every non-builtin ctor and every function get a struct; multi-ctor
    // types additionally get an enum. The builtin count is the ctors owned
    // by primitive/Vector types — schemes declare different subsets of them
    // (mtproto_api has 5, telegram_api 9), so it is computed, not assumed.
    let builtin_ctors: usize = m
        .type_kinds
        .iter()
        .zip(p.types.iter())
        .filter(|(k, _)| matches!(k, TypeKind::Prim(_)) || matches!(k, TypeKind::Vector))
        .map(|(_, t)| t.constructors.len())
        .sum();
    let structs = struct_kinds + enum_ctors + p.total_funcs;
    assert_eq!(
        struct_kinds + enum_ctors,
        p.total_ctors - builtin_ctors,
        "ctor accounting"
    );
    out.push_str(&format!(
        "pub const GENERATED_STRUCTS: usize = {structs};\n\
         pub const GENERATED_ENUMS: usize = {enums};\n"
    ));
    emit_samples_and_tests(&mut out, p, m, opts);
    out
}

/// Emits the `#[cfg(test)]` S11 machinery: a `samples` module with
/// depth-capped value synthesizers (one per ctor, one per type, one for the
/// umbrella), one store→parse round-trip test per ctor, and a malformed-
/// input test exercising the shared error paths. Depth 0 empties vectors and
/// keeps scalar object references at depth 0 — termination is guaranteed
/// because td's schemes have no scalar-only type cycles (asserted in
/// `SchemeModel::build`).
fn emit_samples_and_tests(out: &mut String, p: &TlProgram, m: &SchemeModel, opts: &WriterOpts) {
    // ---- samples ---------------------------------------------------------
    let samples_cfg = match opts.samples_feature {
        Some(feature) => format!("#[cfg(any(test, feature = \"{feature}\"))]\n"),
        None => "#[cfg(test)]\n".to_owned(),
    };
    out.push_str(&samples_cfg);
    out.push_str("#[allow(non_snake_case, dead_code)]\npub mod samples {\n");
    out.push_str("    use super::*;\n\n");
    out.push_str(
        "    /// Runs a test body on a thread with a large stack: the depth-3\n\
         \x20   /// sample builders materialize large intermediates in debug builds\n\
         \x20   /// (no inlining), which overflows the default test-thread stack for\n\
         \x20   /// deep scheme types (e.g. td_api chatEvent).\n\
         \x20   pub fn __stack<F: FnOnce() + Send + 'static>(f: F) {\n\
         \x20       let joined = std::thread::Builder::new()\n\
         \x20           .stack_size(64 * 1024 * 1024)\n\
         \x20           .spawn(f)\n\
         \x20           .expect(\"spawn test thread\")\n\
         \x20           .join();\n\
         \x20       // Re-panic with the original payload: `.expect` on the join\n\
         \x20       // result would print `Any { .. }` and lose the assert message.\n\
         \x20       if let Err(payload) = joined {\n\
         \x20           if let Some(s) = payload.downcast_ref::<String>() {\n\
         \x20               panic!(\"{s}\");\n\
         \x20           }\n\
         \x20           if let Some(s) = payload.downcast_ref::<&'static str>() {\n\
         \x20               panic!(\"{s}\");\n\
         \x20           }\n\
         \x20           std::panic::resume_unwind(payload);\n\
         \x20       }\n\
         \x20   }\n\n",
    );
    for &tid in p.type_ids.values() {
        let ctors: Vec<usize> = match &m.type_kinds[tid] {
            TypeKind::Struct { ctor } => vec![*ctor],
            TypeKind::Enum { ctors } if !ctors.is_empty() => ctors.clone(),
            _ => continue,
        };
        let name = m.type_name(p, tid);
        let ctor_struct = |c: usize| match &m.type_kinds[tid] {
            // Single-ctor types name their struct after the type; enum
            // variant structs after the ctor.
            TypeKind::Struct { .. } => name.clone(),
            _ => capitalize(&p.ctors[c].id),
        };
        let first = ctors[0];
        let make = |c: usize, depth: &str| {
            let s = ctor_struct(c);
            match &m.type_kinds[tid] {
                TypeKind::Struct { .. } => format!("s_{s}({depth})"),
                _ => format!("{name}::{s}(s_{s}({depth}))"),
            }
        };
        match m.breaking_ctor[tid] {
            // Cyclic type: at depth 0 pick the min-rank breaking ctor —
            // scalar children then have strictly smaller ranks, so the
            // synthesizer terminates even though the scheme recurses.
            Some(bc) => {
                let breaker = make(bc, "0");
                let first_mk = make(first, "depth");
                out.push_str(&format!(
                    "    pub fn t_{name}(depth: u32) -> {name} {{\n        if depth == 0 {{\n            {breaker}\n        }} else {{\n            {first_mk}\n        }}\n    }}\n\n"
                ));
            }
            None => {
                let first_mk = make(first, "depth");
                out.push_str(&format!(
                    "    pub fn t_{name}(depth: u32) -> {name} {{\n        {first_mk}\n    }}\n\n"
                ));
            }
        }
    }
    // Umbrella: first function at depth > 0 (covers invokeWithLayer et al),
    // the !X-free function at depth 0 (their Box<Function> fields would
    // otherwise recurse forever).
    let first_fn = capitalize(&p.ctors[m.functions[0]].id);
    let safe_fn = capitalize(&p.ctors[m.safe_function].id);
    out.push_str(&format!(
        "    pub fn t_{}(depth: u32) -> {0} {{\n        if depth == 0 {{\n            {0}::{safe_fn}(s_{safe_fn}(0))\n        }} else {{\n            {0}::{first_fn}(s_{first_fn}(depth))\n        }}\n    }}\n\n",
        opts.umbrella
    ));
    // One builder per emitted struct: non-builtin ctors and functions.
    for &tid in p.type_ids.values() {
        match &m.type_kinds[tid] {
            TypeKind::Struct { ctor } => {
                let name = m.type_name(p, tid);
                emit_sample_builder(out, p, m, opts, *ctor, &name);
            }
            TypeKind::Enum { ctors } => {
                for &c in ctors {
                    let name = capitalize(&p.ctors[c].id);
                    emit_sample_builder(out, p, m, opts, c, &name);
                }
            }
            _ => {}
        }
    }
    for &f in &m.functions {
        let name = capitalize(&p.ctors[f].id);
        emit_sample_builder(out, p, m, opts, f, &name);
    }

    // ---- S15 replay dump --------------------------------------------------
    // One `<tag> <T|F> <ctor-id> <hex>` line per sample (depths 1 and 3);
    // the C++ replay oracle fetches each blob with td's own parser and
    // re-stores it — the bytes must come back identical.
    if opts.samples_feature.is_some() {
        out.push_str(
            "    fn __hex(bytes: &[u8]) -> String {\n        \
             let mut s = String::with_capacity(bytes.len() * 2);\n        \
             for b in bytes {\n            s.push_str(std::format!(\"{b:02x}\").as_str());\n        }\n        \
             s\n    }\n\n",
        );
        out.push_str(
            "    /// S15 replay oracle dump: prints one line per sample; the\n\
             \x20   /// kind is T for types (stored boxed) and F for functions.\n    \
             pub fn __replay_dump(tag: &str) {\n",
        );
        for &tid in p.type_ids.values() {
            match &m.type_kinds[tid] {
                TypeKind::Struct { ctor } => {
                    emit_replay_lines(out, p, *ctor, true, &m.type_name(p, tid));
                }
                TypeKind::Enum { ctors } if !ctors.is_empty() => {
                    let target = class_name(&p.types[tid].id);
                    let mut seen = BTreeSet::new();
                    for &c in ctors {
                        if seen.insert(p.ctors[c].name) {
                            emit_replay_lines(out, p, c, false, &target);
                        }
                    }
                }
                _ => {}
            }
        }
        {
            let mut seen = BTreeSet::new();
            for &f in &m.functions {
                if seen.insert(p.ctors[f].name) {
                    emit_replay_lines(out, p, f, false, opts.umbrella);
                }
            }
        }
        out.push_str("    }\n");
    }
    out.push_str("}\n\n");

    // ---- round-trip tests -----------------------------------------------
    for &tid in p.type_ids.values() {
        match &m.type_kinds[tid] {
            TypeKind::Struct { ctor } => {
                let name = m.type_name(p, tid);
                emit_roundtrip_test(out, p, *ctor, &name, true, &name, opts.json);
            }
            TypeKind::Enum { ctors } => {
                let target = class_name(&p.types[tid].id);
                let mut seen = BTreeSet::new();
                for &c in ctors {
                    // Duplicate-id ctors are shadowed by the first
                    // declaration in the read dispatch — not round-trippable.
                    if seen.insert(p.ctors[c].name) {
                        let name = capitalize(&p.ctors[c].id);
                        emit_roundtrip_test(out, p, c, &target, false, &name, opts.json);
                    }
                }
            }
            _ => {}
        }
    }
    {
        let mut seen = BTreeSet::new();
        for &f in &m.functions {
            if seen.insert(p.ctors[f].name) {
                let name = capitalize(&p.ctors[f].id);
                emit_roundtrip_test(out, p, f, opts.umbrella, false, &name, opts.json);
            }
        }
    }

    if opts.tostring {
        emit_tostring_golden_test(out);
    }

    // ---- malformed-input seeds ------------------------------------------
    let first_fn = capitalize(&p.ctors[m.functions[0]].id);
    let (first_enum, first_struct) = {
        let mut e = None;
        let mut s = None;
        for &tid in p.type_ids.values() {
            match &m.type_kinds[tid] {
                TypeKind::Enum { ctors } if !ctors.is_empty() => {
                    e.get_or_insert(class_name(&p.types[tid].id));
                }
                TypeKind::Struct { .. } => {
                    s.get_or_insert(m.type_name(p, tid));
                }
                _ => {}
            }
        }
        (
            e.expect("scheme has no enum"),
            s.expect("scheme has no struct"),
        )
    };
    out.push_str(&format!(
        r#"#[cfg(test)]
#[test]
fn __malformed_inputs_do_not_panic() {{
    // Crafted seeds: truncated and wrong ids, overrun strings in all three
    // framings, absurd vector counts, negative flags words, and bool words
    // that are neither id. Every parse path must return Err, never panic
    // (td's own contract: bounds-checked reads with set_error).
    let seeds: Vec<Vec<u8>> = vec![
        Vec::new(),
        vec![0],
        vec![0, 0, 0],
        vec![0, 0, 0, 0],
        0x1234_5678i32.to_le_bytes().to_vec(),
        (-1i32).to_le_bytes().to_vec(),
        vec![5, b'a', b'b', b'c'],
        vec![254, 10, 0, 0],
        vec![255, 255, 255, 255, 255, 255, 255, 255, 255],
        vec![tl::VECTOR_ID as u8, 0, 0, 0, 0xff, 0xff, 0xff, 0x7f],
        vec![0x00, 0xb5, 0xc4, 0x15, 0, 0, 0, 0],
        vec![0x99, 0x72, 0x75, 0xb5, 0x99, 0x72, 0x75, 0xb5, 0x13, 0x37],
    ];
    for seed in &seeds {{
        let _ = {first_fn}::parse(seed);
        let _ = {first_enum}::parse(seed);
        let _ = {first_struct}::parse_bare(seed);
    }}
}}
"#
    ));

    if !opts.json {
        return;
    }

    // ---- malformed JSON seeds --------------------------------------------
    out.push_str(&format!(
        r#"#[cfg(test)]
#[test]
fn __malformed_json_does_not_panic() {{
    // JSON-side twin of the binary seeds: parser edge cases (truncated
    // strings/objects, bad escapes, lone surrogates, depth bomb, trailing
    // garbage, empty and well-formed-but-wrong inputs). Every from_json
    // path must return Ok or Err — never panic.
    let deep = "[".repeat(200);
    let seeds = [
        "",
        " ",
        "{{",
        "}}",
        "nul",
        "tru",
        "{{\"@type\":",
        "{{\"@type\"}}",
        "{{\"@type\":\"{first_fn_name}\"}} x",
        "[1,]",
        "[,]",
        "\"\\ud800\"",
        "\"\\uZZZZ\"",
        "\"\\q\"",
        "\"unterminated",
        "999e",
        "-",
        "{{\"@type\":42}}",
        "{{@type:\"x\"}}",
        deep.as_str(),
    ];
    for seed in &seeds {{
        let _ = {first_fn}::from_json(seed);
        let _ = {first_enum}::from_json(seed);
        let _ = {first_struct}::from_json(seed);
    }}
}}
"#,
        first_fn_name = p.ctors[m.functions[0]].id
    ));
}

/// Emits the two replay lines (depths 1 and 3) for one sample: structs
/// store boxed via their own method; enum variants and functions wrap in
/// the target enum and dispatch through its `store`.
fn emit_replay_lines(out: &mut String, p: &TlProgram, c: usize, bare: bool, target: &str) {
    let name = capitalize(&p.ctors[c].id);
    let kind = if target == "Function" { "F" } else { "T" };
    for depth in [1, 3] {
        // Single-ctor types name their sample builder after the type
        // (`s_Int128`), enum variants and functions after the ctor.
        let sample = if bare {
            format!("s_{target}({depth})")
        } else {
            format!("{target}::{name}(s_{name}({depth}))")
        };
        let store = if bare { "store_boxed" } else { "store" };
        out.push_str(&format!(
            "        {{\n            let mut out = Vec::new();\n            {sample}.{store}(&mut out);\n            std::println!(\"{{tag}} {kind} {} {} {{}}\", __hex(&out));\n        }}\n",
            p.ctors[c].name as i32,
            p.ctors[c].id
        ));
    }
}

/// Emits `samples::s_<StructName>(depth)` for one constructor struct.
fn emit_sample_builder(
    out: &mut String,
    p: &TlProgram,
    m: &SchemeModel,
    opts: &WriterOpts,
    c: usize,
    name: &str,
) {
    let fields = &m.ctor_fields[c];
    out.push_str(&format!("    pub fn s_{name}(depth: u32) -> {name} {{\n"));
    out.push_str("        let _ = depth;\n");
    if fields.is_empty() {
        out.push_str(&format!("        {name}\n    }}\n\n"));
        return;
    }
    out.push_str(&format!("        {name} {{\n"));
    for f in fields {
        out.push_str(&format!(
            "            {}: {},\n",
            field_ident(&f.name),
            sample_expr(p, m, opts, fields, &f.ty, "depth")
        ));
    }
    out.push_str("        }\n    }\n\n");
}

/// The synthesizer expression for one field's type. Flags words OR in every
/// conditional bit (so all conditionals round-trip); vectors hold one
/// element above depth 0 and are empty at depth 0; object references
/// decrement the depth.
fn sample_expr(
    p: &TlProgram,
    m: &SchemeModel,
    opts: &WriterOpts,
    fields: &[Field],
    ty: &FieldType,
    depth: &str,
) -> String {
    match ty {
        FieldType::Prim(prim) => match prim {
            Prim::Nat => {
                // The flags word sample ORs every conditional bit in the
                // ctor — a superset is fine for round-tripping (any set bit
                // is stored and read back verbatim), and it exercises every
                // conditional field's wire presence.
                let mut bits: u64 = 0;
                for f in fields {
                    if let FieldType::Conditional { bit, .. } = &f.ty {
                        bits |= 1u64 << bit;
                    }
                }
                format!("{bits}i32")
            }
            Prim::Int32 => "0x1234_5678i32".to_owned(),
            Prim::Int53 | Prim::Int64 => "0x1234_5678_9abc_def0i64".to_owned(),
            Prim::Double => "1.5f64".to_owned(),
            Prim::Bool | Prim::True => "true".to_owned(),
            Prim::String => {
                if opts.raw_strings {
                    "b\"tl-sample\".to_vec()".to_owned()
                } else {
                    "\"tl-sample\".to_owned()".to_owned()
                }
            }
            Prim::Bytes => "b\"tl\\x00\\x01\".to_vec()".to_owned(),
        },
        FieldType::Vector { inner, .. } => {
            format!(
                "if {depth} == 0 {{ Vec::new() }} else {{ vec![{}] }}",
                element_expr(p, m, opts, fields, inner, &format!("{depth} - 1"))
            )
        }
        FieldType::FixedArray { len, inner } => {
            // The element expr is a primitive literal (int words) — Copy,
            // so the repeat form builds all N elements.
            format!("[{}; {len}]", sample_expr(p, m, opts, fields, inner, depth))
        }
        FieldType::Conditional { inner, .. } => {
            // The element form never wraps, so a flag-gated class field
            // takes exactly one `Some` (here) — not one per recursion level.
            let expr = element_expr(p, m, opts, fields, inner, depth);
            if is_object_field(m, ty) {
                format!("Some({expr})")
            } else {
                expr
            }
        }
        FieldType::Boxed(t) | FieldType::Bare(t) => {
            // Parenthesized: depth may already be an expression like
            // `depth - 1` inside vector elements. Class fields are
            // `Option` — samples always carry a present object; the fixed
            // word types (int128/int256/int512) stay bare values.
            let expr = format!("t_{}(({depth}).saturating_sub(1))", m.type_name(p, *t));
            let expr = if m.cyclic[*t] {
                format!("Box::new({expr})")
            } else {
                expr
            };
            if is_class(m, ty) {
                format!("Some({expr})")
            } else {
                expr
            }
        }
        FieldType::Function => {
            format!(
                "Some(Box::new(t_{}(({depth}).saturating_sub(1))))",
                opts.umbrella
            )
        }
    }
}

/// The synthesizer expression for a vector ELEMENT: class references stay
/// bare (a `vector<T>` holds `T` values, never `Option<T>`).
fn element_expr(
    p: &TlProgram,
    m: &SchemeModel,
    opts: &WriterOpts,
    fields: &[Field],
    ty: &FieldType,
    depth: &str,
) -> String {
    match ty {
        FieldType::Conditional { inner, .. } => element_expr(p, m, opts, fields, inner, depth),
        FieldType::Boxed(t) | FieldType::Bare(t) => {
            let expr = format!("t_{}(({depth}).saturating_sub(1))", m.type_name(p, *t));
            if m.cyclic[*t] {
                format!("Box::new({expr})")
            } else {
                expr
            }
        }
        _ => sample_expr(p, m, opts, fields, ty, depth),
    }
}

/// Emits one round-trip test: build a depth-3 sample, store, parse, compare.
/// Single-ctor types additionally round-trip the bare (unboxed) form; JSON
/// schemes (td_api) also round-trip to_json → from_json.
fn emit_roundtrip_test(
    out: &mut String,
    _p: &TlProgram,
    _c: usize,
    target: &str,
    bare: bool,
    name: &str,
    json: bool,
) {
    out.push_str("#[cfg(test)]\n#[test]\n#[allow(non_snake_case)]\n");
    // Single-ctor types round-trip the struct directly; enum variants and
    // functions wrap the sample in the target enum first (parse yields the
    // enum, not the variant struct).
    let sample = if bare {
        format!("samples::s_{name}(3)")
    } else {
        format!("{target}::{name}(samples::s_{name}(3))")
    };
    // Structs store via `store_boxed`; the enum wrapper dispatches via
    // `store` (its arms call the variant's `store_boxed`).
    let store = if bare { "store_boxed" } else { "store" };
    out.push_str(&format!(
        "fn __rt_{name}() {{\n\
         \x20   samples::__stack(move || {{\n\
         \x20       let v = {sample};\n\
         \x20       let mut out = Vec::new();\n\
         \x20       v.{store}(&mut out);\n\
         \x20       let w = {target}::parse(&out).expect(\"{name} parse\");\n\
         \x20       assert_eq!(v, w);\n"
    ));
    if bare {
        out.push_str(&format!(
            "\x20       let mut out = Vec::new();\n\
             \x20       v.store(&mut out);\n\
             \x20       let w = {target}::parse_bare(&out).expect(\"{name} bare parse\");\n\
             \x20       assert_eq!(v, w);\n"
        ));
    }
    if json {
        out.push_str(&format!(
            "\x20       let j = v.to_json();\n\
             \x20       let w = {target}::from_json(&j).expect(\"{name} json parse\");\n\
             \x20       assert_eq!(v, w);\n"
        ));
    }
    out.push_str("    });\n}\n\n");
}

/// Derives shared by every generated type. Structs additionally derive
/// `Default` (their fields are plain values); enums get a manual `Default`
/// instead — the min-rank breaking ctor, so the recursive type graph
/// defaults terminate.
fn derives(opts: &WriterOpts) -> &'static str {
    if opts.serde {
        "#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]"
    } else {
        "#[derive(Clone, Debug, PartialEq)]"
    }
}

fn struct_derives(opts: &WriterOpts) -> &'static str {
    if opts.serde {
        "#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]"
    } else {
        "#[derive(Clone, Debug, Default, PartialEq)]"
    }
}

fn emit_struct(
    out: &mut String,
    p: &TlProgram,
    m: &SchemeModel,
    opts: &WriterOpts,
    c: usize,
    name: &str,
) {
    out.push_str(struct_derives(opts));
    out.push('\n');
    if opts.serde {
        out.push_str(&format!("#[serde(rename = \"{}\")]\n", p.ctors[c].id));
    }
    let fields = &m.ctor_fields[c];
    if fields.is_empty() {
        out.push_str(&format!("pub struct {name};\n\n"));
    } else {
        out.push_str(&format!("pub struct {name} {{\n"));
        for f in fields {
            out.push_str(&format!(
                "    pub {}: {},\n",
                field_ident(&f.name),
                field_decl_type(p, m, opts, &f.ty)
            ));
        }
        out.push_str("}\n\n");
    }

    // Binary store (S10): td's `Class::store(TlStorerUnsafe)` writes the
    // fields only (bare); the constructor id is written by the boxed
    // wrapper (`TlStoreBoxed<TlStoreObject>`). Mirror that exactly:
    // `store` = fields, `store_boxed` = id + fields.
    out.push_str(&format!("impl {name} {{\n"));
    out.push_str(&format!(
        "    pub const ID: i32 = {};\n\n",
        p.ctors[c].name as i32
    ));
    out.push_str("    pub fn store_boxed(&self, out: &mut Vec<u8>) {\n");
    out.push_str("        out.extend_from_slice(&Self::ID.to_le_bytes());\n");
    out.push_str("        self.store(out);\n");
    out.push_str("    }\n\n");
    out.push_str("    pub fn store(&self, out: &mut Vec<u8>) {\n");
    if fields.is_empty() {
        out.push_str("        let _ = out;\n");
    }
    StoreWriter {
        m,
        depth: 2,
        var_counter: 0,
    }
    .emit_fields(out, fields);
    out.push_str("    }\n\n");
    out.push_str("    pub fn parse(input: &[u8]) -> Result<Self, &'static str> {\n");
    out.push_str("        let mut r = tl::Reader::new(input);\n");
    out.push_str("        Self::read(&mut r)\n");
    out.push_str("    }\n\n");
    out.push_str("    pub fn parse_bare(input: &[u8]) -> Result<Self, &'static str> {\n");
    out.push_str("        let mut r = tl::Reader::new(input);\n");
    out.push_str("        Self::read_bare(&mut r)\n");
    out.push_str("    }\n\n");
    out.push_str("    pub fn read(r: &mut tl::Reader<'_>) -> Result<Self, &'static str> {\n");
    out.push_str("        if r.i32()? != Self::ID {\n");
    out.push_str("            return Result::Err(\"Wrong constructor\");\n");
    out.push_str("        }\n");
    out.push_str("        Self::read_bare(r)\n");
    out.push_str("    }\n\n");
    out.push_str("    pub fn read_bare(r: &mut tl::Reader<'_>) -> Result<Self, &'static str> {\n");
    ReadWriter {
        m,
        opts,
        depth: 2,
        var_counter: 0,
    }
    .emit_fields(out, p, fields);
    out.push_str("    }\n");
    if opts.json {
        emit_struct_json(out, p, m, opts, c, fields);
    }
    if opts.tostring {
        emit_struct_tostring(out, p, m, c, fields);
    }
    out.push_str("}\n\n");
}

/// Emits the S12 JSON methods for one constructor struct: `to_json_into`
/// (td's generated `to_json` + `json_encode` in one compact write) and the
/// `from_json` trio mirroring td's `from_json`/`tl_json.h` leniency.
///
/// Format rules (all matching td's `tl_json_converter.cpp` output):
/// `{"@type":"ctor","field":value,...}` with no spaces; the `flags:#` word
/// is never serialized; int64 fields are quoted decimal strings while int53
/// are plain numbers; doubles use 6 fixed decimals; bytes are standard
/// base64; strings escape non-ASCII as lowercase `\uXXXX` (surrogate pairs
/// above the BMP); vectors are arrays (a direct `vector<int64>` quotes its
/// elements, deeper int64s do not).
///
/// Nullability (S76): td's nullable `object_ptr` fields are `Option<T>` in
/// the model — the JSON key is omitted when `None` (td's
/// `if (object.x_) jo(...)`), while the binary store/parse materializes a
/// default object (td's generated classes default-construct members; the
/// wire never encodes null).
fn emit_struct_json(
    out: &mut String,
    p: &TlProgram,
    m: &SchemeModel,
    opts: &WriterOpts,
    c: usize,
    fields: &[Field],
) {
    out.push_str(
        "\n    pub fn to_json(&self) -> String {\n        let mut out = String::new();\n        self.to_json_into(&mut out);\n        out\n    }\n\n",
    );
    out.push_str("    pub fn to_json_into(&self, out: &mut String) {\n");
    out.push_str(&format!(
        "        out.push_str(\"{{\\\"@type\\\":\\\"{}\\\"\");\n",
        p.ctors[c].id
    ));
    JsonStoreWriter {
        m,
        depth: 2,
        var_counter: 0,
    }
    .emit_fields(out, fields);
    out.push_str("        out.push('}');\n    }\n");

    out.push_str(
        "\n    pub fn from_json(input: &str) -> Result<Self, String> {\n        \
         let root = tl_json::parse_json(input)?;\n        Self::from_json_value(&root)\n    }\n\n",
    );
    out.push_str(
        "    /// td's default-constructible `from_json(object_ptr<T>&)` path: any\n\
         \x20   /// object is accepted and `@type` is ignored — only the fields read.\n",
    );
    // Unit structs construct with `Self` (clippy: default_constructed_unit_structs).
    let null_default = if fields.is_empty() {
        "Self"
    } else {
        "Self::default()"
    };
    out.push_str(&format!(
        "    pub fn from_json_value(v: &tl_json::Json) -> Result<Self, String> {{\n        \
         match v {{\n            \
         tl_json::Json::Null => Result::Ok({null_default}),\n            \
         tl_json::Json::Object(o) => Self::from_json_object(o),\n            \
         other => Result::Err(format!(\"Expected Object, but receive {{}}\", other.type_name())),\n        \
         }}\n    }}\n\n",
    ));
    if fields.is_empty() {
        out.push_str(
            "    fn from_json_object(_o: &tl_json::Object) -> Result<Self, String> {\n        \
             Result::Ok(Self)\n    }\n",
        );
        return;
    }
    out.push_str(
        "    fn from_json_object(o: &tl_json::Object) -> Result<Self, String> {\n        \
         let mut value = Self::default();\n",
    );
    JsonLoadWriter {
        m,
        opts,
        depth: 2,
        var_counter: 0,
    }
    .emit_fields(out, p, fields);
    out.push_str("        Result::Ok(value)\n    }\n");
}

/// Emits the `Object` umbrella (S16): one variant per non-builtin
/// constructor — td's `object_ptr<Object>` JSON surface. `to_json_into` is
/// td's generated `to_json(const Object&)` switch; `from_json` mirrors
/// `tl_constructor_from_string(Object)` + downcast (name or numeric id).
/// No serde derives: this is the dynamic layer — `to_json`/`from_json` are
/// its only conversions (serde's internally-tagged derive over ~2200
/// variants would cost far more compile time than the whole module).
fn emit_object_umbrella(out: &mut String, p: &TlProgram, m: &SchemeModel, opts: &WriterOpts) {
    let mut variants: Vec<(String, String, usize)> = Vec::new(); // (variant, struct, ctor)
    for &tid in p.type_ids.values() {
        match &m.type_kinds[tid] {
            TypeKind::Struct { ctor } => {
                variants.push((capitalize(&p.ctors[*ctor].id), m.type_name(p, tid), *ctor));
            }
            TypeKind::Enum { ctors } if !ctors.is_empty() => {
                for &c in ctors {
                    let name = capitalize(&p.ctors[c].id);
                    variants.push((name.clone(), name, c));
                }
            }
            _ => {}
        }
    }
    let mut seen = BTreeSet::new();
    for (variant, _, _) in &variants {
        assert!(
            seen.insert(variant.clone()),
            "duplicate Object umbrella variant {variant}"
        );
    }

    out.push_str("#[derive(Clone, Debug, PartialEq)]\n");
    out.push_str("pub enum Object {\n");
    for (variant, struct_name, _) in &variants {
        out.push_str(&format!("    {variant}({struct_name}),\n"));
    }
    out.push_str("}\n\n");
    out.push_str("impl Object {\n");
    out.push_str(
        "\n    pub fn to_json(&self) -> String {\n        let mut out = String::new();\n        self.to_json_into(&mut out);\n        out\n    }\n\n",
    );
    out.push_str("    pub fn to_json_into(&self, out: &mut String) {\n        match self {\n");
    for (variant, _, _) in &variants {
        out.push_str(&format!(
            "            Self::{variant}(v) => v.to_json_into(out),\n"
        ));
    }
    out.push_str("        }\n    }\n");
    out.push_str(
        "\n    /// td's Object-level request surface: an object's `@type`\n\
         \x20   /// (name or numeric id) selects the constructor. Null has no\n\
         \x20   /// Object representation (td's null maps to a null pointer).\n",
    );
    out.push_str(
        "    pub fn from_json(input: &str) -> Result<Self, String> {\n        \
         let root = tl_json::parse_json(input)?;\n        Self::from_json_value(&root)\n    }\n\n",
    );
    // Per-variant constructor helpers (see emit_enum_json: the dispatch
    // match arms must be plain calls — inline construction over ~2200
    // variants measures a 141 MB debug stack frame).
    for (variant, struct_name, _) in &variants {
        out.push_str(&format!(
            "    fn __json_new_{variant}(o: &tl_json::Object) -> Result<Self, String> {{\n        \
             Result::Ok(Self::{variant}(self::{struct_name}::from_json_object(o)?))\n    }}\n"
        ));
    }
    out.push_str(
        "    pub fn from_json_value(v: &tl_json::Json) -> Result<Self, String> {\n        \
         let o = match v {\n            \
         tl_json::Json::Object(o) => o,\n            \
         other => return Result::Err(format!(\"Expected Object, but receive {}\", other.type_name())),\n        \
         };\n        \
         let ty = match tl_json::field(o, \"@type\") {\n            \
         Some(ty) => ty,\n            \
         None => return Result::Err(\"Can't find field \\\"@type\\\"\".to_owned()),\n        \
         };\n        \
         match ty {\n            \
         tl_json::Json::String(s) => match s.as_str() {\n",
    );
    for (variant, _, c) in &variants {
        out.push_str(&format!(
            "                \"{}\" => Self::__json_new_{variant}(o),\n",
            p.ctors[*c].id
        ));
    }
    out.push_str(
        "                _ => Result::Err(format!(\"Unknown class \\\"{s}\\\"\")),\n\
         \x20           },\n            \
         tl_json::Json::Number(n) => match tl_json::to_integer_i32(n) {\n",
    );
    let mut seen_ids = BTreeSet::new();
    for (variant, struct_name, c) in &variants {
        if !seen_ids.insert(p.ctors[*c].name) {
            continue;
        }
        out.push_str(&format!(
            "                self::{struct_name}::ID => Self::__json_new_{variant}(o),\n"
        ));
    }
    out.push_str(
        "                id => Result::Err(format!(\"Unknown constructor 0x{:08x}\", id as u32)),\n\
         \x20           },\n            \
         other => Result::Err(format!(\"Expected String or Integer, but receive {}\", other.type_name())),\n        \
         }\n    }\n",
    );
    if opts.tostring {
        // td's `to_string(const BaseObject&)`: the virtual `store` dispatch.
        out.push_str(
            "\n    /// td's `to_string(const BaseObject&)` for the dynamic object.\n    \
             pub fn to_debug_string(&self) -> String {\n        \
             let mut out = String::new();\n        \
             let mut storer = TlStorerToString { out: &mut out, shift: 0 };\n        \
             self.store_debug(&mut storer, \"\");\n        \
             out\n    }\n\n    \
             pub fn store_debug(&self, s: &mut TlStorerToString, field_name: &str) {\n        \
             match self {\n",
        );
        for (variant, _, _) in &variants {
            out.push_str(&format!(
                "            Self::{variant}(v) => v.store_debug(s, field_name),\n"
            ));
        }
        out.push_str("        }\n    }\n");
    }
    out.push_str("}\n\n");
    // `Update::try_from_object` — the update-typed view C++ carries through
    // `td_api::updates` and `Td::send_update` (`object_ptr<Update>`); the
    // rewrap is variant-for-variant over the `Update` type's constructors.
    if let Some(&update_ctor) = p
        .type_ids
        .values()
        .find(|&&tid| class_name(&p.types[tid].id) == "Update")
    {
        let update_variants: Vec<String> = match &m.type_kinds[update_ctor] {
            TypeKind::Struct { ctor } => vec![capitalize(&p.ctors[*ctor].id)],
            TypeKind::Enum { ctors } => ctors.iter().map(|&c| capitalize(&p.ctors[c].id)).collect(),
            _ => Vec::new(),
        };
        out.push_str("impl Update {\n");
        out.push_str(
            "    #[allow(clippy::result_large_err)] // the Err carries the rejected Object, like the C++ ptr\n",
        );
        out.push_str(
            "    pub fn try_from_object(value: Object) -> Result<Update, Object> {\n        match value {\n",
        );
        for variant in update_variants {
            out.push_str(&format!(
                "            Object::{variant}(v) => Result::Ok(Update::{variant}(v)),\n"
            ));
        }
        out.push_str("            other => Result::Err(other),\n        }\n    }\n}\n\n");
    }
    let _ = opts;
}

/// Emits the S12 JSON methods for one enum: to_json dispatches to the
/// variant's `to_json_into` (td's `downcast_call`); from_json extracts
/// `@type` — a constructor name string or a numeric id (td's
/// `tl_constructor_from_string` + `TlDowncastHelper`) — and dispatches to
/// the variant's field reader. First declaration wins on duplicate wire ids
/// (same rule as the binary `read`).
fn emit_enum_json(out: &mut String, p: &TlProgram, ctors: &[usize]) {
    out.push_str(
        "\n    pub fn to_json(&self) -> String {\n        let mut out = String::new();\n        self.to_json_into(&mut out);\n        out\n    }\n\n",
    );
    out.push_str("    pub fn to_json_into(&self, out: &mut String) {\n        match self {\n");
    for &c in ctors {
        let variant = capitalize(&p.ctors[c].id);
        out.push_str(&format!(
            "            Self::{variant}(v) => v.to_json_into(out),\n"
        ));
    }
    out.push_str("        }\n    }\n");
    out.push_str(
        "\n    pub fn from_json(input: &str) -> Result<Self, String> {\n        \
         let root = tl_json::parse_json(input)?;\n        Self::from_json_value(&root)\n    }\n\n",
    );
    // Per-variant constructor helpers: each arm of the dispatch match must
    // be a plain call. Inline `Self::V(self::V::from_json_object(o)?)`
    // arms give the dispatch fn one unmerged temporary per variant — fine
    // for small enums, but the ~2200-variant Object umbrella and the
    // 1000+ function umbrella reach multi-megabyte (measured: 141 MB)
    // debug stack frames that overflow any thread.
    for &c in ctors {
        let variant = capitalize(&p.ctors[c].id);
        out.push_str(&format!(
            "    fn __json_new_{variant}(o: &tl_json::Object) -> Result<Self, String> {{\n        \
             Result::Ok(Self::{variant}(self::{variant}::from_json_object(o)?))\n    }}\n"
        ));
    }
    out.push_str(
        "    /// td's `from_json(object_ptr<T>&, JsonValue)`: null yields the\n\
         \x20   /// default variant (C++ keeps a null pointer — see the model note),\n\
         \x20   /// an object's `@type` (name or numeric id) selects the variant.\n",
    );
    out.push_str(
        "    pub fn from_json_value(v: &tl_json::Json) -> Result<Self, String> {\n        \
         let o = match v {\n            \
         tl_json::Json::Null => return Result::Ok(Self::default()),\n            \
         tl_json::Json::Object(o) => o,\n            \
         other => return Result::Err(format!(\"Expected Object, but receive {}\", other.type_name())),\n        \
         };\n        \
         let ty = match tl_json::field(o, \"@type\") {\n            \
         Some(ty) => ty,\n            \
         None => return Result::Err(\"Can't find field \\\"@type\\\"\".to_owned()),\n        \
         };\n        \
         match ty {\n            \
         tl_json::Json::String(s) => match s.as_str() {\n",
    );
    for &c in ctors {
        let variant = capitalize(&p.ctors[c].id);
        out.push_str(&format!(
            "                \"{}\" => Self::__json_new_{variant}(o),\n",
            p.ctors[c].id
        ));
    }
    out.push_str(
        "                _ => Result::Err(format!(\"Unknown class \\\"{s}\\\"\")),\n\
         \x20           },\n            \
         tl_json::Json::Number(n) => match tl_json::to_integer_i32(n) {\n",
    );
    let mut seen = BTreeSet::new();
    for &c in ctors {
        if !seen.insert(p.ctors[c].name) {
            continue;
        }
        let variant = capitalize(&p.ctors[c].id);
        out.push_str(&format!(
            "                self::{variant}::ID => Self::__json_new_{variant}(o),\n"
        ));
    }
    out.push_str(
        "                id => Result::Err(format!(\"Unknown constructor 0x{:08x}\", id as u32)),\n\
         \x20           },\n            \
         other => Result::Err(format!(\"Expected String or Integer, but receive {}\", other.type_name())),\n        \
         }\n    }\n",
    );
}

/// Emits the to_json field statements — each is `,"name":` followed by the
/// value in td's exact encoding. The `flags:#` word is never serialized;
/// conditional fields are emitted unconditionally (td's writer gates only
/// object null-ness, never the flag bit).
struct JsonStoreWriter<'a> {
    m: &'a SchemeModel,
    depth: usize,
    var_counter: usize,
}

impl JsonStoreWriter<'_> {
    fn indent(&self) -> String {
        " ".repeat(self.depth * 4)
    }

    fn emit_fields(&mut self, out: &mut String, fields: &[Field]) {
        for f in fields {
            if let FieldType::Prim(Prim::Nat) = f.ty {
                // The flags word has no JSON representation (td's tl_simple
                // drops it from the arg list entirely).
                continue;
            }
            if is_object_field(self.m, &f.ty) {
                // td's nullable `object_ptr` fields: the key (and value) is
                // omitted entirely when null — `if (object.x_) jo(...)`.
                let ind = self.indent();
                let var = format!("__o{}", self.var_counter);
                self.var_counter += 1;
                let field = field_ident(&f.name);
                out.push_str(&format!("{ind}if let Some({var}) = &self.{field} {{\n"));
                self.depth += 1;
                out.push_str(&format!(
                    "{}out.push_str(\",{}:\");\n",
                    self.indent(),
                    json_key(&f.name)
                ));
                out.push_str(&format!("{}{var}.to_json_into(out);\n", self.indent()));
                self.depth -= 1;
                out.push_str(&format!("{ind}}}\n"));
                continue;
            }
            out.push_str(&format!(
                "{}out.push_str(\",{}:\");\n",
                self.indent(),
                json_key(&f.name)
            ));
            let expr = format!("self.{}", field_ident(&f.name));
            self.emit_value(out, &expr, &f.ty);
        }
    }

    /// Emits the statements writing `expr` (a place expression) as JSON.
    fn emit_value(&mut self, out: &mut String, expr: &str, ty: &FieldType) {
        let ind = self.indent();
        let call = |e: &str| {
            if e.starts_with('*') {
                format!("({e})")
            } else {
                e.to_owned()
            }
        };
        match ty {
            FieldType::Prim(prim) => {
                // Strings/bytes take &str/&[u8]; references auto-deref at
                // the call, so vector elements pass the reference itself
                // and fields pass `&self.x`.
                let refe = |e: &str| match e.strip_prefix('*') {
                    Some(stripped) => stripped.to_owned(),
                    None => format!("&{e}"),
                };
                let (f, arg) = match prim {
                    Prim::Nat | Prim::Int32 => ("num32", expr.to_owned()),
                    Prim::Int53 => ("num64", expr.to_owned()),
                    // td's JsonInt64: quoted decimal string.
                    Prim::Int64 => ("num64q", expr.to_owned()),
                    Prim::Double => ("double", expr.to_owned()),
                    Prim::Bool | Prim::True => ("boolean", expr.to_owned()),
                    Prim::String => ("str", refe(expr)),
                    Prim::Bytes => ("bytes", refe(expr)),
                };
                out.push_str(&format!("{ind}tl_json::{f}(out, {arg});\n"));
            }
            FieldType::Vector { inner, .. } => {
                // Note: a *direct* vector<int64> quotes its elements only
                // via the innermost Int64 prim arm — exactly td's
                // JsonVectorInt64 rule (deeper int64s stay plain numbers).
                out.push_str(&format!("{ind}out.push('[');\n"));
                let var = format!("__v{}", self.var_counter);
                self.var_counter += 1;
                // `&self.f` iterates fields; `*var` element places (nested
                // vectors) iterate the reference itself.
                let iter = match expr.strip_prefix('*') {
                    Some(stripped) => stripped.to_owned(),
                    None => format!("&{expr}"),
                };
                out.push_str(&format!(
                    "{ind}let mut __first = true;\n{ind}for {var} in {iter} {{\n"
                ));
                self.depth += 1;
                out.push_str(&format!(
                    "{}if !__first {{\n{}    out.push(',');\n{}}}\n{}__first = false;\n",
                    self.indent(),
                    self.indent(),
                    self.indent(),
                    self.indent()
                ));
                self.emit_value(out, &format!("*{var}"), inner);
                self.depth -= 1;
                out.push_str(&format!("{ind}}}\n{ind}out.push(']');\n"));
            }
            FieldType::FixedArray { .. } => {
                // td_api has no fixed arrays; the wire schemes have no JSON.
                unreachable!("fixed arrays have no JSON form");
            }
            FieldType::Conditional { inner, .. } => {
                // Emitted unconditionally — see emit_fields.
                self.emit_value(out, expr, inner);
            }
            FieldType::Boxed(_) | FieldType::Bare(_) => {
                out.push_str(&format!("{ind}{}.to_json_into(out);\n", call(expr)));
            }
            FieldType::Function => {
                out.push_str(&format!("{ind}{}.to_json_into(out);\n", call(expr)));
            }
        }
    }
}

/// A JSON object key: TL field names are already td's wire names, quoted
/// verbatim (td's `jo(arg.name, …)`).
fn json_key(name: &str) -> String {
    format!("\\\"{name}\\\"")
}

/// The `tl_json::read_*` function for a (possibly flag-gated) primitive —
/// the readers that can be passed as function items into `read_vector`.
fn prim_reader_fn(ty: &FieldType) -> Option<&'static str> {
    match ty {
        FieldType::Conditional { inner, .. } => prim_reader_fn(inner),
        FieldType::Prim(prim) => Some(match prim {
            Prim::Nat | Prim::Int32 => "read_i32",
            Prim::Int53 | Prim::Int64 => "read_i64",
            Prim::Double => "read_f64",
            Prim::Bool | Prim::True => "read_bool",
            Prim::String => "read_string",
            Prim::Bytes => "read_bytes",
        }),
        _ => None,
    }
}

/// Emits the from_json field statements: `tl_json::field` is td's
/// `extract_field` (first match wins, missing → keep the default).
struct JsonLoadWriter<'a> {
    m: &'a SchemeModel,
    opts: &'a WriterOpts,
    depth: usize,
    var_counter: usize,
}

impl JsonLoadWriter<'_> {
    fn indent(&self) -> String {
        " ".repeat(self.depth * 4)
    }

    fn emit_fields(&mut self, out: &mut String, p: &TlProgram, fields: &[Field]) {
        for f in fields {
            if let FieldType::Prim(Prim::Nat) = f.ty {
                continue;
            }
            let ind = self.indent();
            let target = format!("value.{}", field_ident(&f.name));
            let var = format!("__j{}", self.var_counter);
            self.var_counter += 1;
            let elem_var = format!("__e{}", self.var_counter);
            self.var_counter += 1;
            // Top-level expressions already carry their own `?` (boxing
            // consumes it inside `Box::new(..?)`). Class fields wrap in
            // `Some` — an explicit JSON `null` stays `None`, td's
            // object_ptr-from-null rule. The null check rides the `if let`
            // condition (a nested `if` trips `clippy::collapsible_if` on
            // the generated code).
            let object = is_object_field(self.m, &f.ty);
            let null_guard = if object {
                format!(" && !matches!({var}, tl_json::Json::Null)")
            } else {
                String::new()
            };
            out.push_str(&format!(
                "{ind}if let Some({var}) = tl_json::field(o, \"{}\"){null_guard} {{\n",
                f.name
            ));
            self.depth += 1;
            let expr = self.read_expr(p, &var, &elem_var, &f.ty, false);
            let expr = if object {
                format!("Some({expr})")
            } else {
                expr
            };
            out.push_str(&format!("{}{target} = {expr};\n", self.indent()));
            self.depth -= 1;
            out.push_str(&format!("{ind}}}\n"));
        }
    }

    /// The expression converting a `&Json` named `var` into a field value;
    /// `elem` names the closure parameter of the enclosing vector level.
    /// At the top level the expression carries its own `?`; inside a vector
    /// closure it is the closure's tail (a plain `Result`), so boxing
    /// re-wraps into `Result::Ok(..?..)`.
    fn read_expr(
        &mut self,
        p: &TlProgram,
        var: &str,
        elem: &str,
        ty: &FieldType,
        closure: bool,
    ) -> String {
        // Result-returning forms get their `?` only at the top level.
        let q = if closure { "" } else { "?" };
        match ty {
            FieldType::Prim(prim) => format!(
                "tl_json::read_{}({var}){q}",
                match prim {
                    Prim::Nat | Prim::Int32 => "i32",
                    Prim::Int53 | Prim::Int64 => "i64",
                    Prim::Double => "f64",
                    Prim::Bool | Prim::True => "bool",
                    Prim::String => "string",
                    Prim::Bytes => "bytes",
                }
            ),
            FieldType::FixedArray { .. } => {
                // td_api has no fixed arrays; the wire schemes have no JSON.
                unreachable!("fixed arrays have no JSON form");
            }
            FieldType::Vector { inner, .. } => {
                // Single-function readers pass the function item itself —
                // a `|e| f(e)` closure trips clippy's redundant_closure in
                // the generated code. Boxed readers keep the closure (their
                // body re-wraps the `?`).
                if let Some(f) = prim_reader_fn(inner) {
                    return format!("tl_json::read_vector({var}, tl_json::{f}){q}");
                }
                if let FieldType::Boxed(t) | FieldType::Bare(t) = &**inner {
                    let name = self.m.type_name(p, *t);
                    if !self.m.cyclic[*t] {
                        return format!("tl_json::read_vector({var}, {name}::from_json_value){q}");
                    }
                }
                let inner_elem = format!("__e{}", self.var_counter);
                self.var_counter += 1;
                let inner_expr = self.read_expr(p, elem, &inner_elem, inner, true);
                format!("tl_json::read_vector({var}, |{elem}| {inner_expr}){q}")
            }
            FieldType::Conditional { inner, .. } => self.read_expr(p, var, elem, inner, closure),
            FieldType::Boxed(t) | FieldType::Bare(t) => {
                let name = self.m.type_name(p, *t);
                if self.m.cyclic[*t] {
                    if closure {
                        format!("Result::Ok(Box::new({name}::from_json_value({var})?))")
                    } else {
                        format!("Box::new({name}::from_json_value({var})?)")
                    }
                } else {
                    format!("{name}::from_json_value({var}){q}")
                }
            }
            FieldType::Function => {
                let name = self.opts.umbrella;
                if closure {
                    format!("Result::Ok(Box::new({name}::from_json_value({var})?))")
                } else {
                    format!("Box::new({name}::from_json_value({var})?)")
                }
            }
        }
    }
}

fn emit_enum(
    out: &mut String,
    p: &TlProgram,
    opts: &WriterOpts,
    ctors: &[usize],
    name: &str,
    default_ctor: usize,
) {
    out.push_str(derives(opts));
    out.push('\n');
    if opts.serde {
        out.push_str("#[serde(tag = \"@type\")]\n");
    }
    out.push_str(&format!("pub enum {name} {{\n"));
    for &c in ctors {
        let variant = capitalize(&p.ctors[c].id);
        if opts.serde {
            out.push_str(&format!("    #[serde(rename = \"{}\")]\n", p.ctors[c].id));
        }
        out.push_str(&format!("    {variant}({variant}),\n"));
    }
    out.push_str("}\n\n");
    out.push_str(&format!("impl {name} {{\n"));
    out.push_str("    pub fn store(&self, out: &mut Vec<u8>) {\n");
    out.push_str("        match self {\n");
    for &c in ctors {
        let variant = capitalize(&p.ctors[c].id);
        out.push_str(&format!(
            "            Self::{variant}(v) => v.store_boxed(out),\n"
        ));
    }
    out.push_str("        }\n    }\n\n");
    // S11: boxed parse — read the constructor id, dispatch to the variant
    // struct's `read_bare` (td's generated `T::fetch` switch). The variant
    // names shadow their same-named structs inside this impl, so the structs
    // are addressed through the module (`self::`) path.
    out.push_str("    pub fn parse(input: &[u8]) -> Result<Self, &'static str> {\n");
    out.push_str("        let mut r = tl::Reader::new(input);\n");
    out.push_str("        Self::read(&mut r)\n");
    out.push_str("    }\n\n");
    out.push_str("    pub fn read(r: &mut tl::Reader<'_>) -> Result<Self, &'static str> {\n");
    out.push_str("        match r.i32()? {\n");
    // Duplicate constructor ids (the invokeWith*Prefix aliases share their
    // wire id with the non-prefix ctor): the first declaration wins — the
    // wire cannot distinguish them.
    let mut seen = BTreeSet::new();
    for &c in ctors {
        if !seen.insert(p.ctors[c].name) {
            continue;
        }
        let variant = capitalize(&p.ctors[c].id);
        out.push_str(&format!(
            "            self::{variant}::ID => Result::Ok(Self::{variant}(self::{variant}::read_bare(r)?)),\n"
        ));
    }
    out.push_str("            _ => Result::Err(\"Unknown constructor\"),\n");
    out.push_str("        }\n    }\n");
    if opts.json {
        emit_enum_json(out, p, ctors);
    }
    if opts.tostring {
        emit_enum_tostring(out, p, ctors);
    }
    out.push_str("}\n\n");
    // Manual Default: the breaking (min-rank) ctor — derived Default would
    // recurse forever on cyclic types, and `#[default]` only works on unit
    // variants. The rank argument makes the whole Default graph acyclic.
    let default_variant = capitalize(&p.ctors[default_ctor].id);
    out.push_str(&format!("impl Default for {name} {{\n"));
    out.push_str(&format!(
        "    fn default() -> Self {{\n        Self::{default_variant}(Default::default())\n    }}\n}}\n\n"
    ));
    // `From<{name}> for Object` — every td_api type participates in the
    // dynamic `Object` umbrella; the rewrap is variant-for-variant. The
    // Function umbrella (opts.umbrella) is not an Object.
    if opts.json && name != opts.umbrella {
        out.push_str(&format!("impl From<{name}> for Object {{\n"));
        out.push_str("    fn from(value: ");
        out.push_str(name);
        out.push_str(") -> Object {\n        match value {\n");
        for &c in ctors {
            let variant = capitalize(&p.ctors[c].id);
            out.push_str(&format!(
                "            {name}::{variant}(v) => Object::{variant}(v),\n"
            ));
        }
        out.push_str("        }\n    }\n}\n\n");
    }
}

/// The declared struct-field type: class fields are `Option<...>`
/// (`None` = td's null `object_ptr`, omitted from the JSON output);
/// everything else (primitives, vectors — including vectors of classes)
/// stays a plain value, exactly the fields td's JSON emitter writes
/// unconditionally. Flag-gated class fields are nullable the same way.
fn field_decl_type(p: &TlProgram, m: &SchemeModel, opts: &WriterOpts, ty: &FieldType) -> String {
    let base = rust_field_type(p, m, opts, ty);
    if is_object_field(m, ty) {
        format!("Option<{base}>")
    } else {
        base
    }
}

/// Whether the type is a scheme-class reference — every such field is a
/// nullable `object_ptr<T>` in td's C++ model (null when the JSON key is
/// absent; the binary store materializes a default object, exactly like
/// td's generated classes default-construct members).
fn is_class(m: &SchemeModel, ty: &FieldType) -> bool {
    match ty {
        // The fixed word types (int128/int256/int512) are embedded values
        // in td's C++ model — never nullable object_ptr.
        FieldType::Boxed(t) | FieldType::Bare(t) => !m.is_word_type[*t],
        FieldType::Function => true,
        _ => false,
    }
}

/// Whether a field is a (possibly flag-gated) scheme-class reference — the
/// JSON-omittable `object_ptr` set.
fn is_object_field(m: &SchemeModel, ty: &FieldType) -> bool {
    match ty {
        FieldType::Conditional { inner, .. } => is_object_field(m, inner),
        t => is_class(m, t),
    }
}

fn rust_field_type(p: &TlProgram, m: &SchemeModel, opts: &WriterOpts, ty: &FieldType) -> String {
    match ty {
        FieldType::Prim(prim) => {
            if opts.raw_strings && *prim == Prim::String {
                "Vec<u8>".to_owned()
            } else {
                prim.rust_type().to_owned()
            }
        }
        FieldType::Vector { inner, .. } => format!("Vec<{}>", rust_field_type(p, m, opts, inner)),
        FieldType::FixedArray { len, inner } => {
            format!("[{}; {len}]", rust_field_type(p, m, opts, inner))
        }
        FieldType::Conditional { inner, .. } => rust_field_type(p, m, opts, inner),
        // The umbrella is self-referential (invokeWithLayer holds a
        // Function), so it always sits behind a Box — td's BaseObject ptr.
        FieldType::Function => format!("Box<{}>", opts.umbrella),
        FieldType::Boxed(t) | FieldType::Bare(t) => {
            let name = m.type_name(p, *t);
            if m.cyclic[*t] {
                format!("Box<{name}>")
            } else {
                name
            }
        }
    }
}

/// Emits the `store` (bare) field statements, mirroring the C++
/// `gen_field_store` sequence (including the `var0 = flags_ | …` capture).
struct StoreWriter<'a> {
    m: &'a SchemeModel,
    depth: usize,
    var_counter: usize,
}

impl StoreWriter<'_> {
    fn indent(&self) -> String {
        " ".repeat(self.depth * 4)
    }

    fn emit_fields(&mut self, out: &mut String, fields: &[Field]) {
        // The C++ store captures each `#` field as `var0 = flags_ | (b << n)`
        // (auto-OR-ing every ?true bool) and gates other conditionals on it.
        let mut captures: Vec<(usize, String)> = Vec::new(); // field idx → expr
        for (i, f) in fields.iter().enumerate() {
            if let FieldType::Prim(Prim::Nat) = f.ty {
                let mut expr = format!("self.{}", field_ident(&f.name));
                for g in fields {
                    if let FieldType::Conditional {
                        flags_field,
                        bit,
                        inner,
                    } = &g.ty
                        && *flags_field == i
                        && **inner == FieldType::Prim(Prim::True)
                    {
                        // `<< 0` is the identity — drop the shift at bit 0
                        // (and one paren level with it).
                        let term = if *bit == 0 {
                            format!("(self.{} as i32)", field_ident(&g.name))
                        } else {
                            format!("((self.{} as i32) << {bit})", field_ident(&g.name))
                        };
                        expr.push_str(&format!(" | {term}"));
                    }
                }
                captures.push((i, expr));
            }
        }
        for (i, f) in fields.iter().enumerate() {
            let expr = format!("self.{}", field_ident(&f.name));
            if let FieldType::Conditional {
                flags_field,
                bit,
                inner,
            } = &f.ty
            {
                if **inner == FieldType::Prim(Prim::True) {
                    // ?true is folded into the stored flags word.
                    continue;
                }
                let cap = captures
                    .iter()
                    .find(|(idx, _)| *idx == *flags_field)
                    .map(|(_, e)| e.clone())
                    .unwrap_or_else(|| format!("self.{}", field_ident(&fields[*flags_field].name)));
                out.push_str(&format!(
                    // Parenthesized: `cap` is an OR-chain (`self.flags |
                    // (bool << bit) | …`), and without the parens `&`
                    // binds per-operator-precedence into the last term —
                    // `(flags | bools) != 0` — storing every conditional
                    // field whenever any bit is set.
                    "{}if ({}) & {} != 0 {{\n",
                    self.indent(),
                    cap,
                    bit_mask(*bit)
                ));
                self.depth += 1;
                let expr = self.materialize_class(out, &expr, inner);
                self.emit_value(out, &expr, inner);
                self.depth -= 1;
                out.push_str(&format!("{}}}\n", self.indent()));
            } else if let FieldType::Prim(Prim::Nat) = f.ty {
                // The captured (auto-OR'd) flags word, or the plain value.
                let cap = captures
                    .iter()
                    .find(|(idx, _)| *idx == i)
                    .map(|(_, e)| e.clone());
                out.push_str(&format!(
                    "{}tl::__i32(out, {});\n",
                    self.indent(),
                    cap.as_deref().unwrap_or(&expr)
                ));
            } else {
                let expr = self.materialize_class(out, &expr, &f.ty);
                self.emit_value(out, &expr, &f.ty);
            }
        }
    }

    /// Emits the statements storing `expr` (a place expression) as `ty`.
    /// Deref place expressions (`*__v0`) are parenthesized only where a
    /// method is called on them, so primitives stay `tl::__i32(out, *__v0)`.
    fn emit_value(&mut self, out: &mut String, expr: &str, ty: &FieldType) {
        let ind = self.indent();
        let call = |e: &str| {
            if e.starts_with('*') {
                format!("({e})")
            } else {
                e.to_owned()
            }
        };
        match ty {
            FieldType::Prim(prim) => {
                let f = match prim {
                    Prim::Nat | Prim::Int32 => "__i32",
                    Prim::Int53 | Prim::Int64 => "__i64",
                    Prim::Double => "__f64",
                    Prim::Bool => "__bool",
                    Prim::String | Prim::Bytes => "__string",
                    Prim::True => unreachable!("?true never stored directly"),
                };
                // String/bytes pass by reference (the helper is generic);
                // vector elements arrive as `*var` places, where `&**var`
                // is the deref clippy accepts (`&*var` is a flagged
                // reborrow).
                let arg = if matches!(prim, Prim::String | Prim::Bytes) {
                    if expr.starts_with('*') {
                        format!("&*{expr}")
                    } else {
                        format!("&{expr}")
                    }
                } else {
                    expr.to_owned()
                };
                out.push_str(&format!("{ind}tl::{f}(out, {arg});\n"));
            }
            FieldType::Vector { boxed, inner } => {
                // Capitalized `Vector<T>` carries the vector constructor id
                // (td's `TlStoreBoxed<TlStoreVector, 481674261>`); the
                // lower-case bare form stores just the count.
                if *boxed {
                    out.push_str(&format!("{ind}tl::__i32(out, tl::VECTOR_ID);\n"));
                }
                // Unique loop var per vector (nesting AND siblings).
                let n = self.var_counter;
                self.var_counter += 1;
                let var = format!("__v{n}");
                out.push_str(&format!(
                    "{ind}tl::__i32(out, {}.len() as i32);\n",
                    call(expr)
                ));
                // Same deref-vs-reborrow rule as the string arm above.
                let iter = if expr.starts_with('*') {
                    format!("&*{expr}")
                } else {
                    format!("&{expr}")
                };
                out.push_str(&format!("{ind}for {var} in {iter} {{\n"));
                self.depth += 1;
                self.emit_value(out, &format!("*{var}"), inner);
                self.depth -= 1;
                out.push_str(&format!("{ind}}}\n"));
            }
            FieldType::FixedArray { inner, .. } => {
                // Fixed `N*[T]` (int128/256/512 words): exactly N elements,
                // no count word — td's std::array<T, N>.
                let n = self.var_counter;
                self.var_counter += 1;
                let var = format!("__v{n}");
                let iter = if expr.starts_with('*') {
                    format!("&*{expr}")
                } else {
                    format!("&{expr}")
                };
                out.push_str(&format!("{ind}for {var} in {iter} {{\n"));
                self.depth += 1;
                self.emit_value(out, &format!("*{var}"), inner);
                self.depth -= 1;
                out.push_str(&format!("{ind}}}\n"));
            }
            FieldType::Boxed(t) => {
                // Boxed single-ctor (struct) targets store the id via
                // `store_boxed`; multi-ctor (enum) targets — and the
                // umbrella — dispatch through the enum's `store`, whose
                // variant arms call `store_boxed` themselves.
                if matches!(self.m.type_kinds[*t], TypeKind::Struct { .. }) {
                    out.push_str(&format!("{ind}{}.store_boxed(out);\n", call(expr)));
                } else {
                    out.push_str(&format!("{ind}{}.store(out);\n", call(expr)));
                }
            }
            FieldType::Function => {
                out.push_str(&format!("{ind}{}.store(out);\n", call(expr)));
            }
            FieldType::Bare(t) => {
                assert!(
                    matches!(self.m.type_kinds[*t], TypeKind::Struct { .. }),
                    "bare reference to multi-ctor type"
                );
                out.push_str(&format!("{ind}{}.store(out);\n", call(expr)));
            }
            FieldType::Conditional { .. } => unreachable!("conditionals are handled per-field"),
        }
    }

    /// A class field's `Option<T>` materializes for the binary store (td's
    /// object_ptr members default-construct; the wire never encodes null):
    /// binds `expr` to a `&T` place (default when `None`) and returns the
    /// local's name for [`Self::emit_value`].
    fn materialize_class(&mut self, out: &mut String, expr: &str, ty: &FieldType) -> String {
        if !is_object_field(self.m, ty) {
            return expr.to_owned();
        }
        let ind = self.indent();
        let n = self.var_counter;
        self.var_counter += 1;
        let (default, local) = (format!("__d{n}"), format!("__m{n}"));
        out.push_str(&format!(
            "{ind}let {default} = Default::default();\n{ind}let {local} = {expr}.as_ref().unwrap_or(&{default});\n"
        ));
        local
    }
}

/// Emits a `read_bare` body, mirroring td's generated ctor-from-parser:
/// fields are read into locals in declaration order; conditional fields are
/// gated on the (already-read) flags local; `?true` bools are derived from
/// the flags bits; cleared conditionals default-construct (exactly like the
/// C++ classes, whose members stay default when not read).
struct ReadWriter<'a> {
    m: &'a SchemeModel,
    opts: &'a WriterOpts,
    depth: usize,
    var_counter: usize,
}

impl ReadWriter<'_> {
    fn indent(&self) -> String {
        " ".repeat(self.depth * 4)
    }

    fn emit_fields(&mut self, out: &mut String, p: &TlProgram, fields: &[Field]) {
        if fields.is_empty() {
            out.push_str("        let _ = r;\n        Result::Ok(Self)\n");
            return;
        }
        for f in fields {
            let ind = self.indent();
            let local = field_ident(&f.name);
            match &f.ty {
                FieldType::Prim(Prim::Nat) => {
                    // td: `if ((var0 = flags_ = TlFetchInt::parse(p)) < 0)
                    // FAIL("Variable of type # can't be negative")`.
                    out.push_str(&format!("{ind}let {local}: i32 = r.i32()?;\n"));
                    out.push_str(&format!("{ind}if {local} < 0 {{\n"));
                    self.depth += 1;
                    out.push_str(&format!(
                        "{ind}    return Result::Err(\"Variable of type # can't be negative\");\n"
                    ));
                    self.depth -= 1;
                    out.push_str(&format!("{ind}}}\n"));
                }
                FieldType::Prim(Prim::True) => {
                    // `true` only exists wrapped in a Conditional — its
                    // value derives from the flags word, never the wire.
                    unreachable!("bare ?true field")
                }
                FieldType::Conditional {
                    flags_field,
                    bit,
                    inner,
                } => {
                    let flags_local = field_ident(&fields[*flags_field].name);
                    if **inner == FieldType::Prim(Prim::True) {
                        out.push_str(&format!(
                            "{ind}let {local}: bool = ({flags_local} & {}) != 0;\n",
                            bit_mask(*bit)
                        ));
                    } else {
                        // Single `let … = if … { … } else { … };` — the
                        // cleared case Default-constructs, exactly like the
                        // C++ members that simply stay default when the bit
                        // is not read.
                        out.push_str(&format!(
                            "{ind}let {local}: {} = if ({flags_local} & {}) != 0 {{\n",
                            field_decl_type(p, self.m, self.opts, inner),
                            bit_mask(*bit)
                        ));
                        self.depth += 1;
                        let mut prelude = String::new();
                        let expr = self.emit_read(&mut prelude, p, inner);
                        // Class fields wrap in `Some` — the cleared branch's
                        // `Default::default()` is then `None`, exactly td's
                        // object_ptr that simply stays null.
                        let expr = if is_class(self.m, inner) {
                            format!("Some({expr})")
                        } else {
                            expr
                        };
                        out.push_str(&prelude);
                        out.push_str(&format!("{}{expr}\n", self.indent()));
                        self.depth -= 1;
                        out.push_str(&format!(
                            "{ind}}} else {{\n{ind}    Default::default()\n{ind}}};\n"
                        ));
                    }
                }
                _ => {
                    // The read may emit multi-statement prelude (vectors);
                    // flush it before this field's own statement.
                    let mut prelude = String::new();
                    let expr = self.emit_read(&mut prelude, p, &f.ty);
                    let expr = if is_class(self.m, &f.ty) {
                        format!("Some({expr})")
                    } else {
                        expr
                    };
                    out.push_str(&prelude);
                    out.push_str(&format!(
                        "{ind}let {local}: {} = {expr};\n",
                        field_decl_type(p, self.m, self.opts, &f.ty)
                    ));
                }
            }
        }
        let idents: Vec<String> = fields.iter().map(|f| field_ident(&f.name)).collect();
        out.push_str(&format!(
            "        Result::Ok(Self {{ {} }})\n",
            idents.join(", ")
        ));
    }

    /// Emits the statements reading one value and returns its expression.
    fn emit_read(&mut self, out: &mut String, p: &TlProgram, ty: &FieldType) -> String {
        let ind = self.indent();
        match ty {
            FieldType::Prim(prim) => match prim {
                Prim::Nat | Prim::Int32 => "r.i32()?".to_owned(),
                Prim::Int53 | Prim::Int64 => "r.i64()?".to_owned(),
                Prim::Double => "r.f64()?".to_owned(),
                Prim::Bool => "r.bool()?".to_owned(),
                Prim::String => {
                    if self.opts.raw_strings {
                        "r.bytes()?".to_owned()
                    } else {
                        "r.string()?".to_owned()
                    }
                }
                Prim::Bytes => "r.bytes()?".to_owned(),
                Prim::True => unreachable!("?true read in the per-field walk"),
            },
            FieldType::Vector { boxed, inner } => {
                let n = self.var_counter;
                self.var_counter += 1;
                let v = format!("__v{n}");
                let elem_ty = rust_field_type(p, self.m, self.opts, inner);
                out.push_str(&format!("{ind}let {v}: Vec<{elem_ty}> = {{\n{ind}    "));
                self.depth += 1;
                let ind2 = self.indent();
                if *boxed {
                    out.push_str(&format!(
                        "if r.i32()? != tl::VECTOR_ID {{\n{ind2}    return Result::Err(\"Wrong constructor\");\n{ind2}}}\n{ind2}"
                    ));
                }
                // td's TlFetchVector: the count (u32 view of the i32) must
                // not exceed the bytes left — this also rejects the huge
                // negative counts.
                out.push_str(&format!(
                    "let count: u32 = r.i32()? as u32;\n\
                     {ind2}if count as usize > r.remaining() {{\n\
                     {ind2}    return Result::Err(\"Wrong vector length\");\n\
                     {ind2}}}\n\
                     {ind2}let mut res = Vec::with_capacity(count as usize);\n\
                     {ind2}for _ in 0..count {{\n"
                ));
                self.depth += 1;
                let elem = self.emit_read(out, p, inner);
                out.push_str(&format!("{}res.push({elem});\n", self.indent()));
                self.depth -= 1;
                out.push_str(&format!("{ind2}}}\n{ind2}res\n"));
                self.depth -= 1;
                out.push_str(&format!("{ind}}};\n"));
                format!("__v{n}")
            }
            FieldType::FixedArray { len, inner } => {
                // Exactly N elements; the array literal spells every read.
                // Multiplicity arrays over non-primitives don't exist in the
                // pinned schemes (int128/256/512 words only), so no prelude
                // handling is needed here.
                assert!(
                    matches!(**inner, FieldType::Prim(_)),
                    "fixed array over a non-primitive type"
                );
                let elem = self.emit_read(out, p, inner);
                let elems = vec![elem; *len as usize].join(", ");
                format!("[{elems}]")
            }
            FieldType::Boxed(t) | FieldType::Bare(t) => {
                // Boxed references go through the target type's id-checked
                // read (struct `read` or enum dispatch) — td's
                // `TlFetchObject<T>::parse`; bare ones skip the id.
                let f = if matches!(ty, FieldType::Bare(_)) {
                    "read_bare"
                } else {
                    "read"
                };
                let expr = format!("{}::{f}(r)?", self.m.type_name(p, *t));
                if self.m.cyclic[*t] {
                    format!("Box::new({expr})")
                } else {
                    expr
                }
            }
            FieldType::Function => {
                format!("Box::new({}::read(r)?)", self.opts.umbrella)
            }
            FieldType::Conditional { .. } => unreachable!("conditionals are handled per-field"),
        }
    }
}

const SUPPORT_MODULE: &str = r#"/// TL binary store primitives — the Rust analogue of td's `TlStorerUnsafe`
/// low-level writes (`tdutils/td/utils/tl_storers.h`).
pub mod tl {
    /// `vector#1cb5c415` — the constructor id preceding boxed `Vector<T>`
    /// fields on the wire (td's `TlStoreBoxed<TlStoreVector, 481674261>`).
    pub const VECTOR_ID: i32 = 0x1cb5_c415;

    pub fn __i32(out: &mut Vec<u8>, v: i32) {
        out.extend_from_slice(&v.to_le_bytes());
    }

    pub fn __i64(out: &mut Vec<u8>, v: i64) {
        out.extend_from_slice(&v.to_le_bytes());
    }

    pub fn __f64(out: &mut Vec<u8>, v: f64) {
        out.extend_from_slice(&v.to_le_bytes());
    }

    pub fn __bool(out: &mut Vec<u8>, v: bool) {
        __i32(out, if v { 0x9972_75b5u32 as i32 } else { 0xbc79_9737u32 as i32 });
    }

    /// TL string framing: 1-byte length up to 253, 0xfe + 3 bytes up to
    /// 2^24, 0xff + 4 bytes (+3 zero bytes) up to 2^32; payload always
    /// zero-padded so header+payload is a multiple of 4 (the 1-byte header
    /// counts toward the length, the longer ones already are aligned).
    pub fn __string<T: AsRef<[u8]>>(out: &mut Vec<u8>, v: T) {
        let v = v.as_ref();
        let len = v.len();
        let counted;
        if len < 254 {
            out.push(len as u8);
            counted = len + 1;
        } else if len < (1 << 24) {
            out.push(254);
            out.extend_from_slice(&(len as u32).to_le_bytes()[..3]);
            counted = len;
        } else {
            assert!(len < (1usize << 32), "String size {len} is too big to be stored");
            out.push(255);
            out.extend_from_slice(&(len as u32).to_le_bytes());
            out.extend_from_slice(&[0, 0, 0]);
            counted = len;
        }
        out.extend_from_slice(v);
        let rem = counted & 3;
        if rem != 0 {
            out.resize(out.len() + (4 - rem), 0);
        }
    }

    /// Bounds-checked TL parse cursor — the Rust analogue of
    /// `TlParser`/`TlBufferParser` (`tdutils/td/utils/tl_parsers.h`).
    /// Every read is little-endian; on any short read the error is
    /// "Not enough data to read", exactly like C++.
    pub struct Reader<'a> {
        buf: &'a [u8],
        pos: usize,
    }

    impl<'a> Reader<'a> {
        pub fn new(buf: &'a [u8]) -> Self {
            Self { buf, pos: 0 }
        }

        pub fn remaining(&self) -> usize {
            self.buf.len() - self.pos
        }

        fn need(&mut self, n: usize) -> Result<(), &'static str> {
            if self.remaining() < n {
                Err("Not enough data to read")
            } else {
                Ok(())
            }
        }

        fn take<const N: usize>(&mut self) -> Result<[u8; N], &'static str> {
            self.need(N)?;
            let v: [u8; N] = self.buf[self.pos..self.pos + N].try_into().unwrap();
            self.pos += N;
            Ok(v)
        }

        pub fn i32(&mut self) -> Result<i32, &'static str> {
            Ok(i32::from_le_bytes(self.take()?))
        }

        /// `TlParser::fetch_string_raw` — exactly `len` raw bytes with no
        /// alignment padding (the `message` bodies of containers and the
        /// session layer's `bytes` fields).
        pub fn raw(&mut self, len: usize) -> Result<&'a [u8], &'static str> {
            self.need(len)?;
            let start = self.pos;
            self.pos += len;
            Ok(&self.buf[start..self.pos])
        }

        pub fn i64(&mut self) -> Result<i64, &'static str> {
            Ok(i64::from_le_bytes(self.take()?))
        }

        pub fn f64(&mut self) -> Result<f64, &'static str> {
            Ok(f64::from_le_bytes(self.take()?))
        }

        /// td's `TlFetchBool`: id-checked — anything but the two bool ids
        /// errors with "Bool expected".
        pub fn bool(&mut self) -> Result<bool, &'static str> {
            match self.i32()? {
                v if v == 0x9972_75b5u32 as i32 => Ok(true),
                v if v == 0xbc79_9737u32 as i32 => Ok(false),
                _ => Err("Bool expected"),
            }
        }

        /// `TlParser::fetch_string` framing (tdutils tl_parsers.h) plus the
        /// `TlBufferParser` sanitization td applies to *string* fields:
        /// NULs become spaces and invalid UTF-8 truncates at the last valid
        /// character position.
        pub fn string(&mut self) -> Result<String, &'static str> {
            let raw = self.raw_string()?;
            let mut s = Vec::with_capacity(raw.len());
            for &c in raw {
                s.push(if c == b'\0' { b' ' } else { c });
            }
            match String::from_utf8(s) {
                Ok(v) => Ok(v),
                Err(e) => {
                    // Truncate at the last valid UTF-8 boundary (td's
                    // `last_utf8_character_position`).
                    let mut upto = e.utf8_error().valid_up_to();
                    let s = e.into_bytes();
                    while std::str::from_utf8(&s[..upto]).is_err() {
                        upto -= 1;
                    }
                    Ok(String::from_utf8(s[..upto].to_vec()).unwrap())
                }
            }
        }

        /// Bytes are raw — no sanitization (td reads them with the plain
        /// `fetch_string<BufferSlice>` path).
        pub fn bytes(&mut self) -> Result<Vec<u8>, &'static str> {
            self.raw_string().map(<[u8]>::to_vec)
        }

        fn raw_string(&mut self) -> Result<&'a [u8], &'static str> {
            self.need(4)?;
            let first = self.buf[self.pos] as usize;
            let (len, advance, payload_at): (usize, usize, usize);
            if first < 254 {
                len = first;
                advance = 4; // td's `data += sizeof(int32)`
                payload_at = self.pos + 1;
            } else if first == 254 {
                self.need(3)?;
                len = self.buf[self.pos + 1] as usize
                    | (self.buf[self.pos + 2] as usize) << 8
                    | (self.buf[self.pos + 3] as usize) << 16;
                advance = 4;
                payload_at = self.pos + 4;
            } else {
                self.need(7)?;
                let len64 = self.buf[self.pos + 1] as u64
                    | (self.buf[self.pos + 2] as u64) << 8
                    | (self.buf[self.pos + 3] as u64) << 16
                    | (self.buf[self.pos + 4] as u64) << 24
                    | (self.buf[self.pos + 5] as u64) << 32
                    | (self.buf[self.pos + 6] as u64) << 40
                    | (self.buf[self.pos + 7] as u64) << 48;
                if len64 > usize::MAX as u64 - 3 {
                    return Err("Too big string found");
                }
                len = len64 as usize;
                advance = 8; // td's `data += sizeof(int64)`
                payload_at = self.pos + 8;
            }
            // Payload plus 4-alignment. td advances the header word first
            // (short/mid: `data += sizeof(int32)`, long: `sizeof(int64)`)
            // and then `result_aligned_len` — for the short form that is
            // `floor4(len)`, so the total window is 4 + floor4(len) ==
            // ceil4(1 + len), matching the writer's `counted = len + 1`
            // padding; the longer headers are already aligned, so their
            // window is header + ceil4(len).
            let aligned = if first < 254 {
                (len >> 2) << 2
            } else {
                ((len + 3) >> 2) << 2
            };
            self.need(advance + aligned)?;
            let payload_end = payload_at + len;
            self.pos += advance + aligned;
            Ok(&self.buf[payload_at..payload_end])
        }
    }
}

"#;

/// The S12 JSON runtime: a td-exact compact serializer and lenient parser
/// mirroring `tdutils/td/utils/JsonBuilder.{h,cpp}`, `td/tl/tl_json.h` and
/// `tdutils/td/utils/base64.cpp`. The generated `to_json_into`/`from_json`
/// calls are thin wrappers over these primitives.
const JSON_SUPPORT_MODULE: &str = r#"/// td_api JSON runtime — the Rust analogue of td's JsonBuilder + tl_json.h
/// (S12). Semantics are fixed by the differential oracle against td's own
/// generated `td_api_json_*.cpp` (`conformance/tl_json_oracle`).
pub mod tl_json {
    /// td's JsonValue (`Null/Number/Boolean/String/Array/Object`); numbers
    /// keep their raw source text, exactly like td's slice-backed values.
    pub enum Json {
        Null,
        Bool(bool),
        Number(String),
        String(String),
        Array(Vec<Json>),
        Object(Object),
    }

    /// Field order is preserved; `field` scans in order, first match wins —
    /// td's `JsonObject::extract_field` on its vector of pairs.
    pub type Object = Vec<(String, Json)>;

    impl Json {
        /// td's `JsonValue::get_type_name` strings.
        pub fn type_name(&self) -> &'static str {
            match self {
                Json::Null => "Null",
                Json::Bool(_) => "Boolean",
                Json::Number(_) => "Number",
                Json::String(_) => "String",
                Json::Array(_) => "Array",
                Json::Object(_) => "Object",
            }
        }
    }

    pub fn field<'a>(o: &'a Object, name: &str) -> Option<&'a Json> {
        o.iter().find(|(k, _)| k == name).map(|(_, v)| v)
    }

    // ---- serialization (td's JsonBuilder, compact mode) -------------------

    pub fn num32(out: &mut String, v: i32) {
        out.push_str(&v.to_string());
    }

    pub fn num64(out: &mut String, v: i64) {
        out.push_str(&v.to_string());
    }

    /// td's JsonInt64: int64 fields serialize as quoted decimal strings.
    pub fn num64q(out: &mut String, v: i64) {
        out.push('"');
        num64(out, v);
        out.push('"');
    }

    /// td's StringBuilder `<<` on double: fixed 6 decimals (iostreams with
    /// classic locale); C++ prints nan/inf as bare words, so map NaN's sign
    /// bit the way glibc does ("nan"/"-nan").
    pub fn double(out: &mut String, v: f64) {
        if v.is_nan() {
            out.push_str(if v.is_sign_negative() { "-nan" } else { "nan" });
            return;
        }
        use std::fmt::Write as _;
        let _ = write!(out, "{v:.6}");
    }

    pub fn boolean(out: &mut String, v: bool) {
        out.push_str(if v { "true" } else { "false" });
    }

    /// The TDObject JSON string writer (td's generated `to_json`, backed by
    /// `JsonRawString`): ASCII printable literal; short escapes for
    /// `"`/`\`/`\b\f\n\r\t`; other controls as lowercase `\uXXXX`; every
    /// non-ASCII byte passes through raw UTF-8 (verified byte-identical
    /// against the C++ `td_execute` outputs — M.8 corpus). Note this is
    /// `JsonRawString` semantics, NOT `JsonString`'s all-non-ASCII `\u`
    /// escaping.
    pub fn str(out: &mut String, s: &str) {
        out.push('"');
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\u{0008}' => out.push_str("\\b"),
                '\u{000c}' => out.push_str("\\f"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if (c as u32) < 0x20 => escape_one(out, c as u32),
                _ => out.push(c),
            }
        }
        out.push('"');
    }

    fn escape_one(out: &mut String, c: u32) {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        out.push_str("\\u");
        out.push(HEX[(c >> 12) as usize] as char);
        out.push(HEX[((c >> 8) & 15) as usize] as char);
        out.push(HEX[((c >> 4) & 15) as usize] as char);
        out.push(HEX[(c & 15) as usize] as char);
    }

    /// Bytes as standard-alphabet base64 with `=` padding (td's base64_encode).
    pub fn bytes(out: &mut String, b: &[u8]) {
        out.push('"');
        encode_b64(out, b);
        out.push('"');
    }

    const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn encode_b64(out: &mut String, input: &[u8]) {
        for chunk in input.chunks(3) {
            let left = chunk.len();
            let mut c = (chunk[0] as u32) << 16;
            out.push(B64[(c >> 18) as usize] as char);
            if left != 1 {
                c |= (chunk[1] as u32) << 8;
            }
            out.push(B64[((c >> 12) & 63) as usize] as char);
            if left == 3 {
                c |= chunk[2] as u32;
            }
            if left != 1 {
                out.push(B64[((c >> 6) & 63) as usize] as char);
            } else {
                out.push('=');
            }
            if left == 3 {
                out.push(B64[(c & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }

    /// td's base64_decode: padding dropped (0–2 `=`), standard alphabet,
    /// non-canonical trailing bits rejected.
    pub fn decode_b64(b64: &str) -> Result<Vec<u8>, String> {
        let mut s = b64.as_bytes();
        let mut padding = 0;
        while !s.is_empty() && s[s.len() - 1] == b'=' {
            s = &s[..s.len() - 1];
            padding += 1;
        }
        if padding >= 3 {
            return Err("Wrong string padding".to_owned());
        }
        if (s.len() + padding) & 3 != 0 {
            return Err("Wrong padding length".to_owned());
        }
        let val = |c: u8| -> Option<u32> {
            B64.iter().position(|&x| x == c).map(|i| i as u32)
        };
        let mut out = Vec::with_capacity(s.len() / 4 * 3 + 2);
        let mut i = 0;
        while i < s.len() {
            let left = (s.len() - i).min(4);
            let mut c: u32 = 0;
            for t in 0..left {
                let v = val(s[i]).ok_or_else(|| "Wrong character in the string".to_owned())?;
                i += 1;
                c |= v << ((3 - t) * 6);
            }
            out.push((c >> 16) as u8);
            if left == 2 {
                if c & 0xFFFF != 0 {
                    return Err("Wrong padding in the string".to_owned());
                }
            } else {
                out.push((c >> 8) as u8);
                if left == 3 {
                    if c & 0xFF != 0 {
                        return Err("Wrong padding in the string".to_owned());
                    }
                } else {
                    out.push(c as u8);
                }
            }
        }
        Ok(out)
    }

    // ---- parsing (td's json_decode / do_json_decode) ----------------------

    /// td's `json_decode`: whole input must be one value (trailing
    /// whitespace then end), nesting depth capped at 100.
    pub fn parse_json(input: &str) -> Result<Json, String> {
        let mut p = Parser {
            b: input.as_bytes(),
            pos: 0,
        };
        let v = p.value(100)?;
        p.skip_ws();
        if p.pos != p.b.len() {
            return Err("Expected string end".to_owned());
        }
        Ok(v)
    }

    struct Parser<'a> {
        b: &'a [u8],
        pos: usize,
    }

    impl Parser<'_> {
        fn skip_ws(&mut self) {
            while self.pos < self.b.len()
                && matches!(self.b[self.pos], b' ' | b'\t' | b'\r' | b'\n')
            {
                self.pos += 1;
            }
        }

        fn peek(&self) -> u8 {
            if self.pos < self.b.len() {
                self.b[self.pos]
            } else {
                0
            }
        }

        fn try_lit(&mut self, lit: &str) -> bool {
            if self.b[self.pos..].starts_with(lit.as_bytes()) {
                self.pos += lit.len();
                true
            } else {
                false
            }
        }

        fn value(&mut self, depth: i32) -> Result<Json, String> {
            if depth < 0 {
                return Err("Too big object depth".to_owned());
            }
            self.skip_ws();
            match self.peek() {
                b'f' => {
                    if self.try_lit("false") {
                        Ok(Json::Bool(false))
                    } else {
                        Err("Token starts with 'f' -- false expected".to_owned())
                    }
                }
                b't' => {
                    if self.try_lit("true") {
                        Ok(Json::Bool(true))
                    } else {
                        Err("Token starts with 't' -- true expected".to_owned())
                    }
                }
                b'n' => {
                    if self.try_lit("null") {
                        Ok(Json::Null)
                    } else {
                        Err("Token starts with 'n' -- null expected".to_owned())
                    }
                }
                b'"' => self.string().map(Json::String),
                b'[' => {
                    self.pos += 1;
                    self.skip_ws();
                    let mut res = Vec::new();
                    if self.try_lit("]") {
                        return Ok(Json::Array(res));
                    }
                    loop {
                        if self.pos == self.b.len() {
                            return Err("Unexpected string end".to_owned());
                        }
                        res.push(self.value(depth - 1)?);
                        self.skip_ws();
                        if self.try_lit("]") {
                            break;
                        }
                        if self.try_lit(",") {
                            self.skip_ws();
                            continue;
                        }
                        if self.pos == self.b.len() {
                            return Err("Unexpected string end".to_owned());
                        }
                        return Err("Unexpected symbol while parsing JSON Array".to_owned());
                    }
                    Ok(Json::Array(res))
                }
                b'{' => {
                    self.pos += 1;
                    self.skip_ws();
                    if self.try_lit("}") {
                        return Ok(Json::Object(Vec::new()));
                    }
                    let mut fields = Vec::new();
                    loop {
                        if self.pos == self.b.len() {
                            return Err("Unexpected string end".to_owned());
                        }
                        let key = self.string()?;
                        self.skip_ws();
                        if !self.try_lit(":") {
                            return Err("':' expected".to_owned());
                        }
                        let v = self.value(depth - 1)?;
                        fields.push((key, v));
                        self.skip_ws();
                        if self.try_lit("}") {
                            break;
                        }
                        if self.try_lit(",") {
                            self.skip_ws();
                            continue;
                        }
                        if self.pos == self.b.len() {
                            return Err("Unexpected string end".to_owned());
                        }
                        return Err("Unexpected symbol while parsing JSON Object".to_owned());
                    }
                    Ok(Json::Object(fields))
                }
                b'-' | b'+' | b'.' | b'0'..=b'9' => {
                    let start = self.pos;
                    while self.pos < self.b.len()
                        && matches!(
                            self.b[self.pos],
                            b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9'
                        )
                    {
                        self.pos += 1;
                    }
                    Ok(Json::Number(
                        String::from_utf8_lossy(&self.b[start..self.pos]).into_owned(),
                    ))
                }
                0 => Err("Unexpected string end".to_owned()),
                c if (1..127).contains(&c) => {
                    Err(format!("Unexpected symbol '{}'", c as char))
                }
                _ => Err("Unexpected symbol".to_owned()),
            }
        }

        /// td's json_string_decode: escapes b/f/n/r/t/u (case-insensitive
        /// hex), `\u` surrogate pairs, any other escaped char passes through
        /// verbatim. Lone surrogates would build invalid UTF-8 in C++; a
        /// Rust String cannot hold them, so they become U+FFFD.
        fn string(&mut self) -> Result<String, String> {
            if self.peek() != b'"' {
                return Err("Opening '\"' expected".to_owned());
            }
            self.pos += 1;
            let mut buf: Vec<u8> = Vec::new();
            loop {
                if self.pos == self.b.len() {
                    return Err("Closing '\"' not found".to_owned());
                }
                let c = self.b[self.pos];
                self.pos += 1;
                match c {
                    b'"' => return Ok(String::from_utf8_lossy(&buf).into_owned()),
                    b'\\' => {
                        if self.pos == self.b.len() {
                            return Err("Closing '\"' not found".to_owned());
                        }
                        let e = self.b[self.pos];
                        self.pos += 1;
                        match e {
                            b'b' => buf.push(0x08),
                            b'f' => buf.push(0x0C),
                            b'n' => buf.push(b'\n'),
                            b'r' => buf.push(b'\r'),
                            b't' => buf.push(b'\t'),
                            b'u' => {
                                let cp = self.unicode_escape()?;
                                let mut tmp = [0u8; 4];
                                buf.extend_from_slice(
                                    char::from_u32(cp)
                                        .unwrap_or('\u{FFFD}')
                                        .encode_utf8(&mut tmp)
                                        .as_bytes(),
                                );
                            }
                            other => buf.push(other),
                        }
                    }
                    _ => buf.push(c),
                }
            }
        }

        /// The `\uXXXX` tail after the backslash-u, including td's surrogate
        /// pair combination (second escape consumed only if it is one too).
        fn unicode_escape(&mut self) -> Result<u32, String> {
            let num = self.hex4()?;
            if (0xD7FF..0xE000).contains(&num)
                && self.pos + 6 <= self.b.len()
                && self.b[self.pos] == b'\\'
                && self.b[self.pos + 1] == b'u'
            {
                let save = self.pos;
                self.pos += 2;
                let num2 = self.hex4()?;
                if (0xD7FF..0xE000).contains(&num2) {
                    return Ok((((num & 0x3FF) << 10) | (num2 & 0x3FF)) + 0x10000);
                }
                self.pos = save;
            }
            Ok(num)
        }

        fn hex4(&mut self) -> Result<u32, String> {
            if self.pos + 4 > self.b.len() {
                return Err("\\u has less than 4 symbols".to_owned());
            }
            let mut num: u32 = 0;
            for _ in 0..4 {
                match hex_to_int(self.b[self.pos]) {
                    Some(d) => {
                        num = num * 16 + d;
                        self.pos += 1;
                    }
                    None => return Err("Invalid \\u -- not hex digit".to_owned()),
                }
            }
            Ok(num)
        }
    }

    /// td's hex_to_int: digits and both hex letter cases.
    fn hex_to_int(c: u8) -> Option<u32> {
        match c {
            b'0'..=b'9' => Some((c - b'0') as u32),
            b'a'..=b'f' => Some((c - b'a' + 10) as u32),
            b'A'..=b'F' => Some((c - b'A' + 10) as u32),
            _ => None,
        }
    }

    // ---- typed reads (td's tl_json.h from_json overloads) -----------------

    /// td's `to_integer<int32>`: leading `-`, digits until a non-digit, all
    /// arithmetic in the unsigned domain with td's negation dance — the
    /// i64 value truncated to the low 32 bits is bit-identical to running
    /// C++'s algorithm in 32-bit width.
    pub fn to_integer_i32(s: &str) -> i32 {
        to_integer_i64(s) as i32
    }

    pub fn to_integer_i64(s: &str) -> i64 {
        let b = s.as_bytes();
        let mut i = 0;
        let mut negative = false;
        if i < b.len() && b[i] == b'-' {
            negative = true;
            i += 1;
        }
        let mut v: u64 = 0;
        while i < b.len() && b[i].is_ascii_digit() {
            v = v
                .wrapping_mul(10)
                .wrapping_add((b[i] - b'0') as u64);
            i += 1;
        }
        if v > i64::MAX as u64 {
            v = v.wrapping_neg();
            negative = !negative;
            if v > i64::MAX as u64 {
                return i64::MIN;
            }
        }
        if negative {
            (v as i64).wrapping_neg()
        } else {
            v as i64
        }
    }

    fn integer_err(s: &str) -> String {
        format!("Can't parse as an integer string \"{s}\"")
    }

    /// td's from_json(int32&): Number or String parsed strictly
    /// (`(PSLICE() << res) == str`), Null keeps the default.
    pub fn read_i32(v: &Json) -> Result<i32, String> {
        match v {
            Json::Number(n) | Json::String(n) => {
                let r = to_integer_i32(n);
                if r.to_string() == *n {
                    Ok(r)
                } else {
                    Err(integer_err(n))
                }
            }
            Json::Null => Ok(0),
            other => Err(format!("Expected Number, but receive {}", other.type_name())),
        }
    }

    pub fn read_i64(v: &Json) -> Result<i64, String> {
        match v {
            Json::Number(n) | Json::String(n) => {
                let r = to_integer_i64(n);
                if r.to_string() == *n {
                    Ok(r)
                } else {
                    Err(integer_err(n))
                }
            }
            Json::Null => Ok(0),
            other => Err(format!(
                "Expected String or Number, but receive {}",
                other.type_name()
            )),
        }
    }

    /// td's from_json(bool&): Boolean, or any int form (non-zero wins),
    /// with the inner int error swallowed into the Boolean message.
    pub fn read_bool(v: &Json) -> Result<bool, String> {
        match v {
            Json::Bool(b) => Ok(*b),
            Json::Null => Ok(false),
            Json::Number(_) | Json::String(_) => read_i32(v)
                .map(|x| x != 0)
                .map_err(|_| format!("Expected Boolean, but receive {}", v.type_name())),
            other => Err(format!("Expected Boolean, but receive {}", other.type_name())),
        }
    }

    /// td's from_json(double&): Number only; unparsable text reads as 0.0
    /// (C++ to_double via strtod never errors).
    pub fn read_f64(v: &Json) -> Result<f64, String> {
        match v {
            Json::Number(n) => Ok(n.parse::<f64>().unwrap_or(0.0)),
            Json::Null => Ok(0.0),
            other => Err(format!("Expected Number, but receive {}", other.type_name())),
        }
    }

    pub fn read_string(v: &Json) -> Result<String, String> {
        match v {
            Json::String(s) => Ok(s.clone()),
            Json::Null => Ok(String::new()),
            other => Err(format!("Expected String, but receive {}", other.type_name())),
        }
    }

    pub fn read_bytes(v: &Json) -> Result<Vec<u8>, String> {
        match v {
            Json::String(s) => decode_b64(s),
            Json::Null => Ok(Vec::new()),
            other => Err(format!("Expected String, but receive {}", other.type_name())),
        }
    }

    pub fn read_vector<T>(
        v: &Json,
        f: impl Fn(&Json) -> Result<T, String>,
    ) -> Result<Vec<T>, String> {
        match v {
            Json::Array(a) => a.iter().map(f).collect(),
            Json::Null => Ok(Vec::new()),
            other => Err(format!("Expected Array, but receive {}", other.type_name())),
        }
    }
}

"#;

/// Emits the M.14.2 debug-print methods for one constructor struct: td's
/// generated `Class::store(TlStorerToString&, const char*)` (storer_type 1)
/// plus the `to_string(const BaseObject&)` wrapper. The C++ CLI transcript
/// (the M.14.7 oracle) is built from these strings, so every formatting
/// quirk of `TlStorerToString` is load-bearing.
fn emit_struct_tostring(
    out: &mut String,
    p: &TlProgram,
    m: &SchemeModel,
    c: usize,
    fields: &[Field],
) {
    out.push_str(
        "\n    /// td's generated `Class::store(TlStorerToString&, const char*)`\n    \
         /// (storer_type 1, the `LOG_IS_STRIPPED(ERROR)`-guarded print).\n    \
         pub fn store_debug(&self, s: &mut TlStorerToString, field_name: &str) {\n",
    );
    out.push_str(&format!(
        "        s.store_class_begin(field_name, \"{}\");\n",
        pretty_class_name(&p.ctors[c].id)
    ));
    ToStringStoreWriter {
        m,
        depth: 2,
        var_counter: 0,
    }
    .emit_fields(out, fields);
    out.push_str("        s.store_class_end();\n    }\n");
    out.push_str(
        "\n    /// td's `to_string(const BaseObject&)` for this constructor.\n    \
         pub fn to_debug_string(&self) -> String {\n        \
         let mut out = String::new();\n        \
         let mut storer = TlStorerToString { out: &mut out, shift: 0 };\n        \
         self.store_debug(&mut storer, \"\");\n        \
         out\n    }\n",
    );
}

/// Emits the debug-print dispatch for one enum (and the `Function`
/// umbrella): forwards to the variant struct's `store_debug`, like the C++
/// virtual `store` through the base class.
fn emit_enum_tostring(out: &mut String, p: &TlProgram, ctors: &[usize]) {
    out.push_str(
        "\n    pub fn store_debug(&self, s: &mut TlStorerToString, field_name: &str) {\n        \
         match self {\n",
    );
    for &c in ctors {
        let variant = capitalize(&p.ctors[c].id);
        out.push_str(&format!(
            "            Self::{variant}(v) => v.store_debug(s, field_name),\n"
        ));
    }
    out.push_str("        }\n    }\n");
    out.push_str(
        "\n    /// td's `to_string(const BaseObject&)` for this type.\n    \
         pub fn to_debug_string(&self) -> String {\n        \
         let mut out = String::new();\n        \
         let mut storer = TlStorerToString { out: &mut out, shift: 0 };\n        \
         self.store_debug(&mut storer, \"\");\n        \
         out\n    }\n",
    );
}

/// `TD_TL_writer_cpp::get_pretty_class_name`: underscores become dots
/// outside mtproto (td_api ctor names carry none; the transform is kept for
/// fidelity).
fn pretty_class_name(id: &str) -> String {
    id.replace('_', ".")
}

/// Emits the `store_debug` field statements. Differences from the binary
/// [`StoreWriter`] are the C++ generator's own (`gen_field_store` at
/// storer_type 1): conditionals gate on the *raw* flags word (the
/// `var0 = flags | bools` capture exists only for the wire), `?true` bools
/// print as their own `name = true` lines, null class pointers print `null`
/// (never default-materialized), and vectors carry no constructor id.
struct ToStringStoreWriter<'a> {
    m: &'a SchemeModel,
    depth: usize,
    var_counter: usize,
}

impl ToStringStoreWriter<'_> {
    fn indent(&self) -> String {
        " ".repeat(self.depth * 4)
    }

    fn emit_fields(&mut self, out: &mut String, fields: &[Field]) {
        for f in fields {
            // td's `get_pretty_field_name`: the TL name minus trailing
            // underscores (the C++ member suffix); vector internals pass "".
            let pretty = f.name.trim_end_matches('_');
            let expr = format!("self.{}", field_ident(&f.name));
            if let FieldType::Conditional {
                flags_field,
                bit,
                inner,
            } = &f.ty
            {
                let flags = format!("self.{}", field_ident(&fields[*flags_field].name));
                let mask = bit_mask(*bit);
                if **inner == FieldType::Prim(Prim::True) {
                    out.push_str(&format!(
                        "{}if {flags} & {mask} != 0 {{ s.store_field_bool(\"{pretty}\", true); }}\n",
                        self.indent()
                    ));
                    continue;
                }
                out.push_str(&format!("{}if {flags} & {mask} != 0 {{\n", self.indent()));
                self.depth += 1;
                self.emit_value(out, pretty, &expr, inner);
                self.depth -= 1;
                out.push_str(&format!("{}}}\n", self.indent()));
            } else {
                self.emit_value(out, pretty, &expr, &f.ty);
            }
        }
    }

    /// The borrow form of a place expression for `&`/`match` positions:
    /// deref places (vector items) reborrow through the deref (`&*v`, the
    /// clippy-accepted form the binary writer uses).
    fn borrow(expr: &str) -> String {
        if expr.starts_with('*') {
            format!("&*{expr}")
        } else {
            format!("&{expr}")
        }
    }

    /// The borrow form for the string/bytes arguments: an owned place takes
    /// `&`, while a loop item (`*__v0`) is already a shared reference one
    /// level out — pass the variable bare and let deref coercion produce the
    /// `&str`/`&[u8]` (the `&**v` form trips clippy's `explicit_auto_deref`).
    fn borrow_str(expr: &str) -> String {
        if expr.starts_with('*') {
            expr.trim_start_matches('*').to_owned()
        } else {
            format!("&{expr}")
        }
    }

    /// The borrow form for an `Option` match scrutinee: the place is an
    /// `Option<T>` (vector items arrive as `*__v0` derefs), so one deref
    /// level — `&*__v0` is `&Option<T>`, where the prim arm's [`Self::borrow`]
    /// would peel into the `Option` itself.
    fn borrow_option(expr: &str) -> String {
        if expr.starts_with('*') {
            format!("&*{}", expr.trim_start_matches('*'))
        } else {
            format!("&{expr}")
        }
    }

    /// Emits the statement printing `expr` (a place expression) as `ty`.
    fn emit_value(&mut self, out: &mut String, name: &str, expr: &str, ty: &FieldType) {
        let ind = self.indent();
        let place = |e: &str| {
            if e.starts_with('*') {
                format!("({e})")
            } else {
                e.to_owned()
            }
        };
        match ty {
            FieldType::Prim(prim) => {
                // A bare place expression: no parens — unary deref binds
                // tighter than `as`, so `*__v0 as i64` parses correctly, and
                // the parenthesized form trips `unused_parens` in the
                // generated module.
                let p = expr;
                match prim {
                    Prim::Nat | Prim::Int32 => out.push_str(&format!(
                        "{ind}s.store_field_int(\"{name}\", {p} as i64);\n"
                    )),
                    Prim::Int53 | Prim::Int64 => {
                        out.push_str(&format!("{ind}s.store_field_int(\"{name}\", {p});\n"))
                    }
                    Prim::Double => {
                        out.push_str(&format!("{ind}s.store_field_double(\"{name}\", {p});\n"))
                    }
                    Prim::Bool => {
                        out.push_str(&format!("{ind}s.store_field_bool(\"{name}\", {p});\n"))
                    }
                    Prim::String => out.push_str(&format!(
                        "{ind}s.store_field_str(\"{name}\", {});\n",
                        Self::borrow_str(expr)
                    )),
                    Prim::Bytes => out.push_str(&format!(
                        "{ind}s.store_bytes_field(\"{name}\", {});\n",
                        Self::borrow_str(expr)
                    )),
                    Prim::True => unreachable!("?true is folded into its flags gate"),
                }
            }
            FieldType::Vector { inner, .. } => {
                let n = self.var_counter;
                self.var_counter += 1;
                let var = format!("__v{n}");
                out.push_str(&format!(
                    "{ind}s.store_vector_begin(\"{name}\", {}.len());\n",
                    place(expr)
                ));
                out.push_str(&format!("{ind}for {var} in {} {{\n", Self::borrow(expr)));
                self.depth += 1;
                match inner.as_ref() {
                    // Vector elements are plain values in this model (the
                    // S76 non-nullable `object_ptr` collapse) — no null arm.
                    FieldType::Boxed(_) | FieldType::Bare(_) | FieldType::Function => {
                        out.push_str(&format!("{ind}    {var}.store_debug(s, \"\");\n"));
                    }
                    t => self.emit_value(out, "", &format!("*{var}"), t),
                }
                self.depth -= 1;
                out.push_str(&format!("{ind}}}\n"));
                // The vector's closing brace is `store_class_end`, exactly
                // as the C++ generates (`} s.store_class_end(); }`).
                out.push_str(&format!("{ind}s.store_class_end();\n"));
            }
            FieldType::FixedArray { len, inner } => {
                // The fixed word types (int128/int256/int512) print as raw
                // bytes (`store_field(Slice, const UInt128&)` → `{ AB CD }`).
                assert!(
                    matches!(
                        inner.as_ref(),
                        FieldType::Prim(Prim::Nat | Prim::Int32 | Prim::Int53 | Prim::Int64)
                    ),
                    "fixed word arrays hold ints"
                );
                let n = self.var_counter;
                self.var_counter += 1;
                out.push_str(&format!(
                    "{ind}let __bytes{n}: Vec<u8> = {}.iter().flat_map(|w| w.to_le_bytes()).collect();\n",
                    place(expr)
                ));
                out.push_str(&format!(
                    "{ind}s.store_field_binary(\"{name}\", &__bytes{n}); // {len}*word\n"
                ));
            }
            FieldType::Boxed(t) | FieldType::Bare(t) => {
                debug_assert!(
                    !self.m.is_word_type[*t],
                    "word types are printed as primitives"
                );
                let n = self.var_counter;
                self.var_counter += 1;
                out.push_str(&format!("{ind}match {} {{\n", Self::borrow_option(expr)));
                out.push_str(&format!(
                    "{ind}    Some(__m{n}) => __m{n}.store_debug(s, \"{name}\"),\n"
                ));
                out.push_str(&format!("{ind}    None => s.store_null(\"{name}\"),\n"));
                out.push_str(&format!("{ind}}}\n"));
            }
            FieldType::Function => {
                // `Option<Box<Function>>`: the same null-or-recurse shape.
                let n = self.var_counter;
                self.var_counter += 1;
                out.push_str(&format!("{ind}match {} {{\n", Self::borrow_option(expr)));
                out.push_str(&format!(
                    "{ind}    Some(__m{n}) => __m{n}.store_debug(s, \"{name}\"),\n"
                ));
                out.push_str(&format!("{ind}    None => s.store_null(\"{name}\"),\n"));
                out.push_str(&format!("{ind}}}\n"));
            }
            FieldType::Conditional { .. } => unreachable!("conditionals are handled per-field"),
        }
    }
}

const TOSTRING_SUPPORT_MODULE: &str = r#"/// td's `TlStorerToString` (tdutils/td/utils/TlStorerToString.h): the
/// `field = value` storer behind C++ `to_string(const BaseObject&)`.
/// The formatting quirks are load-bearing (the CLI transcript oracle diffs
/// on them): strings are raw bytes in quotes (no escaping), bools print
/// `true`/`false`, doubles are 6-decimal fixed (`FixedDouble(x, 6)`; the
/// C++ `nan`/`inf` spellings differ and never occur in practice), bytes are
/// uppercase-hex capped at 64 with an `...` marker, and every line is
/// `\n`-terminated with a 2-space shift per nesting level.
pub struct TlStorerToString<'a> {
    out: &'a mut String,
    shift: usize,
}

impl TlStorerToString<'_> {
    fn field_begin(&mut self, name: &str) {
        for _ in 0..self.shift {
            self.out.push(' ');
        }
        if !name.is_empty() {
            self.out.push_str(name);
            self.out.push_str(" = ");
        }
    }

    fn field_end(&mut self) {
        self.out.push('\n');
    }

    /// `store_class_begin`: `name = ClassName {` + shift 2.
    pub fn store_class_begin(&mut self, field_name: &str, class_name: &str) {
        self.field_begin(field_name);
        self.out.push_str(class_name);
        self.out.push_str(" {\n");
        self.shift += 2;
    }

    /// `store_class_end`: unshift, close the brace.
    pub fn store_class_end(&mut self) {
        debug_assert!(self.shift >= 2);
        self.shift -= 2;
        for _ in 0..self.shift {
            self.out.push(' ');
        }
        self.out.push_str("}\n");
    }

    /// `store_vector_begin`: `name = vector[N] {` + shift 2 (the closing
    /// brace comes from `store_class_end`, exactly as the C++ generates).
    pub fn store_vector_begin(&mut self, field_name: &str, size: usize) {
        self.field_begin(field_name);
        self.out.push_str(&format!("vector[{size}] {{\n"));
        self.shift += 2;
    }

    /// `store_field(Slice, const string&)`: the value in quotes, raw bytes
    /// (the C++ `sb_ << value` never escapes).
    pub fn store_field_str(&mut self, name: &str, value: &str) {
        self.field_begin(name);
        self.out.push('"');
        self.out.push_str(value);
        self.out.push('"');
        self.field_end();
    }

    /// The `Bool` arm (`sb_ << bool` prints `true`/`false`).
    pub fn store_field_bool(&mut self, name: &str, value: bool) {
        self.field_begin(name);
        self.out.push_str(if value { "true" } else { "false" });
        self.field_end();
    }

    /// The integer arm: plain decimal.
    pub fn store_field_int(&mut self, name: &str, value: i64) {
        self.field_begin(name);
        self.out.push_str(&value.to_string());
        self.field_end();
    }

    /// The `Double` arm: `FixedDouble(x, 6)` — `std::stringstream` with
    /// `std::ios_base::fixed` in the classic locale.
    pub fn store_field_double(&mut self, name: &str, value: f64) {
        self.field_begin(name);
        self.out.push_str(&format!("{value:.6}"));
        self.field_end();
    }

    /// `store_bytes_field`: `bytes [N] { hex... }`, the first 64 bytes
    /// uppercase-hex (each byte followed by a space), `...` when truncated.
    pub fn store_bytes_field(&mut self, name: &str, value: &[u8]) {
        self.field_begin(name);
        self.out.push_str(&format!("bytes [{}] {{ ", value.len()));
        let len = value.len().min(64);
        self.hex_bytes(&value[..len]);
        if len < value.len() {
            self.out.push_str("...");
        }
        self.out.push('}');
        self.field_end();
    }

    /// `store_field(Slice, const UInt128/256/512&)`: the whole word as raw
    /// bytes — `{ AB CD ... }`, no size, no cap.
    pub fn store_field_binary(&mut self, name: &str, value: &[u8]) {
        self.field_begin(name);
        self.out.push_str("{ ");
        self.hex_bytes(value);
        self.out.push('}');
        self.field_end();
    }

    fn hex_bytes(&mut self, value: &[u8]) {
        const HEX: &[u8; 16] = b"0123456789ABCDEF";
        for &b in value {
            self.out.push(HEX[(b >> 4) as usize] as char);
            self.out.push(HEX[(b & 15) as usize] as char);
            self.out.push(' ');
        }
    }

    /// `store_object_field` on a null `object_ptr`: the bare word `null`
    /// (no quotes — the C++ `Slice("null")` overload wins over the quoted
    /// `string` one).
    pub fn store_null(&mut self, name: &str) {
        self.field_begin(name);
        self.out.push_str("null");
        self.field_end();
    }
}
"#;

/// Emits the hand-pinned `TlStorerToString` format goldens (td_api only):
/// every quirk of the C++ debug print, computed by hand from the C++
/// generator sources (tl_writer_cpp.cpp + TlStorerToString.h).
fn emit_tostring_golden_test(out: &mut String) {
    out.push_str(
        r#"#[cfg(test)]
#[test]
fn to_debug_string_matches_the_cpp_tl_storer_to_string_format() {
    // error { code = 400, message = "Wrong" }
    let e = Error { code: 400, message: "Wrong".to_owned() };
    assert_eq!(e.to_debug_string(), "error {\n  code = 400\n  message = \"Wrong\"\n}\n");

    // Strings print RAW — no escaping (the C++ `sb_ << value` in quotes).
    let e = Error { code: 0, message: "a\nb".to_owned() };
    assert_eq!(e.to_debug_string(), "error {\n  code = 0\n  message = \"a\nb\"\n}\n");

    // Bools print true/false (`sb_ << bool`).
    let b = optionValueBoolean { value: true };
    assert_eq!(b.to_debug_string(), "optionValueBoolean {\n  value = true\n}\n");

    // Doubles print 6-decimal fixed (`FixedDouble(x, 6)`).
    let l = Location { latitude: 1.5, longitude: -0.25, horizontal_accuracy: 0.0 };
    assert_eq!(
        l.to_debug_string(),
        "location {\n  latitude = 1.500000\n  longitude = -0.250000\n  horizontal_accuracy = 0.000000\n}\n"
    );

    // Vectors: `vector[N] {`, nested objects with null pointers, the empty
    // inner class on one shifted line.
    let t = TextEntities {
        entities: vec![
            TextEntity { offset: 0, length: 4, r#type: None },
            TextEntity {
                offset: 5,
                length: 9,
                r#type: Some(TextEntityType::textEntityTypeUrl(textEntityTypeUrl)),
            },
        ],
    };
    assert_eq!(
        t.to_debug_string(),
        "textEntities {\n  entities = vector[2] {\n    textEntity {\n      offset = 0\n      length = 4\n      type = null\n    }\n    textEntity {\n      offset = 5\n      length = 9\n      type = textEntityTypeUrl {\n      }\n    }\n  }\n}\n"
    );

    // Bytes: uppercase hex, capped at 64 with an `...` marker.
    let t = TestBytes { value: vec![0xab; 70] };
    let mut expected = String::from("testBytes {\n  value = bytes [70] { ");
    for _ in 0..64 {
        expected.push_str("AB ");
    }
    expected.push_str("...}\n}\n");
    assert_eq!(t.to_debug_string(), expected);

    // The dynamic surfaces (`to_string(const BaseObject&)`, both umbrellas).
    assert_eq!(
        Object::error(e).to_debug_string(),
        "error {\n  code = 0\n  message = \"a\nb\"\n}\n"
    );
    assert_eq!(Function::getMe(getMe).to_debug_string(), "getMe {\n}\n");
}
"#,
    );
}
