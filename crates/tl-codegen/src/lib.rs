//! Port of `td/generate`: TL code generation with `tl_writer_rust`, run
//! from build.rs. The schemes are embedded at their pinned versions
//! (`scheme/sha256sums.txt` records the provenance), so generated code is a
//! pure function of this crate.
//!
//! S09 emits the Rust data model (structs/enums + serde derives); binary
//! serialization (S10/S11), td_api JSON (S12) and the remaining schemes'
//! writers (S13) layer on the same [`model::SchemeModel`].

pub mod model;
pub mod rust_writer;

use tdtl::TlProgram;

pub const TD_API_TL: &str = include_str!("../scheme/td_api.tl");

/// Generates the td_api Rust module (S09): structs/enums + serde derives.
pub fn generate_td_api_rust() -> String {
    let program =
        TlProgram::parse(TD_API_TL.as_bytes().to_vec()).expect("embedded td_api.tl must parse");
    let m = model::SchemeModel::build(&program);
    rust_writer::write_rust(&program, &m, "Function", "td_api.tl (tdlib d1085f9ce)")
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
    fn generated_model_counts_are_consistent() {
        let program = TlProgram::parse(TD_API_TL.as_bytes().to_vec()).unwrap();
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
        // 753 = 2 meta (#, Type) + 8 builtin simple types + enums + structs.
        assert_eq!(2 + 8 + enums + struct_kinds, program.total_types);
        let code = generate_td_api_rust();
        // One struct per non-builtin ctor (which includes every single-ctor
        // type) plus one per function; one enum per multi-ctor type plus the
        // Function umbrella.
        assert_eq!(
            code.matches("pub struct ").count(),
            (program.total_ctors - 9) + program.total_funcs
        );
        assert_eq!(code.matches("pub enum ").count(), enums + 1); // + Function
    }

    #[test]
    fn generated_code_has_no_name_collisions() {
        // write_rust asserts uniqueness while emitting; just make it run.
        let _ = generate_td_api_rust();
    }
}
