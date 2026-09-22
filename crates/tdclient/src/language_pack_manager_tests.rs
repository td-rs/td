//! The offline [`LanguagePackManagerEnv`] stub and the M.24 unit tests:
//! the options state lives in a plain map, every blocked net query is
//! recorded by name with its promise dropped (the unanswered-request
//! convention), and the database-plane tests run the real sqlite
//! `SqliteKeyValue` plane over temp files, pinning the byte-exact
//! `kv_<pack>_<code>` encodings the C++ writes.

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

    let result = crate::language_pack_manager::get_language_pack_string("", "android", "e", "key");
    match result {
        td_api::Object::error(error) => {
            assert_eq!(error.code, 400);
            assert_eq!(error.message, "Language pack ID is invalid");
        }
        _ => panic!("expected an error object"),
    }

    let result = crate::language_pack_manager::get_language_pack_string("", "android", "en", "");
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
    let result =
        crate::language_pack_manager::get_language_pack_string(&path, "android", "en", "missing");
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
        td_api::Object::languagePackStringValueOrdinary(value) => assert_eq!(value.value, "value"),
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
    let (_p, value_flag, _error_flag, _code) = FlagPromise::<td_api::LocalizationTargetInfo>::new();
    manager.get_languages(&mut env, true, Promise::from_interface(_p));
    assert!(promise_was_set(&value_flag));

    // the wire languages register the server infos (the deduplicated
    // lowered codes, in arrival order)
    let (_p, value_flag, _error_flag, _code) = FlagPromise::<td_api::LocalizationTargetInfo>::new();
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
    manager.synchronize_language_pack(&mut env, String::new(), Promise::from_interface(promise.0));
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
