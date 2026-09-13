//! Port of the synchronous half of `td/telegram/OptionManager.{h,cpp}`
//! (S68 subset; the async option storage and the `on_option_updated` switch
//! land with S69).
//!
//! `td_execute` can answer `getOption` only for the two synchronous options
//! (`OptionManager::get_synchronous_options`); everything else is served
//! asynchronously through the Td actor once S69 wires the option storage.

use crate::td_api;

/// The API version, hardcoded like `OptionManager.cpp` ("1.8.67" — pinned
/// to the reference C++ version for JSON parity).
pub const API_VERSION: &str = "1.8.67";

/// `OptionManager::get_synchronous_options`: `{"version", "commit_hash"}`.
pub fn get_synchronous_options() -> &'static [&'static str] {
    &["version", "commit_hash"]
}

/// `OptionManager::is_synchronous_option`.
pub fn is_synchronous_option(name: &str) -> bool {
    get_synchronous_options().contains(&name)
}

/// `td::get_git_commit_hash` — a non-cmake build reports "unknown"; the
/// Rust port's build commit can be injected here once a build-script git
/// probe exists (S79 packaging).
pub fn get_git_commit_hash() -> &'static str {
    "unknown"
}

/// `OptionManager::get_option_synchronously` (the C++ `UNREACHABLE` on an
/// unknown name becomes a debug assert + a `None`).
pub fn get_option_synchronously(name: &str) -> Option<td_api::OptionValue> {
    debug_assert!(!name.is_empty());
    match name {
        "commit_hash" => Some(td_api::OptionValue::optionValueString(
            td_api::optionValueString {
                value: get_git_commit_hash().to_string(),
            },
        )),
        "version" => Some(td_api::OptionValue::optionValueString(
            td_api::optionValueString {
                value: API_VERSION.to_string(),
            },
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synchronous_options_are_version_and_commit_hash() {
        assert!(is_synchronous_option("version"));
        assert!(is_synchronous_option("commit_hash"));
        assert!(!is_synchronous_option("database_directory"));
        assert!(!is_synchronous_option(""));
    }

    #[test]
    fn version_matches_the_reference_cpp_string() {
        let value = get_option_synchronously("version").unwrap();
        assert_eq!(
            value,
            td_api::OptionValue::optionValueString(td_api::optionValueString {
                value: "1.8.67".to_string()
            })
        );
    }

    #[test]
    fn unknown_options_are_not_synchronous() {
        assert!(get_option_synchronously("database_directory").is_none());
    }
}
