//! Port of the `ConfigRecoverer` static helpers (`ConfigManager.cpp:83-330`,
//! the free functions above the actor) — the DNS/HTTP fallback for a missing
//! config:
//!
//! - [`decode_config`] — the simple-config blob: RSA-encrypted AES key
//!   material (`decrypt_signature` with the pinned simple-config public
//!   key), AES-CBC payload, SHA-256 truncation check and the TL
//!   `help.configSimple` parse.
//! - [`get_simple_config_azure`] / [`get_simple_config_google_dns`] /
//!   [`get_simple_config_mozilla_dns`] / [`get_simple_config_firebase_realtime`]
//!   / [`get_simple_config_firebase_firestore`] — the five fallback fetchers
//!   over the ported [`tdnet::wget::wget`].
//!
//! Divergences (all documented inline):
//! - The C++ fetchers return a `Wget` actor on a caller-chosen scheduler
//!   and resolve a `Promise<SimpleConfigResult>`; the port is an async
//!   function returning `Result<SimpleConfigResult, Status>` (the outer
//!   error is the transport outcome, `r_config`/`r_http_date` carry the
//!   extraction/decode/date-header outcomes), and the `scheduler_id` has no
//!   Rust counterpart.
//! - `get_simple_config_firebase_remote_config` stays unported: it is
//!   commented out in the C++ too (ConfigManager.cpp:251-287), payload
//!   generation included.
//! - The `domain_name` argument of the azure fetcher is unused there, as in
//!   the C++.
//!
//! The recoverer itself (the `ConfigRecoverer` actor, ConfigManager.cpp:499-832)
//! lands as an inline state machine in sub-steps: this file currently carries
//! the recoverer state and the config application (M.3.7.1); the loop/state
//! callbacks, the fetch orchestration, the full-config mini-session and the Td
//! wiring follow in M.3.7.2-M.3.7.5.

use std::sync::OnceLock;

use base64::Engine as _;
use tddb::binlog_key_value::BinlogKeyValue;
use tdmtproto::proxy_secret::ProxySecret;
use tdmtproto::telegram_api;
use tdnet::http_query::HttpQuery;
use tdnet::wget::{VerifyPeer, WgetParams, wget};
use tdutils::http_date::parse_http_date;
use tdutils::http_url::url_encode;
use tdutils::json::{JsonValue, JsonValueType, json_decode};
use tdutils::rsa::Rsa;
use tdutils::status::Status;

use crate::config_manager::{StoredDcOption, StoredDcOptions};

/// `SimpleConfigResult` (ConfigManager.h:32) — `r_http_date` is the parsed
/// response `date` header, `r_config` the extraction + [`decode_config`]
/// outcome.
#[derive(Debug)]
pub struct SimpleConfigResult {
    /// `r_config`.
    pub r_config: Result<telegram_api::help_ConfigSimple, Status>,
    /// `r_http_date`.
    pub r_http_date: Result<i32, Status>,
}

/// The pinned simple-config RSA public key (ConfigManager.cpp:86-97).
const SIMPLE_CONFIG_RSA_PEM: &str = "-----BEGIN RSA PUBLIC KEY-----\n\
MIIBCgKCAQEAyr+18Rex2ohtVy8sroGP
BwXD3DOoKCSpjDqYoXgCqB7ioln4eDCFfOBUlfXUEvM/fnKCpF46VkAftlb4VuPD
eQSS/ZxZYEGqHaywlroVnXHIjgqoxiAd192xRGreuXIaUKmkwlM9JID9WS2jUsTp
zQ91L8MEPLJ/4zrBwZua8W5fECwCCh2c9G5IzzBm+otMS/YKwmR1olzRCyEkyAEj
XWqBI9Ftv5eG8m0VkBzOG655WIYdyV0HfDK/NWcvGqa0w/nriMD6mDjKOryamw0O
P9QuYgMN0C9xMW9y8SmP4h92OAWodTYgY1hZCxdv6cs5UnW9+PWvS+WIbkh+GaWY
xwIDAQAB
-----END RSA PUBLIC KEY-----
";

/// The `static auto rsa` in `decode_config` — parsed once, `expect`ed like
/// the C++ `.move_as_ok()`.
fn simple_config_rsa() -> &'static Rsa {
    static RSA: OnceLock<Rsa> = OnceLock::new();
    RSA.get_or_init(|| match Rsa::from_pem_public_key(SIMPLE_CONFIG_RSA_PEM) {
        tdutils::status::Result::Ok(rsa) => rsa,
        tdutils::status::Result::Err(status) => {
            panic!("the pinned simple-config RSA key must parse: {status}")
        }
    })
}

/// The standard-base64 character value (64 = not in the table), the
/// `get_character_table<false>()` lookups of `base64_filter`.
fn base64_char_value(c: u8) -> u8 {
    match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        _ => 64,
    }
}

/// `td::base64_filter` — keep the base64 characters and `=`, drop the rest.
fn base64_filter(input: &[u8]) -> String {
    input
        .iter()
        .copied()
        .filter(|&c| base64_char_value(c) != 64 || c == b'=')
        .map(|c| c as char)
        .collect()
}

/// `decode_config` (ConfigManager.cpp:84-141) — a simple-config blob is 344
/// base64 characters of an RSA "signature" over 32 key bytes, 16 IV bytes
/// (overlapping the key tail) and 224 AES-CBC bytes, whose last 16 bytes
/// are the first 16 bytes of the SHA-256 of the first 208.
pub fn decode_config(input: &str) -> Result<telegram_api::help_ConfigSimple, Status> {
    let rsa = simple_config_rsa();

    if input.len() < 344 || input.len() > 1024 {
        return Err(Status::error_message(&format!(
            "Invalid [length:{}]",
            input.len()
        )));
    }

    let data_base64 = base64_filter(input.as_bytes());
    if data_base64.len() != 344 {
        return Err(Status::error_message(&format!(
            "Invalid [length:{}] after base64_filter",
            data_base64.len()
        )));
    }
    let data_rsa = match base64::engine::general_purpose::STANDARD.decode(data_base64.as_bytes()) {
        Ok(data) => data,
        Err(_) => return Err(Status::error_message("Can't decode base64")),
    };
    if data_rsa.len() != 256 {
        return Err(Status::error_message(&format!(
            "Invalid [length:{}] after base64_decode",
            data_rsa.len()
        )));
    }

    // `rsa.decrypt_signature(data_rsa_slice, data_rsa_slice)` — in place in
    // the C++; the Rust operation needs distinct slices, so the output is a
    // separate buffer.
    let mut data = [0u8; 256];
    rsa.decrypt_signature(&data_rsa, &mut data);

    let mut key = [0u8; 32];
    key.copy_from_slice(&data[0..32]);
    let mut iv = [0u8; 16];
    iv.copy_from_slice(&data[16..32]);

    // `aes_cbc_decrypt(as_slice(key), as_mutable_slice(iv), data_cbc,
    // data_cbc)` — again in place in the C++.
    let cipher = data[32..].to_vec();
    let mut data_cbc = vec![0u8; 224];
    tdutils::crypto::aes_cbc_decrypt(&key, &mut iv, &cipher, &mut data_cbc);

    let hash = tdutils::crypto::sha256(&data_cbc[0..208]);
    if data_cbc[208..224] != hash[0..16] {
        return Err(Status::error_message("SHA256 mismatch"));
    }

    let len = i32::from_le_bytes(data_cbc[0..4].try_into().expect("224 bytes checked"));
    if !(8..=208).contains(&len) {
        return Err(Status::error_message(&format!(
            "Invalid [data length:{len}] after aes_cbc_decrypt"
        )));
    }
    let constructor_id = i32::from_le_bytes(data_cbc[4..8].try_into().expect("224 bytes checked"));
    if constructor_id != telegram_api::help_ConfigSimple::ID {
        return Err(Status::error_message(&format!(
            "Wrong [constructor:{:#x}]",
            constructor_id as u32
        )));
    }
    let raw_config = &data_cbc[8..len as usize];
    let mut reader = telegram_api::tl::Reader::new(raw_config);
    let config =
        telegram_api::help_ConfigSimple::read_bare(&mut reader).map_err(Status::error_message)?;
    // `parser.fetch_end()` — the exact-size buffer must be consumed.
    if reader.remaining() != 0 {
        return Err(Status::error_message("Extra data"));
    }
    Ok(config)
}

/// The request half of a fetcher: what [`get_simple_config_impl`] sends.
struct SimpleConfigRequest {
    url: String,
    /// Extra headers before the shared `Host`/`User-Agent`.
    headers: Vec<(String, String)>,
    content: Vec<u8>,
    content_type: String,
    /// The `Host` header value (the C++ `host` argument).
    host: &'static str,
}

/// `get_simple_config_azure`'s request (ConfigManager.cpp:151-160).
fn azure_request(is_test: bool) -> SimpleConfigRequest {
    SimpleConfigRequest {
        url: format!(
            "https://software-download.microsoft.com/{}v2/config.txt",
            if is_test { "test" } else { "prod" }
        ),
        headers: Vec::new(),
        content: Vec::new(),
        content_type: String::new(),
        host: "tcdnb.azureedge.net",
    }
}

/// `get_simple_config_dns`'s request (ConfigManager.cpp:216-222); the
/// default TXT domain is `tapv3.stel.com`/`apv3.stel.com`.
fn dns_request(
    address: &'static str,
    host: &'static str,
    domain_name: &str,
    is_test: bool,
) -> SimpleConfigRequest {
    let domain_name = if domain_name.is_empty() {
        if is_test {
            "tapv3.stel.com"
        } else {
            "apv3.stel.com"
        }
    } else {
        domain_name
    };
    SimpleConfigRequest {
        url: format!(
            "https://{address}?name={}&type=TXT",
            url_encode(domain_name)
        ),
        headers: vec![("Accept".to_string(), "application/dns-json".to_string())],
        content: Vec::new(),
        content_type: String::new(),
        host,
    }
}

/// `get_simple_config_firebase_realtime`'s request
/// (ConfigManager.cpp:322-337).
fn firebase_realtime_request() -> SimpleConfigRequest {
    SimpleConfigRequest {
        url: "https://reserve-5a846.firebaseio.com/ipconfigv3.json".to_string(),
        headers: Vec::new(),
        content: Vec::new(),
        content_type: String::new(),
        host: "reserve-5a846.firebaseio.com",
    }
}

/// `get_simple_config_firebase_firestore`'s request
/// (ConfigManager.cpp:339-360).
fn firebase_firestore_request() -> SimpleConfigRequest {
    SimpleConfigRequest {
        url:
            "https://www.google.com/v1/projects/reserve-5a846/databases/(default)/documents/ipconfig/v3"
                .to_string(),
        headers: Vec::new(),
        content: Vec::new(),
        content_type: String::new(),
        host: "firestore.googleapis.com",
    }
}

/// The DNS `get_config` lambda (ConfigManager.cpp:167-215): the two TXT
/// parts arrive either as the `Answer` arg (the JSON-typed response body
/// was split into args) or as the whole body; the parts concatenate
/// bigger-first.
fn get_dns_config(http_query: &HttpQuery) -> Result<String, Status> {
    fn get_data(answer: &[JsonValue]) -> Result<String, Status> {
        let mut parts = Vec::new();
        for answer_part in answer {
            if answer_part.ty() != JsonValueType::Object {
                return Err(Status::error_message("Expected JSON object"));
            }
            let part = into_std(answer_part.get_object().get_required_string_field("data"))?;
            parts.push(part);
        }
        if parts.len() != 2 {
            return Err(Status::error_message("Expected data in two parts"));
        }
        if parts[0].len() < parts[1].len() {
            Ok(parts[1].clone() + &parts[0])
        } else {
            Ok(parts[0].clone() + &parts[1])
        }
    }

    if !http_query.get_arg("Answer").unwrap_or("").is_empty() {
        let answer_arg = http_query.get_arg("Answer").unwrap_or("");
        tdutils::log_info!("Receive DNS response {answer_arg}");
        let answer = into_std(json_decode(answer_arg))?;
        if answer.ty() != JsonValueType::Array {
            return Err(Status::error_message("Expected JSON array"));
        }
        get_data(answer.get_array())
    } else {
        tdutils::log_info!(
            "Receive DNS response {}",
            String::from_utf8_lossy(&http_query.body)
        );
        let mut json = into_std(json_decode(&String::from_utf8_lossy(&http_query.body)))?;
        if json.ty() != JsonValueType::Object {
            return Err(Status::error_message("Expected JSON object"));
        }
        let answer = into_std(
            json.get_object_mut()
                .extract_required_field("Answer", JsonValueType::Array),
        )?;
        get_data(answer.get_array())
    }
}

/// The firebase-realtime `get_config` lambda: the bare JSON string body
/// lands under the `"content"` arg.
fn get_firebase_realtime_config(http_query: &HttpQuery) -> Result<String, Status> {
    Ok(http_query.get_arg("content").unwrap_or("").to_string())
}

/// The firebase-firestore `get_config` lambda (ConfigManager.cpp:343-356):
/// `fields.data.stringValue`.
fn get_firebase_firestore_config(http_query: &HttpQuery) -> Result<String, Status> {
    let mut json = into_std(json_decode(http_query.get_arg("fields").unwrap_or("")))?;
    if json.ty() != JsonValueType::Object {
        return Err(Status::error_message("Expected JSON object"));
    }
    let data = into_std(
        json.get_object_mut()
            .extract_required_field("data", JsonValueType::Object),
    )?;
    into_std(data.get_object().get_required_string_field("stringValue"))
}

/// `get_simple_config_impl` (ConfigManager.cpp:143-149): the shared Wget
/// wrapper — 10s timeout, 3 redirects, TLS peer verification off, the
/// pinned `Host`/`User-Agent`, then `date`-header parsing, the fetcher's
/// extraction and [`decode_config`].
async fn get_simple_config_impl(
    request: SimpleConfigRequest,
    prefer_ipv6: bool,
    get_config: impl FnOnce(&HttpQuery) -> Result<String, Status>,
) -> Result<SimpleConfigResult, Status> {
    tdutils::log_info!("Request simple config from {}", request.url);
    let mut headers = request.headers;
    headers.push(("Host".to_string(), request.host.to_string()));
    headers.push((
        "User-Agent".to_string(),
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) \
         Chrome/77.0.3865.90 Safari/537.36"
            .to_string(),
    ));
    let params = WgetParams {
        headers,
        timeout_in: std::time::Duration::from_secs(10),
        ttl: 3,
        prefer_ipv6,
        verify_peer: VerifyPeer::Off,
        content: request.content,
        content_type: request.content_type,
        max_body_size: None,
    };
    let http_query = into_std(wget(&request.url, params).await)?;

    let r_http_date = into_std(parse_http_date(http_query.get_header("date").unwrap_or("")));
    let r_config = match get_config(&http_query) {
        Ok(raw_config) => decode_config(&raw_config),
        Err(error) => Err(error),
    };
    Ok(SimpleConfigResult {
        r_config,
        r_http_date,
    })
}

/// `get_simple_config_azure` (ConfigManager.cpp:151-160); the response body
/// is the blob itself.
pub async fn get_simple_config_azure(
    prefer_ipv6: bool,
    _domain_name: &str,
    is_test: bool,
) -> Result<SimpleConfigResult, Status> {
    get_simple_config_impl(azure_request(is_test), prefer_ipv6, |http_query| {
        Ok(String::from_utf8_lossy(&http_query.body).into_owned())
    })
    .await
}

/// `get_simple_config_google_dns` (ConfigManager.cpp:224-229).
pub async fn get_simple_config_google_dns(
    prefer_ipv6: bool,
    domain_name: &str,
    is_test: bool,
) -> Result<SimpleConfigResult, Status> {
    get_simple_config_impl(
        dns_request("dns.google/resolve", "dns.google", domain_name, is_test),
        prefer_ipv6,
        get_dns_config,
    )
    .await
}

/// `get_simple_config_mozilla_dns` (ConfigManager.cpp:231-236).
pub async fn get_simple_config_mozilla_dns(
    prefer_ipv6: bool,
    domain_name: &str,
    is_test: bool,
) -> Result<SimpleConfigResult, Status> {
    get_simple_config_impl(
        dns_request(
            "mozilla.cloudflare-dns.com/dns-query",
            "mozilla.cloudflare-dns.com",
            domain_name,
            is_test,
        ),
        prefer_ipv6,
        get_dns_config,
    )
    .await
}

/// `get_simple_config_firebase_realtime` (ConfigManager.cpp:322-337); test
/// configs are rejected before the fetch, like the C++ `set_error`.
pub async fn get_simple_config_firebase_realtime(
    prefer_ipv6: bool,
    _domain_name: &str,
    is_test: bool,
) -> Result<SimpleConfigResult, Status> {
    if is_test {
        return Err(Status::error(400, "Test config is not supported"));
    }
    get_simple_config_impl(
        firebase_realtime_request(),
        prefer_ipv6,
        get_firebase_realtime_config,
    )
    .await
}

/// `get_simple_config_firebase_firestore` (ConfigManager.cpp:339-360).
pub async fn get_simple_config_firebase_firestore(
    prefer_ipv6: bool,
    _domain_name: &str,
    is_test: bool,
) -> Result<SimpleConfigResult, Status> {
    if is_test {
        return Err(Status::error(400, "Test config is not supported"));
    }
    get_simple_config_impl(
        firebase_firestore_request(),
        prefer_ipv6,
        get_firebase_firestore_config,
    )
    .await
}

/// The `td::Result<T>` → `std::result::Result<T, Status>` conversion at the
/// tdutils call boundaries.
fn into_std<T>(result: tdutils::status::Result<T>) -> Result<T, Status> {
    match result {
        tdutils::status::Result::Ok(value) => Ok(value),
        tdutils::status::Result::Err(status) => Err(status),
    }
}

// ---- The ConfigRecoverer state (ConfigManager.cpp:499-832, M.3.7.1) ------
//
// The C++ actor as an inline state machine on the Td actor, like the
// ConfigManager port. M.3.7.1 carries the state fields and the config
// application; the loop()/state callbacks (M.3.7.2), the fetch
// orchestration (M.3.7.3), the full-config mini-session (M.3.7.4) and the
// Td wiring (M.3.7.5) land on top.

// ---- DcOption (td/telegram/net/DcOptions.h) -------------------------------

/// `DcId::MAX_RAW_DC_ID` (net/DcId.h:20).
pub const MAX_RECOVERER_DC_ID: i32 = 1000;

/// The C++ `DcOption(DcId, IpPort)` (net/DcOptions.h:143-172) — the option
/// flavor the recoverer keeps: a raw dc id with one IPv4 endpoint and the
/// optional `ipPortSecret`. The always-set `ObfuscatedTcpOnly` flag and the
/// absent `IPv6` one are implied (the simple config carries IPv4 only), so
/// they are not stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecovererDcOption {
    /// `dc_id_` — the raw id.
    pub raw_dc_id: i32,
    /// `ip_address_` — the dotted-quad address.
    pub ip_address: String,
    /// The port.
    pub port: i32,
    /// `secret_` — the `ipPortSecret` secret.
    pub secret: Vec<u8>,
}

/// `IPAddress::ipv4_to_str` — the big-endian dotted quad.
fn ipv4_to_string(ipv4: i32) -> String {
    let ipv4 = ipv4 as u32;
    format!(
        "{}.{}.{}.{}",
        ipv4 >> 24,
        (ipv4 >> 16) & 0xff,
        (ipv4 >> 8) & 0xff,
        ipv4 & 0xff
    )
}

impl RecovererDcOption {
    /// The constructor + the `is_valid` filter (`ip_address_.is_valid() &&
    /// dc_id_.is_exact()`): `None` is the invalid C++ option (a dc id
    /// outside 1..=1000, or an `ipPortSecret` whose secret
    /// `ProxySecret::from_binary` rejects — the C++ constructor then leaves
    /// the address unparsed). Invalid options are dropped at the call
    /// sites, like the C++ `if (option.is_valid())`.
    pub fn from_ip_port(dc_id: i32, ip_port: &telegram_api::IpPort) -> Option<Self> {
        if !(1..=MAX_RECOVERER_DC_ID).contains(&dc_id) {
            return None;
        }
        let (ip_address, port, secret) = match ip_port {
            telegram_api::IpPort::ipPort(ip_port) => {
                (ipv4_to_string(ip_port.ipv4), ip_port.port, Vec::new())
            }
            telegram_api::IpPort::ipPortSecret(ip_port) => {
                if ProxySecret::from_binary(&ip_port.secret, false).is_error() {
                    return None;
                }
                (
                    ipv4_to_string(ip_port.ipv4),
                    ip_port.port,
                    ip_port.secret.clone(),
                )
            }
        };
        Some(Self {
            raw_dc_id: dc_id,
            ip_address,
            port,
            secret,
        })
    }

    /// The replayed `dc_options_update` options (the full `DcOption` flavor
    /// stored by the ConfigManager) narrowed to the recoverer's subset.
    pub fn from_stored(option: &StoredDcOption) -> Self {
        Self {
            raw_dc_id: option.raw_dc_id(),
            ip_address: option.ip_address().to_string(),
            port: option.port(),
            secret: option.secret().to_vec(),
        }
    }

    /// `DcOption::is_valid` — the constructor guarantees the dotted-quad
    /// form, so only the dc id range and the address presence are left to
    /// check.
    pub fn is_valid(&self) -> bool {
        (1..=MAX_RECOVERER_DC_ID).contains(&self.raw_dc_id) && !self.ip_address.is_empty()
    }

    /// `get_ip_address().ip_port()` — the `ip:port` connect string.
    pub fn ip_address_port(&self) -> String {
        format!("{}:{}", self.ip_address, self.port)
    }
}

/// `ConfigManager::load_dc_options_update` (ConfigManager.cpp:1150-1157),
/// yielding the recoverer's option subset.
pub fn load_dc_options_update(pmc: &BinlogKeyValue) -> Vec<RecovererDcOption> {
    let raw = pmc.get_raw(b"dc_options_update");
    if raw.is_empty() {
        return Vec::new();
    }
    match StoredDcOptions::parse(&raw) {
        Ok(stored) => stored
            .options()
            .iter()
            .map(RecovererDcOption::from_stored)
            .collect(),
        Err(error) => {
            tracing::error!("Failed to parse dc_options_update: {error}");
            Vec::new()
        }
    }
}

/// The `G()` reads the config application makes per step, gathered by the
/// driver (the C++ recoverer reads them through the global directly). The
/// fetch-argument reads (`is_test_dc`, `prefer_ipv6`,
/// `dc_txt_domain_name`) land with the fetch orchestration (M.3.7.3).
#[derive(Clone, Debug)]
pub struct RecovererEnv {
    /// `G()->unix_time()`.
    pub unix_time: i32,
    /// `G()->get_option_string("my_phone_number")`.
    pub my_phone_number: String,
    /// `expect_blocking()` — `G()->get_option_boolean("expect_blocking", true)`,
    /// resolved by the driver.
    pub expect_blocking: bool,
}

/// What one recoverer step asks its driver to do — the C++ side effects the
/// inline recoverer cannot take itself (`G()` writes, actor creation, the
/// timer). M.3.7.1 carries only the ConnectionCreator fan-out; the fetch
/// and wakeup actions land with the loop (M.3.7.2/M.3.7.3).
#[derive(Debug, Default)]
pub struct RecovererActions {
    /// `ConnectionCreator::on_dc_options(DcOptions(full_config->dc_options_))`
    /// after a full config (the fan-out lands with the net step; the
    /// options are carried for it).
    pub connection_creator_dc_options: Vec<telegram_api::DcOption>,
}

/// The `ConfigRecoverer` (ConfigManager.cpp:499-832) as an inline state
/// machine on the Td actor, like the ConfigManager port. The C++ actor's
/// `ref_cnt_`/promise plumbing is the driver's job here; the fetch results
/// come back through [`ConfigRecoverer::on_full_config`].
pub struct ConfigRecoverer {
    /// `is_online_`.
    is_online: bool,

    /// `simple_config_` — the options of the last simple config.
    simple_config: Vec<RecovererDcOption>,
    /// `simple_config_expires_at_`.
    #[allow(dead_code)] // read by the loop (M.3.7.2)
    simple_config_expires_at: f64,
    /// `simple_config_at_` (only logged in the C++).
    simple_config_at: f64,

    /// `dc_options_update_`.
    dc_options_update: Vec<RecovererDcOption>,

    /// `dc_options_` — `dc_options_update_` + `simple_config_`.
    dc_options: Vec<RecovererDcOption>,
    /// `dc_options_at_`.
    #[allow(dead_code)] // read by the loop (M.3.7.2)
    dc_options_at: f64,
    /// `dc_options_i_`.
    #[allow(dead_code)] // read by the loop (M.3.7.2)
    dc_options_i: usize,

    /// `full_config_`.
    #[allow(dead_code)] // read by on_online/loop (M.3.7.2)
    full_config: Option<Box<telegram_api::Config>>,
    /// `full_config_expires_at_`.
    #[allow(dead_code)] // read by the loop (M.3.7.2)
    full_config_expires_at: f64,
    /// `!full_config_query_.empty()`.
    #[allow(dead_code)] // read by the loop (M.3.7.2)
    full_config_query_active: bool,
}

impl Default for ConfigRecoverer {
    fn default() -> Self {
        Self::new()
    }
}

impl ConfigRecoverer {
    /// The constructor — the C++ seeds `connecting_since_ = Time::now()`;
    /// the connecting/network state lands with the callbacks (M.3.7.2).
    pub fn new() -> Self {
        Self {
            is_online: false,
            simple_config: Vec::new(),
            simple_config_expires_at: 0.0,
            simple_config_at: 0.0,
            dc_options_update: Vec::new(),
            dc_options: Vec::new(),
            dc_options_at: 0.0,
            dc_options_i: 0,
            full_config: None,
            full_config_expires_at: 0.0,
            full_config_query_active: false,
        }
    }

    /// `simple_config_at_` (only written by the C++ too).
    pub fn simple_config_at(&self) -> f64 {
        self.simple_config_at
    }

    /// `dc_options_` — the merged set (the update part first, then the
    /// simple config).
    pub fn dc_options(&self) -> &[RecovererDcOption] {
        &self.dc_options
    }

    /// `on_dc_options_update` (ConfigManager.cpp:505-509) — the C++ tail
    /// re-runs the loop; that call lands with M.3.7.2.
    pub fn on_dc_options_update(&mut self, dc_options: Vec<RecovererDcOption>) {
        self.dc_options_update = dc_options;
        self.update_dc_options();
    }

    /// `check_phone_number_rules` (ConfigManager.cpp:546-564).
    #[allow(dead_code)] // the non-test callers land with on_simple_config (M.3.7.3)
    fn check_phone_number_rules(phone_number: &str, rules: &str) -> bool {
        if rules.is_empty() || phone_number.is_empty() {
            return true;
        }
        let mut found = false;
        for prefix in rules.split(',') {
            if prefix.is_empty() {
                found = true;
            } else if let Some(positive) = prefix.strip_prefix('+') {
                if phone_number.starts_with(positive) {
                    found = true;
                }
            } else if let Some(negative) = prefix.strip_prefix('-') {
                if phone_number.starts_with(negative) {
                    return false;
                }
            } else {
                tracing::error!("Invalid prefix rule {prefix}");
            }
        }
        found
    }

    /// `do_on_simple_config` (ConfigManager.cpp:590-624). The actions stay
    /// empty here; `on_simple_config` (M.3.7.3) merges its dns-time
    /// difference action with this result.
    #[allow(dead_code)] // the first non-test caller is on_simple_config (M.3.7.3)
    fn do_on_simple_config(
        &mut self,
        r_simple_config: Result<telegram_api::help_ConfigSimple, Status>,
        env: &RecovererEnv,
    ) -> RecovererActions {
        match r_simple_config {
            Ok(config) => {
                tracing::trace!("Receive raw {config:?}");
                if config.expires >= env.unix_time {
                    let phone_number = &env.my_phone_number;
                    self.simple_config.clear();
                    for rule in &config.rules {
                        if Self::check_phone_number_rules(phone_number, &rule.phone_prefix_rules)
                            && (1..=MAX_RECOVERER_DC_ID).contains(&rule.dc_id)
                        {
                            for ip_port in &rule.ips {
                                if let Some(option) =
                                    RecovererDcOption::from_ip_port(rule.dc_id, ip_port)
                                {
                                    self.simple_config.push(option);
                                }
                            }
                        }
                    }
                    tracing::trace!("Receive SimpleConfig {:?}", self.simple_config);
                } else {
                    tracing::trace!("Config has expired at {}", config.expires);
                }

                self.simple_config_expires_at = self.get_config_expire_time(env);
                self.simple_config_at = tdutils::time::now_cached();
                // The C++ Fisher-Yates: `swap(i, Random::fast(0, i))` for
                // `i` in 1..n.
                for i in 1..self.simple_config.len() {
                    let j = tdutils::random::fast(0, i as i32) as usize;
                    self.simple_config.swap(i, j);
                }
            }
            Err(error) => {
                tracing::trace!("Get SimpleConfig error {error}");
                self.simple_config.clear();
                self.simple_config_expires_at = self.get_failed_config_expire_time(env);
            }
        }
        RecovererActions::default()
    }

    /// `on_full_config` (ConfigManager.cpp:626-639) — the ConnectionCreator
    /// fan-out is carried as the action (the `send_closure` itself lands
    /// with the net step, M.3.9.1); the C++ `loop()` tail lands with
    /// M.3.7.2.
    pub fn on_full_config(
        &mut self,
        r_full_config: Result<telegram_api::Config, Status>,
        env: &RecovererEnv,
    ) -> RecovererActions {
        self.full_config_query_active = false;
        let mut actions = RecovererActions::default();
        match r_full_config {
            Ok(config) => {
                tracing::trace!("Receive {:?}", config.dc_options);
                self.full_config_expires_at = self.get_config_expire_time(env);
                actions.connection_creator_dc_options = config.dc_options.clone();
                self.full_config = Some(Box::new(config));
            }
            Err(error) => {
                tracing::trace!("Failed to get config: {error}");
                self.full_config = None;
                self.full_config_expires_at = self.get_failed_config_expire_time(env);
            }
        }
        actions
    }

    /// `expect_blocking` (ConfigManager.cpp:641-643) — resolved into the
    /// env by the driver.
    fn expect_blocking(env: &RecovererEnv) -> bool {
        env.expect_blocking
    }

    /// `get_config_expire_time` (ConfigManager.cpp:645-649).
    fn get_config_expire_time(&self, env: &RecovererEnv) -> f64 {
        let offline_delay = if self.is_online { 0.0 } else { 5.0 * 60.0 };
        let expire_time = if Self::expect_blocking(env) {
            tdutils::random::fast(2 * 60, 3 * 60)
        } else {
            tdutils::random::fast(20 * 60, 30 * 60)
        };
        tdutils::time::now() + offline_delay + f64::from(expire_time)
    }

    /// `get_failed_config_expire_time` (ConfigManager.cpp:651-655).
    fn get_failed_config_expire_time(&self, env: &RecovererEnv) -> f64 {
        let offline_delay = if self.is_online { 0.0 } else { 5.0 * 60.0 };
        let expire_time = if Self::expect_blocking(env) {
            tdutils::random::fast(5, 7)
        } else {
            tdutils::random::fast(15, 30)
        };
        tdutils::time::now() + offline_delay + f64::from(expire_time)
    }

    /// `update_dc_options` (ConfigManager.cpp:823-832) — the saved update
    /// first, then the simple config; any change resets the rotation.
    fn update_dc_options(&mut self) {
        let mut new_dc_options = self.dc_options_update.clone();
        new_dc_options.extend(self.simple_config.iter().cloned());
        if new_dc_options != self.dc_options {
            self.dc_options = new_dc_options;
            self.dc_options_i = 0;
            self.dc_options_at = tdutils::time::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real simple-config blob, recorded 2026-09-15 from
    /// `https://dns.google/resolve?name=apv3.stel.com&type=TXT` — the two
    /// TXT `data` parts concatenated bigger-first, exactly as
    /// [`get_dns_config`] assembles them.
    const RECORDED_DNS_BLOB: &str = "oiVvQMgmCSOQvenEo/Ug1a+IzF4Q/13++Cq2zwvy0z75GVRblmruH6SBuEaPsfEzpAdJw/1QoMphLuQx9bzCU2LgCMLNZ+4awO+Is8p4k0r4FgerDwWBFabdMBUl2sZMOKI4c4soNa6LXal7q7kuVTZjFNSC6p9dcYTYtG1GnQPf5W7kD6hPZDr1PF5+Wga3yp6hErN0iff2BMxmmmxAAbWOQ2TbaDlY36ViLkf9RUXcxAABU4Vg9d9mW035arl4mB8+LFXX7F4rGWpyGpsclPI6jq4L7OvFpmAAJtUp7pD8BSBtCJa7rgojCjkjnYxGPQA/3MHeBCrT22Y5V+Y0KA==";

    /// The raw recorded `Answer` array of the DNS transcript above.
    const RECORDED_DNS_ANSWER: &str = "[{\"name\":\"apv3.stel.com.\",\"type\":16,\"TTL\":60,\"data\":\"oiVvQMgmCSOQvenEo/Ug1a+IzF4Q/13++Cq2zwvy0z75GVRblmruH6SBuEaPsfEzpAdJw/1QoMphLuQx9bzCU2LgCMLNZ+4awO+Is8p4k0r4FgerDwWBFabdMBUl2sZMOKI4c4soNa6LXal7q7kuVTZjFNSC6p9dcYTYtG1GnQPf5W7kD6hPZDr1PF5+Wga3yp6hErN0iff2BMxmmmxAAbWOQ2TbaDlY36ViLkf9RUXcxAABU4Vg9d9mW0\"},{\"name\":\"apv3.stel.com.\",\"type\":16,\"TTL\":60,\"data\":\"35arl4mB8+LFXX7F4rGWpyGpsclPI6jq4L7OvFpmAAJtUp7pD8BSBtCJa7rgojCjkjnYxGPQA/3MHeBCrT22Y5V+Y0KA==\"}]";

    /// A real firebase-realtime transcript, recorded 2026-09-15 from
    /// `https://reserve-5a846.firebaseio.com/ipconfigv3.json` — the whole
    /// response is a bare JSON string wrapping an independently signed
    /// second blob.
    const RECORDED_REALTIME_BODY: &str = "\"SIiJpKPAUg/1tqjnFQSFH78tMOqkYxeiiKkY6B4Ao1hBDp/UbDy9Zda2azORL1ibt7mRfy6E8wPekcSqWvTHDPnaYk7ec9VkEWgSDBYUadtAZAI7mvAaV3rDdUS63lMcteZf7BOVYX56GgXO3CSLmMx0P0CRybYwRwERDgQ5tY+QuPyTXPn5Xm+FoeqjH3niDmPi+8rOmw6Q3paCYJNtGr9+LyspCvWdL0Pto5g0XBLTpMMkj7nwB9bttrMrLbKu0ToOIOwlYd8VkJVypR6KSLhCzs0tevQONL8FB+npIQCr1HdDukoLREofAtUsGc2kaliZ2ORDeur24emaUaNI6g==\"";

    /// The firebase-firestore response shape
    /// (`projects/.../documents/ipconfig/v3`); the live endpoint answers
    /// 404 since 2026, so the transcript is built after the C++ extraction
    /// path and Google's Firestore REST `documents` shape.
    fn firestore_body(blob: &str) -> String {
        format!("{{\"fields\":{{\"data\":{{\"stringValue\":\"{blob}\"}}}}}}")
    }

    #[test]
    fn decodes_recorded_blobs_byte_exact() {
        // The recorded DNS blob decodes to a real help.configSimple.
        let config = match decode_config(RECORDED_DNS_BLOB) {
            Ok(config) => config,
            Err(status) => panic!("the recorded blob must decode: {status}"),
        };
        assert!(config.date > 0);
        assert!(config.expires > 0);
        assert!(!config.rules.is_empty());

        // The firebase-realtime transcript carries an independently signed
        // second blob.
        let realtime_blob = RECORDED_REALTIME_BODY
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .expect("a quoted blob");
        match decode_config(realtime_blob) {
            Ok(config) => {
                assert!(config.date > 0);
                assert!(!config.rules.is_empty());
            }
            Err(status) => panic!("the realtime blob must decode: {status}"),
        }
    }

    #[test]
    fn decodes_through_base64_filter_noise() {
        // Newlines are dropped by `base64_filter`, so a line-wrapped blob
        // decodes the same.
        let mut wrapped = String::new();
        for (i, c) in RECORDED_DNS_BLOB.chars().enumerate() {
            if i > 0 && i % 76 == 0 {
                wrapped.push('\n');
            }
            wrapped.push(c);
        }
        assert!(decode_config(&wrapped).is_ok());
    }

    #[test]
    fn rejects_broken_blobs() {
        // Too short / too long.
        let error = decode_config(&RECORDED_DNS_BLOB[..343]).unwrap_err();
        assert_eq!(error.message(), "Invalid [length:343]");
        assert_eq!(
            decode_config(&"A".repeat(1025)).unwrap_err().message(),
            "Invalid [length:1025]"
        );

        // A non-base64 character insertion shrinks the filtered blob.
        let noisy = format!("{RECORDED_DNS_BLOB}!{RECORDED_DNS_BLOB}");
        assert_eq!(
            decode_config(&noisy).unwrap_err().message(),
            "Invalid [length:688] after base64_filter"
        );

        // A tampered middle character survives the length checks but fails
        // the RSA/AES/SHA chain.
        let mut tampered = RECORDED_DNS_BLOB.to_string();
        tampered.replace_range(170..171, "A");
        assert!(decode_config(&tampered).is_err());

        // A tampered tail too (the SHA-256 truncation check).
        let mut tampered_tail = RECORDED_DNS_BLOB.to_string();
        tampered_tail.replace_range(340..344, "AAAA");
        assert!(decode_config(&tampered_tail).is_err());
    }

    #[test]
    fn pins_fetcher_request_parameters() {
        // azure: the URL flips with is_test, the Host is pinned.
        let prod = azure_request(false);
        assert_eq!(
            prod.url,
            "https://software-download.microsoft.com/prodv2/config.txt"
        );
        assert_eq!(
            azure_request(true).url,
            "https://software-download.microsoft.com/testv2/config.txt"
        );
        assert_eq!(prod.host, "tcdnb.azureedge.net");
        assert!(prod.headers.is_empty());

        // DNS: the default TXT domains, the pinned Accept header and the
        // url-encoded name parameter.
        let google = dns_request("dns.google/resolve", "dns.google", "", false);
        assert_eq!(
            google.url,
            "https://dns.google/resolve?name=apv3.stel.com&type=TXT"
        );
        assert_eq!(google.host, "dns.google");
        let mozilla = dns_request(
            "mozilla.cloudflare-dns.com/dns-query",
            "mozilla.cloudflare-dns.com",
            "",
            true,
        );
        assert_eq!(
            mozilla.url,
            "https://mozilla.cloudflare-dns.com/dns-query?name=tapv3.stel.com&type=TXT"
        );
        assert_eq!(
            mozilla.headers,
            vec![("Accept".to_string(), "application/dns-json".to_string())]
        );
        // A caller-supplied domain wins over the defaults and is
        // url-encoded.
        let custom = dns_request("dns.google/resolve", "dns.google", "apv3 stel.com", false);
        assert_eq!(
            custom.url,
            "https://dns.google/resolve?name=apv3%20stel.com&type=TXT"
        );

        // Firebase realtime/firestore.
        let realtime = firebase_realtime_request();
        assert_eq!(
            realtime.url,
            "https://reserve-5a846.firebaseio.com/ipconfigv3.json"
        );
        assert_eq!(realtime.host, "reserve-5a846.firebaseio.com");
        let firestore = firebase_firestore_request();
        assert_eq!(
            firestore.url,
            "https://www.google.com/v1/projects/reserve-5a846/databases/(default)/documents/ipconfig/v3"
        );
        assert_eq!(firestore.host, "firestore.googleapis.com");
    }

    fn http_query_with_args(args: Vec<(String, String)>, body: &str) -> HttpQuery {
        HttpQuery {
            code: 200,
            reason: "OK".to_string(),
            headers: Vec::new(),
            body: body.as_bytes().to_vec(),
            args,
        }
    }

    #[test]
    fn extracts_dns_config_from_recorded_transcript() {
        // The recorded transcript, as `parse_json_parameters` splits it:
        // the whole `Answer` array as one arg.
        let query = http_query_with_args(
            vec![("Answer".to_string(), RECORDED_DNS_ANSWER.to_string())],
            "",
        );
        assert_eq!(
            get_dns_config(&query).expect("two parts"),
            RECORDED_DNS_BLOB
        );

        // The fallback path: the JSON object arrives as the body (a
        // non-JSON-typed response).
        let query =
            http_query_with_args(Vec::new(), &format!("{{\"Answer\":{RECORDED_DNS_ANSWER}}}"));
        assert_eq!(
            get_dns_config(&query).expect("two parts"),
            RECORDED_DNS_BLOB
        );
    }

    #[test]
    fn dns_config_errors_match_the_cpp() {
        // One part only.
        let query = http_query_with_args(
            vec![("Answer".to_string(), "[{\"data\":\"oiVv\"}]".to_string())],
            "",
        );
        assert_eq!(
            get_dns_config(&query).unwrap_err().message(),
            "Expected data in two parts"
        );

        // A non-object array element.
        let query = http_query_with_args(vec![("Answer".to_string(), "[1, 2]".to_string())], "");
        assert_eq!(
            get_dns_config(&query).unwrap_err().message(),
            "Expected JSON object"
        );

        // A non-array Answer arg.
        let query = http_query_with_args(vec![("Answer".to_string(), "{}".to_string())], "");
        assert_eq!(
            get_dns_config(&query).unwrap_err().message(),
            "Expected JSON array"
        );

        // A non-object body on the fallback path.
        let query = http_query_with_args(Vec::new(), "[]");
        assert_eq!(
            get_dns_config(&query).unwrap_err().message(),
            "Expected JSON object"
        );

        // Missing Answer in the body.
        let query = http_query_with_args(Vec::new(), "{\"Status\":0}");
        assert!(get_dns_config(&query).is_err());
    }

    #[test]
    fn extracts_firebase_realtime_from_recorded_transcript() {
        // The bare-string body lands under the "content" arg
        // (`parse_json_parameters` decodes the string, quotes dropped).
        let content = RECORDED_REALTIME_BODY
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .expect("a quoted blob");
        let query = http_query_with_args(vec![("content".to_string(), content.to_string())], "");
        let blob = get_firebase_realtime_config(&query).expect("content arg");
        assert_eq!(blob.len(), 344);
        assert!(decode_config(&blob).is_ok());
    }

    #[test]
    fn extracts_firebase_firestore_from_transcript() {
        // The response body goes through the real
        // `HttpQuery::parse_json_parameters`, then the extractor.
        let mut query = http_query_with_args(Vec::new(), &firestore_body(RECORDED_DNS_BLOB));
        match query.parse_json_parameters() {
            tdutils::status::Result::Ok(()) => {}
            tdutils::status::Result::Err(status) => {
                panic!("fields object parses: {status}")
            }
        }
        assert_eq!(
            get_firebase_firestore_config(&query).expect("stringValue"),
            RECORDED_DNS_BLOB
        );

        // A missing data field fails.
        let mut query = http_query_with_args(Vec::new(), "{\"fields\":{}}");
        match query.parse_json_parameters() {
            tdutils::status::Result::Ok(()) => {}
            tdutils::status::Result::Err(status) => {
                panic!("fields object parses: {status}")
            }
        }
        assert!(get_firebase_firestore_config(&query).is_err());
    }

    /// The shared `Host`/`User-Agent` headers and the extraction+decode
    /// chain, exercised over the real Wget against an in-process server
    /// serving the recorded azure-style `text/plain` transcript.
    #[tokio::test]
    async fn fetches_simple_config_over_http() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (request_tx, request_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buffer = vec![0u8; 65536];
            let read = socket.read(&mut buffer).await.expect("read");
            request_tx.send(buffer[..read].to_vec()).ok();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\
                 Date: Tue, 15 Sep 2026 12:34:56 GMT\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n",
                RECORDED_DNS_BLOB.len()
            );
            socket.write_all(response.as_bytes()).await.ok();
            socket.write_all(RECORDED_DNS_BLOB.as_bytes()).await.ok();
            socket.shutdown().await.ok();
        });

        let mut request = azure_request(false);
        request.url = format!("http://127.0.0.1:{port}/prodv2/config.txt");
        let result = get_simple_config_impl(request, false, |http_query| {
            Ok(String::from_utf8_lossy(&http_query.body).into_owned())
        })
        .await
        .expect("the fetch succeeds");

        // The decoded config equals a direct decode of the recorded blob,
        // and the date header is parsed.
        let reference = decode_config(RECORDED_DNS_BLOB).expect("the blob decodes");
        match result.r_config {
            Ok(config) => {
                assert_eq!(config.date, reference.date);
                assert_eq!(config.expires, reference.expires);
                assert_eq!(config.rules.len(), reference.rules.len());
            }
            Err(status) => panic!("the fetched blob must decode: {status}"),
        }
        match result.r_http_date {
            Ok(date) => assert_eq!(date, 1789475696),
            Err(status) => panic!("the date header must parse: {status}"),
        }

        server.await.expect("server joins");

        // The server saw the pinned Host/User-Agent headers.
        let head = String::from_utf8_lossy(&request_rx.try_recv().expect("request")).to_lowercase();
        assert!(
            head.contains("host: tcdnb.azureedge.net"),
            "the pinned Host header must be sent: {head}"
        );
        assert!(
            head.contains("chrome/77.0.3865.90"),
            "the pinned User-Agent must be sent: {head}"
        );
    }

    /// A JSON-typed response body is split into args by the Wget layer
    /// (`parse_json_parameters`), so the DNS extraction works over the real
    /// fetch path.
    #[tokio::test]
    async fn fetches_dns_config_over_http() {
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let body = format!("{{\"Answer\":{RECORDED_DNS_ANSWER}}}");
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.ok();
            socket.shutdown().await.ok();
        });

        let mut request = dns_request("dns.google/resolve", "dns.google", "", false);
        request.url = format!("http://127.0.0.1:{port}/resolve?name=apv3.stel.com&type=TXT");
        let result = get_simple_config_impl(request, false, get_dns_config)
            .await
            .expect("the fetch succeeds");

        match result.r_config {
            Ok(config) => assert!(!config.rules.is_empty()),
            Err(status) => panic!("the DNS config must decode: {status}"),
        }
        // No date header → the C++ `parse_http_date("")` error.
        assert!(result.r_http_date.is_err());

        server.await.expect("server joins");
    }

    /// A transport-level failure is the outer error (the C++ promise
    /// error), not an inner `r_config` one; the firebase test-config
    /// rejection happens before any fetch.
    #[tokio::test]
    async fn fetch_errors_are_outer() {
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            socket
                .write_all(
                    b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\
                      Connection: close\r\n\r\n",
                )
                .await
                .ok();
            socket.shutdown().await.ok();
        });

        let mut request = firebase_realtime_request();
        request.url = format!("http://127.0.0.1:{port}/ipconfigv3.json");
        let error = get_simple_config_impl(request, false, get_firebase_realtime_config)
            .await
            .expect_err("a 500 fails the fetch");
        assert_eq!(error.message(), "HTTP error: 500");

        server.await.expect("server joins");

        let error = get_simple_config_firebase_realtime(false, "", true)
            .await
            .expect_err("test configs are rejected");
        assert_eq!(error.code(), 400);
        assert_eq!(error.message(), "Test config is not supported");
        let error = get_simple_config_firebase_firestore(false, "", true)
            .await
            .expect_err("test configs are rejected");
        assert_eq!(error.code(), 400);
    }

    // ==== the recoverer state (M.3.7.1) ====

    use tddb::db_key::DbKey;

    /// A writable pmc over a temporary binlog file (the config_manager test
    /// pattern): `BinlogKeyValue` panics on writes until a binlog is
    /// attached.
    struct RecovererPmc {
        pmc: BinlogKeyValue,
        path: String,
    }

    impl RecovererPmc {
        fn new(name: &str) -> RecovererPmc {
            let path = std::env::temp_dir()
                .join(format!(
                    "td-rs-recoverer-{name}-{}.binlog",
                    std::process::id()
                ))
                .to_string_lossy()
                .into_owned();
            let _ = BinlogKeyValue::destroy(&path);
            let mut pmc = BinlogKeyValue::new();
            pmc.init(&path, DbKey::empty()).ensure();
            RecovererPmc { pmc, path }
        }
    }

    impl std::ops::Deref for RecovererPmc {
        type Target = BinlogKeyValue;

        fn deref(&self) -> &BinlogKeyValue {
            &self.pmc
        }
    }

    impl std::ops::DerefMut for RecovererPmc {
        fn deref_mut(&mut self) -> &mut BinlogKeyValue {
            &mut self.pmc
        }
    }

    impl Drop for RecovererPmc {
        fn drop(&mut self) {
            self.pmc.close();
            std::fs::remove_file(&self.path).ok();
        }
    }

    #[test]
    fn phone_number_rules_match_the_cpp() {
        // Empty rules or an empty number accept everything.
        assert!(ConfigRecoverer::check_phone_number_rules("", ""));
        assert!(ConfigRecoverer::check_phone_number_rules("123", ""));
        assert!(ConfigRecoverer::check_phone_number_rules("", "+999"));

        // A positive prefix list: the number must begin with one of them.
        assert!(ConfigRecoverer::check_phone_number_rules(
            "999123",
            "+999,+888"
        ));
        assert!(!ConfigRecoverer::check_phone_number_rules(
            "777123",
            "+999,+888"
        ));

        // An empty comma part matches everything.
        assert!(ConfigRecoverer::check_phone_number_rules("777123", "+999,"));

        // A negative prefix rejects immediately, even with a positive match.
        assert!(!ConfigRecoverer::check_phone_number_rules(
            "999123",
            "+999,-999123"
        ));

        // Invalid (unprefixed) rules are logged and skipped.
        assert!(!ConfigRecoverer::check_phone_number_rules(
            "999123", "999,+888"
        ));
        assert!(ConfigRecoverer::check_phone_number_rules(
            "888123", "999,+888"
        ));
    }

    fn ip_port(ipv4: i32, port: i32) -> telegram_api::IpPort {
        telegram_api::IpPort::ipPort(telegram_api::ipPort { ipv4, port })
    }

    #[test]
    fn recoverer_dc_options_come_from_ip_ports() {
        let option = RecovererDcOption::from_ip_port(2, &ip_port(0x0102_0304, 443)).expect("valid");
        assert_eq!(option.ip_address, "1.2.3.4");
        assert_eq!(option.ip_address_port(), "1.2.3.4:443");
        assert_eq!(option.raw_dc_id, 2);
        assert!(option.secret.is_empty());
        assert!(option.is_valid());

        // A dc id outside 1..=1000 is the invalid C++ option.
        assert!(RecovererDcOption::from_ip_port(0, &ip_port(1, 443)).is_none());
        assert!(RecovererDcOption::from_ip_port(1001, &ip_port(1, 443)).is_none());

        // An unusable ipPortSecret is invalid, a usable one carries the
        // secret.
        let bad = telegram_api::IpPort::ipPortSecret(telegram_api::ipPortSecret {
            ipv4: 0x0102_0304,
            port: 8888,
            secret: Vec::new(),
        });
        assert!(
            RecovererDcOption::from_ip_port(2, &bad).is_none(),
            "an empty secret makes the option invalid"
        );
        let good = telegram_api::IpPort::ipPortSecret(telegram_api::ipPortSecret {
            ipv4: 0x0102_0304,
            port: 8888,
            secret: vec![0xdd; 16],
        });
        let option = RecovererDcOption::from_ip_port(2, &good).expect("valid");
        assert_eq!(option.secret, vec![0xdd; 16]);
        assert_eq!(option.port, 8888);
    }

    /// A `help.configSimple` with one `AccessPointRule` and one endpoint.
    fn simple_config_with_rules(
        phone_prefix_rules: &str,
        dc_id: i32,
        ip_port: telegram_api::IpPort,
    ) -> telegram_api::help_ConfigSimple {
        telegram_api::help_ConfigSimple {
            date: 99_000,
            expires: 200_000,
            rules: vec![telegram_api::AccessPointRule {
                phone_prefix_rules: phone_prefix_rules.to_string(),
                dc_id,
                ips: vec![ip_port],
            }],
        }
    }

    fn env(expect_blocking: bool) -> RecovererEnv {
        RecovererEnv {
            unix_time: 100_000,
            // The option holds the digits-only phone (the Telegram API
            // form, no leading '+'), so the `+999` rules match it.
            my_phone_number: "999555123".to_string(),
            expect_blocking,
        }
    }

    #[test]
    fn applies_a_good_simple_config() {
        let mut recoverer = ConfigRecoverer::new();
        let environment = env(true);
        recoverer.is_online = true;

        // A rule for +999 with two endpoints, a +888 rule, and an
        // out-of-range dc id: only the options of the matching rules
        // survive, then shuffled (checked as a permutation).
        let config = telegram_api::help_ConfigSimple {
            date: 99_000,
            expires: 200_000,
            rules: vec![
                telegram_api::AccessPointRule {
                    phone_prefix_rules: "+999".to_string(),
                    dc_id: 2,
                    ips: vec![ip_port(0x0909_0909, 443), ip_port(0x0909_090a, 5222)],
                },
                telegram_api::AccessPointRule {
                    phone_prefix_rules: "+888".to_string(),
                    dc_id: 3,
                    ips: vec![ip_port(0x0808_0808, 443)],
                },
                telegram_api::AccessPointRule {
                    phone_prefix_rules: "+999".to_string(),
                    dc_id: 1001,
                    ips: vec![ip_port(0x0707_0707, 443)],
                },
            ],
        };
        let applied_at = tdutils::time::now();
        recoverer.do_on_simple_config(Ok(config), &environment);
        recoverer.update_dc_options();

        // The phone/dcid filters dropped the +888 rule and the dc 1001
        // rule; the shuffle is a permutation of the two survivors.
        assert_eq!(recoverer.dc_options().len(), 2);
        let mut endpoints: Vec<(i32, i32)> = recoverer
            .dc_options()
            .iter()
            .map(|option| (option.raw_dc_id, option.port))
            .collect();
        endpoints.sort_unstable();
        assert_eq!(endpoints, vec![(2, 443), (2, 5222)]);

        // The success expiry is the blocked-mode 2..3 minutes (online) —
        // the lower bound anchors at a `now()` sampled before the call, the
        // upper at one after (the draw can hit either endpoint).
        assert!(
            recoverer.simple_config_expires_at >= applied_at + 120.0
                && recoverer.simple_config_expires_at <= tdutils::time::now() + 180.0
        );
    }

    #[test]
    fn an_expired_simple_config_is_rejected() {
        let mut recoverer = ConfigRecoverer::new();
        let environment = env(true);
        recoverer.is_online = true;

        // `expires` below the env unix time (100_000): the rules are not
        // applied, but the expiry timestamp still advances (cpp:601/622).
        let mut config = simple_config_with_rules("", 2, ip_port(0x0909_0909, 443));
        config.expires = 99_999;
        let applied_at = tdutils::time::now();
        recoverer.do_on_simple_config(Ok(config), &environment);
        recoverer.update_dc_options();
        assert!(recoverer.dc_options().is_empty());
        assert!(
            recoverer.simple_config_expires_at >= applied_at + 120.0
                && recoverer.simple_config_expires_at <= tdutils::time::now() + 180.0
        );
    }

    #[test]
    fn expire_time_clamps_in_both_modes() {
        // (expect_blocking, online) -> (success window, failure window) in
        // seconds (cpp:645-655): blocked 2-3 min / 5-7 s, free 20-30 min /
        // 15-30 s; being offline adds the 5-minute delay.
        for (expect_blocking, online, ok_lo, ok_hi, fail_lo, fail_hi) in [
            (true, true, 120.0, 180.0, 5.0, 7.0),
            (false, true, 1200.0, 1800.0, 15.0, 30.0),
            (
                true,
                false,
                300.0 + 120.0,
                300.0 + 180.0,
                300.0 + 5.0,
                300.0 + 7.0,
            ),
        ] {
            let environment = env(expect_blocking);
            let mut recoverer = ConfigRecoverer::new();
            recoverer.is_online = online;

            let ok_before = tdutils::time::now();
            recoverer.do_on_simple_config(
                Ok(simple_config_with_rules("", 2, ip_port(0x0909_0909, 443))),
                &environment,
            );
            assert!(
                recoverer.simple_config_expires_at >= ok_before + ok_lo
                    && recoverer.simple_config_expires_at <= tdutils::time::now() + ok_hi,
                "success window [{ok_lo}, {ok_hi}]"
            );

            let fail_before = tdutils::time::now();
            recoverer
                .do_on_simple_config(Err(Status::error_message("HTTP error: 500")), &environment);
            assert!(
                recoverer.simple_config.is_empty(),
                "a failure clears the options"
            );
            assert!(
                recoverer.simple_config_expires_at >= fail_before + fail_lo
                    && recoverer.simple_config_expires_at <= tdutils::time::now() + fail_hi,
                "failure window [{fail_lo}, {fail_hi}]"
            );
        }
    }

    #[test]
    fn a_full_config_sets_expiry_and_carries_the_options() {
        let mut recoverer = ConfigRecoverer::new();
        let environment = env(true);
        recoverer.is_online = true;

        // The full config answer carries the options to ConnectionCreator
        // and arms the success expiry.
        let full_config = telegram_api::Config {
            dc_options: vec![telegram_api::DcOption {
                id: 2,
                ip_address: "9.9.9.9".to_string(),
                port: 443,
                ..telegram_api::DcOption::default()
            }],
            ..telegram_api::Config::default()
        };
        let ok_before = tdutils::time::now();
        let actions = recoverer.on_full_config(Ok(full_config), &environment);
        assert_eq!(actions.connection_creator_dc_options.len(), 1);
        assert!(
            recoverer.full_config_expires_at >= ok_before + 120.0
                && recoverer.full_config_expires_at <= tdutils::time::now() + 180.0
        );

        // A failure clears the config and arms the 5..7 s window (blocked,
        // online).
        let fail_before = tdutils::time::now();
        let actions =
            recoverer.on_full_config(Err(Status::error_message("Timeout expired")), &environment);
        assert!(actions.connection_creator_dc_options.is_empty());
        assert!(
            recoverer.full_config_expires_at >= fail_before + 5.0
                && recoverer.full_config_expires_at <= tdutils::time::now() + 7.0
        );
    }

    #[test]
    fn the_saved_dc_options_update_replays_and_merges_first() {
        let mut pmc = RecovererPmc::new("dc-options");
        let stored = StoredDcOptions {
            options: vec![StoredDcOption {
                flags: 0,
                raw_dc_id: 10,
                ip: b"10.0.0.1".to_vec(),
                port: 443,
                secret: Vec::new(),
            }],
        };
        pmc.set_raw(b"dc_options_update", &stored.store());

        let mut recoverer = ConfigRecoverer::new();
        let environment = env(true);
        let saved = load_dc_options_update(&pmc);
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].ip_address, "10.0.0.1");
        recoverer.on_dc_options_update(saved);

        // The replayed update is the update part; a later simple config
        // appends AFTER it (ConfigManager.cpp:823-827).
        let config = simple_config_with_rules("", 2, ip_port(0x0909_0909, 443));
        recoverer.do_on_simple_config(Ok(config), &environment);
        recoverer.update_dc_options();
        assert_eq!(recoverer.dc_options().len(), 2);
        assert_eq!(recoverer.dc_options()[0].ip_address, "10.0.0.1");
        assert_eq!(recoverer.dc_options()[1].ip_address, "9.9.9.9");

        // A corrupt pmc value degrades to the empty set (the C++
        // `log_event_parse(...).ensure()` would crash; the port logs).
        pmc.set_raw(b"dc_options_update", &[1, 2, 3]);
        assert!(load_dc_options_update(&pmc).is_empty());
    }

    #[test]
    fn the_simple_config_shuffle_is_a_permutation() {
        let mut recoverer = ConfigRecoverer::new();
        let environment = env(true);
        recoverer.is_online = true;

        // Three rules for the right phone prefix → three options, shuffled.
        let config = telegram_api::help_ConfigSimple {
            date: 99_000,
            expires: 200_000,
            rules: (1..=3)
                .map(|dc| telegram_api::AccessPointRule {
                    phone_prefix_rules: String::new(),
                    dc_id: dc,
                    ips: vec![ip_port(0x0a00_0000 + dc, 443)],
                })
                .collect(),
        };
        recoverer.do_on_simple_config(Ok(config), &environment);
        recoverer.update_dc_options();

        let mut addresses: Vec<String> = recoverer
            .dc_options()
            .iter()
            .map(|option| option.ip_address.clone())
            .collect();
        addresses.sort();
        let expected: Vec<String> = (1..=3).map(|dc| ipv4_to_string(0x0a00_0000 + dc)).collect();
        assert_eq!(addresses, expected, "the shuffle must be a permutation");
    }
}
