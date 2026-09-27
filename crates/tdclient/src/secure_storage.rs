//! Port of `td/telegram/SecureStorage.{h,cpp}` (M.44) — the Telegram
//! Passport value-crypt layer: the value/secret hashes, the AES-CBC states
//! derived from the password secret, the `Secret`/`EncryptedSecret` pair,
//! and the `encrypt_value`/`decrypt_value` byte scheme over the padded
//! random-prefixed payloads.
//!
//! The crypto primitives ride the tdutils port that already exists on this
//! base: [`tdutils::crypto::AesCbcState`] (AES-256-CBC), [`Sha256State`],
//! `sha512`, `pbkdf2_sha512` and `random::secure_bytes` — no primitive was
//! missing, so nothing is implemented locally. The C++ `FileFd` legs
//! (`encrypt_file`/`decrypt_file`) ride `std::fs` the way the M.12
//! `files.rs` plane does.
//!
//! The C++ `BufferSlice`/`Slice` split collapses into `Vec<u8>`/`&[u8]`;
//! the C++ `DataView` streaming interface is preserved as a trait because
//! the hash/encrypt readers stream over concatenated views (the random
//! prefix glued to the value) in 128 KiB steps.

use tdutils::crypto::{AesCbcState, Sha256State, pbkdf2_sha512, sha256, sha512};
use tdutils::status::{Result, Status};
use tdutils::try_result;

/// The `AesCbcState` key size (SecureStorage.h:32-36 derive 32 key + 16 iv
/// bytes from a 64-byte hash).
const AES_CBC_KEY_SIZE: usize = 32;
const AES_CBC_IV_SIZE: usize = 16;
/// `data_view_for_each` step (SecureStorage.cpp:57).
const DATA_VIEW_STEP: i64 = 128 << 10;

/// `td::secure_storage::ValueHash` (SecureStorage.h:51-62) — the 32-byte
/// SHA-256 of the padded value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ValueHash {
    hash: [u8; 32],
}

impl ValueHash {
    /// `ValueHash::create` (SecureStorage.cpp:22-29) — checks the size.
    pub fn create(data: &[u8]) -> Result<ValueHash> {
        if data.len() != 32 {
            return Result::Err(Status::error(0, &format!("Wrong hash size {}", data.len())));
        }
        let mut hash = [0u8; 32];
        hash.copy_from_slice(data);
        Result::Ok(ValueHash { hash })
    }

    fn from_raw(hash: [u8; 32]) -> ValueHash {
        ValueHash { hash }
    }

    /// `ValueHash::as_slice` (SecureStorage.h:56-58).
    pub fn as_slice(&self) -> &[u8] {
        &self.hash
    }
}

/// `td::secure_storage::DataView` (SecureStorage.h:64-75) — the streaming
/// byte-source interface; `pread` reads a bounded chunk.
pub trait DataView {
    /// `DataView::size`.
    fn size(&self) -> i64;

    /// `DataView::pread(offset, size)`.
    fn pread(&self, offset: i64, size: i64) -> Result<Vec<u8>>;
}

/// `td::secure_storage::BufferSliceDataView` (SecureStorage.cpp:119-132).
pub struct BufferSliceDataView {
    buffer_slice: Vec<u8>,
}

impl BufferSliceDataView {
    pub fn new(buffer_slice: Vec<u8>) -> BufferSliceDataView {
        BufferSliceDataView { buffer_slice }
    }
}

impl DataView for BufferSliceDataView {
    fn size(&self) -> i64 {
        self.buffer_slice.len() as i64
    }

    fn pread(&self, offset: i64, size: i64) -> Result<Vec<u8>> {
        let end_offset = size + offset;
        if self.size() < end_offset {
            return Result::Err(Status::error(0, "Not enough data in BufferSlice"));
        }
        Result::Ok(self.buffer_slice[offset as usize..(offset + size) as usize].to_vec())
    }
}

/// `td::secure_storage::ConcatDataView` (SecureStorage.cpp:134-170) — two
/// views glued together (the random prefix + the value).
pub struct ConcatDataView<'a, 'b> {
    left: &'a dyn DataView,
    right: &'b dyn DataView,
}

impl<'a, 'b> ConcatDataView<'a, 'b> {
    pub fn new(left: &'a dyn DataView, right: &'b dyn DataView) -> ConcatDataView<'a, 'b> {
        ConcatDataView { left, right }
    }
}

impl DataView for ConcatDataView<'_, '_> {
    fn size(&self) -> i64 {
        self.left.size() + self.right.size()
    }

    fn pread(&self, offset: i64, size: i64) -> Result<Vec<u8>> {
        let end_offset = size + offset;
        if self.size() < end_offset {
            return Result::Err(Status::error(0, "Not enough data in ConcatDataView"));
        }

        let substr = |slice: &dyn DataView, offset: i64, size: i64| -> Result<Vec<u8>> {
            let l = i64::max(0, offset);
            let r = i64::min(slice.size(), offset + size);
            if l >= r {
                return Result::Ok(Vec::new());
            }
            slice.pread(l, r - l)
        };

        try_result!(a_value, substr(self.left, offset, size));
        try_result!(b_value, substr(self.right, offset - self.left.size(), size));
        let (a, b) = (a_value, b_value);

        if a.is_empty() {
            return Result::Ok(b);
        }
        if b.is_empty() {
            return Result::Ok(a);
        }

        let mut res = Vec::with_capacity(a.len() + b.len());
        res.extend_from_slice(&a);
        res.extend_from_slice(&b);
        Result::Ok(res)
    }
}

/// The private `FileDataView` (SecureStorage.cpp:91-117) over a
/// `std::fs::File` — the `FileFd` leg of the C++.
struct FileDataView<'a> {
    fd: &'a std::fs::File,
    size: i64,
}

impl DataView for FileDataView<'_> {
    fn size(&self) -> i64 {
        self.size
    }

    fn pread(&self, offset: i64, size: i64) -> Result<Vec<u8>> {
        let mut slice = vec![0u8; size as usize];
        let actual_size = match crate::files::compat_read_at(self.fd, &mut slice, offset as u64) {
            Ok(size) => size,
            Err(_) => return Result::Err(Status::error(0, "Failed to pread file")),
        };
        if actual_size as i64 != size {
            return Result::Err(Status::error(0, "Not enough data in file"));
        }
        Result::Ok(slice)
    }
}

/// `calc_aes_cbc_state_hash` (SecureStorage.cpp:31-39): 32 key bytes then
/// 16 iv bytes out of the 64-byte hash.
fn calc_aes_cbc_state_hash(hash: &[u8]) -> AesCbcState {
    assert_eq!(hash.len(), 64);
    AesCbcState::new(&hash[..AES_CBC_KEY_SIZE], &hash[32..32 + AES_CBC_IV_SIZE])
}

/// `calc_aes_cbc_state_pbkdf2` (SecureStorage.cpp:41-46): 100000
/// PBKDF2-HMAC-SHA-512 iterations.
pub fn calc_aes_cbc_state_pbkdf2(secret: &[u8], salt: &[u8]) -> AesCbcState {
    let mut hash = [0u8; 64];
    pbkdf2_sha512(secret, salt, 100000, &mut hash);
    calc_aes_cbc_state_hash(&hash)
}

/// `calc_aes_cbc_state_sha512` (SecureStorage.cpp:48-53).
pub fn calc_aes_cbc_state_sha512(seed: &[u8]) -> AesCbcState {
    let hash = sha512(seed);
    calc_aes_cbc_state_hash(&hash)
}

/// `data_view_for_each` (SecureStorage.cpp:55-63): stream the view in
/// `DATA_VIEW_STEP` chunks.
fn data_view_for_each(data: &dyn DataView, mut f: impl FnMut(Vec<u8>) -> Result<()>) -> Result<()> {
    let size = data.size();
    let mut i = 0;
    while i < size {
        try_result!(bytes, data.pread(i, i64::min(DATA_VIEW_STEP, size - i)));
        if let Result::Err(status) = f(bytes) {
            return Result::Err(status);
        }
        i += DATA_VIEW_STEP;
    }
    Result::Ok(())
}

/// `calc_value_hash(const DataView &)` (SecureStorage.cpp:65-75) — the
/// streaming SHA-256 (the overload-set suffix follows the Rust-port
/// no-overloading convention).
pub fn calc_value_hash_view(data_view: &dyn DataView) -> Result<ValueHash> {
    let mut state = Sha256State::new();
    state.init();
    if let Result::Err(status) = data_view_for_each(data_view, |bytes| {
        state.feed(&bytes);
        Result::Ok(())
    }) {
        return Result::Err(status);
    }
    let mut res = [0u8; 32];
    state.extract(&mut res, false);
    Result::Ok(ValueHash::from_raw(res))
}

/// `calc_value_hash(Slice)` (SecureStorage.cpp:77-81).
pub fn calc_value_hash(data: &[u8]) -> ValueHash {
    ValueHash::from_raw(sha256(data))
}

/// `gen_random_prefix` (SecureStorage.cpp:83-89): pad the value to a whole
/// AES block with at least 32 random bytes; the first byte stores the
/// prefix length.
pub fn gen_random_prefix(data_size: i64) -> Vec<u8> {
    let size = ((32 + 15 + data_size) & -16) - data_size;
    let mut buff = vec![0u8; size as usize];
    tdutils::random::secure_bytes(&mut buff);
    buff[0] = buff.len() as u8;
    assert!((buff.len() as i64 + data_size) % 16 == 0);
    buff
}

/// `td::secure_storage::Password` (SecureStorage.h:104-111).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Password {
    password: String,
}

impl Password {
    pub fn new(password: String) -> Password {
        Password { password }
    }

    /// `Password::as_slice` (SecureStorage.cpp:175-177).
    pub fn as_slice(&self) -> &[u8] {
        self.password.as_bytes()
    }
}

/// `td::secure_storage::EnryptionAlgorithm` (SecureStorage.h:115) — the
/// C++ typo in the name is preserved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnryptionAlgorithm {
    Sha512,
    Pbkdf2,
}

/// `secret_checksum` (SecureStorage.cpp:179-185): the byte that must make
/// `sum % 255 == 239` (`(255 + 239 - sum % 255) % 255`).
fn secret_checksum(secret: &[u8]) -> u8 {
    let mut sum: u32 = 0;
    for &c in secret {
        sum += c as u32;
    }
    ((255 + 239 - sum % 255) % 255) as u8
}

/// `td::secure_storage::Secret` (SecureStorage.h:117-136) — the 32-byte
/// value secret with `sum % 255 == 239` plus the cached `get_hash`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Secret {
    secret: [u8; 32],
    hash: i64,
}

impl Secret {
    /// `Secret::create` (SecureStorage.cpp:187-202).
    pub fn create(secret: &[u8]) -> Result<Secret> {
        if secret.len() != 32 {
            return Result::Err(Status::error(0, "Wrong secret size"));
        }
        let checksum = secret_checksum(secret);
        if checksum != 0 {
            return Result::Err(Status::error(0, &format!("Wrong checksum {checksum}")));
        }
        let mut res = [0u8; 32];
        res.copy_from_slice(secret);

        let secret_sha256 = sha256(secret);
        let hash = i64::from_le_bytes(secret_sha256[..8].try_into().unwrap());
        Result::Ok(Secret { secret: res, hash })
    }

    /// `Secret::create_new` (SecureStorage.cpp:204-212): random bytes with
    /// the first byte adjusted to fix the checksum.
    pub fn create_new() -> Secret {
        let mut secret = [0u8; 32];
        tdutils::random::secure_bytes(&mut secret);
        let checksum_diff = secret_checksum(&secret);
        let new_byte = ((secret[0] as u32 + checksum_diff as u32) % 255) as u8;
        secret[0] = new_byte;
        Secret::create(&secret)
            .unwrap_or_else(|_| unreachable!("create_new builds a valid checksum"))
    }

    /// `Secret::as_slice` (SecureStorage.cpp:214-216).
    pub fn as_slice(&self) -> &[u8] {
        &self.secret
    }

    /// `Secret::get_hash` (SecureStorage.cpp:218-220).
    pub fn get_hash(&self) -> i64 {
        self.hash
    }

    /// `Secret::encrypt` (SecureStorage.cpp:226-242).
    pub fn encrypt(
        &self,
        key: &[u8],
        salt: &[u8],
        algorithm: EnryptionAlgorithm,
    ) -> EncryptedSecret {
        let mut aes_cbc_state = match algorithm {
            EnryptionAlgorithm::Sha512 => {
                let mut seed = Vec::new();
                seed.extend_from_slice(salt);
                seed.extend_from_slice(key);
                seed.extend_from_slice(salt);
                calc_aes_cbc_state_sha512(&seed)
            }
            EnryptionAlgorithm::Pbkdf2 => calc_aes_cbc_state_pbkdf2(key, salt),
        };

        let mut res = [0u8; 32];
        aes_cbc_state.encrypt(self.as_slice(), &mut res);
        EncryptedSecret::create(&res).unwrap()
    }
}

/// `td::secure_storage::EncryptedSecret` (SecureStorage.h:138-147).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncryptedSecret {
    encrypted_secret: [u8; 32],
}

impl EncryptedSecret {
    /// `EncryptedSecret::create` (SecureStorage.cpp:247-254).
    pub fn create(encrypted_secret: &[u8]) -> Result<EncryptedSecret> {
        if encrypted_secret.len() != 32 {
            return Result::Err(Status::error(0, "Wrong encrypted secret size"));
        }
        let mut res = [0u8; 32];
        res.copy_from_slice(encrypted_secret);
        Result::Ok(EncryptedSecret {
            encrypted_secret: res,
        })
    }

    /// `EncryptedSecret::decrypt` (SecureStorage.cpp:256-272).
    pub fn decrypt(
        &self,
        key: &[u8],
        salt: &[u8],
        algorithm: EnryptionAlgorithm,
    ) -> Result<Secret> {
        let mut aes_cbc_state = match algorithm {
            EnryptionAlgorithm::Sha512 => {
                let mut seed = Vec::new();
                seed.extend_from_slice(salt);
                seed.extend_from_slice(key);
                seed.extend_from_slice(salt);
                calc_aes_cbc_state_sha512(&seed)
            }
            EnryptionAlgorithm::Pbkdf2 => calc_aes_cbc_state_pbkdf2(key, salt),
        };

        let mut res = [0u8; 32];
        aes_cbc_state.decrypt(self.as_slice(), &mut res);
        Secret::create(&res)
    }

    /// `EncryptedSecret::as_slice` (SecureStorage.cpp:274-276).
    pub fn as_slice(&self) -> &[u8] {
        &self.encrypted_secret
    }
}

/// The `Decryptor` (SecureStorage.h:150-161, cpp:281-317) — CBC-decrypts
/// the stream while hashing the decrypted payload, stripping the length
/// byte + random prefix on the first block.
pub struct Decryptor {
    aes_cbc_state: AesCbcState,
    sha256_state: Sha256State,
    skipped_prefix: bool,
    to_skip: usize,
}

impl Decryptor {
    /// `Decryptor::Decryptor` (SecureStorage.cpp:281-283).
    pub fn new(aes_cbc_state: AesCbcState) -> Decryptor {
        let mut sha256_state = Sha256State::new();
        sha256_state.init();
        Decryptor {
            aes_cbc_state,
            sha256_state,
            skipped_prefix: false,
            to_skip: 0,
        }
    }

    /// `Decryptor::append` (SecureStorage.cpp:285-304).
    pub fn append(&mut self, data: Vec<u8>) -> Result<Vec<u8>> {
        if data.is_empty() {
            return Result::Ok(Vec::new());
        }
        if !data.len().is_multiple_of(16) {
            return Result::Err(Status::error(0, "Part size must be divisible by 16"));
        }
        // the C++ decrypts in place; the Rust CBC takes disjoint buffers
        let mut decrypted = vec![0u8; data.len()];
        self.aes_cbc_state.decrypt(&data, &mut decrypted);
        self.sha256_state.feed(&decrypted);
        if !self.skipped_prefix {
            self.to_skip = decrypted[0] as usize;
            let to_skip = usize::min(self.to_skip, decrypted.len());
            if self.to_skip > decrypted.len() {
                self.to_skip = 0; // to fail final to_skip check
            }
            self.skipped_prefix = true;
            decrypted = decrypted.split_off(to_skip);
        }
        Result::Ok(decrypted)
    }

    /// `Decryptor::finish` (SecureStorage.cpp:306-317).
    pub fn finish(&mut self) -> Result<ValueHash> {
        if !self.skipped_prefix {
            return Result::Err(Status::error(0, "No data was given"));
        }
        if self.to_skip < 32 {
            return Result::Err(Status::error(0, "Too small random prefix"));
        }

        let mut res = [0u8; 32];
        self.sha256_state.extract(&mut res, true);
        Result::Ok(ValueHash::from_raw(res))
    }
}

/// The `Encryptor` (SecureStorage.h:164-174, cpp:319-338) — CBC-encrypts
/// the view on read, sequentially. The C++ `mutable` members
/// (`aes_cbc_state_`/`current_offset_`) mutate through the `DataView`
/// `&self` interface, so they ride the cell/refcell pair here.
pub struct Encryptor<'a> {
    aes_cbc_state: std::cell::RefCell<AesCbcState>,
    current_offset: std::cell::Cell<i64>,
    data_view: &'a dyn DataView,
}

impl<'a> Encryptor<'a> {
    /// `Encryptor::Encryptor` (SecureStorage.cpp:319-321).
    pub fn new(aes_cbc_state: AesCbcState, data_view: &'a dyn DataView) -> Encryptor<'a> {
        Encryptor {
            aes_cbc_state: std::cell::RefCell::new(aes_cbc_state),
            current_offset: std::cell::Cell::new(0),
            data_view,
        }
    }

    /// `Encryptor::size` (SecureStorage.cpp:323-325).
    pub fn size(&self) -> i64 {
        self.data_view.size()
    }

    /// `Encryptor::pread` (SecureStorage.cpp:327-338).
    pub fn pread(&self, offset: i64, size: i64) -> Result<Vec<u8>> {
        if offset != self.current_offset.get() {
            return Result::Err(Status::error(0, "Arbitrary offset is not supported"));
        }
        if size % 16 != 0 {
            return Result::Err(Status::error(0, "Part size must be divisible by 16"));
        }
        try_result!(part_value, self.data_view.pread(offset, size));
        let mut part = part_value;
        let mut encrypted = vec![0u8; part.len()];
        self.aes_cbc_state
            .borrow_mut()
            .encrypt(&part, &mut encrypted);
        part = encrypted;
        self.current_offset.set(self.current_offset.get() + size);
        Result::Ok(part)
    }
}

impl DataView for Encryptor<'_> {
    fn size(&self) -> i64 {
        Encryptor::size(self)
    }

    fn pread(&self, offset: i64, size: i64) -> Result<Vec<u8>> {
        Encryptor::pread(self, offset, size)
    }
}

/// `td::secure_storage::EncryptedValue` (SecureStorage.h:181-184).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncryptedValue {
    pub data: Vec<u8>,
    pub hash: ValueHash,
}

/// `td::format::as_hex_dump<4>` — lowercase hex with a space between
/// 4-byte groups (the `decrypt_value` error text rides it).
fn as_hex_dump_4(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 3);
    for (i, byte) in bytes.iter().enumerate() {
        if i != 0 && i % 4 == 0 {
            out.push(' ');
        }
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// `encrypt_value` (SecureStorage.cpp:340-351): hash
/// `random_prefix || value`, then AES-CBC-encrypt the concatenation.
pub fn encrypt_value(secret: &Secret, data: &[u8]) -> Result<EncryptedValue> {
    let random_prefix_view = BufferSliceDataView::new(gen_random_prefix(data.len() as i64));
    let data_view = BufferSliceDataView::new(data.to_vec());
    let full_view = ConcatDataView::new(&random_prefix_view, &data_view);

    try_result!(hash, calc_value_hash_view(&full_view));

    let mut seed = Vec::new();
    seed.extend_from_slice(secret.as_slice());
    seed.extend_from_slice(hash.as_slice());
    let aes_cbc_state = calc_aes_cbc_state_sha512(&seed);
    let encryptor = Encryptor::new(aes_cbc_state, &full_view);
    let size = encryptor.size();
    try_result!(encrypted_data, encryptor.pread(0, size));
    Result::Ok(EncryptedValue {
        data: encrypted_data,
        hash,
    })
}

/// `decrypt_value` (SecureStorage.cpp:353-363).
pub fn decrypt_value(secret: &Secret, hash: &ValueHash, data: &[u8]) -> Result<Vec<u8>> {
    let mut seed = Vec::new();
    seed.extend_from_slice(secret.as_slice());
    seed.extend_from_slice(hash.as_slice());
    let aes_cbc_state = calc_aes_cbc_state_sha512(&seed);
    let mut decryptor = Decryptor::new(aes_cbc_state);
    try_result!(decrypted_value, decryptor.append(data.to_vec()));
    try_result!(stored_hash, decryptor.finish());
    if stored_hash.as_slice() != hash.as_slice() {
        return Result::Err(Status::error(
            0,
            &format!(
                "Hash mismatch {} {}",
                as_hex_dump_4(stored_hash.as_slice()),
                as_hex_dump_4(hash.as_slice())
            ),
        ));
    }
    Result::Ok(decrypted_value)
}

/// `encrypt_file` (SecureStorage.cpp:365-381).
pub fn encrypt_file(secret: &Secret, src: &str, dest: &str) -> Result<ValueHash> {
    let src_file = match std::fs::File::open(src) {
        Ok(file) => file,
        Err(_) => return Result::Err(Status::error(0, "Failed to open source file")),
    };
    let dest_file = match std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(dest)
    {
        Ok(file) => file,
        Err(_) => return Result::Err(Status::error(0, "Failed to open destination file")),
    };
    let src_file_size = match src_file.metadata() {
        Ok(metadata) => metadata.len() as i64,
        Err(_) => return Result::Err(Status::error(0, "Failed to get source file size")),
    };

    let random_prefix_view = BufferSliceDataView::new(gen_random_prefix(src_file_size));
    let data_view = FileDataView {
        fd: &src_file,
        size: src_file_size,
    };
    let full_view = ConcatDataView::new(&random_prefix_view, &data_view);

    try_result!(hash, calc_value_hash_view(&full_view));

    let mut seed = Vec::new();
    seed.extend_from_slice(secret.as_slice());
    seed.extend_from_slice(hash.as_slice());
    let aes_cbc_state = calc_aes_cbc_state_sha512(&seed);
    let encryptor = Encryptor::new(aes_cbc_state, &full_view);
    {
        use std::io::Write;
        let mut dest_writer = std::io::BufWriter::new(&dest_file);
        let write_result =
            data_view_for_each(&encryptor, |bytes| match dest_writer.write_all(&bytes) {
                Ok(()) => Result::Ok(()),
                Err(_) => Result::Err(Status::error(0, "Failed to write destination file")),
            });
        if let Result::Err(status) = write_result {
            return Result::Err(status);
        }
    }
    Result::Ok(hash)
}

/// `decrypt_file` (SecureStorage.cpp:383-405).
pub fn decrypt_file(secret: &Secret, hash: &ValueHash, src: &str, dest: &str) -> Result<()> {
    let src_file = match std::fs::File::open(src) {
        Ok(file) => file,
        Err(_) => return Result::Err(Status::error(0, "Failed to open source file")),
    };
    let dest_file = match std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(dest)
    {
        Ok(file) => file,
        Err(_) => return Result::Err(Status::error(0, "Failed to open destination file")),
    };
    let src_file_size = match src_file.metadata() {
        Ok(metadata) => metadata.len() as i64,
        Err(_) => return Result::Err(Status::error(0, "Failed to get source file size")),
    };

    let src_file_view = FileDataView {
        fd: &src_file,
        size: src_file_size,
    };

    let mut seed = Vec::new();
    seed.extend_from_slice(secret.as_slice());
    seed.extend_from_slice(hash.as_slice());
    let aes_cbc_state = calc_aes_cbc_state_sha512(&seed);
    let mut decryptor = Decryptor::new(aes_cbc_state);
    {
        use std::io::Write;
        let mut dest_writer = std::io::BufWriter::new(&dest_file);
        let write_result = data_view_for_each(&src_file_view, |bytes| {
            try_result!(decrypted_bytes, decryptor.append(bytes));
            match dest_writer.write_all(&decrypted_bytes) {
                Ok(()) => Result::Ok(()),
                Err(_) => Result::Err(Status::error(0, "Failed to write destination file")),
            }
        });
        if let Result::Err(status) = write_result {
            return Result::Err(status);
        }
    }

    try_result!(stored_hash, decryptor.finish());

    if hash.as_slice() != stored_hash.as_slice() {
        return Result::Err(Status::error(0, "Hash mismatch"));
    }

    Result::Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed 32-byte secret with the valid `sum % 255 == 239` checksum,
    /// precomputed once so the tests are deterministic.
    fn fixed_secret() -> Secret {
        // checksum byte fix-up mirrors Secret::create_new but on a fixed base
        let mut bytes = [0x42u8; 32];
        bytes[0] = 0x00;
        let diff = secret_checksum(&bytes);
        bytes[0] = ((bytes[0] as u32 + diff as u32) % 255) as u8;
        Secret::create(&bytes).unwrap()
    }

    #[test]
    fn secret_checksum_matches_the_cpp_formula() {
        // sum of [0x42 * 31 + 0x??] must land on 239 mod 255
        let secret = fixed_secret();
        let sum: u32 = secret.as_slice().iter().map(|&c| c as u32).sum();
        assert_eq!(sum % 255, 239);

        // wrong sizes and broken checksums are rejected
        assert!(Secret::create(&[0u8; 31]).is_error());
        assert!(Secret::create(&[0u8; 33]).is_error());
        let mut broken = [0u8; 32];
        broken.copy_from_slice(secret.as_slice());
        broken[5] ^= 0xFF;
        assert!(Secret::create(&broken).is_error());
    }

    #[test]
    fn secret_encrypt_decrypt_round_trip_sha512() {
        let secret = fixed_secret();
        let key = b"key-bytes";
        let salt = b"salt";
        let encrypted = secret.encrypt(key, salt, EnryptionAlgorithm::Sha512);
        assert_eq!(encrypted.as_slice().len(), 32);
        let decrypted = encrypted
            .decrypt(key, salt, EnryptionAlgorithm::Sha512)
            .unwrap();
        assert_eq!(decrypted.as_slice(), secret.as_slice());

        // a wrong key decrypts to garbage that fails the checksum
        assert!(
            encrypted
                .decrypt(b"other-key", salt, EnryptionAlgorithm::Sha512)
                .is_error()
        );
    }

    #[test]
    fn encrypted_secret_size_is_checked() {
        assert!(EncryptedSecret::create(&[0u8; 31]).is_error());
        assert!(EncryptedSecret::create(&[0u8; 32]).is_ok());
    }

    #[test]
    fn value_hash_create_checks_the_size() {
        assert!(ValueHash::create(&[0u8; 31]).is_error());
        assert_eq!(
            ValueHash::create(&[7u8; 32]).unwrap().as_slice(),
            &[7u8; 32]
        );
    }

    #[test]
    fn calc_value_hash_slice_matches_plain_sha256() {
        let data = b"some passport value";
        let expected = sha256(data);
        assert_eq!(calc_value_hash(data).as_slice(), &expected);
    }

    #[test]
    fn calc_value_hash_view_matches_the_slice_one() {
        let data = b"another passport value";
        let view = BufferSliceDataView::new(data.to_vec());
        assert_eq!(
            calc_value_hash_view(&view).unwrap().as_slice(),
            calc_value_hash(data).as_slice()
        );

        // the concatenated view hashes prefix || value
        let prefix = vec![1u8; 32];
        let prefix_view = BufferSliceDataView::new(prefix.clone());
        let data_view = BufferSliceDataView::new(data.to_vec());
        let concat = ConcatDataView::new(&prefix_view, &data_view);
        let mut joined = prefix.clone();
        joined.extend_from_slice(data);
        assert_eq!(
            calc_value_hash_view(&concat).unwrap().as_slice(),
            calc_value_hash(&joined).as_slice()
        );
    }

    #[test]
    fn gen_random_prefix_pads_to_a_whole_block_with_length_byte() {
        for data_size in [0i64, 1, 15, 16, 31, 100, 1000] {
            let prefix = gen_random_prefix(data_size);
            assert!((prefix.len() as i64 + data_size) % 16 == 0);
            assert!(prefix.len() >= 32);
            assert_eq!(prefix[0] as usize, prefix.len());
        }
    }

    #[test]
    fn encrypt_decrypt_value_round_trip() {
        let secret = Secret::create_new();
        for payload in [&b"hello passport"[..], &[0u8; 64][..], b"x".as_slice()] {
            let encrypted = encrypt_value(&secret, payload).unwrap();
            // one AES block per (prefix || payload) block, prefix >= 32
            assert!(!encrypted.data.is_empty());
            assert_eq!(encrypted.data.len() % 16, 0);
            let decrypted = decrypt_value(&secret, &encrypted.hash, &encrypted.data).unwrap();
            assert_eq!(decrypted, payload);
        }
    }

    #[test]
    fn decrypt_value_detects_corruption() {
        let secret = Secret::create_new();
        let payload = b"the value that must not change";
        let encrypted = encrypt_value(&secret, payload).unwrap();

        // a flipped ciphertext byte breaks the stored hash
        let mut corrupted = encrypted.data.clone();
        corrupted[10] ^= 0xFF;
        assert!(decrypt_value(&secret, &encrypted.hash, &corrupted).is_error());

        // a wrong hash breaks it too
        let wrong_hash = calc_value_hash(b"other");
        assert!(decrypt_value(&secret, &wrong_hash, &encrypted.data).is_error());
    }

    #[test]
    fn decryptor_rejects_unaligned_parts_and_empty_finish() {
        let mut decryptor = Decryptor::new(calc_aes_cbc_state_sha512(b"seed"));
        assert!(decryptor.append(vec![0u8; 15]).is_error());
        assert!(decryptor.append(Vec::new()).is_ok());
        assert!(decryptor.finish().is_error());
    }

    #[test]
    fn encryptor_rejects_arbitrary_offsets_and_unaligned_parts() {
        let view = BufferSliceDataView::new(vec![0u8; 48]);
        let encryptor = Encryptor::new(calc_aes_cbc_state_sha512(b"seed"), &view);
        assert!(encryptor.pread(16, 32).is_error());
        assert!(encryptor.pread(0, 5).is_error());
        assert!(encryptor.pread(0, 32).is_ok());
        // sequential-only: the offset moved forward
        assert!(encryptor.pread(0, 16).is_error());
        assert!(encryptor.pread(32, 16).is_ok());
    }

    #[test]
    fn encrypt_file_decrypt_file_round_trip() {
        let dir = std::env::temp_dir().join(format!(
            "td_secure_storage_test_{}_{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.bin");
        let encrypted_path = dir.join("enc.bin");
        let decrypted_path = dir.join("dec.bin");
        let payload: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&src, &payload).unwrap();

        let secret = Secret::create_new();
        let hash = encrypt_file(
            &secret,
            src.to_str().unwrap(),
            encrypted_path.to_str().unwrap(),
        )
        .unwrap();
        decrypt_file(
            &secret,
            &hash,
            encrypted_path.to_str().unwrap(),
            decrypted_path.to_str().unwrap(),
        )
        .unwrap();
        let decrypted = std::fs::read(&decrypted_path).unwrap();
        assert_eq!(decrypted, payload);

        // tampering with the encrypted file fails the final hash check
        let mut bytes = std::fs::read(&encrypted_path).unwrap();
        bytes[20] ^= 0xFF;
        std::fs::write(&encrypted_path, &bytes).unwrap();
        assert!(
            decrypt_file(
                &secret,
                &hash,
                encrypted_path.to_str().unwrap(),
                decrypted_path.to_str().unwrap(),
            )
            .is_error()
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn password_and_view_helpers() {
        let password = Password::new("pass".to_owned());
        assert_eq!(password.as_slice(), b"pass");

        // pread bounds of the buffer view and the concat view
        let view = BufferSliceDataView::new(b"0123456789".to_vec());
        assert_eq!(view.size(), 10);
        assert_eq!(view.pread(2, 3).unwrap(), b"234");
        assert!(view.pread(8, 3).is_error());

        let empty = BufferSliceDataView::new(Vec::new());
        let concat = ConcatDataView::new(&view, &empty);
        assert_eq!(concat.size(), 10);
        assert_eq!(concat.pread(0, 10).unwrap(), b"0123456789");
        assert!(concat.pread(0, 11).is_error());
    }
}
