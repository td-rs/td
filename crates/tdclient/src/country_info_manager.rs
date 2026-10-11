//! Port of `td/telegram/CountryInfoManager.{h,cpp}` (the countries glue
//! slice): the per-language-code country list cache with the compiled-in
//! "en" fallback list, the phone-number → calling-code prefix matcher and
//! its pattern formatter, and the two statics `get_country_flag_emoji` +
//! `get_phone_number_info_sync` (the SynchronousRequests arms). The
//! `help.getNearestDc` / `help.getCountriesList` queries ride the
//! client-side dispatcher under their `CountryNetKind` (the unauth send
//! legs — `create_unauth`); the answers drain through
//! `Td::on_country_net_answer` into `on_get_country_list`, whose pending
//! lot carries the re-entry descriptors (the C++ closure's
//! `do_get_*(is_recursive = true)` send).
//!
//! C++ shape notes:
//! - the C++ `countries_` cache and the fragment-prefix statics are
//!   process globals under `country_mutex_`; the Rust keeps two dedicated
//!   mutexes (the C++ `is_fragment_phone_number` mutates its statics
//!   under the *caller's* lock, the Rust helper is self-contained);
//! - the `manager_count_` teardown (`tear_down` clears `countries_` when
//!   the last manager dies) has no collapsed hook — the cache lives for
//!   the process lifetime, invisible outside multi-Td processes.

use std::collections::HashMap;
use std::sync::Mutex;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use tdmtproto::telegram_api;
use tdutils::promise::Promise;
use tdutils::status::Status;

use crate::td_api;

/// `CountryInfoManager::CallingCodeInfo` (cpp:89-93).
struct CallingCodeInfo {
    calling_code: String,
    prefixes: Vec<String>,
    patterns: Vec<String>,
}

/// `CountryInfoManager::CountryInfo` (cpp:95-107).
struct CountryInfo {
    country_code: String,
    default_name: String,
    name: String,
    calling_codes: Vec<CallingCodeInfo>,
    is_hidden: bool,
}

impl CountryInfo {
    /// `CountryInfo::get_country_info_object` (cpp:102-106).
    fn get_country_info_object(&self) -> td_api::CountryInfo {
        td_api::CountryInfo {
            country_code: self.country_code.clone(),
            name: if self.name.is_empty() {
                self.default_name.clone()
            } else {
                self.name.clone()
            },
            english_name: self.default_name.clone(),
            flag_emoji: get_country_flag_emoji(&self.country_code),
            is_hidden: self.is_hidden,
            calling_codes: self
                .calling_codes
                .iter()
                .map(|info| info.calling_code.clone())
                .collect(),
        }
    }
}

/// `CountryInfoManager::CountryList` (cpp:109-127) — the `Default` is the
/// C++ NSDMI zero state (`hash = 0`, `next_reload_time = 0.0`), what the
/// `countries_[language_code]` operator[] insert creates.
#[derive(Default)]
struct CountryList {
    countries: Vec<CountryInfo>,
    hash: i32,
    next_reload_time: f64,
}

impl CountryList {
    /// `CountryList::get_countries_object` (cpp:114-117).
    fn get_countries_object(&self) -> td_api::Countries {
        td_api::Countries {
            countries: self
                .countries
                .iter()
                .map(CountryInfo::get_country_info_object)
                .collect(),
        }
    }

    /// `CountryList::get_country_info_object` (cpp:119-126).
    fn get_country_info_object(&self, country_code: &str) -> Option<td_api::CountryInfo> {
        self.countries
            .iter()
            .find(|info| info.country_code == country_code)
            .map(CountryInfo::get_country_info_object)
    }
}

/// `CountryInfoManager::countries_` (cpp:658) — the process-global cache.
/// The C++ `tear_down` clear on the last dying manager has no collapsed
/// hook (see the module docs).
static COUNTRIES: std::sync::LazyLock<Mutex<HashMap<String, CountryList>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// `fragment_prefixes_str_` + `fragment_prefixes_` (cpp:659-660). The C++
/// helper reads them under the caller's `country_mutex_`; the Rust keeps
/// a dedicated lock so the helper is self-contained. The pair survives
/// the manager teardown in the C++ too.
static FRAGMENT_PREFIXES: Mutex<(String, Vec<String>)> = Mutex::new((String::new(), Vec::new()));

/// The merged `CountryInfoManager` — the pending-load dedup is the only
/// per-manager state (`pending_load_country_queries_`, cpp:95); the cache
/// and the fragment prefixes are process statics like the C++.
#[derive(Default)]
pub(crate) struct CountryInfoManager {
    pending_load_country_queries: HashMap<String, Vec<PendingCountryQuery>>,
}

/// The pending-lot entry — the C++ parks a `Promise<Unit>` whose lambda
/// re-enters the matching `do_get_*` with `is_recursive = true` on the
/// query completion; the port parks the re-entry descriptor itself (the
/// `load_country_promise` closure can't reach the manager).
pub(crate) enum PendingCountryQuery {
    /// `do_get_countries`'s re-entry (cpp:220-228).
    Countries { promise: Promise<td_api::Countries> },
    /// `do_get_country`'s re-entry (cpp:260-268).
    Country {
        country_code: String,
        promise: Promise<Option<td_api::CountryInfo>>,
    },
    /// `do_get_phone_number_info`'s re-entry (cpp:303-311).
    PhoneNumber {
        phone_number_prefix: String,
        promise: Promise<td_api::PhoneNumberInfo>,
    },
    /// The background reload's noop promise (the C++ `Promise<Unit>()`).
    Background,
}

/// The [`CountryInfoManager`] seams over the `Td` fields — the env
/// convention (`ConnectionCreatorEnv`).
pub(crate) trait CountryInfoManagerEnv {
    /// `G()->close_flag()`.
    fn close_flag(&self) -> bool;

    /// `td_->auth_manager_->is_bot()`.
    fn is_bot(&self) -> bool;

    /// `td_->option_manager_->have_option`.
    fn have_option(&self, name: &str) -> bool;

    /// `td_->option_manager_->get_option_string`.
    fn get_option_string(&self, name: &str, default_value: &str) -> String;

    /// `to_lower(td_->language_pack_manager_->get_main_language_code())`
    /// (cpp:188-190).
    fn get_main_language_code(&mut self) -> String;

    /// `GetNearestDcQuery::send` (cpp:37-39) — the query rides the
    /// dispatcher; the promise parks at the Td-side lot until the answer.
    fn get_nearest_dc_via_net(&mut self, promise: Promise<String>);

    /// `GetCountriesListQuery::send` (cpp:67-69) — the query rides the
    /// dispatcher; the queued pending entries stay in the manager until
    /// `on_get_country_list` completes them.
    fn get_countries_list_via_net(&mut self, language_code: &str, hash: i32);
}

impl CountryInfoManager {
    /// The constructor half (`CountryInfoManager::CountryInfoManager`,
    /// cpp:129-131): the fragment-prefixes option read. The actor
    /// registration/start_up halves are the collapsed world.
    pub(crate) fn init(&mut self, env: &mut impl CountryInfoManagerEnv) {
        self.on_update_fragment_prefixes(env);
    }

    /// `on_update_fragment_prefixes` (cpp:168-186).
    pub(crate) fn on_update_fragment_prefixes(&mut self, env: &mut impl CountryInfoManagerEnv) {
        if env.close_flag() {
            return;
        }
        if env.is_bot() {
            return;
        }
        if !env.have_option("fragment_prefixes") {
            return;
        }

        let fragment_prefixes_str = env.get_option_string("fragment_prefixes", "888");
        let mut guard = FRAGMENT_PREFIXES.lock().unwrap();
        if fragment_prefixes_str == guard.0 {
            return;
        }
        guard.0 = fragment_prefixes_str;
        guard.1 = full_split(&guard.0, ',');
    }

    /// `get_countries` (cpp:192-194).
    pub(crate) fn get_countries(
        &mut self,
        env: &mut impl CountryInfoManagerEnv,
        promise: Promise<td_api::Countries>,
    ) {
        let language_code = env.get_main_language_code();
        self.do_get_countries(env, language_code, false, promise);
    }

    /// `do_get_countries` (cpp:196-229).
    fn do_get_countries(
        &mut self,
        env: &mut impl CountryInfoManagerEnv,
        mut language_code: String,
        mut is_recursive: bool,
        mut promise: Promise<td_api::Countries>,
    ) {
        if is_recursive {
            let main_language_code = env.get_main_language_code();
            if language_code != main_language_code {
                language_code = main_language_code;
                is_recursive = false;
            }
        }

        let answer = get_country_list(
            Some(&mut *self),
            env,
            &language_code,
            CountryList::get_countries_object,
        );
        if let Some(countries) = answer {
            return promise.set_value(countries);
        }

        if is_recursive {
            return promise.set_error(Status::error(500, "Requested data is inaccessible"));
        }
        if language_code.is_empty() {
            return promise.set_error(Status::error(400, "Invalid language code specified"));
        }
        self.load_country_list(
            env,
            language_code,
            0,
            PendingCountryQuery::Countries { promise },
        );
    }

    /// `get_country` (cpp:231-234).
    pub(crate) fn get_country(
        &mut self,
        env: &mut impl CountryInfoManagerEnv,
        country_code: String,
        promise: Promise<Option<td_api::CountryInfo>>,
    ) {
        let language_code = env.get_main_language_code();
        self.do_get_country(env, country_code, language_code, false, promise);
    }

    /// `do_get_country` (cpp:236-269). The unknown-code answer is the
    /// C++ `promise.set_value(nullptr)` — the request arm maps it through
    /// the `Td::send_result` 404 band.
    fn do_get_country(
        &mut self,
        env: &mut impl CountryInfoManagerEnv,
        country_code: String,
        mut language_code: String,
        mut is_recursive: bool,
        mut promise: Promise<Option<td_api::CountryInfo>>,
    ) {
        if is_recursive {
            let main_language_code = env.get_main_language_code();
            if language_code != main_language_code {
                language_code = main_language_code;
                is_recursive = false;
            }
        }

        let answer = get_country_list(Some(&mut *self), env, &language_code, |list| {
            list.get_country_info_object(&country_code)
        });
        if let Some(info) = answer {
            return promise.set_value(info);
        }

        if is_recursive {
            return promise.set_error(Status::error(500, "Requested data is inaccessible"));
        }
        if language_code.is_empty() {
            return promise.set_error(Status::error(400, "Invalid language code specified"));
        }
        self.load_country_list(
            env,
            language_code,
            0,
            PendingCountryQuery::Country {
                country_code,
                promise,
            },
        );
    }

    /// `get_phone_number_info` (cpp:271-278).
    pub(crate) fn get_phone_number_info(
        &mut self,
        env: &mut impl CountryInfoManagerEnv,
        phone_number_prefix: String,
        mut promise: Promise<td_api::PhoneNumberInfo>,
    ) {
        let mut phone_number_prefix = phone_number_prefix;
        crate::misc::clean_phone_number(&mut phone_number_prefix);
        if phone_number_prefix.is_empty() {
            // The C++ `phoneNumberInfo(nullptr, "", "", false)` — the
            // flattened model keeps `country` nullable, so this is the
            // exact default.
            return promise.set_value(td_api::PhoneNumberInfo::default());
        }
        let language_code = env.get_main_language_code();
        self.do_get_phone_number_info(env, phone_number_prefix, language_code, false, promise);
    }

    /// `do_get_phone_number_info` (cpp:280-312).
    fn do_get_phone_number_info(
        &mut self,
        env: &mut impl CountryInfoManagerEnv,
        phone_number_prefix: String,
        mut language_code: String,
        mut is_recursive: bool,
        mut promise: Promise<td_api::PhoneNumberInfo>,
    ) {
        if is_recursive {
            let main_language_code = env.get_main_language_code();
            if language_code != main_language_code {
                language_code = main_language_code;
                is_recursive = false;
            }
        }

        let answer = get_country_list(Some(&mut *self), env, &language_code, |list| {
            get_phone_number_info_object(list, &phone_number_prefix)
        });
        if let Some(info) = answer {
            return promise.set_value(info);
        }

        if is_recursive {
            return promise.set_error(Status::error(500, "Requested data is inaccessible"));
        }
        if language_code.is_empty() {
            return promise.set_error(Status::error(400, "Invalid language code specified"));
        }
        self.load_country_list(
            env,
            language_code,
            0,
            PendingCountryQuery::PhoneNumber {
                phone_number_prefix,
                promise,
            },
        );
    }

    /// `get_current_country_code` (cpp:420-422) — the `GetNearestDcQuery`
    /// send over the dispatcher.
    pub(crate) fn get_current_country_code(
        &mut self,
        env: &mut impl CountryInfoManagerEnv,
        promise: Promise<String>,
    ) {
        env.get_nearest_dc_via_net(promise);
    }

    /// `load_country_list` (cpp:424-437) — the pending-query dedup and
    /// the single in-flight `GetCountriesListQuery` per language.
    fn load_country_list(
        &mut self,
        env: &mut impl CountryInfoManagerEnv,
        language_code: String,
        hash: i32,
        pending: PendingCountryQuery,
    ) {
        let queries = self
            .pending_load_country_queries
            .entry(language_code.clone())
            .or_default();
        if matches!(pending, PendingCountryQuery::Background) && !queries.is_empty() {
            return;
        }
        queries.push(pending);
        if queries.len() == 1 {
            // `GetCountriesListQuery::send(language_code, hash)` — the
            // first entry starts the query
            env.get_countries_list_via_net(&language_code, hash);
        }
    }

    /// `on_get_country_list` (cpp:439-470) — the query completion: pop
    /// the pending lot; the error arm keeps the cached lists (the 60-120s
    /// reload backoff) and resolves through the re-entry, the missing
    /// cache fails the lot; the ok arm fills the cache and resolves.
    pub(crate) fn on_get_country_list(
        &mut self,
        env: &mut impl CountryInfoManagerEnv,
        language_code: &str,
        result: std::result::Result<telegram_api::help_CountriesList, Status>,
    ) {
        let Some(pending) = self.pending_load_country_queries.remove(language_code) else {
            return;
        };
        let status = match result {
            Ok(country_list) => {
                {
                    let mut countries = COUNTRIES.lock().unwrap();
                    on_get_country_list_impl(&mut countries, language_code, country_list);
                }
                self.resolve_pending(env, language_code, pending);
                return;
            }
            Err(status) => status,
        };
        // the error arm: with a cached list the re-entry resolves from
        // the cache (the C++ `set_promises`), and the reload backoff
        // holds for 60-120s.
        let have_cache = {
            let mut countries = COUNTRIES.lock().unwrap();
            match countries.get_mut(language_code) {
                Some(list) => {
                    let backoff = tdutils::time::now() + f64::from(tdutils::random::fast(60, 120));
                    list.next_reload_time = list.next_reload_time.max(backoff);
                    true
                }
                None => false,
            }
        };
        if have_cache {
            self.resolve_pending(env, language_code, pending);
        } else {
            for query in pending {
                match query {
                    PendingCountryQuery::Countries { mut promise } => {
                        promise.set_error(status.clone())
                    }
                    PendingCountryQuery::Country { mut promise, .. } => {
                        promise.set_error(status.clone())
                    }
                    PendingCountryQuery::PhoneNumber { mut promise, .. } => {
                        promise.set_error(status.clone())
                    }
                    PendingCountryQuery::Background => {}
                }
            }
        }
    }

    /// The `set_promises` half — the parked re-entries run their
    /// `do_get_*` with `is_recursive = true` (the C++ closure send).
    fn resolve_pending(
        &mut self,
        env: &mut impl CountryInfoManagerEnv,
        language_code: &str,
        pending: Vec<PendingCountryQuery>,
    ) {
        for query in pending {
            match query {
                PendingCountryQuery::Countries { promise } => {
                    self.do_get_countries(env, language_code.to_owned(), true, promise);
                }
                PendingCountryQuery::Country {
                    country_code,
                    promise,
                } => {
                    self.do_get_country(env, country_code, language_code.to_owned(), true, promise);
                }
                PendingCountryQuery::PhoneNumber {
                    phone_number_prefix,
                    promise,
                } => {
                    self.do_get_phone_number_info(
                        env,
                        phone_number_prefix,
                        language_code.to_owned(),
                        true,
                        promise,
                    );
                }
                PendingCountryQuery::Background => {}
            }
        }
    }
}

/// `CountryInfoManager::get_country_list` (cpp:532-628) — the guarded
/// cache lookup with the compiled-in "en" fallback. The answer is built
/// under the lock like the C++ `promise.set_value(list->...)` scope; the
/// background `load_country_list` trigger (the C++ `Auto()` promise) is
/// collected under the lock and fired after the release (the seam is a
/// no-op).
fn get_country_list<T>(
    manager: Option<&mut CountryInfoManager>,
    env: &mut impl CountryInfoManagerEnv,
    language_code: &str,
    with_list: impl FnOnce(&CountryList) -> T,
) -> Option<T> {
    let mut reload = None;
    {
        let mut guard = COUNTRIES.lock().unwrap();
        if !guard.contains_key(language_code) {
            if language_code != "en" {
                return None;
            }
            let decoded = decode_embedded_countries_list()
                .expect("the C++ CHECK(parser.get_error() == nullptr)");
            on_get_country_list_impl(&mut guard, "en", decoded);
            let country = guard.get("en").expect("the embedded list just inserted");
            if manager.is_some() {
                reload = Some(("en".to_owned(), country.hash));
            }
        } else if manager.is_some() {
            let country = guard.get(language_code).expect("the checked list");
            if country.next_reload_time < tdutils::time::now() {
                reload = Some((language_code.to_owned(), country.hash));
            }
        }
        let result = guard.get(language_code).map(with_list);
        drop(guard);
        if let (Some(manager), Some((language_code, hash))) = (manager, reload) {
            manager.load_country_list(env, language_code, hash, PendingCountryQuery::Background);
        }
        result
    }
}

/// The `get_country_list(nullptr, ...)` half (cpp:532-619) — the sync
/// lookup: the embed, no reload triggers.
fn get_country_list_static<'a>(
    guard: &'a mut HashMap<String, CountryList>,
    language_code: &str,
) -> Option<&'a CountryList> {
    if !guard.contains_key(language_code) {
        if language_code != "en" {
            return None;
        }
        let decoded =
            decode_embedded_countries_list().expect("the C++ CHECK(parser.get_error() == nullptr)");
        on_get_country_list_impl(guard, "en", decoded);
    }
    guard.get(language_code)
}

/// `CountryInfoManager::on_get_country_list_impl` (cpp:472-530) — the
/// cache fill. The NotModified arm is exact but reachable only once the
/// net step feeds the query results.
fn on_get_country_list_impl(
    countries: &mut HashMap<String, CountryList>,
    language_code: &str,
    country_list: telegram_api::help_CountriesList,
) {
    let list = match country_list {
        telegram_api::help_CountriesList::help_countriesListNotModified(_) => {
            match countries.get_mut(language_code) {
                None => {
                    tdutils::log_error!(
                        "Receive countriesListNotModified for unknown list with language code {language_code}"
                    );
                    // the C++ `countries_[language_code]` + erase —
                    // net-zero on a missing key
                }
                Some(list) => {
                    tdutils::log_info!(
                        "List of countries with language code {language_code} is not modified"
                    );
                    list.next_reload_time =
                        tdutils::time::now() + f64::from(tdutils::random::fast(86400, 2 * 86400));
                }
            }
            return;
        }
        telegram_api::help_CountriesList::help_countriesList(list) => list,
    };

    let entry = countries.entry(language_code.to_string()).or_default();
    entry.countries.clear();
    for mut c in list.countries {
        let mut info = CountryInfo {
            country_code: std::mem::take(&mut c.iso2),
            default_name: std::mem::take(&mut c.default_name),
            name: std::mem::take(&mut c.name),
            is_hidden: c.hidden,
            calling_codes: Vec::new(),
        };
        for mut code in c.country_codes {
            // `to_integer_safe<int32>` + the positive gate (cpp:501-502).
            let r_calling_code = code.country_code.parse::<i32>();
            match r_calling_code {
                Ok(calling_code) if calling_code > 0 => {
                    let mut prefixes = std::mem::take(&mut code.prefixes);
                    if prefixes.is_empty() {
                        prefixes.resize(1, String::new());
                    }
                    info.calling_codes.push(CallingCodeInfo {
                        calling_code: std::mem::take(&mut code.country_code),
                        prefixes,
                        patterns: std::mem::take(&mut code.patterns),
                    });
                }
                _ => {
                    tdutils::log_error!(
                        "Receive invalid calling code {} for country {}",
                        code.country_code,
                        info.country_code
                    );
                }
            }
        }
        if info.calling_codes.is_empty() {
            tdutils::log_error!(
                "Receive empty list of calling codes for {}",
                info.country_code
            );
            continue;
        }

        entry.countries.push(info);
    }
    entry.hash = list.hash;
    entry.next_reload_time =
        tdutils::time::now() + f64::from(tdutils::random::fast(86400, 2 * 86400));
}

/// The compiled-in fallback (cpp:536-602) — the C++ static
/// `gzdecode(base64url_decode(...).ok())` bytes, decoded once per process.
fn embedded_en_bytes() -> &'static [u8] {
    static EN: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    EN.get_or_init(|| {
        let compressed = URL_SAFE_NO_PAD
            .decode(EMBEDDED_EN_COUNTRIES_LIST)
            .expect("the C++ base64url_decode(...).ok()");
        tdutils::gzip::gzdecode(&compressed).as_slice().to_vec()
    })
    .as_slice()
}

/// `telegram_api::help_getCountriesList::fetch_result(parser)` over the
/// decompressed bytes (cpp:604-607); the caller CHECKs the full
/// consumption like the C++ `fetch_end` + `get_error() == nullptr`.
fn decode_embedded_countries_list()
-> std::result::Result<telegram_api::help_CountriesList, &'static str> {
    let mut reader = telegram_api::tl::Reader::new(embedded_en_bytes());
    let result = telegram_api::help_CountriesList::read(&mut reader);
    if result.is_ok() && reader.remaining() != 0 {
        return std::result::Result::Err("Non-empty bytes left in the embedded list");
    }
    result
}

/// `CountryInfoManager::get_phone_number_info_sync` (cpp:314-328) — the
/// SynchronousRequests static: the cleaned prefix, the cached list with
/// the "en" fallback, the matcher. Unlike the C++ `CHECK(list != nullptr)`
/// the fallback cannot miss (the embed is deterministic).
pub(crate) fn get_phone_number_info_sync(
    language_code: &str,
    phone_number_prefix: String,
) -> td_api::PhoneNumberInfo {
    let mut phone_number_prefix = phone_number_prefix;
    crate::misc::clean_phone_number(&mut phone_number_prefix);
    if phone_number_prefix.is_empty() {
        // the C++ `phoneNumberInfo(nullptr, "", "", false)` — exact as the
        // flattened default (see `get_phone_number_info`)
        return td_api::PhoneNumberInfo::default();
    }

    let mut guard = COUNTRIES.lock().unwrap();
    let list = match get_country_list_static(&mut guard, language_code) {
        Some(list) => list,
        // the "en" fallback (cpp:322-325) — the embed makes it infallible
        None => get_country_list_static(&mut guard, "en")
            .expect("the C++ CHECK(list != nullptr) — the embedded \"en\" fallback"),
    };
    get_phone_number_info_object(list, &phone_number_prefix)
}

/// `CountryInfoManager::get_phone_number_info_object` (cpp:330-418) —
/// the phone-number → country + calling-code prefix matcher and the
/// pattern formatter. The no-band answer keeps `country: None` — the
/// flattened model keeps the field nullable, the exact C++ null.
fn get_phone_number_info_object(list: &CountryList, phone_number: &str) -> td_api::PhoneNumberInfo {
    let mut best_country: Option<&CountryInfo> = None;
    let mut best_calling_code: Option<&CallingCodeInfo> = None;
    let mut best_length = 0usize;
    let mut is_prefix = false; // is phone number a prefix of a valid country_code + prefix
    let is_anonymous = is_fragment_phone_number(phone_number);
    for country in &list.countries {
        for calling_code in &country.calling_codes {
            if begins_with(phone_number, &calling_code.calling_code) {
                let calling_code_size = calling_code.calling_code.len();
                let formatted_input = &phone_number[calling_code_size..];
                for prefix in &calling_code.prefixes {
                    if begins_with(prefix, formatted_input) {
                        is_prefix = true;
                    }
                    if calling_code_size + prefix.len() > best_length
                        && begins_with(formatted_input, prefix)
                    {
                        best_country = Some(country);
                        best_calling_code = Some(calling_code);
                        best_length = calling_code_size + prefix.len();
                    }
                }
            }
            if begins_with(&calling_code.calling_code, phone_number) {
                is_prefix = true;
            }
        }
    }
    let Some(best_calling_code) = best_calling_code else {
        return td_api::PhoneNumberInfo {
            country: None,
            country_calling_code: if is_prefix {
                phone_number.to_string()
            } else {
                String::new()
            },
            formatted_phone_number: if is_prefix {
                String::new()
            } else {
                phone_number.to_string()
            },
            is_anonymous,
        };
    };
    let best_country = best_country.expect("set with the calling code");

    let formatted_part = &phone_number[best_calling_code.calling_code.len()..];
    let mut formatted_phone_number = String::new();
    let mut max_matched_digits = 0usize;
    for pattern in &best_calling_code.patterns {
        let pattern = pattern.as_bytes();
        let mut result: Vec<u8> = Vec::with_capacity(pattern.len());
        let mut current_pattern_pos = 0usize;
        let mut is_failed_match = false;
        let mut matched_digits = 0usize;
        for c in formatted_part.bytes() {
            while current_pattern_pos < pattern.len()
                && pattern[current_pattern_pos] != b'X'
                && !is_digit(pattern[current_pattern_pos])
            {
                result.push(pattern[current_pattern_pos]);
                current_pattern_pos += 1;
            }
            if current_pattern_pos == pattern.len() {
                // result += ' ';
            }
            if current_pattern_pos >= pattern.len() || pattern[current_pattern_pos] == b'X' {
                result.push(c);
                current_pattern_pos += 1;
            } else {
                debug_assert!(is_digit(pattern[current_pattern_pos]));
                if c == pattern[current_pattern_pos] {
                    matched_digits += 1;
                    result.push(c);
                    current_pattern_pos += 1;
                } else {
                    is_failed_match = true;
                    break;
                }
            }
        }
        for &rest in &pattern[current_pattern_pos..] {
            if is_digit(rest) {
                is_failed_match = true;
            }
        }
        if !is_failed_match && matched_digits >= max_matched_digits {
            max_matched_digits = matched_digits;
            while current_pattern_pos < pattern.len() {
                if pattern[current_pattern_pos] == b'X' {
                    result.push(b'-');
                } else {
                    debug_assert!(!is_digit(pattern[current_pattern_pos]));
                    result.push(b' ');
                }
                current_pattern_pos += 1;
            }
            // the C++ appends the pattern bytes raw; corrupted server
            // patterns with invalid UTF-8 sanitize here instead
            formatted_phone_number = String::from_utf8_lossy(&result).into_owned();
        }
    }

    td_api::PhoneNumberInfo {
        country: Some(best_country.get_country_info_object()),
        country_calling_code: best_calling_code.calling_code.clone(),
        formatted_phone_number: if formatted_phone_number.is_empty() {
            formatted_part.to_string()
        } else {
            formatted_phone_number
        },
        is_anonymous,
    }
}

/// `CountryInfoManager::is_fragment_phone_number` (cpp:151-166) — the
/// empty-prefix lazy default and the prefix walk over the cleaned number.
fn is_fragment_phone_number(phone_number: &str) -> bool {
    if phone_number.is_empty() {
        return false;
    }
    let mut guard = FRAGMENT_PREFIXES.lock().unwrap();
    if guard.1.is_empty() {
        guard.0 = "888".to_string();
        guard.1.push("888".to_string());
    }
    let mut cleaned = phone_number.to_string();
    crate::misc::clean_phone_number(&mut cleaned);
    guard.1.iter().any(|prefix| begins_with(&cleaned, prefix))
}

/// `CountryInfoManager::get_country_flag_emoji` (cpp:630-654) — the
/// regional-indicator pair with the four special codes.
pub(crate) fn get_country_flag_emoji(country_code: &str) -> String {
    let bytes = country_code.as_bytes();
    if bytes.len() != 2 || !is_alpha(bytes[0]) || !is_alpha(bytes[1]) {
        return String::new();
    }
    let first = bytes[0].to_ascii_uppercase();
    let second = bytes[1].to_ascii_uppercase();
    if first == b'Y' && second == b'L' {
        return "\u{1F52E}".to_string(); // crystal ball
    }
    if first == b'F' && second == b'T' {
        return "\u{1F3F4}\u{200D}\u{2620}\u{FE0F}".to_string(); // pirate flag
    }
    if first == b'X' && second == b'G' {
        return "\u{1F6F0}".to_string(); // satellite
    }
    if first == b'X' && second == b'V' {
        return "\u{1F30D}".to_string(); // globe showing Europe-Africa
    }
    let mut result = String::with_capacity(8);
    result.push(char::from_u32(0x1F1A5 + u32::from(first)).expect("regional indicator"));
    result.push(char::from_u32(0x1F1A5 + u32::from(second)).expect("regional indicator"));
    debug_assert_eq!(result.len(), 8);
    result
}

/// `td::begins_with` (tdutils/misc.h) — `str` starts with `prefix`.
fn begins_with(s: &str, prefix: &str) -> bool {
    s.len() >= prefix.len() && s.as_bytes().starts_with(prefix.as_bytes())
}

/// `td::is_digit` (tdutils/misc.h) — the ASCII `'0'..='9'`.
fn is_digit(c: u8) -> bool {
    c.is_ascii_digit()
}

/// `td::is_alpha` (tdutils/misc.h) — `(c | 0x20) in 'a'..='z'`.
fn is_alpha(c: u8) -> bool {
    (c | 0x20).is_ascii_lowercase()
}

/// `td::full_split` (tdutils/misc.h:35-55) — the empty pieces stay (a
/// trailing delimiter yields a trailing empty prefix, the C++ landmine
/// the fragment prefixes inherit).
fn full_split(s: &str, delimiter: char) -> Vec<String> {
    let mut result = Vec::new();
    if s.is_empty() {
        return result;
    }
    let mut rest = s;
    while let Some(delimiter_pos) = rest.find(delimiter) {
        result.push(rest[..delimiter_pos].to_string());
        rest = &rest[delimiter_pos + delimiter.len_utf8()..];
    }
    result.push(rest.to_string());
    result
}

/// The C++ embedded literal (CountryInfoManager.cpp:537-602), verbatim.
const EMBEDDED_EN_COUNTRIES_LIST: &str = concat!(
    "eJyNW81y40hyrib13-qe6d3ZWR8cE4xwxHod4dkg8UfgyH9RJCg2AervViRryBqBgAYEpKVeYG8--A",
    "H2sgfHHnyzn6Dtky9-Ar-AD34FJ0hRAIlkoSO6pzVUZVVWVuaXX2YV_xz-959-8x___rf_Swj5u3_60xf4h-",
    "QqdXJccSee79Pod-_gs__7z3w1B__m5bK2-ezk9raw-rMl2yDfDV0esEmh4tNRoTHnPg3YAn6XmssolzafvV",
    "9NtPp7G8_VJO8rP01n1OWLgLq78jlDJgn5twli-Rb5VHEDPg1p4XeFKvVH4YSm9TgoSZqe0O9ko8jWvtrkpO",
    "JOQ-44FNlLNIeSOUcX7OqMYDuIXVVVbAsTZP05Q2XLyuazs1g2se4VOQLdPdA8JSspitiGA3Ja8acMzOju2G",
    "71e1VJrGORj5U58_mYugWLzr30egclTc-2kw17DReBn95rTonP_Gwju7XXIegbyVKHI2ed00oJn1lJ79j5hh",
    "xW_HCEyOYloyzW-56cVV6YP6L8Z-oi_m4owjOuVsh3VW8BRwzuesH8Fzb1nlZmT88l62XhmVer5CTyeDrxsN",
    "gDf1W0rHOo1slZlbpTh07YYpbWQdeLYh0a5LjKnCkP56lzkKW3-c5vN0CygyXVJjmvhv4DmKDQpAsPs4Mkad",
    "s2Xf-J52iBHUJnSn2-a0eyjjsjHnsB-tKZT3kKawCr5M1np7exsWLZNsiGfuhOeFpHdeesCts-W70kh1WIbB",
    "fdnyG2sRnZ2J8DsqXPWFFKmWfcI5-qoDbjhTr1w8WCOnSO6KHF-8fnuQI9PIc_IfikGqV4D29un5D9TH6oei",
    "7lPvvHgsXdoNCACKYBDxcQCBYd0fS5qUbi3AbkqOrTF-6k411V3z77sIGL1NlZq3Onc7rA4kTKtKEN68_CAI",
    "35sio-vxvwTy9YPFM8ziUtI87vVjEWHR2SF8Q5pXoPejOHvzDEvsXS27hahRzVQL_J9jnk4d93pbXOv49klK",
    "KcV_VyvqwX87qk5iVZA98v5rWiAv4j5ctlPS_Bz4pcgvm1fLlo5I0ifK5LURxDLjLykm7kZUXOy5qaV0oajC",
    "3n1ZKRVxU9r5XkvKbA_EU1rxfLIBuN0WEM_IV11JKSV8swVi_ldZDRyzAPrKPJ8HNsx_NEnts6x1qdfKp5kC",
    "gLv6-z-R8KA_b4h3_AzkSRhXatNcl3NciYkIMKlZ_W6TCaCz1fWYxftRY5f9XpVR1MH0mszwV5bz3zAFKTQ9",
    "1JOqcmcyIm3yYfa1_-LWCFyd-3nzwIUwyn1O04K6TjrNaJ9uI9FNqLSI_dvLTyO02X4vFdclibcYcheVzVxH",
    "m8ZpKTGgUy4nkubvc3f0DxvNZbrb3LeVZr61oSUxKrJ-WvYH3P8eajVN5Z61_ezX-IPw7IWc0DJCwMwIfSe4",
    "AYEu9hSA5qKyqDrJ_kUQgm165hbfrICtfMn2DnLetCTKwBrtVCn375F7qbt9-94vfms3dGUu4OsGb5CHCGcB",
    "4148zuwU9f2HgWRVs4cvg4PYciFRO-nua7deAsLcin1F2muK5ixGvVL8lJ_Wc-8sKAo_6lysK4rnfIcZ25c-",
    "o_pOMxzlnoPuvg23Vvzt3IKzBuV9bKJCNn1a_Ix80cr-gUjc9vzaMXjT3zJMZIXzFG2Tcm1uc-qo2mDOH8ea",
    "kk78RKmis2auS4MQ6B7voI_xDjdQPOvLEIPLy2kpJ6r7Nvct0WOWxMl48BEmNSMRnjSZhIyA9gbZ8HPkP2Hf",
    "Om0zj1J2Qtcmg9Uow35mRFXBs3bHLSCGbce0TxCfw3wdkQ3tEEvtvk7m4-Ies41eNxl-SgyX9GY0QrG0KfaH",
    "bIt03qPESLiHKGWizGMiY5M_nY91y24Ahv1IyY1zSvyIcm9T0WT46cwRvOHe_asAm8s-lTd5zGx5wsJ3JEYc",
    "tpI_l3K3k76k947nLuAdPthfMR8zEddH2lQ_Tzob72ARQXWhVy2KIjD60jlG0O_qpMLFslH1_7Nh3uTifePL",
    "0nZdunNsV3PEcdsNNnEVFEapGynIUBrRX2ev4UiUPDUIX-3GrCWcLaAP6tkFOsVwE4oMTjL8BWs9S419wmxo",
    "tWm5y2-AjoXUB9rDZXi3HcYufUBXmfsVX0oD5n7PW5lkmOWnTFKhAOVhTbtweyIXcZKivuA7X65KwF8MocL3",
    "xE-IBqvK29F6Nbn8mnxi8hDTxAeafwqgrmq-Ic3YK4i-yHxt0W5uK8qmWD_UENNqcOtr5alLZiZZcbtYBXgf",
    "wcwbSDklbOrLlbN-TDevM_VjlU3CFqA1U8x110lsvIgZGziPWHn7V1mMayFx1yegE1RaED_8HwRhXv_6JHTk",
    "B-AvQO7euqRUXo_xeQ82q-BwU-km91cb18YZPDC8oj0oWsa4jXHZLji9CdUn-Z9huNCH2uXSenbXfymlaQvm",
    "Lcz0L7km3AtrbPsPoL8EKMN-0uOWovfMrS_Y28URbXfm2oY0BvVGejtI3nr1rHslfkvM4Z1J4t6o_38ARFE_",
    "pp-zM5aPv0F0zW0JTMWG0PVvJYDZcz9OxYb1tg9_E-uyti3cHX2gF1lhi_MrLXvjTJ8SWdU6DYaZzQy5n918",
    "srcnTp-RO0r6Qlzh2pfy_75PCSPlKMG-qlpO44L-00yGGHuUsUn1Vxrui0yFln6U-XLwu8J2ZowljpXKxq95",
    "E3wX1OV8UY0WmTkw73-YjitZmmi-vmDpxbzZt7vpfuqUmamLN2euTXFo16mB0eBFH_sseeONJX1LXMmqjTJ-",
    "97nh_MAKh36gOytkPMezsD8t6CYhQZuz5zKRnrb2cey9-Qo074DMiK-Zoqttcd1N10CTXzG5FO71eGqjZrv_",
    "fgN_SFPsxSfrPqNZbX8tF877T4_9cc-80RClu6dSvkoEvROxCwn5aQLyRcKSFfJcddNqJA01P2N7S4lujWwP",
    "6rc--G43Reg5pczbxj7LbJhy5n41nA3EXA0PsARRL34buQ2y2fF7rUfUAxX9n1g23c6EJu7vIRXofL4vvarh",
    "XZauEFMw-JG00Ys13gZF0ezMLodhTj1OUtTr3GjOS-h-SsG_6RAWqE_hTj5Mk8mcas7jU56tLgCePVcrkkXv",
    "uOHILNcKws6UKbmRVybALSjMeIzUri3G7WyJHpuXTsYTqLe2VmPVrXmXhPWM9DFp6V2YAa23PBS9nU3157FR",
    "dyoodrQi4woSCc0sUYiiXEL95se56MwaRPmxfkW5P6ixl1HGHv2Eis2yHfrLHTpGM2wXs7epIvbtJQYt0uOT",
    "Cpg_f3JPF9oAk5xAR6Pqf-rp45Q02MAy5tRv19B891Rnknbna4uHlFDmGHaA2hqxk69snfrGzEfBfM5Edl8x",
    "77bmqbYhaGmZ-j8_YD7vJfQuxeydDisYNobOjzYBX0iI0lsf7W2g8XzPdpOm-BvtlvMkw7sh8U8mgdL76bMY",
    "fkdK0_D9HekSzuA5jXcPbUmfCnPe9jNHE9b0LOBnn6zLG4Et_9mbcgy_7Ix-n4zalSYo27lY50uUDvz3NaMT",
    "H2PjqPl6g7sXv2r7xRjIU9wMIenfMRlns08duJXo186LHnQo0662hH75bK8Xjgtz0-ZVj_RpLKwnPvNclHiJ",
    "ufPGdzlYWsVY6xqNeCffE9vW05WQPhXLzXJqc9KCJ8Og0xP1WLYn7W6wKPZFGYr0I7XcvE9QDWq1zPAbUI7P",
    "mZLpG-YHyXdfbGwRIY1YNapMceqYPiWzl7_wOQhzgLsTMtxz293pAc9Dh4Hnr2cjzuPrLHc-GeUfQ-VIs5Em",
    "rPK5McXM3RmhRiVhfK9ivkqA84O0fv88Q5u98gB30WmQG5z9uu5VP8pt8k3772R_ues3xtY2B2it9V9Fvk2z",
    "59DGkhstamXYedgRrLXJD3_Rl3-OMjDE_7mhb3xffWzf0OOenTh9WjP2yvRurtEDJHF-zspWJz7a96hq1M8t",
    "s1l-9zSC0MijgzQjQHqQHUYnzX0YcarB8yP_CiC1tvU6skagB9371cPMaQM-vCvkVO-4BzC8iyyN04xNRu_3",
    "QbK_s22BfyfjiNQhLLeyWxfW7IIay_yzle_SeuSft30TmuUGuJraNm9PQ_V8jhZ4r32I2MN4-DBjkZfPlr6P",
    "I99_-auB4YXJHjgTfH3mrmlKLYfwYWObKYj_boZV18rzYYkl8NwgVEp1tosgkDYhPtYHeeZO27NwYGwA8Gzx",
    "ACGLdSd31kew9WhZxbNJzw1TPePdxU2-FHO35mVck3VvQGwntrDCDYEedkq0bOLLYcz5gDzo31GvXYdnGSiu",
    "Xr5NAKJ1jPK3pbJNa1AWf2DAZP2zqnbPWGEdkW1NzcndJHz8feqmgZ9_nWRWTrCG8umMPe3oHlknaSlHLifu",
    "jt7I52_cdqkxPL8Z6Yi5-ZrIvrcKuzlgf4ReUVSYwNVhf2EqEmLXSZ5-7m4lfOI4l1MMEP6LokcbH6shxjrt",
    "Ujx1ZUiFIH4XHifoUFMW5BjDsYL1MzdByAnYD6QyZH96hmvBW2rE2_buWxSO2fobtNvre-_MUr2N78y18hQ_",
    "X9L__qjjl2NyfJyZ7lpn2R0OWavG84BYs6T_j7iaK4jrRuozMH9zUpVH4Mw1vIfVKJZOQ16w7id-mj_R9Dk8",
    "V-d09OGotnGhWeqM9l8DK7Rr63Q_8h6tjWKGTvhaCXqWW_Z7Hr5KA2oxPc_3dqs536wm6RA9ub7nnvrAv90r",
    "4gJ_aM8vXVMoJFGe_m7EtyZtOf-Sv7Sp-DIe5J2R1ybHsPDNhBGuuNmBvYUJPYfO75P3aBxzAkLyTGmuQ8Op",
    "vo2w97OCHolfTRdC_HBpywgQ0s0PdFYky0r8ih7QG-o9w37mfYA3IU6cmQGsn4intp2yafbIAUPqETcEPbG9",
    "HICdL-p2d_X8W-jnR5ok6IcbQYP23gCDblz9hZ67omjDkbYs6m7gvdk2ukjO-0DKHeHz5Ez-1ZSlaOv1tw_i",
    "a70xcctsjRcLqP34i5ydAi-aFVSfGq0lfwquEd6O2HEa1FcH-bo-z264b35Gz4MmL740vP3Pd1jfyw5gvX3B",
    "0z-Pd3BajuC-u3N1HZheFv9vdurhvk9Bpy6QuUOggG59SvuG-9bpHvq1FHbDED7fwpF90JSV-hU5t8Glq7U6",
    "H3S8XMuQADrjkLIGfvxkROT3yP6XoI46gb0gDBsAT3uGmSb26o4_AoaTTDIHTTd3SaHt8R3UTv9VZfi8Jicl",
    "VDR5_lblvkh5YD0e8UTG_EHQapOQBOzAP4aQloOU_L64l1bjvkqOMtvCfsXkDfm8vXa1-T79tuwHx3VXiACj",
    "0WPHuQGNOcVNelxM9xf-WuQQ7v2JxhPNzQyvswZbX-HdTtdz-m6vbV7xQpvv9T1jXQ25r3Ub2y4lPrF_cIHu",
    "WkMhFiwr1Jju73va2Ke7G47A05uefzER0945xXQ994_M8_z__r_wF2XX6H",
);

/// The tests below mutate the process-global country cache and the
/// fragment prefixes, so they serialize on one lock — in parallel they
/// would corrupt each other (the tdutils TEST_LOCK pattern). The td.rs
/// round-trip test shares it (its dispatcher flow touches the same
/// cache).
#[cfg(test)]
pub(crate) static COUNTRY_TEST_LOCK: Mutex<()> = Mutex::new(());

/// The td.rs round-trip helper: the hermetic stale "en" list (the
/// reload fires deterministically, whatever the cache holds).
#[cfg(test)]
pub(crate) fn force_stale_english_list_for_test(hash: i32) {
    COUNTRIES.lock().unwrap().insert(
        "en".to_string(),
        CountryList {
            countries: Vec::new(),
            hash,
            next_reload_time: 0.0,
        },
    );
}

/// The td.rs round-trip helper: drop the "en" entry (the embedded
/// decode re-creates it on demand).
#[cfg(test)]
pub(crate) fn remove_english_list_for_test() {
    COUNTRIES.lock().unwrap().remove("en");
}

#[cfg(test)]
mod tests {
    use super::*;
    use tdutils::promise::PromiseCreator;
    use tdutils::status::Result;

    /// The manager env over a map: the option strings ride a map, the net
    /// seams record their queries (the nearest-dc promise parks like the
    /// Td env's lot).
    struct TestCountryEnv {
        options: std::collections::BTreeMap<String, String>,
        main_language_code: String,
        nearest_dc_queries: usize,
        nearest_dc_promises: Vec<Promise<String>>,
        countries_list_queries: Vec<(String, i32)>,
    }

    impl TestCountryEnv {
        fn new(main_language_code: &str) -> TestCountryEnv {
            TestCountryEnv {
                options: std::collections::BTreeMap::new(),
                main_language_code: main_language_code.to_string(),
                nearest_dc_queries: 0,
                nearest_dc_promises: Vec::new(),
                countries_list_queries: Vec::new(),
            }
        }

        fn set_option_string(&mut self, name: &str, value: &str) {
            self.options.insert(name.to_string(), format!("S{value}"));
        }
    }

    impl CountryInfoManagerEnv for TestCountryEnv {
        fn close_flag(&self) -> bool {
            false
        }

        fn is_bot(&self) -> bool {
            false
        }

        fn have_option(&self, name: &str) -> bool {
            self.options.contains_key(name)
        }

        fn get_option_string(&self, name: &str, default_value: &str) -> String {
            match self.options.get(name).map(String::as_str) {
                Some(value) => value["S".len()..].to_string(),
                None => default_value.to_string(),
            }
        }

        fn get_main_language_code(&mut self) -> String {
            self.main_language_code.clone()
        }

        fn get_nearest_dc_via_net(&mut self, promise: Promise<String>) {
            self.nearest_dc_queries += 1;
            self.nearest_dc_promises.push(promise);
        }

        fn get_countries_list_via_net(&mut self, language_code: &str, hash: i32) {
            self.countries_list_queries
                .push((language_code.to_string(), hash));
        }
    }

    /// The promise + its capture slot, the shared test seam type.
    type CapturedPromise<T> = (
        Promise<T>,
        std::sync::Arc<std::sync::Mutex<Option<Result<T>>>>,
    );

    fn captured_promise<T: Send + 'static>() -> CapturedPromise<T> {
        let slot = std::sync::Arc::new(std::sync::Mutex::new(None));
        let capture = std::sync::Arc::clone(&slot);
        let promise = PromiseCreator::lambda_result(move |result: Result<T>| {
            *capture.lock().unwrap() = Some(result);
        });
        (promise, slot)
    }

    /// The compiled-in list decodes into the exact C++ facts: the order,
    /// the hash, the hidden four, the prefix/pattern shapes.
    #[test]
    fn embedded_list_decodes_with_the_cpp_facts() {
        let _lock = COUNTRY_TEST_LOCK.lock().unwrap();
        let decoded = match decode_embedded_countries_list() {
            std::result::Result::Ok(decoded) => decoded,
            std::result::Result::Err(error) => panic!("the embedded list must decode: {error}"),
        };
        let telegram_api::help_CountriesList::help_countriesList(list) = decoded else {
            panic!("the embedded list is a full list");
        };
        assert_eq!(list.countries.len(), 235);
        assert_eq!(list.hash, -882011426);

        let first = &list.countries[0];
        assert_eq!(first.iso2, "AD");
        assert_eq!(first.default_name, "Andorra");
        assert!(first.name.is_empty());
        assert!(!first.hidden);
        assert_eq!(first.country_codes.len(), 1);
        assert_eq!(first.country_codes[0].country_code, "376");
        assert!(first.country_codes[0].prefixes.is_empty());
        assert_eq!(first.country_codes[0].patterns, ["XX XX XX"]);

        let last = &list.countries[234];
        assert_eq!(last.iso2, "ZW");
        assert_eq!(last.default_name, "Zimbabwe");
        assert_eq!(last.country_codes[0].country_code, "263");

        let gb = list.countries.iter().find(|c| c.iso2 == "GB").unwrap();
        assert_eq!(gb.default_name, "United Kingdom");
        assert_eq!(gb.country_codes[0].country_code, "44");
        assert_eq!(gb.country_codes[0].patterns, ["XXXX XXXXXX"]);

        let kz = list.countries.iter().find(|c| c.iso2 == "KZ").unwrap();
        assert_eq!(kz.country_codes[0].prefixes, ["6", "7"]);
        assert_eq!(kz.country_codes[0].patterns, ["XXX XXX XX XX"]);

        let hidden: Vec<&str> = list
            .countries
            .iter()
            .filter(|c| c.hidden)
            .map(|c| c.iso2.as_str())
            .collect();
        assert_eq!(hidden, ["FT", "XG", "XV", "YL"]);

        // the multi-code countries keep their order (DO/PR/SH/XV)
        let multi: Vec<&str> = list
            .countries
            .iter()
            .filter(|c| c.country_codes.len() > 1)
            .map(|c| c.iso2.as_str())
            .collect();
        assert_eq!(multi, ["DO", "PR", "SH", "XV"]);
    }

    /// `get_country_flag_emoji` (cpp:630-654): the regional-indicator
    /// pairing, the four special codes and the validation bands.
    #[test]
    fn country_flag_emoji_matches_the_cpp() {
        let _lock = COUNTRY_TEST_LOCK.lock().unwrap();
        assert_eq!(get_country_flag_emoji("US"), "\u{1F1FA}\u{1F1F8}");
        assert_eq!(get_country_flag_emoji("us"), "\u{1F1FA}\u{1F1F8}");
        assert_eq!(get_country_flag_emoji("GB"), "\u{1F1EC}\u{1F1E7}");
        // the special codes are case-insensitive like the C++ to_upper
        assert_eq!(get_country_flag_emoji("yl"), "\u{1F52E}"); // crystal ball
        assert_eq!(
            get_country_flag_emoji("FT"),
            "\u{1F3F4}\u{200D}\u{2620}\u{FE0F}"
        );
        assert_eq!(get_country_flag_emoji("xg"), "\u{1F6F0}"); // satellite
        assert_eq!(get_country_flag_emoji("XV"), "\u{1F30D}"); // Europe-Africa globe
        // the validation bands
        assert_eq!(get_country_flag_emoji(""), "");
        assert_eq!(get_country_flag_emoji("U"), "");
        assert_eq!(get_country_flag_emoji("USA"), "");
        assert_eq!(get_country_flag_emoji("U1"), "");
        assert_eq!(get_country_flag_emoji("1U"), "");
    }

    /// `on_update_fragment_prefixes` + `is_fragment_phone_number`
    /// (cpp:151-186): the lazy "888" default and the option-driven
    /// prefixes (the C++ `full_split` keeps the empty pieces).
    #[test]
    fn fragment_prefixes_follow_the_option() {
        let _lock = COUNTRY_TEST_LOCK.lock().unwrap();
        // the pristine lazy state
        *FRAGMENT_PREFIXES.lock().unwrap() = (String::new(), Vec::new());
        assert!(!is_fragment_phone_number(""));
        // the first non-empty probe fills the lazy "888" default
        assert!(is_fragment_phone_number("88812345678"));
        assert!(!is_fragment_phone_number("9991234567"));
        // the cleaned compare: formatting noise is not part of the number
        assert!(is_fragment_phone_number("+888 123-45-67"));

        let mut manager = CountryInfoManager::default();
        let mut env = TestCountryEnv::new("en");
        env.set_option_string("fragment_prefixes", "777,900");
        manager.on_update_fragment_prefixes(&mut env);
        assert!(is_fragment_phone_number("7771234567"));
        assert!(is_fragment_phone_number("900"));
        assert!(!is_fragment_phone_number("88812345678"));

        // the empty pieces stay in the split — a trailing comma makes
        // every number fragment-anonymous, like the C++
        env.set_option_string("fragment_prefixes", "777,,900,");
        manager.on_update_fragment_prefixes(&mut env);
        assert!(is_fragment_phone_number("777"));
        assert!(is_fragment_phone_number("900"));
        assert!(is_fragment_phone_number("555"));

        // the no-option update keeps the current prefixes (cpp:175-176)
        env.options.remove("fragment_prefixes");
        manager.on_update_fragment_prefixes(&mut env);
        assert!(is_fragment_phone_number("555"));

        // restore the pristine lazy state for the other tests
        *FRAGMENT_PREFIXES.lock().unwrap() = (String::new(), Vec::new());
    }

    /// `get_phone_number_info_sync` over the compiled-in list — the
    /// decision table replayed against the simulated C++ matcher
    /// (CountryInfoManager.cpp:330-418): the code/prefix best-match walk,
    /// the RU/KZ dedup on the shared calling code, the pattern
    /// formatting, the is_prefix band and the fragment flag.
    #[test]
    fn phone_number_info_sync_matches_the_cpp_decision_table() {
        let _lock = COUNTRY_TEST_LOCK.lock().unwrap();
        // (phone, country code, calling code, formatted, anonymous)
        let table = [
            ("19991234567", Some("US"), "1", "999 123 4567", false),
            ("79991234567", Some("RU"), "7", "999 123 4567", false),
            ("441234567890", Some("GB"), "44", "1234 567890", false),
            // the shared calling code 7: the stored prefixes pick KZ
            ("76123456789", Some("KZ"), "7", "612 345 67 89", false),
            ("77123456789", Some("KZ"), "7", "712 345 67 89", false),
            ("79123456789", Some("RU"), "7", "912 345 6789", false),
            // the pattern filler: the consumed input leaves dashes/spaces
            ("180", Some("US"), "1", "80- --- ----", false),
            ("7", Some("RU"), "7", "--- --- ----", false),
            // the hidden fragment country with the two-pattern match
            ("88812345678", Some("FT"), "888", "1234 5678", true),
            // the is_prefix band: the C++ null country carries the dialed part
            ("99", None, "99", "", false),
            ("999", None, "", "999", false),
        ];
        for (phone, country_code, calling_code, formatted, is_anonymous) in table {
            let info = get_phone_number_info_sync("en", phone.to_string());
            assert_eq!(
                info.country.as_ref().map(|c| c.country_code.as_str()),
                country_code,
                "country for {phone}"
            );
            assert_eq!(
                info.country_calling_code, calling_code,
                "calling code for {phone}"
            );
            assert_eq!(
                info.formatted_phone_number, formatted,
                "formatted for {phone}"
            );
            assert_eq!(info.is_anonymous, is_anonymous, "anonymous for {phone}");
        }

        // the matched answers carry the full country objects
        let info = get_phone_number_info_sync("en", "441234567890".to_string());
        let country = info.country.as_ref().unwrap();
        assert_eq!(country.name, "United Kingdom");
        assert_eq!(country.english_name, "United Kingdom");
        assert_eq!(country.flag_emoji, "\u{1F1EC}\u{1F1E7}");
        assert_eq!(country.calling_codes, ["44"]);
        let info = get_phone_number_info_sync("en", "88812345678".to_string());
        let country = info.country.as_ref().unwrap();
        assert_eq!(country.name, "Anonymous Numbers");
        assert!(country.is_hidden);

        // the "en" fallback for the missing languages (cpp:322-325)
        assert_eq!(
            get_phone_number_info_sync("de", "19991234567".to_string())
                .country
                .unwrap()
                .country_code,
            "US"
        );

        // the empty-prefix early return (cpp:317-319) — the C++ null country
        let info = get_phone_number_info_sync("en", String::new());
        assert!(info.country.is_none());
        assert_eq!(info.country_calling_code, "");
        assert!(!info.is_anonymous);
    }

    /// `get_phone_number_info` (cpp:271-278): the manager cleans the
    /// prefix before the matcher sees it.
    #[test]
    fn dispatched_phone_number_info_cleans_the_prefix() {
        let _lock = COUNTRY_TEST_LOCK.lock().unwrap();
        let mut manager = CountryInfoManager::default();
        let mut env = TestCountryEnv::new("en");

        let (promise, slot) = captured_promise::<td_api::PhoneNumberInfo>();
        manager.get_phone_number_info(&mut env, "+1 (999) 123-4567".to_string(), promise);
        let info = match &*slot.lock().unwrap() {
            Some(Result::Ok(info)) => info.clone(),
            other => panic!("expected the US info, got {other:?}"),
        };
        assert_eq!(info.country.as_ref().unwrap().country_code, "US");
        assert_eq!(info.country_calling_code, "1");
        assert_eq!(info.formatted_phone_number, "999 123 4567");

        // the letters-only prefix cleans to the empty band (cpp:274-276)
        let (promise, slot) = captured_promise::<td_api::PhoneNumberInfo>();
        manager.get_phone_number_info(&mut env, "abc".to_string(), promise);
        let info = match &*slot.lock().unwrap() {
            Some(Result::Ok(info)) => info.clone(),
            other => panic!("expected the empty info, got {other:?}"),
        };
        assert_eq!(info, td_api::PhoneNumberInfo::default());
    }

    /// `get_countries` over the compiled-in "en" main language: the full
    /// list answers locally; the background reload trigger rides the
    /// `[blocked: net]` seam.
    #[test]
    fn get_countries_answers_from_the_embedded_list() {
        let _lock = COUNTRY_TEST_LOCK.lock().unwrap();
        // reset the cache so the compiled-in embed (and its reload
        // trigger) happens in this test regardless of test order
        COUNTRIES.lock().unwrap().remove("en");
        let mut manager = CountryInfoManager::default();
        let mut env = TestCountryEnv::new("en");

        let (promise, slot) = captured_promise::<td_api::Countries>();
        manager.get_countries(&mut env, promise);
        let countries = match &*slot.lock().unwrap() {
            Some(Result::Ok(countries)) => countries.clone(),
            other => panic!("expected the countries list, got {other:?}"),
        };
        assert_eq!(countries.countries.len(), 235);
        assert_eq!(countries.countries[0].country_code, "AD");
        assert_eq!(countries.countries[0].name, "Andorra");
        assert_eq!(countries.countries[0].calling_codes, ["376"]);
        assert_eq!(countries.countries[0].flag_emoji, "\u{1F1E6}\u{1F1E9}");

        // the embedded-list reload fires once; the pending dedup catches
        // the second round (cpp:426-427)
        assert_eq!(env.countries_list_queries, [("en".to_string(), -882011426)]);
        let (promise, slot) = captured_promise::<td_api::Countries>();
        manager.get_countries(&mut env, promise);
        assert!(matches!(&*slot.lock().unwrap(), Some(Result::Ok(_))));
        assert_eq!(env.countries_list_queries.len(), 1);
    }

    /// The stale-window reload: a seeded old list answers from the cache
    /// and fires exactly one background query (the pending dedup holds
    /// the repeat, the seam never clears it — the net step would).
    #[test]
    fn stale_list_answers_and_fires_one_reload() {
        let _lock = COUNTRY_TEST_LOCK.lock().unwrap();
        COUNTRIES.lock().unwrap().insert(
            "xx".to_string(),
            CountryList {
                countries: Vec::new(),
                hash: 42,
                next_reload_time: 0.0,
            },
        );

        let mut manager = CountryInfoManager::default();
        let mut env = TestCountryEnv::new("xx");
        let (promise, slot) = captured_promise::<td_api::Countries>();
        manager.get_countries(&mut env, promise);
        assert!(matches!(&*slot.lock().unwrap(), Some(Result::Ok(_))));
        assert_eq!(env.countries_list_queries, [("xx".to_string(), 42)]);

        let (promise, slot) = captured_promise::<td_api::Countries>();
        manager.get_countries(&mut env, promise);
        assert!(matches!(&*slot.lock().unwrap(), Some(Result::Ok(_))));
        assert_eq!(env.countries_list_queries.len(), 1);

        COUNTRIES.lock().unwrap().remove("xx");
    }

    /// `load_country_list` (cpp:424-437): the `Auto()` empty-promise
    /// dedup, the real-promise queueing and the per-language queries.
    #[test]
    fn load_country_list_dedups_pending_queries() {
        let _lock = COUNTRY_TEST_LOCK.lock().unwrap();
        let mut manager = CountryInfoManager::default();
        let mut env = TestCountryEnv::new("en");

        manager.load_country_list(
            &mut env,
            "xx".to_string(),
            0,
            PendingCountryQuery::Background,
        );
        manager.load_country_list(
            &mut env,
            "xx".to_string(),
            0,
            PendingCountryQuery::Background,
        );
        assert_eq!(env.countries_list_queries.len(), 1);
        assert_eq!(manager.pending_load_country_queries["xx"].len(), 1);

        let (promise, _slot) = captured_promise::<td_api::Countries>();
        manager.load_country_list(
            &mut env,
            "xx".to_string(),
            0,
            PendingCountryQuery::Countries { promise },
        );
        assert_eq!(manager.pending_load_country_queries["xx"].len(), 2);
        assert_eq!(env.countries_list_queries.len(), 1);

        manager.load_country_list(
            &mut env,
            "yy".to_string(),
            7,
            PendingCountryQuery::Background,
        );
        assert_eq!(
            env.countries_list_queries,
            [("xx".to_string(), 0), ("yy".to_string(), 7)]
        );
    }

    /// The `do_get_*` error bands: the empty-language 400 and the
    /// exhausted-recursive 500 (cpp:214-219/254-259/297-302).
    #[test]
    fn do_get_countries_error_bands() {
        let _lock = COUNTRY_TEST_LOCK.lock().unwrap();
        let mut manager = CountryInfoManager::default();
        let mut env = TestCountryEnv::new("");

        let (promise, slot) = captured_promise::<td_api::Countries>();
        manager.get_countries(&mut env, promise);
        match &*slot.lock().unwrap() {
            Some(Result::Err(status)) => {
                assert_eq!(status.code(), 400);
                assert_eq!(status.message(), "Invalid language code specified");
            }
            other => panic!("expected the 400 band, got {other:?}"),
        }

        // the recursive band: the main language's list stays missing (the
        // seam never feeds the cache), the re-entry errors with 500
        let mut env = TestCountryEnv::new("zz");
        let (promise, slot) = captured_promise::<td_api::Countries>();
        manager.do_get_countries(&mut env, "zz".to_string(), true, promise);
        match &*slot.lock().unwrap() {
            Some(Result::Err(status)) => {
                assert_eq!(status.code(), 500);
                assert_eq!(status.message(), "Requested data is inaccessible");
            }
            other => panic!("expected the 500 band, got {other:?}"),
        }
    }

    /// `get_country` (cpp:231-269): the known codes answer the country
    /// info (the hidden ones included), the unknown codes answer the C++
    /// null (the arm's `Td::send_result` 404 band), and the reload tail
    /// parks at the seam.
    #[test]
    fn get_country_answers_known_and_unknown_codes() {
        let _lock = COUNTRY_TEST_LOCK.lock().unwrap();
        // reset the cache so the compiled-in embed (and its reload
        // trigger) happens in this test regardless of test order
        COUNTRIES.lock().unwrap().remove("en");
        let mut manager = CountryInfoManager::default();
        let mut env = TestCountryEnv::new("en");

        let (promise, slot) = captured_promise::<Option<td_api::CountryInfo>>();
        manager.get_country(&mut env, "GB".to_string(), promise);
        let info = match &*slot.lock().unwrap() {
            Some(Result::Ok(Some(info))) => info.clone(),
            other => panic!("expected the GB info, got {other:?}"),
        };
        assert_eq!(info.country_code, "GB");
        assert_eq!(info.name, "United Kingdom");
        assert_eq!(info.english_name, "United Kingdom");
        assert_eq!(info.flag_emoji, "\u{1F1EC}\u{1F1E7}");
        assert_eq!(info.calling_codes, ["44"]);
        assert!(!info.is_hidden);

        // the hidden countries surface their flag through getCountry
        let (promise, slot) = captured_promise::<Option<td_api::CountryInfo>>();
        manager.get_country(&mut env, "FT".to_string(), promise);
        let info = match &*slot.lock().unwrap() {
            Some(Result::Ok(Some(info))) => info.clone(),
            other => panic!("expected the FT info, got {other:?}"),
        };
        assert!(info.is_hidden);
        assert_eq!(info.calling_codes, ["888"]);

        // the unknown code answers the C++ `set_value(nullptr)` null
        let (promise, slot) = captured_promise::<Option<td_api::CountryInfo>>();
        manager.get_country(&mut env, "ZZ".to_string(), promise);
        assert!(matches!(&*slot.lock().unwrap(), Some(Result::Ok(None))));

        // the embedded-list reload fired on the first touch
        assert_eq!(env.countries_list_queries, [("en".to_string(), -882011426)]);
    }

    /// `get_current_country_code` (cpp:420-422): the promise rides the
    /// net seam (the TestEnv parks it like the Td env does).
    #[test]
    fn get_country_code_rides_the_net_seam() {
        let _lock = COUNTRY_TEST_LOCK.lock().unwrap();
        let mut manager = CountryInfoManager::default();
        let mut env = TestCountryEnv::new("en");

        let (promise, slot) = captured_promise::<String>();
        manager.get_current_country_code(&mut env, promise);
        assert_eq!(env.nearest_dc_queries, 1);
        // the promise parks until the net answer (the TestEnv records it)
        assert!((*slot.lock().unwrap()).is_none());
    }
}
