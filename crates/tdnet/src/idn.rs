//! `td::idn_to_ascii` (`td/utils/port/IPAddress.cpp`) — the pure host
//! normalization half of the DNS layer (S47). It lives outside the
//! native-gated [`crate::dns`] module because the web-proxy host check
//! (`Proxy::normalize_web_proxy_host`) calls it on every platform: the
//! normalized host feeds the relay capability HMAC, which must not depend
//! on the OS.

use tdutils::status::Status;

/// `td::idn_to_ascii` — normalize a host for DNS: fully-ASCII hosts are
/// lowercased; otherwise each non-ASCII label is punycode-encoded with an
/// `xn--` prefix (ASCII labels inside a non-ASCII host are appended
/// verbatim, as in C++).
///
/// `&str` is always valid UTF-8, so the C++ "Host name must be encoded in
/// UTF-8" error is unreachable here.
pub fn idn_to_ascii(host: &str) -> Result<String, Status> {
    if host.is_ascii() {
        return Ok(host.to_ascii_lowercase());
    }

    const MAX_DNS_NAME_LENGTH: usize = 255;
    if host.len() >= MAX_DNS_NAME_LENGTH * 4 {
        // upper bound, 4 bytes per symbol
        return Err(Status::error_message("Host name is too long"));
    }

    let mut result = String::with_capacity(host.len());
    for (i, part) in host.split('.').enumerate() {
        if i > 0 {
            result.push('.');
        }
        if part.is_ascii() {
            result.push_str(part);
        } else {
            // TODO nameprep should be applied first, but punycode is better
            // than nothing (as the C++ comment says).
            result.push_str("xn--");
            punycode(&mut result, part);
        }
    }
    Ok(result)
}

/// RFC 3492 punycode encoder, ported verbatim from
/// `td/utils/port/IPAddress.cpp` — including its sign-flipped bias
/// representation and the `delta /= 35` in the adaptation loop, which differ
/// textually from the RFC pseudocode but reproduce the C++ output
/// byte-for-byte (pinned by the vectors in `tests/dns.rs`).
fn punycode(result: &mut String, part: &str) {
    let mut codes = Vec::with_capacity(part.chars().count());
    let mut processed: u32 = 0;
    for ch in part.chars() {
        let code = ch as u32;
        if code <= 127 {
            result.push((code as u8).to_ascii_lowercase() as char);
            processed += 1;
        }
        codes.push(code);
    }

    if processed > 0 {
        result.push('-');
    }

    let mut n: u32 = 127;
    let mut delta: u32 = 0;
    let mut bias: i32 = -72;
    let mut is_first = true;
    while (processed as usize) < codes.len() {
        // choose lowest not processed code
        let mut next_n: u32 = 0x110000;
        for &code in &codes {
            if code > n && code < next_n {
                next_n = code;
            }
        }
        delta = delta.wrapping_add((next_n - n - 1).wrapping_mul(processed + 1));

        for &code in &codes {
            if code < next_n {
                delta = delta.wrapping_add(1);
            }
            if code == next_n {
                // found next symbol, encode delta
                let mut left = delta as i32;
                loop {
                    bias += 36;
                    let t = bias.clamp(1, 26);
                    if left < t {
                        result.push((left as u8 + b'a') as char);
                        break;
                    }

                    left -= t;
                    let digit = t + left % (36 - t);
                    result.push(if digit < 26 {
                        (digit as u8 + b'a') as char
                    } else {
                        (digit as u8 - 26 + b'0') as char
                    });
                    left /= 36 - t;
                }
                processed += 1;

                // update bias (RFC 3492's adapt, inlined)
                delta = if is_first { delta / 700 } else { delta / 2 };
                is_first = false;
                delta += delta / processed;
                bias = 0;
                while delta > 35 * 13 {
                    delta /= 35;
                    bias -= 36;
                }
                bias -= (36 * delta / (delta + 38)) as i32;
                delta = 0;
            }
        }

        delta = delta.wrapping_add(1);
        n = next_n;
    }
}
