//! Port of `td/mtproto/Transport.cpp` (S57): the MTProto packet crypto
//! layer — v1/v2 headers, msg_key computation, IGE encrypt/decrypt and the
//! no-crypto form.

use crate::auth_key::AuthKey;
use crate::kdf::{kdf, kdf2};
use crate::message_id::MessageId;
use crate::packet_info::{PacketInfo, PacketType};
use tdutils::buffer::BufferWriter;
use tdutils::crypto::{aes_ige_decrypt, aes_ige_encrypt, sha1, sha256};
use tdutils::random;
use tdutils::status::{Result, Status};

// Layout constants (little-endian, matching the C++ packed structs).
// CryptoHeader: auth_key_id + message_key + encrypted salt + encrypted
// session_id (sizeof 40, encrypted part 16, prefix 16 at offset 40).
const CRYPTO_HEADER_SIZE: usize = 40;
const CRYPTO_ENCRYPTED_HEADER_SIZE: usize = 16;
const CRYPTO_PREFIX_SIZE: usize = 16;
const CRYPTO_ENCRYPT_BEGIN: usize = 24;
// EndToEndHeader: auth_key_id + message_key (sizeof 24, encrypted part 0,
// prefix 4 at offset 24).
const E2E_HEADER_SIZE: usize = 24;
const E2E_ENCRYPTED_HEADER_SIZE: usize = 0;
const E2E_PREFIX_SIZE: usize = 4;
const E2E_ENCRYPT_BEGIN: usize = 24;
const NO_CRYPTO_HEADER_SIZE: usize = 8;

fn u64_at(data: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap())
}

fn u32_at(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap())
}

/// `Transport::calc_message_ack_and_key` (MTProto v1): SHA-1 over the
/// encrypted header plus data; ack has bit 31 set.
fn calc_message_ack_and_key(part: &[u8]) -> (u32, [u8; 16]) {
    let message_sha1 = sha1(part);
    let ack = u32::from_le_bytes(message_sha1[..4].try_into().unwrap()) | (1 << 31);
    let mut key = [0u8; 16];
    key.copy_from_slice(&message_sha1[4..20]);
    (ack, key)
}

/// `Transport::calc_message_key2` (MTProto v2): SHA-256 over
/// `auth_key[88+x..120+x] + to_encrypt`, key = bytes 8..24.
pub fn calc_message_key2(auth_key: &AuthKey, x: usize, to_encrypt: &[u8]) -> (u32, [u8; 16]) {
    let mut data = Vec::with_capacity(32 + to_encrypt.len());
    data.extend_from_slice(&auth_key.key()[88 + x..120 + x]);
    data.extend_from_slice(to_encrypt);
    let msg_key_large = sha256(&data);
    let ack = u32::from_le_bytes(msg_key_large[..4].try_into().unwrap()) | (1 << 31);
    let mut key = [0u8; 16];
    key.copy_from_slice(&msg_key_large[8..24]);
    (ack, key)
}

/// `Transport::calc_crypto_size` (v1).
fn calc_crypto_size(header_size: usize, encrypted_header_size: usize, data_size: usize) -> usize {
    let raw_size = header_size - encrypted_header_size;
    raw_size + ((encrypted_header_size + data_size + 15) & !15)
}

/// `Transport::calc_crypto_size2` (v2): the padded-size ladder, or a random
/// 0..256-byte pad.
fn calc_crypto_size2(
    header_size: usize,
    encrypted_header_size: usize,
    data_size: usize,
    use_random_padding: bool,
) -> usize {
    let raw_size = header_size - encrypted_header_size;
    let encrypted_size = |data_size: usize| {
        (encrypted_header_size + data_size + 12 + 15) & !15
    };
    if use_random_padding {
        let rand_data_size = random::secure_uint32() as usize & 0xff;
        return raw_size + encrypted_size(data_size + rand_data_size);
    }
    let mut encrypted_size = encrypted_size(data_size);
    const SIZES: [usize; 9] = [64, 128, 192, 256, 384, 512, 768, 1024, 1280];
    for size in SIZES {
        if encrypted_size <= size {
            return raw_size + size;
        }
    }
    encrypted_size = (encrypted_size - 1280 + 447) / 448 * 448 + 1280;
    raw_size + encrypted_size
}

fn calc_no_crypto_size(data_size: usize) -> usize {
    NO_CRYPTO_HEADER_SIZE + data_size
}

/// `Transport::ReadResult`.
#[derive(Debug)]
pub enum ReadResult {
    Packet { offset: usize, len: usize },
    Nop,
    Error(i32),
    Quickack(u32),
}

/// `Transport::read_auth_key_id`.
pub fn read_auth_key_id(message: &[u8]) -> Result<u64> {
    if message.len() < 8 {
        return Result::Err(Status::error_message(&format!(
            "Invalid MTProto message: smaller than 8 bytes [size = {}]",
            message.len()
        )));
    }
    Result::Ok(u64_at(message, 0))
}

/// `Transport::read`: the top-level dispatch — quick-ack/nop/error short
/// forms, then no-crypto, crypto or e2e-crypto decrypt. The returned window
/// `[offset, offset+len)` lies inside `message` (the C++ MutableSlice
/// result).
pub fn read(
    message: &mut [u8],
    error_code: i32,
    auth_key: &AuthKey,
    packet_info: &mut PacketInfo,
) -> Result<ReadResult> {
    // The client decrypts server messages with X=8 (MTProto's directional
    // asymmetry); `read_with_x` exists for the server-side oracle.
    read_with_x(message, error_code, auth_key, packet_info, 8)
}

/// `Transport::read` with an explicit KDF/message-key offset — the server
/// side reads with X=0.
pub fn read_with_x(
    message: &mut [u8],
    error_code: i32,
    auth_key: &AuthKey,
    packet_info: &mut PacketInfo,
    x: usize,
) -> Result<ReadResult> {
    if error_code >= 300 {
        return Result::Ok(ReadResult::Error(-error_code));
    }
    if message.len() < 16 {
        if message.len() < 4 {
            return Result::Err(Status::error_message(&format!(
                "Invalid MTProto message: smaller than 4 bytes [size = {}]",
                message.len()
            )));
        }
        let code = i32::from_le_bytes(message[..4].try_into().unwrap());
        if code == 0 {
            return Result::Ok(ReadResult::Nop);
        } else if code == -1 && message.len() >= 8 {
            return Result::Ok(ReadResult::Quickack(u32_at(message, 4)));
        } else {
            return Result::Ok(ReadResult::Error(code));
        }
    }

    packet_info.no_crypto_flag = i64::from_le_bytes(message[..8].try_into().unwrap()) == 0;
    let (offset, len) = if packet_info.type_ == PacketType::EndToEnd {
        assert!(!auth_key.empty());
        let res = read_e2e_crypto(message, auth_key, packet_info);
        tdutils::try_result!(r, res);
        r
    } else if packet_info.no_crypto_flag {
        let res = read_no_crypto(message, packet_info);
        tdutils::try_result!(r, res);
        r
    } else {
        if auth_key.empty() {
            return Result::Err(Status::error_message(
                "Failed to decrypt MTProto message: auth key is empty",
            ));
        }
        let res = read_crypto_x(message, auth_key, packet_info, x);
        tdutils::try_result!(r, res);
        r
    };
    Result::Ok(ReadResult::Packet { offset, len })
}

fn read_no_crypto(message: &[u8], _packet_info: &mut PacketInfo) -> Result<(usize, usize)> {
    if message.len() < NO_CRYPTO_HEADER_SIZE {
        return Result::Err(Status::error_message(&format!(
            "Invalid MTProto message: too small [message.size() = {}] < [sizeof(NoCryptoHeader) = {}]",
            message.len(),
            NO_CRYPTO_HEADER_SIZE
        )));
    }
    let data_size = message.len() - NO_CRYPTO_HEADER_SIZE;
    assert_eq!(message.len(), calc_no_crypto_size(data_size));
    Result::Ok((NO_CRYPTO_HEADER_SIZE, data_size))
}

/// `Transport::read_crypto_impl`.
#[allow(clippy::too_many_arguments)]
fn read_crypto_impl(
    x: usize,
    header_size: usize,
    encrypted_header_size: usize,
    prefix_size: usize,
    encrypt_begin: usize,
    message: &mut [u8],
    auth_key: &AuthKey,
    packet_info: &mut PacketInfo,
) -> Result<(usize, usize)> {
    if message.len() < header_size {
        return Result::Err(Status::error_message(&format!(
            "Invalid MTProto message: too small [message.size() = {}] < [sizeof(HeaderT) = {}]",
            message.len(),
            header_size
        )));
    }
    let auth_key_id = u64_at(message, 0);
    let mut message_key = [0u8; 16];
    message_key.copy_from_slice(&message[8..24]);
    let to_decrypt_len = (message.len() - encrypt_begin) & !15;

    if auth_key_id != auth_key.id() {
        return Result::Err(Status::error_message(&format!(
            "Invalid MTProto message: auth_key_id mismatch [found = {:x}] [expected = {:x}]",
            auth_key_id,
            auth_key.id()
        )));
    }

    let (aes_key, aes_iv) = if packet_info.version == 1 {
        kdf(auth_key.key(), &message_key, x)
    } else {
        kdf2(auth_key.key(), &message_key, x)
    };
    let mut aes_iv = aes_iv;

    // In-place IGE decrypt over [encrypt_begin, end) — the C++
    // `aes_ige_decrypt(key, iv, to_decrypt, to_decrypt)`.
    unsafe {
        let from = std::slice::from_raw_parts(message.as_ptr().add(encrypt_begin), to_decrypt_len);
        let to =
            std::slice::from_raw_parts_mut(message.as_mut_ptr().add(encrypt_begin), to_decrypt_len);
        aes_ige_decrypt(&aes_key, &mut aes_iv, from, to);
    }

    let tail_size = message.len() - header_size;
    if tail_size < prefix_size {
        return Result::Err(Status::error_message("Too small encrypted part"));
    }
    let prefix_offset = header_size;
    let message_data_length = u32_at(message, prefix_offset + prefix_size - 4);
    let data_size = message_data_length as usize + prefix_size;
    let mut is_length_bad = false;
    let real_message_key: [u8; 16];

    if packet_info.version == 1 {
        is_length_bad |= packet_info.check_mod4 && message_data_length % 4 != 0;
        let expected_size = calc_crypto_size(header_size, encrypted_header_size, data_size);
        is_length_bad |= expected_size != message.len();
        let check_size = data_size * usize::from(!is_length_bad) + tail_size * usize::from(is_length_bad);
        // The C++ part runs from encrypt_begin to `header->data + check_size`.
        let (ack, key) = calc_message_ack_and_key(&message[encrypt_begin..header_size + check_size]);
        packet_info.message_ack = ack;
        real_message_key = key;
    } else {
        let (ack, key) = calc_message_key2(auth_key, x, &message[encrypt_begin..encrypt_begin + to_decrypt_len]);
        packet_info.message_ack = ack;
        real_message_key = key;
    }

    let mut is_key_bad = 0;
    for i in 0..16 {
        is_key_bad |= real_message_key[i] ^ message_key[i];
    }
    if is_key_bad != 0 {
        return Result::Err(Status::error_message(&format!(
            "Invalid MTProto message: message_key mismatch [found = {}] [expected = {}]",
            hex_dump(&message_key),
            hex_dump(&real_message_key)
        )));
    }

    if packet_info.version == 2 {
        if packet_info.check_mod4 && message_data_length % 4 != 0 {
            return Result::Err(Status::error_message(&format!(
                "Invalid MTProto message: invalid length (not divisible by four) [total_size = {}] [message_data_length = {}]",
                message.len(),
                message_data_length
            )));
        }
        if tail_size - prefix_size < message_data_length as usize {
            return Result::Err(Status::error_message(&format!(
                "Invalid MTProto message: invalid length (message_data_length is too big) [total_size = {}] [message_data_length = {}]",
                message.len(),
                message_data_length
            )));
        }
        let pad_size = tail_size - data_size;
        if pad_size < 12 || pad_size > 1024 {
            return Result::Err(Status::error_message(&format!(
                "Invalid MTProto message: invalid length (invalid padding length) [padding_size = {}] [total_size = {}] [message_data_length = {}]",
                pad_size,
                message.len(),
                message_data_length
            )));
        }
    } else if is_length_bad {
        return Result::Err(Status::error_message(&format!(
            "Invalid MTProto message: invalid length [total_size = {}] [message_data_length = {}]",
            message.len(),
            message_data_length
        )));
    }

    // The returned window includes the prefix — the C++ `MutableSlice(header->data,
    // data_size)` the session layer parses msg_id/seq_no/bytes from.
    Result::Ok((header_size, data_size))
}

fn read_crypto_x(
    message: &mut [u8],
    auth_key: &AuthKey,
    packet_info: &mut PacketInfo,
    x: usize,
) -> Result<(usize, usize)> {
    let res = read_crypto_impl(
        x,
        CRYPTO_HEADER_SIZE,
        CRYPTO_ENCRYPTED_HEADER_SIZE,
        CRYPTO_PREFIX_SIZE,
        CRYPTO_ENCRYPT_BEGIN,
        message,
        auth_key,
        packet_info,
    );
    tdutils::try_result!(res, res);
    packet_info.type_ = PacketType::Common;
    packet_info.salt = u64_at(message, 24);
    packet_info.session_id = u64_at(message, 32);
    // The CryptoPrefix (msg_id, seq_no) sits at the start of the returned
    // window; the E2E prefix carries only the length.
    packet_info.message_id = MessageId::new(u64_at(message, 40));
    packet_info.seq_no = u32_at(message, 48) as i32;
    Result::Ok(res)
}

fn read_e2e_crypto(
    message: &mut [u8],
    auth_key: &AuthKey,
    packet_info: &mut PacketInfo,
) -> Result<(usize, usize)> {
    let x = if packet_info.is_creator && packet_info.version != 1 {
        8
    } else {
        0
    };
    let res = read_crypto_impl(
        x,
        E2E_HEADER_SIZE,
        E2E_ENCRYPTED_HEADER_SIZE,
        E2E_PREFIX_SIZE,
        E2E_ENCRYPT_BEGIN,
        message,
        auth_key,
        packet_info,
    );
    tdutils::try_result!(res, res);
    packet_info.type_ = PacketType::EndToEnd;
    Result::Ok(res)
}

/// `Transport::write`: the top-level dispatch. `data` is the already-stored
/// packet body (prefix + payload — the C++ `storer`).
pub fn write(
    data: &[u8],
    auth_key: &AuthKey,
    packet_info: &mut PacketInfo,
    prepend_size: usize,
    append_size: usize,
) -> BufferWriter {
    if packet_info.type_ == PacketType::EndToEnd {
        return write_e2e_crypto(data, auth_key, packet_info, prepend_size, append_size);
    }
    if packet_info.no_crypto_flag {
        return write_no_crypto(data, packet_info, prepend_size, append_size);
    }
    assert!(!auth_key.empty());
    write_crypto(data, auth_key, packet_info, prepend_size, append_size)
}

fn write_no_crypto(
    data: &[u8],
    _packet_info: &mut PacketInfo,
    prepend_size: usize,
    append_size: usize,
) -> BufferWriter {
    let size = calc_no_crypto_size(data.len());
    let mut packet = BufferWriter::with_reserve(size, prepend_size, append_size);
    packet.as_mutable_slice()[..8].fill(0); // NoCryptoHeader auth_key_id = 0
    packet.as_mutable_slice()[8..8 + data.len()].copy_from_slice(data);
    packet
}

#[allow(clippy::too_many_arguments)]
fn write_crypto_impl(
    x: usize,
    header_size: usize,
    _encrypted_header_size: usize,
    encrypt_begin: usize,
    data: &[u8],
    auth_key: &AuthKey,
    packet_info: &mut PacketInfo,
    padded_size: usize,
    packet: &mut BufferWriter,
) {
    {
        let slice = packet.as_mutable_slice();
        slice[header_size..header_size + data.len()].copy_from_slice(data);
    }
    let pad_size = padded_size - header_size - data.len();
    {
        let slice = packet.as_mutable_slice();
        random::secure_bytes(&mut slice[header_size + data.len()..header_size + data.len() + pad_size]);
    }

    let message_key: [u8; 16];
    let (aes_key, aes_iv) = if packet_info.version == 1 {
        // The C++ part runs from encrypt_begin to `header->data + data_size`
        // (the prefix sits at header->data, not after encrypt_begin).
        let (ack, key) = {
            let slice = packet.as_slice();
            calc_message_ack_and_key(&slice[encrypt_begin..header_size + data.len()])
        };
        packet_info.message_ack = ack;
        message_key = key;
        kdf(auth_key.key(), &message_key, x)
    } else {
        let to_encrypt_len = padded_size - encrypt_begin;
        let (ack, key) = {
            let slice = packet.as_slice();
            calc_message_key2(auth_key, x, &slice[encrypt_begin..encrypt_begin + to_encrypt_len])
        };
        packet_info.message_ack = ack;
        message_key = key;
        kdf2(auth_key.key(), &message_key, x)
    };
    {
        packet.as_mutable_slice()[8..24].copy_from_slice(&message_key);
    }

    let mut aes_iv = aes_iv;
    // In-place IGE encrypt over [encrypt_begin, padded_size) — the C++
    // `aes_ige_encrypt(key, iv, to_encrypt, to_encrypt)`.
    let encrypt_len = padded_size - encrypt_begin;
    unsafe {
        let slice = packet.as_mutable_slice();
        let from = std::slice::from_raw_parts(slice.as_ptr().add(encrypt_begin), encrypt_len);
        let to = std::slice::from_raw_parts_mut(slice.as_mut_ptr().add(encrypt_begin), encrypt_len);
        aes_ige_encrypt(&aes_key, &mut aes_iv, from, to);
    }
}

fn write_crypto(
    data: &[u8],
    auth_key: &AuthKey,
    packet_info: &mut PacketInfo,
    prepend_size: usize,
    append_size: usize,
) -> BufferWriter {
    let data_size = data.len();
    let padded_size = if packet_info.version == 1 {
        calc_crypto_size(CRYPTO_HEADER_SIZE, CRYPTO_ENCRYPTED_HEADER_SIZE, data_size)
    } else {
        calc_crypto_size2(
            CRYPTO_HEADER_SIZE,
            CRYPTO_ENCRYPTED_HEADER_SIZE,
            data_size,
            packet_info.use_random_padding,
        )
    };
    let mut packet = BufferWriter::with_reserve(padded_size, prepend_size, append_size);
    {
        let slice = packet.as_mutable_slice();
        slice[..8].copy_from_slice(&auth_key.id().to_le_bytes());
        slice[24..32].copy_from_slice(&packet_info.salt.to_le_bytes());
        slice[32..40].copy_from_slice(&packet_info.session_id.to_le_bytes());
    }
    write_crypto_impl(
        0,
        CRYPTO_HEADER_SIZE,
        CRYPTO_ENCRYPTED_HEADER_SIZE,
        CRYPTO_ENCRYPT_BEGIN,
        data,
        auth_key,
        packet_info,
        padded_size,
        &mut packet,
    );
    packet
}

fn write_e2e_crypto(
    data: &[u8],
    auth_key: &AuthKey,
    packet_info: &mut PacketInfo,
    prepend_size: usize,
    append_size: usize,
) -> BufferWriter {
    let data_size = data.len();
    let padded_size = if packet_info.version == 1 {
        calc_crypto_size(E2E_HEADER_SIZE, E2E_ENCRYPTED_HEADER_SIZE, data_size)
    } else {
        calc_crypto_size2(
            E2E_HEADER_SIZE,
            E2E_ENCRYPTED_HEADER_SIZE,
            data_size,
            packet_info.use_random_padding,
        )
    };
    let mut packet = BufferWriter::with_reserve(padded_size, prepend_size, append_size);
    packet.as_mutable_slice()[..8].copy_from_slice(&auth_key.id().to_le_bytes());
    let x = if packet_info.is_creator || packet_info.version == 1 {
        0
    } else {
        8
    };
    write_crypto_impl(
        x,
        E2E_HEADER_SIZE,
        E2E_ENCRYPTED_HEADER_SIZE,
        E2E_ENCRYPT_BEGIN,
        data,
        auth_key,
        packet_info,
        padded_size,
        &mut packet,
    );
    packet
}

/// `format::as_hex_dump` style helper for the error texts.
fn hex_dump(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth_key() -> AuthKey {
        AuthKey::new(0x0f0e0d0c0b0a0908, (0..=255).collect())
    }

    /// A valid stored body: msg_id = 1, seq_no = 0, length = 4, payload.
    fn body() -> Vec<u8> {
        let mut res = Vec::new();
        res.extend_from_slice(&1u64.to_le_bytes());
        res.extend_from_slice(&0u32.to_le_bytes());
        res.extend_from_slice(&4u32.to_le_bytes());
        res.extend_from_slice(b"aabb");
        res
    }

    fn packet_info(version: i32) -> PacketInfo {
        let mut pi = PacketInfo::new();
        pi.version = version;
        pi.no_crypto_flag = false;
        pi.type_ = PacketType::Common;
        pi.salt = 0x0102030405060708;
        pi.session_id = 0x1112131415161718;
        pi
    }

    #[test]
    fn v2_write_matches_the_known_plaintext() {
        // MTProto v2 is direction-asymmetric too (client writes with X=0,
        // the server answers with X=8), so the client-side read cannot
        // decrypt its own packet — verify the write against the primitives.
        let auth_key = auth_key();
        let mut write_info = packet_info(2);
        let packet = write(&body(), &auth_key, &mut write_info, 0, 0);
        assert_eq!(packet.size(), 24 + 64); // 24 + ladder(round16(16 + 20 + 12))

        // msg_key = SHA256(auth_key[88..120] + plaintext)[8..24]; verified
        // below after the server-side decryption.

        // Decrypt with the server-side parameters (X=0) and check the
        // plaintext.
        let message_key = &packet.as_slice()[8..24];
        let (aes_key, aes_iv) = kdf2(auth_key.key(), message_key.try_into().unwrap(), 0);
        let mut aes_iv = aes_iv;
        let mut frame = packet.as_slice().to_vec();
        unsafe {
            let from = std::slice::from_raw_parts(frame.as_ptr().add(24), frame.len() - 24);
            let to = std::slice::from_raw_parts_mut(frame.as_mut_ptr().add(24), frame.len() - 24);
            aes_ige_decrypt(&aes_key, &mut aes_iv, from, to);
        }
        assert_eq!(&frame[24..32], &0x0102030405060708u64.to_le_bytes());
        assert_eq!(&frame[32..40], &0x1112131415161718u64.to_le_bytes());
        assert_eq!(&frame[40..40 + body().len()], body().as_slice());
        // The trailing bytes are the 12..1024-byte random padding.
        let pad_size = packet.size() - 40 - body().len();
        assert!((12..=1024).contains(&pad_size));

        // And the decrypted message key round-trips: recompute it over the
        // decrypted region and compare with the header.
        let (ack, key2) = calc_message_key2(&auth_key, 0, &frame[24..]);
        assert_eq!(write_info.message_ack, ack);
        assert_eq!(key2.as_slice(), message_key);
    }

    #[test]
    fn ige_self_inversion_debug() {
        for len in [16usize, 32, 48] {
            let key: Vec<u8> = (0..=255).collect();
            let (aes_key, aes_iv) = kdf(&key, &[0x42u8; 16], 0);
            let mut data = (0..len as u8).collect::<Vec<u8>>();
            let original = data.clone();
            let mut iv = aes_iv;
            unsafe {
                let ptr = data.as_mut_ptr();
                let from = std::slice::from_raw_parts(ptr, len);
                let to = std::slice::from_raw_parts_mut(ptr, len);
                aes_ige_encrypt(&aes_key, &mut iv, from, to);
            }
            assert_ne!(data, original);
            let mut iv = aes_iv;
            unsafe {
                let ptr = data.as_mut_ptr();
                let from = std::slice::from_raw_parts(ptr, len);
                let to = std::slice::from_raw_parts_mut(ptr, len);
                aes_ige_decrypt(&aes_key, &mut iv, from, to);
            }
            assert_eq!(data, original, "IGE self-inversion failed at len {len}: {:02x?}", data);
        }
    }

    #[test]
    fn v1_write_matches_the_known_plaintext() {
        // MTProto v1 is asymmetric (client encrypts with X=0, the server
        // decrypts with X=0 and answers with X=8), so a client-side
        // self-roundtrip cannot decrypt — verify the write against the
        // independently computed primitives instead.
        let auth_key = auth_key();
        let mut write_info = packet_info(1);
        let packet = write(&body(), &auth_key, &mut write_info, 0, 0);
        assert_eq!(packet.size(), 72); // 24 + round16(16 + 20)

        // The message key is SHA-1(salt + session_id + body)[4..20] — the
        // python-verified value for the fixed test inputs.
        let message_key = &packet.as_slice()[8..24];
        assert_eq!(
            hex_dump(message_key),
            "9359f7137654affa4af19b6085cde021"
        );

        // Decrypt with the server-side key (X=0) and check the plaintext.
        let (aes_key, aes_iv) = kdf(auth_key.key(), message_key.try_into().unwrap(), 0);
        let mut aes_iv = aes_iv;
        let mut frame = packet.as_slice().to_vec();
        unsafe {
            let from = std::slice::from_raw_parts(frame.as_ptr().add(24), frame.len() - 24);
            let to = std::slice::from_raw_parts_mut(frame.as_mut_ptr().add(24), frame.len() - 24);
            aes_ige_decrypt(&aes_key, &mut aes_iv, from, to);
        }
        assert_eq!(&frame[24..32], &0x0102030405060708u64.to_le_bytes());
        assert_eq!(&frame[32..40], &0x1112131415161718u64.to_le_bytes());
        assert_eq!(&frame[40..60], body().as_slice());
    }

    #[test]
    fn no_crypto_roundtrip() {
        let auth_key = auth_key();
        let mut write_info = packet_info(2);
        write_info.no_crypto_flag = true;
        let packet = write(&body(), &auth_key, &mut write_info, 0, 0);
        // auth_key_id = 0 in the header
        assert_eq!(&packet.as_slice()[..8], &[0u8; 8]);
        assert_eq!(&packet.as_slice()[8..], body().as_slice());

        let mut frame = packet.as_slice().to_vec();
        let mut read_info = packet_info(2);
        read_info.no_crypto_flag = true;
        match read(&mut frame, 0, &auth_key, &mut read_info).unwrap() {
            ReadResult::Packet { offset, len } => {
                assert_eq!(&frame[offset..offset + len], body().as_slice());
            }
            other => panic!("expected a packet, got {other:?}"),
        }
    }

    #[test]
    fn read_rejects_wrong_key_and_wrong_session_data() {
        let auth_key = auth_key();
        let mut write_info = packet_info(2);
        let packet = write(&body(), &auth_key, &mut write_info, 0, 0);
        let mut frame = packet.as_slice().to_vec();

        // A different key id fails the auth_key_id check.
        let other = AuthKey::new(0xDEADBEEF, (0..=255).collect());
        let mut read_info = packet_info(2);
        let err = read(&mut frame, 0, &other, &mut read_info).error().clone();
        assert!(err.message().contains("auth_key_id mismatch"));

        // A different key (same id) fails the message_key check.
        let mut frame = packet.as_slice().to_vec();
        let same_id = AuthKey::new(0x0f0e0d0c0b0a0908, (0..=255).map(|i| 255 - i).collect());
        let mut read_info = packet_info(2);
        let err = read(&mut frame, 0, &same_id, &mut read_info).error().clone();
        assert!(err.message().contains("message_key mismatch"));
    }

    #[test]
    fn read_handles_short_forms() {
        let auth_key = auth_key();
        let mut read_info = packet_info(2);
        assert!(matches!(
            read(&mut vec![0u8; 4], 0, &auth_key, &mut read_info).unwrap(),
            ReadResult::Nop
        ));
        let mut quick = Vec::new();
        quick.extend_from_slice(&(-1i32).to_le_bytes());
        quick.extend_from_slice(&0x4242u32.to_le_bytes());
        let mut read_info = packet_info(2);
        assert!(matches!(
            read(&mut quick, 0, &auth_key, &mut read_info).unwrap(),
            ReadResult::Quickack(0x4242)
        ));
        let mut code = 404i32.to_le_bytes().to_vec();
        let mut read_info = packet_info(2);
        assert!(matches!(
            read(&mut code, 0, &auth_key, &mut read_info).unwrap(),
            ReadResult::Error(404)
        ));
        // http error codes ≥ 300 negate the transport error code.
        let mut read_info = packet_info(2);
        let mut small = vec![0u8; 4];
        assert!(matches!(
            read(&mut small, 404, &auth_key, &mut read_info).unwrap(),
            ReadResult::Error(-404)
        ));
    }

    #[test]
    fn read_rejects_truncated_packets() {
        let auth_key = auth_key();
        let mut write_info = packet_info(2);
        let packet = write(&body(), &auth_key, &mut write_info, 0, 0);
        let mut frame = packet.as_slice()[..20].to_vec();
        let mut read_info = packet_info(2);
        let err = read(&mut frame, 0, &auth_key, &mut read_info).error().clone();
        assert!(err.message().contains("too small"));
    }

    #[test]
    fn v1_padding_ladder_sizes_match_cpp() {
        // v2 basic padding: 24 + ladder(round16(16 + data + 12)).
        assert_eq!(calc_crypto_size2(40, 16, 0, false), 24 + 64);
        assert_eq!(calc_crypto_size2(40, 16, 100, false), 24 + 128);
        // encrypted = 4032 → (4032 - 1280 + 447) / 448 * 448 + 1280 = 4416.
        assert_eq!(calc_crypto_size2(40, 16, 4000, false), 24 + 4416);
        // v1: 24 + round16(16 + data).
        assert_eq!(calc_crypto_size(40, 16, 100), 24 + 128);
        assert_eq!(calc_crypto_size(40, 16, 101), 24 + 128);
    }
}
