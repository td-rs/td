//! Port of `td/utils/HttpUrl.h` + `HttpUrl.cpp`.
//!
//! `parse_url` accepts `[http(s)://][userinfo@]host[:port][/query]` with the
//! exact tdlib validation rules: host/userinfo charset allow-lists (RFC 7230 +
//! RFC 3986 plus plain UTF-8), IPv6 `[...]` hosts, leading-zero port stripping
//! and `query` normalization (leading `/` added, control bytes
//! percent-escaped, trailing spaces trimmed).

use crate::status::{Result, Status};

/// `HttpUrl::Protocol`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Protocol {
    #[default]
    Http,
    Https,
}

impl Protocol {
    fn default_port(self) -> i32 {
        match self {
            Protocol::Http => 80,
            Protocol::Https => 443,
        }
    }

    pub fn scheme(self) -> &'static str {
        match self {
            Protocol::Http => "http://",
            Protocol::Https => "https://",
        }
    }
}

/// `HttpUrl`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpUrl {
    pub protocol: Protocol,
    pub userinfo: String,
    /// Lowercased; keeps the `[...]` brackets for IPv6 hosts like the C++ class.
    pub host: String,
    pub is_ipv6: bool,
    pub specified_port: i32,
    pub port: i32,
    /// Always starts with `/`.
    pub query: String,
}

impl HttpUrl {
    /// `HttpUrl::get_url`. Panics if `query` does not start with `/`
    /// (C++ `LOG_CHECK`, an invariant of `parse_url`).
    pub fn get_url(&self) -> String {
        assert!(
            !self.query.is_empty() && self.query.starts_with('/'),
            "bad query: {}",
            self.query
        );
        let mut result = String::new();
        result += self.protocol.scheme();
        if !self.userinfo.is_empty() {
            result += &self.userinfo;
            result.push('@');
        }
        result += &self.host;
        if self.specified_port > 0 {
            result.push(':');
            result += &self.specified_port.to_string();
        }
        result += &self.query;
        result
    }
}

/// `parse_url(url)` — tdlib's default protocol is HTTP.
pub fn parse_url(url: &str) -> Result<HttpUrl> {
    parse_url_with_default(url, Protocol::Http)
}

fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | b'\r')
}

fn hex_to_int(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// `url_decode` from `td/utils/misc.cpp`: `%XX` needs two hex digits right
/// after `%`, otherwise the `%` stays literal; bytes pass through unchanged.
pub fn url_decode(from: &str, decode_plus_sign_as_space: bool) -> String {
    let from = from.as_bytes();
    let mut to = Vec::with_capacity(from.len());
    let mut i = 0;
    while i < from.len() {
        if from[i] == b'%'
            && i + 2 < from.len()
            && let (Some(high), Some(low)) = (hex_to_int(from[i + 1]), hex_to_int(from[i + 2]))
        {
            to.push(high * 16 + low);
            i += 3;
            continue;
        }
        let c = if decode_plus_sign_as_space && from[i] == b'+' {
            b' '
        } else {
            from[i]
        };
        to.push(c);
        i += 1;
    }
    String::from_utf8(to).unwrap_or_default()
}

/// `url_encode` from `td/utils/misc.cpp`: ASCII alphanumerics and `-._~`
/// pass through, everything else becomes `%XX` with upper-case hex digits.
pub fn url_encode(data: &str) -> String {
    let mut result = String::with_capacity(data.len() * 3);
    for &c in data.as_bytes() {
        if c.is_ascii_alphanumeric() || matches!(c, b'-' | b'.' | b'_' | b'~') {
            result.push(c as char);
        } else {
            result += &format!("%{c:02X}");
        }
    }
    result
}

fn is_hex(b: u8) -> bool {
    hex_to_int(b).is_some()
}

fn check_url_part(part: &[u8], name: &str, allow_colon: bool) -> Result<()> {
    let mut i = 0;
    while i < part.len() {
        let c = part[i];
        if c.is_ascii_alphanumeric()
            || matches!(
                c,
                b'.' | b'-'
                    | b'_'
                    | b'!'
                    | b'$'
                    | b','
                    | b'~'
                    | b'*'
                    | b'\''
                    | b'('
                    | b')'
                    | b';'
                    | b'&'
                    | b'+'
                    | b'='
            )
            || (allow_colon && c == b':')
        {
            // symbols allowed by RFC 7230 and RFC 3986
        } else if c == b'%' {
            if i + 2 < part.len() && is_hex(part[i + 1]) && is_hex(part[i + 2]) {
                // percent encoded symbol as allowed by RFC 7230 and RFC 3986
                i += 2;
            } else {
                return Result::Err(Status::error_message(&format!(
                    "Wrong percent-encoded symbol in URL {name}"
                )));
            }
        } else if c < 128 {
            // all other ASCII symbols aren't allowed
            return Result::Err(Status::error_message(&format!(
                "Disallowed character in URL {name}"
            )));
        }
        // plain UTF-8 bytes (>= 128) are allowed
        i += 1;
    }
    Result::Ok(())
}

/// `parse_url(url, default_protocol)`.
pub fn parse_url_with_default(url: &str, default_protocol: Protocol) -> Result<HttpUrl> {
    let bytes = url.as_bytes();

    // protocol_str = to_lower(read_till_nofail(":/?#@[]"))
    let mut pos = 0;
    while pos < bytes.len() && !matches!(bytes[pos], b':' | b'/' | b'?' | b'#' | b'@' | b'[' | b']')
    {
        pos += 1;
    }
    let protocol_str = url[..pos].to_ascii_lowercase();

    let (protocol, pos) = if bytes.len() >= pos + 3 && &bytes[pos..pos + 3] == b"://" {
        let protocol = match protocol_str.as_str() {
            "http" => Protocol::Http,
            "https" => Protocol::Https,
            _ => return Result::Err(Status::error_message("Unsupported URL protocol")),
        };
        (protocol, pos + 3)
    } else {
        // C++ restarts the parser from the beginning with the default protocol.
        (default_protocol, 0)
    };

    // userinfo_host_port = read_till_nofail("/?#")
    let mut end = pos;
    while end < bytes.len() && !matches!(bytes[end], b'/' | b'?' | b'#') {
        end += 1;
    }
    let userinfo_host_port = &bytes[pos..end];

    // Scan backwards for the port separator; ']' and '@' stop the scan.
    let mut colon = userinfo_host_port.len().saturating_sub(1);
    while colon > 0
        && userinfo_host_port[colon] != b':'
        && userinfo_host_port[colon] != b']'
        && userinfo_host_port[colon] != b'@'
    {
        colon -= 1;
    }

    let (userinfo_host, port) = if colon > 0 && userinfo_host_port[colon] == b':' {
        let mut port_slice = &userinfo_host_port[colon + 1..];
        while port_slice.len() > 1 && port_slice[0] == b'0' {
            port_slice = &port_slice[1..];
        }
        // to_integer_safe: a parse failure or a zero port both yield the
        // sentinel -1, rejected below with "Wrong port number".
        let parsed = std::str::from_utf8(port_slice)
            .ok()
            .and_then(|s| s.parse::<i32>().ok())
            .filter(|p| *p != 0)
            .unwrap_or(-1);
        (&userinfo_host_port[..colon], parsed)
    } else {
        (userinfo_host_port, 0)
    };

    if !(0..=65535).contains(&port) {
        return Result::Err(Status::error_message(
            "Wrong port number specified in the URL",
        ));
    }

    let (userinfo, host) = match userinfo_host.iter().rposition(|&c| c == b'@') {
        Some(at_pos) => (&userinfo_host[..at_pos], &userinfo_host[at_pos + 1..]),
        None => (&userinfo_host[..0], userinfo_host),
    };

    let mut is_ipv6 = false;
    if !host.is_empty() && host[0] == b'[' && host[host.len() - 1] == b']' {
        // C++ validates via IPAddress::init_ipv6_port(host, 1).
        let inner = std::str::from_utf8(&host[1..host.len() - 1]);
        let valid = match inner {
            Ok(inner) => inner.parse::<std::net::Ipv6Addr>().is_ok(),
            Err(_) => false,
        };
        if !valid {
            return Result::Err(Status::error_message(
                "Wrong IPv6 address specified in the URL",
            ));
        }
        is_ipv6 = true;
    }
    if host.is_empty() {
        return Result::Err(Status::error_message("URL host is empty"));
    }
    if host == b"." {
        return Result::Err(Status::error_message("Host is invalid"));
    }

    let specified_port = port;
    let port = if port == 0 {
        protocol.default_port()
    } else {
        port
    };

    // query = read_all(), trailing spaces trimmed, '/'-prefixed, control bytes escaped
    let mut query = &bytes[end..];
    while let Some(&last) = query.last() {
        if is_space(last) {
            query = &query[..query.len() - 1];
        } else {
            break;
        }
    }
    let mut query_str = String::new();
    if query.is_empty() {
        query_str.push('/');
    } else {
        if query[0] != b'/' {
            query_str.push('/');
        }
        for &c in query {
            if c <= 0x20 {
                query_str.push('%');
                query_str.push(
                    char::from_digit(u32::from(c) / 16, 16)
                        .unwrap()
                        .to_ascii_uppercase(),
                );
                query_str.push(
                    char::from_digit(u32::from(c) % 16, 16)
                        .unwrap()
                        .to_ascii_uppercase(),
                );
            } else {
                query_str.push(c as char);
            }
        }
    }

    let host_str = String::from_utf8_lossy(host).to_lowercase();
    if is_ipv6 {
        for &c in &host_str.as_bytes()[1..host_str.len() - 1] {
            if c != b':' && !c.is_ascii_hexdigit() && c != b'.' {
                return Result::Err(Status::error_message("Wrong IPv6 URL host"));
            }
        }
    } else {
        crate::try_result!(_unit, check_url_part(host_str.as_bytes(), "host", false));
        crate::try_result!(_unit, check_url_part(userinfo, "userinfo", true));
    }

    Result::Ok(HttpUrl {
        protocol,
        userinfo: String::from_utf8_lossy(userinfo).into_owned(),
        host: host_str,
        is_ipv6,
        specified_port,
        port,
        query: query_str,
    })
}

/// `HttpUrlQuery`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HttpUrlQuery {
    pub path: Vec<String>,
    pub args: Vec<(String, String)>,
}

impl HttpUrlQuery {
    /// `HttpUrlQuery::has_arg` — exact byte match, first occurrence.
    pub fn has_arg(&self, key: &str) -> bool {
        self.args.iter().any(|(k, _)| k == key)
    }

    /// `HttpUrlQuery::get_arg` — exact byte match, first occurrence.
    pub fn get_arg(&self, key: &str) -> Option<&str> {
        self.args
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

/// `parse_url_query`.
pub fn parse_url_query(query: &str) -> HttpUrlQuery {
    let query = query.strip_prefix('/').unwrap_or(query);
    let bytes = query.as_bytes();

    let mut path_size = 0;
    while path_size < bytes.len() && bytes[path_size] != b'?' && bytes[path_size] != b'#' {
        path_size += 1;
    }

    // full_split(path, '/'), each segment url_decoded, trailing empties popped.
    let mut path: Vec<String> = query[..path_size]
        .split('/')
        .map(|segment| url_decode(segment, false))
        .collect();
    while path.last().is_some_and(|segment| segment.is_empty()) {
        path.pop();
    }

    let mut args = Vec::new();
    if path_size < bytes.len() && bytes[path_size] == b'?' {
        let rest = &query[path_size + 1..];
        let rest = match rest.find('#') {
            Some(hash) => &rest[..hash],
            None => rest,
        };
        for key_value in rest.split('&') {
            let (key, value) = match key_value.find('=') {
                Some(eq) => (&key_value[..eq], &key_value[eq + 1..]),
                None => (key_value, ""),
            };
            let key = url_decode(key, true);
            if !key.is_empty() {
                args.push((key, url_decode(value, true)));
            }
        }
    }

    HttpUrlQuery { path, args }
}

/// `get_url_host` — empty string on a parse failure.
pub fn get_url_host(url: &str) -> String {
    match parse_url(url) {
        Result::Ok(http_url) => http_url.host,
        Result::Err(_) => String::new(),
    }
}

/// `get_url_query_file_name`.
pub fn get_url_query_file_name(query: &str) -> String {
    let query_slice = match query.find(['?', '#']) {
        Some(pos) => &query[..pos],
        None => query,
    };
    match query_slice.rfind('/') {
        Some(slash_pos) => query_slice[slash_pos + 1..].to_string(),
        None => query_slice.to_string(),
    }
}

/// `get_url_file_name` — empty string on a parse failure.
pub fn get_url_file_name(url: &str) -> String {
    match parse_url(url) {
        // C++ logs a warning on failure here.
        Result::Ok(http_url) => get_url_query_file_name(&http_url.query),
        Result::Err(_) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(url: &str) -> HttpUrl {
        parse_url(url).unwrap()
    }

    fn parse_err(url: &str) -> String {
        match parse_url(url) {
            Result::Err(status) => status.message().to_string(),
            Result::Ok(_) => panic!("expected error for {url}"),
        }
    }

    #[test]
    fn parse_url_defaults() {
        let url = parse_ok("telegram.org");
        assert_eq!(url.protocol, Protocol::Http);
        assert_eq!(url.host, "telegram.org");
        assert_eq!(url.port, 80);
        assert_eq!(url.specified_port, 0);
        assert_eq!(url.query, "/");
        assert_eq!(url.get_url(), "http://telegram.org/");

        let url = parse_ok("https://a.b");
        assert_eq!(url.protocol, Protocol::Https);
        assert_eq!(url.port, 443);
    }

    #[test]
    fn parse_url_query_and_case() {
        let url = parse_ok("https://Telegram.ORG/Path?a=b");
        assert_eq!(url.host, "telegram.org");
        assert_eq!(url.query, "/Path?a=b");

        let url = parse_ok("http://a.b?x=1");
        assert_eq!(url.query, "/?x=1");

        let url = parse_ok("http://a.b#frag");
        assert_eq!(url.query, "/#frag");
    }

    #[test]
    fn parse_url_userinfo_and_ports() {
        let url = parse_ok("http://user:pass@host:8080/x");
        assert_eq!(url.userinfo, "user:pass");
        assert_eq!(url.port, 8080);
        assert_eq!(url.specified_port, 8080);
        assert_eq!(url.get_url(), "http://user:pass@host:8080/x");

        // leading zeros stripped, value kept
        let url = parse_ok("http://host:0008080/x");
        assert_eq!(url.port, 8080);
        assert_eq!(url.specified_port, 8080);
    }

    #[test]
    fn parse_url_ipv6() {
        let url = parse_ok("http://[2001:0Db8::1]/x");
        assert!(url.is_ipv6);
        assert_eq!(url.host, "[2001:0db8::1]");
        assert_eq!(url.port, 80);
        assert_eq!(url.query, "/x");

        let url = parse_ok("[2001:db8::1]:443");
        assert!(url.is_ipv6);
        assert_eq!(url.port, 443);
    }

    #[test]
    fn parse_url_escapes_control_bytes_and_trims_spaces() {
        assert_eq!(parse_ok("host/a b").query, "/a%20b");
        assert_eq!(parse_ok("host/x\t").query, "/x");
        assert_eq!(parse_ok("host/x   ").query, "/x");
    }

    #[test]
    fn parse_url_errors() {
        assert_eq!(parse_err("ftp://a.b/"), "Unsupported URL protocol");
        assert_eq!(
            parse_err("http://a.b:0/"),
            "Wrong port number specified in the URL"
        );
        assert_eq!(
            parse_err("http://a.b:70000/"),
            "Wrong port number specified in the URL"
        );
        assert_eq!(
            parse_err("http://a.b:abc/"),
            "Wrong port number specified in the URL"
        );
        assert_eq!(
            parse_err("http://a.b:/"),
            "Wrong port number specified in the URL"
        );
        assert_eq!(parse_err("http:///x"), "URL host is empty");
        assert_eq!(parse_err("http://./x"), "Host is invalid");
        assert_eq!(
            parse_err("http://[zz::1]/"),
            "Wrong IPv6 address specified in the URL"
        );
        assert_eq!(
            parse_err("http://ho%2Gst/"),
            "Wrong percent-encoded symbol in URL host"
        );
        assert_eq!(
            parse_err("http://ho\"st/"),
            "Disallowed character in URL host"
        );
        assert_eq!(
            parse_err("http://us\"er@h/"),
            "Disallowed character in URL userinfo"
        );
    }

    #[test]
    fn parse_url_percent_encoded_host_kept_verbatim() {
        // the host is validated but never decoded
        assert_eq!(parse_ok("http://a%41b/").host, "a%41b");
    }

    #[test]
    fn url_decode_matches_misc_cpp() {
        assert_eq!(url_decode("%41", false), "A");
        assert_eq!(url_decode("%4", false), "%4");
        assert_eq!(url_decode("%zz", false), "%zz");
        assert_eq!(url_decode("a+b", true), "a b");
        assert_eq!(url_decode("a+b", false), "a+b");
        assert_eq!(url_decode("%C3%A9", false), "é");
        assert_eq!(url_decode("100%", false), "100%");
    }

    #[test]
    fn parse_url_query_paths_and_args() {
        let q = parse_url_query("/path/to/x?a=1&b=2&a=3#frag");
        assert_eq!(
            q.path,
            vec!["path".to_string(), "to".to_string(), "x".to_string()]
        );
        assert_eq!(q.args.len(), 3);
        assert_eq!(q.get_arg("a"), Some("1"));
        assert!(q.has_arg("b"));
        assert_eq!(q.get_arg("c"), None);

        let q = parse_url_query("/?k=a+b");
        assert_eq!(q.get_arg("k"), Some("a b"));

        let q = parse_url_query("/?=v&k2=v2");
        assert_eq!(q.args, vec![("k2".to_string(), "v2".to_string())]);

        let q = parse_url_query("/a//b/");
        assert_eq!(
            q.path,
            vec!["a".to_string(), String::new(), "b".to_string()]
        );

        let q = parse_url_query("x#frag?a=1");
        assert_eq!(q.path, vec!["x".to_string()]);
        assert!(q.args.is_empty());

        let q = parse_url_query("/?k=%2F1");
        assert_eq!(q.get_arg("k"), Some("/1"));
    }

    #[test]
    fn file_name_helpers() {
        assert_eq!(get_url_query_file_name("/a/b/c.txt?x"), "c.txt");
        assert_eq!(get_url_query_file_name("/a/b/"), "");
        assert_eq!(get_url_file_name("http://a.b/c/d.png"), "d.png");
        // "notaurl" parses fine (host = "notaurl", query = "/")
        assert_eq!(get_url_file_name("notaurl"), "");
        assert_eq!(get_url_host("http://a.b:8080/x"), "a.b");
        assert_eq!(get_url_host("ftp://x/"), "");
    }
}
