//! `LinkManager` (td/telegram/LinkManager.cpp) — the M.3.9.1 slice: the
//! autologin-token bookkeeping `process_config` drives
//! (LinkManager.cpp:4000-4003). The link parsing/formatting surface
//! (the 4.8K `check_link`/`parse_internal_link` machinery, Wave 2), the
//! domain-list restore and `update_autologin_domains` (cpp:1899-1910,
//! 4005-4018 — the appConfig path) stay with the LinkManager step.
//!
//! The M.8 MessageEntity slice adds the static link helpers the entity
//! converters and text parsers need: `check_link` (cpp:1926-1992),
//! `get_link_user_id` (cpp:4385-4425), `get_checked_link` (cpp:1938-1943),
//! `get_link_custom_emoji_id` (cpp:4467-4482) and `get_link_formatted_date`
//! (cpp:4484-4504).

use crate::chat_manager::AdministratorRights;
use crate::emoji_status::CustomEmojiId;
use crate::formatted_date::FormattedDate;
use crate::message_id::MessageFullId;
use crate::td_api;
use crate::user_manager::UserId;
use tdutils::http_url::{
    HttpUrlQuery, Protocol, parse_url, parse_url_query, url_decode, url_encode,
};
use tdutils::status::Status;

/// `tolower_begins_with` (LinkManager.cpp:1914-1921) — ASCII case-fold.
fn tolower_begins_with(s: &str, prefix: &str) -> bool {
    if prefix.len() > s.len() {
        return false;
    }
    s.as_bytes()
        .iter()
        .zip(prefix.as_bytes())
        .all(|(c, p)| c.to_ascii_lowercase() == *p)
}

/// `LinkManager::check_link_impl` (LinkManager.cpp:1941-1988).
fn check_link_impl(link: &str, http_only: bool, https_only: bool) -> Result<String, Status> {
    let mut link = link;
    let mut is_tg = false;
    let mut is_ton = false;
    let mut is_tonsite = false;
    if tolower_begins_with(link, "tg:") {
        link = &link[3..];
        is_tg = true;
    } else if tolower_begins_with(link, "ton:") {
        link = &link[4..];
        is_ton = true;
    } else if tolower_begins_with(link, "tonsite:") {
        link = &link[8..];
        is_tonsite = true;
    }
    if (is_tg || is_ton || is_tonsite) && link.starts_with("//") {
        link = &link[2..];
    }
    let http_url = parse_url(link).into_inner()?;
    if https_only && (http_url.protocol != Protocol::Https || is_tg || is_ton || is_tonsite) {
        return Err(Status::error_message("Only HTTPS links are allowed"));
    }
    if is_tg || is_ton || is_tonsite {
        if http_only {
            return Err(Status::error_message("Only HTTP links are allowed"));
        }
        if tolower_begins_with(link, "http://")
            || http_url.protocol == Protocol::Https
            || !http_url.userinfo.is_empty()
            || http_url.specified_port != 0
            || http_url.is_ipv6
        {
            return Err(Status::error_message(if is_tg {
                "Wrong tg URL"
            } else {
                "Wrong ton URL"
            }));
        }

        let mut query = http_url.query.as_str();
        // The invariant `query[0] == '/'` is kept by `parse_url`.
        assert!(query.starts_with('/'));
        if query.len() > 1 && query.as_bytes()[1] == b'?' {
            query = &query[1..];
        }
        for c in http_url.host.bytes() {
            if !c.is_ascii_alphanumeric() && c != b'-' && c != b'_' && !(is_tonsite && c == b'.') {
                return Err(Status::error_message("Unallowed characters in URL host"));
            }
        }
        return Ok(format!(
            "{}://{}{}",
            if is_tg {
                "tg"
            } else if is_tonsite {
                "tonsite"
            } else {
                "ton"
            },
            http_url.host,
            query
        ));
    }

    if !http_url.host.contains('.') && !http_url.is_ipv6 {
        return Err(Status::error_message("Wrong HTTP URL"));
    }
    Ok(http_url.get_url())
}

/// `LinkManager::check_link` (LinkManager.cpp:1926-1939).
pub fn check_link(link: &str, http_only: bool, https_only: bool) -> Result<String, Status> {
    match check_link_impl(link, http_only, https_only) {
        Ok(result) => Ok(result),
        Err(error) => {
            if tdutils::utf8::check_utf8(link.as_bytes()) {
                Err(Status::error(
                    400,
                    &format!("URL '{link}' is invalid: {}", error.message()),
                ))
            } else {
                Err(Status::error(
                    400,
                    &format!("URL is invalid: {}", error.message()),
                ))
            }
        }
    }
}

/// `LinkManager::get_link_user_id` (LinkManager.cpp:4385-4425): the
/// `tg:user?id=<int64>` resolver. The id parse is the strict
/// `to_integer_safe` round-trip form.
pub fn get_link_user_id(url: &str) -> UserId {
    let lower_cased_url = url.to_ascii_lowercase();
    let url = lower_cased_url.as_str();

    const LINK_SCHEME: &str = "tg:";
    let Some(url) = url.strip_prefix(LINK_SCHEME) else {
        return UserId::new(0);
    };
    let url = url.strip_prefix("//").unwrap_or(url);

    const HOST: &str = "user";
    if !url.starts_with(HOST)
        || (url.len() > HOST.len() && !matches!(url.as_bytes()[HOST.len()], b'/' | b'?' | b'#'))
    {
        return UserId::new(0);
    }
    let url = &url[HOST.len()..];
    let url = url.strip_prefix('/').unwrap_or(url);
    let Some(url) = url.strip_prefix('?') else {
        return UserId::new(0);
    };
    let url = url.split('#').next().unwrap_or("");

    for parameter in url.split('&') {
        let (key, value) = parameter.split_once('=').unwrap_or((parameter, ""));
        if key == "id" {
            let Some(user_id) = crate::config_manager::to_integer_safe_i64(value) else {
                return UserId::new(0);
            };
            return UserId::new(user_id);
        }
    }
    UserId::new(0)
}

/// `LinkManager::get_checked_link` (LinkManager.cpp:1938-1943): the
/// empty-string-on-error form used by the text parsers.
pub fn get_checked_link(link: &str) -> String {
    check_link(link, false, false).unwrap_or_default()
}

/// `LinkManager::check_tg_url_host` (LinkManager.cpp:4435-4465): the shared
/// `tg:<host>?<query>` validation; returns the query part.
///
/// The scheme and the host compare against the LOWER-CASED prefix — the C++
/// builds `to_lower(url.substr(0, min_size))` and matches `host` there
/// (cpp:4437, 4451). The m8b defect was exactly this: the first port compared
/// the raw bytes case-sensitively, so `tg://EMOJI?id=...` was rejected with
/// 400 "URL must have host" where the C++ normalizes and accepts.
fn check_tg_url_host(url: &str, host: &str) -> Result<String, Status> {
    let min_size = 3 + 2 + host.len() + 1;
    // `to_lower(url.size() > min_size ? url.substr(0, min_size) : url)` — the
    // ASCII fold keeps the byte count (and UTF-8 continuation bytes) intact.
    let take = min_size.min(url.len());
    let lowered_prefix: Vec<u8> = url.as_bytes()[..take]
        .iter()
        .map(|c| c.to_ascii_lowercase())
        .collect();

    if !lowered_prefix.starts_with(b"tg:") {
        return Err(Status::error(400, "URL must have scheme tg"));
    }
    let mut rest_offset = 3usize;
    let mut url_rest = &url[3..];
    if url_rest.starts_with("//") {
        rest_offset += 2;
        url_rest = &url_rest[2..];
    }
    // The lowered prefix keeps the raw byte layout, so the offsets line up.
    let lowered_rest = &lowered_prefix[rest_offset..];
    if !lowered_rest.starts_with(host.as_bytes())
        || (url_rest.len() > host.len()
            && !matches!(url_rest.as_bytes()[host.len()], b'/' | b'?' | b'#'))
    {
        return Err(Status::error(
            400,
            &format!("URL must have host \"{host}\""),
        ));
    }
    let mut url_rest = &url_rest[host.len()..];
    if url_rest.starts_with('/') {
        url_rest = &url_rest[1..];
    }
    let Some(url_rest) = url_rest.strip_prefix('?') else {
        return Err(Status::error(400, "URL must have parameters"));
    };
    Ok(url_rest.split('#').next().unwrap_or("").to_owned())
}

/// `LinkManager::get_link_custom_emoji_id` (LinkManager.cpp:4467-4482).
pub fn get_link_custom_emoji_id(url: &str) -> Result<CustomEmojiId, Status> {
    let query = check_tg_url_host(url, "emoji")?;
    for parameter in query.split('&') {
        let (key, value) = parameter.split_once('=').unwrap_or((parameter, ""));
        if key == "id" {
            let Some(document_id) = crate::config_manager::to_integer_safe_i64(value) else {
                return Err(Status::error(
                    400,
                    "Invalid custom emoji identifier specified",
                ));
            };
            if document_id == 0 {
                return Err(Status::error(
                    400,
                    "Invalid custom emoji identifier specified",
                ));
            }
            return Ok(CustomEmojiId::new(document_id));
        }
    }
    Err(Status::error(
        400,
        "Custom emoji URL must have an emoji identifier",
    ))
}

/// `LinkManager::get_link_formatted_date` (LinkManager.cpp:4484-4504).
pub fn get_link_formatted_date(url: &str) -> Result<FormattedDate, Status> {
    let query = check_tg_url_host(url, "time")?;
    let mut date = 0i32;
    let mut format = String::new();
    for parameter in query.split('&') {
        let (key, value) = parameter.split_once('=').unwrap_or((parameter, ""));
        if key == "unix" {
            let parsed = match crate::config_manager::to_integer_safe_i64(value) {
                Some(parsed) if parsed > 0 && parsed <= i32::MAX as i64 => parsed as i32,
                _ => return Err(Status::error(400, "Invalid Unix time specified")),
            };
            date = parsed;
        }
        if key == "format" {
            format = value.to_owned();
        }
    }
    if date == 0 {
        return Err(Status::error(
            400,
            "URL must have the corresponding Unix time",
        ));
    }
    crate::formatted_date::FormattedDate::get_formatted_date_by_format(date, &format)
}

/// `td::trim` (misc.h:131-144) — trims the `is_space` set from both ends.
fn td_trim(s: &str) -> &str {
    s.trim_matches(|c| matches!(c, ' ' | '\t' | '\n' | '\x0b' | '\x0c' | '\r'))
}

/// `td::to_integer<int32>` (misc.h:157-183) — the lenient form: an optional
/// `-`, then digits with wrapping overflow, stopping at the first non-digit.
fn to_integer_i32(s: &str) -> i32 {
    let bytes = s.as_bytes();
    let mut index = 0usize;
    let mut is_negative = false;
    if index < bytes.len() && bytes[index] == b'-' {
        is_negative = true;
        index += 1;
    }
    let mut value: i64 = 0;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        value = value
            .wrapping_mul(10)
            .wrapping_add((bytes[index] - b'0') as i64);
        index += 1;
    }
    if is_negative {
        value = -value;
    }
    // The C++ saturates to i32::MIN on a double wrap-around; the common
    // inputs (ports, ids) never reach it, so the plain truncation keeps
    // parity for every defined case.
    value as i32
}

/// `td::to_integer<int64>` (misc.h:157-183) — the lenient i64 form.
fn to_integer_i64(s: &str) -> i64 {
    let bytes = s.as_bytes();
    let mut index = 0usize;
    let mut is_negative = false;
    if index < bytes.len() && bytes[index] == b'-' {
        is_negative = true;
        index += 1;
    }
    let mut value: u64 = 0;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        value = value
            .wrapping_mul(10)
            .wrapping_add((bytes[index] - b'0') as u64);
        index += 1;
    }
    if is_negative {
        (value as i64).wrapping_neg()
    } else {
        value as i64
    }
}

/// `td::to_integer_safe<int32>` (misc.h:200-206) — the lenient parse plus the
/// minimal-decimal round-trip check.
fn to_integer_safe_i32(s: &str) -> Option<i32> {
    let value = to_integer_i32(s);
    if value.to_string() == s {
        Some(value)
    } else {
        None
    }
}

/// `is_base64url_characters` (base64.cpp:226-228) — the alphabet check only,
/// no padding rules (that is `is_base64url`).
fn is_base64url_characters(input: &[u8]) -> bool {
    input
        .iter()
        .all(|&c| matches!(c, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_'))
}

/// `is_valid_start_parameter`/`is_valid_video_chat_invite_hash`
/// (LinkManager.cpp:56-62).
fn is_valid_start_parameter(start_parameter: &str) -> bool {
    is_base64url_characters(start_parameter.as_bytes())
}

/// `is_valid_phone_number` (cpp:64-74).
fn is_valid_phone_number(phone_number: &str) -> bool {
    !phone_number.is_empty()
        && phone_number.len() <= 32
        && phone_number.bytes().all(|c| c.is_ascii_digit())
}

/// `is_valid_phone_number_hash` (cpp:76-86).
fn is_valid_phone_number_hash(hash: &str) -> bool {
    !hash.is_empty() && hash.len() <= 32 && hash.bytes().all(|c| c.is_ascii_hexdigit())
}

/// `is_valid_game_name` (cpp:88-90).
fn is_valid_game_name(name: &str) -> bool {
    name.len() >= 3 && crate::misc::is_valid_username(name)
}

/// `is_valid_web_app_name` (cpp:100-102) — the same shape as the game name.
fn is_valid_web_app_name(name: &str) -> bool {
    name.len() >= 3 && crate::misc::is_valid_username(name)
}

/// `is_valid_text_composition_style_name` (cpp:92-94).
fn is_valid_text_composition_style_name(name: &str) -> bool {
    name.len() >= 8 && is_base64url_characters(name.as_bytes())
}

/// `is_valid_theme_name` (cpp:96-98).
fn is_valid_theme_name(name: &str) -> bool {
    !name.is_empty() && tdutils::utf8::check_utf8(name.as_bytes())
}

/// `is_valid_sticker_set_name` (cpp:104-106).
fn is_valid_sticker_set_name(name: &str) -> bool {
    !name.is_empty() && is_base64url_characters(name.as_bytes())
}

/// `LanguagePackManager::check_language_code_name`
/// (LanguagePackManager.cpp:117-124): `-`/alpha/digit bytes, at most 64 of
/// them, and then either a custom (`X`-prefixed), empty or 2+-length code.
fn check_language_code_name(name: &str) -> bool {
    for c in name.bytes() {
        if c != b'-' && !c.is_ascii_alphabetic() && !c.is_ascii_digit() {
            return false;
        }
    }
    let is_custom = !name.is_empty() && name.as_bytes()[0] == b'X';
    name.len() <= 64 && (is_custom || name.is_empty() || name.len() >= 2)
}

/// `is_valid_language_pack_id` (cpp:108-110).
fn is_valid_language_pack_id(language_pack_id: &str) -> bool {
    !language_pack_id.is_empty() && check_language_code_name(language_pack_id)
}

/// `is_valid_upgraded_gift_name` (cpp:112-120).
fn is_valid_upgraded_gift_name(name: &str) -> bool {
    !name.is_empty() && tdutils::utf8::check_utf8(name.as_bytes())
}

/// `is_valid_user_token` (cpp:122-124).
fn is_valid_user_token(token: &str) -> bool {
    !token.is_empty() && tdutils::utf8::check_utf8(token.as_bytes())
}

/// `is_valid_unpaid_messages_token` (LinkManager.cpp:66-68) — at most
/// 64 base64url characters.
fn is_valid_unpaid_messages_token(token: &str) -> bool {
    !token.is_empty() && token.len() <= 64 && crate::misc::is_base64url_characters(token.as_bytes())
}

/// `is_valid_login_code` (cpp:126-128).
fn is_valid_login_code(code: &str) -> bool {
    !code.is_empty() && tdutils::utf8::check_utf8(code.as_bytes())
}

/// `is_valid_premium_referrer` (cpp:130-132).
fn is_valid_premium_referrer(referrer: &str) -> bool {
    tdutils::utf8::check_utf8(referrer.as_bytes())
}

/// `is_valid_gram_receiver` (LinkManager.cpp:177-185).
fn is_valid_gram_receiver(receiver: &str) -> bool {
    if receiver.is_empty() {
        return true;
    }
    if let Some(username) = receiver.strip_prefix('@') {
        return receiver.len() >= 5 && crate::misc::is_valid_username(username);
    }
    crate::misc::is_base64url_characters(receiver.as_bytes())
}

/// `is_valid_gram_amount` (LinkManager.cpp:187-212).
fn is_valid_gram_amount(amount: &str) -> bool {
    if amount.is_empty() {
        return true;
    }
    let dot_pos = amount.find('.').unwrap_or(amount.len());
    let integer_part = &amount[..dot_pos];
    if integer_part.len() > 7 || (integer_part.len() == 7 && integer_part.starts_with('9')) {
        return false;
    }
    if !integer_part.bytes().all(|c| c.is_ascii_digit()) {
        return false;
    }
    let fractional_part = if dot_pos != amount.len() {
        &amount[dot_pos + 1..]
    } else {
        ""
    };
    if fractional_part.len() > 9 {
        return false;
    }
    fractional_part.bytes().all(|c| c.is_ascii_digit())
}

/// `get_gram_amount` (LinkManager.cpp:214-233) — the nano-TON multiplier
/// is 10^9; the caller has validated the digits.
fn get_gram_amount(amount: &str) -> i64 {
    debug_assert!(is_valid_gram_amount(amount));
    let dot_pos = amount.find('.').unwrap_or(amount.len());
    let integer_part = &amount[..dot_pos];
    let mut result: i64 = 0;
    for c in integer_part.bytes() {
        result = result * 10 + (c - b'0') as i64;
    }
    result *= 1_000_000_000;
    let fractional_part = if dot_pos != amount.len() {
        &amount[dot_pos + 1..]
    } else {
        ""
    };
    let mut multiplier: i64 = 100_000_000;
    for c in fractional_part.bytes() {
        result += multiplier * (c - b'0') as i64;
        multiplier /= 10;
    }
    result
}

/// `is_valid_proxy_server` (cpp:134-136).
fn is_valid_proxy_server(server: &str) -> bool {
    !server.is_empty() && server.len() <= 255 && tdutils::utf8::check_utf8(server.as_bytes())
}

/// `is_valid_proxy_username`/`is_valid_proxy_password` (cpp:138-144).
fn is_valid_proxy_username(username: &str) -> bool {
    tdutils::utf8::check_utf8(username.as_bytes())
}

/// `is_valid_proxy_password` (cpp:142-144).
fn is_valid_proxy_password(password: &str) -> bool {
    tdutils::utf8::check_utf8(password.as_bytes())
}

/// `is_valid_invoice_name` (cpp:146-148).
fn is_valid_invoice_name(invoice_name: &str) -> bool {
    !invoice_name.is_empty() && tdutils::utf8::check_utf8(invoice_name.as_bytes())
}

/// `is_valid_gift_code` (cpp:150-152).
fn is_valid_gift_code(gift_code: &str) -> bool {
    !gift_code.is_empty() && tdutils::utf8::check_utf8(gift_code.as_bytes())
}

/// `is_valid_business_link_name` (cpp:154-156).
fn is_valid_business_link_name(link_name: &str) -> bool {
    !link_name.is_empty() && tdutils::utf8::check_utf8(link_name.as_bytes())
}

/// `is_valid_star_top_up_purpose` (cpp:158-160).
fn is_valid_star_top_up_purpose(purpose: &str) -> bool {
    tdutils::utf8::check_utf8(purpose.as_bytes())
}

/// `StoryId` (StoryId.h) — the slice this manager needs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StoryId(i32);

impl StoryId {
    pub fn new(id: i32) -> StoryId {
        StoryId(id)
    }
    /// `StoryId::is_server` — `0 < id <= MAX_SERVER_STORY_ID`.
    pub fn is_server(self) -> bool {
        const MAX_SERVER_STORY_ID: i32 = 1999999999;
        self.0 > 0 && self.0 <= MAX_SERVER_STORY_ID
    }
    pub fn get(self) -> i32 {
        self.0
    }
}

/// `StoryAlbumId` (StoryAlbumId.h) — `is_valid` is `id > 0`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StoryAlbumId(i32);

impl StoryAlbumId {
    pub fn new(id: i32) -> StoryAlbumId {
        StoryAlbumId(id)
    }
    pub fn is_valid(self) -> bool {
        self.0 > 0
    }
    pub fn get(self) -> i32 {
        self.0
    }
}

/// `StarGiftCollectionId` (StarGiftCollectionId.h) — `is_valid` is `id > 0`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StarGiftCollectionId(i32);

impl StarGiftCollectionId {
    pub fn new(id: i32) -> StarGiftCollectionId {
        StarGiftCollectionId(id)
    }
    pub fn is_valid(self) -> bool {
        self.0 > 0
    }
    pub fn get(self) -> i32 {
        self.0
    }
}

/// `is_valid_story_id` (cpp:162-165).
fn is_valid_story_id(story_id: &str) -> bool {
    to_integer_safe_i32(story_id).is_some_and(|id| StoryId::new(id).is_server())
}

/// `is_valid_star_gift_collection_id` (cpp:167-170).
fn is_valid_star_gift_collection_id(collection_id: &str) -> bool {
    to_integer_safe_i32(collection_id).is_some_and(|id| StarGiftCollectionId::new(id).is_valid())
}

/// `is_valid_story_album_id` (cpp:172-175).
fn is_valid_story_album_id(story_album_id: &str) -> bool {
    to_integer_safe_i32(story_album_id).is_some_and(|id| StoryAlbumId::new(id).is_valid())
}

/// `get_url_query_hash` (cpp:339-357).
fn get_url_query_hash(is_tg: bool, url_query: &tdutils::http_url::HttpUrlQuery) -> String {
    let path = &url_query.path;
    if is_tg {
        if path.len() == 1 && path[0] == "join" {
            // join?invite=<hash>
            return url_query.get_arg("invite").unwrap_or("").to_owned();
        }
    } else {
        if path.len() >= 2 && path[0] == "joinchat" {
            // /joinchat/<hash>
            return path[1].clone();
        }
        if let Some(first) = path.first()
            && first.len() >= 2
            && (first.as_bytes()[0] == b' ' || first.as_bytes()[0] == b'+')
        {
            // /+<link>
            return first[1..].to_owned();
        }
    }
    String::new()
}

/// `get_url_query_slug` (cpp:359-373).
fn get_url_query_slug(
    is_tg: bool,
    url_query: &tdutils::http_url::HttpUrlQuery,
    link_name: &str,
) -> String {
    let path = &url_query.path;
    if is_tg {
        if path.len() == 1 && path[0] == link_name {
            // {link_name}?slug=<hash>
            return url_query.get_arg("slug").unwrap_or("").to_owned();
        }
    } else if path.len() >= 2 && path[0] == link_name {
        // /{link_name}/<hash>
        return path[1].clone();
    }
    String::new()
}

/// `get_url_query_draft_text` (cpp:375-389).
fn get_url_query_draft_text(url_query: &tdutils::http_url::HttpUrlQuery) -> String {
    let text_slice = url_query.get_arg("text").unwrap_or("");
    if text_slice.is_empty() {
        return String::new();
    }
    if !tdutils::utf8::check_utf8(text_slice.as_bytes()) {
        return String::new();
    }
    let text = tdutils::utf8::utf8_truncate(text_slice, 4096);
    if text.starts_with('@') {
        format!(" {text}")
    } else {
        text.to_owned()
    }
}

/// `get_url_query_unpaid_messages_token` (LinkManager.cpp:462-468) — the
/// `free=` argument keeps only the valid token forms.
fn get_url_query_unpaid_messages_token(url_query: &tdutils::http_url::HttpUrlQuery) -> String {
    let token = url_query.get_arg("free").unwrap_or("");
    if !is_valid_unpaid_messages_token(token) {
        return String::new();
    }
    token.to_owned()
}

/// `get_dialog_link_query` (LinkManager.cpp:470-481) — the shared
/// `&`-prefixed query of the dialog links; the external callers rewrite
/// the leading separator to `?`.
fn get_dialog_link_query(
    draft_text: &str,
    open_profile: bool,
    unpaid_messages_token: &str,
) -> String {
    let mut query = String::new();
    if !draft_text.is_empty() {
        query.push_str("&text=");
        query.push_str(&url_encode(draft_text));
    }
    if open_profile {
        query.push_str("&profile");
    }
    if !unpaid_messages_token.is_empty() {
        query.push_str("&free=");
        query.push_str(unpaid_messages_token);
    }
    query
}

/// `get_referral_program_start_parameter_prefixes` (cpp:391-396). The
/// context-free `{"_tgr_"}` fallback is folded into the env default —
/// a collapsed-actor divergence (the C++ reads the option only under a
/// scheduler context).
fn get_referral_program_start_parameter_prefixes(env: &impl LinkEnv) -> Vec<String> {
    let raw = env.get_option_string_with_default("starref_start_param_prefixes", "_tgr_");
    raw.split(' ').map(str::to_owned).collect()
}

/// `get_administrator_rights` (cpp:398-462) — the `admin=` flag set. The C++
/// passes `can_manage_welcome_messages` for BOTH the linked-peers and the
/// welcome-messages slots of the `AdministratorRights` constructor
/// (cpp:460-461); the doubled argument is kept byte-faithfully.
fn get_administrator_rights(
    rights: &str,
    for_channel: bool,
) -> crate::chat_manager::AdministratorRights {
    let mut can_manage_dialog = false;
    let mut can_change_info = false;
    let mut can_post_messages = false;
    let mut can_edit_messages = false;
    let mut can_delete_messages = false;
    let mut can_invite_users = false;
    let mut can_restrict_members = false;
    let mut can_pin_messages = false;
    let mut can_manage_topics = false;
    let mut can_promote_members = false;
    let mut can_manage_calls = false;
    let mut can_post_stories = false;
    let mut can_edit_stories = false;
    let mut can_delete_stories = false;
    let mut can_manage_direct_messages = false;
    let mut can_manage_ranks = false;
    let mut can_manage_welcome_messages = false;
    let mut is_anonymous = false;
    for right in rights.split(' ') {
        match right {
            "change_info" => can_change_info = true,
            "post_messages" => can_post_messages = true,
            "edit_messages" => can_edit_messages = true,
            "delete_messages" => can_delete_messages = true,
            "restrict_members" => can_restrict_members = true,
            "invite_users" => can_invite_users = true,
            "pin_messages" => can_pin_messages = true,
            "manage_topics" => can_manage_topics = true,
            "promote_members" => can_promote_members = true,
            "manage_video_chats" => can_manage_calls = true,
            "post_stories" => can_post_stories = true,
            "edit_stories" => can_edit_stories = true,
            "delete_stories" => can_delete_stories = true,
            "manage_direct_messages" => can_manage_direct_messages = true,
            "manage_tags" => can_manage_ranks = true,
            "send_welcome_messages" => can_manage_welcome_messages = true,
            "anonymous" => is_anonymous = true,
            "manage_chat" => can_manage_dialog = true,
            _ => {}
        }
    }
    crate::chat_manager::AdministratorRights::from_bools(
        is_anonymous,
        can_manage_dialog,
        can_change_info,
        can_post_messages,
        can_edit_messages,
        can_delete_messages,
        can_invite_users,
        can_restrict_members,
        can_pin_messages,
        can_manage_topics,
        can_promote_members,
        can_manage_calls,
        can_post_stories,
        can_edit_stories,
        can_delete_stories,
        can_manage_direct_messages,
        can_manage_ranks,
        can_manage_welcome_messages,
        can_manage_welcome_messages,
        if for_channel {
            crate::chat_manager::ChannelType::Broadcast
        } else {
            crate::chat_manager::ChannelType::Megagroup
        },
    )
}

/// `get_admin_string` (cpp:464-524) — the reverse `&admin=` builder.
fn get_admin_string(rights: &crate::chat_manager::AdministratorRights) -> String {
    let mut admin_rights: Vec<&'static str> = Vec::new();
    if rights.can_change_info_and_settings() {
        admin_rights.push("change_info");
    }
    if rights.can_post_messages() {
        admin_rights.push("post_messages");
    }
    if rights.can_edit_messages() {
        admin_rights.push("edit_messages");
    }
    if rights.can_delete_messages() {
        admin_rights.push("delete_messages");
    }
    if rights.can_restrict_members() {
        admin_rights.push("restrict_members");
    }
    if rights.can_invite_users() {
        admin_rights.push("invite_users");
    }
    if rights.can_pin_messages() {
        admin_rights.push("pin_messages");
    }
    if rights.can_manage_topics() {
        admin_rights.push("manage_topics");
    }
    if rights.can_promote_members() {
        admin_rights.push("promote_members");
    }
    if rights.can_manage_calls() {
        admin_rights.push("manage_video_chats");
    }
    if rights.can_post_stories() {
        admin_rights.push("post_stories");
    }
    if rights.can_edit_stories() {
        admin_rights.push("edit_stories");
    }
    if rights.can_delete_stories() {
        admin_rights.push("delete_stories");
    }
    if rights.can_manage_direct_messages() {
        admin_rights.push("manage_direct_messages");
    }
    if rights.can_manage_ranks() {
        admin_rights.push("manage_tags");
    }
    if rights.can_manage_welcome_messages() {
        admin_rights.push("send_welcome_messages");
    }
    if rights.is_anonymous() {
        admin_rights.push("anonymous");
    }
    if rights.can_manage_dialog() {
        admin_rights.push("manage_chat");
    }
    if admin_rights.is_empty() {
        return String::new();
    }
    format!("&admin={}", admin_rights.join("+"))
}

/// `get_target_chat_types` (cpp:526-546).
fn get_target_chat_types(chat_types: &str) -> Option<td_api::TargetChatTypes> {
    let mut allow_users = false;
    let mut allow_bots = false;
    let mut allow_groups = false;
    let mut allow_channels = false;
    for chat_type in chat_types.split(' ') {
        match chat_type {
            "users" => allow_users = true,
            "bots" => allow_bots = true,
            "groups" => allow_groups = true,
            "channels" => allow_channels = true,
            _ => {}
        }
    }
    if !allow_users && !allow_bots && !allow_groups && !allow_channels {
        return None;
    }
    Some(td_api::TargetChatTypes {
        allow_user_chats: allow_users,
        allow_bot_chats: allow_bots,
        allow_group_chats: allow_groups,
        allow_channel_chats: allow_channels,
    })
}

/// `get_web_app_open_mode_object` (cpp:548-556).
fn get_web_app_open_mode_object(mode: &str) -> td_api::WebAppOpenMode {
    if mode == "compact" {
        return td_api::WebAppOpenMode::webAppOpenModeCompact(td_api::webAppOpenModeCompact);
    }
    if mode == "fullscreen" {
        return td_api::WebAppOpenMode::webAppOpenModeFullScreen(td_api::webAppOpenModeFullScreen);
    }
    td_api::WebAppOpenMode::webAppOpenModeFullSize(td_api::webAppOpenModeFullSize)
}

/// The `Td *td` slice of the collapsed LinkManager. The C++ static helpers
/// read `G()` under a `Scheduler::context() != nullptr` guard; the collapsed
/// port threads this trait instead, whose defaults are the context-free C++
/// behavior (an empty option table).
pub trait LinkEnv {
    /// `G()->get_option_string(name, default_value)`.
    fn get_option_string_with_default(&self, _name: &str, default_value: &str) -> String {
        default_value.to_owned()
    }

    /// `G()->get_option_string(name)` — empty when unset.
    fn get_option_string(&self, _name: &str) -> String {
        String::new()
    }

    /// `InternalLinkBotStart`'s resolved-dialog autostart promotion
    /// (LinkManager.cpp:636-647): the `premium_bot_username` option half and
    /// the `get_resolved_dialog_by_username` + `get_dialog_has_last_message` +
    /// `is_dialog_blocked` reads. The option half lands with the `Td` impl;
    /// the dialog half is `[blocked: dialog-manager]`.
    fn bot_start_autostart(&self, _bot_username: &str) -> bool {
        false
    }

    /// `DialogManager::reload_video_chat_on_search` (cpp:2195-2197,
    /// 2848-2850) — `[blocked: dialog-manager]`.
    fn reload_video_chat_on_search(&mut self, _username: &str) {}

    /// `G()->close_flag()`.
    fn close_flag(&self) -> bool {
        false
    }

    /// `G()->td_db()->get_binlog_pmc()->get/set` — the string form
    /// (`&mut self`: the C++ accessor hands out the mutable pmc).
    fn binlog_pmc_get(&mut self, _key: &str) -> String {
        String::new()
    }

    fn binlog_pmc_set(&mut self, _key: &str, _value: &str) {}

    /// `ConfigManager::reload_config` (cpp:4100) — `[blocked: net]`. The
    /// default answers `false` (the config is not reloaded), which routes
    /// the C++ failure path.
    fn reload_config(&mut self) -> bool {
        false
    }

    /// `DialogManager::get_input_peer(dialog_id, Read)` — the send-leg
    /// peer resolution of `RequestUrlAuthQuery::send` (cpp:1660-1665).
    /// The narrow env views can't reach the dialog plane, so the default
    /// answers `None` (the C++ `CHECK(input_peer != nullptr)` is fatal;
    /// the port downgrades it to a non-fatal log-and-drop); the
    /// button-URL path the peer form rides stays `[blocked: M.10
    /// integration]` ahead of it, so the live flow never reaches this
    /// seam before that lands.
    fn get_input_peer_read(
        &mut self,
        _dialog_id: crate::dialog_id::DialogId,
    ) -> Option<tdmtproto::telegram_api::InputPeer> {
        None
    }

    /// `CheckUrlAuthMatchCodeQuery` (cpp:1782-1808,
    /// `messages.checkUrlAuthMatchCode`) — `[blocked: net]`.
    fn send_check_url_auth_match_code_query(
        &mut self,
        _url: String,
        _match_code: String,
        _promise: tdutils::promise::Promise<()>,
    ) {
    }

    /// `AcceptUrlAuthQuery` (cpp:1810-1867, `messages.acceptUrlAuth`) —
    /// the acceptance query behind `getLoginUrl`, `getExternalLink` and
    /// `acceptOauthRequest` (the peer/button context and — upstream
    /// 2485f5889 — the TON Connect session ride along in the C++
    /// `send`; the session rides the wire query only when set, flag
    /// bit 1<<5; the offline tail drops the promise, so the port keeps
    /// the full parameter shape but never answers).
    #[allow(clippy::too_many_arguments)] // the C++ handler has the same shape
    fn send_accept_url_auth_query(
        &mut self,
        _url: String,
        _message_full_id: MessageFullId,
        _button_id: i32,
        _allow_write_access: bool,
        _allow_phone_number_access: bool,
        _match_code: String,
        _tonconnect_session: Option<tdmtproto::telegram_api::InputTonConnectOauthSession>,
        _promise: tdutils::promise::Promise<String>,
    ) {
    }

    /// `DeclineUrlAuthQuery` (cpp:1869+, `messages.declineUrlAuth`) —
    /// `[blocked: net]`.
    fn send_decline_url_auth_query(
        &mut self,
        _url: String,
        _promise: tdutils::promise::Promise<()>,
    ) {
    }

    /// `MessagesManager::get_login_button_url` (MessagesManager.cpp:6726-6762)
    /// — the message plane is the `[blocked: M.10 integration]` seam: no
    /// message is resolvable before MessagesManager lands, so the default
    /// answers the C++ not-found band.
    fn get_login_button_url(
        &mut self,
        _message_full_id: MessageFullId,
        _button_id: i64,
    ) -> tdutils::status::Result<String> {
        tdutils::status::Result::Err(Status::error(400, "Message not found"))
    }
}

/// The `LinkManager::InternalLink` parse results (LinkManager.cpp:558-1506):
/// the C++ `InternalLink` subclass per constructor becomes one variant, and
/// the `get_internal_link_type_object` methods fold into
/// [`InternalLink::get_internal_link_type_object`].
#[derive(Clone, Debug, PartialEq)]
pub enum InternalLink {
    /// `InternalLinkAttachMenuBot`.
    AttachMenuBot {
        allowed_chat_types: Option<td_api::TargetChatTypes>,
        dialog_link: Option<Box<InternalLink>>,
        bot_username: String,
        url: String,
    },
    /// `InternalLinkAuthenticationCode`.
    AuthenticationCode { code: String },
    /// `InternalLinkBackground`.
    Background { background_name: String },
    /// `InternalLinkBotAddToChannel`.
    BotAddToChannel {
        bot_username: String,
        administrator_rights: AdministratorRights,
    },
    /// `InternalLinkBotStart`.
    BotStart {
        bot_username: String,
        start_parameter: String,
        autostart: bool,
    },
    /// `InternalLinkBotStartInGroup`.
    BotStartInGroup {
        bot_username: String,
        start_parameter: String,
        administrator_rights: AdministratorRights,
    },
    /// `InternalLinkBusinessChat`.
    BusinessChat { link_name: String },
    /// `InternalLinkBuyStars`.
    BuyStars { star_count: i64, purpose: String },
    /// `InternalLinkCalls`.
    Calls { section: String },
    /// `InternalLinkConfirmPhone`.
    ConfirmPhone { hash: String, phone_number: String },
    /// `InternalLinkContacts`.
    Contacts { section: String },
    /// `InternalLinkDialogBoost`.
    DialogBoost { url: String },
    /// `InternalLinkDialogFolderInvite`.
    DialogFolderInvite { url: String },
    /// `InternalLinkDialogInvite`.
    DialogInvite { url: String },
    /// `InternalLinkDialogReferralProgram`.
    DialogReferralProgram { username: String, referral: String },
    /// `InternalLinkDialogSelection`.
    DialogSelection,
    /// `InternalLinkGame`.
    Game {
        bot_username: String,
        game_short_name: String,
    },
    /// `InternalLinkGiftAuction`.
    GiftAuction { slug: String },
    /// `InternalLinkGroupCall`.
    GroupCall { url: String },
    /// `InternalLinkInstantView`.
    InstantView { url: String, fallback_url: String },
    /// `InternalLinkInvoice`.
    Invoice { invoice_name: String },
    /// `InternalLinkLanguage`.
    Language { language_pack_id: String },
    /// `InternalLinkLiveStory`.
    LiveStory { dialog_username: String },
    /// `InternalLinkMainWebApp`.
    MainWebApp {
        bot_username: String,
        start_parameter: String,
        mode: String,
    },
    /// `InternalLinkMessage`.
    Message { url: String },
    /// `InternalLinkMessageDraft`.
    MessageDraft {
        text: crate::message_entity::FormattedText,
        contains_link: bool,
    },
    /// `InternalLinkMonoforum`.
    Monoforum { channel_username: String },
    /// `InternalLinkMyProfile`.
    MyProfile { section: String },
    /// `InternalLinkNewChannelChat`.
    NewChannelChat,
    /// `InternalLinkNewGroupChat`.
    NewGroupChat,
    /// `InternalLinkNewPrivateChat`.
    NewPrivateChat,
    /// `InternalLinkOauth`.
    Oauth { url: String },
    /// `InternalLinkPassportDataRequest`.
    PassportDataRequest {
        bot_user_id: UserId,
        scope: String,
        public_key: String,
        nonce: String,
        callback_url: String,
    },
    /// `InternalLinkPostStory`.
    PostStory { content_type: String },
    /// `InternalLinkPremiumFeatures`.
    PremiumFeatures { referrer: String },
    /// `InternalLinkPremiumGift`.
    PremiumGift { referrer: String },
    /// `InternalLinkPremiumGiftCode`.
    PremiumGiftCode { code: String },
    /// `InternalLinkProxy`.
    Proxy {
        server: String,
        port: i32,
        r#type: Option<td_api::ProxyType>,
    },
    /// `InternalLinkPublicDialog`.
    PublicDialog {
        dialog_username: String,
        draft_text: String,
        open_profile: bool,
        /// The `?free=` URL-query token (`get_url_query_unpaid_
        /// messages_token`, LinkManager.cpp:462-468).
        unpaid_messages_token: String,
    },
    /// `InternalLinkQrCodeAuthentication`.
    QrCodeAuthentication,
    /// `InternalLinkRequestManagedBot`.
    RequestManagedBot {
        manager_bot_username: String,
        bot_username: String,
        bot_name: String,
    },
    /// `InternalLinkRestorePurchases`.
    RestorePurchases,
    /// `InternalLinkSavedMessages`.
    SavedMessages,
    /// `InternalLinkSearch`.
    Search,
    /// `InternalLinkSettings`.
    Settings { path: Vec<String> },
    /// `InternalLinkSendGrams` (LinkManager.cpp:1227-1246).
    SendGrams { receiver: String, gram_amount: i64 },
    /// `InternalLinkStickerSet`.
    StickerSet {
        sticker_set_name: String,
        expect_custom_emoji: bool,
    },
    /// `InternalLinkStarGiftCollection`.
    StarGiftCollection {
        gift_owner_username: String,
        collection_id: StarGiftCollectionId,
    },
    /// `InternalLinkStory`.
    Story {
        story_poster_username: String,
        story_id: StoryId,
    },
    /// `InternalLinkStoryAlbum`.
    StoryAlbum {
        story_album_owner_username: String,
        story_album_id: StoryAlbumId,
    },
    /// `InternalLinkTextCompositionStyle`.
    TextCompositionStyle { style_name: String },
    /// `InternalLinkTonConnect` (LinkManager.cpp:1533-1556).
    TonConnect {
        version: i32,
        dapp_client_id: String,
        connect_request: String,
        return_strategy: String,
        rpc_request: String,
        trace_id: String,
    },
    /// `InternalLinkTheme`.
    Theme { theme_name: String },
    /// `InternalLinkUnknownDeepLink`.
    UnknownDeepLink { link: String },
    /// `InternalLinkUpgradedGift`.
    UpgradedGift { name: String },
    /// `InternalLinkUserPhoneNumber`.
    UserPhoneNumber {
        phone_number: String,
        draft_text: String,
        open_profile: bool,
        unpaid_messages_token: String,
    },
    /// `InternalLinkUserToken`.
    UserToken {
        token: String,
        unpaid_messages_token: String,
    },
    /// `InternalLinkVideoChat`.
    VideoChat {
        dialog_username: String,
        invite_hash: String,
        is_live_stream: bool,
    },
    /// `InternalLinkWebApp`.
    WebApp {
        bot_username: String,
        web_app_short_name: String,
        start_parameter: String,
        mode: String,
    },
}

impl InternalLink {
    /// `InternalLinkAttachMenuBot`'s constructor (cpp:579-587): the start
    /// parameter becomes the `start://` url form only when valid.
    pub fn new_attach_menu_bot(
        allowed_chat_types: Option<td_api::TargetChatTypes>,
        dialog_link: Option<InternalLink>,
        bot_username: String,
        start_parameter: &str,
    ) -> InternalLink {
        let url = if !start_parameter.is_empty() && is_valid_start_parameter(start_parameter) {
            format!("start://{start_parameter}")
        } else {
            String::new()
        };
        InternalLink::AttachMenuBot {
            allowed_chat_types,
            dialog_link: dialog_link.map(Box::new),
            bot_username,
            url,
        }
    }

    /// `InternalLinkBuyStars`' constructor (cpp:698-701): the star count is
    /// clamped into `[1, 10^12]`.
    pub fn new_buy_stars(star_count: i64, purpose: String) -> InternalLink {
        InternalLink::BuyStars {
            star_count: star_count.clamp(1, 1_000_000_000_000),
            purpose,
        }
    }

    /// `InternalLinkUserPhoneNumber`'s constructor (cpp:1440-1444): the phone
    /// number is stored with the leading `+`.
    pub fn new_user_phone_number(
        phone_number: &str,
        draft_text: String,
        open_profile: bool,
        unpaid_messages_token: String,
    ) -> InternalLink {
        InternalLink::UserPhoneNumber {
            phone_number: format!("+{phone_number}"),
            draft_text,
            open_profile,
            unpaid_messages_token,
        }
    }

    /// `get_internal_link_type_object` (LinkManager.cpp:564-575, ...): the
    /// per-class methods fold into one match. The `env` reads are the
    /// C++ `Scheduler::context()`-guarded option/dialog lookups.
    pub fn get_internal_link_type_object(&self, env: &impl LinkEnv) -> td_api::InternalLinkType {
        match self {
            InternalLink::AttachMenuBot {
                allowed_chat_types,
                dialog_link,
                bot_username,
                url,
            } => {
                let target_chat = if let Some(dialog_link) = dialog_link {
                    td_api::TargetChat::targetChatInternalLink(td_api::targetChatInternalLink {
                        link: Some(Box::new(dialog_link.get_internal_link_type_object(env))),
                    })
                } else if let Some(allowed_chat_types) = allowed_chat_types {
                    td_api::TargetChat::targetChatChosen(td_api::targetChatChosen {
                        types: Some(td_api::TargetChatTypes {
                            allow_user_chats: allowed_chat_types.allow_user_chats,
                            allow_bot_chats: allowed_chat_types.allow_bot_chats,
                            allow_group_chats: allowed_chat_types.allow_group_chats,
                            allow_channel_chats: allowed_chat_types.allow_channel_chats,
                        }),
                    })
                } else {
                    td_api::TargetChat::targetChatCurrent(td_api::targetChatCurrent)
                };
                td_api::InternalLinkType::internalLinkTypeAttachmentMenuBot(
                    td_api::internalLinkTypeAttachmentMenuBot {
                        target_chat: Some(Box::new(target_chat)),
                        bot_username: bot_username.clone(),
                        url: url.clone(),
                    },
                )
            }
            InternalLink::AuthenticationCode { code } => {
                td_api::InternalLinkType::internalLinkTypeAuthenticationCode(
                    td_api::internalLinkTypeAuthenticationCode { code: code.clone() },
                )
            }
            InternalLink::Background { background_name } => {
                td_api::InternalLinkType::internalLinkTypeBackground(
                    td_api::internalLinkTypeBackground {
                        background_name: background_name.clone(),
                    },
                )
            }
            InternalLink::BotAddToChannel {
                bot_username,
                administrator_rights,
            } => td_api::InternalLinkType::internalLinkTypeBotAddToChannel(
                td_api::internalLinkTypeBotAddToChannel {
                    bot_username: bot_username.clone(),
                    administrator_rights: Some(
                        administrator_rights.get_chat_administrator_rights_object(),
                    ),
                },
            ),
            InternalLink::BotStart {
                bot_username,
                start_parameter,
                autostart,
            } => {
                let autostart = *autostart || env.bot_start_autostart(bot_username);
                td_api::InternalLinkType::internalLinkTypeBotStart(
                    td_api::internalLinkTypeBotStart {
                        bot_username: bot_username.clone(),
                        start_parameter: start_parameter.clone(),
                        autostart,
                    },
                )
            }
            InternalLink::BotStartInGroup {
                bot_username,
                start_parameter,
                administrator_rights,
            } => td_api::InternalLinkType::internalLinkTypeBotStartInGroup(
                td_api::internalLinkTypeBotStartInGroup {
                    bot_username: bot_username.clone(),
                    start_parameter: start_parameter.clone(),
                    administrator_rights: if *administrator_rights == AdministratorRights::default()
                    {
                        None
                    } else {
                        Some(administrator_rights.get_chat_administrator_rights_object())
                    },
                },
            ),
            InternalLink::BusinessChat { link_name } => {
                td_api::InternalLinkType::internalLinkTypeBusinessChat(
                    td_api::internalLinkTypeBusinessChat {
                        link_name: link_name.clone(),
                    },
                )
            }
            InternalLink::BuyStars {
                star_count,
                purpose,
            } => td_api::InternalLinkType::internalLinkTypeStarPurchase(
                td_api::internalLinkTypeStarPurchase {
                    star_count: *star_count,
                    purpose: purpose.clone(),
                },
            ),
            InternalLink::Calls { section } => td_api::InternalLinkType::internalLinkTypeCallsPage(
                td_api::internalLinkTypeCallsPage {
                    section: section.clone(),
                },
            ),
            InternalLink::ConfirmPhone { hash, phone_number } => {
                td_api::InternalLinkType::internalLinkTypePhoneNumberConfirmation(
                    td_api::internalLinkTypePhoneNumberConfirmation {
                        hash: hash.clone(),
                        phone_number: phone_number.clone(),
                    },
                )
            }
            InternalLink::Contacts { section } => {
                td_api::InternalLinkType::internalLinkTypeContactsPage(
                    td_api::internalLinkTypeContactsPage {
                        section: section.clone(),
                    },
                )
            }
            InternalLink::DialogBoost { url } => {
                td_api::InternalLinkType::internalLinkTypeChatBoost(
                    td_api::internalLinkTypeChatBoost { url: url.clone() },
                )
            }
            InternalLink::DialogFolderInvite { url } => {
                td_api::InternalLinkType::internalLinkTypeChatFolderInvite(
                    td_api::internalLinkTypeChatFolderInvite {
                        invite_link: url.clone(),
                    },
                )
            }
            InternalLink::DialogInvite { url } => {
                td_api::InternalLinkType::internalLinkTypeChatInvite(
                    td_api::internalLinkTypeChatInvite {
                        invite_link: url.clone(),
                    },
                )
            }
            InternalLink::DialogReferralProgram { username, referral } => {
                td_api::InternalLinkType::internalLinkTypeChatAffiliateProgram(
                    td_api::internalLinkTypeChatAffiliateProgram {
                        username: username.clone(),
                        referrer: referral.clone(),
                    },
                )
            }
            InternalLink::DialogSelection => {
                td_api::InternalLinkType::internalLinkTypeChatSelection(
                    td_api::internalLinkTypeChatSelection,
                )
            }
            InternalLink::Game {
                bot_username,
                game_short_name,
            } => td_api::InternalLinkType::internalLinkTypeGame(td_api::internalLinkTypeGame {
                bot_username: bot_username.clone(),
                game_short_name: game_short_name.clone(),
            }),
            InternalLink::GiftAuction { slug } => {
                td_api::InternalLinkType::internalLinkTypeGiftAuction(
                    td_api::internalLinkTypeGiftAuction {
                        auction_id: slug.clone(),
                    },
                )
            }
            InternalLink::GroupCall { url } => td_api::InternalLinkType::internalLinkTypeGroupCall(
                td_api::internalLinkTypeGroupCall {
                    invite_link: url.clone(),
                },
            ),
            InternalLink::InstantView { url, fallback_url } => {
                td_api::InternalLinkType::internalLinkTypeInstantView(
                    td_api::internalLinkTypeInstantView {
                        url: url.clone(),
                        fallback_url: fallback_url.clone(),
                    },
                )
            }
            InternalLink::Invoice { invoice_name } => {
                td_api::InternalLinkType::internalLinkTypeInvoice(td_api::internalLinkTypeInvoice {
                    invoice_name: invoice_name.clone(),
                })
            }
            InternalLink::Language { language_pack_id } => {
                td_api::InternalLinkType::internalLinkTypeLanguagePack(
                    td_api::internalLinkTypeLanguagePack {
                        language_pack_id: language_pack_id.clone(),
                    },
                )
            }
            InternalLink::LiveStory { dialog_username } => {
                td_api::InternalLinkType::internalLinkTypeLiveStory(
                    td_api::internalLinkTypeLiveStory {
                        story_poster_username: dialog_username.clone(),
                    },
                )
            }
            InternalLink::MainWebApp {
                bot_username,
                start_parameter,
                mode,
            } => td_api::InternalLinkType::internalLinkTypeMainWebApp(
                td_api::internalLinkTypeMainWebApp {
                    bot_username: bot_username.clone(),
                    start_parameter: start_parameter.clone(),
                    mode: Some(get_web_app_open_mode_object(mode)),
                },
            ),
            InternalLink::Message { url } => {
                td_api::InternalLinkType::internalLinkTypeMessage(td_api::internalLinkTypeMessage {
                    url: url.clone(),
                })
            }
            InternalLink::MessageDraft {
                text,
                contains_link,
            } => td_api::InternalLinkType::internalLinkTypeMessageDraft(
                td_api::internalLinkTypeMessageDraft {
                    text: Some(crate::message_entity::get_formatted_text_object(
                        // `get_formatted_text_object(nullptr, text_, true, -1)` (cpp:920).
                        None::<&mut ()>,
                        text,
                        true,
                        -1,
                    )),
                    contains_link: *contains_link,
                },
            ),
            InternalLink::Monoforum { channel_username } => {
                td_api::InternalLinkType::internalLinkTypeDirectMessagesChat(
                    td_api::internalLinkTypeDirectMessagesChat {
                        channel_username: channel_username.clone(),
                    },
                )
            }
            InternalLink::MyProfile { section } => {
                td_api::InternalLinkType::internalLinkTypeMyProfilePage(
                    td_api::internalLinkTypeMyProfilePage {
                        section: section.clone(),
                    },
                )
            }
            InternalLink::NewChannelChat => {
                td_api::InternalLinkType::internalLinkTypeNewChannelChat(
                    td_api::internalLinkTypeNewChannelChat,
                )
            }
            InternalLink::NewGroupChat => td_api::InternalLinkType::internalLinkTypeNewGroupChat(
                td_api::internalLinkTypeNewGroupChat,
            ),
            InternalLink::NewPrivateChat => {
                td_api::InternalLinkType::internalLinkTypeNewPrivateChat(
                    td_api::internalLinkTypeNewPrivateChat,
                )
            }
            InternalLink::Oauth { url } => {
                td_api::InternalLinkType::internalLinkTypeOauth(td_api::internalLinkTypeOauth {
                    url: url.clone(),
                })
            }
            InternalLink::PassportDataRequest {
                bot_user_id,
                scope,
                public_key,
                nonce,
                callback_url,
            } => td_api::InternalLinkType::internalLinkTypePassportDataRequest(
                td_api::internalLinkTypePassportDataRequest {
                    bot_user_id: bot_user_id.get(),
                    scope: scope.clone(),
                    public_key: public_key.clone(),
                    nonce: nonce.clone(),
                    callback_url: callback_url.clone(),
                },
            ),
            InternalLink::PostStory { content_type } => {
                let content_type = if content_type == "photo" {
                    Some(td_api::StoryContentType::storyContentTypePhoto(
                        td_api::storyContentTypePhoto,
                    ))
                } else if content_type == "video" {
                    Some(td_api::StoryContentType::storyContentTypeVideo(
                        td_api::storyContentTypeVideo,
                    ))
                } else if content_type == "live" {
                    Some(td_api::StoryContentType::storyContentTypeLive(
                        td_api::storyContentTypeLive,
                    ))
                } else if !content_type.is_empty() {
                    Some(td_api::StoryContentType::storyContentTypeUnsupported(
                        td_api::storyContentTypeUnsupported,
                    ))
                } else {
                    None
                };
                td_api::InternalLinkType::internalLinkTypeNewStory(
                    td_api::internalLinkTypeNewStory { content_type },
                )
            }
            InternalLink::PremiumFeatures { referrer } => {
                td_api::InternalLinkType::internalLinkTypePremiumFeaturesPage(
                    td_api::internalLinkTypePremiumFeaturesPage {
                        referrer: referrer.clone(),
                    },
                )
            }
            InternalLink::PremiumGift { referrer } => {
                td_api::InternalLinkType::internalLinkTypePremiumGiftPurchase(
                    td_api::internalLinkTypePremiumGiftPurchase {
                        referrer: referrer.clone(),
                    },
                )
            }
            InternalLink::PremiumGiftCode { code } => {
                td_api::InternalLinkType::internalLinkTypePremiumGiftCode(
                    td_api::internalLinkTypePremiumGiftCode { code: code.clone() },
                )
            }
            InternalLink::Proxy {
                server,
                port,
                r#type,
            } => {
                let proxy = r#type.as_ref().map(|proxy_type| {
                    let proxy_type = match proxy_type {
                        td_api::ProxyType::proxyTypeSocks5(type_socks) => {
                            td_api::ProxyType::proxyTypeSocks5(td_api::proxyTypeSocks5 {
                                username: type_socks.username.clone(),
                                password: type_socks.password.clone(),
                            })
                        }
                        td_api::ProxyType::proxyTypeMtproto(type_mtproto) => {
                            td_api::ProxyType::proxyTypeMtproto(td_api::proxyTypeMtproto {
                                secret: type_mtproto.secret.clone(),
                            })
                        }
                        // UNREACHABLE in the C++ (cpp:1090)
                        td_api::ProxyType::proxyTypeHttp(_) => {
                            unreachable!("proxyTypeHttp cannot appear in a parsed proxy link")
                        }
                        // 1.8.70 proxyTypeWeb — the parsed link form
                        // round-trips like the socks/mtproto forms
                        // (LinkManager.cpp:1229-1232).
                        td_api::ProxyType::proxyTypeWeb(type_web) => {
                            td_api::ProxyType::proxyTypeWeb(td_api::proxyTypeWeb {
                                path: type_web.path.clone(),
                                secret: type_web.secret.clone(),
                            })
                        }
                    };
                    td_api::Proxy {
                        server: server.clone(),
                        port: *port,
                        r#type: Some(proxy_type),
                    }
                });
                td_api::InternalLinkType::internalLinkTypeProxy(td_api::internalLinkTypeProxy {
                    proxy,
                })
            }
            InternalLink::PublicDialog {
                dialog_username,
                draft_text,
                open_profile,
                unpaid_messages_token,
            } => td_api::InternalLinkType::internalLinkTypePublicChat(
                td_api::internalLinkTypePublicChat {
                    chat_username: dialog_username.clone(),
                    draft_text: draft_text.clone(),
                    open_profile: *open_profile,
                    unpaid_messages_token: unpaid_messages_token.clone(),
                },
            ),
            InternalLink::QrCodeAuthentication => {
                td_api::InternalLinkType::internalLinkTypeQrCodeAuthentication(
                    td_api::internalLinkTypeQrCodeAuthentication,
                )
            }
            InternalLink::RequestManagedBot {
                manager_bot_username,
                bot_username,
                bot_name,
            } => {
                // cpp:1223-1236 (230f87181): a username that ends in
                // "bot" or in one of the Premium-granted
                // `bot_allowed_suffixes` is taken as-is; otherwise the
                // "bot" suffix is appended.
                let username = bot_username.to_ascii_lowercase();
                let mut is_found = username.ends_with("bot");
                let suffixes = env.get_option_string("bot_allowed_suffixes");
                // `td::full_split`: an empty option yields no parts.
                if !is_found && !suffixes.is_empty() {
                    for suffix in suffixes.split(' ') {
                        if username.ends_with(suffix) {
                            is_found = true;
                        }
                    }
                }
                let suggested = if is_found {
                    bot_username.clone()
                } else {
                    format!("{bot_username}bot")
                };
                td_api::InternalLinkType::internalLinkTypeRequestManagedBot(
                    td_api::internalLinkTypeRequestManagedBot {
                        manager_bot_username: manager_bot_username.clone(),
                        suggested_bot_username: suggested,
                        suggested_bot_name: bot_name.clone(),
                    },
                )
            }
            InternalLink::RestorePurchases => {
                td_api::InternalLinkType::internalLinkTypeRestorePurchases(
                    td_api::internalLinkTypeRestorePurchases,
                )
            }
            InternalLink::SavedMessages => td_api::InternalLinkType::internalLinkTypeSavedMessages(
                td_api::internalLinkTypeSavedMessages,
            ),
            InternalLink::Search => {
                td_api::InternalLinkType::internalLinkTypeSearch(td_api::internalLinkTypeSearch)
            }
            InternalLink::Settings { path } => {
                let section = get_settings_section_object(path);
                td_api::InternalLinkType::internalLinkTypeSettings(
                    td_api::internalLinkTypeSettings { section },
                )
            }
            InternalLink::SendGrams {
                receiver,
                gram_amount,
            } => {
                // LinkManager.cpp:1231-1244.
                let receiver_object = if receiver.is_empty() {
                    None
                } else if let Some(username) = receiver.strip_prefix('@') {
                    Some(
                        td_api::TonWalletTransferReceiver::tonWalletTransferReceiverUser(
                            td_api::tonWalletTransferReceiverUser {
                                username: username.to_owned(),
                            },
                        ),
                    )
                } else {
                    Some(
                        td_api::TonWalletTransferReceiver::tonWalletTransferReceiverAddress(
                            td_api::tonWalletTransferReceiverAddress {
                                address: receiver.clone(),
                            },
                        ),
                    )
                };
                td_api::InternalLinkType::internalLinkTypeTonWalletTransfer(
                    td_api::internalLinkTypeTonWalletTransfer {
                        receiver: receiver_object,
                        gram_amount: *gram_amount,
                    },
                )
            }
            InternalLink::StickerSet {
                sticker_set_name,
                expect_custom_emoji,
            } => td_api::InternalLinkType::internalLinkTypeStickerSet(
                td_api::internalLinkTypeStickerSet {
                    sticker_set_name: sticker_set_name.clone(),
                    expect_custom_emoji: *expect_custom_emoji,
                },
            ),
            InternalLink::StarGiftCollection {
                gift_owner_username,
                collection_id,
            } => td_api::InternalLinkType::internalLinkTypeGiftCollection(
                td_api::internalLinkTypeGiftCollection {
                    gift_owner_username: gift_owner_username.clone(),
                    collection_id: collection_id.get(),
                },
            ),
            InternalLink::Story {
                story_poster_username,
                story_id,
            } => td_api::InternalLinkType::internalLinkTypeStory(td_api::internalLinkTypeStory {
                story_poster_username: story_poster_username.clone(),
                story_id: story_id.get(),
            }),
            InternalLink::StoryAlbum {
                story_album_owner_username,
                story_album_id,
            } => td_api::InternalLinkType::internalLinkTypeStoryAlbum(
                td_api::internalLinkTypeStoryAlbum {
                    story_album_owner_username: story_album_owner_username.clone(),
                    story_album_id: story_album_id.get(),
                },
            ),
            InternalLink::TextCompositionStyle { style_name } => {
                td_api::InternalLinkType::internalLinkTypeTextCompositionStyle(
                    td_api::internalLinkTypeTextCompositionStyle {
                        style_name: style_name.clone(),
                    },
                )
            }
            InternalLink::TonConnect {
                version,
                dapp_client_id,
                connect_request,
                return_strategy,
                rpc_request,
                trace_id,
            } => {
                // LinkManager.cpp:1551 — the connect request re-validates
                // on conversion (a parse-time-valid request stays valid).
                let request = get_ton_connect_connect_request_object(connect_request)
                    .expect("a validated connect request");
                td_api::InternalLinkType::internalLinkTypeTonConnect(
                    td_api::internalLinkTypeTonConnect {
                        version: *version,
                        dapp_client_id: dapp_client_id.clone(),
                        connect_request: Some(request),
                        return_strategy: return_strategy.clone(),
                        rpc_request: rpc_request.clone(),
                        trace_id: trace_id.clone(),
                    },
                )
            }
            InternalLink::Theme { theme_name } => {
                td_api::InternalLinkType::internalLinkTypeTheme(td_api::internalLinkTypeTheme {
                    theme_name: theme_name.clone(),
                })
            }
            InternalLink::UnknownDeepLink { link } => {
                td_api::InternalLinkType::internalLinkTypeUnknownDeepLink(
                    td_api::internalLinkTypeUnknownDeepLink { link: link.clone() },
                )
            }
            InternalLink::UpgradedGift { name } => {
                td_api::InternalLinkType::internalLinkTypeUpgradedGift(
                    td_api::internalLinkTypeUpgradedGift { name: name.clone() },
                )
            }
            InternalLink::UserPhoneNumber {
                phone_number,
                draft_text,
                open_profile,
                unpaid_messages_token,
            } => td_api::InternalLinkType::internalLinkTypeUserPhoneNumber(
                td_api::internalLinkTypeUserPhoneNumber {
                    phone_number: phone_number.clone(),
                    draft_text: draft_text.clone(),
                    open_profile: *open_profile,
                    unpaid_messages_token: unpaid_messages_token.clone(),
                },
            ),
            InternalLink::UserToken {
                token,
                unpaid_messages_token,
            } => td_api::InternalLinkType::internalLinkTypeUserToken(
                td_api::internalLinkTypeUserToken {
                    token: token.clone(),
                    unpaid_messages_token: unpaid_messages_token.clone(),
                },
            ),
            InternalLink::VideoChat {
                dialog_username,
                invite_hash,
                is_live_stream,
            } => td_api::InternalLinkType::internalLinkTypeVideoChat(
                td_api::internalLinkTypeVideoChat {
                    chat_username: dialog_username.clone(),
                    invite_hash: invite_hash.clone(),
                    is_live_stream: *is_live_stream,
                },
            ),
            InternalLink::WebApp {
                bot_username,
                web_app_short_name,
                start_parameter,
                mode,
            } => td_api::InternalLinkType::internalLinkTypeWebApp(td_api::internalLinkTypeWebApp {
                bot_username: bot_username.clone(),
                web_app_short_name: web_app_short_name.clone(),
                start_parameter: start_parameter.clone(),
                mode: Some(get_web_app_open_mode_object(mode)),
            }),
        }
    }
}

/// `get_appearance_settings_subsections` (LinkManager.cpp:177-192).
fn get_appearance_settings_subsections() -> &'static [&'static str] {
    &[
        "themes",
        "themes/edit",
        "themes/create",
        "wallpapers",
        "wallpapers/edit",
        "wallpapers/set",
        "wallpapers/choose-photo",
        "your-color/profile",
        "your-color/profile/add-icons",
        "your-color/profile/use-gift",
        "your-color/profile/reset",
        "your-color/name",
        "your-color/name/add-icons",
        "your-color/name/use-gift",
        "night-mode",
        "auto-night-mode",
        "text-size",
        "text-size/use-system",
        "message-corners",
        "animations",
        "stickers-and-emoji",
        "stickers-and-emoji/edit",
        "stickers-and-emoji/trending",
        "stickers-and-emoji/archived",
        "stickers-and-emoji/archived/edit",
        "stickers-and-emoji/emoji",
        "stickers-and-emoji/emoji/edit",
        "stickers-and-emoji/emoji/archived",
        "stickers-and-emoji/emoji/archived/edit",
        "stickers-and-emoji/emoji/suggest",
        "stickers-and-emoji/emoji/quick-reaction",
        "stickers-and-emoji/emoji/quick-reaction/choose",
        "stickers-and-emoji/suggest-by-emoji",
        "stickers-and-emoji/large-emoji",
        "stickers-and-emoji/dynamic-order",
        "stickers-and-emoji/emoji/show-more",
        "app-icon",
        // no formatting
        "tap-for-next-media",
    ]
}

/// `get_business_settings_subsections` (cpp:194-197).
fn get_business_settings_subsections() -> &'static [&'static str] {
    &["do-not-hide-ads"]
}

/// `get_data_settings_subsections` (cpp:199-220).
fn get_data_settings_subsections() -> &'static [&'static str] {
    &[
        "storage",
        "storage/edit",
        "storage/auto-remove",
        "storage/clear-cache",
        "storage/max-cache",
        "usage",
        "usage/mobile",
        "usage/wifi",
        "usage/reset",
        "usage/roaming",
        "auto-download/mobile",
        "auto-download/mobile/enable",
        "auto-download/mobile/usage",
        "auto-download/mobile/photos",
        "auto-download/mobile/stories",
        "auto-download/mobile/videos",
        "auto-download/mobile/files",
        "auto-download/wifi",
        "auto-download/wifi/enable",
        "auto-download/wifi/usage",
        "auto-download/wifi/photos",
        "auto-download/wifi/stories",
        "auto-download/wifi/videos",
        "auto-download/wifi/files",
        "auto-download/roaming",
        "auto-download/roaming/enable",
        "auto-download/roaming/usage",
        "auto-download/roaming/photos",
        "auto-download/roaming/stories",
        "auto-download/roaming/videos",
        "auto-download/roaming/files",
        "auto-download/reset",
        "save-to-photos/chats",
        "save-to-photos/chats/max-video-size",
        "save-to-photos/chats/add-exception",
        "save-to-photos/chats/delete-all",
        "save-to-photos/groups",
        "save-to-photos/groups/max-video-size",
        "save-to-photos/groups/add-exception",
        "save-to-photos/groups/delete-all",
        "save-to-photos/channels",
        "save-to-photos/channels/max-video-size",
        "save-to-photos/channels/add-exception",
        "save-to-photos/channels/delete-all",
        "less-data-calls",
        "open-links",
        "share-sheet",
        "share-sheet/suggested-chats",
        "share-sheet/suggest-by",
        "share-sheet/reset",
        "saved-edited-photos",
        "pause-music",
        "raise-to-listen",
        "raise-to-speak",
        "show-18-content",
        "proxy",
        "proxy/edit",
        "proxy/use-proxy",
        "proxy/add-proxy",
        "proxy/share-list",
        // no formatting
        "proxy/use-for-calls",
    ]
}

/// `get_device_settings_subsections` (cpp:222-225).
fn get_device_settings_subsections() -> &'static [&'static str] {
    &[
        "edit",
        "link-desktop",
        "terminate-sessions",
        "auto-terminate",
    ]
}

/// `get_edit_profile_settings_subsections` (cpp:227-232).
fn get_edit_profile_settings_subsections() -> &'static [&'static str] {
    &[
        "set-photo",
        "first-name",
        "last-name",
        "bio",
        "birthday",
        "change-number",
        "username",
        "your-color",
        "channel",
        "add-account",
        "log-out",
    ]
}

/// `get_edit_profile_other_settings_subsections` (cpp:234-241).
fn get_edit_profile_other_settings_subsections() -> &'static [&'static str] {
    &[
        "emoji-status",
        "profile-color/profile",
        "profile-color/profile/add-icons",
        "profile-color/profile/use-gift",
        "profile-color/name",
        "profile-color/name/add-icons",
        "profile-color/name/use-gift",
        // no formatting
        "profile-photo/use-emoji",
    ]
}

/// `get_folder_settings_subsections` (cpp:243-246).
fn get_folder_settings_subsections() -> &'static [&'static str] {
    &["edit", "create", "add-recommended", "show-tags", "tab-view"]
}

/// `get_in_app_browser_settings_subsections` (cpp:248-252).
fn get_in_app_browser_settings_subsections() -> &'static [&'static str] {
    &[
        "enable-browser",
        "clear-cookies",
        "clear-cache",
        "history",
        "clear-history",
        "never-open",
        "clear-list",
        "search",
    ]
}

/// `get_language_settings_subsections` (cpp:254-257).
fn get_language_settings_subsections() -> &'static [&'static str] {
    &["show-button", "translate-chats", "do-not-translate"]
}

/// `get_my_stars_settings_subsections` (cpp:259-262).
fn get_my_stars_settings_subsections() -> &'static [&'static str] {
    &["top-up", "stats", "gift", "earn"]
}

/// `get_notification_settings_subsections` (cpp:264-282).
fn get_notification_settings_subsections() -> &'static [&'static str] {
    &[
        "accounts",
        "private-chats",
        "private-chats/edit",
        "private-chats/show",
        "private-chats/preview",
        "private-chats/sound",
        "private-chats/add-exception",
        "private-chats/delete-exceptions",
        "private-chats/light-color",
        "private-chats/vibrate",
        "private-chats/priority",
        "groups",
        "groups/edit",
        "groups/show",
        "groups/preview",
        "groups/sound",
        "groups/add-exception",
        "groups/delete-exceptions",
        "groups/light-color",
        "groups/vibrate",
        "groups/priority",
        "channels",
        "channels/edit",
        "channels/show",
        "channels/preview",
        "channels/sound",
        "channels/add-exception",
        "channels/delete-exceptions",
        "channels/light-color",
        "channels/vibrate",
        "channels/priority",
        "stories",
        "stories/new",
        "stories/important",
        "stories/show-sender",
        "stories/sound",
        "stories/add-exception",
        "stories/delete-exceptions",
        "stories/light-color",
        "stories/vibrate",
        "stories/priority",
        "reactions",
        "reactions/messages",
        "reactions/stories",
        "reactions/show-sender",
        "reactions/sound",
        "reactions/light-color",
        "reactions/vibrate",
        "reactions/priority",
        "in-app-sounds",
        "in-app-vibrate",
        "in-app-preview",
        "in-chat-sounds",
        "in-app-popup",
        "lock-screen-names",
        "include-channels",
        "include-muted-chats",
        "count-unread-messages",
        "new-contacts",
        "pinned-messages",
        "reset",
        // no formatting
        "web",
    ]
}

/// `get_power_saving_settings_subsections` (cpp:284-290).
fn get_power_saving_settings_subsections() -> &'static [&'static str] {
    &[
        "videos",
        "gifs",
        "stickers",
        "emoji",
        "effects",
        "preload",
        "background",
        "call-animations",
        "particles",
        // no formatting
        "transitions",
    ]
}

/// `get_privacy_settings_subsections` (cpp:292-312).
fn get_privacy_settings_subsections() -> &'static [&'static str] {
    &[
        "blocked",
        "blocked/edit",
        "blocked/block-user",
        "blocked/block-user/chats",
        "blocked/block-user/contacts",
        "active-websites",
        "active-websites/edit",
        "active-websites/disconnect-all",
        "passcode",
        "passcode/disable",
        "passcode/change",
        "passcode/auto-lock",
        "passcode/face-id",
        "passcode/fingerprint",
        "2sv",
        "2sv/change",
        "2sv/disable",
        "2sv/change-email",
        "passkey",
        "passkey/create",
        "auto-delete",
        "auto-delete/set-custom",
        "login-email",
        "phone-number",
        "phone-number/never",
        "phone-number/always",
        "last-seen",
        "last-seen/never",
        "last-seen/always",
        "last-seen/hide-read-time",
        "profile-photos",
        "profile-photos/never",
        "profile-photos/always",
        "profile-photos/set-public",
        "profile-photos/update-public",
        "profile-photos/remove-public",
        "bio",
        "bio/never",
        "bio/always",
        "gifts",
        "gifts/show-icon",
        "gifts/never",
        "gifts/always",
        "gifts/accepted-types",
        "birthday",
        "birthday/add",
        "birthday/never",
        "birthday/always",
        "saved-music",
        "saved-music/never",
        "saved-music/always",
        "forwards",
        "forwards/never",
        "forwards/always",
        "calls",
        "calls/never",
        "calls/always",
        "calls/p2p",
        "calls/p2p/never",
        "calls/p2p/always",
        "calls/ios-integration",
        "voice",
        "voice/never",
        "voice/always",
        "messages",
        "messages/set-price",
        "messages/exceptions",
        "invites",
        "invites/never",
        "invites/always",
        "self-destruct",
        "data-settings",
        "data-settings/sync-contacts",
        "data-settings/delete-synced",
        "data-settings/suggest-contacts",
        "data-settings/delete-cloud-drafts",
        "data-settings/clear-payment-info",
        "data-settings/link-previews",
        "data-settings/bot-settings",
        "data-settings/map-provider",
        // no formatting
        "archive-and-mute",
    ]
}

/// `get_qr_code_settings_subsections` (cpp:314-317).
fn get_qr_code_settings_subsections() -> &'static [&'static str] {
    &["share", "scan"]
}

/// `get_send_gift_settings_subsections` (cpp:319-322).
fn get_send_gift_settings_subsections() -> &'static [&'static str] {
    &["self"]
}

/// `get_calls_sections` (cpp:324-327).
fn get_calls_sections() -> &'static [&'static str] {
    &["all", "missed", "edit", "show-tab", "start-call"]
}

/// `get_contacts_sections` (cpp:329-332).
fn get_contacts_sections() -> &'static [&'static str] {
    &["search", "sort", "new", "invite", "manage"]
}

/// `get_my_profile_sections` (cpp:334-337).
fn get_my_profile_sections() -> &'static [&'static str] {
    &[
        "posts",
        "posts/all-stories",
        "posts/add-album",
        "gifts",
        "archived-posts",
    ]
}

/// `InternalLinkSettings::get_internal_link_type_object`'s section table
/// (cpp:1170-1329) — the `path`-to-`SettingsSection` decision table.
fn get_settings_section_object(path: &[String]) -> Option<td_api::SettingsSection> {
    let first = path.first()?;

    let mut subsection = String::new();
    if path.len() >= 2 {
        subsection = path[1].clone();
        for segment in &path[2..] {
            subsection.push('/');
            subsection.push_str(segment);
        }
    }

    let in_table = |table: &[&'static str]| table.contains(&subsection.as_str());
    use td_api::SettingsSection as S;

    if first == "appearance" {
        return Some(if in_table(get_appearance_settings_subsections()) {
            S::settingsSectionAppearance(td_api::settingsSectionAppearance { subsection })
        } else {
            S::settingsSectionAppearance(td_api::settingsSectionAppearance {
                subsection: String::new(),
            })
        });
    }
    if first == "ask-question" {
        return Some(S::settingsSectionAskQuestion(
            td_api::settingsSectionAskQuestion,
        ));
    }
    if first == "auto_delete" {
        return Some(S::settingsSectionPrivacyAndSecurity(
            td_api::settingsSectionPrivacyAndSecurity {
                subsection: "auto-delete".to_owned(),
            },
        ));
    }
    if first == "business" {
        return Some(if in_table(get_business_settings_subsections()) {
            S::settingsSectionBusiness(td_api::settingsSectionBusiness { subsection })
        } else {
            S::settingsSectionBusiness(td_api::settingsSectionBusiness {
                subsection: String::new(),
            })
        });
    }
    if first == "change_number" {
        return Some(S::settingsSectionEditProfile(
            td_api::settingsSectionEditProfile {
                subsection: "change-number".to_owned(),
            },
        ));
    }
    if first == "chat" && path.len() >= 2 && path[1] == "browser" {
        let subsection = if path.len() == 2 {
            String::new()
        } else {
            path[2].clone()
        };
        return Some(
            if get_in_app_browser_settings_subsections().contains(&subsection.as_str()) {
                S::settingsSectionInAppBrowser(td_api::settingsSectionInAppBrowser { subsection })
            } else {
                S::settingsSectionInAppBrowser(td_api::settingsSectionInAppBrowser {
                    subsection: String::new(),
                })
            },
        );
    }
    if first == "data" {
        return Some(if in_table(get_data_settings_subsections()) {
            S::settingsSectionDataAndStorage(td_api::settingsSectionDataAndStorage { subsection })
        } else {
            S::settingsSectionDataAndStorage(td_api::settingsSectionDataAndStorage {
                subsection: String::new(),
            })
        });
    }
    if first == "devices" {
        return Some(if in_table(get_device_settings_subsections()) {
            S::settingsSectionDevices(td_api::settingsSectionDevices { subsection })
        } else {
            S::settingsSectionDevices(td_api::settingsSectionDevices {
                subsection: String::new(),
            })
        });
    }
    if first == "edit" {
        return Some(if in_table(get_edit_profile_settings_subsections()) {
            S::settingsSectionEditProfile(td_api::settingsSectionEditProfile { subsection })
        } else {
            S::settingsSectionEditProfile(td_api::settingsSectionEditProfile {
                subsection: String::new(),
            })
        });
    }
    if first == "edit_profile" {
        return Some(S::settingsSectionEditProfile(
            td_api::settingsSectionEditProfile {
                subsection: String::new(),
            },
        ));
    }
    if first == "emoji-status" || first == "profile-color" || first == "profile-photo" {
        let subsection = if subsection.is_empty() {
            first.clone()
        } else {
            format!("{first}/{subsection}")
        };
        if get_edit_profile_other_settings_subsections().contains(&subsection.as_str()) {
            return Some(S::settingsSectionEditProfile(
                td_api::settingsSectionEditProfile { subsection },
            ));
        }
        if first == "emoji-status" {
            return Some(S::settingsSectionEditProfile(
                td_api::settingsSectionEditProfile {
                    subsection: first.clone(),
                },
            ));
        }
        return Some(S::settingsSectionEditProfile(
            td_api::settingsSectionEditProfile {
                subsection: String::new(),
            },
        ));
    }
    if first == "faq" {
        return Some(S::settingsSectionFaq(td_api::settingsSectionFaq));
    }
    if first == "features" {
        return Some(S::settingsSectionFeatures(td_api::settingsSectionFeatures));
    }
    if first == "folders" {
        return Some(if in_table(get_folder_settings_subsections()) {
            S::settingsSectionChatFolders(td_api::settingsSectionChatFolders { subsection })
        } else {
            S::settingsSectionChatFolders(td_api::settingsSectionChatFolders {
                subsection: String::new(),
            })
        });
    }
    if first == "language" {
        return Some(if in_table(get_language_settings_subsections()) {
            S::settingsSectionLanguage(td_api::settingsSectionLanguage { subsection })
        } else {
            S::settingsSectionLanguage(td_api::settingsSectionLanguage {
                subsection: String::new(),
            })
        });
    }
    if first == "login_email" {
        return Some(S::settingsSectionPrivacyAndSecurity(
            td_api::settingsSectionPrivacyAndSecurity {
                subsection: "login-email".to_owned(),
            },
        ));
    }
    if first == "notifications" {
        return Some(if in_table(get_notification_settings_subsections()) {
            S::settingsSectionNotifications(td_api::settingsSectionNotifications { subsection })
        } else {
            S::settingsSectionNotifications(td_api::settingsSectionNotifications {
                subsection: String::new(),
            })
        });
    }
    if first == "power-saving" {
        return Some(if in_table(get_power_saving_settings_subsections()) {
            S::settingsSectionPowerSaving(td_api::settingsSectionPowerSaving { subsection })
        } else {
            S::settingsSectionPowerSaving(td_api::settingsSectionPowerSaving {
                subsection: String::new(),
            })
        });
    }
    if first == "password" {
        return Some(S::settingsSectionPrivacyAndSecurity(
            td_api::settingsSectionPrivacyAndSecurity {
                subsection: "2sv".to_owned(),
            },
        ));
    }
    if first == "phone_privacy" {
        return Some(S::settingsSectionPrivacyAndSecurity(
            td_api::settingsSectionPrivacyAndSecurity {
                subsection: "phone-number".to_owned(),
            },
        ));
    }
    if first == "premium" {
        return Some(S::settingsSectionPremium(td_api::settingsSectionPremium));
    }
    if first == "privacy" {
        return Some(if in_table(get_privacy_settings_subsections()) {
            S::settingsSectionPrivacyAndSecurity(td_api::settingsSectionPrivacyAndSecurity {
                subsection,
            })
        } else {
            S::settingsSectionPrivacyAndSecurity(td_api::settingsSectionPrivacyAndSecurity {
                subsection: String::new(),
            })
        });
    }
    if first == "privacy-policy" {
        return Some(S::settingsSectionPrivacyPolicy(
            td_api::settingsSectionPrivacyPolicy,
        ));
    }
    if first == "qr-code" {
        return Some(if in_table(get_qr_code_settings_subsections()) {
            S::settingsSectionQrCode(td_api::settingsSectionQrCode { subsection })
        } else {
            S::settingsSectionQrCode(td_api::settingsSectionQrCode {
                subsection: String::new(),
            })
        });
    }
    if first == "search" {
        return Some(S::settingsSectionSearch(td_api::settingsSectionSearch));
    }
    if first == "send-gift" {
        return Some(if in_table(get_send_gift_settings_subsections()) {
            S::settingsSectionSendGift(td_api::settingsSectionSendGift { subsection })
        } else {
            S::settingsSectionSendGift(td_api::settingsSectionSendGift {
                subsection: String::new(),
            })
        });
    }
    if first == "stars" {
        return Some(if in_table(get_my_stars_settings_subsections()) {
            S::settingsSectionMyStars(td_api::settingsSectionMyStars { subsection })
        } else {
            S::settingsSectionMyStars(td_api::settingsSectionMyStars {
                subsection: String::new(),
            })
        });
    }
    if first == "themes" {
        return Some(S::settingsSectionAppearance(
            td_api::settingsSectionAppearance {
                subsection: String::new(),
            },
        ));
    }
    if first == "grams" || first == "ton" {
        return Some(S::settingsSectionMyGrams(td_api::settingsSectionMyGrams));
    }
    None
}

/// The `LinkType` (LinkManager.h:198).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LinkType {
    External,
    TMe,
    Tg,
    Telegraph,
    TonConnect,
}

/// The `LinkInfo` (LinkManager.h:200-203).
#[derive(Debug)]
struct LinkInfo {
    link_type: LinkType,
    query: String,
}

/// `LinkManager::get_link_info` (LinkManager.cpp:1996-2105).
fn get_link_info(env: &impl LinkEnv, link: &str) -> LinkInfo {
    let mut result = LinkInfo {
        link_type: LinkType::External,
        query: String::new(),
    };
    if link.is_empty() {
        return result;
    }
    // link.truncate(link.find('#'))
    let link = match link.find('#') {
        Some(pos) => &link[..pos],
        None => link,
    };

    let mut is_tg = false;
    let mut link = link;
    if tolower_begins_with(link, "tg:") {
        link = &link[3..];
        if link.starts_with("//") {
            link = &link[2..];
        }
        is_tg = true;
    } else if tolower_begins_with(link, "tc:") {
        // 53449a3a9: the tc: scheme carries a TonConnect query directly.
        let mut link = &link[3..];
        if link.starts_with("//") {
            link = &link[2..];
        }
        result.link_type = LinkType::TonConnect;
        result.query = link.to_owned();
        return result;
    }

    let http_url = match parse_url(link) {
        tdutils::status::Result::Ok(http_url) => http_url,
        tdutils::status::Result::Err(_) => return result,
    };
    if !http_url.userinfo.is_empty() || http_url.is_ipv6 {
        return result;
    }

    if is_tg {
        if tolower_begins_with(link, "http://")
            || http_url.protocol == Protocol::Https
            || http_url.specified_port != 0
        {
            return result;
        }
        result.link_type = LinkType::Tg;
        result.query = link.to_owned();
        return result;
    }

    if http_url.port != 80 && http_url.port != 443 {
        return result;
    }

    let host = url_decode(&http_url.host, false).to_lowercase();
    if host.ends_with(".t.me") && host.len() >= 9 && host.find('.') == Some(host.len() - 5) {
        let subdomain = &host[..host.len() - 5];
        const DISALLOWED_SUBDOMAINS: &[&str] = &[
            "addemoji",
            "addlist",
            "addstickers",
            "addstyle",
            "addtheme",
            "auction",
            "auth",
            "boost",
            "call",
            "confirmphone",
            "contact",
            "giftcode",
            "invoice",
            "joinchat",
            "login",
            "m",
            "nft",
            "proxy",
            "setlanguage",
            "share",
            "socks",
            "web",
            "webproxy",
            "a",
            "k",
            "z",
        ];
        if crate::misc::is_valid_username(subdomain) && !DISALLOWED_SUBDOMAINS.contains(&subdomain)
        {
            result.link_type = LinkType::TMe;
            result.query = format!("/{subdomain}{}", http_url.query);
            return result;
        }
    }
    let host = match host.strip_prefix("www.") {
        Some(rest) => rest,
        None => host.as_str(),
    };

    let mut t_me_urls = vec!["t.me", "telegram.me", "telegram.dog"];
    // The `Scheduler::context() != nullptr` branch (cpp:2062-2071) — "for
    // tests only": the collapsed env always reads the option.
    let cur_t_me_url = env.get_option_string("t_me_url");
    if tolower_begins_with(&cur_t_me_url, "http://")
        || tolower_begins_with(&cur_t_me_url, "https://")
    {
        let t_me_url = &cur_t_me_url[if cur_t_me_url.as_bytes()[4] == b's' {
            8
        } else {
            7
        }..];
        if !t_me_urls.contains(&t_me_url) {
            t_me_urls.push(t_me_url);
        }
    }

    for t_me_url in t_me_urls {
        if host == t_me_url {
            result.link_type = LinkType::TMe;
            let mut query: &str = &http_url.query;
            loop {
                if let Some(rest) = query.strip_prefix("/s/") {
                    query = rest_with_slash(query, 2);
                    let _ = rest;
                    continue;
                }
                if query.starts_with("/%73/") {
                    query = rest_with_slash(query, 4);
                    continue;
                }
                break;
            }
            result.query = query.to_owned();
            return result;
        }
    }

    if http_url.query.len() > 1 {
        for telegraph_url in ["telegra.ph", "te.legra.ph", "graph.org"] {
            if host == telegraph_url {
                result.link_type = LinkType::Telegraph;
                result.query = http_url.query.clone();
                return result;
            }
        }
    }
    result
}

/// The `remove_prefix(2)`/`remove_prefix(4)` form of the `/s/` loop
/// (cpp:2078-2088): the leading slash stays.
fn rest_with_slash(query: &str, prefix_len: usize) -> &str {
    &query[prefix_len..]
}

/// `LinkManager::is_internal_link` (LinkManager.cpp:2107-2110).
pub fn is_internal_link(env: &impl LinkEnv, link: &str) -> bool {
    get_link_info(env, link).link_type != LinkType::External
}

/// `LinkManager::parse_internal_link` (LinkManager.cpp:2112-2127).
pub fn parse_internal_link(
    env: &mut impl LinkEnv,
    link: &str,
    is_trusted: bool,
) -> Option<InternalLink> {
    let info = get_link_info(env, link);
    match info.link_type {
        LinkType::External => None,
        LinkType::Tg => parse_tg_link_query(env, &info.query, is_trusted),
        LinkType::TMe => parse_t_me_link_query(env, &info.query, is_trusted),
        LinkType::Telegraph => Some(InternalLink::InstantView {
            url: format!("https://telegra.ph{}", info.query),
            fallback_url: link.to_owned(),
        }),
        LinkType::TonConnect => parse_ton_connect_link_query(&info.query),
    }
}

/// The `CopyArg` streamer (cpp:2140-2155): appends `?name[=value]` (first
/// arg) or `&name[=value]` for an argument present in the query; a missing
/// argument contributes nothing.
fn copy_arg(out: &mut String, url_query: &HttpUrlQuery, is_first: &mut bool, name: &str) {
    let arg = url_query.get_arg(name).unwrap_or("");
    if arg.is_empty() {
        if !url_query.args.iter().any(|(key, _)| key == name) {
            return;
        }
        let c = if *is_first { '?' } else { '&' };
        *is_first = false;
        out.push(c);
        out.push_str(name);
        return;
    }
    let c = if *is_first { '?' } else { '&' };
    *is_first = false;
    out.push(c);
    out.push_str(name);
    out.push('=');
    out.push_str(&url_encode(arg));
}

/// The `thread = PSTRING() << copy_arg("thread")` form: renders one
/// `copy_arg` into a fresh string, advancing the shared first-arg state.
fn copy_arg_to_string(url_query: &HttpUrlQuery, is_first: &mut bool, name: &str) -> String {
    let mut result = String::new();
    copy_arg(&mut result, url_query, is_first, name);
    result
}

/// `LinkManager::get_internal_link_message_draft` (cpp:2923-2953).
/// `get_ton_connect_connect_request_object` (LinkManager.cpp:619-649) —
/// the JSON connect-request contract of the TonConnect deep links.
fn get_ton_connect_connect_request_object(
    r: &str,
) -> std::result::Result<td_api::TonConnectConnectRequest, Status> {
    use tdutils::json::{JsonValue, JsonValueType, json_decode};
    let mut value = match json_decode(r) {
        tdutils::status::Result::Ok(value) => value,
        tdutils::status::Result::Err(e) => return Err(Status::error(400, e.message())),
    };
    if value.ty() != JsonValueType::Object {
        return Err(Status::error(400, "Request must be an Object"));
    }
    let mut request = std::mem::replace(&mut value, JsonValue::Null);
    let JsonValue::Object(ref mut request) = request else {
        unreachable!()
    };
    let manifest_url = match request.get_required_string_field("manifestUrl") {
        tdutils::status::Result::Ok(url) => url,
        tdutils::status::Result::Err(e) => return Err(Status::error(400, e.message())),
    };
    let checked_manifest_url = check_link(&manifest_url, false, false)?;
    let mut connect_items = Vec::new();
    let items = match request.extract_optional_field("items", JsonValueType::Array) {
        tdutils::status::Result::Ok(items) => items,
        tdutils::status::Result::Err(e) => return Err(Status::error(400, e.message())),
    };
    if items.ty() == JsonValueType::Array {
        for item_value in items.get_array() {
            if item_value.ty() != JsonValueType::Object {
                return Err(Status::error(400, "Item must be an Object"));
            }
            let JsonValue::Object(item) = item_value else {
                unreachable!()
            };
            let item_name = match item.get_required_string_field("name") {
                tdutils::status::Result::Ok(name) => name,
                tdutils::status::Result::Err(e) => return Err(Status::error(400, e.message())),
            };
            if item_name == "ton_addr" {
                let network = match item.get_optional_string_field("network", "") {
                    tdutils::status::Result::Ok(network) => network,
                    tdutils::status::Result::Err(e) => {
                        return Err(Status::error(400, e.message()));
                    }
                };
                connect_items.push(td_api::TonConnectConnectItem::tonConnectConnectItemAddress(
                    td_api::tonConnectConnectItemAddress { network },
                ));
            } else if item_name == "ton_proof" {
                let payload = match item.get_required_string_field("payload") {
                    tdutils::status::Result::Ok(payload) => payload,
                    tdutils::status::Result::Err(e) => {
                        return Err(Status::error(400, e.message()));
                    }
                };
                connect_items.push(td_api::TonConnectConnectItem::tonConnectConnectItemProof(
                    td_api::tonConnectConnectItemProof { payload },
                ));
            } else {
                return Err(Status::error(400, "Invalid connect item"));
            }
        }
    }
    Ok(td_api::TonConnectConnectRequest {
        manifest_url: checked_manifest_url,
        items: connect_items,
    })
}

/// `get_ton_connect_query_from_startapp` (LinkManager.cpp:243-262,
/// extracted by e3f0a2489): the `tonconnect-` startapp payload decodes
/// `__`→`=`, `--`→`%`, `-`→`&` into the `?`-rooted connect query; a
/// non-tonconnect startapp answers the empty query.
fn get_ton_connect_query_from_startapp(startapp: &str) -> String {
    let Some(stripped) = startapp.strip_prefix("tonconnect-") else {
        return String::new();
    };
    let mut connect_query: Vec<u8> = b"?".to_vec();
    let bytes = stripped.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'_' && i + 1 < bytes.len() && bytes[i + 1] == b'_' {
            i += 1;
            connect_query.push(b'=');
        } else if bytes[i] == b'-' && i + 1 < bytes.len() && bytes[i + 1] == b'-' {
            i += 1;
            connect_query.push(b'%');
        } else if bytes[i] == b'-' {
            connect_query.push(b'&');
        } else {
            connect_query.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&connect_query).into_owned()
}

/// `get_internal_link_ton_connect` (LinkManager.cpp:3157-3172).
fn get_internal_link_ton_connect(query: &str, check_path: bool) -> Option<InternalLink> {
    let url_query = parse_url_query(query);
    if check_path && !url_query.path.is_empty() {
        return None;
    }
    let version = url_query.get_arg("v").unwrap_or("").parse::<i32>().ok()?;
    let r = url_query.get_arg("r").unwrap_or("").to_owned();
    if let Err(error) = get_ton_connect_connect_request_object(&r) {
        tdutils::log_info!("{error}");
        return None;
    }
    Some(InternalLink::TonConnect {
        version,
        dapp_client_id: url_query.get_arg("id").unwrap_or("").to_owned(),
        connect_request: r,
        return_strategy: url_query.get_arg("ret").unwrap_or("").to_owned(),
        rpc_request: url_query.get_arg("e").unwrap_or("").to_owned(),
        trace_id: url_query.get_arg("trace_id").unwrap_or("").to_owned(),
    })
}

/// `parse_ton_connect_link_query` (LinkManager.cpp:2743-2745).
fn parse_ton_connect_link_query(query: &str) -> Option<InternalLink> {
    get_internal_link_ton_connect(query, true)
}

fn get_internal_link_message_draft(url: &str, text: &str) -> Option<InternalLink> {
    if url.is_empty() && text.is_empty() {
        return None;
    }
    let text = text.strip_suffix('\n').map_or(text, |rest| {
        let mut rest = rest;
        while rest.ends_with('\n') {
            rest = &rest[..rest.len() - 1];
        }
        rest
    });
    let mut url = td_trim(url);
    let mut text = text;
    if url.is_empty() {
        url = text;
        text = "";
    }
    let mut full_text = crate::message_entity::FormattedText::default();
    let contains_url = !text.is_empty();
    if contains_url {
        full_text.text = format!("{url}\n{text}");
    } else {
        full_text.text = url.to_owned();
    }
    if crate::message_entity::fix_formatted_text(
        &mut full_text.text,
        &mut full_text.entities,
        false,
        false,
        false,
        false,
        true,
        true,
        None,
    )
    .is_err()
    {
        return None;
    }
    if full_text.text.starts_with('@') {
        full_text.text = format!(" {}", full_text.text);
        for entity in &mut full_text.entities {
            entity.offset += 1;
        }
    }
    Some(InternalLink::MessageDraft {
        text: full_text,
        contains_link: contains_url,
    })
}

/// `LinkManager::get_internal_link_web_proxy` (LinkManager.cpp:3260-3275)
/// — a port parameter is ignored, because web proxies always use 443; an
/// unparseable address or secret degrades to the empty unsupported-proxy
/// form like the C++.
fn get_internal_link_web_proxy(address: &str, secret: &str) -> InternalLink {
    use crate::connection_creator::Proxy;
    if let Ok((host, path)) = Proxy::parse_web_proxy_address(address)
        && let Ok(link_secret) = Proxy::get_web_proxy_link_secret(secret, !path.is_empty())
    {
        return InternalLink::Proxy {
            server: host,
            port: Proxy::WEB_PROXY_PORT,
            r#type: Some(td_api::ProxyType::proxyTypeWeb(td_api::proxyTypeWeb {
                path,
                secret: String::from_utf8_lossy(&link_secret.get_encoded_secret()).into_owned(),
            })),
        };
    }
    InternalLink::Proxy {
        server: String::new(),
        port: 0,
        r#type: None,
    }
}

/// `LinkManager::get_internal_link_passport` (cpp:2955-2984).
fn get_internal_link_passport(
    query: &str,
    args: &[(String, String)],
    allow_unknown: bool,
) -> Option<InternalLink> {
    let get_arg = |key: &str| -> &str {
        args.iter()
            .find(|(arg_key, _)| arg_key == key)
            .map(|(_, value)| value.as_str())
            .unwrap_or("")
    };

    let bot_user_id = UserId::new(to_integer_i64(get_arg("bot_id")));
    let scope = get_arg("scope");
    let public_key = get_arg("public_key");
    let mut nonce = get_arg("nonce");
    if nonce.is_empty() {
        nonce = get_arg("payload");
    }
    let callback_url = get_arg("callback_url");

    let invalid = !bot_user_id.is_valid()
        || scope.is_empty()
        || !tdutils::utf8::check_utf8(scope.as_bytes())
        || public_key.is_empty()
        || !tdutils::utf8::check_utf8(public_key.as_bytes())
        || nonce.is_empty()
        || !tdutils::utf8::check_utf8(nonce.as_bytes());
    if invalid {
        if !allow_unknown {
            return None;
        }
        return Some(InternalLink::UnknownDeepLink {
            link: format!("tg://{query}"),
        });
    }
    Some(InternalLink::PassportDataRequest {
        bot_user_id,
        scope: scope.to_owned(),
        public_key: public_key.to_owned(),
        nonce: nonce.to_owned(),
        callback_url: callback_url.to_owned(),
    })
}

/// `LinkManager::parse_tg_link_query` (cpp:2158-2575) — the tg:// link
/// decision table.
fn parse_tg_link_query(
    env: &mut impl LinkEnv,
    query: &str,
    is_trusted: bool,
) -> Option<InternalLink> {
    let url_query = parse_url_query(query);
    let path = &url_query.path;

    let mut is_first_arg = true;
    let mut copied = String::new();
    let pass_arg = |name: &str| url_encode(url_query.get_arg(name).unwrap_or(""));
    let get_arg = |name: &str| url_query.get_arg(name).unwrap_or("").to_owned();
    let has_arg = |name: &str| !url_query.get_arg(name).unwrap_or("").is_empty();

    if path.len() == 1 && path[0] == "resolve" {
        let username = get_arg("domain");
        if crate::misc::is_valid_username(&username) {
            if has_arg("post") {
                // resolve?domain=<username>&post=12345&single&thread=<thread_id>&comment=<message_id>&t=<media_timestamp>
                copied.push_str("tg://resolve");
                for name in [
                    "domain", "post", "single", "thread", "comment", "t", "task", "option",
                ] {
                    copy_arg(&mut copied, &url_query, &mut is_first_arg, name);
                }
                return Some(InternalLink::Message { url: copied });
            }
            if username == "oauth" && has_arg("startapp") {
                copied.push_str("tg://resolve");
                copy_arg(&mut copied, &url_query, &mut is_first_arg, "domain");
                copy_arg(&mut copied, &url_query, &mut is_first_arg, "startapp");
                return Some(InternalLink::Oauth { url: copied });
            }
            if username == "sendgrams" && has_arg("startapp") {
                // e3f0a2489 — tg://resolve?domain=sendgrams&startapp=
                // tonconnect-... is the TON Connect link too
                if let Some(link) = get_internal_link_ton_connect(
                    &get_ton_connect_query_from_startapp(&get_arg("startapp")),
                    true,
                ) {
                    return Some(link);
                }
            }
            for (key, value) in &url_query.args {
                if (key == "voicechat" || key == "videochat" || key == "livestream")
                    && is_base64url_characters(value.as_bytes())
                {
                    // resolve?domain=<username>&videochat[=<invite_hash>]
                    env.reload_video_chat_on_search(&username);
                    return Some(InternalLink::VideoChat {
                        dialog_username: username,
                        invite_hash: value.clone(),
                        is_live_stream: key == "livestream",
                    });
                }
                if key == "ref" && is_valid_start_parameter(value) && !value.is_empty() {
                    // resolve?domain=<bot_username>&ref=<referrer>
                    return Some(InternalLink::DialogReferralProgram {
                        username,
                        referral: value.clone(),
                    });
                }
                if key == "start" && is_valid_start_parameter(value) {
                    let prefixes = get_referral_program_start_parameter_prefixes(env);
                    for prefix in &prefixes {
                        if let Some(rest) = value.strip_prefix(prefix.as_str())
                            && value.len() > prefix.len()
                        {
                            // resolve?domain=<bot_username>&start=_tgr_<referrer>
                            return Some(InternalLink::DialogReferralProgram {
                                username,
                                referral: rest.to_owned(),
                            });
                        }
                    }
                    // resolve?domain=<bot_username>&start=<parameter>
                    return Some(InternalLink::BotStart {
                        bot_username: username,
                        start_parameter: value.clone(),
                        autostart: is_trusted,
                    });
                }
                if key == "startgroup" && is_valid_start_parameter(value) {
                    // resolve?domain=<bot_username>&startgroup[=<parameter>]&admin=...
                    let administrator_rights =
                        get_administrator_rights(url_query.get_arg("admin").unwrap_or(""), false);
                    return Some(InternalLink::BotStartInGroup {
                        bot_username: username,
                        start_parameter: value.clone(),
                        administrator_rights,
                    });
                }
                if key == "startchannel" {
                    // resolve?domain=<bot_username>&startchannel&admin=...
                    let administrator_rights =
                        get_administrator_rights(url_query.get_arg("admin").unwrap_or(""), true);
                    if administrator_rights != AdministratorRights::default() {
                        return Some(InternalLink::BotAddToChannel {
                            bot_username: username,
                            administrator_rights,
                        });
                    }
                }
                if key == "game" && is_valid_game_name(value) {
                    // resolve?domain=<bot_username>&game=<short_name>
                    return Some(InternalLink::Game {
                        bot_username: username,
                        game_short_name: value.clone(),
                    });
                }
                if key == "appname" && is_valid_web_app_name(value) {
                    // resolve?domain=<bot_username>&appname=<app_name>&startapp=<start_parameter>&mode=compact
                    return Some(InternalLink::WebApp {
                        bot_username: username,
                        web_app_short_name: value.clone(),
                        start_parameter: get_arg("startapp"),
                        mode: get_arg("mode"),
                    });
                }
                if key == "story" && is_valid_story_id(value) {
                    // resolve?domain=<username>&story=<story_id>
                    return Some(InternalLink::Story {
                        story_poster_username: username,
                        story_id: StoryId::new(to_integer_i32(value)),
                    });
                }
                if key == "story" && value == "live" {
                    // resolve?domain=<username>&story=live
                    return Some(InternalLink::LiveStory {
                        dialog_username: username,
                    });
                }
                if key == "startapp"
                    && is_valid_start_parameter(value)
                    && !url_query.has_arg("appname")
                {
                    // resolve?domain=<bot_username>&startapp[=<start_parameter>]&mode=compact
                    return Some(InternalLink::MainWebApp {
                        bot_username: username,
                        start_parameter: value.clone(),
                        mode: get_arg("mode"),
                    });
                }
                if key == "attach" && crate::misc::is_valid_username(value) {
                    // resolve?domain=<username>&attach=<bot_username>&startattach=<start_parameter>
                    return Some(InternalLink::new_attach_menu_bot(
                        None,
                        Some(InternalLink::PublicDialog {
                            dialog_username: username,
                            draft_text: String::new(),
                            open_profile: false,
                            unpaid_messages_token: String::new(),
                        }),
                        value.clone(),
                        url_query.get_arg("startattach").unwrap_or(""),
                    ));
                }
                if key == "startattach" && !has_arg("attach") {
                    // resolve?domain=<bot_username>&startattach[=<start_parameter>]&choose=...
                    return Some(InternalLink::new_attach_menu_bot(
                        get_target_chat_types(url_query.get_arg("choose").unwrap_or("")),
                        None,
                        username,
                        value,
                    ));
                }
                if key == "direct" {
                    // resolve?domain=<username>&direct
                    return Some(InternalLink::Monoforum {
                        channel_username: username,
                    });
                }
                if key == "collection" && is_valid_star_gift_collection_id(value) {
                    // resolve?domain=<username>&collection=<collection_id>
                    return Some(InternalLink::StarGiftCollection {
                        gift_owner_username: username,
                        collection_id: StarGiftCollectionId::new(to_integer_i32(value)),
                    });
                }
                if key == "album" && is_valid_story_album_id(value) {
                    // resolve?domain=<username>&album=<story_album_id>
                    return Some(InternalLink::StoryAlbum {
                        story_album_owner_username: username,
                        story_album_id: StoryAlbumId::new(to_integer_i32(value)),
                    });
                }
            }
            if username == "telegrampassport" {
                // resolve?domain=telegrampassport&bot_id=...&scope=...
                if let Some(passport_link) =
                    get_internal_link_passport(query, &url_query.args, false)
                {
                    return Some(passport_link);
                }
            }
            // resolve?domain=<username>
            // resolve?domain=<username>&free=<token>
            return Some(InternalLink::PublicDialog {
                dialog_username: username,
                draft_text: get_url_query_draft_text(&url_query),
                open_profile: url_query.has_arg("profile"),
                unpaid_messages_token: get_url_query_unpaid_messages_token(&url_query),
            });
        } else {
            let phone_number_str = get_arg("phone");
            let phone_number = phone_number_str
                .strip_prefix(' ')
                .unwrap_or(&phone_number_str);
            if is_valid_phone_number(phone_number) {
                let attach = get_arg("attach");
                if crate::misc::is_valid_username(&attach) {
                    // resolve?phone=<phone_number>&attach=<bot_username>
                    return Some(InternalLink::new_attach_menu_bot(
                        None,
                        Some(InternalLink::new_user_phone_number(
                            phone_number,
                            String::new(),
                            false,
                            String::new(),
                        )),
                        attach,
                        url_query.get_arg("startattach").unwrap_or(""),
                    ));
                }
                // resolve?phone=12345
                // resolve?phone=12345&free=<token>
                return Some(InternalLink::new_user_phone_number(
                    phone_number,
                    get_url_query_draft_text(&url_query),
                    url_query.has_arg("profile"),
                    get_url_query_unpaid_messages_token(&url_query),
                ));
            }
        }
        return None;
    }
    if path.len() == 1 && path[0] == "nft" {
        // nft?slug=<slug>
        let name = get_arg("slug");
        if is_valid_upgraded_gift_name(&name) {
            return Some(InternalLink::UpgradedGift { name });
        }
    } else if path.len() == 1 && path[0] == "contact" {
        // contact?token=<token>
        // contact?token=<token>&free=<token>
        let token = get_arg("token");
        if is_valid_user_token(&token) {
            return Some(InternalLink::UserToken {
                token,
                unpaid_messages_token: get_url_query_unpaid_messages_token(&url_query),
            });
        }
    } else if !path.is_empty() && path[0] == "contacts" {
        // contacts[/section]
        if path.len() == 2 && get_contacts_sections().contains(&path[1].as_str()) {
            return Some(InternalLink::Contacts {
                section: path[1].clone(),
            });
        }
        return Some(InternalLink::Contacts {
            section: String::new(),
        });
    } else if path.len() == 2 && path[0] == "chats" && path[1] == "edit" {
        // chats/edit
        return Some(InternalLink::DialogSelection);
    } else if path.len() == 2 && path[0] == "chats" && path[1] == "emoji-status" {
        // chats/emoji-status
        return Some(InternalLink::Settings {
            path: vec!["emoji-status".to_owned()],
        });
    } else if path.len() == 2 && path[0] == "chats" && path[1] == "search" {
        // chats/search
        return Some(InternalLink::Search);
    } else if path.len() == 1 && path[0] == "login" {
        // login?code=123456
        let code = get_arg("code");
        if is_valid_login_code(&code) {
            return Some(InternalLink::AuthenticationCode { code });
        }
        // login?token=<token>
        if has_arg("token") {
            return Some(InternalLink::QrCodeAuthentication);
        }
    } else if path.len() == 1 && path[0] == "new" {
        // new
        return Some(InternalLink::NewPrivateChat);
    } else if path.len() == 2 && path[0] == "new" && path[1] == "channel" {
        // new/channel
        return Some(InternalLink::NewChannelChat);
    } else if path.len() == 2 && path[0] == "new" && path[1] == "group" {
        // new/group
        return Some(InternalLink::NewGroupChat);
    } else if path.len() == 1 && path[0] == "oauth" && has_arg("token") {
        // oauth?token=...
        copied.push_str("tg://oauth");
        copy_arg(&mut copied, &url_query, &mut is_first_arg, "token");
        return Some(InternalLink::Oauth { url: copied });
    } else if path.len() <= 2 && path.first().map(String::as_str) == Some("post") {
        // post[/content-type]
        return Some(InternalLink::PostStory {
            content_type: if path.len() == 2 {
                path[1].clone()
            } else {
                String::new()
            },
        });
    } else if path.len() == 1 && path[0] == "restore_purchases" {
        // restore_purchases
        return Some(InternalLink::RestorePurchases);
    } else if path.len() == 1 && path[0] == "passport" {
        // passport?bot_id=...&scope=...&public_key=...
        return get_internal_link_passport(query, &url_query.args, true);
    } else if path.len() == 1 && path[0] == "premium_offer" {
        // premium_offer?ref=<referrer>
        let referrer = get_arg("ref");
        if is_valid_premium_referrer(&referrer) {
            return Some(InternalLink::PremiumFeatures { referrer });
        }
    } else if path.len() == 1 && path[0] == "premium_multigift" {
        // premium_multigift?ref=<referrer>
        let referrer = get_arg("ref");
        if is_valid_premium_referrer(&referrer) {
            return Some(InternalLink::PremiumGift { referrer });
        }
    } else if path.len() >= 2 && path[0] == "settings" && path[1] == "saved-messages" {
        // settings/saved-messages
        return Some(InternalLink::SavedMessages);
    } else if path.len() >= 2 && path[0] == "settings" && path[1] == "calls" {
        // settings/calls[/section]
        let mut section = String::new();
        if path.len() >= 3 {
            section = path[2].clone();
            for segment in &path[3..] {
                section.push('/');
                section.push_str(segment);
            }
        }
        if !get_calls_sections().contains(&section.as_str()) {
            section = String::new();
        }
        return Some(InternalLink::Calls { section });
    } else if path.len() >= 2 && path[0] == "settings" && path[1] == "my-profile" {
        // settings/my-profile[/section]
        let mut section = String::new();
        if path.len() >= 3 {
            section = path[2].clone();
            for segment in &path[3..] {
                section.push('/');
                section.push_str(segment);
            }
        }
        if !get_my_profile_sections().contains(&section.as_str()) {
            section = String::new();
        }
        return Some(InternalLink::MyProfile { section });
    } else if !path.is_empty() && path[0] == "settings" {
        // settings[/section[/subsection]]
        return Some(InternalLink::Settings {
            path: path[1..].to_vec(),
        });
    } else if !path.is_empty() && path[0] == "stars" {
        // stars
        return Some(InternalLink::Settings {
            path: vec!["stars".to_owned()],
        });
    } else if !path.is_empty() && (path[0] == "ton" || path[0] == "grams") {
        // grams
        return Some(InternalLink::Settings {
            path: vec!["grams".to_owned()],
        });
    } else if path.len() == 1 && path[0] == "addlist" {
        let slug = get_url_query_slug(true, &url_query, "addlist");
        if !slug.is_empty() && is_base64url_characters(slug.as_bytes()) {
            // addlist?slug=<slug>
            return Some(InternalLink::DialogFolderInvite {
                url: get_dialog_filter_invite_link(env, &slug, true),
            });
        }
    } else if path.len() == 1 && path[0] == "call" {
        let slug = get_url_query_slug(true, &url_query, "call");
        if !slug.is_empty() && is_base64url_characters(slug.as_bytes()) {
            // call?slug=<slug>
            return Some(InternalLink::GroupCall {
                url: get_group_call_invite_link(env, &slug, true),
            });
        }
    } else if path.len() == 1 && path[0] == "join" {
        let invite_hash = get_url_query_hash(true, &url_query);
        if !invite_hash.is_empty()
            && !is_valid_phone_number(&invite_hash)
            && is_base64url_characters(invite_hash.as_bytes())
        {
            // join?invite=<hash>
            return Some(InternalLink::DialogInvite {
                url: get_dialog_invite_link(env, &invite_hash, true),
            });
        }
    } else if path.len() == 1 && (path[0] == "addstickers" || path[0] == "addemoji") {
        // addstickers?set=<name> / addemoji?set=<name>
        let name = get_arg("set");
        if is_valid_sticker_set_name(&name) {
            return Some(InternalLink::StickerSet {
                sticker_set_name: name,
                expect_custom_emoji: path[0] == "addemoji",
            });
        }
    } else if path.len() == 1 && path[0] == "setlanguage" {
        // setlanguage?lang=<name>
        let language_pack_id = get_arg("lang");
        if is_valid_language_pack_id(&language_pack_id) {
            return Some(InternalLink::Language { language_pack_id });
        }
    } else if path.len() == 1 && path[0] == "addstyle" {
        // addstyle?slug=<name>
        let style_name = get_arg("slug");
        if is_valid_text_composition_style_name(&style_name) {
            return Some(InternalLink::TextCompositionStyle { style_name });
        }
    } else if path.len() == 1 && path[0] == "addtheme" {
        // addtheme?slug=<name>
        let theme_name = get_arg("slug");
        if is_valid_theme_name(&theme_name) {
            return Some(InternalLink::Theme { theme_name });
        }
    } else if path.len() == 1 && path[0] == "confirmphone" {
        let hash = get_arg("hash");
        let phone_number = get_arg("phone");
        if is_valid_phone_number_hash(&hash) && is_valid_phone_number(&phone_number) {
            // confirmphone?phone=<phone>&hash=<hash>
            return Some(InternalLink::ConfirmPhone { hash, phone_number });
        }
    } else if path.len() == 1 && path[0] == "sendgrams" {
        // sendgrams?to=<receiver>&amount=<amount>
        // sendgrams?v=... (23d3991c4)
        if let Some(link) = get_internal_link_ton_connect(query, false) {
            return Some(link);
        }
        let receiver = get_arg("to");
        let amount = get_arg("amount");
        if is_valid_gram_receiver(&receiver) && is_valid_gram_amount(&amount) {
            let gram_amount = get_gram_amount(&amount);
            if gram_amount == 0 || !receiver.is_empty() {
                return Some(InternalLink::SendGrams {
                    receiver,
                    gram_amount,
                });
            }
        }
    } else if path.len() == 1 && path[0] == "socks" {
        // socks?server=<server>&port=<port>&user=<user>&pass=<pass>
        let server = get_arg("server");
        let port = to_integer_i32(&get_arg("port"));
        let username = get_arg("user");
        let password = get_arg("pass");
        if is_valid_proxy_server(&server)
            && 0 < port
            && port < 65536
            && is_valid_proxy_username(&username)
            && is_valid_proxy_password(&password)
        {
            return Some(InternalLink::Proxy {
                server,
                port,
                r#type: Some(td_api::ProxyType::proxyTypeSocks5(
                    td_api::proxyTypeSocks5 { username, password },
                )),
            });
        } else {
            return Some(InternalLink::Proxy {
                server: String::new(),
                port: 0,
                r#type: None,
            });
        }
    } else if path.len() == 1 && path[0] == "proxy" {
        // proxy?server=<server>&port=<port>&secret=<secret>
        let server = get_arg("server");
        let port = to_integer_i32(&get_arg("port"));
        let secret =
            tdmtproto::proxy_secret::ProxySecret::from_link(get_arg("secret").as_bytes(), false);
        if is_valid_proxy_server(&server) && 0 < port && port < 65536 && secret.is_ok() {
            return Some(InternalLink::Proxy {
                server,
                port,
                r#type: Some(td_api::ProxyType::proxyTypeMtproto(
                    td_api::proxyTypeMtproto {
                        secret: String::from_utf8_lossy(&secret.unwrap().get_encoded_secret())
                            .into_owned(),
                    },
                )),
            });
        } else {
            return Some(InternalLink::Proxy {
                server: String::new(),
                port: 0,
                r#type: None,
            });
        }
    } else if path.len() == 1 && path[0] == "webproxy" {
        // webproxy?server=<host>[%2F<path>]&secret=<secret>
        return Some(get_internal_link_web_proxy(
            &get_arg("server"),
            &get_arg("secret"),
        ));
    } else if path.len() == 1 && path[0] == "privatepost" {
        // privatepost?channel=123456789&post=12345&single&thread=...
        if has_arg("channel") && has_arg("post") {
            copied.push_str("tg://privatepost");
            for name in [
                "channel", "post", "single", "thread", "comment", "t", "task", "option",
            ] {
                copy_arg(&mut copied, &url_query, &mut is_first_arg, name);
            }
            return Some(InternalLink::Message { url: copied });
        }
    } else if path.len() == 1 && path[0] == "boost" {
        // boost?domain=channel_username / boost?channel=123456
        if has_arg("domain") {
            copied.push_str("tg://boost");
            copy_arg(&mut copied, &url_query, &mut is_first_arg, "domain");
            return Some(InternalLink::DialogBoost { url: copied });
        }
        if has_arg("channel") {
            copied.push_str("tg://boost");
            copy_arg(&mut copied, &url_query, &mut is_first_arg, "channel");
            return Some(InternalLink::DialogBoost { url: copied });
        }
    } else if path.len() == 1 && path[0] == "bg" {
        // bg?color=<color> / bg?gradient=<hex>-<hex>&rotation=...
        // bg?slug=<background_name>&mode=blur+motion
        if has_arg("color") {
            return Some(InternalLink::Background {
                background_name: pass_arg("color"),
            });
        }
        if has_arg("gradient") {
            let mut background_name = pass_arg("gradient");
            copy_arg(
                &mut background_name,
                &url_query,
                &mut is_first_arg,
                "rotation",
            );
            return Some(InternalLink::Background { background_name });
        }
        if has_arg("slug") {
            let mut background_name = pass_arg("slug");
            for name in ["mode", "intensity", "bg_color", "rotation"] {
                copy_arg(&mut background_name, &url_query, &mut is_first_arg, name);
            }
            return Some(InternalLink::Background { background_name });
        }
    } else if path.len() == 1 && path[0] == "invoice" {
        // invoice?slug=<invoice_name>
        let invoice_name = get_arg("slug");
        if is_valid_invoice_name(&invoice_name) {
            return Some(InternalLink::Invoice { invoice_name });
        }
    } else if path.len() == 1 && path[0] == "giftcode" {
        // giftcode?slug=<code>
        let code = get_arg("slug");
        if is_valid_gift_code(&code) {
            return Some(InternalLink::PremiumGiftCode { code });
        }
    } else if path.len() == 1 && path[0] == "message" {
        // message?slug=<name>
        let link_name = get_arg("slug");
        if is_valid_business_link_name(&link_name) {
            return Some(InternalLink::BusinessChat { link_name });
        }
    } else if path.len() == 1 && (path[0] == "share" || path[0] == "msg" || path[0] == "msg_url") {
        // msg_url?url=<url>&text=<text>
        return get_internal_link_message_draft(&get_arg("url"), &get_arg("text"));
    } else if path.len() == 1 && path[0] == "stars_topup" {
        // stars_topup?balance=<star_count>&purpose=<purpose>
        let purpose = get_arg("purpose");
        if has_arg("balance") && is_valid_star_top_up_purpose(&purpose) {
            return Some(InternalLink::new_buy_stars(
                to_integer_i64(url_query.get_arg("balance").unwrap_or("")),
                purpose,
            ));
        }
    } else if path.len() == 1 && path[0] == "stargift_auction" {
        let slug = get_url_query_slug(true, &url_query, "stargift_auction");
        if !slug.is_empty() {
            // stargift_auction?slug=<slug>
            return Some(InternalLink::GiftAuction { slug });
        }
    } else if path.len() == 1 && path[0] == "newbot" {
        // newbot?manager=<manager_bot_username>&username=<new_bot_username>&name=<new_bot_name>
        let manager_bot_username = get_arg("manager");
        let bot_username = get_arg("username");
        if crate::misc::is_valid_username(&manager_bot_username)
            && (bot_username.is_empty() || crate::misc::is_valid_username(&bot_username))
        {
            return Some(InternalLink::RequestManagedBot {
                manager_bot_username,
                bot_username,
                bot_name: get_arg("name"),
            });
        }
    }
    if !path.is_empty() && !path[0].is_empty() {
        return Some(InternalLink::UnknownDeepLink {
            link: format!("tg://{query}"),
        });
    }
    None
}

/// `LinkManager::get_t_me_url` (cpp:4427-4433): the option read (the
/// scheduler-context branch) folds into the env; its default is the
/// context-free `"https://t.me/"`.
pub fn get_t_me_url(env: &impl LinkEnv) -> String {
    env.get_option_string_with_default("t_me_url", "https://t.me/")
}

/// `LinkManager::get_dialog_filter_invite_link` (cpp:4248-4257).
fn get_dialog_filter_invite_link(env: &impl LinkEnv, slug: &str, is_internal: bool) -> String {
    if !is_base64url_characters(slug.as_bytes()) {
        return String::new();
    }
    if is_internal {
        format!("tg:addlist?slug={slug}")
    } else {
        format!("{}addlist/{slug}", get_t_me_url(env))
    }
}

/// `LinkManager::get_dialog_invite_link` (cpp:4275-4284).
pub(crate) fn get_dialog_invite_link(
    env: &impl LinkEnv,
    invite_hash: &str,
    is_internal: bool,
) -> String {
    if !is_base64url_characters(invite_hash.as_bytes()) {
        return String::new();
    }
    if is_internal {
        format!("tg:join?invite={invite_hash}")
    } else {
        format!("{}+{invite_hash}", get_t_me_url(env))
    }
}

/// `LinkManager::get_group_call_invite_link` (cpp:4299-4308).
fn get_group_call_invite_link(env: &impl LinkEnv, slug: &str, is_internal: bool) -> String {
    if !is_base64url_characters(slug.as_bytes()) {
        return String::new();
    }
    if is_internal {
        format!("tg:call?slug={slug}")
    } else {
        format!("{}call/{slug}", get_t_me_url(env))
    }
}

/// The `BackgroundFill` link-plane subset (BackgroundType.h:19-37,
/// BackgroundType.cpp:104-245): the string parsing/encoding half the link
/// machinery needs; the wire (wallPaperSettings) half stays with the
/// BackgroundType track.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct BackgroundFill {
    top_color: i32,
    bottom_color: i32,
    rotation_angle: i32,
    third_color: i32,
    fourth_color: i32,
}

impl BackgroundFill {
    fn get_type(&self) -> BackgroundFillType {
        if self.third_color != -1 {
            BackgroundFillType::FreeformGradient
        } else if self.top_color == self.bottom_color {
            BackgroundFillType::Solid
        } else {
            BackgroundFillType::Gradient
        }
    }

    fn new_solid(solid_color: i32) -> BackgroundFill {
        BackgroundFill {
            top_color: solid_color,
            bottom_color: solid_color,
            ..BackgroundFill::default()
        }
    }

    /// The private `(top, bottom, rotation)` constructor: the rotation is
    /// dropped unless the fill is a real gradient (BackgroundType.h:44-49).
    fn new_gradient(top_color: i32, bottom_color: i32, rotation_angle: i32) -> BackgroundFill {
        let fill = BackgroundFill {
            top_color,
            bottom_color,
            rotation_angle,
            third_color: -1,
            fourth_color: -1,
        };
        if fill.get_type() != BackgroundFillType::Gradient {
            return BackgroundFill {
                rotation_angle: 0,
                ..fill
            };
        }
        fill
    }

    fn new_freeform(
        first_color: i32,
        second_color: i32,
        third_color: i32,
        fourth_color: i32,
    ) -> BackgroundFill {
        BackgroundFill {
            top_color: first_color,
            bottom_color: second_color,
            rotation_angle: 0,
            third_color,
            fourth_color,
        }
    }

    /// `BackgroundFill::get_background_fill(Slice name)`
    /// (BackgroundType.cpp:140-197).
    fn get_background_fill(name: &str) -> Result<BackgroundFill, Status> {
        // name.substr(0, name.find('#'))
        let name = match name.find('#') {
            Some(pos) => &name[..pos],
            None => name,
        };

        let mut parameters = "";
        let name = match name.find('?') {
            Some(pos) => {
                parameters = &name[pos + 1..];
                &name[..pos]
            }
            None => name,
        };

        let get_color = |color_string: &str| -> Result<i32, Status> {
            let decoded = url_decode(color_string, true);
            let value = hex_to_integer_safe_u32(&decoded)
                .map_err(|_| Status::error(400, "WALLPAPER_INVALID"))?;
            if color_string.len() > 6 {
                return Err(Status::error(400, "WALLPAPER_INVALID"));
            }
            Ok(value as i32)
        };

        let mut hyphen_pos = name.find('-');
        if name.contains('~') {
            let color_strings: Vec<&str> = name.split('~').collect();
            if color_strings.len() == 2 {
                hyphen_pos = Some(color_strings[0].len());
            } else {
                if color_strings.len() > 4 {
                    return Err(Status::error(400, "WALLPAPER_INVALID"));
                }
                let first_color = get_color(color_strings[0])?;
                let second_color = get_color(color_strings[1])?;
                let third_color = get_color(color_strings[2])?;
                let mut fourth_color = -1;
                if color_strings.len() == 4 {
                    fourth_color = get_color(color_strings[3])?;
                }
                return Ok(BackgroundFill::new_freeform(
                    first_color,
                    second_color,
                    third_color,
                    fourth_color,
                ));
            }
        }

        if let Some(hyphen_pos) = hyphen_pos {
            let top_color = get_color(&name[..hyphen_pos])?;
            let bottom_color = get_color(&name[hyphen_pos + 1..])?;
            let mut rotation_angle = 0;
            if let Some(rest) = parameters.strip_prefix("rotation=") {
                rotation_angle = to_integer_i32(&url_decode(rest, true));
                if !is_valid_rotation_angle(rotation_angle) {
                    rotation_angle = 0;
                }
            }
            return Ok(BackgroundFill::new_gradient(
                top_color,
                bottom_color,
                rotation_angle,
            ));
        }

        Ok(BackgroundFill::new_solid(get_color(name)?))
    }

    /// `BackgroundFill::get_link` (BackgroundType.cpp:199-219).
    fn get_link(&self, is_first: bool) -> String {
        match self.get_type() {
            BackgroundFillType::Solid => get_color_hex_string(self.top_color),
            BackgroundFillType::Gradient => format!(
                "{}-{}{}rotation={}",
                get_color_hex_string(self.top_color),
                get_color_hex_string(self.bottom_color),
                if is_first { '?' } else { '&' },
                self.rotation_angle
            ),
            BackgroundFillType::FreeformGradient => {
                let mut result = format!(
                    "{}~{}~{}",
                    get_color_hex_string(self.top_color),
                    get_color_hex_string(self.bottom_color),
                    get_color_hex_string(self.third_color)
                );
                if self.fourth_color != -1 {
                    result.push('~');
                    result.push_str(&get_color_hex_string(self.fourth_color));
                }
                result
            }
        }
    }

    /// `BackgroundFill::get_background_fill_object`
    /// (BackgroundType.cpp:434-451).
    fn get_background_fill_object(&self) -> td_api::BackgroundFill {
        match self.get_type() {
            BackgroundFillType::Solid => {
                td_api::BackgroundFill::backgroundFillSolid(td_api::backgroundFillSolid {
                    color: self.top_color,
                })
            }
            BackgroundFillType::Gradient => {
                td_api::BackgroundFill::backgroundFillGradient(td_api::backgroundFillGradient {
                    top_color: self.top_color,
                    bottom_color: self.bottom_color,
                    rotation_angle: self.rotation_angle,
                })
            }
            BackgroundFillType::FreeformGradient => {
                let mut colors = vec![
                    self.top_color,
                    self.bottom_color,
                    self.third_color,
                    self.fourth_color,
                ];
                if *colors.last().unwrap() == -1 {
                    colors.pop();
                }
                td_api::BackgroundFill::backgroundFillFreeformGradient(
                    td_api::backgroundFillFreeformGradient { colors },
                )
            }
        }
    }
    /// `BackgroundFill::get_background_fill(const td_api::BackgroundFill *)`
    /// (BackgroundType.cpp:106-139).
    fn get_background_fill_from_api(
        fill: Option<&td_api::BackgroundFill>,
    ) -> Result<BackgroundFill, Status> {
        use td_api::BackgroundFill as F;
        let Some(fill) = fill else {
            return Err(Status::error(400, "Background fill info must be non-empty"));
        };
        match fill {
            F::backgroundFillSolid(solid) => {
                if !is_valid_color(solid.color) {
                    return Err(Status::error(400, "Invalid solid fill color value"));
                }
                Ok(BackgroundFill::new_solid(solid.color))
            }
            F::backgroundFillGradient(gradient) => {
                if !is_valid_color(gradient.top_color) {
                    return Err(Status::error(400, "Invalid top gradient color value"));
                }
                if !is_valid_color(gradient.bottom_color) {
                    return Err(Status::error(400, "Invalid bottom gradient color value"));
                }
                if !is_valid_rotation_angle(gradient.rotation_angle) {
                    return Err(Status::error(400, "Invalid rotation angle value"));
                }
                Ok(BackgroundFill::new_gradient(
                    gradient.top_color,
                    gradient.bottom_color,
                    gradient.rotation_angle,
                ))
            }
            F::backgroundFillFreeformGradient(freeform) => {
                if freeform.colors.len() != 3 && freeform.colors.len() != 4 {
                    return Err(Status::error(400, "Wrong number of gradient colors"));
                }
                if freeform.colors.iter().any(|color| !is_valid_color(*color)) {
                    return Err(Status::error(400, "Invalid freeform gradient color value"));
                }
                Ok(BackgroundFill::new_freeform(
                    freeform.colors[0],
                    freeform.colors[1],
                    freeform.colors[2],
                    if freeform.colors.len() == 3 {
                        -1
                    } else {
                        freeform.colors[3]
                    },
                ))
            }
        }
    }
}

/// `BackgroundFill::Type` (BackgroundType.h:61-66).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BackgroundFillType {
    Solid,
    Gradient,
    FreeformGradient,
}

/// `get_color_hex_string` (BackgroundType.cpp:16-23).
fn get_color_hex_string(color: i32) -> String {
    let mut result = String::new();
    let mut i = 20;
    loop {
        result.push(
            [
                '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'a', 'b', 'c', 'd', 'e', 'f',
            ][((color >> i) & 0xF) as usize],
        );
        if i == 0 {
            break;
        }
        i -= 4;
    }
    result
}

/// `hex_to_integer_safe<uint32>` (misc.h:231-247).
fn hex_to_integer_safe_u32(s: &str) -> Result<u32, Status> {
    let mut value: u32 = 0;
    let mut bytes = s.as_bytes();
    if bytes.is_empty() {
        return Err(Status::error_message("String is empty"));
    }
    while let Some((c, rest)) = bytes.split_first() {
        let digit = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            _ => return Err(Status::error_message("String contains non-hex digit")),
        };
        if value > u32::MAX / 16 {
            return Err(Status::error_message("String hex number overflows"));
        }
        value = value * 16 + u32::from(digit);
        bytes = rest;
    }
    Ok(value)
}

/// `is_valid_rotation_angle` (BackgroundType.cpp:30-32).
fn is_valid_rotation_angle(rotation_angle: i32) -> bool {
    (0..360).contains(&rotation_angle) && rotation_angle % 45 == 0
}

/// `is_valid_intensity` (BackgroundType.cpp:34-36).
fn is_valid_intensity(intensity: i32, allow_negative: bool) -> bool {
    (if allow_negative { -100 } else { 0 }) <= intensity && intensity <= 100
}

/// `td::is_valid_color` (misc.cpp:430-432).
fn is_valid_color(color: i32) -> bool {
    (0..=0xFFFFFF).contains(&color)
}

/// The `BackgroundType` link-plane subset (BackgroundType.h:75-158,
/// BackgroundType.cpp:252-399, 466-481).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct BackgroundType {
    r#type: BackgroundTypeName,
    is_blurred: bool,
    is_moving: bool,
    intensity: i32,
    fill: BackgroundFill,
    theme_name: String,
}

/// `BackgroundType::Type` (BackgroundType.h:76).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum BackgroundTypeName {
    Wallpaper,
    Pattern,
    #[default]
    Fill,
    ChatTheme,
}

impl BackgroundType {
    fn new_wallpaper(is_blurred: bool, is_moving: bool, dark_theme_dimming: i32) -> BackgroundType {
        BackgroundType {
            r#type: BackgroundTypeName::Wallpaper,
            is_blurred,
            is_moving,
            intensity: dark_theme_dimming,
            ..BackgroundType::default()
        }
    }

    fn new_pattern(is_moving: bool, fill: BackgroundFill, intensity: i32) -> BackgroundType {
        BackgroundType {
            r#type: BackgroundTypeName::Pattern,
            is_moving,
            intensity,
            fill,
            ..BackgroundType::default()
        }
    }

    fn new_fill(fill: BackgroundFill, dark_theme_dimming: i32) -> BackgroundType {
        BackgroundType {
            r#type: BackgroundTypeName::Fill,
            intensity: dark_theme_dimming,
            fill,
            ..BackgroundType::default()
        }
    }

    fn new_chat_theme(theme_name: String) -> BackgroundType {
        BackgroundType {
            r#type: BackgroundTypeName::ChatTheme,
            theme_name,
            ..BackgroundType::default()
        }
    }

    /// `has_file` (BackgroundType.h:113-116).
    fn has_file(&self) -> bool {
        self.r#type == BackgroundTypeName::Wallpaper || self.r#type == BackgroundTypeName::Pattern
    }

    /// `has_gradient_fill` (BackgroundType.h:117-120).
    fn has_gradient_fill(&self) -> bool {
        self.r#type == BackgroundTypeName::Fill && self.fill.get_type() != BackgroundFillType::Solid
    }

    /// `BackgroundType::apply_parameters_from_link`
    /// (BackgroundType.cpp:252-286).
    fn apply_parameters_from_link(&mut self, name: &str) {
        let query = parse_url_query(name);

        self.is_blurred = false;
        self.is_moving = false;
        for mode in query.get_arg("mode").unwrap_or("").split(' ') {
            if self.r#type != BackgroundTypeName::Pattern && mode.to_lowercase() == "blur" {
                self.is_blurred = true;
            }
            if mode.to_lowercase() == "motion" {
                self.is_moving = true;
            }
        }

        if self.r#type == BackgroundTypeName::Pattern {
            self.intensity = -101;
            let intensity_arg = query.get_arg("intensity").unwrap_or("");
            if !intensity_arg.is_empty() {
                self.intensity = to_integer_i32(intensity_arg);
            }
            if !is_valid_intensity(self.intensity, true) {
                self.intensity = 50;
            }

            let bg_color = query.get_arg("bg_color").unwrap_or("");
            if !bg_color.is_empty() {
                let r_fill = BackgroundFill::get_background_fill(&format!(
                    "{}?rotation={}",
                    url_encode(bg_color),
                    url_encode(query.get_arg("rotation").unwrap_or(""))
                ));
                if let Ok(fill) = r_fill {
                    self.fill = fill;
                }
            }
        }
    }

    /// `BackgroundType::get_link` (BackgroundType.cpp:288-323).
    fn get_link(&self, is_first: bool) -> String {
        let mut mode = String::new();
        if self.is_blurred {
            mode = "blur".to_owned();
        }
        if self.is_moving {
            if !mode.is_empty() {
                mode.push('+');
            }
            mode.push_str("motion");
        }

        match self.r#type {
            BackgroundTypeName::Wallpaper => {
                if !mode.is_empty() {
                    return format!("mode={mode}");
                }
                String::new()
            }
            BackgroundTypeName::Pattern => {
                let mut link = format!(
                    "intensity={}&bg_color={}",
                    self.intensity,
                    self.fill.get_link(false)
                );
                if !mode.is_empty() {
                    link += "&mode=";
                    link += &mode;
                }
                link
            }
            BackgroundTypeName::Fill => self.fill.get_link(is_first),
            BackgroundTypeName::ChatTheme => String::new(),
        }
    }

    /// `BackgroundType::get_background_type` (BackgroundType.cpp:354-390).
    fn get_background_type(
        background_type: Option<&td_api::BackgroundType>,
        dark_theme_dimming: i32,
    ) -> Result<BackgroundType, Status> {
        use td_api::BackgroundType as T;
        let Some(background_type) = background_type else {
            return Ok(BackgroundType::default());
        };
        if !(0..=100).contains(&dark_theme_dimming) {
            return Err(Status::error(400, "Invalid dark them brightness specified"));
        }

        match background_type {
            T::backgroundTypeWallpaper(wallpaper_type) => Ok(BackgroundType::new_wallpaper(
                wallpaper_type.is_blurred,
                wallpaper_type.is_moving,
                dark_theme_dimming,
            )),
            T::backgroundTypePattern(pattern_type) => {
                let background_fill =
                    BackgroundFill::get_background_fill_from_api(pattern_type.fill.as_ref())?;
                if !is_valid_intensity(pattern_type.intensity, false) {
                    return Err(Status::error(400, "Wrong intensity value"));
                }
                let intensity = if pattern_type.is_inverted {
                    -std::cmp::max(pattern_type.intensity, 1)
                } else {
                    pattern_type.intensity
                };
                Ok(BackgroundType::new_pattern(
                    pattern_type.is_moving,
                    background_fill,
                    intensity,
                ))
            }
            T::backgroundTypeFill(fill_type) => {
                let background_fill =
                    BackgroundFill::get_background_fill_from_api(fill_type.fill.as_ref())?;
                Ok(BackgroundType::new_fill(
                    background_fill,
                    dark_theme_dimming,
                ))
            }
            T::backgroundTypeChatTheme(chat_theme_type) => Ok(BackgroundType::new_chat_theme(
                chat_theme_type.theme_name.clone(),
            )),
        }
    }

    /// `BackgroundType::get_local_background_type` (BackgroundType.cpp:392-395).
    fn get_local_background_type(name: &str) -> Result<BackgroundType, Status> {
        let fill = BackgroundFill::get_background_fill(name)?;
        Ok(BackgroundType::new_fill(fill, 0))
    }

    /// `BackgroundType::is_background_name_local` (BackgroundType.cpp:397-399).
    fn is_background_name_local(name: &str) -> bool {
        name.len() <= 13
            || name.find('?').is_some_and(|pos| pos <= 13)
            || !is_base64url_characters(
                match name.find('?') {
                    Some(pos) => &name[..pos],
                    None => name,
                }
                .as_bytes(),
            )
    }

    /// `BackgroundType::get_background_type_object`
    /// (BackgroundType.cpp:466-481).
    fn get_background_type_object(&self) -> td_api::BackgroundType {
        use td_api::BackgroundType as T;
        match self.r#type {
            BackgroundTypeName::Wallpaper => {
                T::backgroundTypeWallpaper(td_api::backgroundTypeWallpaper {
                    is_blurred: self.is_blurred,
                    is_moving: self.is_moving,
                })
            }
            BackgroundTypeName::Pattern => {
                T::backgroundTypePattern(td_api::backgroundTypePattern {
                    fill: Some(self.fill.get_background_fill_object()),
                    intensity: if self.intensity < 0 {
                        -self.intensity
                    } else {
                        self.intensity
                    },
                    is_inverted: self.intensity < 0,
                    is_moving: self.is_moving,
                })
            }
            BackgroundTypeName::Fill => T::backgroundTypeFill(td_api::backgroundTypeFill {
                fill: Some(self.fill.get_background_fill_object()),
            }),
            BackgroundTypeName::ChatTheme => {
                T::backgroundTypeChatTheme(td_api::backgroundTypeChatTheme {
                    theme_name: self.theme_name.clone(),
                })
            }
        }
    }
}

/// `LinkManager::parse_t_me_link_query` (cpp:2577-2921) — the t.me link
/// decision table.
fn parse_t_me_link_query(
    env: &mut impl LinkEnv,
    query: &str,
    is_trusted: bool,
) -> Option<InternalLink> {
    assert!(
        query.starts_with('/'),
        "t.me link query must start with '/'"
    );
    let url_query = parse_url_query(query);
    let path = &url_query.path;
    if path.is_empty() || path[0].is_empty() {
        return None;
    }

    let mut is_first_arg = true;
    let mut copied = String::new();
    let get_arg = |name: &str| url_query.get_arg(name).unwrap_or("").to_owned();
    let has_arg = |name: &str| !url_query.get_arg(name).unwrap_or("").is_empty();

    if path[0] == "c" {
        if path.len() >= 3 && to_integer_i64(&path[1]) > 0 && to_integer_i64(&path[2]) > 0 {
            // /c/123456789/12345?single&thread=<thread_id>&comment=<message_id>
            // /c/123456789/1234/12345?single&comment=<message_id>
            is_first_arg = false;
            let mut post = to_integer_i64(&path[2]);
            let mut thread = copy_arg_to_string(&url_query, &mut is_first_arg, "thread");
            if path.len() >= 4 && to_integer_i64(&path[3]) > 0 {
                thread = format!("&thread={post}");
                post = to_integer_i64(&path[3]);
            }
            copied.push_str(&format!(
                "tg://privatepost?channel={}&post={post}",
                to_integer_i64(&path[1])
            ));
            copy_arg(&mut copied, &url_query, &mut is_first_arg, "single");
            copied.push_str(&thread);
            copy_arg(&mut copied, &url_query, &mut is_first_arg, "comment");
            copy_arg(&mut copied, &url_query, &mut is_first_arg, "t");
            copy_arg(&mut copied, &url_query, &mut is_first_arg, "task");
            copy_arg(&mut copied, &url_query, &mut is_first_arg, "option");
            return Some(InternalLink::Message { url: copied });
        } else if path.len() >= 2 && to_integer_i64(&path[1]) > 0 && url_query.has_arg("boost") {
            // /c/123456789?boost
            return Some(InternalLink::DialogBoost {
                url: format!("tg://boost?channel={}", to_integer_i64(&path[1])),
            });
        }
    } else if path[0] == "login" {
        if path.len() >= 2 && is_valid_login_code(&path[1]) {
            // /login/<code>
            return Some(InternalLink::AuthenticationCode {
                code: path[1].clone(),
            });
        }
    } else if path[0] == "addlist" {
        let slug = get_url_query_slug(false, &url_query, "addlist");
        if !slug.is_empty() && is_base64url_characters(slug.as_bytes()) {
            // /addlist/<slug>
            return Some(InternalLink::DialogFolderInvite {
                url: get_dialog_filter_invite_link(env, &slug, true),
            });
        }
    } else if path[0] == "call" {
        let slug = get_url_query_slug(false, &url_query, "call");
        if !slug.is_empty() && is_base64url_characters(slug.as_bytes()) {
            // /call/<slug>
            return Some(InternalLink::GroupCall {
                url: get_group_call_invite_link(env, &slug, true),
            });
        }
    } else if path[0] == "joinchat" {
        let invite_hash = get_url_query_hash(false, &url_query);
        if !invite_hash.is_empty()
            && !is_valid_phone_number(&invite_hash)
            && is_base64url_characters(invite_hash.as_bytes())
        {
            // /joinchat/<hash>
            return Some(InternalLink::DialogInvite {
                url: get_dialog_invite_link(env, &invite_hash, true),
            });
        }
    } else if path[0] == "auction" {
        let slug = get_url_query_slug(false, &url_query, "auction");
        if !slug.is_empty() {
            // /auction/<slug>
            return Some(InternalLink::GiftAuction { slug });
        }
    } else if path[0].starts_with(' ') || path[0].starts_with('+') {
        let invite_hash = get_url_query_hash(false, &url_query);
        if is_valid_phone_number(&invite_hash) {
            let attach = get_arg("attach");
            if crate::misc::is_valid_username(&attach) {
                // /+<phone_number>?attach=<bot_username>
                // /+<phone_number>?attach=<bot_username>&startattach=<start_parameter>
                return Some(InternalLink::new_attach_menu_bot(
                    None,
                    Some(InternalLink::new_user_phone_number(
                        &invite_hash,
                        String::new(),
                        false,
                        String::new(),
                    )),
                    attach,
                    url_query.get_arg("startattach").unwrap_or(""),
                ));
            }
            // /+<phone_number>
            // /+<phone_number>?free=<token>
            return Some(InternalLink::new_user_phone_number(
                &invite_hash,
                get_url_query_draft_text(&url_query),
                url_query.has_arg("profile"),
                get_url_query_unpaid_messages_token(&url_query),
            ));
        } else if !invite_hash.is_empty() && is_base64url_characters(invite_hash.as_bytes()) {
            // /+<link>
            return Some(InternalLink::DialogInvite {
                url: get_dialog_invite_link(env, &invite_hash, true),
            });
        }
    } else if path[0] == "nft" {
        if path.len() >= 2 && !path[1].is_empty() {
            // /nft/<slug>
            let mut name = path[1].clone();
            for segment in &path[2..] {
                name.push('/');
                name.push_str(segment);
            }
            if is_valid_upgraded_gift_name(&name) {
                return Some(InternalLink::UpgradedGift { name });
            }
        }
    } else if path[0] == "contact" {
        if path.len() >= 2 && is_valid_user_token(&path[1]) {
            // /contact/<token>
            // /contact/<token>?free=<token>
            return Some(InternalLink::UserToken {
                token: path[1].clone(),
                unpaid_messages_token: get_url_query_unpaid_messages_token(&url_query),
            });
        }
    } else if path[0] == "addstickers" || path[0] == "addemoji" {
        if path.len() >= 2 && is_valid_sticker_set_name(&path[1]) {
            // /addstickers/<name> / /addemoji/<name>
            return Some(InternalLink::StickerSet {
                sticker_set_name: path[1].clone(),
                expect_custom_emoji: path[0] == "addemoji",
            });
        }
    } else if path[0] == "setlanguage" {
        if path.len() >= 2 && is_valid_language_pack_id(&path[1]) {
            // /setlanguage/<name>
            return Some(InternalLink::Language {
                language_pack_id: path[1].clone(),
            });
        }
    } else if path[0] == "addstyle" {
        if path.len() >= 2 && is_valid_text_composition_style_name(&path[1]) {
            // /addstyle/<name>
            return Some(InternalLink::TextCompositionStyle {
                style_name: path[1].clone(),
            });
        }
    } else if path[0] == "addtheme" {
        if path.len() >= 2 && is_valid_theme_name(&path[1]) {
            // /addtheme/<name>
            return Some(InternalLink::Theme {
                theme_name: path[1].clone(),
            });
        }
    } else if path[0] == "confirmphone" {
        let hash = get_arg("hash");
        let phone_number = get_arg("phone");
        if is_valid_phone_number_hash(&hash) && is_valid_phone_number(&phone_number) {
            // /confirmphone?phone=<phone>&hash=<hash>
            return Some(InternalLink::ConfirmPhone { hash, phone_number });
        }
    } else if path[0] == "sendgrams" {
        // /sendgrams?to=<receiver>&amount=<amount>
        // /sendgrams?startapp=tonconnect-... (23d3991c4; the decode moved
        // into get_ton_connect_query_from_startapp with e3f0a2489)
        if let Some(link) = get_internal_link_ton_connect(
            &get_ton_connect_query_from_startapp(&get_arg("startapp")),
            true,
        ) {
            return Some(link);
        }
        let receiver = get_arg("to");
        let amount = get_arg("amount");
        if is_valid_gram_receiver(&receiver) && is_valid_gram_amount(&amount) {
            let gram_amount = get_gram_amount(&amount);
            if gram_amount == 0 || !receiver.is_empty() {
                return Some(InternalLink::SendGrams {
                    receiver,
                    gram_amount,
                });
            }
        }
    } else if path[0] == "socks" {
        // /socks?server=<server>&port=<port>&user=<user>&pass=<pass>
        let server = get_arg("server");
        let port = to_integer_i32(&get_arg("port"));
        let username = get_arg("user");
        let password = get_arg("pass");
        if is_valid_proxy_server(&server)
            && 0 < port
            && port < 65536
            && is_valid_proxy_username(&username)
            && is_valid_proxy_password(&password)
        {
            return Some(InternalLink::Proxy {
                server,
                port,
                r#type: Some(td_api::ProxyType::proxyTypeSocks5(
                    td_api::proxyTypeSocks5 { username, password },
                )),
            });
        } else {
            return Some(InternalLink::Proxy {
                server: String::new(),
                port: 0,
                r#type: None,
            });
        }
    } else if path[0] == "proxy" {
        // /proxy?server=<server>&port=<port>&secret=<secret>
        let server = get_arg("server");
        let port = to_integer_i32(&get_arg("port"));
        let secret =
            tdmtproto::proxy_secret::ProxySecret::from_link(get_arg("secret").as_bytes(), false);
        if is_valid_proxy_server(&server) && 0 < port && port < 65536 && secret.is_ok() {
            return Some(InternalLink::Proxy {
                server,
                port,
                r#type: Some(td_api::ProxyType::proxyTypeMtproto(
                    td_api::proxyTypeMtproto {
                        secret: String::from_utf8_lossy(&secret.unwrap().get_encoded_secret())
                            .into_owned(),
                    },
                )),
            });
        } else {
            return Some(InternalLink::Proxy {
                server: String::new(),
                port: 0,
                r#type: None,
            });
        }
    } else if path[0] == "webproxy" {
        // /webproxy?server=<host>[%2F<path>]&secret=<secret>
        return Some(get_internal_link_web_proxy(
            &get_arg("server"),
            &get_arg("secret"),
        ));
    } else if path[0] == "bg" {
        if path.len() >= 2 && !path[1].is_empty() {
            // /bg/<hex_color> / /bg/<hex_color>-<hex_color>?rotation=...
            // /bg/<background_name>?mode=blur+motion
            if BackgroundType::is_background_name_local(&path[1]) {
                let mut background_name = url_encode(&path[1]);
                copy_arg(
                    &mut background_name,
                    &url_query,
                    &mut is_first_arg,
                    "rotation",
                );
                return Some(InternalLink::Background { background_name });
            }
            let mut background_name = url_encode(&path[1]);
            for name in ["mode", "intensity", "bg_color", "rotation"] {
                copy_arg(&mut background_name, &url_query, &mut is_first_arg, name);
            }
            return Some(InternalLink::Background { background_name });
        }
    } else if path[0] == "invoice" {
        if path.len() >= 2 && is_valid_invoice_name(&path[1]) {
            // /invoice/<name>
            return Some(InternalLink::Invoice {
                invoice_name: path[1].clone(),
            });
        }
    } else if path[0].eq_ignore_ascii_case("getpremium") {
        // /GetPremium?ref=<ref> (940933af7)
        let mut ref_ = get_arg("ref").to_ascii_lowercase();
        if !is_valid_premium_referrer(&ref_) || ref_.len() > 32 {
            ref_.clear();
        }
        if !ref_
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
        {
            ref_.clear();
        }
        if !ref_.is_empty() {
            ref_ = format!("tme_getpremium_{ref_}");
        } else {
            ref_ = "tme_getpremium".to_string();
        }
        return Some(InternalLink::PremiumFeatures { referrer: ref_ });
    } else if path[0] == "giftcode" {
        if path.len() >= 2 && is_valid_gift_code(&path[1]) {
            // /giftcode/<code>
            return Some(InternalLink::PremiumGiftCode {
                code: path[1].clone(),
            });
        }
    } else if path[0] == "m" {
        if path.len() >= 2 && is_valid_business_link_name(&path[1]) {
            // /m/<link_name>
            return Some(InternalLink::BusinessChat {
                link_name: path[1].clone(),
            });
        }
    } else if path[0].starts_with('$') {
        let invoice_name = path[0][1..].to_owned();
        if is_valid_invoice_name(&invoice_name) {
            // /$<invoice_name>
            return Some(InternalLink::Invoice { invoice_name });
        }
    } else if path[0] == "share" || path[0] == "msg" {
        if path.len() > 1 && path[1] == "url" {
            // /share/url?url=<url>&text=<text>
            return get_internal_link_message_draft(&get_arg("url"), &get_arg("text"));
        }
    } else if path[0] == "iv" {
        if path.len() == 1 && has_arg("url") {
            // /iv?url=<url>&rhash=<rhash>
            let mut url = get_t_me_url(env);
            url.push_str("iv");
            copy_arg(&mut url, &url_query, &mut is_first_arg, "url");
            copy_arg(&mut url, &url_query, &mut is_first_arg, "rhash");
            return Some(InternalLink::InstantView {
                url,
                fallback_url: get_arg("url"),
            });
        }
    } else if path.len() >= 3
        && path[0] == "newbot"
        && crate::misc::is_valid_username(&path[1])
        && (path[2].is_empty() || crate::misc::is_valid_username(&path[2]))
    {
        // /newbot/<manager_bot_username>/<new_bot_username>?name=<new_bot_name>
        return Some(InternalLink::RequestManagedBot {
            manager_bot_username: path[1].clone(),
            bot_username: path[2].clone(),
            bot_name: get_arg("name"),
        });
    } else if path.len() == 2 && path[0] == "newbot" && crate::misc::is_valid_username(&path[1]) {
        // /newbot/<manager_bot_username>?name=<new_bot_name>
        return Some(InternalLink::RequestManagedBot {
            manager_bot_username: path[1].clone(),
            bot_username: String::new(),
            bot_name: get_arg("name"),
        });
    } else if crate::misc::is_valid_username(&path[0]) && path[0] != "i" {
        if path.len() >= 2 && to_integer_i64(&path[1]) > 0 {
            // /<username>/12345?single&thread=<thread_id>&comment=<message_id>
            // /<username>/1234/12345?single&comment=<message_id>
            is_first_arg = false;
            let mut post = to_integer_i64(&path[1]);
            let mut thread = copy_arg_to_string(&url_query, &mut is_first_arg, "thread");
            if path.len() >= 3 && to_integer_i64(&path[2]) > 0 {
                thread = format!("&thread={post}");
                post = to_integer_i64(&path[2]);
            }
            copied.push_str(&format!(
                "tg://resolve?domain={}&post={post}",
                url_encode(&path[0])
            ));
            copy_arg(&mut copied, &url_query, &mut is_first_arg, "single");
            copied.push_str(&thread);
            copy_arg(&mut copied, &url_query, &mut is_first_arg, "comment");
            copy_arg(&mut copied, &url_query, &mut is_first_arg, "t");
            copy_arg(&mut copied, &url_query, &mut is_first_arg, "task");
            copy_arg(&mut copied, &url_query, &mut is_first_arg, "option");
            return Some(InternalLink::Message { url: copied });
        }
        let username = path[0].clone();
        if username.to_lowercase() == "boost" {
            if path.len() == 2 && crate::misc::is_valid_username(&path[1]) {
                // /boost/<username>
                return Some(InternalLink::DialogBoost {
                    url: format!("tg://boost?domain={}", url_encode(&path[1])),
                });
            }
            let channel_id = get_arg("c");
            if path.len() == 1 && to_integer_i64(&channel_id) > 0 {
                // /boost?c=<channel_id>
                return Some(InternalLink::DialogBoost {
                    url: format!("tg://boost?channel={}", to_integer_i64(&channel_id)),
                });
            }
        }
        if path.len() == 3 && path[1] == "s" && is_valid_story_id(&path[2]) {
            // /<username>/s/<story_id>
            return Some(InternalLink::Story {
                story_poster_username: username,
                story_id: StoryId::new(to_integer_i32(&path[2])),
            });
        }
        if path.len() == 3 && path[1] == "s" && path[2] == "live" {
            // /<username>/s/live
            return Some(InternalLink::LiveStory {
                dialog_username: username,
            });
        }
        if path.len() == 3 && path[1] == "c" && is_valid_star_gift_collection_id(&path[2]) {
            // /<username>/c/<collection_id>
            return Some(InternalLink::StarGiftCollection {
                gift_owner_username: username,
                collection_id: StarGiftCollectionId::new(to_integer_i32(&path[2])),
            });
        }
        if path.len() == 3 && path[1] == "a" && is_valid_story_album_id(&path[2]) {
            // /<username>/a/<story_album_id>
            return Some(InternalLink::StoryAlbum {
                story_album_owner_username: username,
                story_album_id: StoryAlbumId::new(to_integer_i32(&path[2])),
            });
        }
        if path.len() == 2 && is_valid_web_app_name(&path[1]) {
            // /<username>/<web_app_name>?startapp=<start_parameter>&mode=compact
            return Some(InternalLink::WebApp {
                bot_username: username,
                web_app_short_name: path[1].clone(),
                start_parameter: get_arg("startapp"),
                mode: get_arg("mode"),
            });
        }
        for (key, value) in &url_query.args {
            if (key == "voicechat" || key == "videochat" || key == "livestream")
                && is_base64url_characters(value.as_bytes())
            {
                // /<username>?videochat[=<invite_hash>]
                env.reload_video_chat_on_search(&username);
                return Some(InternalLink::VideoChat {
                    dialog_username: username,
                    invite_hash: value.clone(),
                    is_live_stream: key == "livestream",
                });
            }
            if key == "boost" {
                // /<username>?boost
                return Some(InternalLink::DialogBoost {
                    url: format!("tg://boost?domain={}", url_encode(&username)),
                });
            }
            if key == "ref" && is_valid_start_parameter(value) && !value.is_empty() {
                // /<bot_username>?ref=<referrer>
                return Some(InternalLink::DialogReferralProgram {
                    username,
                    referral: value.clone(),
                });
            }
            if key == "start" && is_valid_start_parameter(value) {
                let prefixes = get_referral_program_start_parameter_prefixes(env);
                for prefix in &prefixes {
                    if let Some(rest) = value.strip_prefix(prefix.as_str())
                        && value.len() > prefix.len()
                    {
                        // /<bot_username>?start=_tgr_<referrer>
                        return Some(InternalLink::DialogReferralProgram {
                            username,
                            referral: rest.to_owned(),
                        });
                    }
                }
                // /<bot_username>?start=<parameter>
                return Some(InternalLink::BotStart {
                    bot_username: username,
                    start_parameter: value.clone(),
                    autostart: is_trusted,
                });
            }
            if key == "startgroup" && is_valid_start_parameter(value) {
                // /<bot_username>?startgroup[=<parameter>]&admin=...
                let administrator_rights =
                    get_administrator_rights(url_query.get_arg("admin").unwrap_or(""), false);
                return Some(InternalLink::BotStartInGroup {
                    bot_username: username,
                    start_parameter: value.clone(),
                    administrator_rights,
                });
            }
            if key == "startchannel" {
                // /<bot_username>?startchannel&admin=...
                let administrator_rights =
                    get_administrator_rights(url_query.get_arg("admin").unwrap_or(""), true);
                if administrator_rights != AdministratorRights::default() {
                    return Some(InternalLink::BotAddToChannel {
                        bot_username: username,
                        administrator_rights,
                    });
                }
            }
            if key == "startapp" && is_valid_start_parameter(value) {
                // /<bot_username>?startapp[=<parameter>]&mode=compact
                return Some(InternalLink::MainWebApp {
                    bot_username: username,
                    start_parameter: value.clone(),
                    mode: get_arg("mode"),
                });
            }
            if key == "game" && is_valid_game_name(value) {
                // /<bot_username>?game=<short_name>
                return Some(InternalLink::Game {
                    bot_username: username,
                    game_short_name: value.clone(),
                });
            }
            if key == "attach" && crate::misc::is_valid_username(value) {
                // /<username>?attach=<bot_username>&startattach=<start_parameter>
                return Some(InternalLink::new_attach_menu_bot(
                    None,
                    Some(InternalLink::PublicDialog {
                        dialog_username: username,
                        draft_text: String::new(),
                        open_profile: false,
                        unpaid_messages_token: String::new(),
                    }),
                    value.clone(),
                    url_query.get_arg("startattach").unwrap_or(""),
                ));
            }
            if key == "startattach" && !has_arg("attach") {
                // /<bot_username>?startattach[=<start_parameter>]&choose=...
                return Some(InternalLink::new_attach_menu_bot(
                    get_target_chat_types(url_query.get_arg("choose").unwrap_or("")),
                    None,
                    username,
                    value,
                ));
            }
            if key == "direct" {
                // /<username>?direct
                return Some(InternalLink::Monoforum {
                    channel_username: username,
                });
            }
        }

        // /<username>
        // /<username>?free=<token>
        return Some(InternalLink::PublicDialog {
            dialog_username: username,
            draft_text: get_url_query_draft_text(&url_query),
            open_profile: url_query.has_arg("profile"),
            unpaid_messages_token: get_url_query_unpaid_messages_token(&url_query),
        });
    }
    None
}

/// `LinkManager::get_internal_link` (cpp:2986-2992): the td_api → link
/// direction. `link_type` mirrors the C++ `object_ptr` (null → the
/// "Link type must be non-empty" error).
pub fn get_internal_link(
    env: &mut impl LinkEnv,
    link_type: Option<&td_api::InternalLinkType>,
    is_internal: bool,
) -> std::result::Result<String, Status> {
    let Some(link_type) = link_type else {
        return Err(Status::error(400, "Link type must be non-empty"));
    };
    get_internal_link_impl(env, link_type, is_internal)
}

/// `LinkManager::get_internal_link_impl` (cpp:2994-3998) — the per-type link
/// builders.
fn get_internal_link_impl(
    env: &mut impl LinkEnv,
    type_ptr: &td_api::InternalLinkType,
    is_internal: bool,
) -> std::result::Result<String, Status> {
    use td_api::InternalLinkType as T;
    match type_ptr {
        T::internalLinkTypeAttachmentMenuBot(link) => {
            let bot_username = &link.bot_username;
            if !crate::misc::is_valid_username(bot_username) {
                return Err(Status::error(400, "Invalid bot username specified"));
            }
            let mut start_parameter = String::new();
            if !link.url.is_empty() {
                let Some(start_parameter_slice) = link.url.strip_prefix("start://") else {
                    return Err(Status::error(400, "Unsupported link URL specified"));
                };
                if start_parameter_slice.is_empty()
                    || !is_valid_start_parameter(start_parameter_slice)
                {
                    return Err(Status::error(400, "Invalid start parameter specified"));
                }
                start_parameter = format!("={start_parameter_slice}");
            }
            let Some(target_chat) = link.target_chat.as_deref() else {
                return Err(Status::error(400, "Target chat must be non-empty"));
            };
            match target_chat {
                td_api::TargetChat::targetChatChosen(dialog_types) => {
                    let mut types: Vec<&str> = Vec::new();
                    if let Some(dialog_types) = dialog_types.types.as_ref() {
                        if dialog_types.allow_user_chats {
                            types.push("users");
                        }
                        if dialog_types.allow_bot_chats {
                            types.push("bots");
                        }
                        if dialog_types.allow_group_chats {
                            types.push("groups");
                        }
                        if dialog_types.allow_channel_chats {
                            types.push("channels");
                        }
                    }
                    if types.is_empty() {
                        return Err(Status::error(
                            400,
                            "At least one target chat type must be allowed",
                        ));
                    }
                    let choose = types.join("+");
                    if is_internal {
                        Ok(format!(
                            "tg://resolve?domain={bot_username}&startattach{start_parameter}&choose={choose}"
                        ))
                    } else {
                        Ok(format!(
                            "{}{bot_username}?startattach{start_parameter}&choose={choose}",
                            get_t_me_url(env)
                        ))
                    }
                }
                td_api::TargetChat::targetChatCurrent(_) => {
                    if is_internal {
                        Ok(format!(
                            "tg://resolve?domain={bot_username}&startattach{start_parameter}"
                        ))
                    } else {
                        Ok(format!(
                            "{}{bot_username}?startattach{start_parameter}",
                            get_t_me_url(env)
                        ))
                    }
                }
                td_api::TargetChat::targetChatInternalLink(target) => {
                    if !start_parameter.is_empty() {
                        start_parameter = format!("&startattach{start_parameter}");
                    }
                    let Some(target) = target.link.as_deref() else {
                        return Err(Status::error(400, "Target link must be non-empty"));
                    };
                    match target {
                        T::internalLinkTypeUserPhoneNumber(user_phone_number_link) => {
                            if user_phone_number_link.open_profile {
                                return Err(Status::error(
                                    400,
                                    "Link must not open chat profile information screen",
                                ));
                            }
                            let phone_number = user_phone_number_link
                                .phone_number
                                .strip_prefix('+')
                                .unwrap_or(&user_phone_number_link.phone_number);
                            if !is_valid_phone_number(phone_number) {
                                return Err(Status::error(
                                    400,
                                    "Invalid target phone number specified",
                                ));
                            }
                            if is_internal {
                                Ok(format!(
                                    "tg://resolve?phone=+{phone_number}&attach={bot_username}{start_parameter}"
                                ))
                            } else {
                                Ok(format!(
                                    "{}+{phone_number}?attach={bot_username}{start_parameter}",
                                    get_t_me_url(env)
                                ))
                            }
                        }
                        T::internalLinkTypePublicChat(public_chat_link) => {
                            if public_chat_link.open_profile {
                                return Err(Status::error(
                                    400,
                                    "Link must not open chat profile information screen",
                                ));
                            }
                            if !crate::misc::is_valid_username(&public_chat_link.chat_username) {
                                return Err(Status::error(
                                    400,
                                    "Invalid target public chat username specified",
                                ));
                            }
                            if is_internal {
                                Ok(format!(
                                    "tg://resolve?domain={}&attach={bot_username}{start_parameter}",
                                    public_chat_link.chat_username
                                ))
                            } else {
                                Ok(format!(
                                    "{}{}?attach={bot_username}{start_parameter}",
                                    get_t_me_url(env),
                                    public_chat_link.chat_username
                                ))
                            }
                        }
                        _ => Err(Status::error(400, "Unsupported target link specified")),
                    }
                }
            }
        }
        T::internalLinkTypeAuthenticationCode(link) => {
            if !is_valid_login_code(&link.code) {
                return Err(Status::error(400, "Invalid authentication code specified"));
            }
            if is_internal {
                Ok(format!("tg://login?code={}", url_encode(&link.code)))
            } else {
                Ok(format!(
                    "{}login/{}",
                    get_t_me_url(env),
                    url_encode(&link.code)
                ))
            }
        }
        T::internalLinkTypeBackground(link) => {
            let background_name = &link.background_name;
            let slug = match background_name.find('?') {
                Some(pos) if pos < background_name.len() => &background_name[..pos],
                _ => background_name,
            };
            if slug.is_empty() {
                return Err(Status::error(400, "Background name must be non-empty"));
            }

            if BackgroundType::is_background_name_local(slug) {
                let background_type = BackgroundType::get_local_background_type(background_name)?;
                let background_link = background_type.get_link(!is_internal);
                if background_type.has_file() {
                    // CHECK(!background_type.has_file()) in the C++ (cpp:3131)
                    unreachable!("a local background name never carries a file");
                }
                if is_internal {
                    let field_name = if background_type.has_gradient_fill() {
                        "gradient"
                    } else {
                        "color"
                    };
                    return Ok(format!("tg://bg?{field_name}={background_link}"));
                }
                return Ok(format!("{}bg/{background_link}", get_t_me_url(env)));
            }

            let prefix = if is_internal {
                "tg://bg?slug=".to_owned()
            } else {
                format!("{}bg/", get_t_me_url(env))
            };
            let url_query = parse_url_query(background_name);
            let mut is_first_arg = !is_internal;
            let mut result = format!("{prefix}{}", url_encode(slug));
            for name in ["mode", "intensity", "bg_color", "rotation"] {
                copy_arg(&mut result, &url_query, &mut is_first_arg, name);
            }
            Ok(result)
        }
        T::internalLinkTypeBotAddToChannel(link) => {
            if !crate::misc::is_valid_username(&link.bot_username) {
                return Err(Status::error(400, "Invalid bot username specified"));
            }
            let admin = get_admin_string(&AdministratorRights::from_chat_administrator_rights(
                link.administrator_rights.as_ref(),
                crate::chat_manager::ChannelType::Broadcast,
            ));
            if is_internal {
                Ok(format!(
                    "tg://resolve?domain={}&startchannel{admin}",
                    link.bot_username
                ))
            } else {
                Ok(format!(
                    "{}{}?startchannel{admin}",
                    get_t_me_url(env),
                    link.bot_username
                ))
            }
        }
        T::internalLinkTypeBotStart(link) => {
            if link.autostart {
                return Err(Status::error(400, "Can't create an autostart bot link"));
            }
            if !crate::misc::is_valid_username(&link.bot_username) {
                return Err(Status::error(400, "Invalid bot username specified"));
            }
            if !is_valid_start_parameter(&link.start_parameter) {
                return Err(Status::error(400, "Invalid start parameter specified"));
            }
            let start_parameter = if link.start_parameter.is_empty() {
                String::new()
            } else {
                format!("={}", link.start_parameter)
            };
            if is_internal {
                Ok(format!(
                    "tg://resolve?domain={}&start{start_parameter}",
                    link.bot_username
                ))
            } else {
                Ok(format!(
                    "{}{}?start{start_parameter}",
                    get_t_me_url(env),
                    link.bot_username
                ))
            }
        }
        T::internalLinkTypeBotStartInGroup(link) => {
            if !crate::misc::is_valid_username(&link.bot_username) {
                return Err(Status::error(400, "Invalid bot username specified"));
            }
            if !is_valid_start_parameter(&link.start_parameter) {
                return Err(Status::error(400, "Invalid start parameter specified"));
            }
            let admin = get_admin_string(&AdministratorRights::from_chat_administrator_rights(
                link.administrator_rights.as_ref(),
                crate::chat_manager::ChannelType::Megagroup,
            ));
            let start_parameter = if link.start_parameter.is_empty() {
                String::new()
            } else {
                format!("={}", link.start_parameter)
            };
            if is_internal {
                Ok(format!(
                    "tg://resolve?domain={}&startgroup{start_parameter}{admin}",
                    link.bot_username
                ))
            } else {
                Ok(format!(
                    "{}{}?startgroup{start_parameter}{admin}",
                    get_t_me_url(env),
                    link.bot_username
                ))
            }
        }
        T::internalLinkTypeBusinessChat(link) => {
            if !is_valid_business_link_name(&link.link_name) {
                return Err(Status::error_message("Invalid link name specified"));
            }
            if is_internal {
                Ok(format!("tg://message?slug={}", url_encode(&link.link_name)))
            } else {
                Ok(format!(
                    "{}m/{}",
                    get_t_me_url(env),
                    url_encode(&link.link_name)
                ))
            }
        }
        T::internalLinkTypeCallsPage(link) => {
            if !is_internal {
                return Err(Status::error_message(
                    "HTTP link is unavailable for the link type",
                ));
            }
            if get_calls_sections().contains(&link.section.as_str()) {
                Ok(format!("tg://settings/calls/{}", link.section))
            } else {
                Ok("tg://settings/calls".to_owned())
            }
        }
        T::internalLinkTypeChatAffiliateProgram(link) => {
            if !crate::misc::is_valid_username(&link.username) {
                return Err(Status::error(400, "Invalid username specified"));
            }
            if !is_valid_start_parameter(&link.referrer) || link.referrer.is_empty() {
                return Err(Status::error(400, "Invalid referrer specified"));
            }
            let start_parameter = format!(
                "start={}{}",
                get_referral_program_start_parameter_prefixes(env)[0],
                link.referrer
            );
            if is_internal {
                Ok(format!(
                    "tg://resolve?domain={}&{start_parameter}",
                    link.username
                ))
            } else {
                Ok(format!(
                    "{}{}?{start_parameter}",
                    get_t_me_url(env),
                    link.username
                ))
            }
        }
        T::internalLinkTypeChatBoost(link) => {
            let Some(parsed_link) = parse_internal_link(env, &link.url, false) else {
                return Err(Status::error(400, "Invalid chat boost URL specified"));
            };
            let parsed_object = parsed_link.get_internal_link_type_object(env);
            let parsed_object = match parsed_object {
                T::internalLinkTypeChatBoost(boost) => T::internalLinkTypeChatBoost(boost),
                _ => return Err(Status::error(400, "Invalid chat boost URL specified")),
            };
            if !is_internal {
                return Err(Status::error(
                    400,
                    "Use getChatBoostLink to get an HTTPS link to boost a chat",
                ));
            }
            let T::internalLinkTypeChatBoost(parsed) = parsed_object else {
                unreachable!("checked above");
            };
            Ok(parsed.url)
        }
        T::internalLinkTypeChatFolderInvite(link) => {
            let slug = get_dialog_filter_invite_link_slug(env, &link.invite_link);
            if slug.is_empty() {
                return Err(Status::error(400, "Invalid invite link specified"));
            }
            Ok(get_dialog_filter_invite_link(env, &slug, is_internal))
        }
        T::internalLinkTypeChatInvite(link) => {
            let invite_hash = get_dialog_invite_link_hash(env, &link.invite_link);
            if invite_hash.is_empty() {
                return Err(Status::error(400, "Invalid invite link specified"));
            }
            Ok(get_dialog_invite_link(env, &invite_hash, is_internal))
        }
        T::internalLinkTypeChatSelection(_) => {
            if !is_internal {
                return Err(Status::error_message(
                    "HTTP link is unavailable for the link type",
                ));
            }
            Ok("tg://chats/edit".to_owned())
        }
        T::internalLinkTypeContactsPage(link) => {
            if !is_internal {
                return Err(Status::error_message(
                    "HTTP link is unavailable for the link type",
                ));
            }
            if get_contacts_sections().contains(&link.section.as_str()) {
                Ok(format!("tg://contacts/{}", link.section))
            } else {
                Ok("tg://contacts".to_owned())
            }
        }
        T::internalLinkTypeDirectMessagesChat(link) => {
            if !crate::misc::is_valid_username(&link.channel_username) {
                return Err(Status::error(400, "Invalid channel username specified"));
            }
            if is_internal {
                Ok(format!(
                    "tg://resolve?domain={}&direct",
                    url_encode(&link.channel_username)
                ))
            } else {
                Ok(format!(
                    "{}{}?direct",
                    get_t_me_url(env),
                    url_encode(&link.channel_username)
                ))
            }
        }
        T::internalLinkTypeGame(link) => {
            if !crate::misc::is_valid_username(&link.bot_username) {
                return Err(Status::error(400, "Invalid bot username specified"));
            }
            if !is_valid_game_name(&link.game_short_name) {
                return Err(Status::error(400, "Invalid game name specified"));
            }
            if is_internal {
                Ok(format!(
                    "tg://resolve?domain={}&game={}",
                    link.bot_username, link.game_short_name
                ))
            } else {
                Ok(format!(
                    "{}{}?game={}",
                    get_t_me_url(env),
                    link.bot_username,
                    link.game_short_name
                ))
            }
        }
        T::internalLinkTypeGiftAuction(link) => {
            if link.auction_id.is_empty() {
                return Err(Status::error(
                    400,
                    "Invalid gift auction identifier specified",
                ));
            }
            if is_internal {
                Ok(format!(
                    "tg://stargift_auction?slug={}",
                    url_encode(&link.auction_id)
                ))
            } else {
                Ok(format!(
                    "{}auction/{}",
                    get_t_me_url(env),
                    url_encode(&link.auction_id)
                ))
            }
        }
        T::internalLinkTypeGiftCollection(link) => {
            if !crate::misc::is_valid_username(&link.gift_owner_username) {
                return Err(Status::error(
                    400,
                    "Invalid gift collection owner username specified",
                ));
            }
            if !StarGiftCollectionId::new(link.collection_id).is_valid() {
                return Err(Status::error(
                    400,
                    "Invalid gift collection identifier specified",
                ));
            }
            if is_internal {
                Ok(format!(
                    "tg://resolve?domain={}&collection={}",
                    link.gift_owner_username, link.collection_id
                ))
            } else {
                Ok(format!(
                    "{}{}/c/{}",
                    get_t_me_url(env),
                    link.gift_owner_username,
                    link.collection_id
                ))
            }
        }
        T::internalLinkTypeGroupCall(link) => {
            let slug = get_group_call_invite_link_slug(env, &link.invite_link);
            if slug.is_empty() {
                return Err(Status::error(400, "Invalid group call link specified"));
            }
            Ok(get_group_call_invite_link(env, &slug, is_internal))
        }
        T::internalLinkTypeInstantView(link) => {
            if !is_internal {
                return Err(Status::error_message(
                    "Deep link is unavailable for the link type",
                ));
            }
            let info = get_link_info(env, &link.url);
            let fallback_info = get_link_info(env, &link.fallback_url);
            match info.link_type {
                LinkType::External | LinkType::Tg | LinkType::TonConnect => {
                    Err(Status::error_message("Invalid instant view URL provided"))
                }
                LinkType::Telegraph => {
                    if fallback_info.link_type != LinkType::Telegraph
                        || link.url != format!("https://telegra.ph{}", fallback_info.query)
                    {
                        return Err(Status::error_message("Unrelated fallback URL provided"));
                    }
                    Ok(link.fallback_url.clone())
                }
                LinkType::TMe => {
                    // skip URL and fallback_url consistency checks
                    Ok(link.url.clone())
                }
            }
        }
        T::internalLinkTypeInvoice(link) => {
            if !is_valid_invoice_name(&link.invoice_name) {
                return Err(Status::error(400, "Invalid invoice name specified"));
            }
            if is_internal {
                Ok(format!(
                    "tg://invoice?slug={}",
                    url_encode(&link.invoice_name)
                ))
            } else {
                Ok(format!(
                    "{}${}",
                    get_t_me_url(env),
                    url_encode(&link.invoice_name)
                ))
            }
        }
        T::internalLinkTypeLanguagePack(link) => {
            if !is_valid_language_pack_id(&link.language_pack_id) {
                return Err(Status::error(400, "Invalid language pack specified"));
            }
            if is_internal {
                Ok(format!(
                    "tg://setlanguage?lang={}",
                    url_encode(&link.language_pack_id)
                ))
            } else {
                Ok(format!(
                    "{}setlanguage/{}",
                    get_t_me_url(env),
                    url_encode(&link.language_pack_id)
                ))
            }
        }
        T::internalLinkTypeLiveStory(link) => {
            if !crate::misc::is_valid_username(&link.story_poster_username) {
                return Err(Status::error(
                    400,
                    "Invalid story poster username specified",
                ));
            }
            if is_internal {
                Ok(format!(
                    "tg://resolve?domain={}&story=live",
                    link.story_poster_username
                ))
            } else {
                Ok(format!(
                    "{}{}/s/live",
                    get_t_me_url(env),
                    link.story_poster_username
                ))
            }
        }
        T::internalLinkTypeMainWebApp(link) => {
            if !crate::misc::is_valid_username(&link.bot_username) {
                return Err(Status::error(400, "Invalid bot username specified"));
            }
            let mut start_parameter = String::new();
            if !link.start_parameter.is_empty() {
                if !is_valid_start_parameter(&link.start_parameter) {
                    return Err(Status::error(400, "Invalid start parameter specified"));
                }
                start_parameter = format!("={}", link.start_parameter);
            }
            let mut mode = String::new();
            if let Some(mode_object) = link.mode.as_ref() {
                mode = get_web_app_open_mode_string(mode_object);
            }
            if is_internal {
                Ok(format!(
                    "tg://resolve?domain={}&startapp{start_parameter}{mode}",
                    link.bot_username
                ))
            } else {
                Ok(format!(
                    "{}{}?startapp{start_parameter}{mode}",
                    get_t_me_url(env),
                    link.bot_username
                ))
            }
        }
        T::internalLinkTypeMessage(link) => {
            let Some(parsed_link) = parse_internal_link(env, &link.url, false) else {
                return Err(Status::error(400, "Invalid message URL specified"));
            };
            let parsed_object = parsed_link.get_internal_link_type_object(env);
            let T::internalLinkTypeMessage(parsed) = parsed_object else {
                return Err(Status::error(400, "Invalid message URL specified"));
            };
            if !is_internal {
                return Err(Status::error(
                    400,
                    "Use getMessageLink to get an HTTPS link to a message",
                ));
            }
            Ok(parsed.url)
        }
        T::internalLinkTypeMessageDraft(link) => {
            let mut text = link
                .text
                .as_ref()
                .map(|formatted_text| formatted_text.text.clone())
                .unwrap_or_default();
            let url;
            if link.contains_link {
                let (first, rest) = match text.split_once('\n') {
                    Some((first, rest)) => (first.to_owned(), rest.to_owned()),
                    None => (text.clone(), String::new()),
                };
                url = first;
                text = rest;
            } else {
                url = text.clone();
                text.clear();
            }
            let text_parameter = if !text.is_empty() {
                format!("&text={}", url_encode(&text))
            } else {
                String::new()
            };
            if is_internal {
                Ok(format!(
                    "tg://msg_url?url={}{text_parameter}",
                    url_encode(&url)
                ))
            } else {
                Ok(format!(
                    "{}share/url?url={}{text_parameter}",
                    get_t_me_url(env),
                    url_encode(&url)
                ))
            }
        }
        T::internalLinkTypeMyProfilePage(link) => {
            if !is_internal {
                return Err(Status::error_message(
                    "HTTP link is unavailable for the link type",
                ));
            }
            if get_my_profile_sections().contains(&link.section.as_str()) {
                Ok(format!("tg://settings/my-profile/{}", link.section))
            } else {
                Ok("tg://settings/my-profile".to_owned())
            }
        }
        T::internalLinkTypeNewChannelChat(_) => {
            if !is_internal {
                return Err(Status::error_message(
                    "HTTP link is unavailable for the link type",
                ));
            }
            Ok("tg://new/channel".to_owned())
        }
        T::internalLinkTypeNewGroupChat(_) => {
            if !is_internal {
                return Err(Status::error_message(
                    "HTTP link is unavailable for the link type",
                ));
            }
            Ok("tg://new/group".to_owned())
        }
        T::internalLinkTypeNewPrivateChat(_) => {
            if !is_internal {
                return Err(Status::error_message(
                    "HTTP link is unavailable for the link type",
                ));
            }
            Ok("tg://new".to_owned())
        }
        T::internalLinkTypeNewStory(link) => {
            if !is_internal {
                return Err(Status::error_message(
                    "HTTP link is unavailable for the link type",
                ));
            }
            if let Some(content_type) = link.content_type.as_ref() {
                return Ok(match content_type {
                    td_api::StoryContentType::storyContentTypePhoto(_) => "tg://post/photo",
                    td_api::StoryContentType::storyContentTypeVideo(_) => "tg://post/video",
                    td_api::StoryContentType::storyContentTypeLive(_) => "tg://post/live",
                    td_api::StoryContentType::storyContentTypeUnsupported(_) => {
                        "tg://post/unsupported"
                    }
                }
                .to_owned());
            }
            Ok("tg://post".to_owned())
        }
        T::internalLinkTypeOauth(link) => {
            if !is_internal {
                return Err(Status::error_message(
                    "HTTP link is unavailable for the link type",
                ));
            }
            let Some(parsed_link) = parse_internal_link(env, &link.url, false) else {
                return Err(Status::error(400, "Invalid OAuth URL specified"));
            };
            let parsed_object = parsed_link.get_internal_link_type_object(env);
            let T::internalLinkTypeOauth(parsed) = parsed_object else {
                return Err(Status::error(400, "Invalid OAuth URL specified"));
            };
            Ok(parsed.url)
        }
        T::internalLinkTypePassportDataRequest(link) => {
            if !is_internal {
                return Err(Status::error_message(
                    "HTTP link is unavailable for the link type",
                ));
            }
            if !UserId::new(link.bot_user_id).is_valid() {
                return Err(Status::error_message(
                    "Invalid bot user identifier specified",
                ));
            }
            if link.scope.is_empty()
                || !tdutils::utf8::check_utf8(link.scope.as_bytes())
                || link.public_key.is_empty()
                || !tdutils::utf8::check_utf8(link.public_key.as_bytes())
                || link.nonce.is_empty()
                || !tdutils::utf8::check_utf8(link.nonce.as_bytes())
            {
                return Err(Status::error_message("Invalid parameters specified"));
            }
            Ok(format!(
                "tg://resolve?domain=telegrampassport&bot_id={}&scope={}&public_key={}&nonce={}&callback_url={}",
                link.bot_user_id,
                url_encode(&link.scope),
                url_encode(&link.public_key),
                url_encode(&link.nonce),
                url_encode(&link.callback_url)
            ))
        }
        T::internalLinkTypePhoneNumberConfirmation(link) => {
            if !is_valid_phone_number(&link.phone_number) {
                return Err(Status::error_message("Invalid phone number specified"));
            }
            if !is_valid_phone_number_hash(&link.hash) {
                return Err(Status::error_message("Invalid phone number hash specified"));
            }
            if is_internal {
                Ok(format!(
                    "tg://confirmphone?phone={}&hash={}",
                    url_encode(&link.phone_number),
                    url_encode(&link.hash)
                ))
            } else {
                Ok(format!(
                    "{}confirmphone?phone={}&hash={}",
                    get_t_me_url(env),
                    url_encode(&link.phone_number),
                    url_encode(&link.hash)
                ))
            }
        }
        T::internalLinkTypePremiumFeaturesPage(link) => {
            if !is_valid_premium_referrer(&link.referrer) {
                return Err(Status::error_message("Invalid referrer specified"));
            }
            if !is_internal {
                // 940933af7: t.me/GetPremium links for the page form.
                let mut ref_ = String::new();
                let mut is_valid = true;
                if link.referrer != "tme_getpremium" {
                    // ok
                    is_valid = link.referrer.starts_with("tme_getpremium_");
                    if is_valid {
                        let referrer = &link.referrer["tme_getpremium_".len()..];
                        if referrer.is_empty() || referrer.len() > 32 {
                            is_valid = false;
                        }
                        for c in referrer.bytes() {
                            if !c.is_ascii_lowercase() && !c.is_ascii_digit() && c != b'_' {
                                is_valid = false;
                                break;
                            }
                        }
                        if is_valid {
                            ref_ = format!("?ref={referrer}");
                        }
                    }
                }
                if !is_valid {
                    return Err(Status::error_message(
                        "HTTP link is unavailable for the link type",
                    ));
                }
                return Ok(format!("{}GetPremium{ref_}", get_t_me_url(env)));
            }
            Ok(format!(
                "tg://premium_offer?ref={}",
                url_encode(&link.referrer)
            ))
        }
        T::internalLinkTypePremiumGiftCode(link) => {
            if !is_valid_gift_code(&link.code) {
                return Err(Status::error_message("Invalid gift code specified"));
            }
            if is_internal {
                Ok(format!("tg://giftcode?slug={}", url_encode(&link.code)))
            } else {
                Ok(format!(
                    "{}giftcode/{}",
                    get_t_me_url(env),
                    url_encode(&link.code)
                ))
            }
        }
        T::internalLinkTypePremiumGiftPurchase(link) => {
            if !is_internal {
                return Err(Status::error_message(
                    "HTTP link is unavailable for the link type",
                ));
            }
            if !is_valid_premium_referrer(&link.referrer) {
                return Err(Status::error_message("Invalid referrer specified"));
            }
            Ok(format!(
                "tg://premium_multigift?ref={}",
                url_encode(&link.referrer)
            ))
        }
        T::internalLinkTypeProxy(link) => {
            let Some(proxy) = link.proxy.as_ref() else {
                if is_internal {
                    return Ok("tg://proxy?port=-1&server=0.0.0.0".to_owned());
                }
                return Ok(format!("{}proxy?port=-1&server=0.0.0.0", get_t_me_url(env)));
            };
            let proxy = NetProxy::create_proxy(proxy)?;
            get_proxy_link(env, &proxy, is_internal)
        }
        T::internalLinkTypePublicChat(link) => {
            if !crate::misc::is_valid_username(&link.chat_username) {
                return Err(Status::error(400, "Invalid chat username specified"));
            }
            if !tdutils::utf8::check_utf8(link.draft_text.as_bytes()) {
                return Err(Status::error(400, "Draft text must be encoded in UTF-8"));
            }
            if !link.unpaid_messages_token.is_empty()
                && !is_valid_unpaid_messages_token(&link.unpaid_messages_token)
            {
                return Err(Status::error(
                    400,
                    "Invalid unpaid messages token specified",
                ));
            }
            Ok(get_public_dialog_link(
                env,
                &link.chat_username,
                &link.draft_text,
                link.open_profile,
                &link.unpaid_messages_token,
                is_internal,
            ))
        }
        T::internalLinkTypeQrCodeAuthentication(_) => Err(Status::error_message(
            "The link must never be generated client-side",
        )),
        T::internalLinkTypeRequestManagedBot(link) => {
            if !crate::misc::is_valid_username(&link.manager_bot_username) {
                return Err(Status::error(400, "Invalid manager bot username specified"));
            }
            if !crate::misc::is_valid_username(&link.suggested_bot_username) {
                return Err(Status::error(
                    400,
                    "Invalid suggested bot username specified",
                ));
            }
            if !tdutils::utf8::check_utf8(link.suggested_bot_name.as_bytes()) {
                return Err(Status::error(
                    400,
                    "Suggested bot name must be encoded in UTF-8",
                ));
            }
            if is_internal {
                Ok(format!(
                    "tg://newbot?manager={}&username={}&name={}",
                    link.manager_bot_username,
                    link.suggested_bot_username,
                    url_encode(&link.suggested_bot_name)
                ))
            } else {
                Ok(format!(
                    "{}newbot/{}/{}?name={}",
                    get_t_me_url(env),
                    link.manager_bot_username,
                    link.suggested_bot_username,
                    url_encode(&link.suggested_bot_name)
                ))
            }
        }
        T::internalLinkTypeRestorePurchases(_) => {
            if !is_internal {
                return Err(Status::error_message(
                    "HTTP link is unavailable for the link type",
                ));
            }
            Ok("tg://restore_purchases".to_owned())
        }
        T::internalLinkTypeSavedMessages(_) => {
            if !is_internal {
                return Err(Status::error_message(
                    "HTTP link is unavailable for the link type",
                ));
            }
            Ok("tg://settings/saved-messages".to_owned())
        }
        T::internalLinkTypeSearch(_) => {
            if !is_internal {
                return Err(Status::error_message(
                    "HTTP link is unavailable for the link type",
                ));
            }
            Ok("tg://chats/search".to_owned())
        }
        T::internalLinkTypeSettings(link) => {
            if !is_internal {
                return Err(Status::error_message(
                    "HTTP link is unavailable for the link type",
                ));
            }
            let Some(section) = link.section.as_ref() else {
                return Ok("tg://settings".to_owned());
            };
            get_settings_link(section)
        }
        T::internalLinkTypeStarPurchase(link) => {
            if !is_internal {
                return Err(Status::error_message(
                    "HTTP link is unavailable for the link type",
                ));
            }
            if link.star_count <= 0 {
                return Err(Status::error(400, "Invalid Telegram Star amount provided"));
            }
            if !is_valid_star_top_up_purpose(&link.purpose) {
                return Err(Status::error(400, "Invalid purpose specified"));
            }
            Ok(format!(
                "tg://stars_topup?balance={}&purpose={}",
                link.star_count,
                url_encode(&link.purpose)
            ))
        }
        T::internalLinkTypeStickerSet(link) => {
            if !is_valid_sticker_set_name(&link.sticker_set_name) {
                return Err(Status::error(400, "Invalid sticker set name specified"));
            }
            let name = if link.expect_custom_emoji {
                "emoji"
            } else {
                "stickers"
            };
            if is_internal {
                Ok(format!(
                    "tg://add{name}?set={}",
                    url_encode(&link.sticker_set_name)
                ))
            } else {
                Ok(format!(
                    "{}add{}/{}",
                    get_t_me_url(env),
                    name,
                    url_encode(&link.sticker_set_name)
                ))
            }
        }
        T::internalLinkTypeStory(link) => {
            if !crate::misc::is_valid_username(&link.story_poster_username) {
                return Err(Status::error(
                    400,
                    "Invalid story poster username specified",
                ));
            }
            if !StoryId::new(link.story_id).is_server() {
                return Err(Status::error(400, "Invalid story identifier specified"));
            }
            if is_internal {
                Ok(format!(
                    "tg://resolve?domain={}&story={}",
                    link.story_poster_username, link.story_id
                ))
            } else {
                Ok(format!(
                    "{}{}/s/{}",
                    get_t_me_url(env),
                    link.story_poster_username,
                    link.story_id
                ))
            }
        }
        T::internalLinkTypeStoryAlbum(link) => {
            if !crate::misc::is_valid_username(&link.story_album_owner_username) {
                return Err(Status::error(
                    400,
                    "Invalid story album owner username specified",
                ));
            }
            if !StoryAlbumId::new(link.story_album_id).is_valid() {
                return Err(Status::error(
                    400,
                    "Invalid story album identifier specified",
                ));
            }
            if is_internal {
                Ok(format!(
                    "tg://resolve?domain={}&album={}",
                    link.story_album_owner_username, link.story_album_id
                ))
            } else {
                Ok(format!(
                    "{}{}/a/{}",
                    get_t_me_url(env),
                    link.story_album_owner_username,
                    link.story_album_id
                ))
            }
        }
        T::internalLinkTypeTextCompositionStyle(link) => {
            if !is_valid_text_composition_style_name(&link.style_name) {
                return Err(Status::error(400, "Invalid style name specified"));
            }
            if is_internal {
                Ok(format!(
                    "tg://addstyle?slug={}",
                    url_encode(&link.style_name)
                ))
            } else {
                Ok(format!(
                    "{}addstyle/{}",
                    get_t_me_url(env),
                    url_encode(&link.style_name)
                ))
            }
        }
        T::internalLinkTypeTheme(link) => {
            if !is_valid_theme_name(&link.theme_name) {
                return Err(Status::error(400, "Invalid theme name specified"));
            }
            if is_internal {
                Ok(format!(
                    "tg://addtheme?slug={}",
                    url_encode(&link.theme_name)
                ))
            } else {
                Ok(format!(
                    "{}addtheme/{}",
                    get_t_me_url(env),
                    url_encode(&link.theme_name)
                ))
            }
        }
        T::internalLinkTypeTonConnect(_) => {
            // TON Connect links are unsupported as HTTP links
            // (LinkManager.cpp:4076-4077).
            Err(Status::error(
                400,
                "HTTP link is unavailable for the link type",
            ))
        }
        T::internalLinkTypeTonWalletTransfer(link) => {
            // LinkManager.cpp:3974-4028.
            let receiver = match link.receiver.as_ref() {
                None => String::new(),
                Some(td_api::TonWalletTransferReceiver::tonWalletTransferReceiverUser(user)) => {
                    format!("@{}", user.username)
                }
                Some(td_api::TonWalletTransferReceiver::tonWalletTransferReceiverAddress(
                    address,
                )) => {
                    if address.address.is_empty() || address.address.starts_with('@') {
                        "!invalid!".to_string()
                    } else {
                        address.address.clone()
                    }
                }
            };
            if !is_valid_gram_receiver(&receiver) {
                return Err(Status::error(400, "Invalid receiver specified"));
            }
            if !(0..9_000_000_000_000_000).contains(&link.gram_amount) {
                return Err(Status::error(400, "Invalid amount specified"));
            }
            if receiver.is_empty() && link.gram_amount != 0 {
                return Err(Status::error(
                    400,
                    "Amount must not be specified without the receiver",
                ));
            }
            let mut result = if is_internal {
                "tg://sendgrams".to_string()
            } else {
                format!("{}sendgrams", get_t_me_url(env))
            };
            if !receiver.is_empty() {
                result += "?to=";
                result += &receiver;
                if link.gram_amount != 0 {
                    result += "&amount=";
                    result += &(link.gram_amount / 1_000_000_000).to_string();
                    let mut fractional_part = link.gram_amount % 1_000_000_000;
                    if fractional_part != 0 {
                        result.push('.');
                        let mut multiplier = 100_000_000;
                        while fractional_part != 0 {
                            result.push((b'0' + (fractional_part / multiplier) as u8) as char);
                            fractional_part %= multiplier;
                            multiplier /= 10;
                        }
                    }
                }
            }
            Ok(result)
        }
        T::internalLinkTypeUnknownDeepLink(link) => {
            if !is_internal {
                return Err(Status::error_message(
                    "HTTP link is unavailable for the link type",
                ));
            }
            let Some(parsed_link) = parse_internal_link(env, &link.link, false) else {
                return Err(Status::error(400, "Invalid deep link URL specified"));
            };
            let parsed_object = parsed_link.get_internal_link_type_object(env);
            let T::internalLinkTypeUnknownDeepLink(parsed) = parsed_object else {
                return Err(Status::error(400, "Invalid deep link URL specified"));
            };
            Ok(parsed.link)
        }
        T::internalLinkTypeUpgradedGift(link) => {
            if !is_valid_upgraded_gift_name(&link.name) {
                return Err(Status::error(400, "Invalid gift name specified"));
            }
            if is_internal {
                Ok(format!("tg://nft?slug={}", url_encode(&link.name)))
            } else {
                Ok(format!(
                    "{}nft/{}",
                    get_t_me_url(env),
                    url_encode(&link.name)
                ))
            }
        }
        T::internalLinkTypeUserPhoneNumber(link) => {
            let phone_number = link
                .phone_number
                .strip_prefix('+')
                .unwrap_or(&link.phone_number);
            if !is_valid_phone_number(phone_number) {
                return Err(Status::error(400, "Invalid phone number specified"));
            }
            if !tdutils::utf8::check_utf8(link.draft_text.as_bytes()) {
                return Err(Status::error(400, "Draft text must be encoded in UTF-8"));
            }
            if !link.unpaid_messages_token.is_empty()
                && !is_valid_unpaid_messages_token(&link.unpaid_messages_token)
            {
                return Err(Status::error(
                    400,
                    "Invalid unpaid messages token specified",
                ));
            }
            let mut query = get_dialog_link_query(
                &link.draft_text,
                link.open_profile,
                &link.unpaid_messages_token,
            );
            if is_internal {
                Ok(format!("tg://resolve?phone=+{phone_number}{query}"))
            } else {
                if !query.is_empty() {
                    query.replace_range(0..1, "?");
                }
                Ok(format!("{}+{phone_number}{query}", get_t_me_url(env)))
            }
        }
        T::internalLinkTypeUserToken(link) => {
            if !is_valid_user_token(&link.token) {
                return Err(Status::error(400, "Invalid user token specified"));
            }
            if !link.unpaid_messages_token.is_empty()
                && !is_valid_unpaid_messages_token(&link.unpaid_messages_token)
            {
                return Err(Status::error(
                    400,
                    "Invalid unpaid messages token specified",
                ));
            }
            let mut query = get_dialog_link_query("", false, &link.unpaid_messages_token);
            if is_internal {
                Ok(format!(
                    "tg://contact?token={}{}",
                    url_encode(&link.token),
                    query
                ))
            } else {
                if !query.is_empty() {
                    query.replace_range(0..1, "?");
                }
                Ok(format!(
                    "{}contact/{}{}",
                    get_t_me_url(env),
                    url_encode(&link.token),
                    query
                ))
            }
        }
        T::internalLinkTypeVideoChat(link) => {
            if !crate::misc::is_valid_username(&link.chat_username) {
                return Err(Status::error(400, "Invalid chat username specified"));
            }
            if !is_base64url_characters(link.invite_hash.as_bytes()) {
                return Err(Status::error(400, "Invalid invite hash specified"));
            }
            let invite_hash = if !link.invite_hash.is_empty() {
                format!("={}", url_encode(&link.invite_hash))
            } else {
                String::new()
            };
            let name = if link.is_live_stream {
                "livestream"
            } else {
                "videochat"
            };
            if is_internal {
                Ok(format!(
                    "tg://resolve?domain={}&{name}{invite_hash}",
                    link.chat_username
                ))
            } else {
                Ok(format!(
                    "{}{}?{name}{invite_hash}",
                    get_t_me_url(env),
                    link.chat_username
                ))
            }
        }
        T::internalLinkTypeWebApp(link) => {
            if !crate::misc::is_valid_username(&link.bot_username) {
                return Err(Status::error(400, "Invalid bot username specified"));
            }
            if !is_valid_web_app_name(&link.web_app_short_name) {
                return Err(Status::error(400, "Invalid Web App name specified"));
            }
            if !is_valid_start_parameter(&link.start_parameter) {
                return Err(Status::error(400, "Invalid start parameter specified"));
            }
            let mut mode = String::new();
            if let Some(mode_object) = link.mode.as_ref() {
                mode = get_web_app_open_mode_string(mode_object);
            }
            let mut parameters = String::new();
            if !link.start_parameter.is_empty() {
                parameters = format!(
                    "{}startapp={}{}",
                    if is_internal { '&' } else { '?' },
                    link.start_parameter,
                    mode
                );
            } else if !mode.is_empty() {
                if !is_internal {
                    mode.replace_range(0..1, "?");
                }
                parameters = mode;
            }
            if is_internal {
                Ok(format!(
                    "tg://resolve?domain={}&appname={}{parameters}",
                    link.bot_username, link.web_app_short_name
                ))
            } else {
                Ok(format!(
                    "{}{}/{}{parameters}",
                    get_t_me_url(env),
                    link.bot_username,
                    link.web_app_short_name
                ))
            }
        }
    }
}

/// The `&mode=` string form of a `WebAppOpenMode` (cpp:3409-3422).
fn get_web_app_open_mode_string(mode_object: &td_api::WebAppOpenMode) -> String {
    match mode_object {
        td_api::WebAppOpenMode::webAppOpenModeCompact(_) => "&mode=compact".to_owned(),
        td_api::WebAppOpenMode::webAppOpenModeFullSize(_) => String::new(),
        td_api::WebAppOpenMode::webAppOpenModeFullScreen(_) => "&mode=fullscreen".to_owned(),
    }
}

/// `internalLinkTypeSettings`' reverse section table (cpp:3654-3792).
fn get_settings_link(section: &td_api::SettingsSection) -> std::result::Result<String, Status> {
    use td_api::SettingsSection as S;
    let in_table = |table: &[&'static str], subsection: &str| table.contains(&subsection);
    Ok(match section {
        S::settingsSectionAppearance(section) => {
            if in_table(get_appearance_settings_subsections(), &section.subsection) {
                format!("tg://settings/appearance/{}", section.subsection)
            } else {
                "tg://settings/themes".to_owned()
            }
        }
        S::settingsSectionBusiness(section) => {
            if in_table(get_business_settings_subsections(), &section.subsection) {
                format!("tg://settings/business/{}", section.subsection)
            } else {
                "tg://settings/business".to_owned()
            }
        }
        S::settingsSectionAskQuestion(_) => "tg://settings/ask-question".to_owned(),
        S::settingsSectionChatFolders(section) => {
            if in_table(get_folder_settings_subsections(), &section.subsection) {
                format!("tg://settings/folders/{}", section.subsection)
            } else {
                "tg://settings/folders".to_owned()
            }
        }
        S::settingsSectionDataAndStorage(section) => {
            if in_table(get_data_settings_subsections(), &section.subsection) {
                format!("tg://settings/data/{}", section.subsection)
            } else {
                "tg://settings/data".to_owned()
            }
        }
        S::settingsSectionDevices(section) => {
            if in_table(get_device_settings_subsections(), &section.subsection) {
                format!("tg://settings/devices/{}", section.subsection)
            } else {
                "tg://settings/devices".to_owned()
            }
        }
        S::settingsSectionEditProfile(section) => {
            if in_table(get_edit_profile_settings_subsections(), &section.subsection) {
                if section.subsection == "change-number" {
                    "tg://settings/change_number".to_owned()
                } else {
                    format!("tg://settings/edit/{}", section.subsection)
                }
            } else if in_table(
                get_edit_profile_other_settings_subsections(),
                &section.subsection,
            ) {
                format!("tg://settings/{}", section.subsection)
            } else {
                "tg://settings/edit_profile".to_owned()
            }
        }
        S::settingsSectionFaq(_) => "tg://settings/faq".to_owned(),
        S::settingsSectionFeatures(_) => "tg://settings/features".to_owned(),
        S::settingsSectionInAppBrowser(section) => {
            if in_table(
                get_in_app_browser_settings_subsections(),
                &section.subsection,
            ) {
                format!("tg://settings/chat/browser/{}", section.subsection)
            } else {
                "tg://settings/chat/browser".to_owned()
            }
        }
        S::settingsSectionLanguage(section) => {
            if in_table(get_language_settings_subsections(), &section.subsection) {
                format!("tg://settings/language/{}", section.subsection)
            } else {
                "tg://settings/language".to_owned()
            }
        }
        S::settingsSectionMyStars(section) => {
            if in_table(get_my_stars_settings_subsections(), &section.subsection) {
                format!("tg://settings/stars/{}", section.subsection)
            } else {
                "tg://stars".to_owned()
            }
        }
        S::settingsSectionMyGrams(_) => "tg://ton".to_owned(),
        S::settingsSectionNotifications(section) => {
            if in_table(get_notification_settings_subsections(), &section.subsection) {
                format!("tg://settings/notifications/{}", section.subsection)
            } else {
                "tg://settings/notifications".to_owned()
            }
        }
        S::settingsSectionPowerSaving(section) => {
            if in_table(get_power_saving_settings_subsections(), &section.subsection) {
                format!("tg://settings/power-saving/{}", section.subsection)
            } else {
                "tg://settings/power-saving".to_owned()
            }
        }
        S::settingsSectionPremium(_) => "tg://settings/premium".to_owned(),
        S::settingsSectionPrivacyAndSecurity(section) => {
            if in_table(get_privacy_settings_subsections(), &section.subsection) {
                if section.subsection == "phone-number" {
                    "tg://settings/phone_privacy".to_owned()
                } else if section.subsection == "auto-delete" {
                    "tg://settings/auto_delete".to_owned()
                } else if section.subsection == "login-email" {
                    "tg://settings/login_email".to_owned()
                } else if section.subsection == "2sv" {
                    "tg://settings/password".to_owned()
                } else {
                    format!("tg://settings/privacy/{}", section.subsection)
                }
            } else {
                "tg://settings/privacy".to_owned()
            }
        }
        S::settingsSectionPrivacyPolicy(_) => "tg://settings/privacy-policy".to_owned(),
        S::settingsSectionQrCode(section) => {
            if in_table(get_qr_code_settings_subsections(), &section.subsection) {
                format!("tg://settings/qr-code/{}", section.subsection)
            } else {
                "tg://settings/qr-code".to_owned()
            }
        }
        S::settingsSectionSearch(_) => "tg://settings/search".to_owned(),
        S::settingsSectionSendGift(section) => {
            if in_table(get_send_gift_settings_subsections(), &section.subsection) {
                format!("tg://settings/send-gift/{}", section.subsection)
            } else {
                "tg://settings/send-gift".to_owned()
            }
        }
    })
}

/// `LinkManager::get_public_dialog_link` (cpp:4881-4892).
fn get_public_dialog_link(
    env: &impl LinkEnv,
    username: &str,
    draft_text: &str,
    open_profile: bool,
    unpaid_messages_token: &str,
    is_internal: bool,
) -> String {
    let mut query = get_dialog_link_query(draft_text, open_profile, unpaid_messages_token);
    if is_internal {
        format!("tg://resolve?domain={}{}", url_encode(username), query)
    } else {
        if !query.is_empty() {
            query.replace_range(0..1, "?");
        }
        format!("{}{}{}", get_t_me_url(env), url_encode(username), query)
    }
}

/// `LinkManager::get_dialog_filter_invite_link_slug` (cpp:4235-4246).
pub(crate) fn get_dialog_filter_invite_link_slug(env: &impl LinkEnv, invite_link: &str) -> String {
    let link_info = get_link_info(env, invite_link);
    if link_info.link_type != LinkType::Tg && link_info.link_type != LinkType::TMe {
        return String::new();
    }
    let url_query = parse_url_query(&link_info.query);
    let slug = get_url_query_slug(link_info.link_type == LinkType::Tg, &url_query, "addlist");
    if !is_base64url_characters(slug.as_bytes()) {
        return String::new();
    }
    slug
}

/// `LinkManager::get_dialog_invite_link_hash` (cpp:4259-4273).
pub(crate) fn get_dialog_invite_link_hash(env: &impl LinkEnv, invite_link: &str) -> String {
    let link_info = get_link_info(env, invite_link);
    if link_info.link_type != LinkType::Tg && link_info.link_type != LinkType::TMe {
        return String::new();
    }
    let url_query = parse_url_query(&link_info.query);
    let invite_hash = get_url_query_hash(link_info.link_type == LinkType::Tg, &url_query);
    if is_valid_phone_number(&invite_hash) {
        return String::new();
    }
    if !is_base64url_characters(invite_hash.as_bytes()) {
        return String::new();
    }
    invite_hash
}

/// `LinkManager::get_group_call_invite_link_slug` (cpp:4286-4297).
pub(crate) fn get_group_call_invite_link_slug(env: &impl LinkEnv, invite_link: &str) -> String {
    let link_info = get_link_info(env, invite_link);
    if link_info.link_type != LinkType::Tg && link_info.link_type != LinkType::TMe {
        return String::new();
    }
    let url_query = parse_url_query(&link_info.query);
    let slug = get_url_query_slug(link_info.link_type == LinkType::Tg, &url_query, "call");
    if !is_base64url_characters(slug.as_bytes()) {
        return String::new();
    }
    slug
}

/// `LinkManager::get_instant_view_link_url` (cpp:4310-4321) — the
/// MessagesManager/web-page consumer lands with the message plane.
#[allow(dead_code)] // consumed by the MessagesManager/web-page track
pub(crate) fn get_instant_view_link_url(env: &impl LinkEnv, link: &str) -> String {
    let link_info = get_link_info(env, link);
    if link_info.link_type != LinkType::TMe {
        return String::new();
    }
    let url_query = parse_url_query(&link_info.query);
    if url_query.path.len() == 1 && url_query.path[0] == "iv" {
        return url_query.get_arg("url").unwrap_or("").to_owned();
    }
    String::new()
}

/// `LinkManager::get_instant_view_link_rhash` (cpp:4323-4334) — the
/// MessagesManager/web-page consumer lands with the message plane.
#[allow(dead_code)] // consumed by the MessagesManager/web-page track
pub(crate) fn get_instant_view_link_rhash(env: &impl LinkEnv, link: &str) -> String {
    let link_info = get_link_info(env, link);
    if link_info.link_type != LinkType::TMe {
        return String::new();
    }
    let url_query = parse_url_query(&link_info.query);
    if url_query.path.len() == 1
        && url_query.path[0] == "iv"
        && !url_query.get_arg("url").unwrap_or("").is_empty()
    {
        return url_query.get_arg("rhash").unwrap_or("").to_owned();
    }
    String::new()
}

/// `LinkManager::get_instant_view_link` (cpp:4336-4338) — the
/// WebPageManager instant-view builder consumer (Wave 3).
#[allow(dead_code)] // consumed by the WebPageManager track
pub(crate) fn get_instant_view_link(env: &impl LinkEnv, url: &str, rhash: &str) -> String {
    format!(
        "{}iv?url={}&rhash={}",
        get_t_me_url(env),
        url_encode(url),
        url_encode(rhash)
    )
}

/// `LinkManager::has_video_chat_invite_hash` (cpp:4225-4233) — the
/// MessagesManager/search-consumer lands with the message plane.
#[allow(dead_code)] // consumed by the MessagesManager track
pub(crate) fn has_video_chat_invite_hash(env: &mut impl LinkEnv, link: &str) -> bool {
    let Some(internal_link) = parse_internal_link(env, link, false) else {
        return false;
    };
    matches!(
        internal_link.get_internal_link_type_object(env),
        td_api::InternalLinkType::internalLinkTypeVideoChat(ref video_chat) if !video_chat.invite_hash.is_empty()
    )
}

/// `LinkManager::get_background_url` (cpp:4178-4199).
pub fn get_background_url(
    env: &impl LinkEnv,
    name: &str,
    background_type: Option<&td_api::BackgroundType>,
) -> std::result::Result<String, Status> {
    let Some(background_type) = background_type else {
        return Err(Status::error(400, "Type must be non-empty"));
    };
    if matches!(
        background_type,
        td_api::BackgroundType::backgroundTypeChatTheme(_)
    ) {
        return Err(Status::error(400, "Background has no link"));
    }
    let background_type = BackgroundType::get_background_type(Some(background_type), 0)?;
    let mut url = format!("{}bg/", get_t_me_url(env));
    let link = background_type.get_link(true);
    if background_type.has_file() {
        url += name;
        if !link.is_empty() {
            url.push('?');
            url += &link;
        }
    } else {
        url += &link;
    }
    Ok(url)
}

/// `LinkManager::get_background_type_object` (cpp:4201-4223) — the
/// background/search-consumer lands with the BackgroundManager track.
#[allow(dead_code)] // consumed by the BackgroundManager track
pub(crate) fn get_background_type_object(
    env: &mut impl LinkEnv,
    link: &str,
    is_pattern: bool,
) -> Option<td_api::BackgroundType> {
    let parsed_link = parse_internal_link(env, link, false)?;
    let background_name = match parsed_link.get_internal_link_type_object(env) {
        td_api::InternalLinkType::internalLinkTypeBackground(background) => {
            background.background_name
        }
        _ => return None,
    };
    if !BackgroundType::is_background_name_local(&background_name) {
        // `BackgroundType type(false, is_pattern, nullptr)` — the wire
        // constructor with no settings: a plain Wallpaper/Pattern.
        let mut background_type = if is_pattern {
            BackgroundType::new_pattern(false, BackgroundFill::default(), 0)
        } else {
            BackgroundType::new_wallpaper(false, false, 0)
        };
        background_type.apply_parameters_from_link(&background_name);
        return Some(background_type.get_background_type_object());
    }
    let Ok(background_type) = BackgroundType::get_local_background_type(&background_name) else {
        return None;
    };
    Some(background_type.get_background_type_object())
}

/// The `td::Proxy` value-plane subset (net/Proxy.{h,cpp}) the link plane
/// needs: `create_proxy` validation plus the accessors `get_proxy_link`
/// reads. The net half (the client id, the use_proxy machinery) stays with
/// the net track.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NetProxy {
    r#type: NetProxyType,
    server: String,
    port: i32,
    user: String,
    password: String,
    path: String,
    secret: tdmtproto::proxy_secret::ProxySecret,
}

/// `Proxy::Type` (net/Proxy.h).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NetProxyType {
    Socks5,
    HttpTcp,
    HttpCaching,
    Mtproto,
    Web,
}

impl NetProxy {
    fn new_socks5(server: String, port: i32, user: String, password: String) -> NetProxy {
        NetProxy {
            r#type: NetProxyType::Socks5,
            server,
            port,
            user,
            password,
            path: String::new(),
            secret: tdmtproto::proxy_secret::ProxySecret::default(),
        }
    }

    fn new_http(
        server: String,
        port: i32,
        user: String,
        password: String,
        http_only: bool,
    ) -> NetProxy {
        NetProxy {
            r#type: if http_only {
                NetProxyType::HttpCaching
            } else {
                NetProxyType::HttpTcp
            },
            server,
            port,
            user,
            password,
            path: String::new(),
            secret: tdmtproto::proxy_secret::ProxySecret::default(),
        }
    }

    fn new_mtproto(
        server: String,
        port: i32,
        secret: tdmtproto::proxy_secret::ProxySecret,
    ) -> NetProxy {
        NetProxy {
            r#type: NetProxyType::Mtproto,
            server,
            port,
            user: String::new(),
            password: String::new(),
            path: String::new(),
            secret,
        }
    }

    /// `Proxy::web` (net/Proxy.h:89-97) — the port is forced to 443.
    fn new_web(
        server: String,
        path: String,
        secret: tdmtproto::proxy_secret::ProxySecret,
    ) -> NetProxy {
        NetProxy {
            r#type: NetProxyType::Web,
            server,
            port: crate::connection_creator::Proxy::WEB_PROXY_PORT,
            user: String::new(),
            password: String::new(),
            path,
            secret,
        }
    }

    /// `Proxy::create_proxy(const td_api::proxy *)`
    /// (net/Proxy.cpp:56-60, 15-54). The web arm shares the
    /// connection-creator statics (the C++ single `Proxy::create_proxy`
    /// ladder — the link plane reads only the accessors).
    fn create_proxy(proxy: &td_api::Proxy) -> std::result::Result<NetProxy, Status> {
        use crate::connection_creator::Proxy as NetProxyHelpers;
        let Some(proxy_type) = proxy.r#type.as_ref() else {
            return Err(Status::error(400, "Proxy type must be non-empty"));
        };
        if let td_api::ProxyType::proxyTypeWeb(type_web) = proxy_type {
            if proxy.port != 0 && proxy.port != NetProxyHelpers::WEB_PROXY_PORT {
                return Err(Status::error(400, "Web proxy port must be 443"));
            }
            let host = NetProxyHelpers::normalize_web_proxy_host(&proxy.server)
                .map_err(|message| Status::error(400, &message))?;
            if !NetProxyHelpers::is_valid_web_proxy_path(&type_web.path) {
                return Err(Status::error(400, "Invalid web proxy path"));
            }
            let secret = NetProxyHelpers::get_web_proxy_secret(&type_web.secret)
                .map_err(|message| Status::error(400, &message))?;
            return Ok(NetProxy::new_web(host, type_web.path.clone(), secret));
        }
        if proxy.server.is_empty() {
            return Err(Status::error(400, "Server name must be non-empty"));
        }
        if proxy.server.len() > 255 {
            return Err(Status::error(400, "Server name is too long"));
        }
        if !tdutils::utf8::check_utf8(proxy.server.as_bytes()) {
            return Err(Status::error(400, "Server name must be encoded in UTF-8"));
        }
        if proxy.port <= 0 || proxy.port > 65535 {
            return Err(Status::error(400, "Wrong port number"));
        }

        match proxy_type {
            td_api::ProxyType::proxyTypeSocks5(type_socks) => Ok(NetProxy::new_socks5(
                proxy.server.clone(),
                proxy.port,
                type_socks.username.clone(),
                type_socks.password.clone(),
            )),
            td_api::ProxyType::proxyTypeHttp(type_http) => Ok(NetProxy::new_http(
                proxy.server.clone(),
                proxy.port,
                type_http.username.clone(),
                type_http.password.clone(),
                type_http.http_only,
            )),
            td_api::ProxyType::proxyTypeMtproto(type_mtproto) => {
                let secret = match tdmtproto::proxy_secret::ProxySecret::from_link(
                    type_mtproto.secret.as_bytes(),
                    false,
                ) {
                    tdutils::status::Result::Ok(secret) => secret,
                    tdutils::status::Result::Err(_) => {
                        return Err(Status::error(400, "Wrong proxy secret"));
                    }
                };
                Ok(NetProxy::new_mtproto(
                    proxy.server.clone(),
                    proxy.port,
                    secret,
                ))
            }
            // unreachable like the C++ switch's UNREACHABLE default — the
            // web arm is handled before the validation ladder
            td_api::ProxyType::proxyTypeWeb(_) => {
                unreachable!("the web arm is handled before the validation ladder")
            }
        }
    }

    /// The encoded secret in its string form.
    fn get_encoded_secret(&self) -> String {
        String::from_utf8_lossy(&self.secret.get_encoded_secret()).into_owned()
    }

    /// `Proxy::get_web_proxy_address` (net/Proxy.cpp:144-151) — the
    /// "host[/path]" form the link carries in the server parameter.
    fn get_web_proxy_address(&self) -> String {
        debug_assert!(
            self.r#type == NetProxyType::Web,
            "CHECK(type_ == Type::Web)"
        );
        if self.path.is_empty() {
            return self.server.clone();
        }
        format!("{}/{}", self.server, self.path)
    }

    /// `Proxy::get_web_proxy_link_secret` (net/Proxy.cpp:147-153) — a
    /// path-carrying proxy marks its link secret with the 0x70 byte so
    /// path-unaware clients reject the link.
    fn get_web_proxy_link_secret(&self) -> String {
        debug_assert!(
            self.r#type == NetProxyType::Web,
            "CHECK(type_ == Type::Web)"
        );
        const WEB_PROXY_LINK_SECRET_MARKER: u8 = 0x70;
        if self.path.is_empty() {
            return String::from_utf8_lossy(&self.secret.get_encoded_secret()).into_owned();
        }
        let mut marked = vec![WEB_PROXY_LINK_SECRET_MARKER];
        marked.extend_from_slice(self.secret.get_raw_secret());
        String::from_utf8(tdmtproto::utils::base64url_encode(&marked))
            .expect("base64url output is ASCII")
    }
}

/// `LinkManager::get_proxy_link` (cpp:4350-4383).
fn get_proxy_link(
    env: &impl LinkEnv,
    proxy: &NetProxy,
    is_internal: bool,
) -> std::result::Result<String, Status> {
    let mut url = if is_internal {
        "tg://".to_owned()
    } else {
        get_t_me_url(env)
    };
    let mut is_socks = false;
    match proxy.r#type {
        NetProxyType::Socks5 => {
            url += "socks";
            is_socks = true;
        }
        NetProxyType::HttpTcp | NetProxyType::HttpCaching => {
            return Err(Status::error(400, "HTTP proxies have no public links"));
        }
        NetProxyType::Mtproto => {
            url += "proxy";
        }
        NetProxyType::Web => {
            // the port is always 443, and the path travels in the server
            // parameter (LinkManager.cpp:4908-4912)
            return Ok(format!(
                "{url}webproxy?server={}&secret={}",
                url_encode(&proxy.get_web_proxy_address()),
                url_encode(&proxy.get_web_proxy_link_secret())
            ));
        }
    }
    url += "?server=";
    url += &url_encode(&proxy.server);
    url += "&port=";
    url += &proxy.port.to_string();
    if is_socks {
        if !proxy.user.is_empty() || !proxy.password.is_empty() {
            url += "&user=";
            url += &url_encode(&proxy.user);
            url += "&pass=";
            url += &url_encode(&proxy.password);
        }
    } else {
        url += "&secret=";
        url += &proxy.get_encoded_secret();
    }
    Ok(url)
}

/// `DialogBoostLinkInfo` (DialogBoostLinkInfo.h).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DialogBoostLinkInfo {
    pub username: String,
    pub channel_id: crate::chat_manager::ChannelId,
}

/// `LinkManager::get_dialog_boost_link_info` (cpp:4509-4606).
pub fn get_dialog_boost_link_info(
    env: &impl LinkEnv,
    url: &str,
) -> std::result::Result<DialogBoostLinkInfo, Status> {
    if url.is_empty() {
        return Err(Status::error_message("URL must be non-empty"));
    }
    let link_info = get_link_info(env, url);
    if link_info.link_type != LinkType::Tg && link_info.link_type != LinkType::TMe {
        return Err(Status::error_message("Invalid chat boost link URL"));
    }
    let url = link_info.query.clone();

    let mut username = "";
    let mut channel_id_slice = "";
    if link_info.link_type == LinkType::Tg {
        // boost?domain=username / boost?channel=123456789

        let Some(rest) = url.strip_prefix("boost") else {
            return Err(Status::error_message("Wrong chat boost link URL"));
        };
        let rest = rest.strip_prefix('/').unwrap_or(rest);
        let Some(rest) = rest.strip_prefix('?') else {
            return Err(Status::error_message("Wrong chat boost link URL"));
        };

        for arg in rest.split('&') {
            let (key, value) = arg.split_once('=').unwrap_or((arg, ""));
            if key == "domain" {
                username = value;
            } else if key == "channel" {
                channel_id_slice = value;
            }
        }
        build_boost_link_info(username, channel_id_slice)
    } else {
        // /username?boost / /c/123456789?boost

        let rest = url.strip_prefix('/').expect("TMe query starts with '/'");

        let username_end_pos = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let mut username = &rest[..username_end_pos];
        let mut rest = &rest[username_end_pos..];
        if let Some(stripped) = rest.strip_prefix('/') {
            rest = stripped;
        }
        if username == "c" {
            username = "";
            let channel_id_end_pos = rest.find(['/', '?', '#']).unwrap_or(rest.len());
            channel_id_slice = &rest[..channel_id_end_pos];
            rest = &rest[channel_id_end_pos..];
        }

        let mut is_boost = false;
        if let Some(query_pos) = rest.find('?') {
            for arg in rest[query_pos + 1..].split('&') {
                let (key, _) = arg.split_once('=').unwrap_or((arg, ""));
                if key == "boost" {
                    is_boost = true;
                }
            }
        }

        if !is_boost {
            return Err(Status::error_message("Wrong chat boost link URL"));
        }

        build_boost_link_info(username, channel_id_slice)
    }
}

/// The shared tail of `get_dialog_boost_link_info` (cpp:4592-4605).
fn build_boost_link_info(
    username: &str,
    channel_id_slice: &str,
) -> std::result::Result<DialogBoostLinkInfo, Status> {
    let mut channel_id = crate::chat_manager::ChannelId::default();
    if username.is_empty() {
        let Some(parsed) = crate::config_manager::to_integer_safe_i64(channel_id_slice) else {
            return Err(Status::error_message("Wrong channel ID"));
        };
        channel_id = crate::chat_manager::ChannelId::new(parsed);
        if !channel_id.is_valid() {
            return Err(Status::error_message("Wrong channel ID"));
        }
    }

    Ok(DialogBoostLinkInfo {
        username: username.to_owned(),
        channel_id,
    })
}

/// `MessageLinkInfo` (MessageLinkInfo.h). `comment_dialog_id` is only ever
/// set by the MessagesManager consumer; the parser leaves it 0 like the C++.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageLinkInfo {
    pub username: String,
    pub channel_id: crate::chat_manager::ChannelId,
    pub message_id: crate::chat_manager::MessageId,
    pub is_single: bool,
    pub media_timestamp: i32,
    pub todo_item_id: i32,
    pub poll_option_id: String,
    pub top_thread_message_id: crate::chat_manager::MessageId,
    pub comment_dialog_id: crate::dialog_id::DialogId,
    pub comment_message_id: crate::chat_manager::MessageId,
    pub for_comment: bool,
}

/// The `media_timestamp_slice` parser (cpp:4775-4810): digits with
/// `h`/`m`/`s` unit suffixes, capped at 10000000.
fn parse_media_timestamp(media_timestamp_slice: &str) -> (i32, bool) {
    let bytes = media_timestamp_slice.as_bytes();
    let max_media_timestamp: i64 = 10000000;
    let mut media_timestamp: i64 = 0;
    let mut is_media_timestamp_invalid = false;
    let mut current_value: i64 = 0;
    for i in 0..=bytes.len() {
        let c = if i < bytes.len() { bytes[i] } else { b's' };
        if c.is_ascii_digit() {
            current_value = current_value * 10 + (c - b'0') as i64;
            if current_value > max_media_timestamp {
                is_media_timestamp_invalid = true;
                break;
            }
        } else {
            let mul: i64 = match c.to_ascii_lowercase() {
                b'h' => 3600,
                b'm' => 60,
                b's' => 1,
                _ => 0,
            };
            if mul == 0
                || current_value > max_media_timestamp / mul
                || media_timestamp + current_value * mul > max_media_timestamp
            {
                is_media_timestamp_invalid = true;
                break;
            }
            media_timestamp += current_value * mul;
            current_value = 0;
        }
    }
    (media_timestamp as i32, is_media_timestamp_invalid)
}

/// The `get_message_link_info` argument scan (cpp:4618-4741).
#[derive(Default)]
struct MessageLinkArgs<'a> {
    username: &'a str,
    channel_id_slice: &'a str,
    message_id_slice: &'a str,
    comment_message_id_slice: &'a str,
    top_thread_message_id_slice: &'a str,
    media_timestamp_slice: &'a str,
    todo_item_id_slice: &'a str,
    poll_option_id_slice: &'a str,
    is_single: bool,
    for_comment: bool,
}

/// `LinkManager::get_message_link_info` (cpp:4608-4843).
pub fn get_message_link_info(
    env: &impl LinkEnv,
    url: &str,
) -> std::result::Result<MessageLinkInfo, Status> {
    if url.is_empty() {
        return Err(Status::error_message("URL must be non-empty"));
    }
    let link_info = get_link_info(env, url);
    if link_info.link_type != LinkType::Tg && link_info.link_type != LinkType::TMe {
        return Err(Status::error_message("Invalid message link URL"));
    }
    let url = link_info.query.clone();

    let mut args = MessageLinkArgs {
        comment_message_id_slice: "0",
        ..MessageLinkArgs::default()
    };
    if link_info.link_type == LinkType::Tg {
        // resolve?domain=username&post=12345&single&t=123&comment=12&thread=21&task=23&option=MA
        // privatepost?channel=123456789&post=12345&...

        let is_resolve;
        let rest;
        if let Some(stripped) = url.strip_prefix("resolve") {
            is_resolve = true;
            rest = stripped;
        } else if let Some(stripped) = url.strip_prefix("privatepost") {
            is_resolve = false;
            rest = stripped;
        } else {
            return Err(Status::error_message("Wrong message link URL"));
        }

        let rest = rest.strip_prefix('/').unwrap_or(rest);
        let Some(rest) = rest.strip_prefix('?') else {
            return Err(Status::error_message("Wrong message link URL"));
        };

        for arg in rest.split('&') {
            let (key, value) = arg.split_once('=').unwrap_or((arg, ""));
            if is_resolve && key == "domain" {
                args.username = value;
            }
            if !is_resolve && key == "channel" {
                args.channel_id_slice = value;
            }
            if key == "post" {
                args.message_id_slice = value;
            }
            if key == "t" {
                args.media_timestamp_slice = value;
            }
            if key == "single" {
                args.is_single = true;
            }
            if key == "comment" {
                args.comment_message_id_slice = value;
            }
            if key == "thread" {
                args.for_comment = true;
                args.top_thread_message_id_slice = value;
            }
            if key == "task" {
                args.todo_item_id_slice = value;
            }
            if key == "option" {
                args.poll_option_id_slice = value;
            }
        }
    } else {
        // /c/123456789/12345 / /c/123456789/1234/12345 / /username/12345?single

        let rest = url.strip_prefix('/').expect("TMe query starts with '/'");

        let Some(username_end_pos) = rest.find('/') else {
            return Err(Status::error_message("Wrong message link URL"));
        };
        let mut username = &rest[..username_end_pos];
        let mut rest = &rest[username_end_pos + 1..];
        let mut channel_id_slice = "";
        if username == "c" {
            username = "";
            let Some(channel_id_end_pos) = rest.find('/') else {
                return Err(Status::error_message("Wrong message link URL"));
            };
            channel_id_slice = &rest[..channel_id_end_pos];
            rest = &rest[channel_id_end_pos + 1..];
        }

        let query_pos = rest.find('?');
        let mut message_id_slice = match query_pos {
            Some(pos) => &rest[..pos],
            None => rest,
        };
        if let Some(query_pos) = query_pos {
            for arg in rest[query_pos + 1..].split('&') {
                let (key, value) = arg.split_once('=').unwrap_or((arg, ""));
                if key == "t" {
                    args.media_timestamp_slice = value;
                }
                if key == "single" {
                    args.is_single = true;
                }
                if key == "comment" {
                    args.comment_message_id_slice = value;
                }
                if key == "thread" {
                    args.for_comment = true;
                    args.top_thread_message_id_slice = value;
                }
                if key == "task" {
                    args.todo_item_id_slice = value;
                }
                if key == "option" {
                    args.poll_option_id_slice = value;
                }
            }
        }
        if let Some(slash_pos) = message_id_slice.find('/') {
            args.top_thread_message_id_slice = &message_id_slice[..slash_pos];
            message_id_slice = &message_id_slice[slash_pos + 1..];
        }
        args.username = username;
        args.channel_id_slice = channel_id_slice;
        args.message_id_slice = message_id_slice;
    }

    let mut channel_id = crate::chat_manager::ChannelId::default();
    if args.username.is_empty() {
        let Some(parsed) = crate::config_manager::to_integer_safe_i64(args.channel_id_slice) else {
            return Err(Status::error_message("Wrong channel ID"));
        };
        channel_id = crate::chat_manager::ChannelId::new(parsed);
        if !channel_id.is_valid() {
            return Err(Status::error_message("Wrong channel ID"));
        }
    }

    let Some(message_id) = to_integer_safe_i32(args.message_id_slice) else {
        return Err(Status::error_message("Wrong message identifier"));
    };
    if !crate::dialog_id::ServerMessageId::new(message_id).is_valid() {
        return Err(Status::error_message("Wrong message identifier"));
    }

    let mut top_thread_message_id = 0;
    if !args.top_thread_message_id_slice.is_empty() {
        let Some(parsed) = to_integer_safe_i32(args.top_thread_message_id_slice) else {
            return Err(Status::error_message("Wrong message topic identifier"));
        };
        top_thread_message_id = parsed;
        if !crate::dialog_id::ServerMessageId::new(top_thread_message_id).is_valid() {
            return Err(Status::error_message("Invalid message topic identifier"));
        }
    }

    let Some(comment_message_id) = to_integer_safe_i32(args.comment_message_id_slice) else {
        return Err(Status::error_message("Wrong comment message identifier"));
    };
    if comment_message_id != 0
        && !crate::dialog_id::ServerMessageId::new(comment_message_id).is_valid()
    {
        return Err(Status::error_message("Wrong comment message identifier"));
    }

    let (media_timestamp, is_media_timestamp_invalid) =
        parse_media_timestamp(args.media_timestamp_slice);

    let mut todo_item_id = 0;
    if !args.todo_item_id_slice.is_empty() {
        let Some(parsed) = to_integer_safe_i32(args.todo_item_id_slice) else {
            return Err(Status::error_message("Wrong checklist task identifier"));
        };
        if parsed <= 0 {
            return Err(Status::error_message("Wrong checklist task identifier"));
        }
        todo_item_id = parsed;
    }

    let poll_option_id =
        match tdmtproto::utils::base64url_decode(args.poll_option_id_slice.as_bytes()) {
            tdutils::status::Result::Ok(mut decoded) => {
                if !tdutils::utf8::check_utf8(&decoded) {
                    decoded.clear();
                }
                String::from_utf8_lossy(&decoded).into_owned()
            }
            tdutils::status::Result::Err(_) => {
                return Err(Status::error_message("Invalid poll option identifier"));
            }
        };

    Ok(MessageLinkInfo {
        username: args.username.to_owned(),
        channel_id,
        message_id: crate::chat_manager::MessageId::from_server_message_id(
            crate::dialog_id::ServerMessageId::new(message_id),
        ),
        comment_message_id: crate::chat_manager::MessageId::from_server_message_id(
            crate::dialog_id::ServerMessageId::new(comment_message_id),
        ),
        top_thread_message_id: crate::chat_manager::MessageId::from_server_message_id(
            crate::dialog_id::ServerMessageId::new(top_thread_message_id),
        ),
        media_timestamp: if is_media_timestamp_invalid {
            0
        } else {
            media_timestamp
        },
        todo_item_id,
        poll_option_id,
        is_single: args.is_single,
        for_comment: args.for_comment,
        comment_dialog_id: crate::dialog_id::DialogId::default(),
    })
}

/// The LinkManager in-flight query kinds — the C++ ResultHandler classes
/// of the net-drain 32 families (LinkManager.cpp:1508-1780). The handler
/// state the answers read (`url_`/`dialog_id_`/the promise) rides the
/// [`PendingLinkQueries`] lots instead — the kinds stay `Copy`.
#[derive(Clone, Copy)]
pub(crate) enum LinkNetKind {
    /// `GetRecentMeUrlsQuery` (cpp:1508) — `help.getRecentMeUrls`.
    RecentMeUrls,
    /// `GetDeepLinkInfoQuery` (cpp:1607) — `help.getDeepLinkInfo`, the
    /// unauth wire form.
    DeepLinkInfo,
    /// `RequestUrlAuthQuery` (cpp:1646) — `messages.requestUrlAuth` in
    /// the peer/button or the bare-url flag form.
    RequestUrlAuth,
    /// `RequestUrlOauthQuery` (cpp:1722) — the oauth form with the
    /// in-app origin.
    RequestUrlOauth,
}

/// The parked query promises — the ResultHandler members the `Copy`
/// [`LinkNetKind`]s can't carry (`RequestUrlAuthQuery`'s `url_`/
/// `dialog_id_`, `RequestUrlOauthQuery`'s `url_`, and every `promise_`).
/// FIFO-paired per family (§5).
#[derive(Default)]
pub(crate) struct PendingLinkQueries {
    /// `GetRecentMeUrlsQuery::promise_`.
    pub(crate) recent_me_urls: Vec<tdutils::promise::Promise<Option<td_api::TMeUrls>>>,
    /// `GetDeepLinkInfoQuery::promise_` (the null answer rides `None`).
    pub(crate) deep_link_info: Vec<tdutils::promise::Promise<Option<td_api::DeepLinkInfo>>>,
    /// `RequestUrlAuthQuery`'s `url_`/`dialog_id_`/`promise_`.
    pub(crate) url_auth: Vec<(
        String,
        Option<crate::dialog_id::DialogId>,
        tdutils::promise::Promise<td_api::LoginUrlInfo>,
    )>,
    /// `RequestUrlOauthQuery`'s `url_`/`promise_`.
    pub(crate) url_oauth: Vec<(String, tdutils::promise::Promise<td_api::OauthLinkInfo>)>,
}

/// The autologin-token state (LinkManager.h:227-231).
#[derive(Default)]
pub(crate) struct LinkManager {
    autologin_token: String,
    autologin_domains: Vec<String>,
    autologin_update_time: f64,
    url_auth_domains: Vec<String>,
    whitelisted_domains: Vec<String>,
    /// The parked query promises of the net-drain 32 families.
    pub(crate) pending: PendingLinkQueries,
    /// The net dispatch surface — the `td_` back-pointer the C++ manager
    /// uses for its query sends, narrowed to the net plane (`None` in
    /// the net-inert tests; set at the manager install, §0's shared-env
    /// shape).
    pub(crate) net: Option<crate::td::LinkNetHandle>,
}

impl LinkManager {
    /// The constructor plus the `start_up` timestamp line
    /// (LinkManager.cpp:1895-1901): the token starts empty and its
    /// update time a year in the past. The pmc domain-list restore runs in
    /// [`LinkManager::start_up`].
    pub(crate) fn new() -> Self {
        Self {
            autologin_update_time: tdutils::time::now() - 365.0 * 86400.0,
            ..Self::default()
        }
    }

    /// `LinkManager::start_up`'s domain-list restore (cpp:1900-1908) — the
    /// three pmc lists join the token/timestamp init.
    pub(crate) fn start_up(&mut self, env: &mut impl LinkEnv) {
        let split_pmc =
            |raw: String| -> Vec<String> { raw.split('\u{FF}').map(str::to_owned).collect() };
        self.autologin_domains = split_pmc(env.binlog_pmc_get("autologin_domains"));
        self.url_auth_domains = split_pmc(env.binlog_pmc_get("url_auth_domains"));
        self.whitelisted_domains = split_pmc(env.binlog_pmc_get("whitelisted_domains"));
    }

    /// `LinkManager::update_autologin_domains` (cpp:4005-4019).
    pub(crate) fn update_autologin_domains(
        &mut self,
        env: &mut impl LinkEnv,
        autologin_domains: Vec<String>,
        url_auth_domains: Vec<String>,
        whitelisted_domains: Vec<String>,
    ) {
        if self.autologin_domains != autologin_domains {
            env.binlog_pmc_set("autologin_domains", &autologin_domains.join("\u{FF}"));
            self.autologin_domains = autologin_domains;
        }
        if self.url_auth_domains != url_auth_domains {
            env.binlog_pmc_set("url_auth_domains", &url_auth_domains.join("\u{FF}"));
            self.url_auth_domains = url_auth_domains;
        }
        if self.whitelisted_domains != whitelisted_domains {
            env.binlog_pmc_set("whitelisted_domains", &whitelisted_domains.join("\u{FF}"));
            self.whitelisted_domains = whitelisted_domains;
        }
    }

    /// The `td_->create_handler<…>()->send(...)` collapse — the wire
    /// query construction lives at the manager send sites; `None` keeps
    /// the net-inert posture (the tests drive the answer halves).
    fn send_link_net_query(
        &self,
        kind: LinkNetKind,
        function: tdmtproto::telegram_api::Function,
        what: &str,
    ) {
        match &self.net {
            Some(net) => net.dispatch(kind, function),
            None => tdutils::log_info!("Send {what} — [net-inert]"),
        }
    }

    /// The `create_unauth` form — `GetDeepLinkInfoQuery::send`
    /// (cpp:1618-1620) serializes without an auth context.
    fn send_link_net_query_unauth(
        &self,
        kind: LinkNetKind,
        function: tdmtproto::telegram_api::Function,
        what: &str,
    ) {
        match &self.net {
            Some(net) => net.dispatch_unauth(kind, function),
            None => tdutils::log_info!("Send {what} — [net-inert]"),
        }
    }

    /// `LinkManager::get_recent_me_urls` (cpp:4021-4023) — the
    /// `GetRecentMeUrlsQuery::send` half (`help.getRecentMeUrls`); the
    /// promise parks in the family lot and the answer leg runs the
    /// users/chats intakes + the recentMeUrl ladder (cpp:1533-1601).
    pub(crate) fn get_recent_me_urls(
        &mut self,
        referrer: String,
        promise: tdutils::promise::Promise<Option<td_api::TMeUrls>>,
    ) {
        self.pending.recent_me_urls.push(promise);
        self.send_link_net_query(
            LinkNetKind::RecentMeUrls,
            tdmtproto::telegram_api::Function::help_getRecentMeUrls(
                tdmtproto::telegram_api::help_getRecentMeUrls { referer: referrer },
            ),
            "help.getRecentMeUrls",
        );
    }

    /// `LinkManager::get_deep_link_info` (cpp:4025-4039): the local half
    /// strips the `tg:` scheme (case-sensitive `begins_with`) and truncates
    /// the host at the first `/`, `?` or `#`, then the
    /// `GetDeepLinkInfoQuery::send` half (`help.getDeepLinkInfo`, the
    /// unauth wire form) parks the promise in the family lot.
    pub(crate) fn get_deep_link_info(
        &mut self,
        link: &str,
        promise: tdutils::promise::Promise<Option<td_api::DeepLinkInfo>>,
    ) {
        let mut link = link;
        if let Some(rest) = link.strip_prefix("tg:") {
            link = rest;
            if let Some(rest) = link.strip_prefix("//") {
                link = rest;
            }
        }
        let mut pos = 0;
        while pos < link.len() && !matches!(link.as_bytes()[pos], b'/' | b'?' | b'#') {
            pos += 1;
        }
        let link = &link[..pos];
        self.pending.deep_link_info.push(promise);
        self.send_link_net_query_unauth(
            LinkNetKind::DeepLinkInfo,
            tdmtproto::telegram_api::Function::help_getDeepLinkInfo(
                tdmtproto::telegram_api::help_getDeepLinkInfo {
                    path: link.to_owned(),
                },
            ),
            "help.getDeepLinkInfo",
        );
    }

    /// `RequestUrlAuthQuery::send` (cpp:1652-1670) — the peer/button form
    /// when the message dialog is valid (the read input peer resolves at
    /// the send leg; the C++ `CHECK(input_peer != nullptr)` is fatal and
    /// the port downgrades it to a non-fatal log-and-drop), the bare-url
    /// form otherwise. The `url_`/`dialog_id_`/promise trio parks in the
    /// family lot for the answer legs (cpp:1672-1719).
    fn send_request_url_auth_query(
        &mut self,
        env: &mut impl LinkEnv,
        url: String,
        message_full_id: MessageFullId,
        button_id: i32,
        promise: tdutils::promise::Promise<td_api::LoginUrlInfo>,
    ) {
        // messages_requestUrlAuth flag bits (telegram_api.tl:2699).
        const PEER_MASK: i32 = 1 << 1;
        const URL_MASK: i32 = 1 << 2;
        let dialog_id = message_full_id.get_dialog_id();
        let (flags, peer, msg_id, wire_button_id, error_dialog_id) = if dialog_id.is_valid() {
            let Some(peer) = env.get_input_peer_read(dialog_id) else {
                // the C++ CHECK(input_peer != nullptr) is fatal; the
                // port downgrades it to log-and-drop — the query never
                // sends and the promise stays unanswered
                tdutils::log_error!("RequestUrlAuthQuery — no input peer for {dialog_id:?}");
                return;
            };
            (
                PEER_MASK,
                Some(Box::new(peer)),
                message_full_id
                    .get_message_id()
                    .get_server_message_id()
                    .get(),
                button_id,
                Some(dialog_id),
            )
        } else {
            (URL_MASK, None, 0, 0, None)
        };
        self.pending
            .url_auth
            .push((url.clone(), error_dialog_id, promise));
        self.send_link_net_query(
            LinkNetKind::RequestUrlAuth,
            tdmtproto::telegram_api::Function::messages_requestUrlAuth(
                tdmtproto::telegram_api::messages_requestUrlAuth {
                    flags,
                    peer,
                    msg_id,
                    button_id: wire_button_id,
                    url,
                    in_app_origin: String::new(),
                },
            ),
            "messages.requestUrlAuth",
        );
    }

    /// `LinkManager::get_external_link_info` (cpp:4041-4129): the local
    /// machine — the tonsite/`.ton` proxy rewrite, the whitelisted
    /// skip-confirmation mark, the autologin-token append and the
    /// freshness-gated config reload. The `RequestUrlAuthQuery` tail
    /// (cpp:4086) rides the net plane (the URL-flag form); the
    /// `ConfigManager::reload_config` closure (cpp:4092-4100) stays the
    /// `[blocked: net]` env seam, and its offline default reports the
    /// config as not reloaded, so the promise answers the default open
    /// result exactly like the C++ error path.
    pub(crate) fn get_external_link_info(
        &mut self,
        env: &mut impl LinkEnv,
        mut link: String,
        mut promise: tdutils::promise::Promise<td_api::LoginUrlInfo>,
    ) {
        let default_result = |url: String, skip_confirmation: bool| {
            td_api::LoginUrlInfo::loginUrlInfoOpen(td_api::loginUrlInfoOpen {
                url,
                skip_confirmation,
            })
        };

        let mut is_ton = false;
        if let Some(rest) = link.strip_prefix("tonsite://") {
            link = rest.to_owned();
            is_ton = true;
        }
        if env.close_flag() {
            promise.set_value(default_result(link, false));
            return;
        }

        let url = match parse_url(&link) {
            tdutils::status::Result::Ok(url) => url,
            tdutils::status::Result::Err(_) => {
                promise.set_value(default_result(link, false));
                return;
            }
        };
        if !url.userinfo.is_empty() || url.is_ipv6 {
            promise.set_value(default_result(link, false));
            return;
        }

        // The ton/`.ton` host rewrite through the ton proxy address
        // (cpp:4061-4079). The C++ mutates `url` and re-points
        // `default_result->url_` at the rewritten form; the port keeps the
        // original link for the same observable outcomes.
        let mut url = url;
        let mut rewritten_default_url = link.clone();
        let host_ends_in_ton =
            url.host.len() >= 4 && url.host[url.host.len() - 4..].to_lowercase() == ".ton";
        if is_ton || host_ends_in_ton {
            let ton_proxy_address = env.get_option_string("ton_proxy_address");
            if ton_proxy_address.is_empty() {
                promise.set_value(default_result(link, false));
                return;
            }
            url.protocol = Protocol::Https;
            let mut new_host = String::new();
            for c in url.host.chars() {
                match c {
                    '.' => new_host += "-d",
                    '-' => new_host += "-h",
                    _ => new_host.push(c),
                }
            }
            url.host = format!("{new_host}.{ton_proxy_address}");
            rewritten_default_url = url.get_url();
        }

        let skip_confirmation = self.whitelisted_domains.contains(&url.host);

        if !self.autologin_domains.contains(&url.host) {
            if self.url_auth_domains.contains(&url.host) {
                self.send_request_url_auth_query(env, link, MessageFullId::default(), 0, promise);
                return;
            }
            promise.set_value(default_result(rewritten_default_url, skip_confirmation));
            return;
        }

        if self.autologin_update_time < tdutils::time::now() - 10000.0 {
            // The reload_config + retry closure (cpp:4092-4100): offline the
            // reload seam reports failure and the C++ error path answers the
            // default result.
            if env.reload_config() {
                self.get_external_link_info(env, link, promise);
            } else {
                promise.set_value(default_result(rewritten_default_url, skip_confirmation));
            }
            return;
        }

        if self.autologin_token.is_empty() {
            promise.set_value(default_result(rewritten_default_url, skip_confirmation));
            return;
        }

        url.protocol = Protocol::Https;
        let path_end = url.query.find(['?', '#']).unwrap_or(url.query.len());
        let path = &url.query[..path_end];
        let parameters_hash = &url.query[path_end..];
        let hash_pos = parameters_hash.find('#').unwrap_or(parameters_hash.len());
        let parameters = &parameters_hash[..hash_pos];
        let hash = &parameters_hash[hash_pos..];

        let mut added_parameter = if parameters.is_empty() {
            '?'.to_string()
        } else if parameters.len() == 1 {
            debug_assert!(parameters == "?");
            String::new()
        } else {
            '&'.to_string()
        };
        added_parameter += "autologin_token=";
        added_parameter += &self.autologin_token;

        url.query = format!("{path}{parameters}{added_parameter}{hash}");
        promise.set_value(td_api::LoginUrlInfo::loginUrlInfoOpen(
            td_api::loginUrlInfoOpen {
                url: url.get_url(),
                skip_confirmation,
            },
        ));
    }

    /// `LinkManager::get_login_url_info` (cpp:4131-4136): the message's
    /// login-button URL resolves through the message plane (the
    /// [`LinkEnv::get_login_button_url`] `[blocked: M.10 integration]`
    /// seam), then the `RequestUrlAuthQuery::send` peer form
    /// (cpp:1646-1670) rides the net plane.
    pub(crate) fn get_login_url_info(
        &mut self,
        env: &mut impl LinkEnv,
        message_full_id: MessageFullId,
        button_id: i64,
        mut promise: tdutils::promise::Promise<td_api::LoginUrlInfo>,
    ) {
        match env.get_login_button_url(message_full_id, button_id) {
            tdutils::status::Result::Ok(url) => self.send_request_url_auth_query(
                env,
                url,
                message_full_id,
                button_id as i32,
                promise,
            ),
            tdutils::status::Result::Err(status) => promise.set_error(status),
        }
    }

    /// `LinkManager::get_login_url` (cpp:4138-4143): the same
    /// button-URL resolution, then the `AcceptUrlAuthQuery` tail
    /// (cpp:1810) is `[blocked: net]`.
    pub(crate) fn get_login_url(
        &mut self,
        env: &mut impl LinkEnv,
        message_full_id: MessageFullId,
        button_id: i64,
        allow_write_access: bool,
        mut promise: tdutils::promise::Promise<String>,
    ) {
        match env.get_login_button_url(message_full_id, button_id) {
            tdutils::status::Result::Ok(url) => {
                env.send_accept_url_auth_query(
                    url,
                    message_full_id,
                    button_id as i32,
                    allow_write_access,
                    false,
                    String::new(),
                    None,
                    promise,
                );
            }
            tdutils::status::Result::Err(status) => promise.set_error(status),
        }
    }

    /// `LinkManager::get_link_login_url` (cpp:4145-4148): the pure
    /// external form of the acceptance query — `AcceptUrlAuthQuery`
    /// (cpp:1810) with no peer/button context, `[blocked: net]`.
    pub(crate) fn get_link_login_url(
        &mut self,
        env: &mut impl LinkEnv,
        url: String,
        allow_write_access: bool,
        promise: tdutils::promise::Promise<String>,
    ) {
        env.send_accept_url_auth_query(
            url,
            MessageFullId::default(),
            0,
            allow_write_access,
            false,
            String::new(),
            None,
            promise,
        );
    }

    /// `LinkManager::check_oauth_request_match_code` (cpp:4150-4152): the
    /// `CheckUrlAuthMatchCodeQuery` (`messages.checkUrlAuthMatchCode`) is
    /// `[blocked: net]`.
    pub(crate) fn check_oauth_request_match_code(
        &mut self,
        env: &mut impl LinkEnv,
        url: String,
        match_code: String,
        promise: tdutils::promise::Promise<()>,
    ) {
        env.send_check_url_auth_match_code_query(url, match_code, promise);
    }

    /// `LinkManager::accept_oauth_request` (cpp:4154-4158, upstream
    /// 2485f5889 at LinkManager.cpp:4682-4700): the oauth acceptance
    /// rides `AcceptUrlAuthQuery` (cpp:1810) with the match code and
    /// the phone-number flag, `[blocked: net]`. The TON Connect
    /// payload validates through `get_wallet_ownership_proof`
    /// (`TRY_RESULT_PROMISE`: the 400 band answers the promise and
    /// skips the send) and rides the query as the wire session — only
    /// when the payload is set.
    #[allow(clippy::too_many_arguments)] // the C++ handler has the same shape
    pub(crate) fn accept_oauth_request(
        &mut self,
        env: &mut impl LinkEnv,
        url: String,
        match_code: String,
        allow_write_access: bool,
        allow_phone_number_access: bool,
        connect_data: Option<td_api::TonConnectConnectData>,
        mut promise: tdutils::promise::Promise<String>,
    ) {
        let mut tonconnect_session = None;
        if let Some(connect_data) = connect_data {
            let (_, proof) = match crate::ton_wallet::get_wallet_ownership_proof(connect_data.proof)
            {
                tdutils::status::Result::Ok(parsed) => parsed,
                tdutils::status::Result::Err(status) => return promise.set_error(status),
            };
            tonconnect_session = Some(tdmtproto::telegram_api::InputTonConnectOauthSession {
                session_id: connect_data.session_id,
                challenge_answer: connect_data.challenge_answer,
                body: connect_data.body,
                proof: Some(proof),
            });
        }
        env.send_accept_url_auth_query(
            url,
            MessageFullId::default(),
            0,
            allow_write_access,
            allow_phone_number_access,
            match_code,
            tonconnect_session,
            promise,
        );
    }

    /// `LinkManager::decline_oauth_request` (cpp:4160-4162): the
    /// `DeclineUrlAuthQuery` (`messages.declineUrlAuth`) is
    /// `[blocked: net]`.
    pub(crate) fn decline_oauth_request(
        &mut self,
        env: &mut impl LinkEnv,
        url: String,
        promise: tdutils::promise::Promise<()>,
    ) {
        env.send_decline_url_auth_query(url, promise);
    }

    /// `LinkManager::get_oauth_link_info` (cpp:4164-4177): the local half
    /// parses the link and demands `internalLinkTypeOauth`, then the
    /// `RequestUrlOauthQuery::send` half (cpp:1722-1736) — the bare-url
    /// flag form with the in-app origin riding its own mask — parks the
    /// url/promise pair for the answer leg (cpp:1738-1780).
    pub(crate) fn get_oauth_link_info(
        &mut self,
        env: &mut impl LinkEnv,
        link: String,
        in_app_origin: String,
        mut promise: tdutils::promise::Promise<td_api::OauthLinkInfo>,
    ) {
        let Some(parsed) = parse_internal_link(env, &link, false) else {
            return promise.set_error(Status::error(400, "Invalid OAuth URL specified"));
        };
        let object = parsed.get_internal_link_type_object(env);
        let td_api::InternalLinkType::internalLinkTypeOauth(oauth) = object else {
            return promise.set_error(Status::error(400, "Invalid OAuth URL specified"));
        };
        // messages_requestUrlAuth flag bits (telegram_api.tl:2699).
        const URL_MASK: i32 = 1 << 2;
        const IN_APP_ORIGIN_MASK: i32 = 1 << 3;
        let mut flags = URL_MASK;
        if !in_app_origin.is_empty() {
            flags |= IN_APP_ORIGIN_MASK;
        }
        self.pending.url_oauth.push((oauth.url.clone(), promise));
        self.send_link_net_query(
            LinkNetKind::RequestUrlOauth,
            tdmtproto::telegram_api::Function::messages_requestUrlAuth(
                tdmtproto::telegram_api::messages_requestUrlAuth {
                    flags,
                    peer: None,
                    msg_id: 0,
                    button_id: 0,
                    url: oauth.url,
                    in_app_origin,
                },
            ),
            "messages.requestUrlAuth",
        );
    }

    /// `LinkManager::update_autologin_token` (cpp:4000-4003).
    pub(crate) fn update_autologin_token(&mut self, autologin_token: String) {
        self.autologin_update_time = tdutils::time::now();
        self.autologin_token = autologin_token;
    }

    /// The current token (`autologin_token_`) — the autologin url
    /// builders (cpp:4092-4124) read it.
    #[allow(dead_code)] // the Wave 2 url builders land; tests pin it now
    pub(crate) fn autologin_token(&self) -> &str {
        &self.autologin_token
    }

    /// The last update time (`autologin_update_time_`) — `Time::now()`
    /// based, i.e. the process clock like the C++ `Time`.
    #[allow(dead_code)] // the Wave 2 url builders land; tests pin it now
    pub(crate) fn autologin_update_time(&self) -> f64 {
        self.autologin_update_time
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// The e3f0a2489 upstream vector (`test/link.cpp`): the
    /// tg://resolve?domain=sendgrams&startapp=tonconnect-... form parses
    /// into the TON Connect link exactly like the t.me path.
    #[test]
    fn tg_sendgrams_startapp_parses_like_ton_connect() {
        struct SendgramsEnv;
        impl LinkEnv for SendgramsEnv {}
        let mut env = SendgramsEnv;
        let link = parse_internal_link(
                &mut env,
                "tg:resolve/\
    ?domain=sendgrams&startapp=tonconnect-v__2-id__c745acd825f83bf7ef88d671ea69c90fd1c7e71f78b1079ec8ad3c231fed7a65-\
    trace--5Fid__01a0b488--2D27a9--2D7419--2D89bc--2D169fc31ba6fd-r__--7B--22manifestUrl--22--3A--22https--3A--2F--\
    2Ftonconnect--2Dsdk--2Ddemo--2Ddapp--2Evercel--2Eapp--2Ftonconnect--2Dmanifest--2Ejson--22--2C--22items--22--3A-\
    -5B--7B--22name--22--3A--22ton--5Faddr--22--7D--5D--7D-ret__none",
                false,
            )
            .expect("the tg sendgrams startapp link parses");
        match link {
            InternalLink::TonConnect {
                version,
                dapp_client_id,
                connect_request,
                return_strategy,
                rpc_request,
                trace_id,
            } => {
                assert_eq!(version, 2);
                assert_eq!(
                    dapp_client_id,
                    "c745acd825f83bf7ef88d671ea69c90fd1c7e71f78b1079ec8ad3c231fed7a65"
                );
                assert_eq!(return_strategy, "none");
                assert_eq!(rpc_request, "");
                assert_eq!(trace_id, "01a0b488-27a9-7419-89bc-169fc31ba6fd");
                assert_eq!(
                    connect_request,
                    "{\"manifestUrl\":\"https://tonconnect-sdk-demo-dapp.vercel.app/tonconnect-manifest.json\",\"items\":[{\"name\":\"ton_addr\"}]}"
                );
            }
            other => panic!("the TON Connect link: {other:?}"),
        }
    }

    /// The `start_up` line (LinkManager.cpp:1901): the token starts
    /// empty and its update time a year in the past — the autologin url
    /// builder's 10000 s freshness gate (cpp:4092) fails closed until a
    /// config supplies one.
    #[test]
    fn the_token_starts_empty_and_a_year_stale() {
        let link_manager = LinkManager::new();
        assert_eq!(link_manager.autologin_token(), "");
        assert!(link_manager.autologin_update_time() <= tdutils::time::now() - 364.0 * 86400.0);
    }

    /// `update_autologin_token` (cpp:4000-4003): the token stores and
    /// the update time jumps to now.
    #[test]
    fn update_autologin_token_stores_and_stamps() {
        let mut link_manager = LinkManager::new();
        let stale = link_manager.autologin_update_time();
        let before = tdutils::time::now();
        link_manager.update_autologin_token("token".to_string());
        assert_eq!(link_manager.autologin_token(), "token");
        assert!(link_manager.autologin_update_time() >= before);
        assert!(link_manager.autologin_update_time() > stale);
    }

    /// `check_link` (LinkManager.cpp:1926-1992) over http/https/tg/ton.
    #[test]
    fn check_link_accepts_and_normalizes() {
        use crate::link_manager::check_link;
        // Plain http URL with a dot in the host passes through.
        assert_eq!(
            check_link("http://example.com/", false, false).unwrap(),
            "http://example.com/"
        );
        assert_eq!(
            check_link("https://EXAMPLE.com/path?x=1", false, false).unwrap(),
            "https://example.com/path?x=1"
        );
        // Host without a dot fails (unless IPv6); check_link wraps the
        // impl error (LinkManager.cpp:1929-1936).
        assert_eq!(
            check_link("http://localhost/", false, false)
                .unwrap_err()
                .message(),
            "URL 'http://localhost/' is invalid: Wrong HTTP URL"
        );
        // tg: scheme keeps only tg://host/path.
        assert_eq!(
            check_link("tg://bot?start=x", false, false).unwrap(),
            "tg://bot?start=x"
        );
        assert_eq!(
            check_link("tg:resolve?domain=telegram", false, false).unwrap(),
            "tg://resolve?domain=telegram"
        );
        // A port in a tg link is rejected (check_link wraps the impl error).
        assert_eq!(
            check_link("tg://bot:80?a=b", false, false)
                .unwrap_err()
                .message(),
            "URL 'tg://bot:80?a=b' is invalid: Wrong tg URL"
        );
        // https_only rejects plain http and tg (check_link wraps the error).
        assert_eq!(
            check_link("http://example.com/", false, true)
                .unwrap_err()
                .message(),
            "URL 'http://example.com/' is invalid: Only HTTPS links are allowed"
        );
        assert_eq!(
            check_link("tg://bot?start=x", false, true)
                .unwrap_err()
                .message(),
            "URL 'tg://bot?start=x' is invalid: Only HTTPS links are allowed"
        );
        // http_only rejects tg.
        assert_eq!(
            check_link("tg://bot?start=x", true, false)
                .unwrap_err()
                .message(),
            "URL 'tg://bot?start=x' is invalid: Only HTTP links are allowed"
        );
        // The error message embeds the URL (check_utf8 branch); the host
        // space is tdutils' parse_url error (HttpUrl.cpp test:95).
        assert_eq!(
            check_link("http://a b/", false, false)
                .unwrap_err()
                .message(),
            "URL 'http://a b/' is invalid: Disallowed character in URL host"
        );
        // ton/tonsite hosts allow dots only in tonsite; the empty query
        // comes back as the '/' path (check_link_impl tail).
        assert_eq!(
            check_link("ton://resolver", false, false).unwrap(),
            "ton://resolver/"
        );
        assert_eq!(
            check_link("tonsite://a.b.c/page", false, false).unwrap(),
            "tonsite://a.b.c/page"
        );
    }

    /// `get_link_user_id` (LinkManager.cpp:4385-4425).
    #[test]
    fn get_link_user_id_parses_tg_user_links() {
        use crate::link_manager::get_link_user_id;
        assert_eq!(
            get_link_user_id("tg://user?id=123456789"),
            UserId::new(123456789)
        );
        assert_eq!(
            get_link_user_id("TG://USER?ID=123"),
            UserId::new(123),
            "scheme/host are case-folded"
        );
        assert_eq!(
            get_link_user_id("tg://user?id=42#fragment"),
            UserId::new(42)
        );
        assert_eq!(get_link_user_id("tg://user/?id=7"), UserId::new(7));
        // Strict to_integer_safe: leading zeros and garbage fail.
        assert_eq!(get_link_user_id("tg://user?id=042"), UserId::new(0));
        assert_eq!(get_link_user_id("tg://user?id=abc"), UserId::new(0));
        // Non-user links and missing ids fail.
        assert_eq!(
            get_link_user_id("tg://resolve?domain=telegram"),
            UserId::new(0)
        );
        assert_eq!(get_link_user_id("tg://user"), UserId::new(0));
        assert_eq!(get_link_user_id("tg://user?other=1"), UserId::new(0));
        assert_eq!(get_link_user_id("https://t.me/user?id=1"), UserId::new(0));
    }

    /// The net-drain 32 net-inert posture (§0): with no dispatch surface
    /// the promises park in their family lots and nothing dispatches —
    /// the offline behavior is byte-identical to the pre-drain parking
    /// (the requests stay unanswered).
    #[test]
    fn net_queries_park_inert() {
        let mut manager = LinkManager::new();
        assert!(manager.net.is_none(), "no dispatch surface");
        manager.get_recent_me_urls("ref".to_owned(), Default::default());
        assert_eq!(manager.pending.recent_me_urls.len(), 1);
        manager.get_deep_link_info("tg://join?invite=x", Default::default());
        assert_eq!(manager.pending.deep_link_info.len(), 1);
        assert!(manager.pending.url_auth.is_empty() && manager.pending.url_oauth.is_empty());
    }

    /// The `RequestUrlAuthQuery::send` peer band (cpp:1662): a message
    /// dialog whose read input peer can't resolve never parks the
    /// promise and never sends — the C++ `CHECK(input_peer != nullptr)`
    /// is fatal there; the port downgrades it to log-and-drop.
    #[test]
    fn request_url_auth_drops_on_unresolvable_peer() {
        use crate::messages_manager::{MessageId, ServerMessageId};
        struct ResolvedButtonEnv;
        impl LinkEnv for ResolvedButtonEnv {
            fn get_login_button_url(
                &mut self,
                _message_full_id: MessageFullId,
                _button_id: i64,
            ) -> tdutils::status::Result<String> {
                tdutils::status::Result::Ok("https://btn.example/auth".to_owned())
            }
            // get_input_peer_read keeps the default `None`
        }
        let mut manager = LinkManager::new();
        manager.get_login_url_info(
            &mut ResolvedButtonEnv,
            MessageFullId::new(
                crate::dialog_id::DialogId::from_user_id(UserId::new(900)),
                MessageId::from_server_message_id(ServerMessageId::new(123)),
            ),
            7,
            Default::default(),
        );
        assert!(manager.pending.url_auth.is_empty(), "the CHECK band");
    }

    /// The upstream `test/link.cpp` web-proxy vectors (e52edfd8b,
    /// `parse_internal_link_part3`) — the parsed form is asserted against
    /// the C++ expectations verbatim, host normalization and the marked
    /// 0x70 link-secret round-trip included.
    #[test]
    fn web_proxy_links_parse_like_the_cpp_reference() {
        struct WebProxyEnv;
        impl LinkEnv for WebProxyEnv {}
        let mut env = WebProxyEnv;
        let proxy_web = |server: &str, path: &str, secret: &str| InternalLink::Proxy {
            server: server.to_owned(),
            port: 443,
            r#type: Some(td_api::ProxyType::proxyTypeWeb(td_api::proxyTypeWeb {
                path: path.to_owned(),
                secret: secret.to_owned(),
            })),
        };
        let unsupported = || InternalLink::Proxy {
            server: String::new(),
            port: 0,
            r#type: None,
        };
        let mut parse = |link: &str| {
            parse_internal_link(&mut env, link, false).expect("the link must parse to a proxy form")
        };

        assert_eq!(
            parse("t.me/webproxy?server=proxy.example.com&secret=8561944064fc730cbfa4473562d8ec59"),
            proxy_web("proxy.example.com", "", "8561944064fc730cbfa4473562d8ec59")
        );
        // the port parameter is ignored, and the hex secret re-canonicalizes
        assert_eq!(
            parse(
                "https://t.me/webproxy?server=proxy.example.com&port=80&secret=8561944064FC730CBFA4473562D8EC59"
            ),
            proxy_web("proxy.example.com", "", "8561944064fc730cbfa4473562d8ec59")
        );
        // scheme/trailing-dot normalization of the host; the dd-padded
        // secret keeps its padding
        assert_eq!(
            parse(
                "t.me/webproxy?server=HTTPS%3A%2F%2FProxy.Example.COM.&secret=dd8561944064fc730cbfa4473562d8ec59"
            ),
            proxy_web(
                "proxy.example.com",
                "",
                "dd8561944064fc730cbfa4473562d8ec59"
            )
        );
        // the punycode pass (idn_to_ascii) is part of the contract
        assert_eq!(
            parse(
                "t.me/webproxy?server=%D0%BF%D1%80%D0%B8%D0%BC%D0%B5%D1%80.%D1%80%D1%84&secret=8561944064fc730cbfa4473562d8ec59"
            ),
            proxy_web(
                "xn--e1afmkfd.xn--p1ai",
                "",
                "8561944064fc730cbfa4473562d8ec59"
            )
        );
        assert_eq!(
            parse(
                "t.me/webproxy?server=%D0%9F%D1%80%D0%B8%D0%BC%D0%B5%D1%80.%D0%A0%D0%A4&secret=8561944064fc730cbfa4473562d8ec59"
            ),
            proxy_web(
                "xn--e1afmkfd.xn--p1ai",
                "",
                "8561944064fc730cbfa4473562d8ec59"
            )
        );
        // the %2F-encoded path and the 0x70-marked link secret
        assert_eq!(
            parse(
                "t.me/webproxy?server=proxy.example.com%2Fdobry-cola-super-app&secret=cIVhlEBk_HMMv6RHNWLY7Fk"
            ),
            proxy_web(
                "proxy.example.com",
                "dobry-cola-super-app",
                "8561944064fc730cbfa4473562d8ec59"
            )
        );
        assert_eq!(
            parse(
                "t.me/webproxy?server=proxy.example.com%2Fa%2Fb_c&secret=cN2FYZRAZPxzDL-kRzVi2OxZ"
            ),
            proxy_web(
                "proxy.example.com",
                "a/b_c",
                "dd8561944064fc730cbfa4473562d8ec59"
            )
        );
        // a marked secret without a path falls back to the plain secret
        assert_eq!(
            parse("t.me/webproxy?server=proxy.example.com&secret=cIVhlEBk_HMMv6RHNWLY7Fk"),
            proxy_web("proxy.example.com", "", "8561944064fc730cbfa4473562d8ec59")
        );

        // the unsupported band: a plain (unmarked) secret with a path,
        // fake-TLS secrets, non-domain hosts, invalid paths, a missing
        // secret
        for link in [
            "t.me/webproxy?server=proxy.example.com%2Fapp&secret=8561944064fc730cbfa4473562d8ec59",
            "t.me/webproxy?server=proxy.example.com&secret=7tAAAAAAAAAAAAAAAAAAAAAAAAcuZ29vZ2xlLmNvbQ",
            "t.me/webproxy?server=proxy.example.com&secret=de8561944064fc730cbfa4473562d8ec59",
            "t.me/webproxy?server=1.2.3.4&secret=8561944064fc730cbfa4473562d8ec59",
            "t.me/webproxy?server=proxy.0x7f&secret=8561944064fc730cbfa4473562d8ec59",
            "t.me/webproxy?server=localhost&secret=8561944064fc730cbfa4473562d8ec59",
            "t.me/webproxy?server=proxy.example.com%2F-app&secret=cIVhlEBk_HMMv6RHNWLY7Fk",
            "t.me/webproxy?server=proxy.example.com%2Fa%2F%2Fb&secret=cIVhlEBk_HMMv6RHNWLY7Fk",
            "t.me/webproxy?server=proxy.example.com%2Fa.b&secret=cIVhlEBk_HMMv6RHNWLY7Fk",
            "t.me/webproxy?server=proxy.example.com",
            "t.me/webproxy",
        ] {
            assert_eq!(
                parse(link),
                unsupported(),
                "link must be unsupported: {link}"
            );
        }

        // the tg forms ride the same helper
        assert_eq!(
            parse("tg:webproxy?server=proxy.example.com&secret=8561944064fc730cbfa4473562d8ec59"),
            proxy_web("proxy.example.com", "", "8561944064fc730cbfa4473562d8ec59")
        );
        assert_eq!(
            parse(
                "tg://webproxy?server=proxy.example.com%2Fdobry-cola-super-app&secret=cIVhlEBk_HMMv6RHNWLY7Fk"
            ),
            proxy_web(
                "proxy.example.com",
                "dobry-cola-super-app",
                "8561944064fc730cbfa4473562d8ec59"
            )
        );
        // the tg unsupported forms (upstream test/link.cpp tail vectors)
        assert_eq!(
            parse(
                "tg:webproxy?server=proxy.example.com%2Fapp&secret=8561944064fc730cbfa4473562d8ec59"
            ),
            unsupported(),
            "a plain secret with a path is unsupported"
        );
        assert_eq!(parse("tg:webproxy"), unsupported());
    }

    /// `LinkManager::get_proxy_link`'s web arm (LinkManager.cpp:4908-4912)
    /// — the link of a parsed web proxy regenerates the upstream bytes:
    /// the path rides the server parameter, the secret stays marked, and
    /// the whole round trip is byte-identical (upstream test/link.cpp
    /// vectors).
    #[test]
    fn web_proxy_link_generation_round_trips_the_upstream_form() {
        struct WebProxyEnv;
        impl LinkEnv for WebProxyEnv {}
        let mut env = WebProxyEnv;

        for link in [
            "tg://webproxy?server=proxy.example.com%2Fa%2Fb_c&secret=cN2FYZRAZPxzDL-kRzVi2OxZ",
            "tg://webproxy?server=proxy.example.com&secret=8561944064fc730cbfa4473562d8ec59",
        ] {
            let Some(InternalLink::Proxy {
                server,
                port,
                r#type,
            }) = parse_internal_link(&mut env, link, false)
            else {
                panic!("the upstream link must parse: {link}");
            };
            let proxy = NetProxy::create_proxy(&td_api::Proxy {
                server: server.clone(),
                port,
                r#type,
            })
            .expect("the parsed form must revalidate");
            assert_eq!(
                get_proxy_link(&env, &proxy, true).unwrap(),
                link,
                "the generated link must round-trip the parsed one"
            );
        }

        // the HTTP proxies keep their no-public-links band
        let http = NetProxy::create_proxy(&td_api::Proxy {
            server: "proxy.example.com".to_owned(),
            port: 8080,
            r#type: Some(td_api::ProxyType::proxyTypeHttp(td_api::proxyTypeHttp {
                username: String::new(),
                password: String::new(),
                http_only: false,
            })),
        })
        .unwrap();
        assert_eq!(
            get_proxy_link(&env, &http, true).unwrap_err().message(),
            "HTTP proxies have no public links"
        );
    }

    /// The `free=` unpaid-messages-token vectors (upstream 869e6e6c5 +
    /// c6f5bdb50, test/link.cpp) — the token parses on every
    /// dialog/user/token link form and drops on the invalid shapes
    /// (empty, a space, over 64 characters).
    #[test]
    fn unpaid_messages_tokens_parse_on_every_dialog_link_form() {
        struct FreeLinkEnv;
        impl LinkEnv for FreeLinkEnv {}
        let mut env = FreeLinkEnv;
        let mut parse = |link: &str| parse_internal_link(&mut env, link, false);

        let public_chat =
            |draft: &str, open_profile: bool, free: &str| InternalLink::PublicDialog {
                dialog_username: "username".to_owned(),
                draft_text: draft.to_owned(),
                open_profile,
                unpaid_messages_token: free.to_owned(),
            };
        let user_phone =
            |draft: &str, open_profile: bool, free: &str| InternalLink::UserPhoneNumber {
                phone_number: "+123456".to_owned(),
                draft_text: draft.to_owned(),
                open_profile,
                unpaid_messages_token: free.to_owned(),
            };
        let user_token = |token: &str, free: &str| InternalLink::UserToken {
            token: token.to_owned(),
            unpaid_messages_token: free.to_owned(),
        };

        // tg:resolve?domain= + free
        assert_eq!(
            parse("tg:resolve?domain=username&free=AbCd-_0123456789").unwrap(),
            public_chat("", false, "AbCd-_0123456789")
        );
        assert_eq!(
            parse("tg:resolve?domain=username&free=abc&profile&text=@asd").unwrap(),
            public_chat(" @asd", true, "abc")
        );
        for link in [
            "tg:resolve?domain=username&free=",
            "tg:resolve?domain=username&free=a%20b",
            "tg:resolve?domain=username&free=a+b",
        ] {
            assert_eq!(parse(link).unwrap(), public_chat("", false, ""), "{link}");
        }

        // tg:resolve?phone= + free
        assert_eq!(
            parse("tg:resolve?phone=123456&free=AbCd-_0123456789").unwrap(),
            user_phone("", false, "AbCd-_0123456789")
        );
        assert_eq!(
            parse("tg:resolve?phone=123456&free=abc&profile&text=@asd").unwrap(),
            user_phone(" @asd", true, "abc")
        );
        assert_eq!(
            parse("tg:resolve?phone=123456&free=a%20b").unwrap(),
            user_phone("", false, "")
        );

        // tg:contact?token= + free — the token travels either way around
        assert_eq!(
            parse("tg:contact?token=123456&free=AbCd-_0123456789").unwrap(),
            user_token("123456", "AbCd-_0123456789")
        );
        assert_eq!(
            parse("tg:contact?free=abc&token=123456").unwrap(),
            user_token("123456", "abc")
        );
        assert_eq!(
            parse("tg:contact?token=123456&free=").unwrap(),
            user_token("123456", "")
        );
        assert_eq!(
            parse("tg:contact?token=123456&free=a%20b").unwrap(),
            user_token("123456", "")
        );

        // the t.me forms: /<username>, /+<phone>, /contact/<token> — the
        // 64-byte token stays, the 65-byte one drops
        assert_eq!(
            parse("t.me/username?free=AbCd-_0123456789").unwrap(),
            public_chat("", false, "AbCd-_0123456789")
        );
        assert_eq!(
            parse("t.me/username?text=@asd&free=abc&profile").unwrap(),
            public_chat(" @asd", true, "abc")
        );
        assert_eq!(
            parse("t.me/username?free=a%20b").unwrap(),
            public_chat("", false, "")
        );
        // the 64-byte token stays, the 65-byte one drops
        assert_eq!(
            parse("t.me/username?free=0123456789012345678901234567890123456789012345678901234567890123")
                .unwrap(),
            public_chat(
                "",
                false,
                "0123456789012345678901234567890123456789012345678901234567890123"
            )
        );
        assert_eq!(
            parse("t.me/username?free=01234567890123456789012345678901234567890123456789012345678901234")
                .unwrap(),
            public_chat("", false, "")
        );
        assert_eq!(
            parse("t.me/+123456?free=AbCd-_0123456789").unwrap(),
            user_phone("", false, "AbCd-_0123456789")
        );
        assert_eq!(
            parse("t.me/+123456?free=abc&profile&text=@asd").unwrap(),
            user_phone(" @asd", true, "abc")
        );
        assert_eq!(
            parse("t.me/+123456?free=a%20b").unwrap(),
            user_phone("", false, "")
        );
        assert_eq!(
            parse("t.me/contact/123456?free=AbCd-_0123456789").unwrap(),
            user_token("123456", "AbCd-_0123456789")
        );
        assert_eq!(
            parse("t.me/contact/123456/?free=abc").unwrap(),
            user_token("123456", "abc")
        );
        assert_eq!(
            parse("t.me/contact/123456?free=a%20b").unwrap(),
            user_token("123456", "")
        );
    }

    /// The `free=` query generation (`get_dialog_link_query`,
    /// LinkManager.cpp:470-481): every dialog-link builder appends the
    /// unpaid-messages token after text/profile, and the invalid token
    /// forms answer the C++ 400 band.
    #[test]
    fn unpaid_messages_tokens_generate_the_free_query() {
        struct FreeLinkEnv;
        impl LinkEnv for FreeLinkEnv {}
        let mut env = FreeLinkEnv;
        let phone = td_api::InternalLinkType::internalLinkTypeUserPhoneNumber(
            td_api::internalLinkTypeUserPhoneNumber {
                phone_number: "+123456".to_owned(),
                draft_text: String::new(),
                open_profile: false,
                unpaid_messages_token: "abc".to_owned(),
            },
        );
        assert_eq!(
            get_internal_link(&mut env, Some(&phone), false).unwrap(),
            "https://t.me/+123456?free=abc"
        );
        assert_eq!(
            get_internal_link(&mut env, Some(&phone), true).unwrap(),
            "tg://resolve?phone=+123456&free=abc"
        );

        let token = td_api::InternalLinkType::internalLinkTypeUserToken(
            td_api::internalLinkTypeUserToken {
                token: "123456".to_owned(),
                unpaid_messages_token: "abc".to_owned(),
            },
        );
        assert_eq!(
            get_internal_link(&mut env, Some(&token), false).unwrap(),
            "https://t.me/contact/123456?free=abc"
        );
        assert_eq!(
            get_internal_link(&mut env, Some(&token), true).unwrap(),
            "tg://contact?token=123456&free=abc"
        );

        let chat = td_api::InternalLinkType::internalLinkTypePublicChat(
            td_api::internalLinkTypePublicChat {
                chat_username: "username".to_owned(),
                draft_text: "@asd".to_owned(),
                open_profile: true,
                unpaid_messages_token: "abc".to_owned(),
            },
        );
        assert_eq!(
            get_internal_link(&mut env, Some(&chat), false).unwrap(),
            "https://t.me/username?text=%40asd&profile&free=abc"
        );
        assert_eq!(
            get_internal_link(&mut env, Some(&chat), true).unwrap(),
            "tg://resolve?domain=username&text=%40asd&profile&free=abc"
        );

        // the invalid-token 400 band (LinkManager.cpp:4425-4428)
        let bad = td_api::InternalLinkType::internalLinkTypeUserPhoneNumber(
            td_api::internalLinkTypeUserPhoneNumber {
                phone_number: "+123456".to_owned(),
                draft_text: String::new(),
                open_profile: false,
                unpaid_messages_token: "a b".to_owned(),
            },
        );
        assert_eq!(
            get_internal_link(&mut env, Some(&bad), false)
                .unwrap_err()
                .message(),
            "Invalid unpaid messages token specified"
        );
    }

    /// The 2485f5889 TON Connect vectors (`accept_oauth_request`,
    /// LinkManager.cpp:4682-4700 at the 23383aef pin): the connect
    /// payload validates through `get_wallet_ownership_proof` and
    /// rides the `AcceptUrlAuthQuery` send as the wire session — only
    /// when set — while the invalid proofs answer the promise with the
    /// 400 band (`TRY_RESULT_PROMISE`) and skip the send entirely.
    #[test]
    fn accept_oauth_request_threads_the_ton_connect_session() {
        use std::sync::{Arc, Mutex};

        #[derive(Default)]
        struct RecordingEnv {
            sent: Vec<Option<tdmtproto::telegram_api::InputTonConnectOauthSession>>,
        }
        impl LinkEnv for RecordingEnv {
            #[allow(clippy::too_many_arguments)] // the C++ handler shape
            fn send_accept_url_auth_query(
                &mut self,
                _url: String,
                _message_full_id: MessageFullId,
                _button_id: i32,
                _allow_write_access: bool,
                _allow_phone_number_access: bool,
                _match_code: String,
                tonconnect_session: Option<tdmtproto::telegram_api::InputTonConnectOauthSession>,
                _promise: tdutils::promise::Promise<String>,
            ) {
                self.sent.push(tonconnect_session);
            }
        }

        let connect_data = |proof: Option<td_api::TonWalletOwnershipProof>| {
            Some(td_api::TonConnectConnectData {
                session_id: 42,
                challenge_answer: b"challenge-answer".to_vec(),
                body: b"connect-body".to_vec(),
                proof,
            })
        };
        let proof_of = |public_key: usize, signature: usize| {
            Some(td_api::TonWalletOwnershipProof {
                public_key: vec![1u8; public_key],
                timestamp: 7,
                signature: vec![2u8; signature],
            })
        };

        // the null payload keeps the session off the wire query
        let mut env = RecordingEnv::default();
        LinkManager::new().accept_oauth_request(
            &mut env,
            "https://oauth.example/request".to_owned(),
            "code".to_owned(),
            true,
            false,
            None,
            Default::default(),
        );
        assert_eq!(env.sent, vec![None], "the null payload sends no session");

        // the valid payload becomes the wire session (the proof's
        // public-key half validates but doesn't ride — only the wire
        // walletOwnershipProof does)
        LinkManager::new().accept_oauth_request(
            &mut env,
            "https://oauth.example/request".to_owned(),
            "code".to_owned(),
            true,
            false,
            connect_data(proof_of(32, 64)),
            Default::default(),
        );
        assert_eq!(
            env.sent[1],
            Some(tdmtproto::telegram_api::InputTonConnectOauthSession {
                session_id: 42,
                challenge_answer: b"challenge-answer".to_vec(),
                body: b"connect-body".to_vec(),
                proof: Some(tdmtproto::telegram_api::WalletOwnershipProof {
                    timestamp: 7,
                    signature: vec![2u8; 64],
                }),
            }),
            "the payload threads as the wire session"
        );

        // the invalid proofs answer the 400 band and never reach the send
        for (proof, message) in [
            (None, "Proof must be non-empty"),
            (proof_of(31, 64), "Invalid public key specified"),
            (proof_of(32, 63), "Invalid signature specified"),
        ] {
            let answer = Arc::new(Mutex::new(None));
            let capture = Arc::clone(&answer);
            let promise = tdutils::promise::PromiseCreator::lambda_result(
                move |result: tdutils::status::Result<String>| {
                    let actual = match result {
                        tdutils::status::Result::Err(status) => status.message().to_owned(),
                        tdutils::status::Result::Ok(_) => panic!("expected the error band"),
                    };
                    *capture.lock().unwrap() = Some(actual);
                },
            );
            LinkManager::new().accept_oauth_request(
                &mut env,
                "https://oauth.example/request".to_owned(),
                "code".to_owned(),
                true,
                false,
                connect_data(proof),
                promise,
            );
            assert_eq!(
                answer.lock().unwrap().take(),
                Some(message.to_owned()),
                "the proof band answers the promise"
            );
        }
        assert_eq!(env.sent.len(), 2, "the error band skips the send");
    }
}
