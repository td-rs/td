//! Port of `td/telegram/LanguagePackManager.{h,cpp}` (M.24) — the
//! language-pack record planes, the per-language sqlite key-value
//! database plane, the custom-language-pack local machinery and the
//! difference-intake machine.
//!
//! The manager is the collapsed actor: `Td` owns the struct, the
//! [`LanguagePackManagerEnv`] trait carries the option/update surfaces,
//! and every `create_handler<...>`/`send_with_promise` network tail is an
//! env seam marked `[blocked: net]` — the C++ promise lands in the query
//! (or in the manager's pending-query maps) and stays unanswered offline,
//! exactly like the collapsed-line convention.
//!
//! Database plane: the C++ keeps one `LanguageDatabase` per
//! `language_pack_database_path` in a process-static registry
//! (`language_databases_`, `language_database_mutex_`) so the synchronous
//! `td_execute getLanguagePackString` can open a database without a Td.
//! The Rust port splits the two halves: the manager owns its database as
//! a plain field (the single-Td collapse — the C++ mutexes only guard
//! cross-thread access), while [`get_language_pack_string`] — the
//! `SynchronousRequests::do_request` tail — keeps the path-keyed static
//! registry ([`SYNC_LANGUAGE_DATABASES`]). Both sides open the same file
//! in WAL mode and write through committed transactions, so the
//! observable byte content of every `kv_<pack>_<code>` table matches the
//! C++ exactly (documented divergence: two connections instead of one
//! shared `SqliteDb` handle).
//!
//! The wire intakes (`on_get_language_pack_strings`,
//! `on_update_language_pack`, `on_get_languages`, `on_get_language`) are
//! fully portable and unit-tested against the wire shapes; the net query
//! tails (`langpack_getLanguages`, `langpack_getLanguage`,
//! `langpack_getLangPack`, `langpack_getStrings`,
//! `langpack_getDifference`) are the env seams.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::td_api;
use tddb::db_key::DbKey;
use tddb::sqlite_db::SqliteDb;
use tddb::sqlite_key_value::SqliteKeyValue;
use tdmtproto::telegram_api;
use tdutils::promise::Promise;
use tdutils::status::{Result, Status};

// ===== the character checks (tdutils/misc.h) =====

/// `td::is_alpha` (tdutils/misc.h:113).
fn is_alpha(c: u8) -> bool {
    (c | 0x20).is_ascii_lowercase()
}

/// `td::is_digit` (tdutils/misc.h:110).
fn is_digit(c: u8) -> bool {
    c.is_ascii_digit()
}

/// `td::is_alnum` (tdutils/misc.h:116).
fn is_alnum(c: u8) -> bool {
    is_alpha(c) || is_digit(c)
}

/// `td::to_lower_inplace` (tdutils/misc.h:64-85) — ASCII-only.
fn to_lower_inplace(s: &mut String) {
    *s = s.chars().map(|c| c.to_ascii_lowercase()).collect();
}

/// `td::to_integer<int32>` (tdutils/misc.h) — the lenient leading-digits
/// parse (`0` on garbage, like the C++).
fn to_integer_i32(s: &str) -> i32 {
    let bytes = s.as_bytes();
    let mut pos = 0;
    while pos < bytes.len() && (bytes[pos] == b' ' || bytes[pos] == b'\t') {
        pos += 1;
    }
    let mut negative = false;
    if pos < bytes.len() && (bytes[pos] == b'+' || bytes[pos] == b'-') {
        negative = bytes[pos] == b'-';
        pos += 1;
    }
    let mut value: i64 = 0;
    while pos < bytes.len() && bytes[pos].is_ascii_digit() {
        value = value * 10 + i64::from(bytes[pos] - b'0');
        if value > i32::MAX as i64 + 1 {
            value = i32::MAX as i64 + 1;
        }
        pos += 1;
    }
    let value = if negative { -value } else { value };
    value.clamp(i32::MIN as i64, i32::MAX as i64) as i32
}

/// `full_split(data, '\0')` (tdutils/algorithm.h) — empty pieces kept.
fn full_split(data: &str, sep: u8) -> Vec<String> {
    data.as_bytes()
        .split(|&c| c == sep)
        .map(|piece| String::from_utf8_lossy(piece).into_owned())
        .collect()
}

/// `implode(strings, '\0')` (tdutils/algorithm.h).
fn implode(strings: &[String], sep: u8) -> String {
    let mut out = Vec::new();
    for (i, piece) in strings.iter().enumerate() {
        if i != 0 {
            out.push(sep);
        }
        out.extend_from_slice(piece.as_bytes());
    }
    String::from_utf8(out).unwrap_or_default()
}

// ===== the static checks (LanguagePackManager.cpp:108-128, 1293-1300) =====

/// `LanguagePackManager::check_language_pack_name` (cpp:108-115).
pub fn check_language_pack_name(name: &str) -> bool {
    for &c in name.as_bytes() {
        if c != b'_' && !is_alpha(c) {
            return false;
        }
    }
    name.len() <= 64
}

/// `LanguagePackManager::check_language_code_name` (cpp:117-124).
pub fn check_language_code_name(name: &str) -> bool {
    for &c in name.as_bytes() {
        if c != b'-' && !is_alpha(c) && !is_digit(c) {
            return false;
        }
    }
    name.len() <= 64 && (is_custom_language_code(name) || name.is_empty() || name.len() >= 2)
}

/// `LanguagePackManager::is_custom_language_code` (cpp:126-128).
pub fn is_custom_language_code(language_code: &str) -> bool {
    !language_code.is_empty() && language_code.as_bytes()[0] == b'X'
}

/// `LanguagePackManager::is_valid_key` (cpp:1293-1300).
pub fn is_valid_key(key: &str) -> bool {
    for &c in key.as_bytes() {
        if !is_alnum(c) && c != b'_' && c != b'.' && c != b'-' {
            return false;
        }
    }
    !key.is_empty()
}

// ===== LanguageInfo (LanguagePackManager.cpp:68-88) =====

/// `LanguagePackManager::LanguageInfo`. The C++ `operator==` (cpp:81-87)
/// deliberately skips `is_from_database_`, so the derived `PartialEq`
/// keeps that field out of the comparison the same way.
#[derive(Clone, Debug, Default)]
pub struct LanguageInfo {
    pub name: String,
    pub native_name: String,
    pub base_language_code: String,
    pub plural_code: String,
    pub is_official: bool,
    pub is_rtl: bool,
    pub is_beta: bool,
    pub is_from_database: bool,
    pub total_string_count: i32,
    pub translated_string_count: i32,
    pub translation_url: String,
}

impl PartialEq for LanguageInfo {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.native_name == other.native_name
            && self.base_language_code == other.base_language_code
            && self.plural_code == other.plural_code
            && self.is_official == other.is_official
            && self.is_rtl == other.is_rtl
            && self.is_beta == other.is_beta
            && self.total_string_count == other.total_string_count
            && self.translated_string_count == other.translated_string_count
            && self.translation_url == other.translation_url
    }
}

impl LanguageInfo {
    /// The `as_bool` lambda (cpp:584-590).
    fn bool_from_database(data: &str) -> bool {
        if data == "true" {
            return true;
        }
        if data != "false" {
            tdutils::log_error!("Have invalid boolean value \"{data}\" in the database");
        }
        false
    }

    /// `get_language_info_string` (cpp:930-935) — the ten \x00-separated
    /// fields; the C++ `operator<<` prints bools as `1`/`0`.
    pub fn get_language_info_string(&self) -> String {
        let mut out = Vec::new();
        let mut push_field = |field: &str| {
            if !out.is_empty() {
                out.push(b'\0');
            }
            out.extend_from_slice(field.as_bytes());
        };
        push_field(&self.name);
        push_field(&self.native_name);
        push_field(&self.base_language_code);
        push_field(&self.plural_code);
        push_field(if self.is_official { "1" } else { "0" });
        push_field(if self.is_rtl { "1" } else { "0" });
        push_field(if self.is_beta { "1" } else { "0" });
        push_field(&self.total_string_count.to_string());
        push_field(&self.translated_string_count.to_string());
        push_field(&self.translation_url);
        String::from_utf8(out).unwrap_or_default()
    }

    /// The per-key custom language pack info form (cpp:626-646): 2
    /// fields (`name`, `native_name`) or all 10; `is_from_database` is
    /// always set on a successful parse.
    pub fn parse_custom(data: &str) -> Option<LanguageInfo> {
        let all_infos = full_split(data, b'\0');
        if all_infos.len() < 2 {
            tdutils::log_error!("Have wrong custom language pack info \"{data}\"");
            return None;
        }
        let mut info = LanguageInfo {
            name: all_infos[0].clone(),
            native_name: all_infos[1].clone(),
            ..Default::default()
        };
        if all_infos.len() > 2 {
            // the C++ CHECK(all_infos.size() == 10); the writer only ever
            // produces the 10-field form
            assert_eq!(all_infos.len(), 10, "custom language pack info size");
            info.base_language_code = all_infos[2].clone();
            info.plural_code = all_infos[3].clone();
            info.is_official = Self::bool_from_database(&all_infos[4]);
            info.is_rtl = Self::bool_from_database(&all_infos[5]);
            info.is_beta = Self::bool_from_database(&all_infos[6]);
            info.total_string_count = to_integer_i32(&all_infos[7]);
            info.translated_string_count = to_integer_i32(&all_infos[8]);
            info.translation_url = all_infos[9].clone();
        }
        info.is_from_database = true;
        Some(info)
    }

    /// The `!server2` form (cpp:597-624): `code \x00 info` pairs, 11
    /// fields per language.
    pub fn parse_server_infos(data: &str) -> Vec<(String, LanguageInfo)> {
        let mut result = Vec::new();
        let all_infos = full_split(data, b'\0');
        if !all_infos.len().is_multiple_of(11) {
            tdutils::log_error!("Have wrong language pack info \"{data}\" in the database");
            return result;
        }
        let mut i = 0;
        while i < all_infos.len() {
            if all_infos[i].is_empty() {
                tdutils::log_error!("Have empty info about a language pack");
            } else {
                let mut info = LanguageInfo {
                    name: all_infos[i + 1].clone(),
                    native_name: all_infos[i + 2].clone(),
                    base_language_code: all_infos[i + 3].clone(),
                    plural_code: all_infos[i + 4].clone(),
                    is_official: Self::bool_from_database(&all_infos[i + 5]),
                    is_rtl: Self::bool_from_database(&all_infos[i + 6]),
                    is_beta: Self::bool_from_database(&all_infos[i + 7]),
                    is_from_database: true,
                    total_string_count: to_integer_i32(&all_infos[i + 8]),
                    translated_string_count: to_integer_i32(&all_infos[i + 9]),
                    translation_url: all_infos[i + 10].clone(),
                };
                info.is_from_database = true;
                result.push((all_infos[i].clone(), info));
            }
            i += 11;
        }
        result
    }
}

// ===== PluralizedString (LanguagePackManager.cpp:34-51) =====

/// `LanguagePackManager::PluralizedString`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PluralizedString {
    pub zero_value: String,
    pub one_value: String,
    pub two_value: String,
    pub few_value: String,
    pub many_value: String,
    pub other_value: String,
}

impl PluralizedString {
    /// `new PluralizedString(...)` from the six wire fields.
    fn from_parts(
        zero_value: String,
        one_value: String,
        two_value: String,
        few_value: String,
        many_value: String,
        other_value: String,
    ) -> PluralizedString {
        PluralizedString {
            zero_value,
            one_value,
            two_value,
            few_value,
            many_value,
            other_value,
        }
    }
}

// ===== the td_api builders (LanguagePackManager.cpp:789-858, 922-928,
// ===== 1193-1212) =====

/// `get_language_pack_string_value_object(const string &value)` (cpp:789).
fn ordinary_string_value_object(value: &str) -> td_api::LanguagePackStringValue {
    td_api::LanguagePackStringValue::languagePackStringValueOrdinary(
        td_api::languagePackStringValueOrdinary {
            value: value.to_owned(),
        },
    )
}

/// `get_language_pack_string_value_object(const PluralizedString &)`
/// (cpp:794).
fn pluralized_string_value_object(value: &PluralizedString) -> td_api::LanguagePackStringValue {
    td_api::LanguagePackStringValue::languagePackStringValuePluralized(
        td_api::languagePackStringValuePluralized {
            zero_value: value.zero_value.clone(),
            one_value: value.one_value.clone(),
            two_value: value.two_value.clone(),
            few_value: value.few_value.clone(),
            many_value: value.many_value.clone(),
            other_value: value.other_value.clone(),
        },
    )
}

/// `get_language_pack_string_value_object()` (cpp:800).
fn deleted_string_value_object() -> td_api::LanguagePackStringValue {
    td_api::LanguagePackStringValue::languagePackStringValueDeleted(
        td_api::languagePackStringValueDeleted {},
    )
}

/// `copy_language_pack_string_value` (cpp:1193-1212). Reached via
/// `on_get_all_language_pack_strings` — the net response half (tests
/// drive it through the fan-out).
#[allow(dead_code)]
fn copy_language_pack_string_value(
    value: &td_api::LanguagePackStringValue,
) -> td_api::LanguagePackStringValue {
    use td_api::LanguagePackStringValue as V;
    match value {
        V::languagePackStringValueOrdinary(v) => {
            V::languagePackStringValueOrdinary(td_api::languagePackStringValueOrdinary {
                value: v.value.clone(),
            })
        }
        V::languagePackStringValuePluralized(v) => {
            V::languagePackStringValuePluralized(td_api::languagePackStringValuePluralized {
                zero_value: v.zero_value.clone(),
                one_value: v.one_value.clone(),
                two_value: v.two_value.clone(),
                few_value: v.few_value.clone(),
                many_value: v.many_value.clone(),
                other_value: v.other_value.clone(),
            })
        }
        V::languagePackStringValueDeleted(_) => deleted_string_value_object(),
    }
}

/// `get_language_pack_string_object(const string &key, const string
/// &value)` (cpp:804).
fn get_language_pack_string_object_ordinary(key: &str, value: &str) -> td_api::LanguagePackString {
    td_api::LanguagePackString {
        key: key.to_owned(),
        value: Some(ordinary_string_value_object(value)),
    }
}

/// `get_language_pack_string_object(const string &key, const
/// PluralizedString &value)` (cpp:809).
fn get_language_pack_string_object_pluralized(
    key: &str,
    value: &PluralizedString,
) -> td_api::LanguagePackString {
    td_api::LanguagePackString {
        key: key.to_owned(),
        value: Some(pluralized_string_value_object(value)),
    }
}

/// `get_language_pack_string_object(const string &key)` (cpp:814).
fn get_language_pack_string_object_deleted(key: &str) -> td_api::LanguagePackString {
    td_api::LanguagePackString {
        key: key.to_owned(),
        value: Some(deleted_string_value_object()),
    }
}

/// `get_language_pack_info_object` (cpp:922-928) — `is_installed` is
/// always `false` and `local_string_count` always `0` here; the callers
/// fix both up.
pub fn get_language_pack_info_object(
    language_code: &str,
    info: &LanguageInfo,
) -> td_api::LanguagePackInfo {
    td_api::LanguagePackInfo {
        id: language_code.to_owned(),
        base_language_pack_id: info.base_language_code.clone(),
        name: info.name.clone(),
        native_name: info.native_name.clone(),
        plural_code: info.plural_code.clone(),
        is_official: info.is_official,
        is_rtl: info.is_rtl,
        is_beta: info.is_beta,
        is_installed: false,
        total_string_count: info.total_string_count,
        translated_string_count: info.translated_string_count,
        local_string_count: 0,
        translation_url: info.translation_url.clone(),
    }
}

// ===== the wire/td_api LanguageInfo converters (cpp:1609-1705) =====

/// `get_language_info(telegram_api::langPackLanguage *language)`
/// (cpp:1609-1647). The C++ lowercases `language->lang_code_` in place
/// and the callers read it afterwards, so the lowered code comes back as
/// the tuple's first element.
pub fn get_language_info_from_wire(
    mut language: telegram_api::LangPackLanguage,
) -> Result<(String, LanguageInfo)> {
    if !check_language_code_name(&language.lang_code) || language.lang_code.is_empty() {
        tdutils::log_error!(
            "Receive unsupported language pack ID {} from server",
            language.lang_code
        );
        return Result::Err(Status::error(500, "Unsupported language pack ID"));
    }
    if is_custom_language_code(&language.lang_code) {
        tdutils::log_error!(
            "Receive custom language pack ID \"{}\" from server",
            language.lang_code
        );
        return Result::Err(Status::error(500, "Unallowed custom language pack ID"));
    }
    to_lower_inplace(&mut language.lang_code);

    let mut info = LanguageInfo {
        name: language.name,
        native_name: language.native_name,
        base_language_code: language.base_lang_code,
        plural_code: language.plural_code,
        is_official: language.official,
        is_rtl: language.rtl,
        is_beta: language.beta,
        is_from_database: false,
        total_string_count: language.strings_count,
        translated_string_count: language.translated_count,
        translation_url: language.translations_url,
    };

    if !check_language_code_name(&info.base_language_code) {
        tdutils::log_error!(
            "Have invalid base language pack ID \"{}\"",
            info.base_language_code
        );
        info.base_language_code.clear();
    }
    if is_custom_language_code(&info.base_language_code) {
        tdutils::log_error!(
            "Receive custom base language pack ID \"{}\" from server",
            info.base_language_code
        );
        info.base_language_code.clear();
    }
    if info.base_language_code == language.lang_code {
        tdutils::log_error!(
            "Receive language pack \"{}\"based on self",
            info.base_language_code
        );
        info.base_language_code.clear();
    }
    Result::Ok((language.lang_code, info))
}

/// `get_language_info(td_api::languagePackInfo *language_pack_info)`
/// (cpp:1649-1705). The C++ cleans the strings in place, clamps the
/// negative counters and sanitizes custom packs before moving the fields
/// out; the id comes back as the tuple's first element.
pub fn get_language_info_from_td_api(
    mut language_pack_info: td_api::LanguagePackInfo,
) -> Result<(String, LanguageInfo)> {
    macro_rules! clean_or_fail {
        ($field:expr, $message:expr) => {
            match crate::misc::clean_input_string(&$field) {
                Some(cleaned) => $field = cleaned,
                None => return Result::Err(Status::error(400, $message)),
            }
        };
    }
    clean_or_fail!(
        language_pack_info.id,
        "Language pack ID must be encoded in UTF-8"
    );
    clean_or_fail!(
        language_pack_info.base_language_pack_id,
        "Base language pack ID must be encoded in UTF-8"
    );
    clean_or_fail!(
        language_pack_info.name,
        "Language pack name must be encoded in UTF-8"
    );
    clean_or_fail!(
        language_pack_info.native_name,
        "Language pack native name must be encoded in UTF-8"
    );
    clean_or_fail!(
        language_pack_info.plural_code,
        "Language pack plural code must be encoded in UTF-8"
    );
    clean_or_fail!(
        language_pack_info.translation_url,
        "Language pack translation URL must be encoded in UTF-8"
    );
    if language_pack_info.total_string_count < 0 {
        language_pack_info.total_string_count = 0;
    }
    if language_pack_info.translated_string_count < 0 {
        language_pack_info.translated_string_count = 0;
    }
    if !check_language_code_name(&language_pack_info.id) {
        return Result::Err(Status::error(
            400,
            "Language pack ID must contain only letters, digits and hyphen",
        ));
    }

    if is_custom_language_code(&language_pack_info.id) {
        language_pack_info.base_language_pack_id.clear();
        language_pack_info.is_official = false;
        language_pack_info.is_rtl = false;
        language_pack_info.is_beta = false;
        language_pack_info.translation_url.clear();
    }

    let info = LanguageInfo {
        name: language_pack_info.name,
        native_name: language_pack_info.native_name,
        base_language_code: language_pack_info.base_language_pack_id,
        plural_code: language_pack_info.plural_code,
        is_official: language_pack_info.is_official,
        is_rtl: language_pack_info.is_rtl,
        is_beta: language_pack_info.is_beta,
        is_from_database: true,
        total_string_count: language_pack_info.total_string_count,
        translated_string_count: language_pack_info.translated_string_count,
        translation_url: language_pack_info.translation_url,
    };
    Result::Ok((language_pack_info.id, info))
}

/// `convert_to_telegram_api` (cpp:1567-1607). The C++ nullptr-`str` band
/// ("Language pack strings must be non-empty") is unrepresentable here —
/// the Rust parameter is owned, never null.
pub fn convert_to_telegram_api(
    str: td_api::LanguagePackString,
) -> Result<telegram_api::LangPackString> {
    let key = str.key;
    if !is_valid_key(&key) {
        return Result::Err(Status::error(400, "Key is invalid"));
    }

    let Some(value) = str.value else {
        // str->value_ == nullptr
        return Result::Ok(telegram_api::LangPackString::langPackStringDeleted(
            telegram_api::langPackStringDeleted { key },
        ));
    };
    use td_api::LanguagePackStringValue as V;
    match value {
        V::languagePackStringValueOrdinary(value) => {
            let Some(cleaned) = crate::misc::clean_input_string(&value.value) else {
                return Result::Err(Status::error(
                    400,
                    "Language pack string value must be encoded in UTF-8",
                ));
            };
            Result::Ok(telegram_api::LangPackString::langPackString(
                telegram_api::langPackString {
                    key,
                    value: cleaned,
                },
            ))
        }
        V::languagePackStringValuePluralized(value) => {
            for field in [
                &value.zero_value,
                &value.one_value,
                &value.two_value,
                &value.few_value,
                &value.many_value,
                &value.other_value,
            ] {
                if crate::misc::clean_input_string(field).is_none() {
                    return Result::Err(Status::error(
                        400,
                        "Language pack string value must be encoded in UTF-8",
                    ));
                }
            }
            // the C++ passes 31 as the explicit flags value — all five
            // optional strings set
            Result::Ok(telegram_api::LangPackString::langPackStringPluralized(
                telegram_api::langPackStringPluralized {
                    flags: 31,
                    key,
                    zero_value: value.zero_value,
                    one_value: value.one_value,
                    two_value: value.two_value,
                    few_value: value.few_value,
                    many_value: value.many_value,
                    other_value: value.other_value,
                },
            ))
        }
        V::languagePackStringValueDeleted(_) => {
            // there is no reason to save deleted strings in a custom
            // language pack to database
            Result::Ok(telegram_api::LangPackString::langPackStringDeleted(
                telegram_api::langPackStringDeleted { key },
            ))
        }
    }
}

// ===== the database plane (LanguagePackManager.cpp:34-104, 130-198,
// ===== 542-787, 1302-1345) =====

/// `get_database_table_name` (cpp:566-568) — the SQL identifier keeps the
/// C++ double quotes.
fn get_database_table_name(language_pack: &str, language_code: &str) -> String {
    format!("\"kv_{language_pack}_{language_code}\"")
}

/// `open_database` (cpp:130-134).
fn open_database(path: &str) -> Result<SqliteDb> {
    let database = match SqliteDb::open_with_key(path, true, &DbKey::empty(), None) {
        Result::Ok(database) => database,
        error @ Result::Err(_) => return error,
    };
    let status = database.exec("PRAGMA journal_mode=WAL");
    if status.is_error() {
        return Result::Err(status);
    }
    Result::Ok(database)
}

/// `load_database_language_version` (cpp:136-147).
fn load_database_language_version(kv: &mut SqliteKeyValue) -> i32 {
    if kv.empty() {
        return -1;
    }
    let str_version = String::from_utf8_lossy(&kv.get(b"!version")).into_owned();
    if str_version.is_empty() {
        return -1;
    }
    to_integer_i32(&str_version)
}

/// `load_database_language_key_count` (cpp:149-167).
fn load_database_language_key_count(kv: &mut SqliteKeyValue) -> i32 {
    if kv.empty() {
        return 0;
    }
    let str_key_count = String::from_utf8_lossy(&kv.get(b"!key_count")).into_owned();
    if str_key_count.is_empty() {
        // calculate key count once for the database and cache it
        let mut key_count = 0;
        for (key, value) in kv.get_all() {
            if key.first() != Some(&b'!') && matches!(value.first(), Some(&b'1') | Some(&b'2')) {
                key_count += 1;
            }
        }
        tdutils::log_info!("Set language pack key count in database to {key_count}");
        kv.set(b"!key_count", key_count.to_string().as_bytes());
        return key_count;
    }
    to_integer_i32(&str_key_count)
}

/// `load_database_language_base_language_code` (cpp:169-175).
fn load_database_language_base_language_code(kv: &mut SqliteKeyValue) -> String {
    if kv.empty() {
        return String::new();
    }
    String::from_utf8_lossy(&kv.get(b"!base_language_code")).into_owned()
}

/// `LanguagePackManager::Language` (cpp:53-66). The C++ mutex/atomics
/// guard cross-actor access; the collapsed manager is single-threaded.
pub struct Language {
    pub(crate) version: i32,
    pub(crate) key_count: i32,
    pub(crate) base_language_code: String,
    pub(crate) is_full: bool,
    pub(crate) was_loaded_full: bool,
    pub(crate) has_get_difference_query: bool,
    pub(crate) get_difference_queries: Vec<Promise<()>>,
    pub(crate) ordinary_strings: HashMap<String, String>,
    pub(crate) pluralized_strings: HashMap<String, PluralizedString>,
    pub(crate) deleted_strings: HashSet<String>,
    pub(crate) kv: SqliteKeyValue,
}

impl Default for Language {
    fn default() -> Self {
        Self::new()
    }
}

impl Language {
    /// The C++ member defaults (cpp:55-59).
    pub fn new() -> Language {
        Language {
            version: -1,
            key_count: 0,
            base_language_code: String::new(),
            is_full: false,
            was_loaded_full: false,
            has_get_difference_query: false,
            get_difference_queries: Vec::new(),
            ordinary_strings: HashMap::new(),
            pluralized_strings: HashMap::new(),
            deleted_strings: HashSet::new(),
            kv: SqliteKeyValue::new(),
        }
    }

    /// `language_has_string_unsafe` (cpp:676-679).
    pub(crate) fn language_has_string_unsafe(&self, key: &str) -> bool {
        self.ordinary_strings.contains_key(key)
            || self.pluralized_strings.contains_key(key)
            || self.deleted_strings.contains(key)
    }

    /// `language_has_strings` (cpp:681-699) — the null-`language` band
    /// lives at the call sites.
    pub(crate) fn language_has_strings(&self, keys: &[String]) -> bool {
        if self.is_full {
            return true;
        }
        if keys.is_empty() {
            return false; // language is already checked to be not full
        }
        for key in keys {
            if !self.language_has_string_unsafe(key) {
                return false;
            }
        }
        true
    }

    /// `load_language_string_unsafe` (cpp:701-722) — the byte-exact
    /// `'1'`/`'2'`+six-`'\x00'`-parts/`"3"` value encodings.
    pub(crate) fn load_language_string_unsafe(&mut self, key: &str, value: &str) {
        assert!(is_valid_key(key), "invalid language pack key {key:?}");
        let bytes = value.as_bytes();
        if bytes.first() == Some(&b'1') {
            self.ordinary_strings
                .insert(key.to_owned(), value[1..].to_owned());
            return;
        }

        if bytes.first() == Some(&b'2') {
            let all = full_split(&value[1..], b'\0');
            if all.len() == 6 {
                self.pluralized_strings.insert(
                    key.to_owned(),
                    PluralizedString::from_parts(
                        all[0].clone(),
                        all[1].clone(),
                        all[2].clone(),
                        all[3].clone(),
                        all[4].clone(),
                        all[5].clone(),
                    ),
                );
                return;
            }
        }

        if !value.is_empty() && value != "3" {
            tdutils::log_error!("Have invalid value \"{value}\"");
        }
        if !self.is_full {
            self.deleted_strings.insert(key.to_owned());
        }
    }

    /// `load_language_strings` (cpp:724-787) — the C++ takes the database
    /// + language mutexes; the collapsed manager needs neither.
    pub(crate) fn load_language_strings(&mut self, keys: &[String]) -> bool {
        if self.is_full {
            tdutils::log_debug!("The language pack is already full in memory");
            return true;
        }
        if self.kv.empty() {
            tdutils::log_debug!("The language pack has no database");
            return false;
        }
        tdutils::log_debug!("Begin to load a language pack from database");
        if keys.is_empty() {
            if self.version == -1 && self.was_loaded_full {
                tdutils::log_debug!("The language pack has already been loaded");
                return false;
            }

            for (key, value) in self.kv.get_all() {
                if key.first() == Some(&b'!') {
                    continue;
                }
                let key = String::from_utf8_lossy(&key).into_owned();
                let value = String::from_utf8_lossy(&value).into_owned();
                if !self.language_has_string_unsafe(&key) {
                    tdutils::log_debug!("Load string with key {key} from database");
                    self.load_language_string_unsafe(&key, &value);
                }
            }
            self.was_loaded_full = true;

            if self.version == -1 {
                return false;
            }

            self.is_full = true;
            self.deleted_strings.clear();
            return true;
        }

        let mut have_all = true;
        for key in keys {
            if !self.language_has_string_unsafe(key) {
                let value = String::from_utf8_lossy(&self.kv.get(key.as_bytes())).into_owned();
                if value.is_empty() && self.version == -1 {
                    tdutils::log_debug!("Have no string with key {key} in the database");
                    have_all = false;
                    continue;
                }
                // an empty value with a known version means the string is
                // just deleted (the C++ reads value[0] == '\0' there)
                tdutils::log_debug!("Load string with key {key} from database");
                self.load_language_string_unsafe(key, &value);
            }
        }
        have_all
    }

    /// `save_strings_to_database` (cpp:1302-1345).
    pub(crate) fn save_strings_to_database(
        &mut self,
        new_version: i32,
        new_is_full: bool,
        new_key_count: i32,
        strings: Vec<(String, String)>,
    ) {
        tdutils::log_debug!(
            "Save to database a language pack with new version {new_version} and {} new strings",
            strings.len()
        );
        if new_version == -1 && strings.is_empty() {
            return;
        }
        if self.kv.empty() {
            tdutils::log_debug!("There is no associated database key-value");
            return;
        }
        let old_version = load_database_language_version(&mut self.kv);
        if old_version > new_version || (old_version == new_version && strings.is_empty()) {
            tdutils::log_debug!("Language pack version doesn't increased from {old_version}");
            return;
        }

        self.kv.begin_write_transaction().ensure();
        for (key, value) in &strings {
            if !is_valid_key(key) {
                tdutils::log_error!("Have invalid key \"{key}\"");
                continue;
            }
            if new_is_full && value == "3" {
                self.kv.erase(key.as_bytes());
            } else {
                self.kv.set(key.as_bytes(), value.as_bytes());
            }
            tdutils::log_debug!("Save language pack string with key {key} to database");
        }
        if old_version != new_version {
            tdutils::log_debug!("Set language pack version in database to {new_version}");
            self.kv.set(b"!version", new_version.to_string().as_bytes());
        }
        if new_key_count != -1 {
            tdutils::log_debug!("Set language pack key count in database to {new_key_count}");
            self.kv
                .set(b"!key_count", new_key_count.to_string().as_bytes());
        }
        self.kv.commit_transaction().ensure();
    }
}

/// `LanguagePackManager::LanguagePack` (cpp:90-97).
#[derive(Default)]
pub(crate) struct LanguagePack {
    pack_kv: SqliteKeyValue,
    /// sorted by language_code (the C++ `std::map`).
    custom_language_pack_infos: BTreeMap<String, LanguageInfo>,
    /// sorted by server (the arrival order).
    server_language_pack_infos: Vec<(String, LanguageInfo)>,
    all_server_language_pack_infos: HashMap<String, LanguageInfo>,
    languages: HashMap<String, Language>,
}

impl LanguagePack {
    fn new() -> LanguagePack {
        LanguagePack::default()
    }

    /// `save_server_language_pack_infos` (cpp:970-985) — the `!server2`
    /// blob is `code \x00 info-string` pairs imploded with `'\x00'`.
    fn save_server_language_pack_infos(&mut self) {
        if self.pack_kv.empty() {
            return;
        }
        tdutils::log_info!("Save changes server language pack infos");
        let mut all_strings = Vec::with_capacity(2 * self.server_language_pack_infos.len());
        for (code, info) in &self.server_language_pack_infos {
            all_strings.push(code.clone());
            all_strings.push(info.get_language_info_string());
        }
        self.pack_kv
            .set(b"!server2", implode(&all_strings, b'\0').as_bytes());
    }
}

/// `LanguagePackManager::LanguageDatabase` (cpp:99-104).
pub(crate) struct LanguageDatabase {
    path: String,
    database: SqliteDb,
    language_packs: HashMap<String, LanguagePack>,
}

impl Default for LanguageDatabase {
    fn default() -> Self {
        Self::new_empty()
    }
}

impl LanguageDatabase {
    /// The path-less in-memory database (the C++ `add_language_database`
    /// empty-path band, cpp:183-196).
    pub(crate) fn new_empty() -> LanguageDatabase {
        LanguageDatabase {
            path: String::new(),
            database: SqliteDb::new(),
            language_packs: HashMap::new(),
        }
    }

    /// `add_language_database` (cpp:177-198) minus the process-static
    /// registry: the manager owns exactly one database (see the module
    /// docs); an unopenable path falls back to the in-memory database
    /// like the C++ recursive call.
    pub(crate) fn open(path: &str) -> LanguageDatabase {
        if path.is_empty() {
            return LanguageDatabase::new_empty();
        }
        match open_database(path) {
            Result::Ok(database) => LanguageDatabase {
                path: path.to_owned(),
                database,
                language_packs: HashMap::new(),
            },
            Result::Err(error) => {
                tdutils::log_error!("Can't open language pack database {path}: {error}");
                LanguageDatabase::new_empty()
            }
        }
    }

    /// `get_language(database, pack, code)` (cpp:542-553).
    pub(crate) fn get_language(
        &mut self,
        language_pack: &str,
        language_code: &str,
    ) -> Option<&mut Language> {
        self.language_packs
            .get_mut(language_pack)?
            .languages
            .get_mut(language_code)
    }

    /// `add_language(database, pack, code)` (cpp:570-674) — creates the
    /// pack (with its `pack_kv_` info load) and the language (with its
    /// kv-loaded version/key_count/base) on first use.
    pub(crate) fn add_language(
        &mut self,
        language_pack: &str,
        language_code: &str,
    ) -> &mut Language {
        if !self.language_packs.contains_key(language_pack) {
            let mut pack = LanguagePack::new();
            if !self.database.empty() {
                pack.pack_kv
                    .init_with_connection(
                        self.database.clone(),
                        &get_database_table_name(language_pack, "0"),
                    )
                    .ensure();
                let mut need_drop_server = false;
                for (raw_key, raw_value) in pack.pack_kv.get_all() {
                    let key = String::from_utf8(raw_key).unwrap_or_default();
                    let value = String::from_utf8(raw_value).unwrap_or_default();

                    if key == "!server" {
                        // legacy info format, drop cache
                        need_drop_server = true;
                        continue;
                    }
                    if key == "!server2" {
                        for (code, info) in LanguageInfo::parse_server_infos(&value) {
                            pack.all_server_language_pack_infos
                                .insert(code.clone(), info.clone());
                            pack.server_language_pack_infos.push((code, info));
                        }
                        continue;
                    }

                    if let Some(info) = LanguageInfo::parse_custom(&value) {
                        pack.custom_language_pack_infos.insert(key, info);
                    }
                }
                if need_drop_server {
                    tdutils::log_info!("Drop old server language pack info cache");
                    pack.pack_kv.erase(b"!server");
                }
            }
            self.language_packs.insert(language_pack.to_owned(), pack);
        }
        let pack = self
            .language_packs
            .get_mut(language_pack)
            .expect("pack just inserted");
        if !pack.languages.contains_key(language_code) {
            let mut language = Language::new();
            if !self.database.empty() {
                language
                    .kv
                    .init_with_connection(
                        self.database.clone(),
                        &get_database_table_name(language_pack, language_code),
                    )
                    .ensure();
                language.version = load_database_language_version(&mut language.kv);
                language.key_count = load_database_language_key_count(&mut language.kv);
                language.base_language_code =
                    load_database_language_base_language_code(&mut language.kv);
                tdutils::log_info!(
                    "Loaded language {language_code} with version {}, key count {} and base language {}",
                    language.version,
                    language.key_count,
                    language.base_language_code
                );
            }
            pack.languages.insert(language_code.to_owned(), language);
        }
        pack.languages
            .get_mut(language_code)
            .expect("language just inserted")
    }
}

/// `get_language_pack_string_value_object(const Language *language, const
/// string &key)` (cpp:818-831).
fn get_language_pack_string_value_from_language(
    language: &Language,
    key: &str,
) -> td_api::LanguagePackStringValue {
    if let Some(value) = language.ordinary_strings.get(key) {
        return ordinary_string_value_object(value);
    }
    if let Some(value) = language.pluralized_strings.get(key) {
        return pluralized_string_value_object(value);
    }
    if !language.is_full && !language.deleted_strings.contains(key) {
        tdutils::log_error!("Have no string for key {key}");
    }
    deleted_string_value_object()
}

/// `get_language_pack_string_object(const Language *language, const
/// string &key)` (cpp:833-836).
fn get_language_pack_string_object_from_language(
    language: &Language,
    key: &str,
) -> td_api::LanguagePackString {
    td_api::LanguagePackString {
        key: key.to_owned(),
        value: Some(get_language_pack_string_value_from_language(language, key)),
    }
}

/// `get_language_pack_strings_object` (cpp:838-858) — the empty-`keys`
/// form iterates the ordinary map before the pluralized one; the C++
/// `FlatHashMap` order is unspecified, so are the Rust maps.
fn get_language_pack_strings_object(
    language: &Language,
    keys: &[String],
) -> td_api::LanguagePackStrings {
    let mut strings = Vec::new();
    if keys.is_empty() {
        for (key, value) in &language.ordinary_strings {
            strings.push(get_language_pack_string_object_ordinary(key, value));
        }
        for (key, value) in &language.pluralized_strings {
            strings.push(get_language_pack_string_object_pluralized(key, value));
        }
    } else {
        for key in keys {
            strings.push(get_language_pack_string_object_from_language(language, key));
        }
    }
    td_api::LanguagePackStrings { strings }
}

// ===== the td_execute tail (SynchronousRequests.cpp:276-280,
// ===== LanguagePackManager.cpp:1261-1291) =====

/// The path-keyed registry behind the synchronous
/// `getLanguagePackString` — the C++ `language_databases_` static
/// (`language_database_mutex_` collapses into this mutex).
static SYNC_LANGUAGE_DATABASES: std::sync::LazyLock<
    std::sync::Mutex<HashMap<String, LanguageDatabase>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

/// `LanguagePackManager::get_language_pack_string` (cpp:1261-1291) — the
/// `SynchronousRequests::do_request(td_api::getLanguagePackString)` tail.
pub fn get_language_pack_string(
    database_path: &str,
    language_pack: &str,
    language_code: &str,
    key: &str,
) -> td_api::Object {
    if !check_language_pack_name(language_pack) || language_pack.is_empty() {
        return crate::synchronous_requests::make_error(400, "Localization target is invalid");
    }
    if !check_language_code_name(language_code) || language_code.is_empty() {
        return crate::synchronous_requests::make_error(400, "Language pack ID is invalid");
    }
    if !is_valid_key(key) {
        return crate::synchronous_requests::make_error(400, "Key is invalid");
    }

    let mut databases = SYNC_LANGUAGE_DATABASES.lock().unwrap();
    if !databases.contains_key(database_path) {
        databases.insert(
            database_path.to_owned(),
            LanguageDatabase::open(database_path),
        );
    }
    let database = databases.get_mut(database_path).expect("just inserted");

    let language = database.add_language(language_pack, language_code);
    let keys = vec![key.to_owned()];
    if language.language_has_strings(&keys) || language.load_language_strings(&keys) {
        let value = get_language_pack_string_value_from_language(language, key);
        return object_from_string_value(value);
    }
    crate::synchronous_requests::make_error(404, "Not Found")
}

/// The `td_api::make_object<td_api::Object>` wrapper of the C++
/// signature's `object_ptr<Object>` return.
fn object_from_string_value(value: td_api::LanguagePackStringValue) -> td_api::Object {
    use td_api::Object;
    match value {
        td_api::LanguagePackStringValue::languagePackStringValueOrdinary(v) => {
            Object::languagePackStringValueOrdinary(v)
        }
        td_api::LanguagePackStringValue::languagePackStringValuePluralized(v) => {
            Object::languagePackStringValuePluralized(v)
        }
        td_api::LanguagePackStringValue::languagePackStringValueDeleted(v) => {
            Object::languagePackStringValueDeleted(v)
        }
    }
}

// ===== the manager plane =====

/// The `Td *td` slice of the collapsed LanguagePackManager.
pub trait LanguagePackManagerEnv {
    // -- options
    fn get_option_string(&self, name: &str, default_value: &str) -> String;
    fn get_option_integer(&self, name: &str, default_value: i64) -> i64;
    fn set_option_empty(&mut self, name: &str);

    // -- the update plane
    /// `send_closure(G()->td(), &Td::send_update, ...)`.
    fn send_update(&mut self, update: td_api::Update);

    // -- the `[blocked: net]` query tails; each one drops the parked
    // -- promise(s), keeping the C++ offline request unanswered
    /// `langpack_getLanguages` (cpp:871-878); the response half is
    /// [`LanguagePackManager::on_get_languages`].
    fn send_get_languages_query(
        &mut self,
        language_pack: String,
        promise: Promise<td_api::LocalizationTargetInfo>,
    );
    /// `langpack_getLanguage` (cpp:887-897); the response half is
    /// [`LanguagePackManager::on_get_language`].
    fn send_get_language_query(
        &mut self,
        language_pack: String,
        language_code: String,
        promise: Promise<td_api::LanguagePackInfo>,
    );
    /// `langpack_getLangPack` (cpp:1128-1150); the response half is
    /// `on_get_language_pack_strings(..., is_diff = false, keys = [])`
    /// feeding `on_get_all_language_pack_strings` (the parked promises
    /// live in the manager's query map).
    fn send_get_lang_pack_query(&mut self, language_pack: String, language_code: String);
    /// `langpack_getStrings` (cpp:1152-1161); the response half is
    /// `on_get_language_pack_strings(..., version = -1, keys)`.
    fn send_get_strings_query(
        &mut self,
        language_pack: String,
        language_code: String,
        keys: Vec<String>,
        promise: Promise<td_api::LanguagePackStrings>,
    );
    /// `langpack_getDifference` (cpp:433-458); the response half is
    /// `on_get_language_pack_strings(..., is_diff = true)`, the failure
    /// half [`LanguagePackManager::on_failed_get_difference`]. The
    /// waiting promises park in the language's difference queue.
    fn send_get_difference_query(
        &mut self,
        language_pack: String,
        language_code: String,
        from_version: i32,
    );
}

/// The collapsed `LanguagePackManager` actor (LanguagePackManager.h:31).
pub struct LanguagePackManager {
    language_pack: String,
    language_code: String,
    base_language_code: String,
    database: LanguageDatabase,
    /// `get_all_language_pack_strings_queries_` (LanguagePackManager.h:104).
    get_all_language_pack_strings_queries:
        HashMap<String, HashMap<String, Vec<Promise<td_api::LanguagePackStrings>>>>,
}

impl Default for LanguagePackManager {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguagePackManager {
    /// The pre-init state (the C++ actor is created in `Td::
    /// init_pure_actor_managers`, Td.cpp:1348, with the constructor work
    /// split into [`LanguagePackManager::init`]).
    pub fn new() -> LanguagePackManager {
        LanguagePackManager {
            language_pack: String::new(),
            language_code: String::new(),
            base_language_code: String::new(),
            database: LanguageDatabase::new_empty(),
            get_all_language_pack_strings_queries: HashMap::new(),
        }
    }

    /// The C++ constructor (cpp:200-226) + `start_up` (cpp:228-250) —
    /// the actor start follows the constructor immediately in the
    /// collapsed line. Idempotence guard: the C++ constructor runs once.
    pub fn init(&mut self, env: &mut impl LanguagePackManagerEnv) {
        self.language_pack = env.get_option_string("localization_target", "");
        self.language_code = env.get_option_string("language_pack_id", "");
        assert!(
            check_language_pack_name(&self.language_pack),
            "invalid localization target"
        );
        assert!(
            check_language_code_name(&self.language_code),
            "invalid language pack ID"
        );

        self.database =
            LanguageDatabase::open(&env.get_option_string("language_pack_database_path", ""));
        if !self.language_pack.is_empty() && !self.language_code.is_empty() {
            let language = self
                .database
                .add_language(&self.language_pack, &self.language_code);
            self.base_language_code = language.base_language_code.clone();
            if !check_language_code_name(&self.base_language_code) {
                tdutils::log_error!(
                    "Have invalid base language pack ID \"{}\"",
                    self.base_language_code
                );
                self.base_language_code.clear();
            }
            if !self.base_language_code.is_empty() {
                self.database
                    .add_language(&self.language_pack, &self.base_language_code);
            }

            tdutils::log_info!(
                "Use localization target \"{}\" with language pack \"{}\" based on \"{}\" of version {} with database \"{}\"",
                self.language_pack,
                self.language_code,
                self.base_language_code,
                self.database
                    .get_language(&self.language_pack, &self.language_code)
                    .map_or(-1, |language| language.version),
                self.database.path
            );
        }

        // `start_up` (cpp:228-250)
        if self.language_pack.is_empty() || self.language_code.is_empty() {
            return;
        }
        let language = self
            .database
            .get_language(&self.language_pack, &self.language_code)
            .expect("language just added");
        if language.version == -1 {
            let language_code = self.language_code.clone();
            self.load_empty_language_pack(env, &language_code);
        }
        self.repair_chosen_language_info(env);

        if !self.base_language_code.is_empty() {
            let base_language = self
                .database
                .get_language(&self.language_pack, &self.base_language_code)
                .expect("base language just added");
            if base_language.version == -1 {
                let base_code = self.base_language_code.clone();
                self.load_empty_language_pack(env, &base_code);
            }
        }

        self.on_language_pack_version_changed(env, false, -1);
        self.on_language_pack_version_changed(env, true, -1);
    }

    /// `get_main_language_code` (cpp:265-308).
    pub fn get_main_language_code(&mut self, env: &mut impl LanguagePackManagerEnv) -> String {
        if self.language_pack.is_empty() || self.language_code.is_empty() {
            return "en".to_owned();
        }
        if self.language_code.len() == 2 {
            return self.language_code.clone();
        }

        let info = self.find_chosen_language_info();
        match info {
            None => {
                tdutils::log_info!(
                    "Failed to find information about chosen language {}, ensure that valid language pack ID is used",
                    self.language_code
                );
                if !is_custom_language_code(&self.language_code) {
                    self.get_languages(env, false, Promise::default());
                }
            }
            Some(info) => {
                if !info.base_language_code.is_empty() {
                    return info.base_language_code;
                }
                if !info.plural_code.is_empty() {
                    return info.plural_code;
                }
            }
        }
        "en".to_owned()
    }

    /// `get_used_language_codes` (cpp:310-354).
    pub fn get_used_language_codes(
        &mut self,
        env: &mut impl LanguagePackManagerEnv,
    ) -> Vec<String> {
        if self.language_pack.is_empty() || self.language_code.is_empty() {
            return Vec::new();
        }

        let mut result = Vec::new();
        if self.language_code.len() == 2 {
            result.push(self.language_code.clone());
        }
        let info = self.find_chosen_language_info();
        match info {
            None => {
                tdutils::log_info!(
                    "Failed to find information about chosen language {}, ensure that valid language pack ID is used",
                    self.language_code
                );
                if !is_custom_language_code(&self.language_code) {
                    self.get_languages(env, false, Promise::default());
                }
            }
            Some(info) => {
                if !info.base_language_code.is_empty() {
                    result.push(info.base_language_code);
                }
                if !info.plural_code.is_empty() {
                    result.push(info.plural_code);
                }
            }
        }
        result
    }

    /// The chosen-language info lookup shared by
    /// `get_main_language_code`/`get_used_language_codes` (cpp:273-291,
    /// 315-333): custom packs look into `custom_language_pack_infos_`,
    /// server packs take the last matching `server_language_pack_infos_`
    /// entry.
    fn find_chosen_language_info(&mut self) -> Option<LanguageInfo> {
        let pack = self.database.language_packs.get_mut(&self.language_pack)?;
        if is_custom_language_code(&self.language_code) {
            return pack
                .custom_language_pack_infos
                .get(&self.language_code)
                .cloned();
        }
        pack.server_language_pack_infos
            .iter()
            .rev()
            .find(|(code, _)| code == &self.language_code)
            .map(|(_, info)| info.clone())
    }

    /// `on_language_pack_changed` (cpp:356-365).
    pub fn on_language_pack_changed(&mut self, env: &mut impl LanguagePackManagerEnv) {
        let new_language_pack = env.get_option_string("localization_target", "");
        if new_language_pack == self.language_pack {
            return;
        }

        self.language_pack = new_language_pack;
        assert!(
            check_language_pack_name(&self.language_pack),
            "invalid localization target"
        );
        self.inc_generation(env);
    }

    /// `on_language_code_changed` (cpp:367-376).
    pub fn on_language_code_changed(&mut self, env: &mut impl LanguagePackManagerEnv) {
        let new_language_code = env.get_option_string("language_pack_id", "");
        if new_language_code == self.language_code {
            return;
        }

        self.language_code = new_language_code;
        assert!(
            check_language_code_name(&self.language_code),
            "invalid language pack ID"
        );
        self.inc_generation(env);
    }

    /// `on_language_pack_version_changed` (cpp:378-421).
    pub fn on_language_pack_version_changed(
        &mut self,
        env: &mut impl LanguagePackManagerEnv,
        is_base: bool,
        mut new_version: i32,
    ) {
        if self.language_pack.is_empty() || self.language_code.is_empty() {
            return;
        }

        let version = self
            .database
            .get_language(&self.language_pack, &self.language_code)
            .map_or(-1, |language| language.version);
        tdutils::log_info!(
            "{} language pack version has changed from main {version} to {new_version}",
            if is_base { "Base" } else { "Main" }
        );
        if version == -1 {
            let language_code = self.language_code.clone();
            return self.load_empty_language_pack(env, &language_code);
        }

        if new_version < 0 {
            let version_key = if is_base {
                "base_language_pack_version"
            } else {
                "language_pack_version"
            };
            new_version = env.get_option_integer(version_key, -1) as i32;
        }
        if new_version <= 0 {
            return;
        }

        let language_code;
        let mut version = version;
        if is_base {
            language_code = self.base_language_code.clone();
            if language_code.is_empty() {
                tdutils::log_error!(
                    "Have no base language, but received new version {new_version}"
                );
                return;
            }
            version = self
                .database
                .get_language(&self.language_pack, &language_code)
                .map_or(-1, |language| language.version);
            if version == -1 {
                return self.load_empty_language_pack(env, &language_code);
            }
        } else {
            language_code = self.language_code.clone();
        }
        if is_custom_language_code(&language_code) || new_version <= version {
            return;
        }

        tdutils::log_info!(
            "{} language pack {language_code} version has changed to {new_version}",
            if is_base { "Base" } else { "Main" }
        );
        let language = self
            .database
            .get_language(&self.language_pack, &language_code)
            .expect("language checked above");
        Self::send_language_get_difference_query(
            env,
            language,
            self.language_pack.clone(),
            language_code,
            version,
            Promise::default(),
        );
    }

    /// `send_language_get_difference_query` (cpp:423-459) — the promise
    /// parks in the language's difference queue and the query goes to
    /// the `[blocked: net]` seam; the response half (lowercased code,
    /// mismatch logs, `on_get_language_pack_strings(..., is_diff = true)`)
    /// comes back with the net step. A free function because the caller
    /// holds the `&mut Language` borrow.
    fn send_language_get_difference_query(
        env: &mut impl LanguagePackManagerEnv,
        language: &mut Language,
        language_pack: String,
        language_code: String,
        version: i32,
        promise: Promise<()>,
    ) {
        language.get_difference_queries.push(promise);
        if language.has_get_difference_query {
            return;
        }

        assert_eq!(
            language.get_difference_queries.len(),
            1,
            "duplicate difference query"
        );
        language.has_get_difference_query = true;
        env.send_get_difference_query(language_pack, language_code, version);
    }

    /// `on_language_pack_too_long` (cpp:461-470).
    pub fn on_language_pack_too_long(
        &mut self,
        env: &mut impl LanguagePackManagerEnv,
        language_code: String,
    ) {
        if language_code == self.language_code {
            return self.on_language_pack_version_changed(env, false, i32::MAX);
        }
        if language_code == self.base_language_code {
            return self.on_language_pack_version_changed(env, true, i32::MAX);
        }
        tdutils::log_warning!(
            "Receive languagePackTooLong for language {language_code}, but use language {} with base language {}",
            self.language_code,
            self.base_language_code
        );
    }

    /// `on_update_language_pack` (cpp:472-511).
    pub fn on_update_language_pack(
        &mut self,
        env: &mut impl LanguagePackManagerEnv,
        mut difference: telegram_api::LangPackDifference,
    ) {
        tdutils::log_info!(
            "Receive update language pack difference for language pack {} from version {} with version {} of size {}",
            difference.lang_code,
            difference.from_version,
            difference.version,
            difference.strings.len()
        );
        to_lower_inplace(&mut difference.lang_code);
        if self.language_code.is_empty() {
            tdutils::log_info!(
                "Ignore difference for language pack {}, because have no used language pack",
                difference.lang_code
            );
            return;
        }
        if self.language_pack.is_empty() {
            tdutils::log_warning!(
                "Ignore difference for language pack {}, because localization target is not set",
                difference.lang_code
            );
            return;
        }
        if difference.lang_code != self.language_code
            && difference.lang_code != self.base_language_code
        {
            tdutils::log_warning!(
                "Ignore difference for language pack {}, because using language pack {} based on {}",
                difference.lang_code,
                self.language_code,
                self.base_language_code
            );
            return;
        }
        if is_custom_language_code(&difference.lang_code) || difference.lang_code.is_empty() {
            tdutils::log_error!(
                "Ignore difference for language pack {}",
                difference.lang_code
            );
            return;
        }

        let version = self
            .database
            .get_language(&self.language_pack, &difference.lang_code)
            .map_or(-1, |language| language.version);
        if difference.version <= version {
            tdutils::log_info!("Skip applying already applied language pack updates");
            return;
        }
        if version == -1 || version < difference.from_version {
            tdutils::log_info!("Can't apply language pack difference");
            return self.on_language_pack_version_changed(
                env,
                difference.lang_code != self.language_code,
                difference.version,
            );
        }

        self.on_get_language_pack_strings(
            env,
            self.language_pack.clone(),
            difference.lang_code,
            difference.version,
            true,
            Vec::new(),
            std::mem::take(&mut difference.strings),
            Promise::default(),
        );
    }

    /// `inc_generation` (cpp:513-540).
    fn inc_generation(&mut self, env: &mut impl LanguagePackManagerEnv) {
        env.set_option_empty("language_pack_version");
        env.set_option_empty("base_language_pack_version");

        if !self.language_pack.is_empty() && !self.language_code.is_empty() {
            tdutils::log_info!("Add main language {}", self.language_code);
            assert!(
                check_language_code_name(&self.language_code),
                "invalid language pack ID"
            );
            let base_language_code = self
                .database
                .add_language(&self.language_pack, &self.language_code)
                .base_language_code
                .clone();
            self.on_language_pack_version_changed(env, false, i32::MAX);
            self.repair_chosen_language_info(env);

            self.base_language_code = base_language_code;
            if !check_language_code_name(&self.base_language_code) {
                tdutils::log_error!(
                    "Have invalid base language pack ID \"{}\"",
                    self.base_language_code
                );
                self.base_language_code.clear();
            }
            if !self.base_language_code.is_empty() {
                assert!(
                    self.base_language_code != self.language_code,
                    "base language equals main language"
                );
                tdutils::log_info!("Add base language {}", self.base_language_code);
                self.database
                    .add_language(&self.language_pack, &self.base_language_code);
                self.on_language_pack_version_changed(env, true, i32::MAX);
            }
        }
        tdutils::log_info!("Finished to apply new language pack");
    }

    /// `repair_chosen_language_info` (cpp:900-920).
    fn repair_chosen_language_info(&mut self, env: &mut impl LanguagePackManagerEnv) {
        assert!(
            !self.language_pack.is_empty() && !self.language_code.is_empty(),
            "repair without a chosen language"
        );
        if is_custom_language_code(&self.language_code) {
            return;
        }

        let Some(pack) = self.database.language_packs.get(&self.language_pack) else {
            panic!("localization target pack missing");
        };
        if pack
            .server_language_pack_infos
            .iter()
            .any(|(code, _)| code == &self.language_code)
        {
            return;
        }

        tdutils::log_info!("Repair info about language {}", self.language_code);
        self.search_language_info(env, self.language_code.clone(), Promise::default());
    }

    /// `get_languages` (cpp:860-879) — the `only_local` half is fully
    /// local; the server half is the `[blocked: net]` seam.
    pub fn get_languages(
        &mut self,
        env: &mut impl LanguagePackManagerEnv,
        only_local: bool,
        mut promise: Promise<td_api::LocalizationTargetInfo>,
    ) {
        if self.language_pack.is_empty() {
            promise.set_error(Status::error(
                400,
                "Option \"localization_target\" needs to be set first",
            ));
            return;
        }

        if only_local {
            self.on_get_languages(env, Vec::new(), self.language_pack.clone(), true, promise);
            return;
        }

        env.send_get_languages_query(self.language_pack.clone(), promise);
    }

    /// `search_language_info` (cpp:881-898) — validation is local, the
    /// query is the `[blocked: net]` seam.
    pub fn search_language_info(
        &mut self,
        env: &mut impl LanguagePackManagerEnv,
        language_code: String,
        mut promise: Promise<td_api::LanguagePackInfo>,
    ) {
        if self.language_pack.is_empty() {
            promise.set_error(Status::error(
                400,
                "Option \"localization_target\" needs to be set first",
            ));
            return;
        }

        env.send_get_language_query(self.language_pack.clone(), language_code, promise);
    }

    /// `on_get_language_info` (cpp:937-968).
    fn on_get_language_info(
        &mut self,
        env: &mut impl LanguagePackManagerEnv,
        language_pack: &str,
        language_pack_info: &mut td_api::LanguagePackInfo,
    ) {
        let language = self
            .database
            .add_language(language_pack, &language_pack_info.id);
        language_pack_info.local_string_count = language.key_count;
        let mut new_base_language_code: Option<String> = None;
        let mut was_updated_base_language_code = false;
        {
            if language_pack_info.base_language_pack_id != language.base_language_code {
                language.base_language_code = language_pack_info.base_language_pack_id.clone();
                if language_pack == self.language_pack
                    && language_pack_info.id == self.language_code
                {
                    self.base_language_code = language.base_language_code.clone();
                    was_updated_base_language_code = true;
                }
                if !language.kv.empty() {
                    new_base_language_code = Some(language_pack_info.base_language_pack_id.clone());
                }
            }
        }
        if was_updated_base_language_code {
            env.set_option_empty("base_language_pack_version");
            if !self.base_language_code.is_empty() {
                self.database
                    .add_language(&self.language_pack, &self.base_language_code);
                self.on_language_pack_version_changed(env, true, i32::MAX);
            }
        }
        if let Some(base_language_code) = new_base_language_code {
            let language = self
                .database
                .get_language(language_pack, &language_pack_info.id)
                .expect("language just added");
            language
                .kv
                .set(b"!base_language_code", base_language_code.as_bytes());
        }
    }

    /// `on_get_languages` (cpp:987-1050).
    pub fn on_get_languages(
        &mut self,
        env: &mut impl LanguagePackManagerEnv,
        languages: Vec<telegram_api::LangPackLanguage>,
        language_pack: String,
        only_local: bool,
        mut promise: Promise<td_api::LocalizationTargetInfo>,
    ) {
        let mut results = td_api::LocalizationTargetInfo {
            language_packs: Vec::new(),
        };
        let mut added_languages: HashSet<String> = HashSet::new();

        // the add_language_info lambda (cpp:993-999)
        fn add_language_info(
            results: &mut td_api::LocalizationTargetInfo,
            added_languages: &mut HashSet<String>,
            language_code: &str,
            info: &LanguageInfo,
            is_installed: bool,
        ) {
            if !language_code.is_empty() && added_languages.insert(language_code.to_owned()) {
                let mut language_pack_info = get_language_pack_info_object(language_code, info);
                language_pack_info.is_installed = is_installed;
                results.language_packs.push(language_pack_info);
            }
        }

        {
            if let Some(pack) = self.database.language_packs.get_mut(&language_pack) {
                for (code, info) in &pack.custom_language_pack_infos {
                    add_language_info(&mut results, &mut added_languages, code, info, true);
                }
                if only_local {
                    for (code, info) in &pack.server_language_pack_infos {
                        add_language_info(&mut results, &mut added_languages, code, info, false);
                    }
                }
            }
        }

        let mut all_server_infos: Vec<(String, LanguageInfo)> = Vec::new();
        for language in languages {
            let Result::Ok((lang_code, info)) = get_language_info_from_wire(language) else {
                continue;
            };

            add_language_info(&mut results, &mut added_languages, &lang_code, &info, false);
            all_server_infos.push((lang_code, info));
        }

        for language_pack_info in &mut results.language_packs {
            self.on_get_language_info(env, &language_pack, language_pack_info);
        }

        if !only_local
            && let Some(pack) = self.database.language_packs.get_mut(&language_pack)
            && pack.server_language_pack_infos != all_server_infos
        {
            for (code, info) in &all_server_infos {
                pack.all_server_language_pack_infos
                    .insert(code.clone(), info.clone());
            }
            pack.server_language_pack_infos = all_server_infos;

            pack.save_server_language_pack_infos();
        }
        promise.set_value(results);
    }

    /// `on_get_language` (cpp:1052-1092).
    pub fn on_get_language(
        &mut self,
        env: &mut impl LanguagePackManagerEnv,
        lang_pack_language: telegram_api::LangPackLanguage,
        language_pack: String,
        language_code: String,
        mut promise: Promise<td_api::LanguagePackInfo>,
    ) {
        let (wire_lang_code, language_info) = match get_language_info_from_wire(lang_pack_language)
        {
            Result::Ok(result) => result,
            Result::Err(status) => {
                promise.set_error(status);
                return;
            }
        };
        let mut result = get_language_pack_info_object(&wire_lang_code, &language_info);

        self.on_get_language_info(env, &language_pack, &mut result);

        // updating languages cache
        if let Some(pack) = self.database.language_packs.get_mut(&language_pack) {
            result.is_installed = pack
                .custom_language_pack_infos
                .contains_key(&wire_lang_code)
                || pack.custom_language_pack_infos.contains_key(&language_code);

            let mut is_changed = false;
            for (code, info) in &mut pack.server_language_pack_infos {
                if (code == &wire_lang_code || code == &language_code) && *info != language_info {
                    tdutils::log_info!("Language pack {code} was changed");
                    is_changed = true;
                    *info = language_info.clone();
                }
            }
            pack.all_server_language_pack_infos
                .insert(wire_lang_code, language_info);

            if is_changed {
                pack.save_server_language_pack_infos();
            }
        } else {
            tdutils::log_error!("Failed to find localization target {language_pack}");
        }

        promise.set_value(result);
    }

    /// `get_language_pack_strings` (cpp:1094-1163) — the loaded-pack
    /// local halves answer immediately; the rest parks on the
    /// `[blocked: net]` seams (the get-all promises in the manager's
    /// query map, the keyed-strings promise on the seam).
    pub fn get_language_pack_strings(
        &mut self,
        env: &mut impl LanguagePackManagerEnv,
        language_code: String,
        keys: Vec<String>,
        mut promise: Promise<td_api::LanguagePackStrings>,
    ) {
        if !check_language_code_name(&language_code) || language_code.is_empty() {
            promise.set_error(Status::error(400, "Language pack ID is invalid"));
            return;
        }
        if self.language_pack.is_empty() {
            promise.set_error(Status::error(
                400,
                "Option \"localization_target\" needs to be set first",
            ));
            return;
        }
        for key in &keys {
            if !is_valid_key(key) {
                promise.set_error(Status::error(400, "Invalid key name"));
                return;
            }
        }

        let language = self
            .database
            .add_language(&self.language_pack, &language_code);
        if language.language_has_strings(&keys) {
            promise.set_value(get_language_pack_strings_object(language, &keys));
            return;
        }
        if language.load_language_strings(&keys) {
            promise.set_value(get_language_pack_strings_object(language, &keys));
            return;
        }

        if is_custom_language_code(&language_code) {
            promise.set_error(Status::error(400, "Custom language pack not found"));
            return;
        }

        if keys.is_empty() {
            let queries = self
                .get_all_language_pack_strings_queries
                .entry(self.language_pack.clone())
                .or_default()
                .entry(language_code.clone())
                .or_default();
            queries.push(promise);
            if queries.len() != 1 {
                // send request only once
                return;
            }

            // the C++ result promise chains into
            // on_get_all_language_pack_strings; offline the query (and
            // with it the response) never comes back
            env.send_get_lang_pack_query(self.language_pack.clone(), language_code);
        } else {
            env.send_get_strings_query(self.language_pack.clone(), language_code, keys, promise);
        }
    }

    /// `load_empty_language_pack` (cpp:1165-1170).
    fn load_empty_language_pack(
        &mut self,
        env: &mut impl LanguagePackManagerEnv,
        language_code: &str,
    ) {
        if is_custom_language_code(language_code) {
            return;
        }
        self.get_language_pack_strings(
            env,
            language_code.to_owned(),
            Vec::new(),
            Promise::default(),
        );
    }

    /// `synchronize_language_pack` (cpp:1172-1191).
    pub fn synchronize_language_pack(
        &mut self,
        env: &mut impl LanguagePackManagerEnv,
        language_code: String,
        mut promise: Promise<()>,
    ) {
        if !check_language_code_name(&language_code) || language_code.is_empty() {
            promise.set_error(Status::error(400, "Language pack ID is invalid"));
            return;
        }
        if self.language_pack.is_empty() {
            promise.set_error(Status::error(
                400,
                "Option \"localization_target\" needs to be set first",
            ));
            return;
        }
        if is_custom_language_code(&language_code) {
            promise.set_value(());
            return;
        }

        let language = self
            .database
            .add_language(&self.language_pack, &language_code);
        language.load_language_strings(&[]);

        let mut version = language.version;
        if version == -1 {
            version = 0;
        }
        Self::send_language_get_difference_query(
            env,
            language,
            self.language_pack.clone(),
            language_code,
            version,
            promise,
        );
    }

    /// `on_get_all_language_pack_strings` (cpp:1214-1259) — the parked
    /// get-all promises fan out; all but the last non-empty promise get
    /// a copy (the C++ moves the original into the last one — the values
    /// are plain data, so the copy count is the only difference). The
    /// response half of `langpack_getLangPack` — reached with the net
    /// step; tests drive it directly.
    #[allow(dead_code)]
    pub(crate) fn on_get_all_language_pack_strings(
        &mut self,
        language_pack: String,
        language_code: String,
        r_strings: Result<td_api::LanguagePackStrings>,
    ) {
        let mut promises = {
            let Some(outer) = self
                .get_all_language_pack_strings_queries
                .get_mut(&language_pack)
            else {
                panic!("missing get-all queries for {language_pack}");
            };
            let Some(queries) = outer.get_mut(&language_code) else {
                panic!("missing get-all queries for {language_code}");
            };
            let promises = std::mem::take(queries);
            outer.remove(&language_code);
            if outer.is_empty() {
                self.get_all_language_pack_strings_queries
                    .remove(&language_pack);
            }
            promises
        };
        assert!(
            !promises.is_empty(),
            "on_get_all_language_pack_strings without promises"
        );

        match r_strings {
            Result::Ok(strings) => {
                let mut left_non_empty_promise_count =
                    promises.iter().filter(|p| !p.is_empty()).count();
                for promise in promises.iter_mut() {
                    if !promise.is_empty() {
                        if left_non_empty_promise_count == 1 {
                            tdutils::log_debug!("Set last non-empty promise");
                        } else {
                            tdutils::log_debug!("Set non-empty promise");
                        }
                        promise.set_value(clone_language_pack_strings(&strings));
                        left_non_empty_promise_count -= 1;
                    } else {
                        tdutils::log_debug!("Set empty promise");
                        promise.set_value(clone_language_pack_strings(&strings));
                    }
                }
                assert_eq!(
                    left_non_empty_promise_count, 0,
                    "unanswered non-empty promises"
                );
            }
            Result::Err(status) => {
                for promise in promises.iter_mut() {
                    promise.set_error(status.clone());
                }
            }
        }
    }

    /// `on_get_language_pack_strings` (cpp:1347-1512) — the wire string
    /// applier: merges the results into the in-memory language, emits
    /// `updateLanguagePackStrings`, saves the byte-exact kv entries and
    /// resolves the parked difference queries.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn on_get_language_pack_strings(
        &mut self,
        env: &mut impl LanguagePackManagerEnv,
        language_pack: String,
        language_code: String,
        version: i32,
        is_diff: bool,
        keys: Vec<String>,
        results: Vec<telegram_api::LangPackString>,
        mut promise: Promise<td_api::LanguagePackStrings>,
    ) {
        let mut is_version_changed = false;
        let mut new_database_version = -1;
        let mut new_key_count = -1;
        let mut new_is_full = false;
        let mut database_strings: Vec<(String, String)> = Vec::new();

        let needs_add = self
            .database
            .get_language(&language_pack, &language_code)
            .is_none();
        if needs_add
            || self
                .database
                .get_language(&language_pack, &language_code)
                .expect("checked above")
                .version
                < version
            || !keys.is_empty()
        {
            let language = self.database.add_language(&language_pack, &language_code);
            language.load_language_strings(&keys);

            if language.version < version || !keys.is_empty() {
                let is_first = language.version == -1;
                let mut strings: Vec<td_api::LanguagePackString> = Vec::new();
                if language.version < version {
                    tdutils::log_info!("Set language pack {language_code} version to {version}");
                    language.version = version;
                    new_database_version = version;
                    is_version_changed = true;
                }

                let mut key_count_delta = 0i32;
                for result in results {
                    match result {
                        telegram_api::LangPackString::langPackString(str) => {
                            if !is_valid_key(&str.key) {
                                tdutils::log_error!("Receive invalid key \"{}\"", str.key);
                                continue;
                            }
                            let key = str.key;
                            let value = str.value;
                            match language.ordinary_strings.entry(key.clone()) {
                                std::collections::hash_map::Entry::Occupied(mut entry) => {
                                    entry.insert(value.clone());
                                }
                                std::collections::hash_map::Entry::Vacant(entry) => {
                                    key_count_delta += 1;
                                    entry.insert(value.clone());
                                }
                            }
                            key_count_delta -=
                                i32::from(language.pluralized_strings.remove(&key).is_some());
                            language.deleted_strings.remove(&key);
                            if is_diff {
                                strings
                                    .push(get_language_pack_string_object_ordinary(&key, &value));
                            }
                            database_strings.push((key, format!("1{value}")));
                        }
                        telegram_api::LangPackString::langPackStringPluralized(str) => {
                            if !is_valid_key(&str.key) {
                                tdutils::log_error!("Receive invalid key \"{}\"", str.key);
                                continue;
                            }
                            let key = str.key;
                            let value = PluralizedString::from_parts(
                                str.zero_value,
                                str.one_value,
                                str.two_value,
                                str.few_value,
                                str.many_value,
                                str.other_value,
                            );
                            match language.pluralized_strings.entry(key.clone()) {
                                std::collections::hash_map::Entry::Occupied(mut entry) => {
                                    entry.insert(value.clone());
                                }
                                std::collections::hash_map::Entry::Vacant(entry) => {
                                    key_count_delta += 1;
                                    entry.insert(value.clone());
                                }
                            }
                            key_count_delta -=
                                i32::from(language.ordinary_strings.remove(&key).is_some());
                            language.deleted_strings.remove(&key);
                            if is_diff {
                                strings
                                    .push(get_language_pack_string_object_pluralized(&key, &value));
                            }
                            database_strings.push((
                                key,
                                format!(
                                    "2{}\0{}\0{}\0{}\0{}\0{}",
                                    value.zero_value,
                                    value.one_value,
                                    value.two_value,
                                    value.few_value,
                                    value.many_value,
                                    value.other_value
                                ),
                            ));
                        }
                        telegram_api::LangPackString::langPackStringDeleted(str) => {
                            if !is_valid_key(&str.key) {
                                tdutils::log_error!("Receive invalid key \"{}\"", str.key);
                                continue;
                            }
                            let key = str.key;
                            key_count_delta -=
                                i32::from(language.ordinary_strings.remove(&key).is_some());
                            key_count_delta -=
                                i32::from(language.pluralized_strings.remove(&key).is_some());
                            language.deleted_strings.insert(key.clone());
                            if is_diff {
                                strings.push(get_language_pack_string_object_deleted(&key));
                            }
                            database_strings.push((key, "3".to_owned()));
                        }
                    }
                }
                if !language.is_full {
                    for key in &keys {
                        if !language.language_has_string_unsafe(key) {
                            tdutils::log_error!("Doesn't receive key {key} from server");
                            language.deleted_strings.insert(key.clone());
                            if is_diff {
                                strings.push(get_language_pack_string_object_deleted(key));
                            }
                            database_strings.push((key.clone(), "3".to_owned()));
                        }
                    }
                }

                if key_count_delta != 0 {
                    new_key_count = language.key_count + key_count_delta;
                    language.key_count = new_key_count;
                }

                if keys.is_empty() && !is_diff {
                    assert!(new_database_version >= 0, "full pack without a new version");
                    language.is_full = true;
                    language.deleted_strings.clear();
                }
                new_is_full = language.is_full;

                if is_diff || (new_is_full && is_first) {
                    env.send_update(td_api::Update::updateLanguagePackStrings(
                        td_api::updateLanguagePackStrings {
                            localization_target: language_pack.clone(),
                            language_pack_id: language_code.clone(),
                            strings,
                        },
                    ));
                }
            }
        }
        if is_custom_language_code(&language_code) && new_database_version == -1 {
            new_database_version = 1;
        }

        let language = self
            .database
            .get_language(&language_pack, &language_code)
            .expect("language exists after the add");
        language.save_strings_to_database(
            new_database_version,
            new_is_full,
            new_key_count,
            database_strings,
        );

        if is_diff {
            let mut get_difference_queries: Vec<Promise<()>> = Vec::new();
            {
                let language = self
                    .database
                    .get_language(&language_pack, &language_code)
                    .expect("language must be non-null");
                if language.has_get_difference_query {
                    language.has_get_difference_query = false;
                    get_difference_queries = std::mem::take(&mut language.get_difference_queries);
                    is_version_changed = true;
                }
            }
            for mut query in get_difference_queries {
                query.set_value(());
            }
        }
        if is_version_changed
            && language_pack == self.language_pack
            && (language_code == self.language_code || language_code == self.base_language_code)
        {
            // send_closure_later(on_language_pack_version_changed) — the
            // collapsed line calls directly
            self.on_language_pack_version_changed(env, language_code != self.language_code, -1);
        }

        if !promise.is_empty() {
            let language = self
                .database
                .get_language(&language_pack, &language_code)
                .expect("language must be non-null");
            promise.set_value(get_language_pack_strings_object(language, &keys));
        }
    }

    /// `add_custom_server_language` (cpp:1534-1565).
    pub fn add_custom_server_language(&mut self, language_code: String, mut promise: Promise<()>) {
        if self.language_pack.is_empty() {
            promise.set_error(Status::error(
                400,
                "Option \"localization_target\" needs to be set first",
            ));
            return;
        }
        if !check_language_code_name(&language_code) {
            promise.set_error(Status::error(
                400,
                "Language pack ID must contain only letters, digits and hyphen",
            ));
            return;
        }
        if is_custom_language_code(&language_code) {
            promise.set_error(Status::error(
                400,
                "Custom local language pack can't be added through addCustomServerLanguagePack",
            ));
            return;
        }

        if self
            .database
            .get_language(&self.language_pack, &language_code)
            .is_none()
        {
            promise.set_error(Status::error(400, "Language pack not found"));
            return;
        }

        let Some(pack) = self.database.language_packs.get_mut(&self.language_pack) else {
            panic!("localization target pack missing");
        };
        let Some(info) = pack.all_server_language_pack_infos.get(&language_code) else {
            promise.set_error(Status::error(400, "Language pack info not found"));
            return;
        };
        let info = info.clone();
        pack.custom_language_pack_infos
            .insert(language_code.clone(), info.clone());
        if !pack.pack_kv.empty() {
            pack.pack_kv.set(
                language_code.as_bytes(),
                info.get_language_info_string().as_bytes(),
            );
        }

        promise.set_value(());
    }

    /// `set_custom_language` (cpp:1707-1742). The C++ nullptr-info band
    /// is the `None` arm here.
    pub fn set_custom_language(
        &mut self,
        env: &mut impl LanguagePackManagerEnv,
        language_pack_info: Option<td_api::LanguagePackInfo>,
        strings: Vec<td_api::LanguagePackString>,
        mut promise: Promise<()>,
    ) {
        if self.language_pack.is_empty() {
            promise.set_error(Status::error(
                400,
                "Option \"localization_target\" needs to be set first",
            ));
            return;
        }

        let Some(language_pack_info) = language_pack_info else {
            // get_language_info(nullptr) (cpp:1651-1653)
            promise.set_error(Status::error(400, "Language pack info must be non-empty"));
            return;
        };
        let (language_code, language_info) = match get_language_info_from_td_api(language_pack_info)
        {
            Result::Ok(result) => result,
            Result::Err(status) => {
                promise.set_error(status);
                return;
            }
        };
        if !is_custom_language_code(&language_code) {
            promise.set_error(Status::error(
                400,
                "Custom language pack ID must begin with 'X'",
            ));
            return;
        }

        let mut server_strings: Vec<telegram_api::LangPackString> = Vec::new();
        for str in strings {
            match convert_to_telegram_api(str) {
                Result::Ok(result) => server_strings.push(result),
                Result::Err(status) => {
                    promise.set_error(status);
                    return;
                }
            }
        }

        // TODO atomic replace (the C++ comment); the C++ .ensure() aborts
        // on the parked-difference-query error band
        if let Result::Err(status) = self.do_delete_language(&language_code) {
            panic!("set_custom_language: do_delete_language failed: {status}");
        }
        self.on_get_language_pack_strings(
            env,
            self.language_pack.clone(),
            language_code.clone(),
            1,
            false,
            Vec::new(),
            server_strings,
            Promise::default(),
        );
        let Some(pack) = self.database.language_packs.get_mut(&self.language_pack) else {
            panic!("localization target pack missing");
        };
        pack.custom_language_pack_infos
            .insert(language_code.clone(), language_info);
        let info_string = pack
            .custom_language_pack_infos
            .get(&language_code)
            .map(|info| info.get_language_info_string());
        if !pack.pack_kv.empty() {
            let info_string = info_string.expect("just inserted");
            pack.pack_kv
                .set(language_code.as_bytes(), info_string.as_bytes());
        }

        promise.set_value(());
    }

    /// `edit_custom_language_info` (cpp:1744-1772). The C++ nullptr-info
    /// band is the `None` arm here.
    pub fn edit_custom_language_info(
        &mut self,
        language_pack_info: Option<td_api::LanguagePackInfo>,
        mut promise: Promise<()>,
    ) {
        if self.language_pack.is_empty() {
            promise.set_error(Status::error(
                400,
                "Option \"localization_target\" needs to be set first",
            ));
            return;
        }

        let Some(language_pack_info) = language_pack_info else {
            // get_language_info(nullptr) (cpp:1651-1653)
            promise.set_error(Status::error(400, "Language pack info must be non-empty"));
            return;
        };
        let (language_code, language_info) = match get_language_info_from_td_api(language_pack_info)
        {
            Result::Ok(result) => result,
            Result::Err(status) => {
                promise.set_error(status);
                return;
            }
        };
        if !is_custom_language_code(&language_code) {
            promise.set_error(Status::error(
                400,
                "Custom language pack ID must begin with 'X'",
            ));
            return;
        }

        let Some(pack) = self.database.language_packs.get_mut(&self.language_pack) else {
            panic!("localization target pack missing");
        };
        let Some(info) = pack.custom_language_pack_infos.get_mut(&language_code) else {
            promise.set_error(Status::error(400, "Custom language pack is not found"));
            return;
        };
        *info = language_info;
        let info_string = info.get_language_info_string();
        if !pack.pack_kv.empty() {
            pack.pack_kv
                .set(language_code.as_bytes(), info_string.as_bytes());
        }

        promise.set_value(());
    }

    /// `set_custom_language_string` (cpp:1774-1804). The C++ null-`str`
    /// band ("Language pack strings must not be null") is unrepresentable
    /// here — the Rust parameter is owned, never null.
    pub fn set_custom_language_string(
        &mut self,
        env: &mut impl LanguagePackManagerEnv,
        language_code: String,
        str: Option<td_api::LanguagePackString>,
        mut promise: Promise<()>,
    ) {
        if self.language_pack.is_empty() {
            promise.set_error(Status::error(
                400,
                "Option \"localization_target\" needs to be set first",
            ));
            return;
        }
        if !check_language_code_name(&language_code) {
            promise.set_error(Status::error(
                400,
                "Language pack ID must contain only letters, digits and hyphen",
            ));
            return;
        }
        if !is_custom_language_code(&language_code) {
            promise.set_error(Status::error(
                400,
                "Custom language pack ID must begin with 'X'",
            ));
            return;
        }

        if self
            .database
            .get_language(&self.language_pack, &language_code)
            .is_none()
        {
            promise.set_error(Status::error(400, "Custom language pack not found"));
            return;
        }
        let Some(str) = str else {
            // the C++ null-str band (cpp:1790-1792)
            promise.set_error(Status::error(400, "Language pack strings must not be null"));
            return;
        };

        let keys = vec![str.key.clone()];

        let lang_pack_string = match convert_to_telegram_api(str) {
            Result::Ok(result) => result,
            Result::Err(status) => {
                promise.set_error(status);
                return;
            }
        };

        let server_strings = vec![lang_pack_string];

        self.on_get_language_pack_strings(
            env,
            self.language_pack.clone(),
            language_code,
            1,
            true,
            keys,
            server_strings,
            Promise::default(),
        );
        promise.set_value(());
    }

    /// `delete_language` (cpp:1806-1826).
    pub fn delete_language(&mut self, language_code: String, mut promise: Promise<()>) {
        if self.language_pack.is_empty() {
            promise.set_error(Status::error(
                400,
                "Option \"localization_target\" needs to be set first",
            ));
            return;
        }
        if !check_language_code_name(&language_code) {
            promise.set_error(Status::error(400, "Language pack ID is invalid"));
            return;
        }
        if language_code.is_empty() {
            promise.set_error(Status::error(400, "Language pack ID is empty"));
            return;
        }
        if self.language_code == language_code || self.base_language_code == language_code {
            promise.set_error(Status::error(
                400,
                "Currently used language pack can't be deleted",
            ));
            return;
        }

        match self.do_delete_language(&language_code) {
            Result::Ok(()) => promise.set_value(()),
            Result::Err(status) => promise.set_error(status),
        }
    }

    /// `do_delete_language` (cpp:1828-1865) — wipes the language's
    /// in-memory state and kv table, then drops the custom info entry.
    fn do_delete_language(&mut self, language_code: &str) -> Result<()> {
        self.database
            .add_language(&self.language_pack, language_code);

        let database = self.database.database.clone();
        let Some(pack) = self.database.language_packs.get_mut(&self.language_pack) else {
            panic!("localization target pack missing");
        };
        let Some(language) = pack.languages.get_mut(language_code) else {
            panic!("language just added");
        };
        if language.has_get_difference_query {
            return Result::Err(Status::error(
                400,
                "Language pack can't be deleted now, try again later",
            ));
        }
        if !language.kv.empty() {
            let _ = language.kv.drop();
            assert!(language.kv.empty(), "kv must be empty after drop");
            assert!(!database.empty(), "language database must be open");
            language
                .kv
                .init_with_connection(
                    database,
                    &get_database_table_name(&self.language_pack, language_code),
                )
                .ensure();
        }
        language.version = -1;
        language.key_count = load_database_language_key_count(&mut language.kv);
        language.is_full = false;
        language.ordinary_strings.clear();
        language.pluralized_strings.clear();
        language.deleted_strings.clear();

        if !pack.pack_kv.empty() {
            pack.pack_kv.erase(language_code.as_bytes());
        }
        pack.custom_language_pack_infos.remove(language_code);

        Result::Ok(())
    }
}

/// `copy_language_pack_strings` — the fan-out copy helper of
/// `on_get_all_language_pack_strings` (cpp:1244-1250).
#[allow(dead_code)]
fn clone_language_pack_strings(
    strings: &td_api::LanguagePackStrings,
) -> td_api::LanguagePackStrings {
    td_api::LanguagePackStrings {
        strings: strings
            .strings
            .iter()
            .map(|result| td_api::LanguagePackString {
                key: result.key.clone(),
                value: result.value.as_ref().map(copy_language_pack_string_value),
            })
            .collect(),
    }
}

/// `on_failed_get_difference` (cpp:1514-1532) — the net-failure half of
/// the difference query; unreachable offline (the seam never answers),
/// kept for the exhaustiveness audit and driven by tests.
#[allow(dead_code)]
pub(crate) fn on_failed_get_difference(
    manager: &mut LanguagePackManager,
    env: &mut impl LanguagePackManagerEnv,
    language_pack: &str,
    language_code: &str,
    error: Status,
) {
    let (get_difference_queries, needs_version_changed) = {
        let language = manager
            .database
            .get_language(language_pack, language_code)
            .expect("language must be non-null");
        let mut queries = Vec::new();
        let mut needs_version_changed = false;
        if language.has_get_difference_query {
            language.has_get_difference_query = false;
            if language_pack == manager.language_pack
                && (language_code == manager.language_code
                    || language_code == manager.base_language_code)
            {
                needs_version_changed = true;
            }
            queries = std::mem::take(&mut language.get_difference_queries);
        }
        (queries, needs_version_changed)
    };
    // the C++ send_closure_later fires after the current handler, i.e.
    // after the promises fail — the direct call keeps that order
    if needs_version_changed {
        manager.on_language_pack_version_changed(env, language_code != manager.language_code, -1);
    }
    for mut query in get_difference_queries {
        query.set_error(error.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tdutils::promise::{Promise, PromiseInterface};

    // ===== the offline env stub =====

    /// A [`PromiseInterface`] that flips flags instead of answering requests —
    /// the offline stand-in for the deferred outbox.
    struct FlagPromise<T: Send + 'static> {
        value_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
        error_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
        error_code: std::sync::Arc<std::sync::atomic::AtomicI32>,
        _type: std::marker::PhantomData<T>,
    }

    impl<T: Send + 'static> FlagPromise<T> {
        /// `(promise, value_flag, error_flag, error_code)`.
        fn new() -> (
            FlagPromise<T>,
            std::sync::Arc<std::sync::atomic::AtomicBool>,
            std::sync::Arc<std::sync::atomic::AtomicBool>,
            std::sync::Arc<std::sync::atomic::AtomicI32>,
        ) {
            let value_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let error_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let error_code = std::sync::Arc::new(std::sync::atomic::AtomicI32::new(0));
            (
                FlagPromise {
                    value_flag: value_flag.clone(),
                    error_flag: error_flag.clone(),
                    error_code: error_code.clone(),
                    _type: std::marker::PhantomData,
                },
                value_flag,
                error_flag,
                error_code,
            )
        }
    }

    impl<T: Send + 'static> PromiseInterface<T> for FlagPromise<T> {
        fn set_value(&mut self, _value: T) {
            self.value_flag
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }

        fn set_error(&mut self, error: tdutils::status::Status) {
            self.error_code
                .store(error.code(), std::sync::atomic::Ordering::SeqCst);
            self.error_flag
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    fn promise_was_set(flags: &std::sync::Arc<std::sync::atomic::AtomicBool>) -> bool {
        flags.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn promise_error_code(flags: &std::sync::Arc<std::sync::atomic::AtomicI32>) -> i32 {
        flags.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The in-memory env: options in a map, the blocked net queries logged,
    /// `updateLanguagePackStrings` broadcasts buffered.
    struct TestEnv {
        options: HashMap<String, String>,
        updates: Vec<td_api::Update>,
        queries: Vec<String>,
    }

    impl TestEnv {
        fn new() -> TestEnv {
            TestEnv {
                options: HashMap::new(),
                updates: Vec::new(),
                queries: Vec::new(),
            }
        }

        fn with_chosen_language() -> TestEnv {
            let mut env = TestEnv::new();
            env.options
                .insert("localization_target".to_owned(), "android".to_owned());
            env.options
                .insert("language_pack_id".to_owned(), "en".to_owned());
            env
        }
    }

    impl LanguagePackManagerEnv for TestEnv {
        fn get_option_string(&self, name: &str, default_value: &str) -> String {
            self.options
                .get(name)
                .cloned()
                .unwrap_or_else(|| default_value.to_owned())
        }

        fn get_option_integer(&self, name: &str, default_value: i64) -> i64 {
            self.options
                .get(name)
                .and_then(|value| value.parse::<i64>().ok())
                .unwrap_or(default_value)
        }

        fn set_option_empty(&mut self, name: &str) {
            self.options.remove(name);
        }

        fn send_update(&mut self, update: td_api::Update) {
            self.updates.push(update);
        }

        fn send_get_languages_query(
            &mut self,
            _language_pack: String,
            _promise: Promise<td_api::LocalizationTargetInfo>,
        ) {
            self.queries.push("langpack.getLanguages".to_owned());
        }

        fn send_get_language_query(
            &mut self,
            _language_pack: String,
            _language_code: String,
            _promise: Promise<td_api::LanguagePackInfo>,
        ) {
            self.queries.push("langpack.getLanguage".to_owned());
        }

        fn send_get_lang_pack_query(&mut self, _language_pack: String, _language_code: String) {
            self.queries.push("langpack.getLangPack".to_owned());
        }

        fn send_get_strings_query(
            &mut self,
            _language_pack: String,
            _language_code: String,
            _keys: Vec<String>,
            _promise: Promise<td_api::LanguagePackStrings>,
        ) {
            self.queries.push("langpack.getStrings".to_owned());
        }

        fn send_get_difference_query(
            &mut self,
            _language_pack: String,
            _language_code: String,
            _from_version: i32,
        ) {
            self.queries.push("langpack.getDifference".to_owned());
        }
    }

    fn ordinary_string(key: &str, value: &str) -> td_api::LanguagePackString {
        td_api::LanguagePackString {
            key: key.to_owned(),
            value: Some(
                td_api::LanguagePackStringValue::languagePackStringValueOrdinary(
                    td_api::languagePackStringValueOrdinary {
                        value: value.to_owned(),
                    },
                ),
            ),
        }
    }

    fn pluralized_string(key: &str) -> td_api::LanguagePackString {
        td_api::LanguagePackString {
            key: key.to_owned(),
            value: Some(
                td_api::LanguagePackStringValue::languagePackStringValuePluralized(
                    td_api::languagePackStringValuePluralized {
                        zero_value: "z".to_owned(),
                        one_value: String::new(),
                        two_value: String::new(),
                        few_value: String::new(),
                        many_value: String::new(),
                        other_value: "o".to_owned(),
                    },
                ),
            ),
        }
    }

    fn string_value_text(value: &td_api::LanguagePackStringValue) -> String {
        match value {
            td_api::LanguagePackStringValue::languagePackStringValueOrdinary(v) => v.value.clone(),
            _ => panic!("expected an ordinary string value"),
        }
    }

    fn wire_ordinary_string(key: &str, value: &str) -> telegram_api::LangPackString {
        telegram_api::LangPackString::langPackString(telegram_api::langPackString {
            key: key.to_owned(),
            value: value.to_owned(),
        })
    }

    fn temp_path(name: &str) -> String {
        std::env::temp_dir()
            .join(name)
            .to_string_lossy()
            .into_owned()
    }

    fn default_custom_info() -> td_api::LanguagePackInfo {
        td_api::LanguagePackInfo {
            id: "X1".to_owned(),
            name: "My".to_owned(),
            native_name: "My".to_owned(),
            ..Default::default()
        }
    }

    // ===== the static checks =====

    #[test]
    fn check_language_pack_name_battery() {
        assert!(check_language_pack_name(""));
        assert!(check_language_pack_name("android"));
        assert!(check_language_pack_name("android_x"));
        assert!(check_language_pack_name("ANDROID"));
        assert!(!check_language_pack_name("android-x"));
        assert!(!check_language_pack_name("android.x"));
        assert!(!check_language_pack_name("android1"));
        assert!(check_language_pack_name(&"a".repeat(64)));
        assert!(!check_language_pack_name(&"a".repeat(65)));
    }

    #[test]
    fn check_language_code_name_battery() {
        assert!(check_language_code_name("")); // the empty code is legal
        assert!(check_language_code_name("en"));
        assert!(check_language_code_name("en-US"));
        assert!(check_language_code_name("X1")); // custom
        assert!(check_language_code_name("zh-hans-123"));
        assert!(!check_language_code_name("e")); // too short for a non-custom code
        assert!(!check_language_code_name("en_US"));
        assert!(!check_language_code_name("en.u"));
        assert!(check_language_code_name(&"a".repeat(64)));
        assert!(!check_language_code_name(&"a".repeat(65)));
    }

    #[test]
    fn custom_language_code_and_valid_key() {
        assert!(is_custom_language_code("X1"));
        assert!(is_custom_language_code("X"));
        assert!(!is_custom_language_code("x1"));
        assert!(!is_custom_language_code(""));
        assert!(!is_custom_language_code("en"));

        assert!(is_valid_key("key.one-two3"));
        assert!(!is_valid_key(""));
        assert!(!is_valid_key("ключ"));
        assert!(!is_valid_key("key space"));
        assert!(!is_valid_key("key\0name"));
    }

    // ===== the LanguageInfo plane =====

    #[test]
    fn language_info_string_is_byte_exact() {
        let info = LanguageInfo {
            name: "English".to_owned(),
            native_name: "English".to_owned(),
            base_language_code: String::new(),
            plural_code: String::new(),
            is_official: true,
            is_rtl: false,
            is_beta: true,
            is_from_database: false,
            total_string_count: 1234,
            translated_string_count: 1200,
            translation_url: "https://example.com".to_owned(),
        };
        // the C++ `operator<<` form: ten \x00-separated fields, bools as 1/0
        assert_eq!(
            info.get_language_info_string(),
            "English\x00English\x00\x00\x001\x000\x001\x001234\x001200\x00https://example.com"
        );
    }

    #[test]
    fn custom_language_pack_info_round_trip() {
        let info = LanguageInfo {
            name: "My Pack".to_owned(),
            native_name: "My Pack".to_owned(),
            base_language_code: "en".to_owned(),
            plural_code: "en".to_owned(),
            is_official: false,
            is_rtl: false,
            is_beta: false,
            is_from_database: false,
            total_string_count: 10,
            translated_string_count: 9,
            translation_url: "url".to_owned(),
        };
        let stored = info.get_language_info_string();
        let parsed = LanguageInfo::parse_custom(&stored).expect("parse must succeed");
        assert!(parsed.is_from_database);
        assert_eq!(parsed, info); // the C++ == skips is_from_database_

        // the 2-field short form
        let parsed = LanguageInfo::parse_custom("n\0nn").expect("parse must succeed");
        assert_eq!(parsed.name, "n");
        assert_eq!(parsed.native_name, "nn");
        assert!(parsed.is_from_database);

        // fewer than 2 fields fails
        assert!(LanguageInfo::parse_custom("only-name").is_none());
        assert!(LanguageInfo::parse_custom("").is_none());
    }

    #[test]
    fn server_language_pack_infos_round_trip() {
        let infos = vec![
            (
                "de".to_owned(),
                LanguageInfo {
                    name: "German".to_owned(),
                    native_name: "Deutsch".to_owned(),
                    base_language_code: String::new(),
                    plural_code: "de".to_owned(),
                    is_official: false,
                    is_rtl: false,
                    is_beta: false,
                    is_from_database: true,
                    total_string_count: 1,
                    translated_string_count: 2,
                    translation_url: "u1".to_owned(),
                },
            ),
            (
                "ar".to_owned(),
                LanguageInfo {
                    name: "Arabic".to_owned(),
                    native_name: "العربية".to_owned(),
                    base_language_code: "en".to_owned(),
                    plural_code: "ar".to_owned(),
                    is_official: false,
                    is_rtl: false,
                    is_beta: false,
                    is_from_database: true,
                    total_string_count: 3,
                    translated_string_count: 4,
                    translation_url: "u2".to_owned(),
                },
            ),
        ];
        // the !server2 form: code, then the ten info fields, per entry
        let mut blob = String::new();
        for (code, info) in &infos {
            if !blob.is_empty() {
                blob.push('\0');
            }
            blob.push_str(code);
            blob.push('\0');
            blob.push_str(&info.get_language_info_string());
        }
        assert_eq!(blob.split('\0').count(), 22); // 11 fields per info

        let parsed = LanguageInfo::parse_server_infos(&blob);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].0, "de");
        assert_eq!(parsed[0].1, infos[0].1);
        assert_eq!(parsed[1].0, "ar");
        assert_eq!(parsed[1].1, infos[1].1);

        // a wrong field count is rejected
        assert!(LanguageInfo::parse_server_infos("a\0b\0c").is_empty());
    }

    // ===== the wire converter =====

    fn wire_language(
        lang_code: &str,
        base_lang_code: &str,
        official: bool,
    ) -> telegram_api::LangPackLanguage {
        telegram_api::LangPackLanguage {
            flags: i32::from(official) | (i32::from(!base_lang_code.is_empty()) << 1),
            official,
            rtl: false,
            beta: false,
            name: "Name".to_owned(),
            native_name: "Native".to_owned(),
            lang_code: lang_code.to_owned(),
            base_lang_code: base_lang_code.to_owned(),
            plural_code: "plural".to_owned(),
            strings_count: 100,
            translated_count: 90,
            translations_url: "https://translations".to_owned(),
        }
    }

    #[test]
    fn get_language_info_from_wire_battery() {
        // the happy path: the code is lowercased, the info moves over
        let (code, info) = match get_language_info_from_wire(wire_language("EN-us", "en", true)) {
            Result::Ok(result) => result,
            Result::Err(status) => panic!("unexpected error: {status}"),
        };
        assert_eq!(code, "en-us");
        assert!(info.is_official);
        assert_eq!(info.base_language_code, "en");
        assert_eq!(info.total_string_count, 100);
        assert!(!info.is_from_database);

        // an unsupported code fails
        let error = match get_language_info_from_wire(wire_language("e", "", false)) {
            Result::Err(status) => status,
            Result::Ok(_) => panic!("expected an error"),
        };
        assert_eq!(error.code(), 500);
        assert_eq!(error.message(), "Unsupported language pack ID");

        // a custom code fails
        let error = match get_language_info_from_wire(wire_language("X1", "", false)) {
            Result::Err(status) => status,
            Result::Ok(_) => panic!("expected an error"),
        };
        assert_eq!(error.code(), 500);
        assert_eq!(error.message(), "Unallowed custom language pack ID");

        // an invalid base code is cleared
        let (_, info) = match get_language_info_from_wire(wire_language("en", "bad_base!", true)) {
            Result::Ok(result) => result,
            Result::Err(status) => panic!("unexpected error: {status}"),
        };
        assert!(info.base_language_code.is_empty());

        // a custom base code is cleared
        let (_, info) = match get_language_info_from_wire(wire_language("en", "X1", true)) {
            Result::Ok(result) => result,
            Result::Err(status) => panic!("unexpected error: {status}"),
        };
        assert!(info.base_language_code.is_empty());

        // a self-based code is cleared; the C++ compares the raw base
        // against the already-lowered code, so "EN" (lowered to "en" for the
        // comparison only after the check) survives — pin the C++ quirk
        let (_, info) = match get_language_info_from_wire(wire_language("en", "en", true)) {
            Result::Ok(result) => result,
            Result::Err(status) => panic!("unexpected error: {status}"),
        };
        assert!(info.base_language_code.is_empty());
        let (_, info) = match get_language_info_from_wire(wire_language("en", "EN", true)) {
            Result::Ok(result) => result,
            Result::Err(status) => panic!("unexpected error: {status}"),
        };
        assert_eq!(info.base_language_code, "EN");
    }

    // ===== the td_api converter =====

    #[test]
    fn get_language_info_from_td_api_battery() {
        let (id, parsed) = match get_language_info_from_td_api(td_api::LanguagePackInfo {
            id: "en-US".to_owned(),
            base_language_pack_id: "en".to_owned(),
            name: "English".to_owned(),
            native_name: "English".to_owned(),
            plural_code: "en".to_owned(),
            is_official: true,
            is_rtl: false,
            is_beta: false,
            is_installed: false,
            total_string_count: -5,
            translated_string_count: 7,
            local_string_count: 0,
            translation_url: "https://x".to_owned(),
        }) {
            Result::Ok(result) => result,
            Result::Err(status) => panic!("unexpected error: {status}"),
        };
        assert_eq!(id, "en-US");
        assert!(parsed.is_from_database);
        assert_eq!(parsed.total_string_count, 0); // the negative clamp

        // an invalid pack id fails
        let error = match get_language_info_from_td_api(td_api::LanguagePackInfo {
            id: "bad_id!".to_owned(),
            ..Default::default()
        }) {
            Result::Err(status) => status,
            Result::Ok(_) => panic!("expected an error"),
        };
        assert_eq!(error.code(), 400);
        assert_eq!(
            error.message(),
            "Language pack ID must contain only letters, digits and hyphen"
        );

        // a custom pack is sanitized: no base, no official/rtl/beta/url
        let (id, parsed) = match get_language_info_from_td_api(td_api::LanguagePackInfo {
            id: "X1".to_owned(),
            base_language_pack_id: "en".to_owned(),
            name: "n".to_owned(),
            native_name: "nn".to_owned(),
            plural_code: "p".to_owned(),
            is_official: true,
            is_rtl: true,
            is_beta: true,
            is_installed: false,
            total_string_count: 1,
            translated_string_count: 2,
            local_string_count: 0,
            translation_url: "https://x".to_owned(),
        }) {
            Result::Ok(result) => result,
            Result::Err(status) => panic!("unexpected error: {status}"),
        };
        assert_eq!(id, "X1");
        assert!(parsed.base_language_code.is_empty());
        assert!(!parsed.is_official);
        assert!(!parsed.is_rtl);
        assert!(!parsed.is_beta);
        assert!(parsed.translation_url.is_empty());
    }

    #[test]
    fn language_pack_info_object_fields() {
        let info = LanguageInfo {
            name: "German".to_owned(),
            native_name: "Deutsch".to_owned(),
            base_language_code: "en".to_owned(),
            plural_code: "de".to_owned(),
            is_official: true,
            is_rtl: false,
            is_beta: false,
            is_from_database: false,
            total_string_count: 10,
            translated_string_count: 9,
            translation_url: "https://translations".to_owned(),
        };
        let object = get_language_pack_info_object("de", &info);
        assert_eq!(object.id, "de");
        assert_eq!(object.base_language_pack_id, "en");
        assert_eq!(object.name, "German");
        assert_eq!(object.native_name, "Deutsch");
        assert_eq!(object.plural_code, "de");
        assert!(object.is_official);
        assert!(!object.is_rtl);
        assert!(!object.is_beta);
        assert!(!object.is_installed); // always false here, fixed up by callers
        assert_eq!(object.total_string_count, 10);
        assert_eq!(object.translated_string_count, 9);
        assert_eq!(object.local_string_count, 0); // same
        assert_eq!(object.translation_url, "https://translations");
    }

    // ===== convert_to_telegram_api =====

    #[test]
    fn convert_to_telegram_api_battery() {
        // ordinary
        let converted = match convert_to_telegram_api(ordinary_string("key1", "value1")) {
            Result::Ok(converted) => converted,
            Result::Err(status) => panic!("unexpected error: {status}"),
        };
        match converted {
            telegram_api::LangPackString::langPackString(str) => {
                assert_eq!(str.key, "key1");
                assert_eq!(str.value, "value1");
            }
            _ => panic!("expected an ordinary langPackString"),
        }

        // pluralized with all six fields and the explicit flags = 31
        let converted = match convert_to_telegram_api(pluralized_string("key2")) {
            Result::Ok(converted) => converted,
            Result::Err(status) => panic!("unexpected error: {status}"),
        };
        match converted {
            telegram_api::LangPackString::langPackStringPluralized(str) => {
                assert_eq!(str.flags, 31);
                assert_eq!(str.key, "key2");
                assert_eq!(str.zero_value, "z");
                assert_eq!(str.other_value, "o");
            }
            _ => panic!("expected a pluralized langPackString"),
        }

        // deleted (a null td_api value)
        let converted = match convert_to_telegram_api(td_api::LanguagePackString {
            key: "key3".to_owned(),
            value: None,
        }) {
            Result::Ok(converted) => converted,
            Result::Err(status) => panic!("unexpected error: {status}"),
        };
        match converted {
            telegram_api::LangPackString::langPackStringDeleted(str) => assert_eq!(str.key, "key3"),
            _ => panic!("expected a deleted langPackString"),
        }

        // an invalid key fails
        let error = match convert_to_telegram_api(ordinary_string("bad key", "v")) {
            Result::Err(status) => status,
            Result::Ok(_) => panic!("expected an error"),
        };
        assert_eq!(error.code(), 400);
        assert_eq!(error.message(), "Key is invalid");

        // control characters are cleaned away like the C++ clean_input_string
        let converted = match convert_to_telegram_api(ordinary_string("key4", "a\u{1}b")) {
            Result::Ok(converted) => converted,
            Result::Err(status) => panic!("unexpected error: {status}"),
        };
        match converted {
            telegram_api::LangPackString::langPackString(str) => assert_eq!(str.value, "a b"),
            _ => panic!("expected an ordinary langPackString"),
        }
    }

    // ===== the database plane =====

    #[test]
    fn database_plane_byte_exact_round_trip() {
        let path = temp_path("m24_langpack_db");
        let _ = tddb::sqlite_db::destroy(&path);

        let mut database = LanguageDatabase::open(&path);
        assert!(!database.database.empty());
        assert_eq!(database.path, path);

        {
            let language = database.add_language("android", "en");
            assert_eq!(language.version, -1);
            assert_eq!(language.key_count, 0);
            // the byte-exact kv value encodings: '1' + value, '2' + six
            // \x00-separated parts, "3" for deleted; the key count rides the
            // save like the C++ applier's new_key_count
            language.save_strings_to_database(
                7,
                false,
                2,
                vec![
                    ("key1".to_owned(), "1value1".to_owned()),
                    ("key2".to_owned(), "2z\0o\0t\0f\0m\0o2".to_owned()),
                    ("key3".to_owned(), "3".to_owned()),
                ],
            );
            // the C++ save writes only the kv; the in-memory version moves
            // in the applier
            assert_eq!(language.version, -1);
            // the raw table bytes (BLOB keys need BLOB parameters — the
            // sqlite type affinity never matches a TEXT literal)
            let mut stmt = database
                .database
                .get_statement("SELECT v FROM \"kv_android_en\" WHERE k = ?1")
                .unwrap();
            stmt.bind_blob(1, b"key2").ensure();
            stmt.step().ensure();
            assert_eq!(stmt.view_blob(0), b"2z\0o\0t\0f\0m\0o2".as_slice());
            stmt.reset();
            stmt.bind_blob(1, b"!version").ensure();
            stmt.step().ensure();
            assert_eq!(stmt.view_blob(0), b"7".as_slice());
            stmt.reset();
        }

        // reopen: the version/key_count/base load and the lazy string loads
        let mut database = LanguageDatabase::open(&path);
        let language = database.add_language("android", "en");
        assert_eq!(language.version, 7);
        assert_eq!(language.key_count, 2); // the saved !key_count
        assert!(!language.is_full);

        // a single keyed load
        assert!(language.load_language_strings(&["key1".to_owned()]));
        assert_eq!(
            language.ordinary_strings.get("key1").map(String::as_str),
            Some("value1")
        );

        // the full load promotes the language and clears the tombstones
        assert!(language.load_language_strings(&[]));
        assert!(language.is_full);
        assert!(language.deleted_strings.is_empty());
        let pluralized = language
            .pluralized_strings
            .get("key2")
            .expect("key2 loaded");
        assert_eq!(pluralized.zero_value, "z");
        assert_eq!(pluralized.other_value, "o2");

        // version gating: an older version never lands
        language.save_strings_to_database(5, false, -1, vec![("key4".to_owned(), "1x".to_owned())]);
        let mut stmt = database
            .database
            .get_statement("SELECT COUNT(*) FROM \"kv_android_en\" WHERE k = ?1")
            .unwrap();
        stmt.bind_blob(1, b"key4").ensure();
        stmt.step().ensure();
        assert_eq!(stmt.view_int32(0), 0, "the older version is discarded");

        let _ = tddb::sqlite_db::destroy(&path);
    }

    #[test]
    fn database_plane_key_count_fallback_and_full_deleted_erase() {
        let path = temp_path("m24_langpack_keycount");
        let _ = tddb::sqlite_db::destroy(&path);

        // prepopulate a language table through a probe, without the
        // !key_count cache entry (the legacy-database shape)
        {
            let database = LanguageDatabase::open(&path);
            let mut probe = tddb::sqlite_key_value::SqliteKeyValue::new();
            probe
                .init_with_connection(
                    database.database.clone(),
                    &get_database_table_name("android", "de"),
                )
                .ensure();
            probe.set(b"a", b"1va".as_slice());
            probe.set(b"b", b"3".as_slice());
            probe.set(b"!version", b"2".as_slice());
        }

        let mut database = LanguageDatabase::open(&path);
        let language = database.add_language("android", "de");
        assert_eq!(language.version, 2);
        assert_eq!(
            language.key_count, 1,
            "only the '1'-prefixed value counts on the fallback recomputation"
        );

        // a full save erases the "3" tombstones from the table
        language.save_strings_to_database(
            3,
            true,
            -1,
            vec![
                ("b".to_owned(), "3".to_owned()),
                ("c".to_owned(), "1vc".to_owned()),
            ],
        );
        let mut stmt = database
            .database
            .get_statement("SELECT COUNT(*) FROM \"kv_android_de\" WHERE k = ?1")
            .unwrap();
        stmt.bind_blob(1, b"b").ensure();
        stmt.step().ensure();
        assert_eq!(stmt.view_int32(0), 0, "the tombstone is erased when full");
        let _ = tddb::sqlite_db::destroy(&path);
    }

    #[test]
    fn pack_kv_custom_and_server_info_load() {
        let path = temp_path("m24_langpack_packkv");
        let _ = tddb::sqlite_db::destroy(&path);

        // phase 1: write the pack kv entries the C++ writes, through a
        // direct probe on the pack's "0" table
        let info = LanguageInfo {
            name: "German".to_owned(),
            native_name: "Deutsch".to_owned(),
            base_language_code: String::new(),
            plural_code: "de".to_owned(),
            is_official: false,
            is_rtl: false,
            is_beta: false,
            is_from_database: true,
            total_string_count: 5,
            translated_string_count: 4,
            translation_url: String::new(),
        };
        {
            let database = LanguageDatabase::open(&path);
            let mut probe = tddb::sqlite_key_value::SqliteKeyValue::new();
            probe
                .init_with_connection(
                    database.database.clone(),
                    &get_database_table_name("android", "0"),
                )
                .ensure();
            probe.set(b"de", info.get_language_info_string().as_bytes());
            let server_blob = "de\0".to_owned() + &info.get_language_info_string();
            probe.set(b"!server2", server_blob.as_bytes());
            probe.set(b"!server", b"legacy".as_slice());
        }

        // phase 2: a fresh load parses the pack kv entries
        let mut database = LanguageDatabase::open(&path);
        database.add_language("android", "en");
        let pack = database.language_packs.get("android").unwrap();
        assert_eq!(pack.custom_language_pack_infos.len(), 1);
        let custom = pack.custom_language_pack_infos.get("de").unwrap();
        assert_eq!(custom.name, "German");
        assert!(custom.is_from_database);
        assert_eq!(pack.server_language_pack_infos.len(), 1);
        assert_eq!(pack.server_language_pack_infos[0].0, "de");
        assert!(pack.all_server_language_pack_infos.contains_key("de"));

        // the legacy "!server" entry is dropped on load
        let mut probe = tddb::sqlite_key_value::SqliteKeyValue::new();
        probe
            .init_with_connection(
                database.database.clone(),
                &get_database_table_name("android", "0"),
            )
            .ensure();
        assert!(
            probe.get(b"!server").is_empty(),
            "the legacy cache is dropped"
        );

        let _ = tddb::sqlite_db::destroy(&path);
    }

    #[test]
    fn sync_get_language_pack_string_battery() {
        // the validation bands, in order
        let result = crate::language_pack_manager::get_language_pack_string("", "", "en", "key");
        match result {
            td_api::Object::error(error) => {
                assert_eq!(error.code, 400);
                assert_eq!(error.message, "Localization target is invalid");
            }
            _ => panic!("expected an error object"),
        }

        let result =
            crate::language_pack_manager::get_language_pack_string("", "android", "e", "key");
        match result {
            td_api::Object::error(error) => {
                assert_eq!(error.code, 400);
                assert_eq!(error.message, "Language pack ID is invalid");
            }
            _ => panic!("expected an error object"),
        }

        let result =
            crate::language_pack_manager::get_language_pack_string("", "android", "en", "");
        match result {
            td_api::Object::error(error) => {
                assert_eq!(error.code, 400);
                assert_eq!(error.message, "Key is invalid");
            }
            _ => panic!("expected an error object"),
        }

        // a fresh database answers 404 for an unknown key...
        let path = temp_path("m24_langpack_sync");
        let _ = tddb::sqlite_db::destroy(&path);
        let result = crate::language_pack_manager::get_language_pack_string(
            &path, "android", "en", "missing",
        );
        match result {
            td_api::Object::error(error) => {
                assert_eq!(error.code, 404);
                assert_eq!(error.message, "Not Found");
            }
            _ => panic!("expected the 404 error object"),
        }

        // ...then, after the plane writes through its own connection to the
        // same file, the sync path sees the committed bytes
        let mut database = LanguageDatabase::open(&path);
        let language = database.add_language("android", "en");
        language.save_strings_to_database(
            1,
            false,
            -1,
            vec![("found".to_owned(), "1value".to_owned())],
        );
        drop(database);
        let result =
            crate::language_pack_manager::get_language_pack_string(&path, "android", "en", "found");
        match result {
            td_api::Object::languagePackStringValueOrdinary(value) => {
                assert_eq!(value.value, "value")
            }
            _ => panic!("expected the ordinary string value object"),
        }
        let _ = tddb::sqlite_db::destroy(&path);
    }

    // ===== the manager plane =====

    #[test]
    fn manager_init_without_options_is_inert() {
        let mut manager = LanguagePackManager::new();
        let mut env = TestEnv::new();
        manager.init(&mut env);
        assert!(manager.language_pack.is_empty());
        assert!(env.queries.is_empty());
        assert!(env.updates.is_empty());
        assert_eq!(manager.get_main_language_code(&mut env), "en");
        assert!(manager.get_used_language_codes(&mut env).is_empty());
    }

    #[test]
    fn manager_init_loads_empty_pack_through_the_seam() {
        let mut manager = LanguagePackManager::new();
        let mut env = TestEnv::with_chosen_language();
        manager.init(&mut env);
        // start_up: the empty pack loads through langpack_getLangPack (once)
        // and the chosen-language info repair through langpack.getLanguage
        assert_eq!(
            env.queries
                .iter()
                .filter(|q| *q == "langpack.getLangPack")
                .count(),
            1,
            "the empty-pack load is deduplicated"
        );
        assert!(env.queries.contains(&"langpack.getLanguage".to_owned()));
    }

    #[test]
    fn set_custom_language_full_local_flow() {
        let mut manager = LanguagePackManager::new();
        let mut env = TestEnv::with_chosen_language();
        manager.init(&mut env);

        let (_p, value_flag, error_flag, _code) = FlagPromise::<()>::new();
        manager.set_custom_language(
            &mut env,
            Some(default_custom_info()),
            vec![ordinary_string("key1", "value1"), pluralized_string("key2")],
            Promise::from_interface(_p),
        );
        assert!(promise_was_set(&value_flag));
        assert!(!promise_was_set(&error_flag));

        // the is_first full set emits updateLanguagePackStrings; the strings
        // array only builds when is_diff, so it is empty here — the C++
        // sends the same empty array
        assert_eq!(env.updates.len(), 1);
        match &env.updates[0] {
            td_api::Update::updateLanguagePackStrings(update) => {
                assert_eq!(update.localization_target, "android");
                assert_eq!(update.language_pack_id, "X1");
                assert!(update.strings.is_empty());
            }
            _ => panic!("expected updateLanguagePackStrings"),
        }

        // the local get answers from the loaded pack
        let (_p, value_flag, error_flag, _code) = FlagPromise::<td_api::LanguagePackStrings>::new();
        manager.get_language_pack_strings(
            &mut env,
            "X1".to_owned(),
            vec!["key1".to_owned(), "key2".to_owned()],
            Promise::from_interface(_p),
        );
        assert!(promise_was_set(&value_flag));
        assert!(!promise_was_set(&error_flag));
        assert!(!env.queries.contains(&"langpack.getStrings".to_owned()));

        // a non-custom id fails
        let (_p, _v, error_flag, error_code) = FlagPromise::<()>::new();
        manager.set_custom_language(
            &mut env,
            Some(td_api::LanguagePackInfo {
                id: "en-US".to_owned(),
                ..Default::default()
            }),
            Vec::new(),
            Promise::from_interface(_p),
        );
        assert!(promise_was_set(&error_flag));
        assert_eq!(promise_error_code(&error_code), 400);

        // an invalid string key fails
        let (_p, _v, error_flag, error_code) = FlagPromise::<()>::new();
        manager.set_custom_language(
            &mut env,
            Some(default_custom_info()),
            vec![ordinary_string("bad key", "v")],
            Promise::from_interface(_p),
        );
        assert!(promise_was_set(&error_flag));
        assert_eq!(promise_error_code(&error_code), 400);
    }

    #[test]
    fn set_custom_language_string_diff_flow() {
        let mut manager = LanguagePackManager::new();
        let mut env = TestEnv::with_chosen_language();
        manager.init(&mut env);
        manager.set_custom_language(
            &mut env,
            Some(default_custom_info()),
            Vec::new(),
            Promise::default(),
        );
        env.updates.clear();

        // the set answers ok and emits the diff update
        let (_p, value_flag, _error_flag, _code) = FlagPromise::<()>::new();
        manager.set_custom_language_string(
            &mut env,
            "X1".to_owned(),
            Some(ordinary_string("key1", "value1")),
            Promise::from_interface(_p),
        );
        assert!(promise_was_set(&value_flag));
        assert_eq!(env.updates.len(), 1);
        match &env.updates[0] {
            td_api::Update::updateLanguagePackStrings(update) => {
                assert_eq!(update.language_pack_id, "X1");
                assert_eq!(update.strings.len(), 1);
                assert_eq!(
                    string_value_text(update.strings[0].value.as_ref().expect("value")),
                    "value1"
                );
            }
            _ => panic!("expected updateLanguagePackStrings"),
        }

        // a second get answers locally with the set string
        let (_p, value_flag, error_flag, _code) = FlagPromise::<td_api::LanguagePackStrings>::new();
        manager.get_language_pack_strings(
            &mut env,
            "X1".to_owned(),
            vec!["key1".to_owned()],
            Promise::from_interface(_p),
        );
        assert!(promise_was_set(&value_flag));
        assert!(!promise_was_set(&error_flag));

        // an unknown custom pack fails
        let (_p, _v, error_flag, error_code) = FlagPromise::<()>::new();
        manager.set_custom_language_string(
            &mut env,
            "X2".to_owned(),
            Some(ordinary_string("key1", "value1")),
            Promise::from_interface(_p),
        );
        assert!(promise_was_set(&error_flag));
        assert_eq!(promise_error_code(&error_code), 400);
    }

    #[test]
    fn edit_custom_language_info_and_delete_language() {
        let mut manager = LanguagePackManager::new();
        let mut env = TestEnv::with_chosen_language();
        manager.init(&mut env);
        manager.set_custom_language(
            &mut env,
            Some(default_custom_info()),
            Vec::new(),
            Promise::default(),
        );

        // the edit renames the pack
        let (_p, value_flag, error_flag, _code) = FlagPromise::<()>::new();
        manager.edit_custom_language_info(
            Some(td_api::LanguagePackInfo {
                id: "X1".to_owned(),
                name: "Renamed".to_owned(),
                native_name: "My".to_owned(),
                ..Default::default()
            }),
            Promise::from_interface(_p),
        );
        assert!(promise_was_set(&value_flag));
        assert!(!promise_was_set(&error_flag));
        let pack = manager.database.language_packs.get("android").unwrap();
        assert_eq!(
            pack.custom_language_pack_infos.get("X1").unwrap().name,
            "Renamed"
        );

        // the edit of an unknown pack fails
        let (_p, _v, error_flag, error_code) = FlagPromise::<()>::new();
        manager.edit_custom_language_info(
            Some(td_api::LanguagePackInfo {
                id: "X9".to_owned(),
                ..Default::default()
            }),
            Promise::from_interface(_p),
        );
        assert!(promise_was_set(&error_flag));
        assert_eq!(promise_error_code(&error_code), 400);

        // the delete of the used language fails
        let (_p, _v, error_flag, error_code) = FlagPromise::<()>::new();
        manager.delete_language("en".to_owned(), Promise::from_interface(_p));
        assert!(promise_was_set(&error_flag));
        assert_eq!(promise_error_code(&error_code), 400);

        // the delete of X1 succeeds
        let (_p, value_flag, error_flag, _code) = FlagPromise::<()>::new();
        manager.delete_language("X1".to_owned(), Promise::from_interface(_p));
        assert!(promise_was_set(&value_flag));
        assert!(!promise_was_set(&error_flag));
        let pack = manager.database.language_packs.get("android").unwrap();
        assert!(!pack.custom_language_pack_infos.contains_key("X1"));

        // then a string get for the deleted custom pack reports the local 400
        let (_p, _v, error_flag, error_code) = FlagPromise::<td_api::LanguagePackStrings>::new();
        manager.get_language_pack_strings(
            &mut env,
            "X1".to_owned(),
            vec!["key1".to_owned()],
            Promise::from_interface(_p),
        );
        assert!(promise_was_set(&error_flag));
        assert_eq!(promise_error_code(&error_code), 400);
    }

    #[test]
    fn get_language_pack_strings_parks_and_fans_out() {
        let mut manager = LanguagePackManager::new();
        let mut env = TestEnv::with_chosen_language();
        manager.init(&mut env);

        // two all-strings requests: the second must not re-query
        let promise1 = FlagPromise::<td_api::LanguagePackStrings>::new();
        manager.get_language_pack_strings(
            &mut env,
            "en".to_owned(),
            Vec::new(),
            Promise::from_interface(promise1.0),
        );
        let promise2 = FlagPromise::<td_api::LanguagePackStrings>::new();
        manager.get_language_pack_strings(
            &mut env,
            "en".to_owned(),
            Vec::new(),
            Promise::from_interface(promise2.0),
        );
        assert_eq!(
            env.queries
                .iter()
                .filter(|q| *q == "langpack.getLangPack")
                .count(),
            1,
            "only one getLangPack goes out"
        );

        // the net response half fans the parked promises out
        manager.on_get_all_language_pack_strings(
            "android".to_owned(),
            "en".to_owned(),
            Result::Ok(td_api::LanguagePackStrings {
                strings: vec![ordinary_string("k", "v")],
            }),
        );
        assert!(promise_was_set(&promise1.1));
        assert!(promise_was_set(&promise2.1));

        // the map is drained
        assert!(
            manager.get_all_language_pack_strings_queries.is_empty(),
            "the parked queries drain after the fan-out"
        );

        // a keyed request for a missing key parks on langpack.getStrings
        let keyed = FlagPromise::<td_api::LanguagePackStrings>::new();
        manager.get_language_pack_strings(
            &mut env,
            "en".to_owned(),
            vec!["missing".to_owned()],
            Promise::from_interface(keyed.0),
        );
        assert!(env.queries.contains(&"langpack.getStrings".to_owned()));
        assert!(
            !promise_was_set(&keyed.1),
            "the keyed promise parks unanswered offline"
        );

        // the error fan-out fails every parked promise
        let error_promise = FlagPromise::<td_api::LanguagePackStrings>::new();
        manager.get_language_pack_strings(
            &mut env,
            "de".to_owned(),
            Vec::new(),
            Promise::from_interface(error_promise.0),
        );
        manager.on_get_all_language_pack_strings(
            "android".to_owned(),
            "de".to_owned(),
            Result::Err(Status::error(429, "Too Many Requests")),
        );
        assert!(promise_was_set(&error_promise.2));
        assert_eq!(promise_error_code(&error_promise.3), 429);
    }

    #[test]
    fn difference_query_parks_and_resolves() {
        let mut manager = LanguagePackManager::new();
        let mut env = TestEnv::with_chosen_language();
        manager.init(&mut env);

        // seed a version 4 full pack through the getLangPack response half
        // (on_get_language_pack_strings applies; on_get_all only fans out)
        manager.on_get_language_pack_strings(
            &mut env,
            "android".to_owned(),
            "en".to_owned(),
            4,
            false,
            Vec::new(),
            vec![wire_ordinary_string("k1", "v1")],
            Promise::default(),
        );
        {
            let language = manager.database.get_language("android", "en").unwrap();
            assert_eq!(language.version, 4);
            assert!(language.is_full);
        }

        // synchronize parks the promise behind langpack.getDifference
        let (_p, value_flag, _error_flag, _code) = FlagPromise::<()>::new();
        manager.synchronize_language_pack(&mut env, "en".to_owned(), Promise::from_interface(_p));
        assert!(env.queries.contains(&"langpack.getDifference".to_owned()));
        assert!(
            !promise_was_set(&value_flag),
            "the difference promise parks offline"
        );
        {
            let language = manager.database.get_language("android", "en").unwrap();
            assert!(language.has_get_difference_query);
            assert_eq!(language.get_difference_queries.len(), 1);
        }

        // the wire difference applies and resolves the parked promise
        manager.on_update_language_pack(
            &mut env,
            telegram_api::LangPackDifference {
                lang_code: "EN".to_owned(), // lowercased on intake
                from_version: 4,
                version: 5,
                strings: vec![wire_ordinary_string("k2", "v2")],
            },
        );
        assert!(
            promise_was_set(&value_flag),
            "the parked difference promise resolves after the apply"
        );

        // the string landed, the diff update went out, the version bumped
        let language = manager.database.get_language("android", "en").unwrap();
        assert_eq!(language.version, 5);
        assert_eq!(
            language.ordinary_strings.get("k2").map(String::as_str),
            Some("v2")
        );
        let last = env.updates.last().expect("an update went out");
        match last {
            td_api::Update::updateLanguagePackStrings(update) => {
                assert_eq!(update.language_pack_id, "en");
                assert_eq!(update.strings.len(), 1);
                assert_eq!(update.strings[0].key, "k2");
            }
            _ => panic!("expected updateLanguagePackStrings"),
        }

        // an already-applied difference is skipped
        let updates_before = env.updates.len();
        manager.on_update_language_pack(
            &mut env,
            telegram_api::LangPackDifference {
                lang_code: "en".to_owned(),
                from_version: 0,
                version: 5,
                strings: vec![wire_ordinary_string("k3", "v3")],
            },
        );
        assert_eq!(
            env.updates.len(),
            updates_before,
            "the stale difference is skipped"
        );
        let language = manager.database.get_language("android", "en").unwrap();
        assert!(!language.ordinary_strings.contains_key("k3"));
    }

    #[test]
    fn on_get_languages_local_assembly_and_server_save() {
        let mut manager = LanguagePackManager::new();
        // the longer code exercises the chosen-info lookups (the C++
        // 2-char shortcut returns early)
        let mut env = TestEnv::new();
        env.options
            .insert("localization_target".to_owned(), "android".to_owned());
        env.options
            .insert("language_pack_id".to_owned(), "en-us".to_owned());
        manager.init(&mut env);
        manager.set_custom_language(
            &mut env,
            Some(default_custom_info()),
            Vec::new(),
            Promise::default(),
        );

        // only_local answers synchronously with the custom pack installed
        let (_p, value_flag, _error_flag, _code) =
            FlagPromise::<td_api::LocalizationTargetInfo>::new();
        manager.get_languages(&mut env, true, Promise::from_interface(_p));
        assert!(promise_was_set(&value_flag));

        // the wire languages register the server infos (the deduplicated
        // lowered codes, in arrival order)
        let (_p, value_flag, _error_flag, _code) =
            FlagPromise::<td_api::LocalizationTargetInfo>::new();
        manager.on_get_languages(
            &mut env,
            vec![
                wire_language("DE", "", true),
                wire_language("AR", "en", false),
            ],
            "android".to_owned(),
            false,
            Promise::from_interface(_p),
        );
        assert!(promise_was_set(&value_flag));

        let pack = manager.database.language_packs.get("android").unwrap();
        assert_eq!(pack.server_language_pack_infos.len(), 2);
        assert_eq!(pack.server_language_pack_infos[0].0, "de");
        assert_eq!(pack.server_language_pack_infos[1].0, "ar");
        assert!(pack.all_server_language_pack_infos.contains_key("de"));
        assert!(pack.all_server_language_pack_infos.contains_key("ar"));

        // no chosen info yet: both accessors fall back and fire the
        // getLanguages repair query
        let queries_before = env.queries.len();
        assert_eq!(
            manager.get_used_language_codes(&mut env),
            Vec::<String>::new()
        );
        assert!(
            env.queries.len() > queries_before,
            "the repair query went out"
        );

        // once the chosen language's info arrives (exact code match), the
        // codes come from it: the plural code for both accessors
        let (_p, value_flag, _e, _c) = FlagPromise::<td_api::LocalizationTargetInfo>::new();
        manager.on_get_languages(
            &mut env,
            vec![wire_language("EN-US", "", true)],
            "android".to_owned(),
            false,
            Promise::from_interface(_p),
        );
        assert!(promise_was_set(&value_flag));
        let used = manager.get_used_language_codes(&mut env);
        assert_eq!(used, vec!["plural".to_owned()]);
        assert_eq!(manager.get_main_language_code(&mut env), "plural");
    }

    #[test]
    fn add_custom_server_language_bands() {
        let mut manager = LanguagePackManager::new();
        let mut env = TestEnv::with_chosen_language();
        manager.init(&mut env);

        // a custom code can't ride this arm
        let (_p, _v, error_flag, error_code) = FlagPromise::<()>::new();
        manager.add_custom_server_language("X1".to_owned(), Promise::from_interface(_p));
        assert!(promise_was_set(&error_flag));
        assert_eq!(promise_error_code(&error_code), 400);

        // an unknown code fails
        let (_p, _v, error_flag, error_code) = FlagPromise::<()>::new();
        manager.add_custom_server_language("zz".to_owned(), Promise::from_interface(_p));
        assert!(promise_was_set(&error_flag));
        assert_eq!(promise_error_code(&error_code), 400);

        // a known server language moves its info into the custom map
        let (_p, _v, _e, _c) = FlagPromise::<td_api::LocalizationTargetInfo>::new();
        manager.on_get_languages(
            &mut env,
            vec![wire_language("de", "", true)],
            "android".to_owned(),
            false,
            Promise::from_interface(_p),
        );
        let (_p, value_flag, error_flag, _code) = FlagPromise::<()>::new();
        manager.add_custom_server_language("de".to_owned(), Promise::from_interface(_p));
        assert!(promise_was_set(&value_flag));
        assert!(!promise_was_set(&error_flag));
        let pack = manager.database.language_packs.get("android").unwrap();
        assert!(pack.custom_language_pack_infos.contains_key("de"));
        assert_eq!(
            pack.custom_language_pack_infos
                .get("de")
                .expect("de info")
                .name,
            "Name"
        );
    }

    #[test]
    fn language_pack_too_long_triggers_the_difference_machine() {
        let mut manager = LanguagePackManager::new();
        let mut env = TestEnv::with_chosen_language();
        env.options
            .insert("language_pack_version".to_owned(), "9".to_owned());
        manager.init(&mut env);

        // seed a version 4 full pack through the getLangPack response half
        manager.on_get_language_pack_strings(
            &mut env,
            "android".to_owned(),
            "en".to_owned(),
            4,
            false,
            Vec::new(),
            Vec::new(),
            Promise::default(),
        );

        // too long for the used language: the version option (9) drives a
        // difference query
        manager.on_language_pack_too_long(&mut env, "en".to_owned());
        assert!(env.queries.contains(&"langpack.getDifference".to_owned()));

        // an unknown language only logs
        let queries_before = env.queries.len();
        manager.on_language_pack_too_long(&mut env, "unknown".to_owned());
        assert_eq!(env.queries.len(), queries_before);
    }

    #[test]
    fn on_failed_get_difference_fails_the_parked_promises() {
        let mut manager = LanguagePackManager::new();
        let mut env = TestEnv::with_chosen_language();
        env.options
            .insert("language_pack_version".to_owned(), "5".to_owned());
        manager.init(&mut env);

        let (_p, _value_flag, error_flag, error_code) = FlagPromise::<()>::new();
        manager.synchronize_language_pack(&mut env, "en".to_owned(), Promise::from_interface(_p));

        crate::language_pack_manager::on_failed_get_difference(
            &mut manager,
            &mut env,
            "android",
            "en",
            Status::error(429, "Too Many Requests: try again later"),
        );
        assert!(promise_was_set(&error_flag));
        assert_eq!(promise_error_code(&error_code), 429);
        let language = manager.database.get_language("android", "en").unwrap();
        assert!(!language.has_get_difference_query);
    }

    #[test]
    fn synchronize_language_pack_validation_bands() {
        let mut manager = LanguagePackManager::new();
        let mut env = TestEnv::with_chosen_language();

        // an invalid code
        let promise = FlagPromise::<()>::new();
        manager.synchronize_language_pack(
            &mut env,
            "bad!".to_owned(),
            Promise::from_interface(promise.0),
        );
        assert!(promise_was_set(&promise.2));
        assert_eq!(promise_error_code(&promise.3), 400);

        // no localization target
        let mut bare_env = TestEnv::new();
        let promise = FlagPromise::<()>::new();
        manager.synchronize_language_pack(
            &mut bare_env,
            "en".to_owned(),
            Promise::from_interface(promise.0),
        );
        assert!(promise_was_set(&promise.2));
        assert_eq!(promise_error_code(&promise.3), 400);

        // the empty code
        let promise = FlagPromise::<()>::new();
        manager.synchronize_language_pack(
            &mut env,
            String::new(),
            Promise::from_interface(promise.0),
        );
        assert!(promise_was_set(&promise.2));
        assert_eq!(promise_error_code(&promise.3), 400);
    }

    #[test]
    fn option_change_regeneration() {
        let mut manager = LanguagePackManager::new();
        let mut env = TestEnv::with_chosen_language();
        manager.init(&mut env);

        // the same values: no regeneration
        let queries_before = env.queries.len();
        manager.on_language_code_changed(&mut env);
        assert_eq!(env.queries.len(), queries_before);

        // a new language code regenerates (the option read comes back with
        // the new value through the env)
        env.options
            .insert("language_pack_id".to_owned(), "de".to_owned());
        manager.on_language_code_changed(&mut env);
        assert_eq!(manager.language_code, "de");
        assert!(
            !env.options.contains_key("language_pack_version"),
            "the version options reset"
        );
        assert!(!env.options.contains_key("base_language_pack_version"));

        env.options
            .insert("localization_target".to_owned(), "ios".to_owned());
        manager.on_language_pack_changed(&mut env);
        assert_eq!(manager.language_pack, "ios");
    }
}
