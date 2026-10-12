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
//! the recoverer state, the config application (M.3.7.1), the `loop()`
//! decision machine with the state callbacks and the ref-count/close
//! semantics (M.3.7.2), the simple-config fetch orchestration — the
//! fetcher rotation, `on_simple_config` with the dns-time alternation
//! (M.3.7.3) — and the full-config mini-session: the pmc key/salt
//! store, the recovery RSA context, the session header and the unauthed
//! `help.getConfig` executor (M.3.7.4); the Td wiring follows in
//! M.3.7.5.

use std::sync::OnceLock;

#[cfg(not(target_family = "wasm"))]
use tokio::net::TcpStream;

use base64::Engine as _;
use tddb::binlog_key_value::BinlogKeyValue;
use tdmtproto::auth_data::{AuthData, ServerSalt, parse_server_salt, store_server_salt};
use tdmtproto::auth_key::AuthKey;
use tdmtproto::dh_callback::DhCallback;
use tdmtproto::handshake::{AuthKeyHandshake, AuthKeyHandshakeContext};
#[cfg(not(target_family = "wasm"))]
use tdmtproto::handshake_actor::run_handshake;
use tdmtproto::message_id::MessageId;
use tdmtproto::proxy_secret::ProxySecret;
#[cfg(not(target_family = "wasm"))]
use tdmtproto::raw_connection::RawConnection;
use tdmtproto::rsa::{PublicRsaKeyInterface, RsaKey};
use tdmtproto::session_connection::{SessionConnection, SessionConnectionCallback, SessionMode};
use tdmtproto::telegram_api;
use tdmtproto::transport_type::{TransportType, TransportTypeEnum};
use tdnet::http_query::HttpQuery;
use tdnet::wget::{VerifyPeer, WgetParams, wget};
use tdutils::buffer::BufferSlice;
use tdutils::http_date::parse_http_date;
use tdutils::http_url::url_encode;
use tdutils::json::{JsonValue, JsonValueType, json_decode};
use tdutils::random;
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

/// The request half of a fetcher: what `get_simple_config_impl` sends
/// (and what the recoverer's fetch action pins for the driver).
#[derive(Clone, Debug)]
pub struct SimpleConfigRequest {
    pub url: String,
    /// Extra headers before the shared `Host`/`User-Agent`.
    pub headers: Vec<(String, String)>,
    pub content: Vec<u8>,
    pub content_type: String,
    /// The `Host` header value (the C++ `host` argument).
    pub host: &'static str,
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

// ---- The fetch rotation (ConfigManager.cpp:755-777, M.3.7.3) -------------

/// The five simple-config fetchers the recoverer rotates over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SimpleConfigService {
    GoogleDns,
    MozillaDns,
    Azure,
    FirebaseRealtime,
    FirebaseFirestore,
}

/// The `switch (simple_config_turn_ % 9)` in the recoverer `loop()`
/// (ConfigManager.cpp:756-773): google on {0, 3, 7}, mozilla on {1, 4, 6},
/// azure on 5, firestore on 2, realtime on 8.
pub fn simple_config_service(turn: u32) -> SimpleConfigService {
    match turn % 9 {
        5 => SimpleConfigService::Azure,
        2 => SimpleConfigService::FirebaseFirestore,
        8 => SimpleConfigService::FirebaseRealtime,
        0 | 3 | 7 => SimpleConfigService::GoogleDns,
        1 | 4 | 6 => SimpleConfigService::MozillaDns,
        _ => SimpleConfigService::MozillaDns,
    }
}

/// The request each fetcher would send — the recoverer's fetch action
/// carries it so the driver can execute (or pin) the request.
fn simple_config_request(
    service: SimpleConfigService,
    domain_name: &str,
    is_test: bool,
) -> SimpleConfigRequest {
    match service {
        SimpleConfigService::GoogleDns => {
            dns_request("dns.google/resolve", "dns.google", domain_name, is_test)
        }
        SimpleConfigService::MozillaDns => dns_request(
            "mozilla.cloudflare-dns.com/dns-query",
            "mozilla.cloudflare-dns.com",
            domain_name,
            is_test,
        ),
        SimpleConfigService::Azure => azure_request(is_test),
        SimpleConfigService::FirebaseRealtime => firebase_realtime_request(),
        SimpleConfigService::FirebaseFirestore => firebase_firestore_request(),
    }
}

/// The rotation dispatch: one C++ `get_simple_config_*(promise,
/// prefer_ipv6, dc_txt_domain_name, is_test_dc, scheduler_id)` call — the
/// promise side is the driver feeding the result back through
/// [`ConfigRecoverer::on_simple_config`].
pub async fn fetch_simple_config(
    service: SimpleConfigService,
    prefer_ipv6: bool,
    domain_name: &str,
    is_test: bool,
) -> Result<SimpleConfigResult, Status> {
    match service {
        SimpleConfigService::GoogleDns => {
            get_simple_config_google_dns(prefer_ipv6, domain_name, is_test).await
        }
        SimpleConfigService::MozillaDns => {
            get_simple_config_mozilla_dns(prefer_ipv6, domain_name, is_test).await
        }
        SimpleConfigService::Azure => {
            get_simple_config_azure(prefer_ipv6, domain_name, is_test).await
        }
        SimpleConfigService::FirebaseRealtime => {
            get_simple_config_firebase_realtime(prefer_ipv6, domain_name, is_test).await
        }
        SimpleConfigService::FirebaseFirestore => {
            get_simple_config_firebase_firestore(prefer_ipv6, domain_name, is_test).await
        }
    }
}

// ---- The ConfigRecoverer state (ConfigManager.cpp:499-832, M.3.7.1) ------
//
// The C++ actor as an inline state machine on the Td actor, like the
// ConfigManager port. M.3.7.1 carries the state fields and the config
// application; the loop()/state callbacks (M.3.7.2) and the fetch
// orchestration (M.3.7.3) land on top of it; the full-config mini-session
// (M.3.7.4) and the Td wiring (M.3.7.5) follow.

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
    if raw.len() < 4
        || i32::from_le_bytes(raw[0..4].try_into().expect("length checked"))
            != crate::store_parse::version::LOG_EVENT_STORE_VERSION
    {
        return Vec::new();
    }
    match StoredDcOptions::parse(&raw[4..]) {
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

/// The `G()` reads the recoverer makes per step, gathered by the driver
/// (the C++ recoverer reads them through the global directly). The loop's
/// clocks come through here too — injected, not read from
/// [`tdutils::time`] — so the decision table is testable deterministically
/// (the C++ mixes `Time::now()` and `Time::now_cached()` the same way; the
/// driver fills both from the real clock, where they are almost equal).
#[derive(Clone, Debug)]
pub struct RecovererEnv {
    /// `G()->unix_time()`.
    pub unix_time: i32,
    /// `G()->get_option_string("my_phone_number")`.
    pub my_phone_number: String,
    /// `expect_blocking()` — `G()->get_option_boolean("expect_blocking", true)`,
    /// resolved by the driver.
    pub expect_blocking: bool,
    /// `Time::now()` — the base of the high-load skip's
    /// `set_timeout_in(Random::fast(200, 300))` and of the dns-time
    /// differences.
    pub now: f64,
    /// `Time::now_cached()` — the clock every expiry check reads.
    pub now_cached: f64,
    /// `Session::is_high_loaded()` (net/Session.cpp:286) — at least 100
    /// live `GenAuthKeyActor`s (net/Session.cpp:113-129); the driver
    /// resolves it (always `false` until the handshake-actor pool of the
    /// net step).
    pub is_high_loaded: bool,
    /// `G()->is_test_dc()`.
    pub is_test_dc: bool,
    /// `G()->get_option_boolean("prefer_ipv6")`.
    pub prefer_ipv6: bool,
    /// `G()->get_option_string("dc_txt_domain_name")`.
    pub dc_txt_domain_name: String,
}

/// One pending simple-config fetch — the C++
/// `get_simple_config_*(promise, prefer_ipv6, dc_txt_domain_name, is_test,
/// scheduler_id)` actor the loop dispatched (the `prefer_ipv6` transport
/// flag rides the env into [`fetch_simple_config`]).
#[derive(Clone, Debug)]
pub struct SimpleConfigFetch {
    /// The `simple_config_turn_` the fetch was issued on.
    pub turn: u32,
    /// The fetcher this turn selected.
    pub service: SimpleConfigService,
    /// The request the C++ actor would send (the pinned url/host/headers).
    pub request: SimpleConfigRequest,
}

/// What one recoverer step asks its driver to do — the C++ side effects the
/// inline recoverer cannot take itself (`G()` writes, actor creation, the
/// timer). The loop's wakeup and the two fetch decisions ride these —
/// the simple-config fetch carries the turn rotation, the full-config
/// fetch the mini-session spec.
#[derive(Debug, Default)]
pub struct RecovererActions {
    /// `ConnectionCreator::on_dc_options(DcOptions(full_config->dc_options_))`
    /// after a full config (ConfigManager.cpp:632) — `Some` exactly when
    /// a full config arrived (an option-less config still carries
    /// `Some(vec![])`); the Td runs the `DcOptions(server_dc_options)`
    /// ctor filter and the consumer call (M.3.9.1).
    pub connection_creator_dc_options: Option<Vec<telegram_api::DcOption>>,
    /// `set_timeout_at(wakeup_timestamp.at())` — the earliest future
    /// deadline the loop wants to wake at. `None` leaves the currently
    /// armed timer alone (the C++ only re-arms on a valid timestamp).
    pub wakeup_at: Option<f64>,
    /// The loop decided `need_simple_config` — the fetch this turn of the
    /// rotation selected; the driver dispatches it through
    /// [`fetch_simple_config`] and feeds the result back through
    /// [`ConfigRecoverer::on_simple_config`].
    pub fetch_simple_config: Option<SimpleConfigFetch>,
    /// The loop decided `need_full_config` — the unauthed mini-session
    /// ([`fetch_full_config`], M.3.7.4) to run; already rotated past.
    pub fetch_full_config: Option<FullConfigFetch>,
    /// `G()->update_dns_time_difference(diff)` after a simple config
    /// (the http date and the config date alternate as the source,
    /// ConfigManager.cpp:578-582).
    pub dns_time_difference: Option<f64>,
}

/// One pending full-config fetch — the C++
/// `full_config_query_ = get_full_config(option, &promise, ...)`
/// (ConfigManager.cpp:781-784). Only the option rides the action; the
/// executor's other arguments (the test-ness, the header options, the
/// server time difference, the pmc) are `G()` reads the driver resolves
/// (M.3.7.5).
#[derive(Clone, Debug)]
pub struct FullConfigFetch {
    /// `dc_options_.dc_options[dc_options_i_]`.
    pub option: RecovererDcOption,
}

/// The `ConfigRecoverer` (ConfigManager.cpp:499-832) as an inline state
/// machine on the Td actor, like the ConfigManager port. The C++ actor's
/// promise/actor plumbing is the driver's job here: the fetches are
/// returned as [`RecovererActions`], the fetch results come back through
/// [`ConfigRecoverer::on_simple_config`] /
/// [`ConfigRecoverer::on_full_config`], and each query share of the C++
/// `ref_cnt_` is taken by the loop and released through
/// [`ConfigRecoverer::hangup_shared`] / [`ConfigRecoverer::hangup`].
pub struct ConfigRecoverer {
    /// `is_connecting_`.
    is_connecting: bool,
    /// `connecting_since_` — seeded `Time::now()` in the constructor.
    connecting_since: f64,

    /// `is_online_`.
    is_online: bool,

    /// `has_network_`.
    has_network: bool,
    /// `has_network_since_` — written, never read by the C++ either.
    has_network_since: f64,
    /// `network_generation_` — the C++ compares but never assigns it
    /// (ConfigManager.cpp:512-520, quirk kept).
    network_generation: u32,

    /// `simple_config_` — the options of the last simple config.
    simple_config: Vec<RecovererDcOption>,
    /// `simple_config_expires_at_`.
    simple_config_expires_at: f64,
    /// `simple_config_at_` (only logged in the C++).
    simple_config_at: f64,
    /// `!simple_config_query_.empty()`.
    simple_config_query_active: bool,
    /// `simple_config_turn_` — the rotation index over the five fetchers.
    simple_config_turn: u32,
    /// `date_option_i_` — alternates the `update_dns_time_difference`
    /// source between the http date and the config date
    /// (ConfigManager.cpp:578-583).
    date_option_i: usize,

    /// `dc_options_update_`.
    dc_options_update: Vec<RecovererDcOption>,

    /// `dc_options_` — `dc_options_update_` + `simple_config_`.
    dc_options: Vec<RecovererDcOption>,
    /// `dc_options_at_`.
    dc_options_at: f64,
    /// `dc_options_i_`.
    dc_options_i: usize,

    /// `full_config_`.
    full_config: Option<Box<telegram_api::Config>>,
    /// `full_config_expires_at_`.
    full_config_expires_at: f64,
    /// `!full_config_query_.empty()`.
    full_config_query_active: bool,

    /// `ref_cnt_` — 1 is the parent's share, plus one per in-flight query.
    ref_cnt: u32,
    /// `close_flag_`.
    close_flag: bool,
}

impl Default for ConfigRecoverer {
    fn default() -> Self {
        Self::new()
    }
}

impl ConfigRecoverer {
    /// The constructor — the C++ seeds `connecting_since_ = Time::now()`.
    pub fn new() -> Self {
        Self {
            is_connecting: false,
            connecting_since: tdutils::time::now(),
            is_online: false,
            has_network: false,
            has_network_since: 0.0,
            network_generation: 0,
            simple_config: Vec::new(),
            simple_config_expires_at: 0.0,
            simple_config_at: 0.0,
            simple_config_query_active: false,
            simple_config_turn: 0,
            date_option_i: 0,
            dc_options_update: Vec::new(),
            dc_options: Vec::new(),
            dc_options_at: 0.0,
            dc_options_i: 0,
            full_config: None,
            full_config_expires_at: 0.0,
            full_config_query_active: false,
            ref_cnt: 1,
            close_flag: false,
        }
    }

    /// `simple_config_at_` (only written by the C++ too).
    pub fn simple_config_at(&self) -> f64 {
        self.simple_config_at
    }

    /// `has_network_since_` (only written by the C++ too).
    pub fn has_network_since(&self) -> f64 {
        self.has_network_since
    }

    /// `dc_options_` — the merged set (the update part first, then the
    /// simple config).
    pub fn dc_options(&self) -> &[RecovererDcOption] {
        &self.dc_options
    }

    /// `!simple_config_query_.empty()` — the driver's delivery guard: a
    /// result without a dispatched query would double-release the query's
    /// ref-count share at `hangup_shared`.
    pub fn simple_config_query_active(&self) -> bool {
        self.simple_config_query_active
    }

    /// `!full_config_query_.empty()` — the full-config delivery guard.
    pub fn full_config_query_active(&self) -> bool {
        self.full_config_query_active
    }

    /// `on_dc_options_update` (ConfigManager.cpp:505-509) — ends in the
    /// loop, like the C++.
    pub fn on_dc_options_update(
        &mut self,
        dc_options: Vec<RecovererDcOption>,
        env: &RecovererEnv,
    ) -> RecovererActions {
        self.dc_options_update = dc_options;
        self.update_dc_options();
        self.loop_(env)
    }

    /// `on_network` (ConfigManager.cpp:512-520) — the state callback's
    /// network leg. The generation guard keeps the C++ quirk:
    /// `network_generation_` is compared but never assigned, so every call
    /// with a generation other than the initial 0 re-stamps
    /// `has_network_since_` while the network is up.
    pub fn on_network(
        &mut self,
        has_network: bool,
        network_generation: u32,
        env: &RecovererEnv,
    ) -> RecovererActions {
        self.has_network = has_network;
        if self.network_generation != network_generation && has_network {
            self.has_network_since = env.now_cached;
        }
        self.loop_(env)
    }

    /// `on_online` (ConfigManager.cpp:521-536) — going online expires the
    /// EMPTY configs immediately (their cached state is worthless after
    /// reconnecting); an unchanged flag runs no loop, so the default
    /// actions leave the armed timer alone.
    pub fn on_online(&mut self, is_online: bool, env: &RecovererEnv) -> RecovererActions {
        if self.is_online == is_online {
            return RecovererActions::default();
        }
        self.is_online = is_online;
        if is_online {
            if self.simple_config.is_empty() {
                self.simple_config_expires_at = 0.0;
            }
            if self.full_config.is_none() {
                self.full_config_expires_at = 0.0;
            }
        }
        self.loop_(env)
    }

    /// `on_connecting` (ConfigManager.cpp:537-544) — `connecting_since_` is
    /// stamped on the rising edge only.
    pub fn on_connecting(&mut self, is_connecting: bool, env: &RecovererEnv) -> RecovererActions {
        tracing::trace!("On connecting {is_connecting}");
        if is_connecting && !self.is_connecting {
            self.connecting_since = env.now_cached;
        }
        self.is_connecting = is_connecting;
        self.loop_(env)
    }

    /// The `StateCallback::on_state` forwarding
    /// (ConfigManager.cpp:804-807) — only a plain `Connecting` counts as a
    /// connecting problem (`ConnectingToProxy` deliberately does not);
    /// registration with the StateManager lands with the wiring (M.3.7.5).
    pub fn on_connection_state(
        &mut self,
        state: RecovererConnectionState,
        env: &RecovererEnv,
    ) -> RecovererActions {
        self.on_connecting(state == RecovererConnectionState::Connecting, env)
    }

    /// The `StateCallback::on_network` forwarding
    /// (ConfigManager.cpp:808-811) — every `NetType` but `None` means the
    /// network is up.
    pub fn on_net_type(
        &mut self,
        net_type: RecovererNetType,
        network_generation: u32,
        env: &RecovererEnv,
    ) -> RecovererActions {
        let has_network = !matches!(net_type, RecovererNetType::None);
        self.on_network(has_network, network_generation, env)
    }

    /// `check_phone_number_rules` (ConfigManager.cpp:546-564).
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

    /// `on_simple_config` (ConfigManager.cpp:566-588) — the fetch result
    /// lands: the query slot is released and the dc-option rotation reset,
    /// an outer error becomes the merged `r_http_date`+`r_config` error,
    /// the dns-time difference source alternates (the http date first),
    /// the config is applied, and the loop re-runs.
    pub fn on_simple_config(
        &mut self,
        r_simple_config_result: Result<SimpleConfigResult, Status>,
        env: &RecovererEnv,
    ) -> RecovererActions {
        self.simple_config_query_active = false;
        self.dc_options_i = 0;

        let cfg = match r_simple_config_result {
            Ok(cfg) => cfg,
            Err(error) => SimpleConfigResult {
                r_http_date: Err(error.clone()),
                r_config: Err(error),
            },
        };

        let mut actions = RecovererActions::default();
        // cpp:578-582 — the http date feeds the difference on the even
        // alternation turns and whenever the config failed (the error
        // path keeps the http date); otherwise the config date does.
        if cfg.r_http_date.is_ok() && (self.date_option_i == 0 || cfg.r_config.is_err()) {
            if let Ok(http_date) = cfg.r_http_date {
                actions.dns_time_difference = Some(f64::from(http_date) - env.now);
            }
        } else if let Ok(config) = &cfg.r_config {
            actions.dns_time_difference = Some(f64::from(config.date) - env.now);
        }
        self.date_option_i = (self.date_option_i + 1) % 2;

        self.do_on_simple_config(cfg.r_config, env);
        self.update_dc_options();
        let loop_actions = self.loop_(env);
        actions.wakeup_at = loop_actions.wakeup_at;
        actions.fetch_simple_config = loop_actions.fetch_simple_config;
        actions.fetch_full_config = loop_actions.fetch_full_config;
        actions
    }

    /// `do_on_simple_config` (ConfigManager.cpp:590-624) — the config
    /// application under [`ConfigRecoverer::on_simple_config`]; its own
    /// actions stay empty.
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
    /// with the net step, M.3.9.1), and the C++ tail `loop()` re-runs
    /// here. The query's ref-count share is NOT released inside: the
    /// driver calls [`ConfigRecoverer::hangup_shared`] after the delivery,
    /// like the C++ `GetConfigActor` death does.
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
                actions.connection_creator_dc_options = Some(config.dc_options.clone());
                self.full_config = Some(Box::new(config));
            }
            Err(error) => {
                tracing::trace!("Failed to get config: {error}");
                self.full_config = None;
                self.full_config_expires_at = self.get_failed_config_expire_time(env);
            }
        }
        let loop_actions = self.loop_(env);
        actions.wakeup_at = loop_actions.wakeup_at;
        actions.fetch_simple_config = loop_actions.fetch_simple_config;
        actions.fetch_full_config = loop_actions.fetch_full_config;
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

    /// `max_connecting_delay` (ConfigManager.cpp:707-709).
    fn max_connecting_delay(&self, env: &RecovererEnv) -> f64 {
        if Self::expect_blocking(env) {
            5.0
        } else {
            20.0
        }
    }

    /// `loop()` (ConfigManager.cpp:711-797) — the decision machine. The
    /// fetch dispatch is carried as actions: the simple-config fetch with
    /// its turn rotation (M.3.7.3), the unauthed mini-session spec with
    /// M.3.7.4; each dispatch takes one query share of the ref count.
    pub fn loop_(&mut self, env: &RecovererEnv) -> RecovererActions {
        let mut actions = RecovererActions::default();
        if self.close_flag {
            return actions;
        }
        if env.is_high_loaded {
            tracing::trace!("Skip config recoverer under high load");
            // `set_timeout_in(Random::fast(200, 300))`.
            actions.wakeup_at = Some(env.now + f64::from(tdutils::random::fast(200, 300)));
            return actions;
        }

        tracing::trace!(
            "{} in {} seconds",
            if self.is_connecting {
                "Failed to connect for"
            } else {
                "Successfully connected in"
            },
            env.now_cached - self.connecting_since
        );

        // The C++ `Timestamp wakeup_timestamp` + `check_timeout` lambda:
        // a past (or never-set) timestamp means "expired"; a future one is
        // folded into the wakeup, which keeps the EARLIEST deadline
        // (`Timestamp::relax`, Time.h:84-90 — the wakeup must fire at the
        // first expiry, not the last).
        let mut wakeup_at = None;
        let has_connecting_problem = self.is_connecting
            && check_timeout(
                &mut wakeup_at,
                self.connecting_since + self.max_connecting_delay(env),
                env.now_cached,
            );
        let is_valid_simple_config = !check_timeout(
            &mut wakeup_at,
            self.simple_config_expires_at,
            env.now_cached,
        );
        if !is_valid_simple_config && !self.simple_config.is_empty() {
            self.simple_config.clear();
            self.update_dc_options();
        }
        let need_simple_config =
            has_connecting_problem && !is_valid_simple_config && !self.simple_config_query_active;
        let has_dc_options = !self.dc_options.is_empty();
        let is_valid_full_config =
            !check_timeout(&mut wakeup_at, self.full_config_expires_at, env.now_cached);
        let need_full_config = has_connecting_problem
            && has_dc_options
            && !is_valid_full_config
            && !self.full_config_query_active
            && check_timeout(
                &mut wakeup_at,
                self.dc_options_at
                    + if Self::expect_blocking(env) {
                        5.0
                    } else {
                        10.0
                    },
                env.now_cached,
            );

        if need_simple_config {
            // `ref_cnt_++` (cpp:750) — the query's share.
            self.ref_cnt += 1;
            self.simple_config_query_active = true;
            let turn = self.simple_config_turn;
            tracing::trace!("Ask simple config with turn {turn}");
            // The rotation (cpp:756-773) picks the fetcher; the arguments
            // (cpp:774-776) are the options the driver resolved into the
            // env, plus the promise the fetch action stands in for.
            let service = simple_config_service(turn);
            actions.fetch_simple_config = Some(SimpleConfigFetch {
                turn,
                service,
                request: simple_config_request(service, &env.dc_txt_domain_name, env.is_test_dc),
            });
            self.simple_config_turn += 1;
        }

        if need_full_config {
            // `ref_cnt_++` (cpp:781) — the mini-session's share.
            self.ref_cnt += 1;
            self.full_config_query_active = true;
            tracing::trace!("Ask full config with dc_options_i = {}", self.dc_options_i);
            let option = self.dc_options[self.dc_options_i].clone();
            actions.fetch_full_config = Some(FullConfigFetch { option });
            self.dc_options_i = (self.dc_options_i + 1) % self.dc_options.len();
        }

        if let Some(at) = wakeup_at {
            tracing::trace!("Wakeup in {} seconds", at - env.now_cached);
            actions.wakeup_at = Some(at);
        }
        actions
    }

    /// `hangup_shared` (ConfigManager.cpp:689-692) — one query share
    /// released: the C++ share rides the holder actor, so the driver
    /// calls this when a holder died — after delivering a query result
    /// ([`ConfigRecoverer::on_full_config`] clears the slot itself) or on
    /// an undelivered death (the M.3.7.4 timeout) — with the query's slot
    /// cleared by then; a slot still marked active at
    /// [`ConfigRecoverer::hangup`] would release its share twice.
    /// Returns `try_stop`.
    pub fn hangup_shared(&mut self) -> bool {
        self.ref_cnt -= 1;
        self.try_stop()
    }

    /// `hangup` (ConfigManager.cpp:693-698) — the parent closes the
    /// recoverer: the in-flight queries are cancelled and release their
    /// shares with it. Returns `try_stop`.
    pub fn hangup(&mut self) -> bool {
        self.ref_cnt -= 1;
        self.close_flag = true;
        if self.simple_config_query_active {
            self.simple_config_query_active = false;
            self.ref_cnt -= 1;
        }
        if self.full_config_query_active {
            self.full_config_query_active = false;
            self.ref_cnt -= 1;
        }
        self.try_stop()
    }

    /// `try_stop` (ConfigManager.cpp:701-705) — the recoverer stops when
    /// its last share drops; the driver's `stop()` is dropping the state
    /// machine (and unregistering the state callback).
    fn try_stop(&self) -> bool {
        self.ref_cnt == 0
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

/// The loop's `check_timeout` lambda (ConfigManager.cpp:728-734): an
/// expired (or never-set, `Timestamp::at(0)`) deadline answers `true`
/// without entering the wakeup relax, which keeps the earliest valid
/// deadline (`Timestamp::relax`, Time.h:84-90).
fn check_timeout(wakeup_at: &mut Option<f64>, timestamp: f64, now_cached: f64) -> bool {
    if timestamp < now_cached {
        return true;
    }
    *wakeup_at = Some(match *wakeup_at {
        Some(at) if at <= timestamp => at,
        _ => timestamp,
    });
    false
}

/// `td::ConnectionState` (ConnectionState.h) as the recoverer's state
/// callback sees it — only a plain `Connecting` counts as a connecting
/// problem; `ConnectingToProxy` deliberately does not
/// (ConfigManager.cpp:804-807).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecovererConnectionState {
    WaitingForNetwork,
    ConnectingToProxy,
    Connecting,
    Updating,
    Ready,
    Empty,
}

/// `td::NetType` (net/NetType.h) as the recoverer's network callback sees
/// it — every variant but `None` means the network is up
/// (ConfigManager.cpp:808-811).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecovererNetType {
    Other,
    WiFi,
    Mobile,
    MobileRoaming,
    None,
    Unknown,
}

// ---- get_full_config (ConfigManager.cpp:325-497, M.3.7.4) -------------------
//
// The C++ builds a one-query mini-session out of three local classes around
// the full `Session` actor: `SimpleAuthData` (a pmc-backed
// `AuthDataShared`), a `SessionCallback` (the raw-connection source and
// result dispatcher) and a `GetConfigActor` (the promise owner with the
// 10-second actor timeout). The port has no `Session` actor yet (it lands
// with the net step), so the mini-session is one async executor over the
// Track-G `SessionConnection`, with the C++ behaviors mapped as documented
// on [`fetch_full_config`].

/// `SimpleAuthData::auth_key_key` — the persisted recovery auth key, keyed
/// by the raw dc id (ConfigManager.cpp:415-421).
fn recovery_auth_key_key(raw_dc_id: i32) -> String {
    format!("config_recovery_auth{raw_dc_id}")
}

/// `SimpleAuthData::future_salts_key` (ConfigManager.cpp:423-427).
fn recovery_salt_key(raw_dc_id: i32) -> String {
    format!("config_recovery_salt{raw_dc_id}")
}

/// The `SimpleAuthData::set_auth_key` pmc write — `td::serialize(AuthKey)`
/// (ConfigManager.cpp:386-392).
pub fn store_recovery_auth_key(pmc: &mut BinlogKeyValue, raw_dc_id: i32, auth_key: &AuthKey) {
    let mut out = Vec::new();
    auth_key.store(&mut out);
    pmc.set_raw(recovery_auth_key_key(raw_dc_id).as_bytes(), &out);
}

/// The `SimpleAuthData::get_auth_key` pmc read (ConfigManager.cpp:377-385):
/// an empty value is the C++ default-constructed (absent) key; a value that
/// fails to parse is the C++ `unserialize(...).ensure()` crash degraded to
/// the same absence.
pub fn load_recovery_auth_key(pmc: &BinlogKeyValue, raw_dc_id: i32) -> Option<AuthKey> {
    let raw = pmc.get_raw(recovery_auth_key_key(raw_dc_id).as_bytes());
    if raw.is_empty() {
        return None;
    }
    let mut reader = tdmtproto::mtproto_api::tl::Reader::new(&raw);
    AuthKey::parse(&mut reader).ok()
}

/// The `SimpleAuthData::set_future_salts` pmc value —
/// `td::serialize(vector<mtproto::ServerSalt>)`: an int32 count + the
/// `store(ServerSalt)` triples (ConfigManager.cpp:404-410).
pub fn store_recovery_salts(salts: &[ServerSalt]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(salts.len() as i32).to_le_bytes());
    for salt in salts {
        store_server_salt(&mut out, salt);
    }
    out
}

/// The `SimpleAuthData::get_future_salts` pmc read
/// (ConfigManager.cpp:412-421): truncated input degrades to the empty set
/// (the C++ `unserialize(...).ensure()` would crash).
pub fn load_recovery_salts(pmc: &BinlogKeyValue, raw_dc_id: i32) -> Vec<ServerSalt> {
    let raw = pmc.get_raw(recovery_salt_key(raw_dc_id).as_bytes());
    let mut res = Vec::new();
    if raw.len() < 4 {
        return res;
    }
    let count = i32::from_le_bytes(raw[..4].try_into().expect("length checked"));
    let mut reader = tdmtproto::mtproto_api::tl::Reader::new(&raw[4..]);
    for _ in 0..count.clamp(0, 1000) {
        match parse_server_salt(&mut reader) {
            Ok(salt) => res.push(salt),
            Err(_) => return Vec::new(),
        }
    }
    res
}

/// `PublicRsaKeySharedMain::create(is_test)`
/// (net/PublicRsaKeySharedMain.cpp:22-58) — one pinned key per flavor; the
/// fingerprint is the parsed key's own (`add_pem`).
struct RecoveryRsa {
    rsa: Rsa,
    fingerprint: i64,
}

impl PublicRsaKeyInterface for RecoveryRsa {
    fn get_rsa_key(&self, fingerprints: &[i64]) -> tdutils::status::Result<RsaKey> {
        if fingerprints.contains(&self.fingerprint) {
            tdutils::status::Result::Ok(RsaKey {
                rsa: self.rsa.clone(),
                fingerprint: self.fingerprint,
            })
        } else {
            tdutils::status::Result::Err(Status::error_message(&format!(
                "Unknown Main fingerprints {fingerprints:?}"
            )))
        }
    }

    fn drop_keys(&mut self) {}
}

/// The `PrimeCallback` of the Track-G probes (the C++
/// `DhCallback`): accept the built-in prime, check others on the fly.
struct RecoveryPrimeCallback;

impl DhCallback for RecoveryPrimeCallback {
    fn is_good_prime(&self, _prime_str: &[u8]) -> i32 {
        -1 // run the full primality checks
    }
    fn add_good_prime(&self, _prime_str: &[u8]) {}
    fn add_bad_prime(&self, _prime_str: &[u8]) {}
}

/// The `td::mtproto::AuthKeyHandshakeContext` of the recovery handshake:
/// the built-in prime checks and the pinned `PublicRsaKeySharedMain` key.
struct RecoveryContext {
    rsa: RecoveryRsa,
}

impl AuthKeyHandshakeContext for RecoveryContext {
    fn get_dh_callback(&self) -> &dyn DhCallback {
        static CALLBACK: RecoveryPrimeCallback = RecoveryPrimeCallback;
        &CALLBACK
    }

    fn get_public_rsa_key_interface(&mut self) -> &mut dyn PublicRsaKeyInterface {
        &mut self.rsa
    }
}

/// The `PublicRsaKeySharedMain` keys, `create(is_test)`
/// (net/PublicRsaKeySharedMain.cpp:26-52).
fn recovery_rsa(is_test: bool) -> RecoveryRsa {
    // The test key (net/PublicRsaKeySharedMain.cpp:26-37).
    const TEST_KEY: &str = "-----BEGIN RSA PUBLIC KEY-----\n\
MIIBCgKCAQEAyMEdY1aR+sCR3ZSJrtztKTKqigvO/vBfqACJLZtS7QMgCGXJ6XIR\n\
yy7mx66W0/sOFa7/1mAZtEoIokDP3ShoqF4fVNb6XeqgQfaUHd8wJpDWHcR2OFwv\n\
plUUI1PLTktZ9uW2WE23b+ixNwJjJGwBDJPQEQFBE+vfmH0JP503wr5INS1poWg/\n\
j25sIWeYPHYeOrFp/eXaqhISP6G+q2IeTaWTXpwZj4LzXq5YOpk4bYEQ6mvRq7D1\n\
aHWfYmlEGepfaYR8Q0YqvvhYtMte3ITnuSJs171+GDqpdKcSwHnd6FudwGO4pcCO\n\
j4WcDuXc2CTHgH8gFTNhp/Y8/SpDOhvn9QIDAQAB\n\
-----END RSA PUBLIC KEY-----";
    // The production key (net/PublicRsaKeySharedMain.cpp:40-52).
    const MAIN_KEY: &str = "-----BEGIN RSA PUBLIC KEY-----\n\
MIIBCgKCAQEA6LszBcC1LGzyr992NzE0ieY+BSaOW622Aa9Bd4ZHLl+TuFQ4lo4g\n\
5nKaMBwK/BIb9xUfg0Q29/2mgIR6Zr9krM7HjuIcCzFvDtr+L0GQjae9H0pRB2OO\n\
62cECs5HKhT5DZ98K33vmWiLowc621dQuwKWSQKjWf50XYFw42h21P2KXUGyp2y/\n\
+aEyZ+uVgLLQbRA1dEjSDZ2iGRy12Mk5gpYc397aYp438fsJoHIgJ2lgMv5h7WY9\n\
t6N/byY9Nw9p21Og3AoXSL2q/2IJ1WRUhebgAdGVMlV1fkuOQoEzR7EdpqtQD9Cs\n\
5+bfo3Nhmcyvk5ftB0WkJ9z6bNZ7yxrP8wIDAQAB\n\
-----END RSA PUBLIC KEY-----";
    let rsa = match Rsa::from_pem_public_key(if is_test { TEST_KEY } else { MAIN_KEY }) {
        tdutils::status::Result::Ok(rsa) => rsa,
        tdutils::status::Result::Err(status) => {
            panic!("the pinned PublicRsaKeySharedMain key must parse: {status}")
        }
    };
    let fingerprint = rsa.get_fingerprint();
    RecoveryRsa { rsa, fingerprint }
}

// ---- the session header (MtprotoHeader.cpp:23-110) --------------------------

/// The `MtprotoHeader::Options` subset the recovery session's header
/// carries (MtprotoHeader.h) — the fields `HeaderStorer` writes for the
/// default (non-anonymous) header the C++ `Session::start_up` puts on the
/// auth data (Session.cpp:277-281). The driver fills them from the tdlib
/// parameters (M.3.7.5).
#[derive(Clone, Debug, Default)]
pub struct RecoveryHeaderOptions {
    pub api_id: i32,
    pub device_model: String,
    pub system_version: String,
    pub application_version: String,
    pub system_language_code: String,
    pub language_pack: String,
    pub language_code: String,
    pub is_emulator: bool,
    /// The Mtproto-type proxy (`server`, `port`) — the header carries an
    /// `inputClientProxy` only for those.
    pub mtproto_proxy: Option<(String, i32)>,
    /// `options.parameters` — the raw JSON string of the tdlib parameters.
    pub parameters: String,
    pub tz_offset: f64,
}

/// `LanguagePackManager::is_custom_language_code`
/// (LanguagePackManager.cpp:126-128).
fn is_custom_language_code(language_code: &str) -> bool {
    !language_code.is_empty() && language_code.starts_with('X')
}

/// The header's language fields (MtprotoHeader.cpp:56-67): an empty pack
/// or a custom (`X`-prefixed) code clears both fields; an empty code with
/// a real pack becomes "en".
pub(crate) fn recovery_language_fields(
    language_pack: &str,
    language_code: &str,
) -> (String, String) {
    if language_pack.is_empty() || is_custom_language_code(language_code) {
        (String::new(), String::new())
    } else if language_code.is_empty() {
        (language_pack.to_string(), "en".to_string())
    } else {
        (language_pack.to_string(), language_code.to_string())
    }
}

/// The `convert_json_value` half of `get_input_json_value`
/// (JsonValue.cpp:101-134) — the JSON parser's value into the TL one. The
/// number is lenient like `to_double` (0.0 on failure); the strings need
/// no `clean_input_string` (the parser already yields valid UTF-8).
pub fn json_value_to_tl(value: &JsonValue) -> telegram_api::JSONValue {
    match value {
        JsonValue::Null => telegram_api::JSONValue::jsonNull(telegram_api::jsonNull),
        JsonValue::Boolean(value) => {
            telegram_api::JSONValue::jsonBool(telegram_api::jsonBool { value: *value })
        }
        JsonValue::Number(value) => telegram_api::JSONValue::jsonNumber(telegram_api::jsonNumber {
            value: value.parse().unwrap_or(0.0),
        }),
        JsonValue::String(value) => telegram_api::JSONValue::jsonString(telegram_api::jsonString {
            value: value.clone(),
        }),
        JsonValue::Array(values) => telegram_api::JSONValue::jsonArray(telegram_api::jsonArray {
            value: values
                .iter()
                .map(|value| Box::new(json_value_to_tl(value)))
                .collect(),
        }),
        JsonValue::Object(object) => {
            telegram_api::JSONValue::jsonObject(telegram_api::jsonObject {
                value: object
                    .iter_fields()
                    .map(|(key, value)| {
                        Box::new(telegram_api::JSONObjectValue {
                            key: key.to_string(),
                            value: Some(Box::new(json_value_to_tl(value))),
                        })
                    })
                    .collect(),
            })
        }
    }
}

/// `MtprotoHeader::gen_header(options, /*is_anonymous=*/ false)`
/// (MtprotoHeader.cpp:23-110) — the `invokeWithLayer{initConnection}`
/// header every query of the recovery session carries; the fresh auth key
/// always needs it (`AuthKey::have_header_`, AuthKey.h:120/127).
///
/// The tz rule (MtprotoHeader.cpp:81-107): the empty parameters are an
/// empty object; a non-object value keeps its shape; an object gets
/// `tz_offset` added or replaced.
pub fn gen_recovery_header(options: &RecoveryHeaderOptions) -> Result<Vec<u8>, Status> {
    let have_proxy = options.mtproto_proxy.is_some();
    let mut flags = 0;
    if have_proxy {
        flags |= 1 << 0;
    }
    // `!is_anonymous` — the params box (MtprotoHeader.cpp:43-45).
    flags |= 1 << 1;
    if options.is_emulator {
        flags |= 1 << 10;
    }

    let mut json_value = if options.parameters.is_empty() {
        telegram_api::JSONValue::jsonObject(telegram_api::jsonObject { value: Vec::new() })
    } else {
        let parsed = match json_decode(&options.parameters) {
            tdutils::status::Result::Ok(value) => value,
            tdutils::status::Result::Err(status) => return Err(status),
        };
        json_value_to_tl(&parsed)
    };
    if let telegram_api::JSONValue::jsonObject(object) = &mut json_value {
        let tz_offset = telegram_api::JSONValue::jsonNumber(telegram_api::jsonNumber {
            value: options.tz_offset,
        });
        match object
            .value
            .iter_mut()
            .find(|member| member.key == "tz_offset")
        {
            Some(member) => member.value = Some(Box::new(tz_offset)),
            None => object.value.push(Box::new(telegram_api::JSONObjectValue {
                key: "tz_offset".to_string(),
                value: Some(Box::new(tz_offset)),
            })),
        }
    }

    // The language rule (MtprotoHeader.cpp:56-67).
    let (lang_pack, lang_code) =
        recovery_language_fields(&options.language_pack, &options.language_code);

    let mut out = Vec::new();
    telegram_api::invokeWithLayer {
        layer: MTPROTO_LAYER,
        query: Some(Box::new(telegram_api::Function::initConnection(
            telegram_api::initConnection {
                flags,
                api_id: options.api_id,
                device_model: options.device_model.clone(),
                system_version: options.system_version.clone(),
                app_version: options.application_version.clone(),
                system_lang_code: options.system_language_code.clone(),
                lang_pack,
                lang_code,
                proxy: options.mtproto_proxy.as_ref().map(|(address, port)| {
                    telegram_api::InputClientProxy {
                        address: address.clone(),
                        port: *port,
                    }
                }),
                params: Some(Box::new(json_value)),
                query: Some(Box::new(telegram_api::Function::help_getConfig(
                    telegram_api::help_getConfig,
                ))),
            },
        ))),
    }
    .store_boxed(&mut out);
    Ok(out)
}

// ---- the mini-session pieces (ConfigManager.cpp:326-362 / 432-467) ----------

/// The `GetConfigActor::on_result` parse — `fetch_result<help_getConfig>`
/// (ConfigManager.cpp:432-434).
fn parse_full_config(packet: &[u8]) -> Result<telegram_api::Config, Status> {
    telegram_api::Config::parse(packet)
        .map_err(|error| Status::error_message(&format!("Failed to parse help.config: {error}")))
}

/// The `SessionCallback::request_raw_connection` try gate
/// (ConfigManager.cpp:337-357): the first two raw-connection requests go
/// to the `ConnectionCreator`; every further one is delayed forever, so
/// the session stalls until the actor timeout.
pub fn request_raw_connection_allowed(tries: usize) -> bool {
    tries <= 2
}

/// The session callback of the mini-session: it captures the answer to the
/// one query, like the C++ `SessionCallback::on_result` handing the query
/// to the dispatcher; the salt updates are persisted by the executor (the
/// no-PFS `Session::on_server_salt_updated` branch, Session.cpp:612-618).
struct FullConfigCallback {
    request_id: MessageId,
    answer: Option<Result<Vec<u8>, Status>>,
    failed: bool,
    closed: bool,
    /// `on_server_salt_updated` fired.
    salts_updated: bool,
}

impl SessionConnectionCallback for FullConfigCallback {
    fn on_connected(&mut self) {}
    fn on_closed(&mut self, _status: Status) {
        self.closed = true;
    }
    fn on_server_salt_updated(&mut self) {
        self.salts_updated = true;
    }
    fn on_server_time_difference_updated(&mut self, _force: bool) {}
    fn on_new_session_created(&mut self, _unique_id: u64, _first_message_id: MessageId) {}
    fn on_session_failed(&mut self, _status: Status) {}
    fn on_container_sent(
        &mut self,
        _container_message_id: MessageId,
        _message_ids: Vec<MessageId>,
    ) {
    }
    fn on_pong(&mut self, _ping_time: f64, _pong_time: f64, _current_time: f64) -> Status {
        Status::ok()
    }
    fn on_update(&mut self, _packet: BufferSlice) -> Status {
        Status::ok()
    }
    fn on_message_ack(&mut self, _message_id: MessageId) {}
    fn on_message_result_ok(
        &mut self,
        message_id: MessageId,
        packet: BufferSlice,
        _original_size: usize,
    ) -> Status {
        if message_id == self.request_id {
            self.answer = Some(Ok(packet.as_slice().to_vec()));
        }
        Status::ok()
    }
    fn on_message_result_error(&mut self, message_id: MessageId, code: i32, message: String) {
        if message_id == self.request_id {
            self.answer = Some(Err(Status::error(code, &message)));
        }
    }
    fn on_message_failed(&mut self, message_id: MessageId, _status: Status) {
        if message_id == self.request_id {
            self.failed = true;
        }
    }
    fn on_message_info(
        &mut self,
        _message_id: MessageId,
        _state: i32,
        _answer_message_id: MessageId,
        _answer_size: i32,
        _source: i32,
    ) {
    }
    fn on_destroy_auth_key(&mut self) -> Status {
        Status::ok()
    }
}

/// `MTPROTO_LAYER` (Version.h:13) — the layer the pinned scheme carries.
pub(crate) const MTPROTO_LAYER: i32 = 230;

/// The `GetConfigActor::set_timeout_in(10)` budget (ConfigManager.cpp:466)
/// — the whole mini-session, handshake included, fails with
/// "Timeout expired" after it.
pub const FULL_CONFIG_ACTOR_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// `get_full_config` (ConfigManager.cpp:325-497) — the unauthed
/// mini-session: the `SimpleAuthData` key store
/// (`config_recovery_auth<raw_dc_id>` in the binlog pmc, a raw handshake
/// when absent), one obfuscated-TCP connection to the option's endpoint
/// and one `help.getConfig`, answered within the 10-second actor budget.
/// Returns the config and the handshake's server time difference (the
/// `SimpleAuthData::update_server_time_difference` write the caller
/// forwards to `Global`, with the `force` flag).
///
/// Divergences from the C++ (all consequences of the `Session` actor
/// landing with the net step):
/// - The C++ session runs PFS with temp keys; the port runs the main key
///   directly (`use_pfs = false`), so salt updates persist to the pmc (the
///   no-PFS `Session::on_server_salt_updated` branch) where the C++
///   recovery session's PFS updates hit the nop `SessionCallback`.
/// - The C++ resends through the `Session`'s unbounded queue; the port
///   resends the query once (a stale session salt is the expected cause).
/// - A reconnection takes a fresh session id (the port's
///   `SessionConnection` owns its auth data by value; the C++ session id
///   survives reconnects).
/// - The `query->total_timeout_limit_ = 60 * 60 * 24` budget
///   (ConfigManager.cpp:462) has no port counterpart — the 10-second actor
///   timeout dominates it anyway.
///
/// The lifecycle texts match the C++ promise errors: "Timeout expired"
/// (`timeout_expired`) and "Failed" (a raw connection that cannot be
/// established — the `hangup_shared`-with-token-1 path).
pub async fn fetch_full_config(
    option: &RecovererDcOption,
    is_test_dc: bool,
    header_options: &RecoveryHeaderOptions,
    server_time_difference: f64,
    pmc: &mut BinlogKeyValue,
) -> Result<(telegram_api::Config, Option<(f64, bool)>), Status> {
    fetch_full_config_impl(
        option,
        is_test_dc,
        header_options,
        server_time_difference,
        pmc,
        FULL_CONFIG_ACTOR_TIMEOUT,
    )
    .await
}

/// [`fetch_full_config`] with the actor budget injected (the tests shrink
/// it).
#[cfg(not(target_family = "wasm"))]
async fn fetch_full_config_impl(
    option: &RecovererDcOption,
    is_test_dc: bool,
    header_options: &RecoveryHeaderOptions,
    server_time_difference: f64,
    pmc: &mut BinlogKeyValue,
    timeout: std::time::Duration,
) -> Result<(telegram_api::Config, Option<(f64, bool)>), Status> {
    let raw_dc_id = option.raw_dc_id;
    // The `GetConfigActor::start_up` int dc (ConfigManager.cpp:455-458):
    // `+10000` on a test instance. Wire test-DC ids (10001+) are their own
    // int dc — they cannot enter the recoverer's option set
    // (`RecovererDcOption::is_valid` caps at 1000), but a direct call like
    // the live smoke carries them.
    let int_dc_id = if raw_dc_id > MAX_RECOVERER_DC_ID {
        raw_dc_id
    } else if is_test_dc {
        raw_dc_id + 10000
    } else {
        raw_dc_id
    };

    // The `TransportType{ObfuscatedTcp, narrow_cast<int16>(raw_dc_id),
    // option_.get_secret()}` of `SessionCallback::request_raw_connection`
    // (ConfigManager.cpp:349-352).
    let proxy_secret = if option.secret.is_empty() {
        ProxySecret::default()
    } else {
        match ProxySecret::from_binary(&option.secret, false) {
            tdutils::status::Result::Ok(secret) => secret,
            tdutils::status::Result::Err(status) => return Err(status),
        }
    };
    let transport_type = TransportType::new(
        TransportTypeEnum::ObfuscatedTcp,
        raw_dc_id as i16,
        proxy_secret,
    );

    // The header every query of the session carries (`Session::start_up` →
    // `AuthData::set_header`, Session.cpp:277-281).
    let header = gen_recovery_header(header_options)?;

    // `SimpleAuthData::get_auth_key` — the persisted key, or a fresh
    // handshake on the first connection.
    let mut key = load_recovery_auth_key(pmc, raw_dc_id);
    let mut auth_time_difference: Option<(f64, bool)> = None;

    // The one query: `NetQueryCreator::create_unauth(help_getConfig())`
    // (ConfigManager.cpp:460) — a bare function; the invokeWithLayer
    // wrapping rides the session header, like the C++.
    let mut query = Vec::new();
    telegram_api::help_getConfig.store_boxed(&mut query);

    let deadline = tokio::time::Instant::now() + timeout;
    // `SessionCallback::request_raw_connection_cnt_`
    // (ConfigManager.cpp:336-357).
    let mut raw_connection_tries = 0usize;

    'establish: loop {
        // `SessionCallback::request_raw_connection` — the two-try gate,
        // then delay forever (park until the actor timeout).
        raw_connection_tries += 1;
        if !request_raw_connection_allowed(raw_connection_tries) {
            tokio::time::sleep_until(deadline).await;
            return Err(Status::error_message("Timeout expired"));
        }
        // `ConnectionCreator::request_raw_connection_by_ip` — a raw
        // connection that cannot be established kills the session
        // (`GetConfigActor::hangup_shared` with token 1 → "Failed").
        let stream =
            match TcpStream::connect((option.ip_address.as_str(), option.port as u16)).await {
                Ok(stream) => stream,
                Err(_) => return Err(Status::error_message("Failed")),
            };

        // `Session::start_up`'s auth data (Session.cpp:249-267): the main
        // key, the persisted salts (:253), the server time difference and
        // a fresh session id.
        let mut auth_data = AuthData::new();
        auth_data.set_use_pfs(false);
        let connection = if key.is_none() {
            // `Session::on_handshake_ready` (Session.cpp:1408-1424): the
            // fresh key and salt, persisted through `SimpleAuthData`
            // (Session.cpp:909/965 and the no-PFS :612-618).
            let mut context = RecoveryContext {
                rsa: recovery_rsa(is_test_dc),
            };
            let mut handshake = AuthKeyHandshake::new(int_dc_id, 0);
            let raw_connection = match run_handshake(
                stream,
                transport_type.clone(),
                &mut context,
                deadline
                    .saturating_duration_since(tokio::time::Instant::now())
                    .as_secs_f64(),
                &mut handshake,
            )
            .await
            {
                tdutils::status::Result::Ok(connection) => connection,
                tdutils::status::Result::Err(status) => return Err(status),
            };
            let auth_key = handshake.release_auth_key();
            auth_time_difference = Some((handshake.get_server_time_diff(), true));
            auth_data.set_main_auth_key(auth_key.clone());
            auth_data.reset_server_time_difference(server_time_difference);
            auth_data.set_server_salt(handshake.get_server_salt() as i64, tdutils::time::now());
            store_recovery_auth_key(pmc, raw_dc_id, &auth_key);
            let salts = auth_data.get_future_salts();
            pmc.set_raw(
                recovery_salt_key(raw_dc_id).as_bytes(),
                &store_recovery_salts(&salts),
            );
            key = Some(auth_key);
            raw_connection
        } else {
            auth_data.set_main_auth_key(key.clone().expect("the key is present"));
            auth_data.reset_server_time_difference(server_time_difference);
            auth_data.set_future_salts(load_recovery_salts(pmc, raw_dc_id), tdutils::time::now());
            RawConnection::new(stream, transport_type.clone(), None)
        };
        auth_data.set_header(header.clone());
        let mut session_id = 0u64;
        while session_id == 0 {
            session_id = random::secure_uint64();
        }
        auth_data.set_session_id(session_id);
        auth_data.set_auth_flag(true);
        let mut connection = SessionConnection::new(SessionMode::Tcp, connection, auth_data);

        // `send_closure(session_, &Session::send, query)` — one query, one
        // in-flight attempt plus the stale-salt resend, under the actor
        // timeout.
        let mut callback = FullConfigCallback {
            request_id: MessageId::default(),
            answer: None,
            failed: false,
            closed: false,
            salts_updated: false,
        };
        let mut sent = false;
        let mut resent_once = false;
        loop {
            if let Some(answer) = callback.answer.take() {
                let packet = answer?;
                let config = parse_full_config(&packet)?;
                return Ok((config, auth_time_difference));
            }
            if callback.salts_updated {
                callback.salts_updated = false;
                // The no-PFS `Session::on_server_salt_updated` persist
                // (Session.cpp:612-618).
                let salts = connection.auth_data().get_future_salts();
                pmc.set_raw(
                    recovery_salt_key(raw_dc_id).as_bytes(),
                    &store_recovery_salts(&salts),
                );
            }
            if callback.closed {
                // The connection died — `Session::on_closed` requests a
                // new raw connection through the gate.
                continue 'establish;
            }
            if callback.failed && !resent_once {
                resent_once = true;
                callback.failed = false;
                sent = false;
            }
            if !sent {
                callback.request_id = connection.send_query(
                    BufferSlice::from_slice_bytes(&query),
                    false,
                    MessageId::default(),
                    Vec::new(),
                    false,
                );
                sent = true;
            }
            connection.flush(&mut callback).await;
            if callback.answer.is_some() || callback.closed {
                continue;
            }
            tokio::select! {
                readable = connection.raw_connection().stream().readable() => {
                    if readable.is_err() {
                        continue 'establish;
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    return Err(Status::error_message("Timeout expired"));
                }
            }
        }
    }
}

/// wasm: the direct-DC config leg needs raw TCP, which browsers cannot
/// open; fail fast (the browser transport is the follow-on slice).
#[cfg(target_family = "wasm")]
async fn fetch_full_config_impl(
    _option: &RecovererDcOption,
    _is_test_dc: bool,
    _header_options: &RecoveryHeaderOptions,
    _server_time_difference: f64,
    _pmc: &mut BinlogKeyValue,
    _timeout: std::time::Duration,
) -> Result<(telegram_api::Config, Option<(f64, bool)>), Status> {
    Err(Status::error_message(
        "Direct-DC config fetch is unsupported on this platform",
    ))
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
            // A fixed clock: the loop's decision table is deterministic
            // against it (tests move it by rebuilding the env).
            now: 10_000.0,
            now_cached: 10_000.0,
            is_high_loaded: false,
            is_test_dc: false,
            prefer_ipv6: false,
            dc_txt_domain_name: String::new(),
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
        assert_eq!(
            actions
                .connection_creator_dc_options
                .as_ref()
                .map(|options| options.len()),
            Some(1)
        );
        assert!(
            recoverer.full_config_expires_at >= ok_before + 120.0
                && recoverer.full_config_expires_at <= tdutils::time::now() + 180.0
        );

        // A failure clears the config and arms the 5..7 s window (blocked,
        // online).
        let fail_before = tdutils::time::now();
        let actions =
            recoverer.on_full_config(Err(Status::error_message("Timeout expired")), &environment);
        assert!(actions.connection_creator_dc_options.is_none());
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
        let mut prefixed = Vec::new();
        crate::store_parse::store_log_event_version(&mut prefixed);
        prefixed.extend_from_slice(&stored.store());
        pmc.set_raw(b"dc_options_update", &prefixed);

        let mut recoverer = ConfigRecoverer::new();
        let environment = env(true);
        let saved = load_dc_options_update(&pmc);
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].ip_address, "10.0.0.1");
        recoverer.on_dc_options_update(saved, &environment);

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

    /// A recoverer dc option built directly (the ip is a valid dotted
    /// quad, so [`RecovererDcOption::is_valid`] holds).
    fn dc_option(dc: i32, ip: &str) -> RecovererDcOption {
        RecovererDcOption {
            raw_dc_id: dc,
            ip_address: ip.to_string(),
            port: 443,
            secret: Vec::new(),
        }
    }

    #[test]
    fn need_simple_config_matches_the_cpp_table() {
        // (is_connecting, since offset, blocking, expires offset,
        //  query_active) -> fetch. Offsets run from the injected
        //  now_cached = 10_000; the blocked connecting limit is 5 s, the
        //  free one 20 s (cpp:707-709, 721-743).
        let rows: Vec<(bool, f64, bool, f64, bool, bool)> = vec![
            // Not connecting: never, however stale everything is.
            (false, -600.0, true, -600.0, false, false),
            // Connecting, but younger than the limit: not yet a problem.
            (true, -1.0, true, -600.0, false, false),
            (true, -19.0, false, -600.0, false, false),
            // Connecting problem + invalid config: fetch — unless a query
            // is already in flight.
            (true, -6.0, true, -600.0, false, true),
            (true, -21.0, false, -600.0, false, true),
            (true, -6.0, true, -600.0, true, false),
            // A valid config: no refetch.
            (true, -6.0, true, 120.0, false, false),
            (true, -21.0, false, 120.0, false, false),
        ];
        for (is_connecting, since, blocking, expires, query_active, want) in rows {
            let environment = env(blocking);
            let mut recoverer = ConfigRecoverer::new();
            recoverer.is_connecting = is_connecting;
            recoverer.connecting_since = 10_000.0 + since;
            recoverer.simple_config_expires_at = 10_000.0 + expires;
            recoverer.simple_config_query_active = query_active;
            let actions = recoverer.loop_(&environment);
            assert_eq!(
                actions.fetch_simple_config.is_some(),
                want,
                "row {is_connecting}/{since}/{blocking}/{expires}/{query_active}"
            );
            assert_eq!(
                recoverer.ref_cnt,
                if want { 2 } else { 1 },
                "each dispatch takes one query share"
            );
        }
    }

    #[test]
    fn need_full_config_matches_the_cpp_table() {
        // With dc options present (cpp:744-748): the full config is
        // refetched only on a connecting problem with an invalid config,
        // no query in flight, and dc options at least 5 s (blocked) /
        // 10 s (free) old.
        // (is_connecting, since offset, blocking, full expires offset,
        //  query_active, dc_options_at offset) -> fetch.
        let rows: Vec<(bool, f64, bool, f64, bool, f64, bool)> = vec![
            // A still-valid full config: never.
            (true, -6.0, true, 120.0, false, -600.0, false),
            // Not a connecting problem: never.
            (false, -1.0, true, -600.0, false, -600.0, false),
            // Everything stale: fetch.
            (true, -6.0, true, -600.0, false, -600.0, true),
            (true, -21.0, false, -600.0, false, -600.0, true),
            // A query is already in flight: never.
            (true, -6.0, true, -600.0, true, -600.0, false),
            // The dc options are too fresh (the 5/10 s gate): never.
            (true, -6.0, true, -600.0, false, -2.0, false),
            (true, -21.0, false, -600.0, false, -9.0, false),
        ];
        for (is_connecting, since, blocking, full_expires, query_active, dc_at, want) in rows {
            let environment = env(blocking);
            let mut recoverer = ConfigRecoverer::new();
            recoverer.is_connecting = is_connecting;
            recoverer.connecting_since = 10_000.0 + since;
            recoverer.full_config_expires_at = 10_000.0 + full_expires;
            recoverer.full_config_query_active = query_active;
            recoverer.dc_options_update = vec![dc_option(1, "1.1.1.1"), dc_option(2, "2.2.2.2")];
            recoverer.update_dc_options();
            recoverer.dc_options_at = 10_000.0 + dc_at;

            let actions = recoverer.loop_(&environment);
            assert_eq!(
                actions.fetch_full_config.is_some(),
                want,
                "row {is_connecting}/{since}/{blocking}/{full_expires}/{query_active}/{dc_at}"
            );
            // The simple side of the same row: a connecting problem with
            // the (never-set, hence invalid) empty simple config refetches
            // too, taking its own share.
            let connecting_limit = if blocking { 5.0 } else { 20.0 };
            let problem = is_connecting && since <= -connecting_limit;
            assert_eq!(actions.fetch_simple_config.is_some(), problem);
            assert_eq!(recoverer.ref_cnt, 1 + u32::from(problem) + u32::from(want));
        }
    }

    #[test]
    fn full_config_fetches_rotate_the_dc_options() {
        // cpp:783-790 — the options are handed out round-robin, starting
        // at the current `dc_options_i_`.
        let environment = env(true);
        let mut recoverer = ConfigRecoverer::new();
        recoverer.is_connecting = true;
        recoverer.connecting_since = 9_000.0;
        recoverer.full_config_expires_at = 9_000.0;
        recoverer.dc_options_update = vec![dc_option(1, "1.1.1.1"), dc_option(2, "2.2.2.2")];
        recoverer.update_dc_options();
        recoverer.dc_options_at = 9_000.0;
        recoverer.dc_options_i = 1;

        let actions = recoverer.loop_(&environment);
        assert_eq!(
            actions
                .fetch_full_config
                .as_ref()
                .map(|fetch| fetch.option.ip_address.as_str()),
            Some("2.2.2.2")
        );
        assert_eq!(recoverer.dc_options_i, 0, "wrapped modulo the set size");

        // After the delivery frees the slot, the next fetch takes the
        // first option.
        recoverer.full_config_query_active = false;
        let actions = recoverer.loop_(&environment);
        assert_eq!(
            actions
                .fetch_full_config
                .as_ref()
                .map(|fetch| fetch.option.ip_address.as_str()),
            Some("1.1.1.1")
        );
        assert_eq!(recoverer.dc_options_i, 1);
    }

    #[test]
    fn wakeup_is_the_earliest_future_deadline() {
        // `Timestamp::relax` keeps the EARLIEST future deadline (the
        // m37-draft folded the latest — a real divergence from the C++).
        let environment = env(true);
        let mut recoverer = ConfigRecoverer::new();
        // A connecting problem (the limit long expired), a valid simple
        // config at +120, an invalid full config, and the dc gate due at
        // +3: the wakeup is the gate.
        recoverer.is_connecting = true;
        recoverer.connecting_since = 9_990.0;
        recoverer.simple_config_expires_at = 10_120.0;
        recoverer.full_config_expires_at = 9_900.0;
        recoverer.dc_options_update = vec![dc_option(1, "1.1.1.1")];
        recoverer.update_dc_options();
        recoverer.dc_options_at = 9_998.0;
        let actions = recoverer.loop_(&environment);
        assert_eq!(actions.wakeup_at, Some(10_003.0));
        assert!(actions.fetch_full_config.is_none(), "the gate holds it");

        // The gate passed: the fetch fires and the wakeup falls back to
        // the simple-config expiry.
        recoverer.dc_options_at = 9_994.0;
        let actions = recoverer.loop_(&environment);
        assert!(actions.fetch_full_config.is_some());
        assert_eq!(actions.wakeup_at, Some(10_120.0));

        // Every deadline expired: nothing to re-arm (the C++ leaves the
        // previously set timeout running).
        recoverer.simple_config_expires_at = 9_500.0;
        recoverer.full_config_query_active = true;
        let actions = recoverer.loop_(&environment);
        assert_eq!(actions.wakeup_at, None);
    }

    #[test]
    fn high_load_skips_the_loop_for_a_while() {
        // cpp:715-719 — everything stale, but under high load the machine
        // only re-arms itself 200..300 s out and fetches nothing.
        let mut environment = env(true);
        environment.is_high_loaded = true;
        let mut recoverer = ConfigRecoverer::new();
        recoverer.is_connecting = true;
        recoverer.connecting_since = 9_000.0;
        recoverer.simple_config_expires_at = 9_000.0;
        let actions = recoverer.loop_(&environment);
        assert!(actions.fetch_simple_config.is_none());
        assert!(actions.fetch_full_config.is_none());
        let wakeup = actions.wakeup_at.expect("the skip re-arms itself");
        assert!((10_200.0..=10_300.0).contains(&wakeup), "wakeup {wakeup}");
        assert_eq!(recoverer.ref_cnt, 1, "no share is taken");
    }

    #[test]
    fn a_closed_recoverer_loops_into_nothing() {
        // cpp:712-714 — after the close, the loop arms nothing and issues
        // nothing, however stale the state.
        let environment = env(true);
        let mut recoverer = ConfigRecoverer::new();
        recoverer.is_connecting = true;
        recoverer.connecting_since = 9_000.0;
        recoverer.simple_config_expires_at = 9_000.0;
        assert!(recoverer.hangup());

        let actions = recoverer.loop_(&environment);
        assert_eq!(actions.wakeup_at, None);
        assert!(actions.fetch_simple_config.is_none());
        assert!(actions.fetch_full_config.is_none());

        // The state callbacks still run their bodies; the loop tail is the
        // same no-op.
        let actions = recoverer.on_connecting(true, &environment);
        assert_eq!(actions.wakeup_at, None);
    }

    #[test]
    fn hangup_folds_the_in_flight_query_releases() {
        // cpp:693-698 — closing cancels both queries and releases their
        // shares together with the parent's.
        let environment = env(true);
        let mut recoverer = ConfigRecoverer::new();
        recoverer.is_connecting = true;
        recoverer.connecting_since = 9_000.0;
        recoverer.simple_config_expires_at = 9_000.0;
        recoverer.dc_options_update = vec![dc_option(1, "1.1.1.1"), dc_option(2, "2.2.2.2")];
        recoverer.update_dc_options();
        recoverer.dc_options_at = 9_000.0;
        recoverer.full_config_expires_at = 9_000.0;
        let actions = recoverer.loop_(&environment);
        assert!(actions.fetch_simple_config.is_some() && actions.fetch_full_config.is_some());
        assert_eq!(recoverer.ref_cnt, 3, "parent + one share per query");

        assert!(recoverer.hangup());
        assert_eq!(recoverer.ref_cnt, 0);
        assert!(!recoverer.simple_config_query_active);
        assert!(!recoverer.full_config_query_active);
    }

    #[test]
    fn query_shares_release_one_by_one() {
        // cpp:689-692 — a delivered or failed query releases exactly its
        // own share; the parent's keeps the recoverer alive.
        //
        // The injected clock anchors at the live monotonic clock +300 s:
        // past the 5 s dc gate, below the success expiry the Ok delivery
        // arms (`time::now()` is process-monotonic, not wall-clock), so
        // the fresh full config reads valid and the loop tail re-issues
        // nothing.
        let clock = tdutils::time::now();
        let mut environment = env(true);
        environment.now = clock + 300.0;
        environment.now_cached = clock + 300.0;
        let mut recoverer = ConfigRecoverer::new();
        recoverer.is_connecting = true;
        recoverer.connecting_since = clock - 9_000.0;
        recoverer.simple_config_expires_at = clock - 1_000.0;
        recoverer.dc_options_update = vec![dc_option(1, "1.1.1.1")];
        recoverer.update_dc_options();
        recoverer.dc_options_at = clock - 600.0;
        recoverer.full_config_expires_at = clock - 1_000.0;
        recoverer.loop_(&environment);
        assert_eq!(recoverer.ref_cnt, 3);

        // The full-config delivery succeeds, so its fresh expiry holds
        // and the loop tail issues no replacement fetch.
        recoverer.on_full_config(Ok(telegram_api::Config::default()), &environment);
        assert!(!recoverer.hangup_shared(), "the parent share remains");
        assert_eq!(recoverer.ref_cnt, 2);

        // The simple-config holder dies undelivered: the slot clears with
        // the holder, then the share releases.
        recoverer.simple_config_query_active = false;
        assert!(!recoverer.hangup_shared());
        assert_eq!(recoverer.ref_cnt, 1);

        // The last share is the parent's close.
        assert!(recoverer.hangup());
    }

    #[test]
    fn going_online_expires_only_empty_configs() {
        // cpp:527-533.
        let environment = env(true);
        let mut recoverer = ConfigRecoverer::new();
        // A simple config WITH options keeps its expiry; an absent full
        // config is expired at once.
        recoverer.simple_config = vec![dc_option(2, "9.9.9.9")];
        recoverer.simple_config_expires_at = 10_050.0;
        recoverer.full_config_expires_at = 10_050.0;
        recoverer.on_online(true, &environment);
        assert_eq!(recoverer.simple_config_expires_at, 10_050.0);
        assert_eq!(recoverer.full_config_expires_at, 0.0);

        // An empty simple config is expired too.
        let mut recoverer = ConfigRecoverer::new();
        recoverer.simple_config_expires_at = 10_050.0;
        recoverer.on_online(true, &environment);
        assert_eq!(recoverer.simple_config_expires_at, 0.0);

        // An unchanged flag runs no loop: the default actions arm nothing
        // (cpp:522-524 returns before loop()).
        let actions = recoverer.on_online(true, &environment);
        assert_eq!(actions.wakeup_at, None);
        assert!(actions.fetch_simple_config.is_none());
    }

    #[test]
    fn on_network_stamp_keeps_the_generation_quirk() {
        // cpp:512-518 — `network_generation_` is never assigned, so any
        // generation but the initial 0 re-stamps on EVERY call while the
        // network is up; generation 0 never does, nor a down network.
        let mut environment = env(true);
        let mut recoverer = ConfigRecoverer::new();
        recoverer.on_network(true, 7, &environment);
        assert_eq!(recoverer.has_network_since(), 10_000.0);

        environment.now_cached = 11_000.0;
        recoverer.on_network(true, 7, &environment);
        assert_eq!(
            recoverer.has_network_since(),
            11_000.0,
            "the same generation still stamps"
        );

        environment.now_cached = 12_000.0;
        recoverer.on_network(false, 7, &environment);
        assert_eq!(recoverer.has_network_since(), 11_000.0);
        recoverer.on_network(true, 0, &environment);
        assert_eq!(recoverer.has_network_since(), 11_000.0);
    }

    #[test]
    fn connecting_since_stamps_on_the_rising_edge_only() {
        // cpp:539-541.
        let mut environment = env(true);
        let mut recoverer = ConfigRecoverer::new();
        recoverer.on_connecting(true, &environment);
        assert_eq!(recoverer.connecting_since, 10_000.0);

        environment.now_cached = 11_000.0;
        recoverer.on_connecting(true, &environment);
        assert_eq!(
            recoverer.connecting_since, 10_000.0,
            "staying true keeps it"
        );
        recoverer.on_connecting(false, &environment);
        assert_eq!(recoverer.connecting_since, 10_000.0);

        environment.now_cached = 12_000.0;
        recoverer.on_connecting(true, &environment);
        assert_eq!(recoverer.connecting_since, 12_000.0, "re-rising re-stamps");
    }

    #[test]
    fn the_state_callback_forwards_only_a_plain_connecting_state() {
        // cpp:804-807 — ConnectingToProxy is deliberately not a connecting
        // problem.
        let environment = env(true);
        for (state, is_connecting) in [
            (RecovererConnectionState::WaitingForNetwork, false),
            (RecovererConnectionState::ConnectingToProxy, false),
            (RecovererConnectionState::Connecting, true),
            (RecovererConnectionState::Updating, false),
            (RecovererConnectionState::Ready, false),
            (RecovererConnectionState::Empty, false),
        ] {
            let mut recoverer = ConfigRecoverer::new();
            recoverer.on_connection_state(state, &environment);
            assert_eq!(recoverer.is_connecting, is_connecting, "{state:?}");
        }
    }

    #[test]
    fn the_network_callback_treats_every_net_type_but_none_as_up() {
        // cpp:808-811.
        let environment = env(true);
        for (net_type, has_network) in [
            (RecovererNetType::Other, true),
            (RecovererNetType::WiFi, true),
            (RecovererNetType::Mobile, true),
            (RecovererNetType::MobileRoaming, true),
            (RecovererNetType::Unknown, true),
            (RecovererNetType::None, false),
        ] {
            let mut recoverer = ConfigRecoverer::new();
            recoverer.on_net_type(net_type, 1, &environment);
            assert_eq!(recoverer.has_network, has_network, "{net_type:?}");
            assert_eq!(
                recoverer.has_network_since() == 10_000.0,
                has_network,
                "only an up network stamps {net_type:?}"
            );
        }
    }

    #[test]
    fn an_expired_simple_config_is_cleared_and_rebuilt() {
        // cpp:739-742 — an invalid simple config with options is dropped
        // and the merged set rebuilt (the update part survives, the
        // rotation resets).
        let environment = env(true);
        let mut recoverer = ConfigRecoverer::new();
        recoverer.dc_options_update = vec![dc_option(1, "1.1.1.1")];
        recoverer.simple_config = vec![dc_option(2, "2.2.2.2")];
        recoverer.update_dc_options();
        assert_eq!(recoverer.dc_options().len(), 2);
        recoverer.dc_options_i = 1;
        recoverer.simple_config_expires_at = 9_000.0;
        recoverer.is_connecting = true;
        recoverer.connecting_since = 9_000.0;

        let actions = recoverer.loop_(&environment);
        assert!(
            actions.fetch_simple_config.is_some(),
            "the clear goes with a refetch"
        );
        assert_eq!(recoverer.dc_options().len(), 1);
        assert_eq!(recoverer.dc_options()[0].ip_address, "1.1.1.1");
        assert_eq!(recoverer.dc_options_i, 0, "the rebuild resets the rotation");
    }

    // ---- the fetch orchestration (M.3.7.3) ---------------------------------

    #[test]
    fn simple_config_service_rotation_matches_the_cpp() {
        // The `switch (simple_config_turn_ % 9)` (ConfigManager.cpp:756-773):
        // google on {0, 3, 7}, mozilla on {1, 4, 6}, azure on 5,
        // firestore on 2, realtime on 8 — checked across two full rounds.
        let expected = [
            SimpleConfigService::GoogleDns,         // 0
            SimpleConfigService::MozillaDns,        // 1
            SimpleConfigService::FirebaseFirestore, // 2
            SimpleConfigService::GoogleDns,         // 3
            SimpleConfigService::MozillaDns,        // 4
            SimpleConfigService::Azure,             // 5
            SimpleConfigService::MozillaDns,        // 6
            SimpleConfigService::GoogleDns,         // 7
            SimpleConfigService::FirebaseRealtime,  // 8
        ];
        for turn in 0..18u32 {
            assert_eq!(
                simple_config_service(turn),
                expected[(turn % 9) as usize],
                "turn {turn}"
            );
        }
    }

    #[test]
    fn the_loop_rotates_the_fetchers_with_pinned_requests() {
        // One full rotation issued turn by turn: the loop hands out the
        // fetch with its pinned request, the slot guards a duplicate, and
        // the delivered failure releases the slot for the next turn. The
        // injected clock anchors at the live monotonic clock, so the
        // 5..7 s failure expiry the delivery arms stays valid and the
        // loop tail refetches nothing (each turn is force-expired below,
        // like a fired wakeup).
        let clock = tdutils::time::now();
        let mut environment = env(true);
        environment.now = clock;
        environment.now_cached = clock;
        let mut recoverer = ConfigRecoverer::new();
        recoverer.is_connecting = true;
        recoverer.connecting_since = clock - 6.0; // past the 5 s blocked limit

        let expected: [(SimpleConfigService, &str, &str); 9] = [
            (
                SimpleConfigService::GoogleDns,
                "https://dns.google/resolve?name=apv3.stel.com&type=TXT",
                "dns.google",
            ),
            (
                SimpleConfigService::MozillaDns,
                "https://mozilla.cloudflare-dns.com/dns-query?name=apv3.stel.com&type=TXT",
                "mozilla.cloudflare-dns.com",
            ),
            (
                SimpleConfigService::FirebaseFirestore,
                "https://www.google.com/v1/projects/reserve-5a846/databases/(default)/documents/ipconfig/v3",
                "firestore.googleapis.com",
            ),
            (
                SimpleConfigService::GoogleDns,
                "https://dns.google/resolve?name=apv3.stel.com&type=TXT",
                "dns.google",
            ),
            (
                SimpleConfigService::MozillaDns,
                "https://mozilla.cloudflare-dns.com/dns-query?name=apv3.stel.com&type=TXT",
                "mozilla.cloudflare-dns.com",
            ),
            (
                SimpleConfigService::Azure,
                "https://software-download.microsoft.com/prodv2/config.txt",
                "tcdnb.azureedge.net",
            ),
            (
                SimpleConfigService::MozillaDns,
                "https://mozilla.cloudflare-dns.com/dns-query?name=apv3.stel.com&type=TXT",
                "mozilla.cloudflare-dns.com",
            ),
            (
                SimpleConfigService::GoogleDns,
                "https://dns.google/resolve?name=apv3.stel.com&type=TXT",
                "dns.google",
            ),
            (
                SimpleConfigService::FirebaseRealtime,
                "https://reserve-5a846.firebaseio.com/ipconfigv3.json",
                "reserve-5a846.firebaseio.com",
            ),
        ];
        for (turn, (service, url, host)) in expected.iter().enumerate() {
            recoverer.simple_config_expires_at = clock - 1.0; // the fired wakeup
            let actions = recoverer.loop_(&environment);
            let fetch = actions.fetch_simple_config.expect("each turn dispatches");
            assert_eq!(fetch.turn as usize, turn);
            assert_eq!(fetch.service, *service, "turn {turn}");
            assert_eq!(fetch.request.url, *url, "turn {turn}");
            assert_eq!(fetch.request.host, *host, "turn {turn}");

            // The in-flight fetch is not duplicated.
            assert!(recoverer.loop_(&environment).fetch_simple_config.is_none());

            // The delivered failure releases the slot; its failure expiry
            // (real clock + 5..7 s) stays valid against the anchored
            // clock, so nothing refetches until the next force-expire.
            let actions = recoverer
                .on_simple_config(Err(Status::error_message("HTTP error: 500")), &environment);
            assert!(actions.fetch_simple_config.is_none());
        }
        assert_eq!(recoverer.simple_config_turn, 9);
    }

    #[test]
    fn the_fetch_arguments_follow_the_env() {
        // cpp:774-776 — the `dc_txt_domain_name` option, `is_test_dc()`
        // and `prefer_ipv6` ride every dispatch: a custom TXT domain wins
        // over the dns defaults, the test dc flips the azure flavor and
        // the default dns domain.
        let mut environment = env(true);
        environment.is_test_dc = true;
        environment.dc_txt_domain_name = "custom.example.com".to_string();
        let mut recoverer = ConfigRecoverer::new();
        recoverer.is_connecting = true;
        recoverer.connecting_since = environment.now_cached - 6.0;
        recoverer.simple_config_expires_at = 0.0;

        // Turn 0 (google): the custom domain, url-encoded into the query.
        let actions = recoverer.loop_(&environment);
        let fetch = actions.fetch_simple_config.expect("turn 0");
        assert_eq!(
            fetch.request.url,
            "https://dns.google/resolve?name=custom.example.com&type=TXT"
        );

        // Turn 1 (mozilla): the custom domain still wins — the test
        // default only applies when the option is empty.
        recoverer.simple_config_query_active = false;
        recoverer.simple_config_expires_at = 0.0;
        recoverer.simple_config_turn = 1;
        let actions = recoverer.loop_(&environment);
        let fetch = actions.fetch_simple_config.expect("turn 1");
        assert_eq!(
            fetch.request.url,
            "https://mozilla.cloudflare-dns.com/dns-query?name=custom.example.com&type=TXT"
        );

        // Without the option, the test default domain is `tapv3.stel.com`.
        let mut environment = env(true);
        environment.is_test_dc = true;
        let mut recoverer = ConfigRecoverer::new();
        recoverer.is_connecting = true;
        recoverer.connecting_since = environment.now_cached - 6.0;
        recoverer.simple_config_expires_at = 0.0;
        recoverer.simple_config_turn = 1;
        let actions = recoverer.loop_(&environment);
        let fetch = actions.fetch_simple_config.expect("turn 1, default domain");
        assert_eq!(
            fetch.request.url,
            "https://mozilla.cloudflare-dns.com/dns-query?name=tapv3.stel.com&type=TXT"
        );

        // Turn 5 (azure): the test flavor of the static config.
        recoverer.simple_config_query_active = false;
        recoverer.simple_config_expires_at = 0.0;
        recoverer.simple_config_turn = 5;
        let actions = recoverer.loop_(&environment);
        let fetch = actions.fetch_simple_config.expect("turn 5");
        assert_eq!(
            fetch.request.url,
            "https://software-download.microsoft.com/testv2/config.txt"
        );
    }

    #[test]
    fn dns_time_difference_alternates_and_survives_errors() {
        // cpp:578-583 — the http date feeds `update_dns_time_difference`
        // while `date_option_i_` is 0 and whenever the config failed; the
        // config date takes the odd successful turns; an outer error
        // (both halves failed) updates nothing; the index advances
        // either way. The fixed env clock makes every difference exact.
        let environment = env(true); // now == now_cached == 10_000
        let mut recoverer = ConfigRecoverer::new();
        let config = |date: i32| SimpleConfigResult {
            r_http_date: Ok(50_000),
            r_config: Ok(telegram_api::help_ConfigSimple {
                date,
                expires: 200_000,
                rules: Vec::new(),
            }),
        };

        // Turn 0 (i = 0): the http date wins over the config date.
        let actions = recoverer.on_simple_config(Ok(config(99_000)), &environment);
        assert_eq!(actions.dns_time_difference, Some(40_000.0));
        // Turn 1: the config date.
        let actions = recoverer.on_simple_config(Ok(config(99_000)), &environment);
        assert_eq!(actions.dns_time_difference, Some(89_000.0));
        // Turn 2: back to the http date.
        let actions = recoverer.on_simple_config(Ok(config(99_000)), &environment);
        assert_eq!(actions.dns_time_difference, Some(40_000.0));

        // A failed config keeps the http date at ANY alternation index
        // (the index is 1 here).
        let actions = recoverer.on_simple_config(
            Ok(SimpleConfigResult {
                r_http_date: Ok(50_000),
                r_config: Err(Status::error_message("Decode failed")),
            }),
            &environment,
        );
        assert_eq!(actions.dns_time_difference, Some(40_000.0));

        // A broken http date falls through to the config date — even at
        // index 0 (the first conjunct already fails).
        let mut fresh = ConfigRecoverer::new();
        let actions = fresh.on_simple_config(
            Ok(SimpleConfigResult {
                r_http_date: Err(Status::error_message("Timezone must be GMT")),
                r_config: Ok(telegram_api::help_ConfigSimple {
                    date: 77_000,
                    expires: 200_000,
                    rules: Vec::new(),
                }),
            }),
            &environment,
        );
        assert_eq!(actions.dns_time_difference, Some(67_000.0));

        // An outer error updates nothing, but still advances the index.
        let actions =
            recoverer.on_simple_config(Err(Status::error_message("HTTP error: 500")), &environment);
        assert_eq!(actions.dns_time_difference, None);
        assert_eq!(
            recoverer.date_option_i, 1,
            "five deliveries leave the index at 1"
        );
    }

    #[test]
    fn on_simple_config_resets_the_slots_and_rebuilds() {
        // cpp:566-588 — the delivery releases the query slot, resets the
        // dc-option rotation, rebuilds the merged options (the update part
        // first), and arms the wakeup instead of an immediate refetch.
        // The injected clock anchors at the live monotonic clock + 60 s:
        // past the 5 s dc gate, below every success expiry (the offline
        // one included), so the fresh config reads valid.
        let clock = tdutils::time::now();
        let mut environment = env(true);
        environment.now = clock + 60.0;
        environment.now_cached = clock + 60.0;
        let mut recoverer = ConfigRecoverer::new();
        recoverer.dc_options_update = vec![dc_option(1, "1.1.1.1")];
        recoverer.update_dc_options();
        recoverer.dc_options_i = 3; // a rotated-out index
        recoverer.simple_config_query_active = true;

        let result = SimpleConfigResult {
            r_http_date: Ok(50_000),
            r_config: Ok(simple_config_with_rules("", 2, ip_port(0x0909_0909, 443))),
        };
        let actions = recoverer.on_simple_config(Ok(result), &environment);
        assert!(!recoverer.simple_config_query_active);
        assert_eq!(recoverer.dc_options_i, 0, "cpp:568 resets the rotation");
        assert_eq!(recoverer.dc_options().len(), 2);
        assert_eq!(recoverer.dc_options()[0].ip_address, "1.1.1.1");
        assert_eq!(recoverer.dc_options()[1].ip_address, "9.9.9.9");

        // The fresh success expiry holds: no refetch, the wakeup arms at
        // the earliest deadline instead.
        assert!(actions.fetch_simple_config.is_none());
        assert!(actions.fetch_full_config.is_none());
        assert!(actions.wakeup_at.is_some());
        assert_eq!(actions.dns_time_difference, Some(50_000.0 - (clock + 60.0)));
    }

    // ---- the SimpleAuthData pmc key/salt store (M.3.7.4) ----

    #[test]
    fn the_recovery_auth_key_round_trips_through_the_pmc() {
        let mut pmc = RecovererPmc::new("auth-key");
        assert!(load_recovery_auth_key(&pmc, 2).is_none());

        let auth_key = AuthKey::new(0x0f0e_0d0c_0b0a_0908, (0..=255).collect());
        store_recovery_auth_key(&mut pmc, 2, &auth_key);
        let loaded = load_recovery_auth_key(&pmc, 2).expect("the key loads");
        assert_eq!(loaded.id(), auth_key.id());

        // The keys are per-dc.
        assert!(load_recovery_auth_key(&pmc, 1).is_none());
    }

    #[test]
    fn the_recovery_salts_round_trip_through_the_pmc() {
        let mut pmc = RecovererPmc::new("salts");
        assert!(load_recovery_salts(&pmc, 2).is_empty());

        let salts = [
            ServerSalt {
                salt: -123456789,
                valid_since: 1_700_000_000.5,
                valid_until: 1_700_003_000.0,
            },
            ServerSalt {
                salt: 42,
                valid_since: 1_700_003_000.0,
                valid_until: f64::MAX,
            },
        ];
        pmc.set_raw(
            recovery_salt_key(2).as_bytes(),
            &store_recovery_salts(&salts),
        );
        let loaded = load_recovery_salts(&pmc, 2);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].salt, -123456789);
        assert_eq!(loaded[0].valid_since, 1_700_000_000.5);
        assert_eq!(loaded[0].valid_until, 1_700_003_000.0);
        assert_eq!(loaded[1].salt, 42);

        // The value is `td::serialize(vector<ServerSalt>)`: an int32 count
        // + the (i64 salt, f64 valid_since, f64 valid_until) triples — 4 +
        // 2 * 24 bytes.
        let raw = store_recovery_salts(&salts);
        assert_eq!(raw.len(), 4 + 2 * 24);
        assert_eq!(&raw[0..4], &2i32.to_le_bytes());
        assert_eq!(&raw[4..12], &(-123456789i64).to_le_bytes());
        assert_eq!(&raw[12..20], &1_700_000_000.5f64.to_le_bytes());
        assert_eq!(&raw[20..28], &1_700_003_000.0f64.to_le_bytes());
        assert_eq!(&raw[28..36], &42i64.to_le_bytes());

        // Truncated input degrades to the empty set (the C++
        // `unserialize(...).ensure()` would crash).
        pmc.set_raw(recovery_salt_key(3).as_bytes(), &[1, 2, 3]);
        assert!(load_recovery_salts(&pmc, 3).is_empty());
        // The salts are per-dc.
        assert!(load_recovery_salts(&pmc, 1).is_empty());
    }

    // ---- the session header (MtprotoHeader.cpp:23-110) ----

    fn recovery_header_fixture() -> RecoveryHeaderOptions {
        RecoveryHeaderOptions {
            api_id: 94575,
            device_model: "device".to_string(),
            system_version: "system".to_string(),
            application_version: "app".to_string(),
            system_language_code: "en".to_string(),
            language_pack: "android".to_string(),
            language_code: "de".to_string(),
            is_emulator: false,
            mtproto_proxy: None,
            parameters: String::new(),
            tz_offset: 0.0,
        }
    }

    /// The header over an explicit [`telegram_api::invokeWithLayer`] store
    /// — pinning the transformation (the flags, the language fields, the
    /// parameters object), not the serialization itself.
    fn stored_header(
        flags: i32,
        lang_pack: &str,
        lang_code: &str,
        params: telegram_api::JSONValue,
        proxy: Option<(&str, i32)>,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        telegram_api::invokeWithLayer {
            layer: MTPROTO_LAYER,
            query: Some(Box::new(telegram_api::Function::initConnection(
                telegram_api::initConnection {
                    flags,
                    api_id: 94575,
                    device_model: "device".to_string(),
                    system_version: "system".to_string(),
                    app_version: "app".to_string(),
                    system_lang_code: "en".to_string(),
                    lang_pack: lang_pack.to_string(),
                    lang_code: lang_code.to_string(),
                    proxy: proxy.map(|(address, port)| telegram_api::InputClientProxy {
                        address: address.to_string(),
                        port,
                    }),
                    params: Some(Box::new(params)),
                    query: Some(Box::new(telegram_api::Function::help_getConfig(
                        telegram_api::help_getConfig,
                    ))),
                },
            ))),
        }
        .store_boxed(&mut out);
        out
    }

    fn tz_object(members: Vec<telegram_api::JSONObjectValue>) -> telegram_api::JSONValue {
        telegram_api::JSONValue::jsonObject(telegram_api::jsonObject {
            value: members.into_iter().map(Box::new).collect(),
        })
    }

    fn tz_member(value: f64) -> telegram_api::JSONObjectValue {
        telegram_api::JSONObjectValue {
            key: "tz_offset".to_string(),
            value: Some(Box::new(telegram_api::JSONValue::jsonNumber(
                telegram_api::jsonNumber { value },
            ))),
        }
    }

    #[test]
    fn the_recovery_header_pins_the_layer_and_the_stored_shape() {
        let header = gen_recovery_header(&recovery_header_fixture()).expect("the header builds");
        // `invokeWithLayer` (0xda9b0d0d) + `MTPROTO_LAYER` (Version.h:13).
        assert_eq!(&header[0..4], &0xda9b_0d0du32.to_le_bytes());
        assert_eq!(&header[4..8], &MTPROTO_LAYER.to_le_bytes());

        // flags = 1 << 1 only (the params box; no proxy, no emulator), the
        // options' fields verbatim, and the empty parameters carrying just
        // tz_offset.
        let expected = stored_header(
            1 << 1,
            "android",
            "de",
            tz_object(vec![tz_member(0.0)]),
            None,
        );
        assert_eq!(header, expected);
    }

    #[test]
    fn the_recovery_header_language_rules_match_the_cpp() {
        // MtprotoHeader.cpp:56-67.
        assert_eq!(
            recovery_language_fields("android", "de"),
            ("android".to_string(), "de".to_string())
        );
        // An empty code with a real pack becomes "en".
        assert_eq!(
            recovery_language_fields("android", ""),
            ("android".to_string(), "en".to_string())
        );
        // An empty pack clears both fields.
        assert_eq!(
            recovery_language_fields("", "de"),
            (String::new(), String::new())
        );
        // A custom (`X`-prefixed) code clears both fields too.
        assert_eq!(
            recovery_language_fields("android", "Xde"),
            (String::new(), String::new())
        );
    }

    #[test]
    fn the_recovery_header_parameters_rules_match_the_cpp() {
        // An existing tz_offset is replaced in place, the other members
        // kept (MtprotoHeader.cpp:88-107).
        let mut options = recovery_header_fixture();
        options.parameters = r#"{"tz_offset": 99, "keep": true}"#.to_string();
        options.tz_offset = 1.5;
        let header = gen_recovery_header(&options).expect("the header builds");
        let keep = telegram_api::JSONObjectValue {
            key: "keep".to_string(),
            value: Some(Box::new(telegram_api::JSONValue::jsonBool(
                telegram_api::jsonBool { value: true },
            ))),
        };
        let expected = stored_header(
            1 << 1,
            "android",
            "de",
            tz_object(vec![tz_member(1.5), keep]),
            None,
        );
        assert_eq!(header, expected);

        // A non-object value keeps its shape, without the tz injection;
        // the emulator flag rides the flags word.
        let mut options = recovery_header_fixture();
        options.parameters = "[1, null]".to_string();
        options.is_emulator = true;
        let header = gen_recovery_header(&options).expect("the header builds");
        let params = telegram_api::JSONValue::jsonArray(telegram_api::jsonArray {
            value: vec![
                Box::new(telegram_api::JSONValue::jsonNumber(
                    telegram_api::jsonNumber { value: 1.0 },
                )),
                Box::new(telegram_api::JSONValue::jsonNull(telegram_api::jsonNull)),
            ],
        });
        let expected = stored_header((1 << 1) | (1 << 10), "android", "de", params, None);
        assert_eq!(header, expected);

        // An Mtproto proxy sets flag 0 and carries inputClientProxy
        // (MtprotoHeader.cpp:37-39, 68-72).
        let mut options = recovery_header_fixture();
        options.mtproto_proxy = Some(("proxy.example.com".to_string(), 934));
        let header = gen_recovery_header(&options).expect("the header builds");
        let expected = stored_header(
            (1 << 0) | (1 << 1),
            "android",
            "de",
            tz_object(vec![tz_member(0.0)]),
            Some(("proxy.example.com", 934)),
        );
        assert_eq!(header, expected);
    }

    #[test]
    fn the_recovery_header_rejects_invalid_parameters_json() {
        let mut options = recovery_header_fixture();
        options.parameters = "{\"broken\":".to_string();
        assert!(gen_recovery_header(&options).is_err());
    }

    // ---- the SessionCallback try-count gate (ConfigManager.cpp:336-357) ----

    #[test]
    fn the_raw_connection_gate_allows_two_tries() {
        assert!(request_raw_connection_allowed(1));
        assert!(request_raw_connection_allowed(2));
        assert!(!request_raw_connection_allowed(3));
        assert!(!request_raw_connection_allowed(4));
    }

    // ---- the result routing (SessionCallback::on_result) ----

    #[test]
    fn full_config_results_route_by_message_id() {
        let mut callback = FullConfigCallback {
            request_id: MessageId::new(0x7500_0000_0000_0001),
            answer: None,
            failed: false,
            closed: false,
            salts_updated: false,
        };

        // A result for another message id is dropped (the C++ dispatcher's
        // job).
        let status = callback.on_message_result_ok(
            MessageId::new(7),
            BufferSlice::from_slice_bytes(b"other"),
            5,
        );
        assert!(status.is_ok());
        assert!(callback.answer.is_none());

        // The query's own answer is captured.
        let status = callback.on_message_result_ok(
            callback.request_id,
            BufferSlice::from_slice_bytes(b"config"),
            6,
        );
        assert!(status.is_ok());
        assert_eq!(callback.answer.take().unwrap().unwrap(), b"config".to_vec());

        // An error for the query id becomes the answer's error.
        callback.on_message_result_error(
            callback.request_id,
            401,
            "AUTH_KEY_UNREGISTERED".to_string(),
        );
        let error = callback
            .answer
            .take()
            .unwrap()
            .expect_err("the error answer");
        assert_eq!(error.message(), "AUTH_KEY_UNREGISTERED");

        // A failure of another message id does not flag the resend.
        callback.on_message_failed(MessageId::new(7), Status::ok());
        assert!(!callback.failed);
        callback.on_message_failed(callback.request_id, Status::ok());
        assert!(callback.failed);

        // The salt/closed flags route the executor's persist/reconnect.
        callback.on_server_salt_updated();
        assert!(callback.salts_updated);
        assert!(!callback.closed);
        callback.on_closed(Status::ok());
        assert!(callback.closed);
    }

    // ---- the GetConfigActor lifecycle (ConfigManager.cpp:436-467) ----

    fn persisted_key_pmc(name: &str, raw_dc_id: i32) -> RecovererPmc {
        // A persisted key skips the handshake, so the lifecycle tests run
        // against a plain connection.
        let mut pmc = RecovererPmc::new(name);
        let auth_key = AuthKey::new(0x0f0e_0d0c_0b0a_0908, (0..=255).collect());
        store_recovery_auth_key(&mut pmc, raw_dc_id, &auth_key);
        pmc
    }

    fn local_option(port: u16) -> RecovererDcOption {
        RecovererDcOption {
            raw_dc_id: 2,
            ip_address: "127.0.0.1".to_string(),
            port: port as i32,
            secret: Vec::new(),
        }
    }

    #[tokio::test]
    async fn full_config_fetch_times_out_when_the_server_is_silent() {
        // An in-process server that accepts and never answers — the
        // 10-second actor budget (shrunk here) expires with the C++
        // `timeout_expired` text.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });

        let mut pmc = persisted_key_pmc("config-timeout", 2);
        let option = local_option(addr.port());
        let result = fetch_full_config_impl(
            &option,
            false,
            &recovery_header_fixture(),
            0.0,
            &mut pmc,
            std::time::Duration::from_millis(750),
        )
        .await;
        let error = match result {
            Err(status) => status,
            Ok(_) => panic!("the fetch times out"),
        };
        assert_eq!(error.message(), "Timeout expired");
    }

    #[tokio::test]
    async fn full_config_fetch_fails_when_the_connection_dies() {
        // The server accepts once and dies with its listener, so the
        // gate-allowed reconnect cannot establish — the C++
        // `hangup_shared`-with-token-1 "Failed" path.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            drop(listener);
            drop(stream);
        });

        let mut pmc = persisted_key_pmc("config-failed", 2);
        let option = local_option(addr.port());
        let result = fetch_full_config_impl(
            &option,
            false,
            &recovery_header_fixture(),
            0.0,
            &mut pmc,
            std::time::Duration::from_secs(5),
        )
        .await;
        let error = match result {
            Err(status) => status,
            Ok(_) => panic!("the reconnect fails"),
        };
        assert_eq!(error.message(), "Failed");
    }

    #[test]
    fn the_full_config_actor_timeout_is_ten_seconds() {
        // ConfigManager.cpp:466 — `set_timeout_in(10)`.
        assert_eq!(
            FULL_CONFIG_ACTOR_TIMEOUT,
            std::time::Duration::from_secs(10)
        );
    }

    /// The optional live test-DC smoke (ConfigManager.cpp:325-497 against
    /// the real network): a fresh handshake on the saved
    /// [`RecovererDcOption`] endpoint, the persisted
    /// `config_recovery_auth<raw_dc_id>` key reused WITHOUT a second
    /// handshake, and a parsed `help.config`. Needs the network and the
    /// public test credentials (`TD_RS_TEST_API_ID`; `help.getConfig` is
    /// unauth): `cargo test -p tdclient -- --ignored fetch_full_config`.
    #[ignore = "needs the network and TD_RS_TEST_API_ID"]
    #[tokio::test]
    async fn fetch_full_config_live_smoke() {
        let Ok(api_id) = std::env::var("TD_RS_TEST_API_ID") else {
            panic!("TD_RS_TEST_API_ID is not set");
        };
        let Ok(api_id) = api_id.parse::<i32>() else {
            panic!("TD_RS_TEST_API_ID must be an integer");
        };

        let option = RecovererDcOption {
            raw_dc_id: 10002,
            ip_address: "149.154.167.40".to_string(),
            port: 443,
            secret: Vec::new(),
        };
        let mut options = recovery_header_fixture();
        options.api_id = api_id;
        let is_test_dc = option.raw_dc_id > 1000;

        let mut pmc = RecovererPmc::new("live-smoke");
        // First run: a fresh handshake persists the recovery auth key.
        let (config, time_difference) =
            fetch_full_config(&option, is_test_dc, &options, 0.0, &mut pmc)
                .await
                .expect("the live full config fetch");
        assert!(
            time_difference.is_some(),
            "a fresh handshake reports the time diff"
        );
        assert!(config.date > 0);
        assert!(
            !config.dc_options.is_empty(),
            "a real config carries options"
        );
        assert!(load_recovery_auth_key(&pmc, option.raw_dc_id).is_some());

        // Second run: the persisted key is reused — no new handshake, no
        // new time difference report.
        let (config2, time_difference2) =
            fetch_full_config(&option, is_test_dc, &options, 0.0, &mut pmc)
                .await
                .expect("the reused-key full config fetch");
        assert!(time_difference2.is_none(), "no handshake, no time diff");
        assert_eq!(config2.date, config.date);
        assert!(!load_recovery_salts(&pmc, option.raw_dc_id).is_empty());
    }
}
