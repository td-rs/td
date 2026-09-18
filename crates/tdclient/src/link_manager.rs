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

use crate::emoji_status::CustomEmojiId;
use crate::formatted_date::FormattedDate;
use crate::user_manager::UserId;
use tdutils::http_url::{Protocol, parse_url};
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

/// `LinkManager::check_tg_url_host` (LinkManager.cpp:4408-4432): the shared
/// `tg:<host>?<query>` validation; returns the query part.
fn check_tg_url_host(url: &str, host: &str) -> Result<String, Status> {
    let url_bytes = url.as_bytes();
    let min_size = 3 + 2 + host.len() + 1;
    let take = min_size.min(url_bytes.len());
    let lowered_prefix = &url_bytes[..take];
    let begins = |prefix: &[u8]| {
        lowered_prefix.len() >= prefix.len()
            && lowered_prefix[..prefix.len()]
                .iter()
                .zip(prefix)
                .all(|(c, p)| c.to_ascii_lowercase() == *p)
    };

    if !begins(b"tg:") {
        return Err(Status::error(400, "URL must have scheme tg"));
    }
    let mut url_rest = &url[3..];
    let mut rest_bytes = &url_bytes[3..];
    if url_rest.starts_with("//") {
        url_rest = &url_rest[2..];
        rest_bytes = &rest_bytes[2..];
    }

    let host_bytes = host.as_bytes();
    if !rest_bytes.starts_with(host_bytes)
        || (rest_bytes.len() > host_bytes.len()
            && !matches!(rest_bytes[host_bytes.len()], b'/' | b'?' | b'#'))
    {
        return Err(Status::error(
            400,
            &format!("URL must have host \"{host}\""),
        ));
    }
    url_rest = &url_rest[host.len()..];
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

/// The autologin-token state (LinkManager.h:227-229).
#[derive(Debug)]
pub(crate) struct LinkManager {
    autologin_token: String,
    autologin_update_time: f64,
}

impl LinkManager {
    /// The constructor plus the `start_up` timestamp line
    /// (LinkManager.cpp:1895-1901): the token starts empty and its
    /// update time a year in the past. The domain-list pmc restore in
    /// between lands with the domain-list step.
    pub(crate) fn new() -> Self {
        Self {
            autologin_token: String::new(),
            autologin_update_time: tdutils::time::now() - 365.0 * 86400.0,
        }
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
}
