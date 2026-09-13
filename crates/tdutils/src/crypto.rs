//! Port of the hash family of `td/utils/crypto.h` + `td/utils/crypto.cpp`:
//! SHA-1/256/512, MD5, HMAC, PBKDF2, the streaming `Sha256State`, and the
//! CRC-64 (XZ) / CRC-16 (XMODEM) checksums, on top of the RustCrypto
//! crates. The AES / RSA / BigNum parts land in S24–S26.
//!
//! MD5 and SHA-1 are included for wire compatibility only: the Telegram
//! protocol and tdlib's file formats mandate them (secret-chat fingerprints,
//! legacy MTProto paths), and a drop-in replacement must reproduce the same
//! digests — they are never a choice of security strength here.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256, Sha512};

/// `td::sha1(Slice, unsigned char[20])`.
pub fn sha1(data: &[u8]) -> [u8; 20] {
    let mut hasher = sha1::Sha1::new();
    hasher.update(data);
    hasher.finalize().into()
}

/// `td::sha1(Slice)` — the digest as a binary string (`td::string` is a byte
/// container, so this is a `Vec<u8>`, not necessarily valid UTF-8).
pub fn sha1_bytes(data: &[u8]) -> Vec<u8> {
    sha1(data).to_vec()
}

/// `td::sha256(Slice, MutableSlice)`.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().into()
}

/// `td::sha256(Slice)`.
pub fn sha256_bytes(data: &[u8]) -> Vec<u8> {
    sha256(data).to_vec()
}

/// `td::sha512(Slice, MutableSlice)`.
pub fn sha512(data: &[u8]) -> [u8; 64] {
    let mut hasher = Sha512::new();
    hasher.update(data);
    hasher.finalize().into()
}

/// `td::sha512(Slice)`.
pub fn sha512_bytes(data: &[u8]) -> Vec<u8> {
    sha512(data).to_vec()
}

/// `td::md5(Slice, MutableSlice)`.
pub fn md5(data: &[u8]) -> [u8; 16] {
    let mut hasher = md5::Md5::new();
    hasher.update(data);
    hasher.finalize().into()
}

/// `td::md5(Slice)`.
pub fn md5_bytes(data: &[u8]) -> Vec<u8> {
    md5(data).to_vec()
}

/// `td::hmac_sha256(key, message, dest)`.
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    // Disambiguate from `KeyInit::new_from_slice` (also in scope for AES).
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(message);
    mac.finalize().into_bytes().into()
}

/// `td::hmac_sha512(key, message, dest)`.
pub fn hmac_sha512(key: &[u8], message: &[u8]) -> [u8; 64] {
    let mut mac = <Hmac<Sha512> as Mac>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(message);
    mac.finalize().into_bytes().into()
}

/// `td::pbkdf2_sha256(password, salt, iteration_count, dest)`.
pub fn pbkdf2_sha256(password: &[u8], salt: &[u8], iteration_count: u32, dest: &mut [u8]) {
    pbkdf2::pbkdf2_hmac::<Sha256>(password, salt, iteration_count, dest);
}

/// `td::pbkdf2_sha512(password, salt, iteration_count, dest)`.
pub fn pbkdf2_sha512(password: &[u8], salt: &[u8], iteration_count: u32, dest: &mut [u8]) {
    pbkdf2::pbkdf2_hmac::<Sha512>(password, salt, iteration_count, dest);
}

/// `td::Sha256State` — streaming SHA-256 with init/feed/extract.
#[derive(Default)]
pub struct Sha256State {
    hasher: Sha256,
    is_inited: bool,
}

impl Sha256State {
    pub fn new() -> Self {
        Self::default()
    }

    /// `td::Sha256State::init`.
    pub fn init(&mut self) {
        self.hasher = Sha256::new();
        self.is_inited = true;
    }

    /// `td::Sha256State::feed`.
    pub fn feed(&mut self, data: &[u8]) {
        assert!(self.is_inited, "Sha256State::feed before init");
        self.hasher.update(data);
    }

    /// `td::Sha256State::extract(output, destroy)` — `destroy` resets the
    /// state, like the C++ flag.
    pub fn extract(&mut self, output: &mut [u8], destroy: bool) {
        assert!(self.is_inited, "Sha256State::extract before init");
        assert!(output.len() >= 32, "Sha256State::extract: output too small");
        output[..32].copy_from_slice(&self.hasher.clone().finalize());
        if destroy {
            self.is_inited = false;
        }
    }
}

/// `td::crc64` — CRC-64/XZ: reflected polynomial `0xC96C5795D7870F42`,
/// init/xorout all ones.
pub fn crc64(data: &[u8]) -> u64 {
    static TABLE: std::sync::OnceLock<[u64; 256]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut table = [0u64; 256];
        for (i, entry) in table.iter_mut().enumerate() {
            let mut crc = i as u64;
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xC96C_5795_D787_0F42
                } else {
                    crc >> 1
                };
            }
            *entry = crc;
        }
        table
    });
    let mut crc = u64::MAX;
    for &byte in data {
        crc = table[((crc ^ u64::from(byte)) & 0xff) as usize] ^ (crc >> 8);
    }
    crc ^ u64::MAX
}

/// `td::crc16` — CRC-16/XMODEM: MSB-first polynomial `0x1021`, init/xorout 0.
pub fn crc16(data: &[u8]) -> u16 {
    static TABLE: std::sync::OnceLock<[u16; 256]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut table = [0u16; 256];
        for (i, entry) in table.iter_mut().enumerate() {
            let mut crc = (i as u16) << 8;
            for _ in 0..8 {
                crc = if crc & 0x8000 != 0 {
                    (crc << 1) ^ 0x1021
                } else {
                    crc << 1
                };
            }
            *entry = crc;
        }
        table
    });
    let mut crc = 0u32;
    for &byte in data {
        let t = ((u32::from(byte) ^ (crc >> 8)) & 0xff) as usize;
        crc = u32::from(table[t]) ^ (crc << 8);
    }
    crc as u16
}

// ---------------------------------------------------------------------------
// AES (S24): ECB, CBC, CTR and the hand-rolled IGE of `crypto.cpp`
// ---------------------------------------------------------------------------

use aes::Aes256;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};

const AES_BLOCK_SIZE: usize = 16;

/// `td::AesState` — AES-256 in ECB mode (the MTProto building block).
pub struct AesState {
    cipher: Aes256,
    encrypt: bool,
}

impl AesState {
    pub fn new() -> Self {
        // Key is replaced by `init` before any use, mirroring the C++ class
        // whose `impl_` is null until `init`.
        Self {
            cipher: Aes256::new_from_slice(&[0u8; 32]).expect("AES-256 key size is fixed"),
            encrypt: true,
        }
    }

    /// `td::AesState::init(key, encrypt)`.
    pub fn init(&mut self, key: &[u8], encrypt: bool) {
        assert_eq!(key.len(), 32, "AES key must be 256-bit");
        self.cipher = Aes256::new_from_slice(key).expect("32-byte key checked above");
        self.encrypt = encrypt;
    }

    /// `td::AesState::encrypt(src, dst, size)` — ECB, block-aligned.
    pub fn encrypt(&mut self, src: &[u8], dst: &mut [u8]) {
        assert!(
            src.len().is_multiple_of(AES_BLOCK_SIZE),
            "ECB needs whole blocks"
        );
        assert!(dst.len() >= src.len());
        assert!(self.encrypt, "encrypt() on a decrypt state");
        for (chunk, out) in src
            .chunks_exact(AES_BLOCK_SIZE)
            .zip(dst.chunks_exact_mut(AES_BLOCK_SIZE))
        {
            let mut block = aes::Block::clone_from_slice(chunk);
            self.cipher.encrypt_block(&mut block);
            out.copy_from_slice(&block);
        }
    }

    /// `td::AesState::decrypt(src, dst, size)` — ECB, block-aligned.
    pub fn decrypt(&mut self, src: &[u8], dst: &mut [u8]) {
        assert!(
            src.len().is_multiple_of(AES_BLOCK_SIZE),
            "ECB needs whole blocks"
        );
        assert!(dst.len() >= src.len());
        assert!(!self.encrypt, "decrypt() on an encrypt state");
        for (chunk, out) in src
            .chunks_exact(AES_BLOCK_SIZE)
            .zip(dst.chunks_exact_mut(AES_BLOCK_SIZE))
        {
            let mut block = aes::Block::clone_from_slice(chunk);
            self.cipher.decrypt_block(&mut block);
            out.copy_from_slice(&block);
        }
    }
}

impl Default for AesState {
    fn default() -> Self {
        Self::new()
    }
}

/// `td::AesCtrState` — streaming AES-256-CTR with the 128-bit big-endian
/// counter seeded by the IV. The keystream is byte-continuous across calls,
/// like OpenSSL's EVP CTR `num` state.
pub struct AesCtrState {
    cipher: Aes256,
    counter: [u8; AES_BLOCK_SIZE],
    keystream: [u8; AES_BLOCK_SIZE],
    /// Bytes of the current keystream block already consumed; the next block
    /// is generated when this reaches `AES_BLOCK_SIZE`.
    used: usize,
}

impl AesCtrState {
    pub fn new() -> Self {
        Self {
            cipher: Aes256::new_from_slice(&[0u8; 32]).expect("AES-256 key size is fixed"),
            counter: [0u8; AES_BLOCK_SIZE],
            keystream: [0u8; AES_BLOCK_SIZE],
            used: AES_BLOCK_SIZE,
        }
    }

    /// `td::AesCtrState::init(key, iv)`.
    pub fn init(&mut self, key: &[u8], iv: &[u8]) {
        assert_eq!(key.len(), 32, "AES key must be 256-bit");
        assert_eq!(iv.len(), AES_BLOCK_SIZE, "CTR IV must be 128-bit");
        self.cipher = Aes256::new_from_slice(key).expect("32-byte key checked above");
        self.counter.copy_from_slice(iv);
        self.used = AES_BLOCK_SIZE;
    }

    fn next_keystream_block(&mut self) {
        // First block encrypts the counter as-is (AES(iv)); the 128-bit
        // big-endian increment happens after use, like OpenSSL's CTR.
        let mut block = aes::Block::clone_from_slice(&self.counter);
        self.cipher.encrypt_block(&mut block);
        self.keystream.copy_from_slice(&block);
        for byte in self.counter.iter_mut().rev() {
            *byte = byte.wrapping_add(1);
            if *byte != 0 {
                break;
            }
        }
        self.used = 0;
    }

    fn advance(&mut self, src: &[u8], dst: &mut [u8]) {
        assert!(dst.len() >= src.len());
        let mut offset = 0;
        while offset < src.len() {
            if self.used == AES_BLOCK_SIZE {
                self.next_keystream_block();
            }
            let n = (src.len() - offset).min(AES_BLOCK_SIZE - self.used);
            for i in 0..n {
                dst[offset + i] = src[offset + i] ^ self.keystream[self.used + i];
            }
            self.used += n;
            offset += n;
        }
    }

    /// `td::AesCtrState::encrypt`.
    pub fn encrypt(&mut self, src: &[u8], dst: &mut [u8]) {
        self.advance(src, dst);
    }

    /// `td::AesCtrState::decrypt` (CTR is an involution).
    pub fn decrypt(&mut self, src: &[u8], dst: &mut [u8]) {
        self.advance(src, dst);
    }

    /// `encrypt(payload, payload)` — the in-place form C++ uses everywhere
    /// (transports encrypt their buffers in place). Not expressible as
    /// `encrypt(&x, &mut x)` in safe Rust.
    pub fn encrypt_in_place(&mut self, data: &mut [u8]) {
        // SAFETY: `advance` reads `src[i]` and writes the same `data[i]`;
        // both borrows target one allocation and the loop indexes them in
        // lockstep, so aliasing is sound — the raw-pointer round trip is only
        // needed to satisfy the borrow checker.
        let ptr = data.as_mut_ptr();
        let len = data.len();
        unsafe {
            self.advance(std::slice::from_raw_parts(ptr, len), data);
        }
    }

    /// `decrypt(payload, payload)` — same in-place form.
    pub fn decrypt_in_place(&mut self, data: &mut [u8]) {
        self.encrypt_in_place(data);
    }
}

impl Default for AesCtrState {
    fn default() -> Self {
        Self::new()
    }
}

/// `td::AesIgeState` — the IGE mode of `crypto.cpp`, byte-exact port of
/// `AesIgeStateImpl` (CBC-encrypt of XOR-chained blocks; the iv is 32 bytes:
/// `encrypted_iv_` then `plaintext_iv_`).
pub struct AesIgeState {
    cipher: Aes256,
    decrypt: bool,
    encrypted_iv: [u8; AES_BLOCK_SIZE],
    plaintext_iv: [u8; AES_BLOCK_SIZE],
}

impl AesIgeState {
    pub fn new() -> Self {
        Self {
            cipher: Aes256::new_from_slice(&[0u8; 32]).expect("AES-256 key size is fixed"),
            decrypt: true,
            encrypted_iv: [0u8; AES_BLOCK_SIZE],
            plaintext_iv: [0u8; AES_BLOCK_SIZE],
        }
    }

    /// `td::AesIgeState::init(key, iv, encrypt)`.
    pub fn init(&mut self, key: &[u8], iv: &[u8], encrypt: bool) {
        assert_eq!(key.len(), 32, "AES key must be 256-bit");
        assert_eq!(iv.len(), 32, "IGE IV must be 256-bit");
        self.cipher = Aes256::new_from_slice(key).expect("32-byte key checked above");
        self.decrypt = !encrypt;
        self.encrypted_iv.copy_from_slice(&iv[..AES_BLOCK_SIZE]);
        self.plaintext_iv.copy_from_slice(&iv[AES_BLOCK_SIZE..]);
    }

    /// `td::AesIgeStateImpl::encrypt` — standard IGE. The C++ processes
    /// 31-block chunks, but the mode is chunk-transparent (the CBC chain
    /// resumes from the last *output* block), so one pass is equivalent.
    pub fn encrypt(&mut self, from: &[u8], to: &mut [u8]) {
        assert!(
            from.len().is_multiple_of(AES_BLOCK_SIZE),
            "IGE needs whole blocks"
        );
        assert!(to.len() >= from.len());
        assert!(!self.decrypt, "encrypt() on a decrypt state");
        let out = &mut to[..from.len()];
        let block_count = from.len() / AES_BLOCK_SIZE;
        if block_count == 0 {
            return;
        }

        let mut data_xored = vec![[0u8; AES_BLOCK_SIZE]; block_count];
        for (i, chunk) in from.chunks_exact(AES_BLOCK_SIZE).enumerate() {
            data_xored[i].copy_from_slice(chunk);
        }
        // The original plaintext blocks: the C++ xors the output with
        // `data[i - 1]`, not the already-modified `data_xored[i - 1]`.
        let data = data_xored.clone();
        if block_count > 1 {
            for (byte, plaintext_iv) in data_xored[1].iter_mut().zip(self.plaintext_iv) {
                *byte ^= plaintext_iv;
            }
            for i in 2..block_count {
                for j in 0..AES_BLOCK_SIZE {
                    data_xored[i][j] ^= data[i - 2][j];
                }
            }
        }

        // C++ updates plaintext_iv_ to the last *plaintext* block, so save it
        // before the CBC pass overwrites the data.
        let mut last_plaintext = [0u8; AES_BLOCK_SIZE];
        last_plaintext.copy_from_slice(&from[from.len() - AES_BLOCK_SIZE..]);

        // CBC encrypt in place over data_xored, chained from encrypted_iv.
        let mut chain = self.encrypted_iv;
        for block in &mut data_xored {
            for (b, c) in block.iter_mut().zip(chain) {
                *b ^= c;
            }
            let mut aes_block = aes::Block::clone_from_slice(block);
            self.cipher.encrypt_block(&mut aes_block);
            block.copy_from_slice(&aes_block);
            chain = *block;
        }

        // y[i] = C[i] ^ x[i-1], with x[-1] = plaintext_iv.
        for (byte, plaintext_iv) in data_xored[0].iter_mut().zip(self.plaintext_iv) {
            *byte ^= plaintext_iv;
        }
        for i in 1..block_count {
            for j in 0..AES_BLOCK_SIZE {
                data_xored[i][j] ^= data[i - 1][j];
            }
        }
        self.plaintext_iv = last_plaintext;
        self.encrypted_iv = data_xored[block_count - 1];
        for (block, chunk) in data_xored.iter().zip(out.chunks_exact_mut(AES_BLOCK_SIZE)) {
            chunk.copy_from_slice(block);
        }
    }

    /// `td::AesIgeStateImpl::decrypt`.
    pub fn decrypt(&mut self, from: &[u8], to: &mut [u8]) {
        assert!(
            from.len().is_multiple_of(AES_BLOCK_SIZE),
            "IGE needs whole blocks"
        );
        assert!(to.len() >= from.len());
        assert!(self.decrypt, "decrypt() on an encrypt state");
        for (in_block, out_block) in from
            .chunks_exact(AES_BLOCK_SIZE)
            .zip(to[..from.len()].chunks_exact_mut(AES_BLOCK_SIZE))
        {
            // The C++ `encrypted.load(in)` copies the ciphertext block
            // before the in-place write below; `in_block` may alias
            // `out_block`, so reading it after the write would give the
            // decrypted bytes instead of the ciphertext.
            let mut encrypted = [0u8; AES_BLOCK_SIZE];
            encrypted.copy_from_slice(in_block);

            let mut block = [0u8; AES_BLOCK_SIZE];
            for (i, byte) in block.iter_mut().enumerate() {
                *byte = self.plaintext_iv[i] ^ encrypted[i];
            }
            let mut aes_block = aes::Block::clone_from_slice(&block);
            self.cipher.decrypt_block(&mut aes_block);
            block.copy_from_slice(&aes_block);
            for (byte, encrypted_iv) in block.iter_mut().zip(self.encrypted_iv) {
                *byte ^= encrypted_iv;
            }
            out_block.copy_from_slice(&block);
            self.plaintext_iv = block;
            self.encrypted_iv = encrypted;
        }
    }
}

impl Default for AesIgeState {
    fn default() -> Self {
        Self::new()
    }
}

/// `td::aes_ige_encrypt(key, iv, from, to)` — updates `iv` in place.
pub fn aes_ige_encrypt(aes_key: &[u8], aes_iv: &mut [u8], from: &[u8], to: &mut [u8]) {
    assert_eq!(aes_iv.len(), 32, "IGE IV must be 256-bit");
    let mut state = AesIgeState::new();
    state.init(aes_key, aes_iv, true);
    state.encrypt(from, to);
    aes_iv[..AES_BLOCK_SIZE].copy_from_slice(&state.encrypted_iv);
    aes_iv[AES_BLOCK_SIZE..].copy_from_slice(&state.plaintext_iv);
}

/// `td::aes_ige_decrypt(key, iv, from, to)` — updates `iv` in place.
pub fn aes_ige_decrypt(aes_key: &[u8], aes_iv: &mut [u8], from: &[u8], to: &mut [u8]) {
    assert_eq!(aes_iv.len(), 32, "IGE IV must be 256-bit");
    let mut state = AesIgeState::new();
    state.init(aes_key, aes_iv, false);
    state.decrypt(from, to);
    aes_iv[..AES_BLOCK_SIZE].copy_from_slice(&state.encrypted_iv);
    aes_iv[AES_BLOCK_SIZE..].copy_from_slice(&state.plaintext_iv);
}

/// `td::aes_cbc_encrypt(key, iv, from, to)` — updates `iv` in place.
pub fn aes_cbc_encrypt(aes_key: &[u8], aes_iv: &mut [u8], from: &[u8], to: &mut [u8]) {
    assert!(to.len() >= from.len());
    assert!(
        from.len().is_multiple_of(AES_BLOCK_SIZE),
        "CBC needs whole blocks"
    );
    assert_eq!(aes_iv.len(), AES_BLOCK_SIZE, "CBC IV must be 128-bit");
    assert_eq!(aes_key.len(), 32, "AES key must be 256-bit");
    if from.is_empty() {
        return;
    }
    let cipher = Aes256::new_from_slice(aes_key).expect("32-byte key checked above");
    let mut chain = [0u8; AES_BLOCK_SIZE];
    chain.copy_from_slice(aes_iv);
    for (chunk, out) in from
        .chunks_exact(AES_BLOCK_SIZE)
        .zip(to[..from.len()].chunks_exact_mut(AES_BLOCK_SIZE))
    {
        let mut block = [0u8; AES_BLOCK_SIZE];
        for i in 0..AES_BLOCK_SIZE {
            block[i] = chunk[i] ^ chain[i];
        }
        let mut aes_block = aes::Block::clone_from_slice(&block);
        cipher.encrypt_block(&mut aes_block);
        block.copy_from_slice(&aes_block);
        out.copy_from_slice(&block);
        chain = block;
    }
    aes_iv.copy_from_slice(&to[from.len() - AES_BLOCK_SIZE..from.len()]);
}

/// `td::aes_cbc_decrypt(key, iv, from, to)` — updates `iv` in place to the
/// last *input* block, then decrypts (the C++ saves the incoming iv first).
pub fn aes_cbc_decrypt(aes_key: &[u8], aes_iv: &mut [u8], from: &[u8], to: &mut [u8]) {
    assert!(to.len() >= from.len());
    assert!(
        from.len().is_multiple_of(AES_BLOCK_SIZE),
        "CBC needs whole blocks"
    );
    assert_eq!(aes_iv.len(), AES_BLOCK_SIZE, "CBC IV must be 128-bit");
    assert_eq!(aes_key.len(), 32, "AES key must be 256-bit");
    if from.is_empty() {
        return;
    }
    let cipher = Aes256::new_from_slice(aes_key).expect("32-byte key checked above");
    let mut chain = [0u8; AES_BLOCK_SIZE];
    chain.copy_from_slice(aes_iv);
    aes_iv.copy_from_slice(&from[from.len() - AES_BLOCK_SIZE..]);
    let mut prev_cipher = [0u8; AES_BLOCK_SIZE];
    for (i, (chunk, out)) in from
        .chunks_exact(AES_BLOCK_SIZE)
        .zip(to[..from.len()].chunks_exact_mut(AES_BLOCK_SIZE))
        .enumerate()
    {
        if i != 0 {
            chain = prev_cipher;
        }
        let mut block = aes::Block::clone_from_slice(chunk);
        cipher.decrypt_block(&mut block);
        let mut plain = [0u8; AES_BLOCK_SIZE];
        plain.copy_from_slice(&block);
        for j in 0..AES_BLOCK_SIZE {
            plain[j] ^= chain[j];
        }
        out.copy_from_slice(&plain);
        prev_cipher.copy_from_slice(chunk);
    }
}

/// `td::AesCbcState` — streaming CBC with the key/iv stored in the state.
pub struct AesCbcState {
    key: [u8; 32],
    iv: [u8; AES_BLOCK_SIZE],
    is_encrypt: bool,
    inited: bool,
}

impl AesCbcState {
    /// `td::AesCbcState(key256, iv128)`.
    pub fn new(key256: &[u8], iv128: &[u8]) -> Self {
        assert_eq!(key256.len(), 32, "AES key must be 256-bit");
        assert_eq!(iv128.len(), AES_BLOCK_SIZE, "CBC IV must be 128-bit");
        let mut key = [0u8; 32];
        key.copy_from_slice(key256);
        let mut iv = [0u8; AES_BLOCK_SIZE];
        iv.copy_from_slice(iv128);
        Self {
            key,
            iv,
            is_encrypt: false,
            inited: false,
        }
    }

    /// `td::AesCbcState::encrypt`.
    pub fn encrypt(&mut self, from: &[u8], to: &mut [u8]) {
        if from.is_empty() {
            return;
        }
        assert!(to.len() >= from.len());
        assert!(
            from.len().is_multiple_of(AES_BLOCK_SIZE),
            "CBC needs whole blocks"
        );
        if !self.inited {
            self.is_encrypt = true;
            self.inited = true;
        } else {
            assert!(self.is_encrypt, "mixed encrypt/decrypt use");
        }
        let mut iv = self.iv;
        aes_cbc_encrypt(&self.key, &mut iv, from, to);
        self.iv = iv;
    }

    /// `td::AesCbcState::decrypt`.
    pub fn decrypt(&mut self, from: &[u8], to: &mut [u8]) {
        if from.is_empty() {
            return;
        }
        assert!(to.len() >= from.len());
        assert!(
            from.len().is_multiple_of(AES_BLOCK_SIZE),
            "CBC needs whole blocks"
        );
        if !self.inited {
            self.is_encrypt = false;
            self.inited = true;
        } else {
            assert!(!self.is_encrypt, "mixed encrypt/decrypt use");
        }
        let mut iv = self.iv;
        aes_cbc_decrypt(&self.key, &mut iv, from, to);
        self.iv = iv;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::random::Xorshift128plus;

    /// The four C++ `strings`: `""`, `"1"`, `"short test string"`, `'a'×1M`.
    fn strings() -> [Vec<u8>; 4] {
        [
            Vec::new(),
            b"1".to_vec(),
            b"short test string".to_vec(),
            vec![b'a'; 1_000_000],
        ]
    }

    #[test]
    fn sha1_vectors() {
        let answers = [
            "da39a3ee5e6b4b0d3255bfef95601890afd80709",
            "356a192b7913b04c54574d18c28d46e6395428ab",
            "b91cac430a1ac74a4d25e042dfece9433272d6b0",
            "34aa973cd4c4daa4f61eeb2bdbad27316534016f",
        ];
        for (string, answer) in strings().iter().zip(answers) {
            assert_eq!(hex(&sha1(string)), answer);
        }
    }

    #[test]
    fn sha256_vectors() {
        let answers = [
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "6b86b273ff34fce19d6b804eff5a3f5747ada4eaa22f1d49c01e52ddb7875b4b",
            "c8f31a63b43c3ca3f00acc3ae149c30f99a145c8adb84260ccb64cbebd0ef296",
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0",
        ];
        for (string, answer) in strings().iter().zip(answers) {
            assert_eq!(hex(&sha256(string)), answer);
        }
    }

    #[test]
    fn sha512_vector() {
        // SHA-512("") — the empty-input vector (RFC 6234 style reference).
        let answer = "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce\
                      47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e";
        assert_eq!(hex(&sha512(b"")), answer);
    }

    #[test]
    fn md5_vectors() {
        let answers = [
            "d41d8cd98f00b204e9800998ecf8427e",
            "c4ca4238a0b923820dcc509a6f75849b",
            "bf00678a761b0d192093eb80ec632221",
            "7707d6ae4e027c70eea2a935c2296f21",
        ];
        for (string, answer) in strings().iter().zip(answers) {
            assert_eq!(hex(&md5(string)), answer);
        }
    }

    #[test]
    fn hmac_sha256_vectors() {
        let answers = [
            "b77deb7d3f3950e7ba374d0186c370a1b13e7f64e75b7df51e176f43819d269f",
            "0509791c5da3aa10b3e094ea840b3e1f7eb8a316e887b42596e38c1eeb91561f",
            "34208fb9906c00f05dfeaaf74b279813e7b544d83390a2424bff9e5c3070f335",
            "9a8dda8539322ca7e8428600d2ceef45950bb87fbebeac05243d14e67f471f0d",
        ];
        for (string, answer) in strings().iter().zip(answers) {
            assert_eq!(hex(&hmac_sha256(b"cucumber", string)), answer);
        }
    }

    #[test]
    fn hmac_sha512_vectors() {
        let answers = [
            "a36f214cdd66fd31a59bf558c4323339d504e303294333bc8557139223f67b31095edacebc28d19e5db468e5754bc6b10394ded13cc88df228100b69cda988b0",
            "df65f71ac952cf41c3ce74ab08db7ef9e3d17055525120feb5f3956a79f2c52fb22edfe89b5d6a20b084eb84454d2f3f07ce200723330b716e0257dc21a9bf28",
            "055a9ee6b2b5160d62f82ef15d30334fdbc33dc7f79104294edb1eeab4ff1150f32a8f40528e19c3253227429c2c7a1f7c88ee97f4ee268060fb02f3ed9eebee",
            "5804a67a4bb93dc7f3ecdd0aa7843723db31b4ed8c89a057038d7c098d07be376d980a3b418f8adc4d28f547a6806cf3e352aa7b2a73442f763303803a75c92c",
        ];
        for (string, answer) in strings().iter().zip(answers) {
            assert_eq!(hex(&hmac_sha512(b"cucumber", string)), answer);
        }
    }

    #[test]
    fn pbkdf2_sha256_vectors() {
        // The C++ matrix: passwords ["", "qwerty", a×1000] × salts (same set)
        // × iterations [1, 2, 1000], in that nesting order.
        let answers = [
            // password ""
            "f7ce0b653d2d72a4108cf5abe912ffdd777616dbbb27a70e8204f3ae2d0f6fad",
            "97398411d6aea43a77acef92226ab8278d4db0668bd1d7a76a725f7680ac45c5",
            "4fc58a21c100ce1835b8f9991d738b56965d14b24e1761fbdffc69ac5e0b667a",
            "347c409cc84f3804ec6f5c15d1c18394022cfa87f323a23823c78625e58df50c",
            "7e3622ef06843e36d5604b99eb5fcd9b685b93fbd17528682685e0e18827a9ee",
            "1a15ba7bde6118949ff880f922b49bbf35b2059d65df903e5282f9e5487f9e39",
            "06e78b0e9a9208473419693cdd680cc33dd4b16c1fbd529cbe59444d207f62af",
            "8601e0259356461efc3f23dd549b0af308601ce1d06cd422c9a4ee1835f6205a",
            "4f6c5dc8d4f51a5700e3e31537339eecd0a04800333646a736c9aea12afee314",
            // password "qwerty"
            "fdfeadfbe1943c4f9eeb7fb44eb9489cbf94b26cd1480005a296bc0410666f6c",
            "f199fdf10100292f703ce525374f7ea5f9b4496b35206790c5090d313ff5938f",
            "b1444b43fe945ff29561e772401db5a00b4c27e4936788b0a717d0b6a99690bc",
            "4fdb7f1095c5a4fb362e171aec8546a614c2fce7449683cc1f0d548439d77324",
            "4c8aed374e44f4a40be8ba7fc23b5bb054be2a4599f239462b412bb5aa22b4e8",
            "fb529c3018f250dcf954c6887c4e70906c12e88f884391612be3aed8782d5684",
            "877e9c8b54f4bc69650a5ff1271ababc8ee7dbc060e3476297358e2a0e89b7c9",
            "f6ec2c1c9b28b53893aaa0987ed37bdbd0e0ed02360628c88d5d8cbd211401e1",
            "fe5faf77f5d881b8a887549f2cc68646bd77b9d998e932d2946e0e626cadc065",
            // password 'a' × 1000
            "eea7d96416cc44bb608eaabb1878166bf51f5cf6a35bc357a49ebf4f73f5af12",
            "b9fc33f78a76f169e838575badbd68c901339bfbf4095d9bd31055c5e10f2460",
            "4ff3d45015f6bc6314b08ea1b6d9616cc1e518f32fa8504dcdac94e6fa1595ac",
            "be232fb2f4e0f467d0ca617701767cb856130dadea2eba89264f70ffbe2265f8",
            "1d017eace64c5b80c0760673f2400c7b6f1ec888b4aecddadeefe651e18f35fb",
            "ee505503e1a74b1585fde3a8fadcb24c1ee788c0e5d4ca8ff32ce8fb19c74f2c",
            "69359bec7400c5a4ca8528913d8dc6b8cd46566abf14fbb058153f4d4a5dcbbd",
            "7db83c33ff87b7fa14f94019e1d41c18fa3ec200821da03e18ce2d9b98e759c6",
            "0c96c218530847fe6778096975af13779cdfb4ae0d19e915ae0db18eb296ff87",
        ];
        let passwords: [&[u8]; 3] = [b"", b"qwerty", &vec![b'a'; 1000]];
        let salts: [&[u8]; 3] = [b"", b"qwerty", &vec![b'a'; 1000]];
        let iterations = [1u32, 2, 1000];
        let mut pos = 0;
        for password in passwords {
            for salt in salts {
                for iteration_count in iterations {
                    let mut result = [0u8; 32];
                    pbkdf2_sha256(password, salt, iteration_count, &mut result);
                    assert_eq!(hex(&result), answers[pos], "vector {pos}");
                    pos += 1;
                }
            }
        }
        assert_eq!(pos, answers.len());
    }

    #[test]
    fn sha256_state_matches_one_shot() {
        let lengths = [0, 1, 31, 32, 33, 9999, 10000, 10001];
        for length in lengths {
            let mut rng = Xorshift128plus::from_seed(length as u64 + 1);
            let mut data = vec![0u8; length];
            rng.bytes(&mut data);
            let baseline = sha256(&data);

            let mut state = Sha256State::new();
            state.init();
            // Split into random chunks, like the C++ rand_split.
            let mut pos = 0;
            while pos < data.len() {
                let chunk = rng.fast(1, 1000) as usize;
                let end = (pos + chunk).min(data.len());
                state.feed(&data[pos..end]);
                pos = end;
            }
            let mut result = [0u8; 32];
            state.extract(&mut result, false);
            assert_eq!(baseline, result, "length {length}");
        }
    }

    #[test]
    fn crc64_vectors() {
        let answers = [
            0u64,
            3039664240384658157,
            17549519902062861804,
            8794730974279819706,
        ];
        for (string, answer) in strings().iter().zip(answers) {
            assert_eq!(crc64(string), answer);
        }
    }

    #[test]
    fn crc16_vectors() {
        let answers = [0u16, 9842, 25046, 37023];
        for (string, answer) in strings().iter().zip(answers) {
            assert_eq!(crc16(string), answer);
        }
    }

    #[test]
    fn string_variants_return_binary_digests() {
        assert_eq!(sha256_bytes(b"1"), sha256(b"1").to_vec());
        assert_eq!(md5_bytes(b"1").len(), 16);
        assert_eq!(sha1_bytes(b"1").len(), 20);
        assert_eq!(sha512_bytes(b"1").len(), 64);
    }

    fn hex(bytes: &[u8]) -> String {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut out = String::with_capacity(bytes.len() * 2);
        for &byte in bytes {
            out.push(DIGITS[(byte >> 4) as usize] as char);
            out.push(DIGITS[(byte & 0xf) as usize] as char);
        }
        out
    }

    /// The C++ deterministic stream: `seed = seed * 123457567 + 987651241`,
    /// output byte `(seed >> 23) & 255` (u32 wrapping).
    struct DeterministicBytes {
        seed: u32,
    }

    impl DeterministicBytes {
        fn new(seed: u32) -> Self {
            Self { seed }
        }

        fn fill(&mut self, out: &mut [u8]) {
            for byte in out {
                self.seed = self
                    .seed
                    .wrapping_mul(123_457_567)
                    .wrapping_add(987_651_241);
                *byte = ((self.seed >> 23) & 255) as u8;
            }
        }
    }

    /// Port of `TEST(Crypto, Aes)`.
    #[test]
    fn aes_ecb_roundtrip() {
        let mut rnd = Xorshift128plus::from_seed(123);
        let mut key = [0u8; 32];
        rnd.bytes(&mut key);
        let mut plaintext = [0u8; 16];
        rnd.bytes(&mut plaintext);
        let mut encrypted = [0u8; 16];
        let mut decrypted = [0u8; 16];

        let mut encryptor = AesState::new();
        encryptor.init(&key, true);
        let mut decryptor = AesState::new();
        decryptor.init(&key, false);

        encryptor.encrypt(&plaintext, &mut encrypted);
        decryptor.decrypt(&encrypted, &mut decrypted);

        assert_eq!(decrypted, plaintext);
        assert_ne!(decrypted, encrypted);
        assert_eq!(crc32fast::hash(&encrypted), 178_892_237);
    }

    /// Port of `TEST(Crypto, AesCtrState)`.
    #[test]
    fn aes_ctr_state_matches_cpp() {
        let answers1 = [
            0u32,
            1_141_589_763,
            596_296_607,
            3_673_001_485,
            2_302_125_528,
            330_967_191,
            2_047_392_231,
            3_537_459_563,
            307_747_798,
            2_149_598_133,
        ];
        let answers2 = [
            0u32,
            2_053_451_992,
            1_384_063_362,
            3_266_188_502,
            2_893_295_118,
            780_356_167,
            1_904_947_434,
            2_043_402_406,
            472_080_809,
            1_807_109_488,
        ];
        let lengths = [
            0usize, 1, 31, 32, 33, 9999, 10000, 10001, 999_999, 1_000_001,
        ];

        for (i, length) in lengths.iter().enumerate() {
            let mut generator = DeterministicBytes::new(*length as u32);
            let mut s = vec![0u8; *length];
            generator.fill(&mut s);
            let mut key = [0u8; 32];
            generator.fill(&mut key);
            let mut iv = [0u8; 16];
            generator.fill(&mut iv);

            let mut state = AesCtrState::new();
            state.init(&key, &iv);
            let mut t = vec![0u8; *length];
            let mut pos = 0;
            for chunk in crate::random::rand_split(&vec![0u8; *length]) {
                let len = chunk.len();
                state.encrypt(&s[pos..pos + len], &mut t[pos..pos + len]);
                pos += len;
            }
            assert_eq!(crc32fast::hash(&t), answers1[i], "length {length}");

            state.init(&key, &iv);
            pos = 0;
            for chunk in crate::random::rand_split(&vec![0u8; *length]) {
                let len = chunk.len();
                // In-place decrypt via a scratch copy (C++ decrypts in
                // place; the copy keeps Rust's aliasing rules satisfied).
                let scratch = t[pos..pos + len].to_vec();
                state.decrypt(&scratch, &mut t[pos..pos + len]);
                pos += len;
            }
            assert_eq!(s, t, "decrypt roundtrip length {length}");

            iv.fill(0xFF);
            state.init(&key, &iv);
            pos = 0;
            for chunk in crate::random::rand_split(&vec![0u8; *length]) {
                let len = chunk.len();
                state.encrypt(&s[pos..pos + len], &mut t[pos..pos + len]);
                pos += len;
            }
            assert_eq!(crc32fast::hash(&t), answers2[i], "0xFF iv length {length}");
        }
    }

    /// Port of `TEST(Crypto, AesIgeState)`.
    #[test]
    fn aes_ige_state_matches_cpp() {
        let answers1 = [
            0u32,
            2_045_698_207,
            2_423_540_300,
            525_522_475,
            1_545_267_325,
            724_143_417,
        ];
        let lengths = [0usize, 16, 32, 256, 1024, 65536];

        for (i, length) in lengths.iter().enumerate() {
            let mut generator = DeterministicBytes::new(*length as u32);
            let mut s = vec![0u8; *length];
            generator.fill(&mut s);
            let mut key = [0u8; 32];
            generator.fill(&mut key);
            let mut iv = [0u8; 32];
            generator.fill(&mut iv);

            let mut state = AesIgeState::new();
            state.init(&key, &iv, true);
            let mut t = vec![0u8; *length];
            let mut iv_copy = iv;
            let mut u = vec![0u8; *length];
            let mut pos = 0;
            for chunk in crate::random::rand_split(&vec![0u8; *length / 16]) {
                let len = 16 * chunk.len();
                state.encrypt(&s[pos..pos + len], &mut t[pos..pos + len]);
                aes_ige_encrypt(
                    &key,
                    &mut iv_copy,
                    &s[pos..pos + len],
                    &mut u[pos..pos + len],
                );
                pos += len;
            }

            assert_eq!(crc32fast::hash(&t), answers1[i], "state length {length}");
            assert_eq!(crc32fast::hash(&u), answers1[i], "one-shot length {length}");

            state.init(&key, &iv, false);
            iv_copy = iv;
            pos = 0;
            for chunk in crate::random::rand_split(&vec![0u8; *length / 16]) {
                let len = 16 * chunk.len();
                let scratch = t[pos..pos + len].to_vec();
                state.decrypt(&scratch, &mut t[pos..pos + len]);
                let scratch = u[pos..pos + len].to_vec();
                aes_ige_decrypt(&key, &mut iv_copy, &scratch, &mut u[pos..pos + len]);
                pos += len;
            }
            assert_eq!(s, t, "state decrypt length {length}");
            assert_eq!(s, u, "one-shot decrypt length {length}");
        }
    }

    /// Port of `TEST(Crypto, AesCbcState)`.
    #[test]
    fn aes_cbc_state_matches_cpp() {
        let answers1 = [
            0u32,
            3_617_355_989,
            3_449_188_102,
            186_999_968,
            4_244_808_847,
            2_626_031_206,
        ];
        let lengths = [0usize, 16, 32, 256, 1024, 65536];

        for (i, length) in lengths.iter().enumerate() {
            let mut generator = DeterministicBytes::new(*length as u32);
            let mut s = vec![0u8; *length];
            generator.fill(&mut s);
            let mut key = [0u8; 32];
            generator.fill(&mut key);
            let mut iv = [0u8; 16];
            generator.fill(&mut iv);

            let mut state = AesCbcState::new(&key, &iv);
            let mut t = vec![0u8; *length];
            let mut iv_copy = iv;
            let mut u = vec![0u8; *length];
            let mut pos = 0;
            for chunk in crate::random::rand_split(&vec![0u8; *length / 16]) {
                let len = 16 * chunk.len();
                state.encrypt(&s[pos..pos + len], &mut t[pos..pos + len]);
                aes_cbc_encrypt(
                    &key,
                    &mut iv_copy,
                    &s[pos..pos + len],
                    &mut u[pos..pos + len],
                );
                pos += len;
            }

            assert_eq!(crc32fast::hash(&t), answers1[i], "state length {length}");
            assert_eq!(crc32fast::hash(&u), answers1[i], "one-shot length {length}");

            let mut state = AesCbcState::new(&key, &iv);
            iv_copy = iv;
            pos = 0;
            for chunk in crate::random::rand_split(&vec![0u8; *length / 16]) {
                let len = 16 * chunk.len();
                let scratch = t[pos..pos + len].to_vec();
                state.decrypt(&scratch, &mut t[pos..pos + len]);
                let scratch = u[pos..pos + len].to_vec();
                aes_cbc_decrypt(&key, &mut iv_copy, &scratch, &mut u[pos..pos + len]);
                pos += len;
            }
            assert_eq!(s, t, "state decrypt length {length}");
            assert_eq!(s, u, "one-shot decrypt length {length}");
        }
    }
}
