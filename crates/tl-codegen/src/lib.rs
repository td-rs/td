//! Port of `td/generate`: TL code generation with `tl_writer_rust`, run
//! from build.rs. The schemes are embedded at their pinned versions
//! (`scheme/sha256sums.txt` records the provenance), so generated code is a
//! pure function of this crate.
//!
//! S09 emits the Rust data model (structs/enums + serde derives); S10 adds
//! the TL binary `store` implementations; td_api JSON (S12) and the
//! remaining schemes' writers (S13) layer on the same [`model::SchemeModel`].

pub mod model;
pub mod rust_writer;

use tdtl::TlProgram;

pub const TD_API_TL: &str = include_str!("../scheme/td_api.tl");
pub const TELEGRAM_API_TL: &str = include_str!("../scheme/telegram_api.tl");

/// Generates the td_api Rust module: structs/enums + serde derives + binary
/// store + the td-exact JSON methods (S12).
pub fn generate_td_api_rust() -> String {
    generate(
        TD_API_TL,
        &rust_writer::WriterOpts {
            umbrella: "Function",
            serde: true,
            json: true,
            source: "td_api.tl (tdlib d1085f9ce)",
        },
    )
}

/// Generates the telegram_api Rust module: structs/enums + binary store
/// (no serde/JSON — telegram_api has no JSON surface).
pub fn generate_telegram_api_rust() -> String {
    generate(
        TELEGRAM_API_TL,
        &rust_writer::WriterOpts {
            umbrella: "Function",
            serde: false,
            json: false,
            source: "telegram_api.tl (tdlib d1085f9ce)",
        },
    )
}

fn generate(scheme: &str, opts: &rust_writer::WriterOpts) -> String {
    let program = TlProgram::parse(scheme.as_bytes().to_vec()).expect("scheme must parse");
    let m = model::SchemeModel::build(&program);
    rust_writer::write_rust(&program, &m, opts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn td_api_counts_match_the_c_oracle() {
        // Fixed by the S07 differential dump (td's own tl-parser): td_api has
        // 753 types / 2192 constructors / 1022 functions.
        let program = TlProgram::parse(TD_API_TL.as_bytes().to_vec()).unwrap();
        assert_eq!(program.total_types, 753);
        assert_eq!(program.total_ctors, 2192);
        assert_eq!(program.total_funcs, 1022);
    }

    #[test]
    fn telegram_api_counts_match_the_c_oracle() {
        // Fixed by the S07 differential dump: 627 types / 1669 ctors / 819
        // functions.
        let program = TlProgram::parse(TELEGRAM_API_TL.as_bytes().to_vec()).unwrap();
        assert_eq!(program.total_types, 627);
        assert_eq!(program.total_ctors, 1669);
        assert_eq!(program.total_funcs, 819);
    }

    #[test]
    fn generated_model_counts_are_consistent() {
        for (scheme, tl) in [("td_api", TD_API_TL), ("telegram_api", TELEGRAM_API_TL)] {
            let program = TlProgram::parse(tl.as_bytes().to_vec()).unwrap();
            let m = model::SchemeModel::build(&program);
            let enums = m
                .type_kinds
                .iter()
                .filter(|k| matches!(k, model::TypeKind::Enum { ctors } if !ctors.is_empty()))
                .count();
            let struct_kinds = m
                .type_kinds
                .iter()
                .filter(|k| matches!(k, model::TypeKind::Struct { .. }))
                .count();
            let prims = m
                .type_kinds
                .iter()
                .filter(|k| matches!(k, model::TypeKind::Prim(_)))
                .count();
            let vectors = m
                .type_kinds
                .iter()
                .filter(|k| matches!(k, model::TypeKind::Vector))
                .count();
            let meta = m
                .type_kinds
                .iter()
                .filter(|k| matches!(k, model::TypeKind::Enum { ctors } if ctors.is_empty()))
                .count();
            assert_eq!(
                prims + vectors + meta + enums + struct_kinds,
                program.total_types,
                "{scheme} type accounting"
            );
            let code = match scheme {
                "td_api" => generate_td_api_rust(),
                _ => generate_telegram_api_rust(),
            };
            // One struct per non-builtin ctor (which includes every
            // single-ctor type) plus one per function; one enum per
            // multi-ctor type plus the Function umbrella. The support
            // module's `tl::Reader` adds exactly one more struct; the JSON
            // runtime (td_api only) adds the `tl_json::Json` enum.
            let json_enums = if scheme == "td_api" { 1 } else { 0 };
            assert_eq!(
                code.matches("pub struct ").count(),
                (program.total_ctors - 9) + program.total_funcs + 1,
                "{scheme} struct count"
            );
            assert_eq!(
                code.matches("pub enum ").count(),
                enums + 1 + json_enums,
                "{scheme} enum count"
            ); // + Function + Json
        }
    }

    #[test]
    fn generated_code_has_no_name_collisions() {
        // write_rust asserts uniqueness while emitting; just make it run.
        let _ = generate_td_api_rust();
        let _ = generate_telegram_api_rust();
    }
}
