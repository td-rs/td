//! Port of `td/mtproto/ProxySecret.{h,cpp}`: the MTProxy secret — 16 raw
//! bytes for classic obfuscation, or a `dd`/`ee` prefixed extended secret
//! (`dd` = random padding, `ee` = TLS emulation plus a domain).

use crate::utils;
use tdutils::status::{Result, Status};

/// `ProxySecret::MAX_DOMAIN_LENGTH`: must be small enough to not overflow
/// TLS-hello length.
pub const MAX_DOMAIN_LENGTH: usize = 182;

/// `td::mtproto::ProxySecret`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ProxySecret {
    secret: Vec<u8>,
}

impl ProxySecret {
    /// `ProxySecret::from_link`: hex, base64url or plain base64.
    pub fn from_link(encoded_secret: &[u8], truncate_if_needed: bool) -> Result<Self> {
        let decoded = match utils::hex_decode(encoded_secret) {
            Result::Ok(decoded) => decoded,
            Result::Err(_) => match utils::base64url_decode(encoded_secret) {
                Result::Ok(decoded) => decoded,
                Result::Err(_) => match utils::base64_decode(encoded_secret) {
                    Result::Ok(decoded) => decoded,
                    Result::Err(_) => {
                        return Result::Err(Status::error(400, "Wrong proxy secret"));
                    }
                },
            },
        };
        Self::from_binary(&decoded, truncate_if_needed)
    }

    /// `ProxySecret::from_binary`.
    pub fn from_binary(mut raw_unchecked_secret: &[u8], truncate_if_needed: bool) -> Result<Self> {
        if raw_unchecked_secret.len() > 17 + MAX_DOMAIN_LENGTH {
            if truncate_if_needed {
                raw_unchecked_secret = &raw_unchecked_secret[..17 + MAX_DOMAIN_LENGTH];
            } else {
                return Result::Err(Status::error(400, "Too long secret"));
            }
        }
        if raw_unchecked_secret.len() == 16
            || (raw_unchecked_secret.len() == 17 && raw_unchecked_secret[0] == 0xdd)
            || (raw_unchecked_secret.len() >= 18 && raw_unchecked_secret[0] == 0xee)
        {
            return Result::Ok(Self::from_raw(raw_unchecked_secret.to_vec()));
        }
        if raw_unchecked_secret.len() < 16 {
            return Result::Err(Status::error(400, "Wrong proxy secret"));
        }
        Result::Err(Status::error(400, "Unsupported proxy secret"))
    }

    /// `ProxySecret::from_raw`.
    pub fn from_raw(secret: Vec<u8>) -> Self {
        Self { secret }
    }

    pub fn get_raw_secret(&self) -> &[u8] {
        &self.secret
    }

    /// `ProxySecret::get_proxy_secret`: the 16 key bytes of an extended
    /// secret, or the whole secret for the classic form.
    pub fn get_proxy_secret(&self) -> &[u8] {
        if self.secret.len() >= 17 {
            &self.secret[1..17]
        } else {
            &self.secret
        }
    }

    pub fn get_encoded_secret(&self) -> Vec<u8> {
        if self.emulate_tls() {
            utils::base64url_encode(&self.secret)
        } else {
            utils::hex_encode(&self.secret)
        }
    }

    pub fn use_random_padding(&self) -> bool {
        self.secret.len() >= 17
    }

    pub fn emulate_tls(&self) -> bool {
        self.secret.len() >= 17 && self.secret[0] == 0xee
    }

    pub fn get_domain(&self) -> &[u8] {
        assert!(self.emulate_tls());
        &self.secret[17..]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classic_16_byte_secret_roundtrips() {
        let secret = ProxySecret::from_binary(&[1u8; 16], false).unwrap();
        assert!(!secret.use_random_padding());
        assert!(!secret.emulate_tls());
        assert_eq!(secret.get_proxy_secret(), &[1u8; 16]);
        // hex form is the canonical encoding.
        assert_eq!(
            secret.get_encoded_secret(),
            b"01010101010101010101010101010101"
        );
        let parsed = ProxySecret::from_link(&secret.get_encoded_secret(), false).unwrap();
        assert_eq!(parsed, secret);
    }

    #[test]
    fn dd_secret_enables_padding() {
        let mut raw = vec![0xdd];
        raw.extend_from_slice(&[2u8; 16]);
        let secret = ProxySecret::from_binary(&raw, false).unwrap();
        assert!(secret.use_random_padding());
        assert!(!secret.emulate_tls());
        assert_eq!(secret.get_proxy_secret(), &[2u8; 16]);
    }

    #[test]
    fn ee_secret_enables_tls_and_domain() {
        let mut raw = vec![0xee];
        raw.extend_from_slice(&[3u8; 16]);
        raw.extend_from_slice(b"www.google.com");
        let secret = ProxySecret::from_binary(&raw, false).unwrap();
        assert!(secret.emulate_tls());
        assert_eq!(secret.get_domain(), b"www.google.com");
        // TLS secrets encode as base64url.
        let encoded = secret.get_encoded_secret();
        let parsed = ProxySecret::from_link(&encoded, false).unwrap();
        assert_eq!(parsed, secret);
    }

    #[test]
    fn bad_secrets_are_rejected() {
        assert!(ProxySecret::from_binary(&[1u8; 15], false).is_error());
        assert!(ProxySecret::from_binary(&[1u8; 17], false).is_error());
        let mut long = vec![0xee];
        long.extend_from_slice(&[1u8; 16]);
        long.extend_from_slice(&[b'x'; 183]);
        assert!(ProxySecret::from_binary(&long, false).is_error());
        assert!(ProxySecret::from_binary(&long, true).is_ok());
        assert!(ProxySecret::from_link(b"not-a-secret!!", false).is_error());
    }
}
