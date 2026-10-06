//! Port of `td/e2e`: end-to-end (secret chat) encryption, encrypted storage.
//! The e2e_api data model with TL binary store is generated at build time
//! from the pinned scheme (S13).

// Generated code keeps C++-identical case-preserved names.
#[allow(
    dead_code,
    non_camel_case_types,
    non_snake_case,
    clippy::large_enum_variant
)]
pub mod e2e_api {
    include!("generated/e2e_api.rs");
}

#[cfg(test)]
mod tests {
    use super::e2e_api;

    #[test]
    fn generated_model_counts_match_the_scheme() {
        // Fixed by the S07 differential dump against td's own tl-parser
        // (types / ctors / functions). Structs = non-builtin ctors +
        // functions; enums = multi-ctor types + the Function umbrella.
        assert_eq!(e2e_api::SCHEME_TYPES, 36);
        assert_eq!(e2e_api::SCHEME_CTORS, 48);
        assert_eq!(e2e_api::SCHEME_FUNCTIONS, 1);
        assert_eq!(e2e_api::GENERATED_STRUCTS, 37);
        assert_eq!(e2e_api::GENERATED_ENUMS, 7);
    }
}
