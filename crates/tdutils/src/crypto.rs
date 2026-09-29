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
///
/// Native builds run the exact C++ OpenSSL-3 backend (crypto.cpp): a
/// thread-local `EVP_MD_CTX` pre-initialized from `EVP_MD_fetch("sha512")`,
/// and per call `EVP_MD_CTX_copy_ex` into a scratch ctx +
/// `EVP_DigestUpdate` + `EVP_DigestFinal_ex` + reset (`make_digest`). The
/// RustCrypto `sha2` crate measured ~1.1× under the C++ row (its SHA-512
/// compress is slower than OpenSSL's asm); SHA-256/SHA-1 stay on `sha2`/`sha1`
/// — those rows run ahead of C++. Emscripten keeps the RustCrypto form.
#[cfg(not(target_os = "emscripten"))]
pub fn sha512(data: &[u8]) -> [u8; 64] {
    openssl_digest::sha512(data)
}

#[cfg(target_os = "emscripten")]
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
///
/// Native builds run the exact C++ OpenSSL-3 backend (crypto.cpp
/// `hmac_impl_sha512`): a thread-local `EVP_MAC_CTX` fetched from
/// `EVP_MAC_fetch("HMAC")` with the `digest` parameter set to "SHA512"
/// once, and per call `EVP_MAC_init` + `EVP_MAC_update` + `EVP_MAC_final`.
/// The `hmac` crate measured ~1.16× under the C++ row; HMAC-SHA256 stays
/// on `hmac` — that row runs ahead of C++. Emscripten keeps RustCrypto.
#[cfg(not(target_os = "emscripten"))]
pub fn hmac_sha512(key: &[u8], message: &[u8]) -> [u8; 64] {
    openssl_digest::hmac_sha512(key, message)
}

#[cfg(target_os = "emscripten")]
pub fn hmac_sha512(key: &[u8], message: &[u8]) -> [u8; 64] {
    let mut mac = <Hmac<Sha512> as Mac>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(message);
    mac.finalize().into_bytes().into()
}

/// The OpenSSL-3 digest/MAC bridge (native) — the exact backend the C++
/// `td::sha512`/`td::hmac_sha512` run against this same libcrypto (the
/// thread-local context pairs of crypto.cpp `make_digest` and
/// `init_thread_local_evp_mac_ctx`/`hmac_impl_finish`; the C++ pre-3.0
/// one-shot `SHA512()`/`HMAC()` forms are not mirrored — the linked
/// libcrypto is 3.x on every native target).
///
/// Guarantee change, documented here like the S96 random bridge: these two
/// functions leave the pure-Rust RustCrypto stack for the vendored OpenSSL
/// — the digests are identical, the backend is the C++ one.
#[cfg(not(target_os = "emscripten"))]
mod openssl_digest {
    use std::cell::RefCell;

    /// `init_thread_local_evp_md_ctx` + `make_digest`'s scratch ctx, with
    /// the C++ thread-local destructor as `Drop`.
    struct Sha512Ctxs {
        md: *mut openssl_sys::EVP_MD_CTX,
        scratch: *mut openssl_sys::EVP_MD_CTX,
    }

    impl Sha512Ctxs {
        fn new() -> Self {
            // SAFETY: OpenSSL contexts are created and used on this thread
            // only, freed exactly once by `Drop` (the thread-local
            // destructor).
            unsafe {
                let name = b"sha512\0";
                let md = openssl_sys::EVP_MD_fetch(
                    std::ptr::null_mut(),
                    name.as_ptr().cast(),
                    std::ptr::null(),
                );
                assert!(!md.is_null(), "EVP_MD_fetch(sha512) failed");
                let ctx = openssl_sys::EVP_MD_CTX_new();
                assert!(!ctx.is_null(), "EVP_MD_CTX_new failed");
                let ok = openssl_sys::EVP_DigestInit_ex(ctx, md, std::ptr::null_mut());
                assert_eq!(ok, 1, "EVP_DigestInit_ex(sha512) failed");
                openssl_sys::EVP_MD_free(md);
                let scratch = openssl_sys::EVP_MD_CTX_new();
                assert!(!scratch.is_null(), "EVP_MD_CTX_new failed");
                Self { md: ctx, scratch }
            }
        }
    }

    impl Drop for Sha512Ctxs {
        fn drop(&mut self) {
            // SAFETY: both contexts were created by `new` on this thread.
            unsafe {
                openssl_sys::EVP_MD_CTX_free(self.md);
                openssl_sys::EVP_MD_CTX_free(self.scratch);
            }
        }
    }

    /// `OSSL_PARAM_UTF8_STRING` — the constant behind
    /// `OSSL_PARAM_construct_utf8_string` (openssl-sys does not bind the
    /// constructors).
    const OSSL_PARAM_UTF8_STRING: std::ffi::c_uint = 4;

    /// `init_thread_local_evp_mac_ctx(ctx, "SHA512")`: the fetched "HMAC"
    /// method with the digest parameter set once.
    struct HmacSha512Mac {
        ctx: *mut openssl_sys::EVP_MAC_CTX,
    }

    impl HmacSha512Mac {
        fn new() -> Self {
            // SAFETY: same single-thread, freed-once contract; the param
            // array mirrors `OSSL_PARAM_construct_utf8_string("digest",
            // "SHA512", 0)` + `OSSL_PARAM_construct_end()`.
            unsafe {
                let name = b"HMAC\0";
                let mac = openssl_sys::EVP_MAC_fetch(
                    std::ptr::null_mut(),
                    name.as_ptr().cast(),
                    std::ptr::null(),
                );
                assert!(!mac.is_null(), "EVP_MAC_fetch(HMAC) failed");
                let ctx = openssl_sys::EVP_MAC_CTX_new(mac);
                assert!(!ctx.is_null(), "EVP_MAC_CTX_new failed");
                let digest = b"SHA512\0";
                let params = [
                    openssl_sys::OSSL_PARAM {
                        key: c"digest".as_ptr(),
                        data_type: OSSL_PARAM_UTF8_STRING,
                        data: digest.as_ptr() as *mut std::ffi::c_void,
                        data_size: digest.len(),
                        return_size: 0,
                    },
                    openssl_sys::OSSL_PARAM {
                        key: std::ptr::null(),
                        data_type: 0,
                        data: std::ptr::null_mut(),
                        data_size: 0,
                        return_size: 0,
                    },
                ];
                let ok = openssl_sys::EVP_MAC_CTX_set_params(ctx, params.as_ptr());
                assert_eq!(ok, 1, "EVP_MAC_CTX_set_params(digest) failed");
                openssl_sys::EVP_MAC_free(mac);
                Self { ctx }
            }
        }
    }

    impl Drop for HmacSha512Mac {
        fn drop(&mut self) {
            // SAFETY: the context was created by `new` on this thread.
            unsafe { openssl_sys::EVP_MAC_CTX_free(self.ctx) }
        }
    }

    thread_local! {
        static SHA512: RefCell<Option<Sha512Ctxs>> = const { RefCell::new(None) };
        static HMAC_SHA512: RefCell<Option<HmacSha512Mac>> = const { RefCell::new(None) };
    }

    /// `make_digest(data, output, evp_md_ctx)` for SHA-512.
    pub(super) fn sha512(data: &[u8]) -> [u8; 64] {
        SHA512.with(|cell| {
            let mut slot = cell.borrow_mut();
            let ctxs = slot.get_or_insert_with(Sha512Ctxs::new);
            // SAFETY: both contexts belong to this thread; the scratch is
            // used for exactly one digest here and reset after.
            unsafe {
                let ok = openssl_sys::EVP_MD_CTX_copy_ex(ctxs.scratch, ctxs.md);
                assert_eq!(ok, 1, "EVP_MD_CTX_copy_ex failed");
                let ok =
                    openssl_sys::EVP_DigestUpdate(ctxs.scratch, data.as_ptr().cast(), data.len());
                assert_eq!(ok, 1, "EVP_DigestUpdate failed");
                let mut out = [0u8; 64];
                let ok = openssl_sys::EVP_DigestFinal_ex(
                    ctxs.scratch,
                    out.as_mut_ptr(),
                    std::ptr::null_mut(),
                );
                assert_eq!(ok, 1, "EVP_DigestFinal_ex failed");
                openssl_sys::EVP_MD_CTX_reset(ctxs.scratch);
                out
            }
        })
    }

    /// `hmac_impl_finish(ctx, key, message, dest)` for SHA-512.
    pub(super) fn hmac_sha512(key: &[u8], message: &[u8]) -> [u8; 64] {
        HMAC_SHA512.with(|cell| {
            let mut slot = cell.borrow_mut();
            let mac = slot.get_or_insert_with(HmacSha512Mac::new);
            // SAFETY: the context belongs to this thread; `EVP_MAC_init`
            // re-keys it for exactly one MAC here.
            unsafe {
                let ok =
                    openssl_sys::EVP_MAC_init(mac.ctx, key.as_ptr(), key.len(), std::ptr::null());
                assert_eq!(ok, 1, "EVP_MAC_init failed");
                let ok = openssl_sys::EVP_MAC_update(mac.ctx, message.as_ptr(), message.len());
                assert_eq!(ok, 1, "EVP_MAC_update failed");
                let mut out = [0u8; 64];
                let ok = openssl_sys::EVP_MAC_final(
                    mac.ctx,
                    out.as_mut_ptr(),
                    std::ptr::null_mut(),
                    out.len(),
                );
                assert_eq!(ok, 1, "EVP_MAC_final failed");
                out
            }
        })
    }
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

/// A 16-byte XOR over fixed-size arrays — the block-width mix of the
/// CBC/IGE chains. The fixed 16-iteration loop auto-vectorizes to one
/// wide XOR; the earlier `u128::from_le_bytes`/`to_le_bytes` forms looked
/// equivalent but compiled to scalar endian-shuffle sequences here, and
/// byte-wise *slices* (`a[i] ^ b[i]` over `&[u8]`) left bounds checks on
/// every lane.
#[inline]
fn xor_block(a: &[u8; AES_BLOCK_SIZE], b: &[u8; AES_BLOCK_SIZE]) -> [u8; AES_BLOCK_SIZE] {
    let mut r = [0u8; AES_BLOCK_SIZE];
    for i in 0..AES_BLOCK_SIZE {
        r[i] = a[i] ^ b[i];
    }
    r
}

/// The CBC-encrypt EVP bridge (native) — one `EVP_EncryptUpdate` over the
/// whole buffer with `EVP_aes_256_cbc`, padding off: the exact backend
/// `td::aes_cbc_encrypt` runs (crypto.cpp `Evp`), the same shape as the
/// S96 §1.1 CTR bridge. CBC encryption chains through the cipher, so the
/// loop is latency-bound and OpenSSL's asm (the XOR folded into the round
/// pipeline, no per-block call) is the reference; the RustCrypto per-block
/// form topped out ~30% under it on the S97 machine.
#[cfg(not(target_os = "emscripten"))]
mod cbc_native {
    use super::AES_BLOCK_SIZE;

    /// CBC-encrypt `from` into `to` under `key`/`iv` (one shot, padding
    /// off). `to.len() >= from.len()`, whole blocks.
    pub(super) fn cbc_encrypt_oneshot(key: &[u8], iv: &[u8], from: &[u8], to: &mut [u8]) {
        debug_assert!(from.len().is_multiple_of(AES_BLOCK_SIZE));
        debug_assert!(to.len() >= from.len());
        debug_assert_eq!(key.len(), 32);
        debug_assert_eq!(iv.len(), AES_BLOCK_SIZE);
        // SAFETY: the context is created here, used on this thread only,
        // and freed exactly once at the end of the function.
        unsafe {
            let ctx = openssl_sys::EVP_CIPHER_CTX_new();
            assert!(!ctx.is_null(), "EVP_CIPHER_CTX_new failed");
            let ok = openssl_sys::EVP_EncryptInit_ex(
                ctx,
                openssl_sys::EVP_aes_256_cbc(),
                std::ptr::null_mut(),
                key.as_ptr(),
                iv.as_ptr(),
            );
            assert_eq!(ok, 1, "EVP_EncryptInit_ex failed");
            assert_eq!(
                openssl_sys::EVP_CIPHER_CTX_set_padding(ctx, 0),
                1,
                "EVP_CIPHER_CTX_set_padding failed"
            );
            let mut out_len: i32 = 0;
            let ok = openssl_sys::EVP_EncryptUpdate(
                ctx,
                to.as_mut_ptr(),
                &mut out_len,
                from.as_ptr(),
                from.len() as i32,
            );
            assert_eq!(ok, 1, "EVP_EncryptUpdate failed");
            debug_assert_eq!(out_len as usize, from.len(), "CBC wrote a partial block");
            openssl_sys::EVP_CIPHER_CTX_free(ctx);
        }
    }
    /// CBC-decrypt `from` into `to` under `key`/`iv` (one shot, padding
    /// off) — `td::aes_cbc_decrypt`'s exact backend (crypto.cpp: a fresh
    /// `Evp`, `init_decrypt_cbc`, `init_iv`, one `EVP_DecryptUpdate`). The
    /// S97 wide `decrypt_blocks` pass ran ~1.3× under it: OpenSSL's asm
    /// decrypts whole buffers through the pipelined rounds, and unlike
    /// encryption the S98 measurement showed the FFI amortization winning
    /// over the extra copy the wide pass needs anyway.
    pub(super) fn cbc_decrypt_oneshot(key: &[u8], iv: &[u8], from: &[u8], to: &mut [u8]) {
        debug_assert!(from.len().is_multiple_of(AES_BLOCK_SIZE));
        debug_assert!(to.len() >= from.len());
        debug_assert_eq!(key.len(), 32);
        debug_assert_eq!(iv.len(), AES_BLOCK_SIZE);
        // SAFETY: the context is created here, used on this thread only,
        // and freed exactly once at the end of the function.
        unsafe {
            let ctx = openssl_sys::EVP_CIPHER_CTX_new();
            assert!(!ctx.is_null(), "EVP_CIPHER_CTX_new failed");
            let ok = openssl_sys::EVP_DecryptInit_ex(
                ctx,
                openssl_sys::EVP_aes_256_cbc(),
                std::ptr::null_mut(),
                key.as_ptr(),
                iv.as_ptr(),
            );
            assert_eq!(ok, 1, "EVP_DecryptInit_ex failed");
            assert_eq!(
                openssl_sys::EVP_CIPHER_CTX_set_padding(ctx, 0),
                1,
                "EVP_CIPHER_CTX_set_padding failed"
            );
            let mut out_len: i32 = 0;
            let ok = openssl_sys::EVP_DecryptUpdate(
                ctx,
                to.as_mut_ptr(),
                &mut out_len,
                from.as_ptr(),
                from.len() as i32,
            );
            assert_eq!(ok, 1, "EVP_DecryptUpdate failed");
            debug_assert_eq!(out_len as usize, from.len(), "CBC wrote a partial block");
            openssl_sys::EVP_CIPHER_CTX_free(ctx);
        }
    }
}
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
            .as_chunks::<AES_BLOCK_SIZE>()
            .0
            .iter()
            .zip(dst.as_chunks_mut::<AES_BLOCK_SIZE>().0.iter_mut())
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
            .as_chunks::<AES_BLOCK_SIZE>()
            .0
            .iter()
            .zip(dst.as_chunks_mut::<AES_BLOCK_SIZE>().0.iter_mut())
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

/// `dst[i] = src[i] ^ ks[i]` in 8-byte words — the word loop auto-vectorizes,
/// where the byte-wise form dominated the profile after the bulk refill
/// landed (the S79 CTR gate).
#[cfg(target_os = "emscripten")]
fn xor_keystream(src: &[u8], ks: &[u8], dst: &mut [u8]) {
    let n = src.len().min(ks.len());
    assert!(dst.len() >= n);
    let mut i = 0;
    while i + 8 <= n {
        let s = u64::from_le_bytes(src[i..i + 8].try_into().unwrap());
        let k = u64::from_le_bytes(ks[i..i + 8].try_into().unwrap());
        dst[i..i + 8].copy_from_slice(&(s ^ k).to_le_bytes());
        i += 8;
    }
    while i < n {
        dst[i] = src[i] ^ ks[i];
        i += 1;
    }
}

/// `td::AesCtrState` — streaming AES-256-CTR with the 128-bit big-endian
/// counter seeded by the IV. The keystream is byte-continuous across calls,
/// like OpenSSL's EVP CTR `num` state.
///
/// Native builds route the bulk path through the **vendored OpenSSL EVP**
/// (`EVP_aes_256_ctr`) — the exact assembly the C++ `td::AesCtrState` runs,
/// linked through `openssl-sys` already present for sqlcipher (the S96
/// §1.1 bridge: the RustCrypto path topped out at ~2.4 GB/s vs OpenSSL's
/// ~11 GB/s on the S96-final machine, a stable 4.7× row). emscripten keeps
/// the RustCrypto implementation below — the wasm build links no OpenSSL.
#[cfg(not(target_os = "emscripten"))]
pub use crate::crypto::ctr_native::AesCtrState;

/// The OpenSSL EVP-backed CTR state (native). Kept as a private module so
/// the public type and its docs live at the use site above.
#[cfg(not(target_os = "emscripten"))]
mod ctr_native {
    /// `AES_BLOCK_SIZE` of the parent module — CTR's IV/counter width.
    const AES_BLOCK_SIZE: usize = 16;

    pub struct AesCtrState {
        ctx: *mut openssl_sys::EVP_CIPHER_CTX,
    }

    impl AesCtrState {
        pub fn new() -> Self {
            let mut state = Self {
                ctx: std::ptr::null_mut(),
            };
            state.init_raw(&[0u8; 32], &[0u8; AES_BLOCK_SIZE]);
            state
        }

        /// `td::AesCtrState::init(key, iv)`.
        pub fn init(&mut self, key: &[u8], iv: &[u8]) {
            assert_eq!(key.len(), 32, "AES key must be 256-bit");
            assert_eq!(iv.len(), AES_BLOCK_SIZE, "CTR IV must be 128-bit");
            self.init_raw(key, iv);
        }

        /// (Re)key the context. A fresh context each time (no
        /// `EVP_CIPHER_CTX_reset` in the bindings): `init` may be called on
        /// a mid-stream state and must restart cleanly (the C++
        /// `AesCtrState::init` semantics, re-arming a fresh counter), and
        /// `init` runs once per transport, not per buffer — the allocation
        /// is off the hot path.
        fn init_raw(&mut self, key: &[u8], iv: &[u8]) {
            // SAFETY: the old context (if any) was created by this module
            // and is freed exactly once, here; the new one is initialized
            // before any use and freed by `Drop` (or the next `init_raw`).
            unsafe {
                if !self.ctx.is_null() {
                    openssl_sys::EVP_CIPHER_CTX_free(self.ctx);
                }
                let ctx = openssl_sys::EVP_CIPHER_CTX_new();
                assert!(!ctx.is_null(), "EVP_CIPHER_CTX_new failed");
                self.ctx = ctx;
                let ok = openssl_sys::EVP_EncryptInit_ex(
                    ctx,
                    openssl_sys::EVP_aes_256_ctr(),
                    std::ptr::null_mut(),
                    key.as_ptr(),
                    iv.as_ptr(),
                );
                assert_eq!(ok, 1, "EVP_EncryptInit_ex failed");
            }
        }

        fn advance(&mut self, src: &[u8], dst: &mut [u8]) {
            assert!(dst.len() >= src.len());
            if src.is_empty() {
                return;
            }
            let mut out_len: i32 = 0;
            // SAFETY: EVP_EncryptUpdate on a CTR context writes exactly
            // `src.len()` bytes (a stream cipher, no padding); the raw
            // -pointer form also accepts `src == dst` (the documented
            // in-place mode), which the safe two-slice API cannot express.
            let ok = unsafe {
                openssl_sys::EVP_EncryptUpdate(
                    self.ctx,
                    dst.as_mut_ptr(),
                    &mut out_len,
                    src.as_ptr(),
                    src.len() as i32,
                )
            };
            assert_eq!(ok, 1, "EVP_EncryptUpdate failed");
            debug_assert_eq!(out_len as usize, src.len(), "CTR wrote a partial block");
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
        /// (transports encrypt their buffers in place).
        pub fn encrypt_in_place(&mut self, data: &mut [u8]) {
            let len = data.len();
            let ptr = data.as_mut_ptr();
            // SAFETY: EVP_EncryptUpdate documents in-place operation
            // (`in == out`); one buffer, lockstep, CTR writes exactly `len`.
            unsafe {
                let ok = openssl_sys::EVP_EncryptUpdate(self.ctx, ptr, &mut 0, ptr, len as i32);
                assert_eq!(ok, 1, "EVP_EncryptUpdate failed");
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

    impl Drop for AesCtrState {
        fn drop(&mut self) {
            // SAFETY: the context was created in `new` and is freed exactly
            // once, here.
            unsafe { openssl_sys::EVP_CIPHER_CTX_free(self.ctx) };
        }
    }

    // SAFETY: an `EVP_CIPHER_CTX` is not used from two threads at once here
    // — transports move the whole state between actors serialized by the
    // actor system (the same single-threaded-use contract the RustCrypto
    // key it replaces had by construction).
    unsafe impl Send for AesCtrState {}
}

/// The RustCrypto CTR state — emscripten only (see [`AesCtrState`]).
#[cfg(target_os = "emscripten")]
mod ctr_rustcrypto {
    use super::AES_BLOCK_SIZE;
    use aes::cipher::{BlockEncrypt, KeyInit};

    pub struct AesCtrState {
        cipher: aes::Aes256,
        counter: [u8; AES_BLOCK_SIZE],
        keystream: [u8; KEYSTREAM_SIZE],
        /// Bytes of the current keystream run already consumed; the next run is
        /// generated when this reaches `KEYSTREAM_SIZE`.
        used: usize,
    }

    /// Keystream blocks per bulk refill (32 × 16 = 512 bytes) — wide enough to
    /// amortize the refill over the CTR transport packets; the aes crate's ni
    /// backend takes its 8-block parallel path per chunk of the slice.
    const KEYSTREAM_BLOCKS: usize = 32;
    const KEYSTREAM_SIZE: usize = KEYSTREAM_BLOCKS * AES_BLOCK_SIZE;

    impl AesCtrState {
        pub fn new() -> Self {
            Self {
                cipher: aes::Aes256::new_from_slice(&[0u8; 32]).expect("AES-256 key size is fixed"),
                counter: [0u8; AES_BLOCK_SIZE],
                keystream: [0u8; KEYSTREAM_SIZE],
                used: KEYSTREAM_SIZE,
            }
        }

        /// `td::AesCtrState::init(key, iv)`.
        pub fn init(&mut self, key: &[u8], iv: &[u8]) {
            assert_eq!(key.len(), 32, "AES key must be 256-bit");
            assert_eq!(iv.len(), AES_BLOCK_SIZE, "CTR IV must be 128-bit");
            self.cipher = aes::Aes256::new_from_slice(key).expect("32-byte key checked above");
            self.counter.copy_from_slice(iv);
            self.used = KEYSTREAM_SIZE;
        }

        fn increment_counter(&mut self) {
            // The 128-bit big-endian increment happens after use, like OpenSSL's
            // CTR.
            for byte in self.counter.iter_mut().rev() {
                *byte = byte.wrapping_add(1);
                if *byte != 0 {
                    break;
                }
            }
        }

        fn refill_keystream(&mut self) {
            let mut blocks: [aes::Block; KEYSTREAM_BLOCKS] =
                std::array::from_fn(|_| aes::Block::clone_from_slice(&[0u8; AES_BLOCK_SIZE]));
            for block in blocks.iter_mut() {
                block.copy_from_slice(&self.counter);
                self.increment_counter();
            }
            self.cipher.encrypt_blocks(&mut blocks);
            for (i, block) in blocks.iter().enumerate() {
                self.keystream[i * AES_BLOCK_SIZE..(i + 1) * AES_BLOCK_SIZE].copy_from_slice(block);
            }
            self.used = 0;
        }

        fn advance(&mut self, src: &[u8], dst: &mut [u8]) {
            assert!(dst.len() >= src.len());
            let mut offset = 0;
            while offset < src.len() {
                if self.used == KEYSTREAM_SIZE {
                    self.refill_keystream();
                }
                let n = (src.len() - offset).min(KEYSTREAM_SIZE - self.used);
                super::xor_keystream(
                    &src[offset..offset + n],
                    &self.keystream[self.used..self.used + n],
                    &mut dst[offset..offset + n],
                );
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

        /// `encrypt(payload, payload)` — the in-place form C++ uses everywhere.
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
}

#[cfg(target_os = "emscripten")]
pub use crate::crypto::ctr_rustcrypto::AesCtrState;

/// `td::AesIgeState` — the IGE mode of `crypto.cpp`, byte-exact port of
/// `AesIgeStateImpl` (CBC-encrypt of XOR-chained blocks; the iv is 32 bytes:
/// `encrypted_iv_` then `plaintext_iv_`).
///
/// Native encrypt runs the exact C++ shape: IGE encrypts as
/// `c[i] = E(x[i] ^ x[i-2] ^ c[i-1])`, `y[i] = c[i] ^ x[i-1]` — a plain CBC
/// chain plus two linear XOR passes — so each 31-block chunk (the C++
/// `BLOCK_COUNT`) is XOR-prepared on the stack and pushed through one bulk
/// `EVP_EncryptUpdate` with `EVP_aes_256_cbc` (padding off), the same
/// OpenSSL asm the C++ `Evp` runs (the S97 IGE-encrypt pass; the RustCrypto
/// per-block single-pass topped out ~40% under it — the serial chain is
/// AES-latency-bound and OpenSSL folds the XOR into the round pipeline).
/// Emscripten keeps that pure-Rust single-pass below (no OpenSSL there).
/// Decrypt stays RustCrypto on every target: IGE decryption has no serial
/// chain through the cipher and the single-pass form already runs ahead of
/// the C++ ECB-EVP loop.
pub struct AesIgeState {
    /// The CBC-encrypt EVP context of the native encrypt path; created by
    /// `init`, freed by `Drop`. Null on emscripten (field not compiled).
    #[cfg(not(target_os = "emscripten"))]
    evp: *mut openssl_sys::EVP_CIPHER_CTX,
    /// The RustCrypto cipher of the decrypt path (and of both emscripten
    /// paths). `None` until `init` needs it — the native *encrypt* path
    /// never expands a RustCrypto key (the S98 pass: `new` eagerly ran a
    /// zero-key expansion and `init` re-ran it, a whole AES-256 key
    /// schedule per one-shot state that the 64-byte rows measured).
    cipher: Option<Aes256>,
    decrypt: bool,
    encrypted_iv: [u8; AES_BLOCK_SIZE],
    plaintext_iv: [u8; AES_BLOCK_SIZE],
}

impl AesIgeState {
    pub fn new() -> Self {
        Self {
            #[cfg(not(target_os = "emscripten"))]
            evp: std::ptr::null_mut(),
            cipher: None,
            decrypt: true,
            encrypted_iv: [0u8; AES_BLOCK_SIZE],
            plaintext_iv: [0u8; AES_BLOCK_SIZE],
        }
    }

    /// `td::AesIgeState::init(key, iv, encrypt)`.
    pub fn init(&mut self, key: &[u8], iv: &[u8], encrypt: bool) {
        assert_eq!(key.len(), 32, "AES key must be 256-bit");
        assert_eq!(iv.len(), 32, "IGE IV must be 256-bit");
        // Only the native encrypt path uses the EVP context; a decrypt state
        // skips its setup entirely (the C++ initializes an ECB-decrypt `Evp`
        // for decrypt — its own backend — while the Rust decrypt path is
        // RustCrypto and never touches `evp`; creating and keying a CBC
        // context per one-shot decrypt state was pure waste, the dominant
        // cost of the 64-byte rows after the S98 pass).
        #[cfg(not(target_os = "emscripten"))]
        if encrypt {
            // SAFETY: the context is created here (or reused from a
            // previous `init` — re-`init` re-keys it), used on this thread
            // only, and freed exactly once by `Drop`.
            unsafe {
                if self.evp.is_null() {
                    self.evp = openssl_sys::EVP_CIPHER_CTX_new();
                    assert!(!self.evp.is_null(), "EVP_CIPHER_CTX_new failed");
                }
                let ok = openssl_sys::EVP_EncryptInit_ex(
                    self.evp,
                    openssl_sys::EVP_aes_256_cbc(),
                    std::ptr::null_mut(),
                    key.as_ptr(),
                    std::ptr::null(),
                );
                assert_eq!(ok, 1, "EVP_EncryptInit_ex failed");
                assert_eq!(
                    openssl_sys::EVP_CIPHER_CTX_set_padding(self.evp, 0),
                    1,
                    "EVP_CIPHER_CTX_set_padding failed"
                );
            }
        }
        // Only the RustCrypto paths below need the expanded key: the
        // native encrypt path runs through the EVP context above.
        #[cfg(not(target_os = "emscripten"))]
        let need_cipher = !encrypt;
        #[cfg(target_os = "emscripten")]
        let need_cipher = true;
        self.cipher = if need_cipher {
            Some(Aes256::new_from_slice(key).expect("32-byte key checked above"))
        } else {
            None
        };
        self.decrypt = !encrypt;
        self.encrypted_iv.copy_from_slice(&iv[..AES_BLOCK_SIZE]);
        self.plaintext_iv.copy_from_slice(&iv[AES_BLOCK_SIZE..]);
    }

    /// `td::AesIgeStateImpl::encrypt` — standard IGE:
    /// `y[i] = E(x[i] ^ y[i-1]) ^ x[i-1]`, with `x[-1] = plaintext_iv` and
    /// `y[-1] = encrypted_iv`. The mode is chunk-transparent (the chain
    /// resumes from the last output block), like the C++ 31-block chunks.
    ///
    /// Native: the C++ CBC-equivalence transformation — XOR pass, one bulk
    /// EVP CBC-encrypt per chunk, XOR pass (see the type docs). Emscripten:
    /// the direct single-pass recurrence, the mirror of
    /// [`decrypt`](Self::decrypt).
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

        #[cfg(not(target_os = "emscripten"))]
        {
            // The C++ `BLOCK_COUNT = 31`: stack working set, one bulk CBC
            // call per chunk.
            const CHUNK_BLOCKS: usize = 31;
            let mut in_pos = 0usize;
            let mut len = block_count;
            while len != 0 {
                let count = CHUNK_BLOCKS.min(len);
                let chunk_end = in_pos + count * AES_BLOCK_SIZE;
                let mut data = [[0u8; AES_BLOCK_SIZE]; CHUNK_BLOCKS];
                for (i, chunk) in from[in_pos..chunk_end]
                    .as_chunks::<AES_BLOCK_SIZE>()
                    .0
                    .iter()
                    .enumerate()
                {
                    data[i] = *chunk;
                }
                // d[0] = x[0]; d[1] = x[1] ^ plaintext_iv; d[i] = x[i] ^ x[i-2]
                // — the inputs of the equivalent CBC chain.
                let mut data_xored = data;
                if count > 1 {
                    data_xored[1] = xor_block(&data[1], &self.plaintext_iv);
                    for i in 2..count {
                        data_xored[i] = xor_block(&data[i], &data[i - 2]);
                    }
                }
                // Chain from encrypted_iv and CBC-encrypt in place (the C++
                // `init_iv` + one `EVP_EncryptUpdate`; the raw-pointer form
                // accepts `in == out`).
                let mut out_len: i32 = 0;
                // SAFETY: `self.evp` was initialized by `init` for CBC-256
                // with padding off; `data_xored` is `count * 16` bytes of
                // stack, read and written in lockstep by the same call.
                unsafe {
                    // The C++ `init_iv`: re-init with null cipher/key keeps
                    // both and only replaces the IV.
                    let ok = openssl_sys::EVP_EncryptInit_ex(
                        self.evp,
                        std::ptr::null(),
                        std::ptr::null_mut(),
                        std::ptr::null(),
                        self.encrypted_iv.as_ptr(),
                    );
                    assert_eq!(ok, 1, "EVP_EncryptInit_ex (iv) failed");
                    let ok = openssl_sys::EVP_EncryptUpdate(
                        self.evp,
                        data_xored.as_mut_ptr().cast(),
                        &mut out_len,
                        data_xored.as_ptr().cast(),
                        (count * AES_BLOCK_SIZE) as i32,
                    );
                    assert_eq!(ok, 1, "EVP_EncryptUpdate failed");
                }
                debug_assert_eq!(
                    out_len as usize,
                    count * AES_BLOCK_SIZE,
                    "IGE chunk wrote a partial block"
                );
                // y[0] = c[0] ^ plaintext_iv; y[i] = c[i] ^ x[i-1].
                data_xored[0] = xor_block(&data_xored[0], &self.plaintext_iv);
                for i in 1..count {
                    data_xored[i] = xor_block(&data_xored[i], &data[i - 1]);
                }
                self.plaintext_iv = data[count - 1];
                self.encrypted_iv = data_xored[count - 1];
                for (i, out_chunk) in out[in_pos..chunk_end]
                    .as_chunks_mut::<AES_BLOCK_SIZE>()
                    .0
                    .iter_mut()
                    .enumerate()
                {
                    *out_chunk = data_xored[i];
                }
                in_pos = chunk_end;
                len -= count;
            }
        }

        #[cfg(target_os = "emscripten")]
        {
            let cipher = self
                .cipher
                .as_ref()
                .expect("IGE encrypt on an uninitialized state");
            let mut y_prev = self.encrypted_iv;
            let mut x_prev = self.plaintext_iv;
            for (x, out_chunk) in from
                .as_chunks::<AES_BLOCK_SIZE>()
                .0
                .iter()
                .zip(out.as_chunks_mut::<AES_BLOCK_SIZE>().0.iter_mut())
            {
                let mut aes_block = aes::Block::clone_from_slice(&xor_block(x, &y_prev));
                cipher.encrypt_block(&mut aes_block);
                let c: [u8; AES_BLOCK_SIZE] = aes_block.as_slice().try_into().unwrap();
                let y = xor_block(&c, &x_prev);
                *out_chunk = y;
                x_prev = *x;
                y_prev = y;
            }
            self.plaintext_iv = x_prev;
            self.encrypted_iv = y_prev;
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
        let cipher = self
            .cipher
            .as_ref()
            .expect("IGE decrypt on an uninitialized state");
        for (in_block, out_block) in from.as_chunks::<AES_BLOCK_SIZE>().0.iter().zip(
            to[..from.len()]
                .as_chunks_mut::<AES_BLOCK_SIZE>()
                .0
                .iter_mut(),
        ) {
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
            cipher.decrypt_block(&mut aes_block);
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

impl Drop for AesIgeState {
    fn drop(&mut self) {
        // SAFETY: the context was created by `init` (if ever) and is freed
        // exactly once, here. emscripten has no field at all.
        #[cfg(not(target_os = "emscripten"))]
        unsafe {
            if !self.evp.is_null() {
                openssl_sys::EVP_CIPHER_CTX_free(self.evp);
            }
        }
    }
}

// SAFETY: an `EVP_CIPHER_CTX` is never used from two threads at once here —
// transports move the whole state between actors serialized by the actor
// system, the same single-threaded-use contract `ctr_native::AesCtrState`
// documents.
unsafe impl Send for AesIgeState {}

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

/// `aes_ige_encrypt` with `from == to` — the C++ handshake call sites pass
/// one `MutableSlice` as both arguments and mutate the blob in place.
pub fn aes_ige_encrypt_in_place(aes_key: &[u8], aes_iv: &mut [u8], data: &mut [u8]) {
    let input = data.to_vec();
    aes_ige_encrypt(aes_key, aes_iv, &input, data);
}

/// `aes_ige_decrypt` with `from == to` — the C++ handshake call sites pass
/// one `MutableSlice` as both arguments and mutate the blob in place.
pub fn aes_ige_decrypt_in_place(aes_key: &[u8], aes_iv: &mut [u8], data: &mut [u8]) {
    let input = data.to_vec();
    aes_ige_decrypt(aes_key, aes_iv, &input, data);
}

/// `td::aes_cbc_encrypt(key, iv, from, to)` — updates `iv` in place.
///
/// CBC *encryption* chains through the cipher (block i's input depends on
/// block i-1's output), so unlike [`aes_cbc_decrypt`] there is no wide pass
/// to take — the loop is latency-bound on the AES chain. Native builds run
/// one bulk EVP call (see [`cbc_native`], the exact C++ backend);
/// emscripten keeps the RustCrypto form below — copy in once, then per
/// block one fixed-size XOR + one in-place `encrypt_block` through the
/// same `&mut [aes::Block]` view the decrypt path uses.
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

    #[cfg(not(target_os = "emscripten"))]
    {
        cbc_native::cbc_encrypt_oneshot(aes_key, aes_iv, from, to);
    }

    #[cfg(target_os = "emscripten")]
    {
        let cipher = Aes256::new_from_slice(aes_key).expect("32-byte key checked above");
        let n_blocks = from.len() / AES_BLOCK_SIZE;
        let body = &mut to[..from.len()];
        body.copy_from_slice(from);
        // SAFETY: `aes::Block` is a 16-byte align-1 array and `body` is
        // 16-byte chunked (asserted above); the reborrow ends before `body`
        // is used again.
        let blocks: &mut [aes::Block] = unsafe {
            std::slice::from_raw_parts_mut(body.as_mut_ptr().cast::<aes::Block>(), n_blocks)
        };
        let mut chain: [u8; AES_BLOCK_SIZE] = [0; AES_BLOCK_SIZE];
        chain.copy_from_slice(aes_iv);
        for block in blocks {
            let plain: [u8; AES_BLOCK_SIZE] = block.as_slice().try_into().unwrap();
            block
                .as_mut_slice()
                .copy_from_slice(&xor_block(&plain, &chain));
            cipher.encrypt_block(block);
            chain = block.as_slice().try_into().unwrap();
        }
    }

    aes_iv.copy_from_slice(&to[from.len() - AES_BLOCK_SIZE..from.len()]);
}

/// `td::aes_cbc_decrypt(key, iv, from, to)` — updates `iv` in place to the
/// last *input* block, then decrypts (the C++ saves the incoming iv first).
///
/// Native builds run one bulk `EVP_DecryptUpdate` with `EVP_aes_256_cbc`
/// (padding off — see [`cbc_native`], the exact C++ backend; the S97 wide
/// `decrypt_blocks` pass it replaces measured ~1.3× under the C++ row).
/// Emscripten keeps the RustCrypto form below: one wide `decrypt_blocks`
/// pass (the aes crate's pipelined AES-NI decrypt path) followed by a
/// vectorizable XOR with each preceding ciphertext block.
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

    #[cfg(not(target_os = "emscripten"))]
    {
        // The C++ order: decrypt under the *incoming* iv first
        // (`evp.init_iv`), then set the out iv to the last input block.
        cbc_native::cbc_decrypt_oneshot(aes_key, aes_iv, from, to);
        aes_iv.copy_from_slice(&from[from.len() - AES_BLOCK_SIZE..]);
    }

    #[cfg(target_os = "emscripten")]
    {
        let cipher = Aes256::new_from_slice(aes_key).expect("32-byte key checked above");
        let n_blocks = from.len() / AES_BLOCK_SIZE;
        // The chaining value of block 0 is the caller's iv; `aes_iv` is
        // overwritten with the last input block below, so save it first.
        let mut first_chain = [0u8; AES_BLOCK_SIZE];
        first_chain.copy_from_slice(aes_iv);
        aes_iv.copy_from_slice(&from[from.len() - AES_BLOCK_SIZE..]);
        // Copy the ciphertext in, decrypt it in place with one wide pass, then
        // re-chain each plaintext block with its preceding ciphertext block.
        let body = &mut to[..from.len()];
        body.copy_from_slice(from);
        // SAFETY: `aes::Block` is a 16-byte align-1 array and `body` is
        // 16-byte chunked (asserted above); the reborrow ends before `body`
        // is used again.
        let blocks: &mut [aes::Block] = unsafe {
            std::slice::from_raw_parts_mut(body.as_mut_ptr().cast::<aes::Block>(), n_blocks)
        };
        cipher.decrypt_blocks(blocks);
        for i in 0..n_blocks {
            let chain: &[u8] = if i == 0 {
                &first_chain
            } else {
                &from[(i - 1) * AES_BLOCK_SIZE..i * AES_BLOCK_SIZE]
            };
            let block = &mut body[i * AES_BLOCK_SIZE..(i + 1) * AES_BLOCK_SIZE];
            for (b, c) in block.iter_mut().zip(chain) {
                *b ^= c;
            }
        }
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

// ---- pq factorization (tdutils crypto.cpp, used by the S60 handshake) ----

/// `pq_gcd` — the even-adjusted binary GCD of crypto.cpp.
fn pq_gcd(mut a: u64, mut b: u64) -> u64 {
    if a == 0 {
        return b;
    }
    while a & 1 == 0 {
        a >>= 1;
    }
    debug_assert!(b & 1 != 0);

    loop {
        if a > b {
            a = (a - b) >> 1;
            while a & 1 == 0 {
                a >>= 1;
            }
        } else if b > a {
            b = (b - a) >> 1;
            while b & 1 == 0 {
                b >>= 1;
            }
        } else {
            return a;
        }
    }
}

/// `pq_add_mul` — returns `(c + a * b) % pq`.
fn pq_add_mul(mut c: u64, mut a: u64, mut b: u64, pq: u64) -> u64 {
    while b != 0 {
        if b & 1 != 0 {
            c += a;
            if c >= pq {
                c -= pq;
            }
        }
        a += a;
        if a >= pq {
            a -= pq;
        }
        b >>= 1;
    }
    c
}

/// `pq_factorize(uint64)` — Pollard-Brent over 63-bit `pq`; returns 1 when
/// the input is degenerate.
pub fn pq_factorize_u64(pq: u64) -> u64 {
    if pq <= 2 || pq > (1u64 << 63) {
        return 1;
    }
    if pq & 1 == 0 {
        return 2;
    }
    let mut g = 0;
    let mut iter = 0;
    let mut i = 0;
    while i < 3 || iter < 1000 {
        let q = (crate::random::fast(17, 32) as u64) % (pq - 1);
        let mut x = (crate::random::fast_uint64()) % (pq - 1) + 1;
        let mut y = x;
        let lim = 1u64 << (i.min(5) + 18);
        for j in 1..lim {
            iter += 1;
            x = pq_add_mul(q, x, x, pq);
            let z = if x < y { pq + x - y } else { x - y };
            g = pq_gcd(z, pq);
            if g != 1 {
                break;
            }
            if j & (j - 1) == 0 {
                y = x;
            }
        }
        if g > 1 && g < pq {
            break;
        }
        i += 1;
    }
    if let Some(other) = pq.checked_div(g).filter(|&other| other < g) {
        g = other;
    }
    g
}

/// `pq_factorize_big` — the BigNum Pollard variant for 64..128-bit `pq`.
pub fn pq_factorize_big(pq_str: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let mut context = crate::bignum::BigNumContext::new();
    let mut a = crate::bignum::BigNum::new();
    let mut b: crate::bignum::BigNum;
    let mut p = crate::bignum::BigNum::new();
    let mut q: crate::bignum::BigNum;
    let mut one = crate::bignum::BigNum::new();
    one.set_value(1);

    let pq = crate::bignum::BigNum::from_binary(pq_str);

    let mut found = false;
    let mut i = 0;
    let mut iter = 0;
    while !found && (i < 3 || iter < 1000) {
        let t = crate::random::fast(17, 32);
        a.set_value(crate::random::fast_uint32());
        b = a.clone();

        let lim = 1usize << (i + 23);
        for j in 1..lim {
            iter += 1;
            a = crate::bignum::BigNum::mod_mul(&a, &a, &pq, &mut context);
            a.add_word(t as u32);
            if crate::bignum::BigNum::compare(&a, &pq) >= 0 {
                a = crate::bignum::BigNum::sub(&a, &pq);
            }
            if crate::bignum::BigNum::compare(&a, &b) > 0 {
                q = crate::bignum::BigNum::sub(&a, &b);
            } else {
                q = crate::bignum::BigNum::sub(&b, &a);
            }
            p = crate::bignum::BigNum::gcd(&q, &pq, &mut context);
            if crate::bignum::BigNum::compare(&p, &one) != 0 {
                found = true;
                break;
            }
            if j & (j - 1) == 0 {
                b = a.clone();
            }
        }
        i += 1;
    }

    if found {
        // found implies p divides pq, so the division cannot fail.
        let (quotient, _) = crate::bignum::BigNum::div(&pq, &p, &mut context).unwrap();
        q = quotient;
        if crate::bignum::BigNum::compare(&p, &q) > 0 {
            std::mem::swap(&mut p, &mut q);
        }
        Some((p.to_binary(-1), q.to_binary(-1)))
    } else {
        None
    }
}

/// `pq_factorize(Slice)` — the dispatcher: small inputs use the u64 path,
/// larger ones the BigNum path. Returns `None` on failure (the C++ -1).
pub fn pq_factorize(pq_str: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let size = pq_str.len();
    if size > 8 || (size == 8 && (pq_str[0] & 128) != 0) {
        return pq_factorize_big(pq_str);
    }

    let mut pq: u64 = 0;
    for &byte in pq_str {
        pq = (pq << 8) | u64::from(byte);
    }

    let p = pq_factorize_u64(pq);
    if p == 0 || !pq.is_multiple_of(p) {
        return None;
    }
    // as_big_endian_string: trim leading zero bytes (the high end of the LE
    // representation), then reverse to big-endian digits.
    let mut p_bytes = p.to_le_bytes().to_vec();
    while p_bytes.len() > 1 && *p_bytes.last().unwrap() == 0 {
        p_bytes.pop();
    }
    p_bytes.reverse();
    let mut q_bytes = (pq / p).to_le_bytes().to_vec();
    while q_bytes.len() > 1 && *q_bytes.last().unwrap() == 0 {
        q_bytes.pop();
    }
    q_bytes.reverse();
    Some((p_bytes, q_bytes))
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
