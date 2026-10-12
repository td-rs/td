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
pub const SECRET_API_TL: &str = include_str!("../scheme/secret_api.tl");
pub const MTPROTO_API_TL: &str = include_str!("../scheme/mtproto_api.tl");
pub const E2E_API_TL: &str = include_str!("../scheme/e2e_api.tl");

/// Generates the td_api Rust module: structs/enums + serde derives + binary
/// store + the td-exact JSON methods (S12).
pub fn generate_td_api_rust() -> String {
    generate(
        TD_API_TL,
        &rust_writer::WriterOpts {
            umbrella: "Function",
            serde: true,
            json: true,
            tostring: true,
            tostring_golden: true,
            samples_feature: None,
            raw_strings: false,
            source: "td_api.tl (tdlib c15d3f5a)",
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
            tostring: true,
            tostring_golden: false,
            samples_feature: Some("conformance-samples"),
            raw_strings: false,
            source: "telegram_api.tl (tdlib c15d3f5a)",
        },
    )
}

/// Generates the secret_api Rust module (S13): structs/enums + binary
/// store/parse — the secret-chat layer scheme. Raw strings: the C++ parses
/// this scheme with the unsanitizing `TlParser`.
pub fn generate_secret_api_rust() -> String {
    generate(
        SECRET_API_TL,
        &rust_writer::WriterOpts {
            umbrella: "Function",
            serde: false,
            json: false,
            tostring: false,
            tostring_golden: false,
            samples_feature: Some("conformance-samples"),
            raw_strings: true,
            source: "secret_api.tl (tdlib c15d3f5a)",
        },
    )
}

/// Generates the mtproto_api Rust module (S13): structs/enums + binary
/// store/parse — the MTProto service-layer scheme. Raw strings: the C++
/// parses this scheme with the unsanitizing `TlParser`.
pub fn generate_mtproto_api_rust() -> String {
    generate(
        MTPROTO_API_TL,
        &rust_writer::WriterOpts {
            umbrella: "Function",
            serde: false,
            json: false,
            tostring: false,
            tostring_golden: false,
            samples_feature: Some("conformance-samples"),
            raw_strings: true,
            source: "mtproto_api.tl (tdlib c15d3f5a)",
        },
    )
}

/// Generates the e2e_api Rust module (S13): structs/enums + binary
/// store/parse — the tde2e encrypted-storage scheme. Raw strings: the C++
/// parses this scheme with the unsanitizing `TlParser`.
pub fn generate_e2e_api_rust() -> String {
    generate(
        E2E_API_TL,
        &rust_writer::WriterOpts {
            umbrella: "Function",
            serde: false,
            json: false,
            tostring: false,
            tostring_golden: false,
            samples_feature: Some("conformance-samples"),
            raw_strings: true,
            source: "e2e_api.tl (tdlib c15d3f5a)",
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
    fn mtproto_scheme_uses_raw_string_fields() {
        // The mtproto/secret/e2e schemes are parsed by the unsanitizing
        // C++ `TlParser`; binary string fields (pq, dh_prime, g_a) must be
        // `Vec<u8>`, not the sanitizing `String`.
        let code = generate_mtproto_api_rust();
        assert!(code.contains("pub pq: Vec<u8>"), "pq is not raw bytes");
        assert!(!code.contains("pub pq: String"));
        let code = generate_telegram_api_rust();
        assert!(code.contains("pub phone_number: String") || code.contains("phone_number: String"));
    }

    #[test]
    fn td_api_counts_match_the_c_oracle() {
        // Fixed by the S07 differential dump (td's own tl-parser), re-run at
        // the c15d3f5a re-pin (6e2b9aa): 792 types / 2260 constructors /
        // 1065 functions. Refresh on every scheme re-pin (RELEASE.md §1-2).
        let program = TlProgram::parse(TD_API_TL.as_bytes().to_vec()).unwrap();
        assert_eq!(program.total_types, 792);
        assert_eq!(program.total_ctors, 2260);
        assert_eq!(program.total_funcs, 1065);
    }

    #[test]
    fn telegram_api_counts_match_the_c_oracle() {
        // Fixed by the S07 differential dump, re-run at the c15d3f5a re-pin
        // (6e2b9aa): 660 types / 1715 ctors / 856 functions.
        let program = TlProgram::parse(TELEGRAM_API_TL.as_bytes().to_vec()).unwrap();
        assert_eq!(program.total_types, 660);
        assert_eq!(program.total_ctors, 1715);
        assert_eq!(program.total_funcs, 856);
    }

    #[test]
    fn remaining_schemes_match_the_c_oracle_dumps() {
        // Counts fixed by the S07 differential dump (tl-scheme-dump vs the
        // C oracle's dump_scheme over the pinned schemes).
        for (tl, types, ctors, funcs) in [
            (SECRET_API_TL, 20, 94, 1),
            (MTPROTO_API_TL, 33, 40, 8),
            (E2E_API_TL, 36, 48, 1),
        ] {
            let program = TlProgram::parse(tl.as_bytes().to_vec()).unwrap();
            assert_eq!(program.total_types, types);
            assert_eq!(program.total_ctors, ctors);
            assert_eq!(program.total_funcs, funcs);
        }
    }

    #[test]
    fn generated_model_counts_are_consistent() {
        let schemes: &[(&str, &str)] = &[
            ("td_api", TD_API_TL),
            ("telegram_api", TELEGRAM_API_TL),
            ("secret_api", SECRET_API_TL),
            ("mtproto_api", MTPROTO_API_TL),
            ("e2e_api", E2E_API_TL),
        ];
        for &(scheme, tl) in schemes {
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
                "telegram_api" => generate_telegram_api_rust(),
                "secret_api" => generate_secret_api_rust(),
                "mtproto_api" => generate_mtproto_api_rust(),
                _ => generate_e2e_api_rust(),
            };
            // One struct per non-builtin ctor (which includes every
            // single-ctor type) plus one per function; one enum per
            // multi-ctor type plus the Function umbrella. The support
            // module's `tl::Reader` adds exactly one more struct; the JSON
            // runtime (td_api only) adds the `tl_json::Json` and `Object`
            // enums.
            // Builtin ctors = those owned by Prim/Vector types (the count
            // differs per scheme).
            let builtin_ctors: usize = m
                .type_kinds
                .iter()
                .zip(program.types.iter())
                .filter(|(k, _)| {
                    matches!(k, model::TypeKind::Prim(_)) || matches!(k, model::TypeKind::Vector)
                })
                .map(|(_, t)| t.constructors.len())
                .sum();
            let json_enums = if scheme == "td_api" { 2 } else { 0 };
            // The `TlStorerToString` support struct (the C++ debug
            // `to_string` storer, M.14.2) adds one more struct alongside
            // `tl::Reader` in every scheme generated with tostring (td_api
            // and — since the receive-pipeline dumps — telegram_api).
            let tostring_struct = usize::from(matches!(scheme, "td_api" | "telegram_api"));
            assert_eq!(
                code.matches("pub struct ").count(),
                (program.total_ctors - builtin_ctors) + program.total_funcs + 1 + tostring_struct,
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
        let _ = generate_secret_api_rust();
        let _ = generate_mtproto_api_rust();
        let _ = generate_e2e_api_rust();
    }
}
