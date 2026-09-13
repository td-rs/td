//! Port of `td/mtproto/RSA.h` + `RSA.cpp` (PEM public-key parsing and the
//! raw 256-byte MTProto RSA operations) on the `rsa` crate, plus PKCS#1
//! v1.5 helpers. Replaces OpenSSL's `PEM_read_bio_PUBKEY` / `BN_mod_exp`.
//!
//! The SHA-1-based key fingerprint is wire-mandated by MTProto (servers and
//! clients exchange the fingerprint bytes), so the algorithm cannot change.

use crate::bignum::{BigNum, BigNumContext};
use crate::crypto::sha1;
use crate::status::{Result, Status};

use base64::Engine as _;
use rsa as rsa_crate;
use rsa_crate::pkcs1::DecodeRsaPublicKey;
use rsa_crate::pkcs8::DecodePublicKey;
use rsa_crate::traits::PublicKeyParts;

/// `td::mtproto::RSA` — a 2048-bit RSA public key `(n, e)`.
#[derive(Clone, Debug)]
pub struct Rsa {
    n: BigNum,
    e: BigNum,
}

/// The `mtproto_api::rsa_public_key` constructor id (CRC32 of the canonical
/// string `rsa_public_key n:string e:string = RSAPublicKey`).
const RSA_PUBLIC_KEY_CONSTRUCTOR: u32 = 0x7a19_cb76;

impl Rsa {
    /// `td::mtproto::RSA::RSA(n, e)`.
    pub fn from_raw_parts(n: BigNum, e: BigNum) -> Self {
        Self { n, e }
    }

    /// `td::mtproto::RSA::from_pem_public_key(pem)` — accepts both the
    /// PKCS#1 `RSA PUBLIC KEY` and the `PUBLIC KEY` (SPKI) PEM forms.
    pub fn from_pem_public_key(pem: &str) -> Result<Self> {
        let der = match pem_decode_public_key(pem) {
            Some(der) => der,
            None => {
                return Result::Err(Status::error_message("Error while reading RSA public key"));
            }
        };
        let key = match rsa_crate::RsaPublicKey::from_pkcs1_der(&der)
            .or_else(|_| rsa_crate::RsaPublicKey::from_public_key_der(&der))
        {
            Ok(key) => key,
            Err(_) => {
                // The C++ distinguishes "Key is not an RSA key"; the `rsa`
                // crate rejects non-RSA keys here with the same net effect.
                return Result::Err(Status::error_message("Error while reading RSA public key"));
            }
        };
        if key.size() != 256 {
            return Result::Err(Status::error_message("EVP_PKEY_size != 256"));
        }
        Result::Ok(Self {
            n: BigNum::from_binary(&key.n().to_bytes_be()),
            e: BigNum::from_binary(&key.e().to_bytes_be()),
        })
    }

    /// `td::mtproto::RSA::get_fingerprint()` — the little-endian `i64` read
    /// from bytes `[12, 20)` of the SHA-1 of the TL-serialized
    /// `rsa_public_key` (as tdlib computes it on little-endian platforms).
    pub fn get_fingerprint(&self) -> i64 {
        let n_str = self.n.to_binary(-1);
        let e_str = self.e.to_binary(-1);
        let mut buf = Vec::with_capacity(4 + 4 + n_str.len() + 4 + e_str.len());
        buf.extend_from_slice(&RSA_PUBLIC_KEY_CONSTRUCTOR.to_le_bytes());
        append_tl_string(&mut buf, &n_str);
        append_tl_string(&mut buf, &e_str);
        let key_sha1 = sha1(&buf);
        i64::from_le_bytes(key_sha1[12..20].try_into().expect("8 bytes"))
    }

    /// `td::mtproto::RSA::size()`.
    pub fn size(&self) -> usize {
        256
    }

    /// `td::mtproto::RSA::encrypt(from, to)` — raw modular exponentiation
    /// over 256-byte blocks; returns false when `from >= n` (as in C++).
    pub fn encrypt(&self, from: &[u8], to: &mut [u8]) -> bool {
        assert_eq!(from.len(), 256, "RSA::encrypt needs 256 bytes");
        assert_eq!(to.len(), 256, "RSA::encrypt needs a 256-byte output");
        let bits = self.n.get_num_bits();
        assert!(
            (2041..=2048).contains(&bits),
            "RSA modulus bits out of range"
        );

        let x = BigNum::from_binary(from);
        if BigNum::compare(&x, &self.n) >= 0 {
            return false;
        }
        let mut context = BigNumContext::new();
        let y = BigNum::mod_exp(&x, &self.e, &self.n, &mut context);
        to.copy_from_slice(&y.to_binary(256));
        true
    }

    /// `td::mtproto::RSA::decrypt_signature(from, to)` — the same raw
    /// exponentiation (RSA is an involution over the modulus here).
    pub fn decrypt_signature(&self, from: &[u8], to: &mut [u8]) {
        assert_eq!(from.len(), 256, "RSA::decrypt_signature needs 256 bytes");
        let mut context = BigNumContext::new();
        let x = BigNum::from_binary(from);
        let y = BigNum::mod_exp(&x, &self.e, &self.n, &mut context);
        to.copy_from_slice(&y.to_binary(256));
    }

    /// The `(n, e)` pair as an `rsa`-crate public key, for the padding-aware
    /// helpers below.
    pub fn rsa_public_key(&self) -> rsa_crate::RsaPublicKey {
        let n = self.n.to_binary(-1);
        let e = self.e.to_binary(-1);
        let n = rsa_crate::BigUint::from_bytes_be(&n);
        let e = rsa_crate::BigUint::from_bytes_be(&e);
        // Reconstructing is infallible for 2048-bit (n, e) pairs; a panic
        // here would mean the PEM checks were bypassed.
        rsa_crate::RsaPublicKey::new(n, e).expect("valid 2048-bit RSA key")
    }
}

/// Appends a TL string (`len < 254`: one length byte; otherwise `0xFE` +
/// 3-byte little-endian length).
fn append_tl_string(buf: &mut Vec<u8>, data: &[u8]) {
    if data.len() < 254 {
        buf.push(data.len() as u8);
    } else {
        buf.push(0xFE);
        buf.extend_from_slice(&(data.len() as u32).to_le_bytes()[..3]);
    }
    buf.extend_from_slice(data);
}

/// Extracts the DER body of the first `BEGIN ... PUBLIC KEY` PEM block.
fn pem_decode_public_key(pem: &str) -> Option<Vec<u8>> {
    let mut in_block = false;
    let mut base64_body = String::new();
    for line in pem.lines() {
        let line = line.trim();
        if line.starts_with("-----BEGIN") {
            in_block = true;
            continue;
        }
        if line.starts_with("-----END") {
            break;
        }
        if in_block {
            base64_body.push_str(line);
        }
    }
    if !in_block || base64_body.is_empty() {
        return None;
    }
    base64::engine::general_purpose::STANDARD
        .decode(base64_body.as_bytes())
        .ok()
}

/// PKCS#1 v1.5 encryption with random non-zero padding (the MTProto
/// handshake padding), via the `rsa` crate.
pub fn pkcs1_v1_5_encrypt(public_key: &rsa_crate::RsaPublicKey, data: &[u8]) -> Result<Vec<u8>> {
    use rsa_crate::pkcs1v15::EncryptingKey;
    use rsa_crate::traits::RandomizedEncryptor;
    let encrypting_key = EncryptingKey::new(public_key.clone());
    let mut rng = rsa_crate::rand_core::OsRng;
    match encrypting_key.encrypt_with_rng(&mut rng, data) {
        Ok(value) => Result::Ok(value),
        Err(error) => Result::Err(Status::error_message(&format!(
            "RSA encrypt failed: {error}"
        ))),
    }
}

/// PKCS#1 v1.5 decryption, via the `rsa` crate.
pub fn pkcs1_v1_5_decrypt(private_key: &rsa_crate::RsaPrivateKey, data: &[u8]) -> Result<Vec<u8>> {
    use rsa_crate::pkcs1v15::DecryptingKey;
    use rsa_crate::traits::Decryptor;
    let decrypting_key = DecryptingKey::new(private_key.clone());
    match decrypting_key.decrypt(data) {
        Ok(value) => Result::Ok(value),
        Err(error) => Result::Err(Status::error_message(&format!(
            "RSA decrypt failed: {error}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The current tdlib test-DC main key (`PublicRsaKeySharedMain.cpp`).
    const TEST_DC_KEY: &str = "-----BEGIN RSA PUBLIC KEY-----\n\
MIIBCgKCAQEAyMEdY1aR+sCR3ZSJrtztKTKqigvO/vBfqACJLZtS7QMgCGXJ6XIR\n\
yy7mx66W0/sOFa7/1mAZtEoIokDP3ShoqF4fVNb6XeqgQfaUHd8wJpDWHcR2OFwv\n\
plUUI1PLTktZ9uW2WE23b+ixNwJjJGwBDJPQEQFBE+vfmH0JP503wr5INS1poWg/\n\
j25sIWeYPHYeOrFp/eXaqhISP6G+q2IeTaWTXpwZj4LzXq5YOpk4bYEQ6mvRq7D1\n\
aHWfYmlEGepfaYR8Q0YqvvhYtMte3ITnuSJs171+GDqpdKcSwHnd6FudwGO4pcCO\n\
j4WcDuXc2CTHgH8gFTNhp/Y8/SpDOhvn9QIDAQAB\n\
-----END RSA PUBLIC KEY-----";

    /// The current tdlib main-DC key (`PublicRsaKeySharedMain.cpp`).
    const MAIN_DC_KEY: &str = "-----BEGIN RSA PUBLIC KEY-----\n\
MIIBCgKCAQEA6LszBcC1LGzyr992NzE0ieY+BSaOW622Aa9Bd4ZHLl+TuFQ4lo4g\n\
5nKaMBwK/BIb9xUfg0Q29/2mgIR6Zr9krM7HjuIcCzFvDtr+L0GQjae9H0pRB2OO\n\
62cECs5HKhT5DZ98K33vmWiLowc621dQuwKWSQKjWf50XYFw42h21P2KXUGyp2y/\n\
+aEyZ+uVgLLQbRA1dEjSDZ2iGRy12Mk5gpYc397aYp438fsJoHIgJ2lgMv5h7WY9\n\
t6N/byY9Nw9p21Og3AoXSL2q/2IJ1WRUhebgAdGVMlV1fkuOQoEzR7EdpqtQD9Cs\n\
5+bfo3Nhmcyvk5ftB0WkJ9z6bNZ7yxrP8wIDAQAB\n\
-----END RSA PUBLIC KEY-----";

    /// The SPKI-format key pair from `tdutils/test/crypto.cpp`.
    const CRYPTO_TEST_PUBLIC_KEY: &str = "-----BEGIN PUBLIC KEY-----\n\
MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEA3mE+f6a5ixGtkLWTQ/lM\n\
E5on/JJdlZV2jAo49ZNZSSavAakvHFTNrz5zj8xaIgti2MpIZuJJO4KAgmR+sHs/\n\
cmpINvH3KtGHL9MfAy9SvlShybsCIVX0LP/yGNf3ATqzBEnPjUKMD279bUyBcQTt\n\
oHm56S7rHobfIse17aRguKgGhzdmhL09nEbJwO+kwCtUVcX9TmBlBQQ82N9fFqVW\n\
X1SI3EOyvfnlOm9mG/OFS5byV8N3ta78Pnwc8dhI6Q+M4V8tdWKaVg8lehxG9obN\n\
3Bkvy2x3x73XEn4pdqJmiT/ezAxBGNhxcY0fxOQhH9rLrz81+F0t0RPkt9mhCWmk\n\
GwIDAQAB\n-----END PUBLIC KEY-----";

    #[test]
    fn parses_real_tdlib_keys_and_matches_fingerprints() {
        // Fingerprint values produced by tdlib's own algorithm
        // (tl_store of rsa_public_key, SHA-1, little-endian bytes [12, 20)).
        let test_dc = Rsa::from_pem_public_key(TEST_DC_KEY).unwrap();
        assert_eq!(test_dc.get_fingerprint(), 0x9bf2_59d9_e76c_bb48u64 as i64);
        assert_eq!(test_dc.size(), 256);

        let main_dc = Rsa::from_pem_public_key(MAIN_DC_KEY).unwrap();
        // The publicly documented main-DC key fingerprint.
        assert_eq!(main_dc.get_fingerprint(), 0xd86c_e0c2_d1bb_17b9u64 as i64);

        // SPKI ("PUBLIC KEY") form parses too.
        let spki = Rsa::from_pem_public_key(CRYPTO_TEST_PUBLIC_KEY).unwrap();
        assert_eq!(spki.get_fingerprint(), 0x06df_e90b_6867_69dc);
    }

    #[test]
    fn rejects_invalid_pem() {
        assert!(Rsa::from_pem_public_key("not a pem").is_error());
        assert!(Rsa::from_pem_public_key("").is_error());
        assert!(
            Rsa::from_pem_public_key("-----BEGIN PUBLIC KEY-----\nAAAA\n-----END PUBLIC KEY-----")
                .is_error()
        );
    }

    #[test]
    fn encrypt_rejects_values_at_or_above_the_modulus() {
        let rsa = Rsa::from_pem_public_key(CRYPTO_TEST_PUBLIC_KEY).unwrap();
        let mut out = [0u8; 256];
        // 2^2048 - 1 exceeds any 2048-bit modulus.
        let too_big = [0xFFu8; 256];
        assert!(!rsa.encrypt(&too_big, &mut out));
        // A small value below the modulus succeeds.
        assert!(rsa.encrypt(&[0u8; 256], &mut out));
    }

    /// Generates one 2048-bit keypair for the private-key tests (key
    /// generation is slow, so a single test exercises raw inversion and the
    /// PKCS#1 v1.5 roundtrip).
    #[test]
    fn generated_keypair_raw_inversion_and_pkcs1_v1_5() {
        use rsa_crate::traits::PrivateKeyParts;
        let mut rng = rsa_crate::rand_core::OsRng;
        let private =
            rsa_crate::RsaPrivateKey::new(&mut rng, 2048).expect("2048-bit key generation");
        let public = rsa_crate::RsaPublicKey::from(&private);
        let rsa = Rsa::from_raw_parts(
            BigNum::from_binary(&public.n().to_bytes_be()),
            BigNum::from_binary(&public.e().to_bytes_be()),
        );

        // Raw inversion: x^e mod n through our Rsa, back through d.
        let mut data = [0u8; 256];
        data[..5].copy_from_slice(b"hello");
        data[255] = 0xAB;
        let mut encrypted = [0u8; 256];
        assert!(rsa.encrypt(&data, &mut encrypted));
        assert_ne!(encrypted, data);
        let d = BigNum::from_binary(&private.d().to_bytes_be());
        let n = BigNum::from_binary(&private.n().to_bytes_be());
        let mut context = BigNumContext::new();
        let recovered = BigNum::mod_exp(&BigNum::from_binary(&encrypted), &d, &n, &mut context);
        assert_eq!(recovered.to_binary(256), data);

        // PKCS#1 v1.5 roundtrip and corruption detection.
        let message = b"PKCS#1 v1.5 padding test payload";
        let encrypted = pkcs1_v1_5_encrypt(&public, message).unwrap();
        assert_eq!(encrypted.len(), 256);
        assert_ne!(&encrypted[..message.len()], &message[..]);
        let decrypted = pkcs1_v1_5_decrypt(&private, &encrypted).unwrap();
        assert_eq!(decrypted, message);
        let mut corrupted = encrypted.clone();
        corrupted[128] ^= 0xFF;
        assert!(pkcs1_v1_5_decrypt(&private, &corrupted).is_error());
    }
}
