//! `tde2e_core::{MessageEncryption, Keys, KeyChain}` — the key-management
//! subset of `tde2e/td/e2e` the TonWalletManager secret-phrase plane rides
//! on (`e2e_api.cpp`, `MessageEncryption.cpp`, `Keys.cpp`, `Container.h` at
//! the pinned td). The full e2e surface (mnemonics, the blockchain, the
//! QR handshake, the encrypted storage) stays unported; only the six
//! entry points the wallet manager calls exist:
//! `key_generate_temporary_private_key`, `key_to_public_key`,
//! `key_from_public_key`, `key_from_ecdh`, `encrypt_message_for_one`,
//! `decrypt_message_for_one`.
//!
//! The container semantics are the C++ ones: ids are process-local
//! sequential handles, deduplicated by `to_hash` — the first 32 bytes of
//! `HMAC-SHA-512(tag, serialization)` — so the same key material resolves
//! to the same id, exactly like `Container::try_build`.

use std::collections::HashMap;
use std::sync::Mutex;
use tdutils::ed25519;
use tdutils::status::{Result, Status};

/// `MessageEncryption`'s MIN_PADDING (MessageEncryption.cpp:19).
const MIN_PADDING: usize = 16;

/// `td::AesCbcState` — the 32-byte key + 16-byte IV pair the hash derives
/// (`MessageEncryption::calc_aes_cbc_state_from_hash`).
struct AesCbcState {
    key: Vec<u8>,
    iv: [u8; 16],
}

impl AesCbcState {
    /// `calc_aes_cbc_state_from_hash` — key = hash[0..32], IV = hash[32..48].
    fn from_hash(hash: &[u8]) -> AesCbcState {
        assert!(hash.len() >= 48);
        AesCbcState {
            key: hash[..32].to_vec(),
            iv: hash[32..48].try_into().expect("16 IV bytes"),
        }
    }

    /// `AesCbcState::encrypt` — raw CBC over the block-aligned data.
    fn encrypt(&self, data: &[u8]) -> Vec<u8> {
        let mut out = vec![0u8; data.len()];
        let mut iv = self.iv;
        tdutils::crypto::aes_cbc_encrypt(&self.key, &mut iv, data, &mut out);
        out
    }

    /// `AesCbcState::decrypt`.
    fn decrypt(&self, data: &[u8]) -> Vec<u8> {
        let mut out = vec![0u8; data.len()];
        let mut iv = self.iv;
        tdutils::crypto::aes_cbc_decrypt(&self.key, &mut iv, data, &mut out);
        out
    }
}

/// `MessageEncryption::kdf_expand` — `HMAC-SHA-512(secret, info)`.
fn kdf_expand(secret: &[u8], info: &[u8]) -> Vec<u8> {
    tdutils::crypto::hmac_sha512(secret, info).to_vec()
}

/// `MessageEncryption::gen_random_prefix` — the block-aligning random
/// padding whose first byte carries its own size.
fn gen_random_prefix(data_size: usize, min_padding: usize) -> Vec<u8> {
    let size = ((min_padding + 15 + data_size) & !15) - data_size;
    let mut buff = vec![0u8; size];
    tdutils::random::secure_bytes(&mut buff);
    buff[0] = size as u8;
    buff
}

/// `MessageEncryption::encrypt_data_with_prefix` — the wire format:
/// `msg_id(16) || AES-CBC(prefix || data)` where `msg_id` is the first 16
/// bytes of `HMAC-SHA-256(hmac_secret, data || extra || extra_len_le32)`
/// and the AES state comes from `HMAC-SHA-512(encrypt_secret, msg_id)`.
fn encrypt_data_with_prefix(data: &[u8], secret: &[u8], extra: &[u8]) -> Vec<u8> {
    assert!(data.len().is_multiple_of(16));
    let large_secret = kdf_expand(secret, b"tde2e_encrypt_data");
    let encrypt_secret = &large_secret[..32];
    let hmac_secret = &large_secret[32..64];

    let mut tail_data = Vec::with_capacity(data.len() + extra.len() + 4);
    tail_data.extend_from_slice(data);
    tail_data.extend_from_slice(extra);
    tail_data.extend_from_slice(&(extra.len() as i32).to_le_bytes());
    let large_msg_id = tdutils::crypto::hmac_sha256(hmac_secret, &tail_data);
    let msg_id = &large_msg_id[..16];

    let cbc_state = AesCbcState::from_hash(&tdutils::crypto::hmac_sha512(encrypt_secret, msg_id));
    let mut res = Vec::with_capacity(data.len() + 16);
    res.extend_from_slice(msg_id);
    res.extend_from_slice(&cbc_state.encrypt(data));
    res
}

/// `MessageEncryption::encrypt_data` — the random-prefixed form.
pub fn encrypt_data(data: &[u8], secret: &[u8]) -> Vec<u8> {
    let prefix = gen_random_prefix(data.len(), MIN_PADDING);
    let mut combined = prefix;
    combined.extend_from_slice(data);
    encrypt_data_with_prefix(&combined, secret, &[])
}

/// `MessageEncryption::decrypt_data` (MessageEncryption.cpp:107-147) — the
/// inverse with the constant-time `msg_id` check and the prefix-size gate.
pub fn decrypt_data(encrypted_data: &[u8], secret: &[u8], extra: &[u8]) -> Result<Vec<u8>> {
    if encrypted_data.len() < 16 {
        return Result::Err(Status::error_message(
            "Failed to decrypt: encrypted_data is less than 16 bytes",
        ));
    }
    if !encrypted_data.len().is_multiple_of(16) {
        return Result::Err(Status::error_message(
            "Failed to decrypt: data size is not divisible by 16",
        ));
    }

    let large_secret = kdf_expand(secret, b"tde2e_encrypt_data");
    let encrypt_secret = &large_secret[..32];
    let hmac_secret = &large_secret[32..64];

    let msg_id = &encrypted_data[..16];
    let encrypted_data = &encrypted_data[16..];

    let cbc_state = AesCbcState::from_hash(&tdutils::crypto::hmac_sha512(encrypt_secret, msg_id));
    let decrypted_data = cbc_state.decrypt(encrypted_data);

    // The MAC covers the decrypted tail: decrypted || extra || extra_len.
    let mut buf = Vec::with_capacity(decrypted_data.len() + extra.len() + 4);
    buf.extend_from_slice(&decrypted_data);
    buf.extend_from_slice(extra);
    buf.extend_from_slice(&(extra.len() as i32).to_le_bytes());

    let expected_large_msg_id = tdutils::crypto::hmac_sha256(hmac_secret, &buf);
    let expected_msg_id = &expected_large_msg_id[..16];

    // check hash (the C++ OR-fold)
    let mut is_mac_bad = 0u8;
    for i in 0..16 {
        is_mac_bad |= expected_msg_id[i] ^ msg_id[i];
    }
    if is_mac_bad != 0 {
        return Result::Err(Status::error_message("Failed to decrypt: msg_id mismatch"));
    }

    let prefix_size = decrypted_data[0] as usize;
    if prefix_size > decrypted_data.len() || prefix_size < MIN_PADDING {
        return Result::Err(Status::error_message(
            "Failed to decrypt: invalid prefix size",
        ));
    }

    Result::Ok(decrypted_data[prefix_size..].to_vec())
}

/// `tde2e_core::PrivateKey::compute_shared_secret` (Keys.cpp:130-134) —
/// the X25519 shared secret folded through the domain-tagged HMAC, first
/// 32 bytes.
fn compute_shared_secret(
    public_key: &ed25519::PublicKey,
    private_key: &ed25519::PrivateKey,
) -> Result<Vec<u8>> {
    let shared = match ed25519::compute_shared_secret(public_key, private_key) {
        Result::Ok(shared) => shared,
        Result::Err(_) => {
            return Result::Err(Status::error_message("Failed to calculate shared secret"));
        }
    };
    let hmac = tdutils::crypto::hmac_sha512(b"tde2e_shared_secret", &shared);
    Result::Ok(hmac[..32].to_vec())
}

/// `to_hash` (e2e_api.cpp:51-56) — the container dedup key.
fn to_hash(tag: &[u8], serialization: &[u8]) -> [u8; 32] {
    let res = tdutils::crypto::hmac_sha512(tag, serialization);
    res[..32].try_into().expect("32 hash bytes")
}

/// A stored key of the chain: one of the three `TypeStorage` kinds of the
/// C++ container.
#[derive(Clone)]
enum StoredKey {
    /// `PrivateKeyWithMnemonic` — the temporary keys carry no words.
    PrivateKey(ed25519::PrivateKey),
    PublicKey(ed25519::PublicKey),
    /// the derived/loaded symmetric secret.
    Secret(Vec<u8>),
}

/// The process-global `KeyChain` (`get_default_keychain`).
struct KeyChain {
    values: HashMap<i64, StoredKey>,
    hash_to_id: HashMap<[u8; 32], i64>,
    next_id: i64,
}

impl KeyChain {
    /// `Container::try_build` — the hash dedup before and after the build.
    fn try_build(
        &mut self,
        hash: Option<[u8; 32]>,
        build: impl FnOnce() -> Result<StoredKey>,
    ) -> Result<i64> {
        if let Some(hash) = hash
            && let Some(id) = self.hash_to_id.get(&hash)
        {
            return Result::Ok(*id);
        }
        let value = match build() {
            Result::Ok(value) => value,
            Result::Err(status) => return Result::Err(status),
        };
        let id = self.next_id;
        self.next_id += 1;
        self.values.insert(id, value);
        if let Some(hash) = hash {
            self.hash_to_id.insert(hash, id);
        }
        Result::Ok(id)
    }

    fn get(&self, id: i64) -> Result<&StoredKey> {
        match self.values.get(&id) {
            Some(key) => Result::Ok(key),
            None => Result::Err(Status::error_message(&format!("Invalid identifier = {id}"))),
        }
    }
}

static KEYCHAIN: Mutex<Option<KeyChain>> = Mutex::new(None);

fn with_keychain<T>(f: impl FnOnce(&mut KeyChain) -> Result<T>) -> Result<T> {
    let mut guard = KEYCHAIN.lock().expect("the tde2e keychain lock");
    let chain = guard.get_or_insert_with(|| KeyChain {
        values: HashMap::new(),
        hash_to_id: HashMap::new(),
        next_id: 1,
    });
    f(chain)
}

/// The public id types (`e2e_api.h:93-94`).
pub type PrivateKeyId = i64;
pub type PublicKeyId = i64;
pub type SymmetricKeyId = i64;

/// `key_generate_temporary_private_key` (e2e_api.cpp:625-627 over
/// 77-83) — a fresh Ed25519 keypair keyed by its public key.
pub fn key_generate_temporary_private_key() -> Result<PrivateKeyId> {
    let private_key = match ed25519::generate_private_key() {
        Result::Ok(key) => key,
        Result::Err(status) => return Result::Err(status),
    };
    let public_bytes = match private_key.get_public_key() {
        Result::Ok(key) => key.as_octet_string(),
        Result::Err(status) => return Result::Err(status),
    };
    let hash = to_hash(b"temporary private key", &public_bytes);
    with_keychain(|chain| {
        chain.try_build(Some(hash), || {
            Result::Ok(StoredKey::PrivateKey(private_key))
        })
    })
}

/// `key_to_public_key` — the raw 32-byte public key of a stored private
/// key (the C++ wraps it in the `e2e_api::PublicKey` value object).
pub fn key_to_public_key(private_key_id: PrivateKeyId) -> Result<Vec<u8>> {
    with_keychain(|chain| match chain.get(private_key_id) {
        Result::Ok(StoredKey::PrivateKey(private_key)) => match private_key.get_public_key() {
            Result::Ok(key) => Result::Ok(key.as_octet_string()),
            Result::Err(status) => Result::Err(status),
        },
        Result::Ok(_) => Result::Err(Status::error_message("Invalid private key identifier")),
        Result::Err(status) => Result::Err(status),
    })
}

/// `key_from_public_key` (e2e_api.cpp:653-655 over 143-147).
pub fn key_from_public_key(public_key: &[u8]) -> Result<PublicKeyId> {
    let key = match ed25519::PublicKey::from_slice(public_key) {
        Result::Ok(key) => key,
        Result::Err(_) => {
            return Result::Err(Status::error_message("Invalid length of public key"));
        }
    };
    let hash = to_hash(b"public ed25519 key", public_key);
    with_keychain(|chain| chain.try_build(Some(hash), || Result::Ok(StoredKey::PublicKey(key))))
}

/// `key_from_ecdh` (e2e_api.cpp:657-659 over 149-158) — the shared secret
/// keyed by the public-key pair serialization (other ‖ own).
pub fn key_from_ecdh(
    private_key_id: PrivateKeyId,
    other_public_key_id: PublicKeyId,
) -> Result<SymmetricKeyId> {
    let private_key = match with_keychain(|chain| match chain.get(private_key_id) {
        Result::Ok(StoredKey::PrivateKey(private_key)) => Result::Ok(private_key.clone()),
        Result::Ok(_) => Result::Err(Status::error_message("Invalid private key identifier")),
        Result::Err(status) => Result::Err(status),
    }) {
        Result::Ok(key) => key,
        Result::Err(status) => return Result::Err(status),
    };
    let other_public_bytes = match with_keychain(|chain| match chain.get(other_public_key_id) {
        Result::Ok(StoredKey::PublicKey(public_key)) => Result::Ok(public_key.as_octet_string()),
        Result::Ok(_) => Result::Err(Status::error_message("Invalid public key identifier")),
        Result::Err(status) => Result::Err(status),
    }) {
        Result::Ok(bytes) => bytes,
        Result::Err(status) => return Result::Err(status),
    };
    let own_public_bytes = match private_key.get_public_key() {
        Result::Ok(key) => key.as_octet_string(),
        Result::Err(status) => return Result::Err(status),
    };
    let hash = to_hash(
        b"x25519 shared secret",
        &[other_public_bytes.clone(), own_public_bytes].concat(),
    );
    let other_public = match ed25519::PublicKey::from_slice(&other_public_bytes) {
        Result::Ok(key) => key,
        Result::Err(_) => return Result::Err(Status::error_message("Invalid public key")),
    };
    let secret = match compute_shared_secret(&other_public, &private_key) {
        Result::Ok(secret) => secret,
        Result::Err(status) => return Result::Err(status),
    };
    with_keychain(|chain| chain.try_build(Some(hash), || Result::Ok(StoredKey::Secret(secret))))
}

/// `encrypt_message_for_one` (e2e_api.cpp:697-699 over 240-244).
pub fn encrypt_message_for_one(key_id: SymmetricKeyId, message: &[u8]) -> Result<Vec<u8>> {
    with_keychain(|chain| match chain.get(key_id) {
        Result::Ok(StoredKey::Secret(secret)) => Result::Ok(encrypt_data(message, secret)),
        Result::Ok(_) => Result::Err(Status::error_message("Invalid secret key identifier")),
        Result::Err(status) => Result::Err(status),
    })
}

/// `decrypt_message_for_one` (e2e_api.cpp:701-703 over 246-250).
pub fn decrypt_message_for_one(
    key_id: SymmetricKeyId,
    encrypted_message: &[u8],
) -> Result<Vec<u8>> {
    with_keychain(|chain| match chain.get(key_id) {
        Result::Ok(StoredKey::Secret(secret)) => decrypt_data(encrypted_message, secret, &[]),
        Result::Ok(_) => Result::Err(Status::error_message("Invalid secret key identifier")),
        Result::Err(status) => Result::Err(status),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_data_round_trips_over_the_wire_format() {
        let secret = b"0123456789abcdef0123456789abcdef";
        let data = b"0123456789abcdef0123456789abcdef0123456789abcdef";
        let encrypted = encrypt_data(data, secret);
        // msg_id(16) + the padded plaintext blocks — the format pins
        // (MIN_PADDING 16 aligns 48 bytes to 64).
        assert_eq!(encrypted.len() % 16, 0);
        assert!(encrypted.len() >= 16 + data.len() + MIN_PADDING);
        match decrypt_data(&encrypted, secret, &[]) {
            Result::Ok(decrypted) => assert_eq!(decrypted, data.to_vec()),
            Result::Err(status) => panic!("decrypt failed: {}", status.message()),
        }
    }

    #[test]
    fn decrypt_rejects_tampering_and_bad_sizes() {
        let secret = b"0123456789abcdef0123456789abcdef";
        let encrypted = encrypt_data(b"0123456789abcdef0123456789abcdef", secret);
        let mut tampered = encrypted.clone();
        tampered[3] ^= 1;
        assert!(decrypt_data(&tampered, secret, &[]).is_error());
        // A wrong secret fails the msg_id check.
        assert!(decrypt_data(&encrypted, b"another0123456789abcdef01234567", &[]).is_error());
        assert!(decrypt_data(&encrypted[..15], secret, &[]).is_error());
        assert!(decrypt_data(&encrypted[..20], secret, &[]).is_error());
    }

    #[test]
    fn ecdh_matches_both_directions_and_dedups() {
        let alice = key_generate_temporary_private_key().unwrap();
        let bob = key_generate_temporary_private_key().unwrap();
        let alice_public = key_to_public_key(alice).unwrap();
        let bob_public = key_to_public_key(bob).unwrap();
        assert_eq!(alice_public.len(), 32);

        let bob_id = key_from_public_key(&bob_public).unwrap();
        let alice_id = key_from_public_key(&alice_public).unwrap();
        let alice_bob = key_from_ecdh(alice, bob_id).unwrap();
        let bob_alice = key_from_ecdh(bob, alice_id).unwrap();

        let message = b"0123456789abcdef0123456789abcdef";
        let encrypted = encrypt_message_for_one(alice_bob, message).unwrap();
        match decrypt_message_for_one(bob_alice, &encrypted) {
            Result::Ok(decrypted) => assert_eq!(decrypted, message.to_vec()),
            Result::Err(status) => panic!("decrypt failed: {}", status.message()),
        }

        // The same pair resolves to the same id (the container dedup).
        let again = key_from_ecdh(alice, key_from_public_key(&bob_public).unwrap()).unwrap();
        assert_eq!(alice_bob, again);
    }

    #[test]
    fn invalid_ids_answer_errors() {
        assert!(key_to_public_key(987654).is_error());
        assert!(key_from_public_key(&[1u8; 31]).is_error());
        assert!(encrypt_message_for_one(987654, b"x").is_error());
        assert!(decrypt_message_for_one(987654, b"x").is_error());
    }
}
