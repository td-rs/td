//! Port of `td/utils/Ed25519.h` + `Ed25519.cpp` on `ed25519-dalek` (plus
//! `x25519-dalek` for the Ed25519→X25519 shared-secret conversion).
//!
//! PEM support uses the RFC 8410 PKCS#8 encoding; modern OpenSSL (1.1.1+)
//! accepts that format, which is what tdlib's PEM round-trips run through.

use crate::status::{Result, Status};

use ed25519_dalek as dalek;

/// `td::Ed25519::PublicKey` — a 32-byte verification key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicKey {
    octet_string: Vec<u8>,
}

impl PublicKey {
    pub const LENGTH: usize = 32;

    /// `td::Ed25519::PublicKey::from_slice` — fails on the wrong size with
    /// "Invalid slice size".
    pub fn from_slice(slice: &[u8]) -> Result<Self> {
        if slice.len() != Self::LENGTH {
            return Result::Err(Status::error_message("Invalid slice size"));
        }
        Result::Ok(Self {
            octet_string: slice.to_vec(),
        })
    }

    /// `td::Ed25519::PublicKey::as_octet_string`.
    pub fn as_octet_string(&self) -> Vec<u8> {
        self.octet_string.clone()
    }

    /// `td::Ed25519::PublicKey::as_uint256` — the bytes, checked to be 32.
    pub fn as_uint256(&self) -> [u8; 32] {
        self.octet_string
            .clone()
            .try_into()
            .expect("Ed25519 public key is 32 bytes")
    }

    /// `td::Ed25519::PublicKey::verify_signature`.
    pub fn verify_signature(&self, data: &[u8], signature: &[u8]) -> Status {
        if signature.len() != dalek::Signature::BYTE_SIZE {
            return Status::error_message("Invalid signature length");
        }
        let key = dalek::VerifyingKey::from_bytes(&self.as_uint256());
        let signature = dalek::Signature::from_slice(signature).expect("length checked above");
        let verify = key.and_then(|key| key.verify_strict(data, &signature));
        match verify {
            Ok(()) => Status::ok(),
            Err(_) => Status::error_message("Invalid signature"),
        }
    }
}

/// `td::Ed25519::PrivateKey` — a 32-byte seed.
#[derive(Clone, Debug)]
pub struct PrivateKey {
    octet_string: Vec<u8>,
}

/// `td::Ed25519::PreparedPrivateKey` — the parsed signing key, prepared once
/// for repeated signing.
pub struct PreparedPrivateKey {
    signing_key: dalek::SigningKey,
}

impl PrivateKey {
    pub const LENGTH: usize = 32;

    /// `td::Ed25519::PrivateKey(SecureString)`.
    pub fn from_octet_string(octet_string: Vec<u8>) -> Result<Self> {
        if octet_string.len() != Self::LENGTH {
            return Result::Err(Status::error_message("Invalid private key length"));
        }
        Result::Ok(Self { octet_string })
    }

    /// `td::Ed25519::PrivateKey::prepare`.
    pub fn prepare(&self) -> Result<std::sync::Arc<PreparedPrivateKey>> {
        let seed: [u8; 32] = self
            .octet_string
            .clone()
            .try_into()
            .expect("32-byte private key checked at construction");
        Result::Ok(std::sync::Arc::new(PreparedPrivateKey {
            signing_key: dalek::SigningKey::from_bytes(&seed),
        }))
    }

    /// `td::Ed25519::PrivateKey::as_octet_string`.
    pub fn as_octet_string(&self) -> Vec<u8> {
        self.octet_string.clone()
    }

    /// `td::Ed25519::PrivateKey::get_public_key`.
    pub fn get_public_key(&self) -> Result<PublicKey> {
        crate::try_result!(signing_key, self.prepare());
        Result::Ok(PublicKey {
            octet_string: signing_key.signing_key.verifying_key().to_bytes().to_vec(),
        })
    }

    /// `td::Ed25519::PrivateKey::sign(data)`.
    pub fn sign(&self, data: &[u8]) -> Result<Vec<u8>> {
        crate::try_result!(signing_key, self.prepare());
        Self::sign_prepared(&signing_key, data)
    }

    /// `td::Ed25519::PrivateKey::sign(prepared, data)`.
    pub fn sign_prepared(prepared: &PreparedPrivateKey, data: &[u8]) -> Result<Vec<u8>> {
        use dalek::Signer as _;
        Result::Ok(prepared.signing_key.sign(data).to_bytes().to_vec())
    }

    /// `td::Ed25519::PrivateKey::as_pem(password)` — RFC 8410 PKCS#8 PEM.
    /// The C++ wraps OpenSSL's PKCS#8 (which may encrypt with `password`);
    /// the RFC 8410 form is the unencrypted one OpenSSL 1.1.1+ also reads.
    pub fn as_pem(&self, password: &[u8]) -> Result<String> {
        if !password.is_empty() {
            return Result::Err(Status::error_message(
                "Encrypted Ed25519 PEM is not supported",
            ));
        }
        crate::try_result!(signing_key, self.prepare());
        use pkcs8::EncodePrivateKey as _;
        match signing_key.signing_key.to_pkcs8_pem(pkcs8::LineEnding::LF) {
            Ok(pem) => Result::Ok(pem.to_string()),
            Err(error) => Result::Err(Status::error_message(&format!(
                "Failed to encode private key: {error}"
            ))),
        }
    }

    /// `td::Ed25519::PrivateKey::from_pem(pem, password)`.
    pub fn from_pem(pem: &str, password: &[u8]) -> Result<Self> {
        if !password.is_empty() {
            return Result::Err(Status::error_message(
                "Encrypted Ed25519 PEM is not supported",
            ));
        }
        use pkcs8::DecodePrivateKey as _;
        match dalek::SigningKey::from_pkcs8_pem(pem) {
            Ok(signing_key) => Self::from_octet_string(signing_key.to_bytes().to_vec()),
            Err(error) => Result::Err(Status::error_message(&format!(
                "Failed to parse private key: {error}"
            ))),
        }
    }
}

/// `td::Ed25519::generate_private_key`.
pub fn generate_private_key() -> Result<PrivateKey> {
    // `drop(csprng)` zeroizes the key material on drop, so the seed is
    // copied out before it is released.
    let csprng = dalek::SigningKey::generate(&mut rand_core::OsRng);
    let bytes = csprng.to_bytes().to_vec();
    drop(csprng);
    PrivateKey::from_octet_string(bytes)
}

/// `td::Ed25519::get_public_key(private_key)`.
pub fn get_public_key(private_key: &[u8]) -> Result<Vec<u8>> {
    crate::try_result!(
        private_key,
        PrivateKey::from_octet_string(private_key.to_vec())
    );
    crate::try_result!(public_key, private_key.get_public_key());
    Result::Ok(public_key.as_octet_string())
}

/// `td::Ed25519::compute_shared_secret` — converts the Ed25519 keys to
/// X25519 (Montgomery) form and computes the shared secret.
pub fn compute_shared_secret(public_key: &PublicKey, private_key: &PrivateKey) -> Result<Vec<u8>> {
    crate::try_result!(signing_key, private_key.prepare());
    let verifying = match dalek::VerifyingKey::from_bytes(&public_key.as_uint256()) {
        Ok(verifying) => verifying,
        Err(error) => {
            return Result::Err(Status::error_message(&format!(
                "Invalid public key: {error}"
            )));
        }
    };
    let montgomery: [u8; 32] = verifying.to_montgomery().to_bytes();
    let shared = x25519_dalek::x25519(signing_key.signing_key.to_scalar_bytes(), montgomery);
    Result::Ok(shared.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 8032 TEST 1 vector.
    #[test]
    fn rfc8032_test_vector_1() {
        let seed = hex_decode("9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60");
        let expected_public =
            hex_decode("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a");
        let expected_signature = hex_decode(
            "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b",
        );

        let private = PrivateKey::from_octet_string(seed).unwrap();
        let public = private.get_public_key().unwrap();
        assert_eq!(public.as_octet_string(), expected_public);
        let signature = private.sign(&[]).unwrap();
        assert_eq!(signature, expected_signature);
        assert!(public.verify_signature(&[], &signature).is_ok());
        assert!(
            public
                .verify_signature(b"other data", &signature)
                .is_error()
        );
        let mut corrupted = signature.clone();
        corrupted[0] ^= 1;
        assert!(public.verify_signature(&[], &corrupted).is_error());
    }

    #[test]
    fn public_key_size_and_errors() {
        assert!(PublicKey::from_slice(&[0u8; 31]).is_error());
        assert!(PublicKey::from_slice(&[0u8; 33]).is_error());
        assert_eq!(
            PublicKey::from_slice(&[0u8; 31]).error().message(),
            "Invalid slice size"
        );
        let key = PublicKey::from_slice(&[7u8; 32]).unwrap();
        assert_eq!(key.as_octet_string(), vec![7u8; 32]);
        assert_eq!(key.as_uint256(), [7u8; 32]);
        // A non-canonical point fails verification of any signature.
        assert!(key.verify_signature(b"data", &[0u8; 64]).is_error());
    }

    #[test]
    fn generated_key_sign_verify_and_pem_roundtrip() {
        let private = generate_private_key().unwrap();
        let public = private.get_public_key().unwrap();
        let message = b"ed25519 roundtrip";
        let signature = private.sign(message).unwrap();
        assert_eq!(signature.len(), 64);
        assert!(public.verify_signature(message, &signature).is_ok());
        assert!(public.verify_signature(message, &[0u8; 64]).is_error());

        let pem = private.as_pem(b"").unwrap();
        assert!(pem.starts_with("-----BEGIN PRIVATE KEY-----"));
        let reparsed = PrivateKey::from_pem(&pem, b"").unwrap();
        assert_eq!(reparsed.as_octet_string(), private.as_octet_string());
        assert!(PrivateKey::from_pem("not a pem", b"").is_error());
        assert!(private.as_pem(b"password").is_error());
    }

    #[test]
    fn prepared_signing_matches_direct_signing() {
        let private = generate_private_key().unwrap();
        let prepared = private.prepare().unwrap();
        let message = b"prepared";
        let direct = private.sign(message).unwrap();
        let prepared_signature = PrivateKey::sign_prepared(&prepared, message).unwrap();
        assert_eq!(direct, prepared_signature);
    }

    #[test]
    fn shared_secret_is_symmetric() {
        let alice = generate_private_key().unwrap();
        let bob = generate_private_key().unwrap();
        let alice_public = alice.get_public_key().unwrap();
        let bob_public = bob.get_public_key().unwrap();

        let secret_a = compute_shared_secret(&bob_public, &alice).unwrap();
        let secret_b = compute_shared_secret(&alice_public, &bob).unwrap();
        assert_eq!(secret_a.len(), 32);
        assert_eq!(secret_a, secret_b);
    }

    #[test]
    fn get_public_key_from_bytes_matches() {
        let private = generate_private_key().unwrap();
        let from_bytes = get_public_key(&private.as_octet_string()).unwrap();
        assert_eq!(
            from_bytes,
            private.get_public_key().unwrap().as_octet_string()
        );
    }

    fn hex_decode(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }
}
