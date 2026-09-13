//! Port of the `td/mtproto/RSA.h` interface (S58). The implementation
//! (PEM parsing, fingerprint, raw modular exponentiation) landed in
//! `tdutils::rsa::Rsa` with S26; this module provides the MTProto-layer
//! `PublicRsaKeyInterface` the auth handshake consumes.

pub use tdutils::rsa::Rsa;
use tdutils::status::Result;

/// `td::mtproto::PublicRsaKeyInterface::RsaKey`.
pub struct RsaKey {
    pub rsa: Rsa,
    pub fingerprint: i64,
}

/// `td::mtproto::PublicRsaKeyInterface` — resolves the server-provided
/// fingerprint list to a usable RSA key.
pub trait PublicRsaKeyInterface {
    fn get_rsa_key(&self, fingerprints: &[i64]) -> Result<RsaKey>;
    fn drop_keys(&mut self);
}

#[cfg(test)]
mod tests {
    use super::*;
    use tdutils::crypto::sha256;

    /// The `TEST(Mtproto, RSA)` key from `td/test/mtproto.cpp` — a
    /// non-Telegram key pinned by the C++ test's expected values.
    const MTCPP_TEST_KEY: &str = "-----BEGIN RSA PUBLIC KEY-----\n\
MIIBCgKCAQEAr4v4wxMDXIaMOh8bayF/NyoYdpcysn5EbjTIOZC0RkgzsRj3SGlu\n\
52QSz+ysO41dQAjpFLgxPVJoOlxXokaOq827IfW0bGCm0doT5hxtedu9UCQKbE8j\n\
lDOk+kWMXHPZFJKWRgKgTu9hcB3y3Vk+JFfLpq3d5ZB48B4bcwrRQnzkx5GhWOFX\n\
x73ZgjO93eoQ2b/lDyXxK4B4IS+hZhjzezPZTI5upTRbs5ljlApsddsHrKk6jJNj\n\
8Ygs/ps8e6ct82jLXbnndC9s8HjEvDvBPH9IPjv5JUlmHMBFZ5vFQIfbpo0u0+1P\n\
n6bkEi5o7/ifoyVv2pAZTRwppTz0EuXD8QIDAQAB\n\
-----END RSA PUBLIC KEY-----";

    #[test]
    fn matches_the_cpp_rsa_test_vectors() {
        // td/test/mtproto.cpp TEST(Mtproto, RSA):
        //   ASSERT_EQ(-7596991558377038078, rsa.get_fingerprint());
        //   ASSERT_EQ(256u, rsa.size());
        //   rsa.encrypt(pem.substr(0, 256), to);
        //   ASSERT_EQ("U2nJEtB2AgpHrm3HB0yhpTQgb0wbesi9Pv/W1v/vULU=",
        //             base64_encode(sha256(to)));
        let rsa = Rsa::from_pem_public_key(MTCPP_TEST_KEY).unwrap();
        assert_eq!(rsa.get_fingerprint(), -7_596_991_558_377_038_078i64);
        assert_eq!(rsa.size(), 256);

        // The C++ encrypts the first 256 bytes of the raw PEM text (with
        // newlines and the BEGIN line), not the base64 body.
        let mut from = [0u8; 256];
        from.copy_from_slice(&MTCPP_TEST_KEY.as_bytes()[..256]);
        let mut to = [0u8; 256];
        assert!(rsa.encrypt(&from, &mut to));
        use base64::Engine;
        let expected = base64::engine::general_purpose::STANDARD.encode(sha256(&to));
        assert_eq!(expected, "U2nJEtB2AgpHrm3HB0yhpTQgb0wbesi9Pv/W1v/vULU=");
    }

    #[test]
    #[should_panic]
    fn encrypt_requires_256_byte_buffers() {
        let rsa = Rsa::from_pem_public_key(MTCPP_TEST_KEY).unwrap();
        let mut out = [0u8; 128];
        let _ = rsa.encrypt(&[0u8; 128], &mut out);
    }
}
