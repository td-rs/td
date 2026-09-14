//! Port of `td/telegram/ConfigManager.{h,cpp}` (Wave 1, ConfigManager
//! step) — the M.3.1 data layer: the persisted `AppConfig` format, the
//! `dc_options_update` binlog-pmc persistence, the `config_expire`
//! save/load and the [`ConfigActions`]/[`ConfigQuery`] plumbing types the
//! fetch state machine returns. The manager actor, the
//! `help.getConfig`/`help.getAppConfig` request flow and the option
//! application land with the following steps (M.3.2+).
//!
//! The C++ manager is an actor that dispatches its own `NetQuery`s and
//! reads `G()` everywhere. The Rust port is an inline manager on the Td
//! actor: the network queries are returned as [`ConfigQuery`] values for
//! the Td to hold (the client-side dispatcher lands with the net step),
//! the binlog pmc and the `OptionManager` are passed in by the Td at
//! each call, and the pending promises are request ids.

use tddb::binlog_key_value::BinlogKeyValue;
use tdmtproto::telegram_api;
use tdutils::status::Status;

use crate::td_api;

/// `ConfigManager::AppConfig::CURRENT_VERSION` (ConfigManager.h:77).
pub const APP_CONFIG_CURRENT_VERSION: i32 = 135;

/// The pending-request bookkeeping every result handler returns: the Td
/// actor completes the requests, applies the premium-options refresh and
/// holds the follow-up network queries.
#[derive(Default)]
pub struct ConfigActions {
    /// Request ids to answer with `ok`.
    pub ok_ids: Vec<u64>,
    /// Request ids to fail with the error.
    pub errors: Vec<(u64, Status)>,
    /// `getOption` requests that complete now, answered from the option
    /// store (the C++ `wrap_promise` in `OptionManager::get_option`).
    pub get_option_answers: Vec<(u64, String)>,
    /// `getApplicationConfig` requests, answered with the app config.
    pub app_config_answers: Vec<(u64, td_api::JsonValue)>,
    /// Follow-up network queries to dispatch.
    pub queries: Vec<ConfigQuery>,
    /// `OptionManager::update_premium_options` ran after the app config
    /// arrived (ConfigManager.cpp:1112).
    pub refresh_premium_options: bool,
}

/// One of the four network queries the ConfigManager sends — the C++
/// `net_query_creator().create[_unauth](...)` calls. The Td holds the
/// returned queries until the client-side dispatcher lands (the net step).
#[derive(Clone, Debug, PartialEq)]
pub enum ConfigQuery {
    /// `request_config_from_dc_impl` — `help.getConfig` on `dc_id`
    /// (unauth).
    GetConfig { dc_id: i32, reopen_sessions: bool },
    /// `try_request_app_config` — `help.getAppConfig` (unauth).
    GetAppConfig { hash: i32 },
    /// `get_content_settings` — `account.getContentSettings` (auth).
    GetContentSettings,
    /// `set_content_settings` — `account.setContentSettings` (auth).
    SetContentSettings { sensitive_enabled: bool },
}

impl ConfigQuery {
    /// The serialized TL body (constructor id + fields) — the bytes the
    /// dispatcher sends. Tested via the generated parse entry points.
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            ConfigQuery::GetConfig { .. } => {
                telegram_api::help_getConfig.store_boxed(&mut out);
            }
            ConfigQuery::GetAppConfig { hash } => {
                telegram_api::help_getAppConfig { hash: *hash }.store_boxed(&mut out);
            }
            ConfigQuery::GetContentSettings => {
                telegram_api::account_getContentSettings.store_boxed(&mut out);
            }
            ConfigQuery::SetContentSettings { sensitive_enabled } => {
                telegram_api::account_setContentSettings {
                    flags: if *sensitive_enabled { 1 } else { 0 },
                    sensitive_enabled: *sensitive_enabled,
                }
                .store_boxed(&mut out);
            }
        }
        out
    }
}

/// `help.getAppConfig` response — either the payload or the hash-matched
/// `help.appConfigNotModified`.
#[derive(Clone, Debug, PartialEq)]
pub enum AppConfigQueryResult {
    NotModified,
    Config {
        hash: i32,
        config: Box<telegram_api::JSONValue>,
    },
}

/// The completed-query payloads the Td feeds back into the manager (the
/// C++ `ConfigManager::on_result` token dispatch).
#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)] // the payload types are the scheme's own (`Config` is huge in the C++ too, but lives behind a tl_object_ptr)
pub enum ConfigQueryResult {
    GetConfig(Result<telegram_api::Config, Status>),
    GetAppConfig(Result<AppConfigQueryResult, Status>),
    GetContentSettings(Result<telegram_api::account_ContentSettings, Status>),
    SetContentSettings(Result<(), Status>),
}

/// `ConfigManager::AppConfig` (ConfigManager.h:76-87) — the persisted app
/// config: version + hash + the (filtered) JSON object.
#[derive(Clone, Debug)]
pub struct AppConfig {
    version: i32,
    hash: i32,
    config: Box<telegram_api::JSONValue>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            version: 0,
            hash: 0,
            config: Box::new(telegram_api::JSONValue::jsonNull(telegram_api::jsonNull)),
        }
    }
}

impl AppConfig {
    /// The C++ `store`: two raw ints then the raw TL buffer of the JSON
    /// value (`fetch_string_raw` + `TlBufferParser`).
    pub fn store(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&self.version.to_le_bytes());
        out.extend_from_slice(&self.hash.to_le_bytes());
        self.config.store(&mut out);
        out
    }

    /// The C++ `parse`; an empty or truncated value fails like the C++
    /// `log_event_parse` on an empty pmc slice.
    pub fn parse(input: &[u8]) -> Result<Self, &'static str> {
        if input.len() < 8 {
            return Err("Invalid config");
        }
        let version = i32::from_le_bytes(input[0..4].try_into().expect("length checked"));
        if version != APP_CONFIG_CURRENT_VERSION {
            return Err("Invalid config version");
        }
        let hash = i32::from_le_bytes(input[4..8].try_into().expect("length checked"));
        let config = Box::new(telegram_api::JSONValue::parse(&input[8..])?);
        Ok(Self {
            version,
            hash,
            config,
        })
    }
}

/// `DcOption::Flags` (DcOptions.h:24) — the stored-form flags.
const DC_OPTION_IPV6: i32 = 1;
const DC_OPTION_MEDIA_ONLY: i32 = 2;
const DC_OPTION_OBFUSCATED_TCP_ONLY: i32 = 4;
const DC_OPTION_CDN: i32 = 8;
const DC_OPTION_STATIC: i32 = 16;
const DC_OPTION_HAS_SECRET: i32 = 32;

/// `DcId::MAX_RAW_DC_ID` (net/DcId.h:20).
const MAX_RAW_DC_ID: i32 = 1000;

/// The persisted `dc_options_update` entry — the C++ `DcOption::store`
/// form: flags, raw dc id (the plain id — the `Cdn` flag carries the
/// externality, `DcId::get_raw_id()`), ip string, port, secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredDcOption {
    flags: i32,
    raw_dc_id: i32,
    ip: Vec<u8>,
    port: i32,
    secret: Vec<u8>,
}

impl StoredDcOption {
    /// The C++ `DcOption(const telegram_api::dcOption &)` + the
    /// `is_valid` filter at the DcOptions constructor.
    fn from_telegram(option: &telegram_api::DcOption) -> Option<Self> {
        if !(1..=MAX_RAW_DC_ID).contains(&option.id) {
            return None;
        }
        let mut flags = 0;
        if option.ipv6 {
            flags |= DC_OPTION_IPV6;
        }
        if option.media_only {
            flags |= DC_OPTION_MEDIA_ONLY;
        }
        if option.tcpo_only {
            flags |= DC_OPTION_OBFUSCATED_TCP_ONLY;
        }
        if option.cdn {
            flags |= DC_OPTION_CDN;
        }
        if option.r#static {
            flags |= DC_OPTION_STATIC;
        }
        if !option.secret.is_empty() {
            flags |= DC_OPTION_HAS_SECRET;
        }
        // `is_valid`: a valid ip address and an exact dc id (the ip parse
        // itself lands with the net step — an empty address is dropped).
        if option.ip_address.is_empty() {
            return None;
        }
        Some(Self {
            flags,
            raw_dc_id: option.id,
            ip: option.ip_address.as_bytes().to_vec(),
            port: option.port,
            secret: option.secret.clone(),
        })
    }

    pub fn store(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.flags.to_le_bytes());
        out.extend_from_slice(&self.raw_dc_id.to_le_bytes());
        store_log_event_string(out, &self.ip);
        out.extend_from_slice(&self.port.to_le_bytes());
        if self.flags & DC_OPTION_HAS_SECRET != 0 {
            store_log_event_string(out, &self.secret);
        }
    }

    pub fn parse(input: &[u8], pos: &mut usize) -> Result<Self, &'static str> {
        let flags = fetch_i32(input, pos)?;
        let raw_dc_id = fetch_i32(input, pos)?;
        let ip = fetch_log_event_string(input, pos)?;
        let port = fetch_i32(input, pos)?;
        let secret = if flags & DC_OPTION_HAS_SECRET != 0 {
            fetch_log_event_string(input, pos)?
        } else {
            Vec::new()
        };
        Ok(Self {
            flags,
            raw_dc_id,
            ip,
            port,
            secret,
        })
    }
}

/// A raw little-endian int32 at `pos`.
fn fetch_i32(input: &[u8], pos: &mut usize) -> Result<i32, &'static str> {
    if *pos + 4 > input.len() {
        return Err("Invalid int");
    }
    let value = i32::from_le_bytes(input[*pos..*pos + 4].try_into().expect("length checked"));
    *pos += 4;
    Ok(value)
}

/// `td::store(Slice)` in `tl_helpers.h` — the TL string length prefix
/// rules (1 byte under 254, `254` + 3 LE bytes under 2^24, else `255` +
/// 4 LE bytes + 3 zero bytes) followed by the raw bytes.
fn store_log_event_string(out: &mut Vec<u8>, bytes: &[u8]) {
    let len = bytes.len();
    if len < 254 {
        out.push(len as u8);
    } else if len < (1 << 24) {
        out.push(254);
        out.extend_from_slice(&(len as u32).to_le_bytes()[..3]);
    } else {
        out.push(255);
        out.extend_from_slice(&(len as u32).to_le_bytes());
        out.extend_from_slice(&[0, 0, 0]);
    }
    out.extend_from_slice(bytes);
}

/// `parser.fetch_string<Slice>()` — the inverse of [`store_log_event_string`].
fn fetch_log_event_string(input: &[u8], pos: &mut usize) -> Result<Vec<u8>, &'static str> {
    if *pos >= input.len() {
        return Err("Invalid string");
    }
    let first = input[*pos];
    *pos += 1;
    let len = match first {
        0..=253 => first as usize,
        254 => {
            if *pos + 3 > input.len() {
                return Err("Invalid string");
            }
            let mut bytes = [0u8; 4];
            bytes[..3].copy_from_slice(&input[*pos..*pos + 3]);
            *pos += 3;
            u32::from_le_bytes(bytes) as usize
        }
        255 => {
            if *pos + 7 > input.len() {
                return Err("Invalid string");
            }
            let len = u32::from_le_bytes(input[*pos..*pos + 4].try_into().expect("length checked"));
            *pos += 7;
            len as usize
        }
    };
    if *pos + len > input.len() {
        return Err("Invalid string");
    }
    let bytes = input[*pos..*pos + len].to_vec();
    *pos += len;
    Ok(bytes)
}

/// The persisted `dc_options_update` — the C++ `DcOptions::store`:
/// an int32 count followed by the stored options.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StoredDcOptions {
    options: Vec<StoredDcOption>,
}

impl StoredDcOptions {
    pub fn store(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(self.options.len() as i32).to_le_bytes());
        for option in &self.options {
            option.store(&mut out);
        }
        out
    }

    pub fn parse(input: &[u8]) -> Result<Self, &'static str> {
        if input.len() < 4 {
            return Err("Invalid dc options");
        }
        let mut pos = 0;
        let count = i32::from_le_bytes(input[0..4].try_into().expect("length checked")) as usize;
        pos += 4;
        let mut options = Vec::with_capacity(count.min(1024));
        for _ in 0..count {
            options.push(StoredDcOption::parse(input, &mut pos)?);
        }
        Ok(Self { options })
    }
}

/// `ConfigManager::save_dc_options_update` (ConfigManager.cpp:1142-1148):
/// persist the filtered options as the C++ `DcOptions::store` log-event
/// blob under `dc_options_update`; an empty list erases the key.
pub fn save_dc_options_update(pmc: &mut BinlogKeyValue, dc_options: &[telegram_api::DcOption]) {
    let stored: Vec<StoredDcOption> = dc_options
        .iter()
        .filter_map(StoredDcOption::from_telegram)
        .collect();
    if stored.is_empty() {
        pmc.erase("dc_options_update");
        return;
    }
    pmc.set_raw(
        b"dc_options_update",
        &StoredDcOptions { options: stored }.store(),
    );
}

/// `ConfigManager::load_config_expire_time` (cpp:1159-1167) — the stored
/// wall-clock expiry, clamped to [now, now + 1 hour], mapped onto the
/// `time::now()` basis.
pub fn load_config_expire_time(pmc: &BinlogKeyValue) -> f64 {
    let stored: i32 = pmc.get("config_expire").parse().unwrap_or(0);
    let expires_in = f64::from(stored) - tdutils::time::system();
    if !(0.0..=3600.0).contains(&expires_in) {
        return tdutils::time::now();
    }
    tdutils::time::now() + expires_in
}

/// `ConfigManager::save_config_expire` (cpp:1169-1171).
pub fn save_config_expire(pmc: &mut BinlogKeyValue, expire_time: f64) {
    let expires_in = expire_time - tdutils::time::now();
    pmc.set(
        "config_expire",
        &((tdutils::time::system() + expires_in) as i32).to_string(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use tddb::db_key::DbKey;

    fn temp_pmc_path(name: &str) -> String {
        std::env::temp_dir()
            .join(format!("td-rs-m31-{name}-{}.binlog", std::process::id()))
            .to_string_lossy()
            .into_owned()
    }

    fn open_pmc(path: &str) -> BinlogKeyValue {
        let mut pmc = BinlogKeyValue::new();
        pmc.init(path, DbKey::empty()).ensure();
        pmc
    }

    fn dc_option(id: i32, ip_address: &str, cdn: bool, secret: Vec<u8>) -> telegram_api::DcOption {
        telegram_api::DcOption {
            flags: 0,
            ipv6: false,
            media_only: false,
            tcpo_only: false,
            cdn,
            r#static: false,
            this_port_only: false,
            id,
            ip_address: ip_address.to_string(),
            port: 443,
            secret,
        }
    }

    #[test]
    fn app_config_bytes_match_the_cpp_layout() {
        let app_config = AppConfig {
            version: APP_CONFIG_CURRENT_VERSION,
            hash: 0,
            config: Box::new(telegram_api::JSONValue::jsonObject(
                telegram_api::jsonObject { value: Vec::new() },
            )),
        };
        let bytes = app_config.store();
        let mut expected = Vec::new();
        expected.extend_from_slice(&APP_CONFIG_CURRENT_VERSION.to_le_bytes());
        expected.extend_from_slice(&0i32.to_le_bytes());
        expected.extend_from_slice(&telegram_api::jsonObject::ID.to_le_bytes());
        expected.extend_from_slice(&telegram_api::tl::VECTOR_ID.to_le_bytes());
        expected.extend_from_slice(&0i32.to_le_bytes());
        assert_eq!(bytes, expected);

        let parsed = AppConfig::parse(&bytes).unwrap();
        assert_eq!(parsed.version, APP_CONFIG_CURRENT_VERSION);
        assert_eq!(parsed.hash, 0);
        assert!(matches!(
            parsed.config.as_ref(),
            telegram_api::JSONValue::jsonObject(_)
        ));
        assert!(AppConfig::parse(b"").is_err());
        assert!(AppConfig::parse(b"short").is_err());
        let mut wrong_version = bytes.clone();
        wrong_version[0..4].copy_from_slice(&(APP_CONFIG_CURRENT_VERSION - 1).to_le_bytes());
        assert!(AppConfig::parse(&wrong_version).is_err());
    }

    #[test]
    fn stored_dc_option_bytes_match_the_cpp_layout() {
        // `DcOption::store` (DcOptions.h): store_int(flags),
        // store_int(get_raw_id), store_string(ip), store_int(port) — the
        // CDN case keeps the plain id, the flag carries the externality.
        let option = StoredDcOption {
            flags: DC_OPTION_OBFUSCATED_TCP_ONLY | DC_OPTION_CDN,
            raw_dc_id: 5,
            ip: b"149.154.167.40".to_vec(),
            port: 443,
            secret: Vec::new(),
        };
        let mut bytes = Vec::new();
        option.store(&mut bytes);
        let mut expected = Vec::new();
        expected.extend_from_slice(&(DC_OPTION_OBFUSCATED_TCP_ONLY | DC_OPTION_CDN).to_le_bytes());
        expected.extend_from_slice(&5i32.to_le_bytes());
        store_log_event_string(&mut expected, b"149.154.167.40");
        expected.extend_from_slice(&443i32.to_le_bytes());
        assert_eq!(bytes, expected);

        let mut pos = 0;
        let parsed = StoredDcOption::parse(&bytes, &mut pos).unwrap();
        assert_eq!(parsed, option);
        assert_eq!(pos, bytes.len());
    }

    #[test]
    fn stored_dc_option_with_secret_matches_the_cpp_layout() {
        let secret = vec![0xAB; 16];
        let option = StoredDcOption {
            flags: DC_OPTION_HAS_SECRET | DC_OPTION_STATIC,
            raw_dc_id: 2,
            ip: b"127.0.0.1".to_vec(),
            port: 80,
            secret: secret.clone(),
        };
        let mut bytes = Vec::new();
        option.store(&mut bytes);
        let mut expected = Vec::new();
        expected.extend_from_slice(&(DC_OPTION_HAS_SECRET | DC_OPTION_STATIC).to_le_bytes());
        expected.extend_from_slice(&2i32.to_le_bytes());
        store_log_event_string(&mut expected, b"127.0.0.1");
        expected.extend_from_slice(&80i32.to_le_bytes());
        store_log_event_string(&mut expected, &secret);
        assert_eq!(bytes, expected);

        let mut pos = 0;
        assert_eq!(StoredDcOption::parse(&bytes, &mut pos).unwrap(), option);
        // A truncated secret tail fails.
        let mut pos = 0;
        assert!(StoredDcOption::parse(&bytes[..bytes.len() - 4], &mut pos).is_err());
    }

    #[test]
    fn stored_dc_options_bytes_match_the_cpp_layout() {
        let options = StoredDcOptions {
            options: vec![
                StoredDcOption {
                    flags: 0,
                    raw_dc_id: 1,
                    ip: b"149.154.175.10".to_vec(),
                    port: 443,
                    secret: Vec::new(),
                },
                StoredDcOption {
                    flags: DC_OPTION_CDN | DC_OPTION_HAS_SECRET,
                    raw_dc_id: 3,
                    ip: b"1.2.3.4".to_vec(),
                    port: 80,
                    secret: vec![7; 16],
                },
            ],
        };
        let bytes = options.store();
        let mut expected = Vec::new();
        expected.extend_from_slice(&2i32.to_le_bytes());
        expected.extend_from_slice(&0i32.to_le_bytes());
        expected.extend_from_slice(&1i32.to_le_bytes());
        store_log_event_string(&mut expected, b"149.154.175.10");
        expected.extend_from_slice(&443i32.to_le_bytes());
        expected.extend_from_slice(&(DC_OPTION_CDN | DC_OPTION_HAS_SECRET).to_le_bytes());
        expected.extend_from_slice(&3i32.to_le_bytes());
        store_log_event_string(&mut expected, b"1.2.3.4");
        expected.extend_from_slice(&80i32.to_le_bytes());
        store_log_event_string(&mut expected, &[7; 16]);
        assert_eq!(bytes, expected);

        assert_eq!(StoredDcOptions::parse(&bytes).unwrap(), options);
        assert!(StoredDcOptions::parse(b"").is_err());
    }

    #[test]
    fn dc_option_from_telegram_keeps_the_plain_raw_id_for_cdn() {
        // `DcId::get_raw_id()` returns the plain id; the `Cdn` flag carries
        // the externality and the C++ parser rebuilds `DcId::external` from
        // it. A negated stored id would diverge from the C++ bytes.
        let stored = StoredDcOption::from_telegram(&dc_option(5, "1.2.3.4", true, vec![]))
            .expect("valid option");
        assert_eq!(stored.raw_dc_id, 5);
        assert_ne!(stored.flags & DC_OPTION_CDN, 0);
        assert_eq!(stored.flags & DC_OPTION_HAS_SECRET, 0);
        // The `is_valid` filter: an out-of-range id or an empty ip drops.
        assert!(StoredDcOption::from_telegram(&dc_option(0, "1.2.3.4", false, vec![])).is_none());
        assert!(StoredDcOption::from_telegram(&dc_option(5, "", false, vec![])).is_none());
    }

    #[test]
    fn dc_options_update_round_trips_across_reopen() {
        let path = temp_pmc_path("dc-options");
        let _ = BinlogKeyValue::destroy(&path);
        let options = vec![
            dc_option(2, "149.154.167.40", false, vec![]),
            dc_option(5, "1.2.3.4", true, vec![7; 16]),
        ];
        {
            let mut pmc = open_pmc(&path);
            save_dc_options_update(&mut pmc, &options);
            pmc.close();
        }
        let stored = {
            let mut pmc = open_pmc(&path);
            let stored = pmc.get_raw(b"dc_options_update");
            pmc.close();
            stored
        };
        let expected = StoredDcOptions {
            options: options
                .iter()
                .filter_map(StoredDcOption::from_telegram)
                .collect(),
        };
        assert_eq!(stored, expected.store());
        assert_eq!(StoredDcOptions::parse(&stored).unwrap(), expected);
        let _ = BinlogKeyValue::destroy(&path);
    }

    #[test]
    fn empty_dc_options_update_erases_the_pmc_key() {
        let path = temp_pmc_path("dc-options-empty");
        let _ = BinlogKeyValue::destroy(&path);
        {
            let mut pmc = open_pmc(&path);
            save_dc_options_update(&mut pmc, &[dc_option(2, "149.154.167.40", false, vec![])]);
            save_dc_options_update(&mut pmc, &[]);
            assert!(pmc.get_raw(b"dc_options_update").is_empty());
            pmc.close();
        }
        let _ = BinlogKeyValue::destroy(&path);
    }

    #[test]
    fn config_expire_round_trips_across_reopen() {
        let path = temp_pmc_path("config-expire");
        let _ = BinlogKeyValue::destroy(&path);
        let expire_time = tdutils::time::now() + 300.0;
        {
            let mut pmc = open_pmc(&path);
            save_config_expire(&mut pmc, expire_time);
            // The C++ form: the decimal wall-clock int.
            let stored: i32 = pmc.get("config_expire").parse().unwrap();
            let expected = (tdutils::time::system() + 300.0) as i32;
            assert!((stored - expected).abs() <= 1);
            pmc.close();
        }
        let loaded = {
            let mut pmc = open_pmc(&path);
            let loaded = load_config_expire_time(&pmc);
            pmc.close();
            loaded
        };
        assert!((loaded - expire_time).abs() < 2.0);
        let _ = BinlogKeyValue::destroy(&path);
    }

    #[test]
    fn config_expire_clamps_out_of_range_values() {
        let path = temp_pmc_path("config-expire-clamp");
        let _ = BinlogKeyValue::destroy(&path);
        {
            let mut pmc = open_pmc(&path);
            let past = (tdutils::time::system() as i32) - 10;
            pmc.set("config_expire", &past.to_string());
            assert!(load_config_expire_time(&pmc) <= tdutils::time::now() + 1.0);
            let too_far = (tdutils::time::system() as i32) + 3601;
            pmc.set("config_expire", &too_far.to_string());
            assert!(load_config_expire_time(&pmc) <= tdutils::time::now() + 1.0);
            // A non-integer stored value reads as 0 → also clamped to now.
            pmc.set("config_expire", "garbage");
            assert!(load_config_expire_time(&pmc) <= tdutils::time::now() + 1.0);
            pmc.close();
        }
        let _ = BinlogKeyValue::destroy(&path);
    }
}
