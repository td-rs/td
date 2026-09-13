//! Port of `td/mtproto/KDF.{h,cpp}` (S57): the MTProto v1 and v2 key
//! derivation functions plus the temp-key KDF of the auth handshake.

use tdutils::crypto::{sha1, sha256};

/// `KDF(auth_key, msg_key, X)`: MTProto v1 — four SHA-1 over
/// `msg_key + auth_key` slices assembled into a 32-byte AES key and a
/// 32-byte IGE iv.
pub fn kdf(auth_key: &[u8], msg_key: &[u8; 16], x: usize) -> ([u8; 32], [u8; 32]) {
    assert_eq!(auth_key.len(), 2048 / 8);

    let mut buf = [0u8; 48];
    buf[..16].copy_from_slice(msg_key);
    buf[16..48].copy_from_slice(&auth_key[x..x + 32]);
    let sha1_a = sha1(&buf);

    buf[..16].copy_from_slice(&auth_key[x + 32..x + 48]);
    buf[16..32].copy_from_slice(msg_key);
    buf[32..48].copy_from_slice(&auth_key[x + 48..x + 64]);
    let sha1_b = sha1(&buf);

    buf[..32].copy_from_slice(&auth_key[x + 64..x + 96]);
    buf[32..48].copy_from_slice(msg_key);
    let sha1_c = sha1(&buf);

    buf[..16].copy_from_slice(msg_key);
    buf[16..48].copy_from_slice(&auth_key[x + 96..x + 128]);
    let sha1_d = sha1(&buf);

    let mut aes_key = [0u8; 32];
    let mut aes_iv = [0u8; 32];

    aes_key[..8].copy_from_slice(&sha1_a[..8]);
    aes_key[8..20].copy_from_slice(&sha1_b[8..20]);
    aes_key[20..32].copy_from_slice(&sha1_c[4..16]);

    aes_iv[..12].copy_from_slice(&sha1_a[8..20]);
    aes_iv[12..20].copy_from_slice(&sha1_b[..8]);
    aes_iv[20..24].copy_from_slice(&sha1_c[16..20]);
    aes_iv[24..32].copy_from_slice(&sha1_d[..8]);

    (aes_key, aes_iv)
}

/// `tmp_KDF(server_nonce, new_nonce)`: the auth-handshake key derivation.
pub fn tmp_kdf(server_nonce: &[u8; 16], new_nonce: &[u8; 32]) -> ([u8; 32], [u8; 32]) {
    // tmp_aes_key := SHA1(new_nonce + server_nonce) + substr(SHA1(server_nonce + new_nonce), 0, 12);
    let mut buf = [0u8; 64];
    let mut tmp_aes_key = [0u8; 32];
    buf[..32].copy_from_slice(new_nonce);
    buf[32..48].copy_from_slice(server_nonce);
    tmp_aes_key[..20].copy_from_slice(&sha1(&buf[..48]));

    buf[..16].copy_from_slice(server_nonce);
    buf[16..48].copy_from_slice(new_nonce);
    let sha1_server_new = sha1(&buf[..48]);
    tmp_aes_key[20..32].copy_from_slice(&sha1_server_new[..12]);

    // tmp_aes_iv := substr(SHA1(server_nonce + new_nonce), 12, 8) + SHA1(new_nonce + new_nonce) + substr(new_nonce, 0, 4)
    let mut tmp_aes_iv = [0u8; 32];
    tmp_aes_iv[..8].copy_from_slice(&sha1_server_new[12..20]);

    buf[..32].copy_from_slice(new_nonce);
    buf[32..64].copy_from_slice(new_nonce);
    tmp_aes_iv[8..28].copy_from_slice(&sha1(&buf));
    tmp_aes_iv[28..32].copy_from_slice(&new_nonce[..4]);

    (tmp_aes_key, tmp_aes_iv)
}

/// `KDF2(auth_key, msg_key, X)`: MTProto v2 — SHA-256 based, 36+16-byte
/// windows of the auth key.
pub fn kdf2(auth_key: &[u8], msg_key: &[u8; 16], x: usize) -> ([u8; 32], [u8; 32]) {
    // sha256_a = SHA256 (msg_key + substr(auth_key, x, 36));
    let mut buf = [0u8; 36 + 16];
    buf[..16].copy_from_slice(msg_key);
    buf[16..52].copy_from_slice(&auth_key[x..x + 36]);
    let sha256_a = sha256(&buf);

    // sha256_b = SHA256 (substr(auth_key, 40+x, 36) + msg_key);
    buf[..36].copy_from_slice(&auth_key[x + 40..x + 76]);
    buf[36..52].copy_from_slice(msg_key);
    let sha256_b = sha256(&buf);

    // aes_key = substr(sha256_a, 0, 8) + substr(sha256_b, 8, 16) + substr(sha256_a, 24, 8);
    let mut aes_key = [0u8; 32];
    aes_key[..8].copy_from_slice(&sha256_a[..8]);
    aes_key[8..24].copy_from_slice(&sha256_b[8..24]);
    aes_key[24..32].copy_from_slice(&sha256_a[24..32]);

    // aes_iv = substr(sha256_b, 0, 8) + substr(sha256_a, 8, 16) + substr(sha256_b, 24, 8);
    let mut aes_iv = [0u8; 32];
    aes_iv[..8].copy_from_slice(&sha256_b[..8]);
    aes_iv[8..24].copy_from_slice(&sha256_a[8..24]);
    aes_iv[24..32].copy_from_slice(&sha256_b[24..32]);

    (aes_key, aes_iv)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(data: &[u8]) -> String {
        data.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn kdf_v1_matches_python_reference() {
        // Computed independently (python hashlib) for auth_key = 0..255 and
        // a fixed message key; pins the SHA-1 window assembly.
        let key: Vec<u8> = (0..=255).collect();
        let msg_key: [u8; 16] = [
            0x8c, 0x28, 0x11, 0x30, 0x49, 0xf6, 0xa5, 0xcf, 0x38, 0x19, 0xfc, 0x4e, 0xce, 0xda,
            0xe4, 0xff,
        ];
        let (k8, iv8) = kdf(&key, &msg_key, 8);
        assert_eq!(
            hex(&k8),
            "491f7933361de2b84330336eee40b6f42a82e345ae2fc616bbb49fd2cffef729"
        );
        assert_eq!(
            hex(&iv8),
            "d51cd3d5f6fe59fe72aa872ed182907bcd305e468420555604f05a36d2b8f614"
        );
        let (k0, iv0) = kdf(&key, &msg_key, 0);
        assert_eq!(
            hex(&k0),
            "234ca698545188321364b12a6bce4002baadb047e72d4247caf5f0ad9d7354b7"
        );
        assert_eq!(
            hex(&iv0),
            "c4fa1dddc3f4cddc1683047fc1ab3b08a8dbebb73bbe7cae6a3027c8543fb39e"
        );
    }
}
