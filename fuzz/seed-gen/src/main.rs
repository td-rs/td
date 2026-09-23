//! S78 seed generator for the `mtproto_packet` fuzz target.
//!
//! The target feeds arbitrary bytes to `packet::read_with_x` under a fixed
//! dummy auth key with x = 8 (the server direction). Random inputs never
//! pass the envelope: auth_key_id, the v1 message key and the mod-16 size
//! must all line up, so the corpus stayed at zero coverage for the whole
//! 09-16..09-21 week (fuzz/audit-2026-09-23.md).
//!
//! This bin writes valid packets: it reuses the crate's own `packet::write`
//! (client direction, x = 0) for the envelope, recovers the plaintext by
//! decrypting with x = 0, then re-encrypts the same plaintext in the server
//! direction (x = 8) with the recomputed v1 message key
//! `sha1(salt + session_id + body)[4..20]`. The reader's checks — auth_key
//! id, message key, mod-4 length, exact packet size — all pass by
//! construction; msg_id/seq_no are not checked at this layer.

use tdmtproto::auth_key::AuthKey;
use tdmtproto::kdf::kdf;
use tdmtproto::packet::write as packet_write;
use tdmtproto::packet_info::{PacketInfo, PacketType};
use tdutils::crypto::{aes_ige_decrypt_in_place, aes_ige_encrypt_in_place, sha1};

fn main() {
    let out_dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "fuzz/corpus/mtproto_packet".to_string());
    std::fs::create_dir_all(&out_dir).unwrap();

    let auth_key = AuthKey::new(0, vec![7u8; 256]);
    let mut written = 0usize;
    for (i, payload) in payloads().iter().enumerate() {
        let packet = server_direction_packet(&auth_key, payload, 2 * i as u64 + 1, i as u32 + 1);
        let name = format!("{out_dir}/seed-{i:02}-len{}.bin", payload.len());
        std::fs::write(&name, packet).unwrap();
        written += 1;
    }
    println!("wrote {written} mtproto_packet seeds to {out_dir}");
}

fn server_direction_packet(auth_key: &AuthKey, payload: &[u8], msg_id: u64, seq_no: u32) -> Vec<u8> {
    // The body the reader parses after decryption: msg_id, seq_no, length,
    // payload (the C++ CryptoPrefix + the stored bytes).
    let mut body = Vec::new();
    body.extend_from_slice(&msg_id.to_le_bytes());
    body.extend_from_slice(&seq_no.to_le_bytes());
    body.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    body.extend_from_slice(payload);

    // A valid client-direction packet (write encrypts with x = 0); the salt
    // and session_id it stores are opaque to the reader.
    let mut packet_info = PacketInfo {
        version: 1,
        no_crypto_flag: false,
        type_: PacketType::Common,
        salt: 0x0102030405060708,
        session_id: 0x1112131415161718,
        ..PacketInfo::default()
    };
    let client_packet = packet_write(&body, auth_key, &mut packet_info, 0, 0);
    let packet = client_packet.as_slice();

    // Recover the plaintext (decrypt the x = 0 packet in place).
    let mut plaintext = packet.to_vec();
    let (aes_key, mut aes_iv) = kdf(auth_key.key(), &packet[8..24].try_into().unwrap(), 0);
    aes_ige_decrypt_in_place(&aes_key, &mut aes_iv, &mut plaintext[24..]);

    // Re-key in the server direction over the same message-key region:
    // salt(8) + session_id(8) + msg_id(8) + seq_no(4) + length(4) + payload.
    let key_region = 32 + payload.len();
    let message_key: [u8; 16] = sha1(&plaintext[24..24 + key_region])[4..20].try_into().unwrap();
    let (aes_key, mut aes_iv) = kdf(auth_key.key(), &message_key, 8);
    aes_ige_encrypt_in_place(&aes_key, &mut aes_iv, &mut plaintext[24..]);

    let mut result = Vec::with_capacity(plaintext.len());
    result.extend_from_slice(&auth_key.id().to_le_bytes());
    result.extend_from_slice(&message_key);
    result.extend_from_slice(&plaintext[24..]);
    result
}

/// Varied mod-4 payload sizes (check_mod4 rejects the rest) plus a couple of
/// TL-flavored bodies shaped like server answers (resPQ / pong /
/// new_session_created constructors) in case the body bytes ever feed a
/// deeper parse.
fn payloads() -> Vec<Vec<u8>> {
    let mut res: Vec<Vec<u8>> = Vec::new();
    for len in [0usize, 4, 8, 12, 36, 64, 100, 256, 512, 1020] {
        res.push((0..len).map(|b| (b % 251) as u8).collect());
    }
    for ctor in [0x05162463u32, 0x3476c7e9, 0x9ec20908] {
        let mut v = ctor.to_le_bytes().to_vec();
        v.extend_from_slice(&[0xAA; 60]);
        res.push(v);
    }
    res
}
