//! The TON wallet value plane — the seed of `td/telegram/TonWalletManager`
//! (the actor itself is unported; the requests stay interim). Only the
//! address validator lives here so far: `check_ton_address`
//! (TonWalletManager.cpp:1554-1576), shared by the TON-address entity
//! detector (MessageEntity.cpp `match_ton_addresses`) and, upstream, by the
//! wallet request gates.

use tdutils::crypto::crc16;

/// `TonWalletManager::check_ton_address` (TonWalletManager.cpp:1554-1576):
/// 48 base64(any-alphabet) characters decoding to 36 bytes with a valid
/// CRC-16/XMODEM checksum over the first 34 and the anycast/metadata bits
/// in the first byte.
pub fn check_ton_address(address: &str) -> Result<(), tdutils::status::Status> {
    let error = |code: i32, msg: &str| tdutils::status::Status::error(code, msg);
    if address.len() != 48 {
        return Err(error(400, "Invalid address length"));
    }
    let bytes = address.as_bytes();
    let decoded = match tdmtproto::utils::base64url_decode(bytes) {
        tdutils::status::Result::Ok(decoded) => decoded,
        tdutils::status::Result::Err(_) => match tdmtproto::utils::base64_decode(bytes) {
            tdutils::status::Result::Ok(decoded) => decoded,
            tdutils::status::Result::Err(_) => return Err(error(400, "Invalid address format")),
        },
    };
    // The C++ CHECKs the size after a successful decode: both alphabets
    // yield exactly 36 bytes for a canonical 48-character address.
    assert_eq!(decoded.len(), 36);
    let buffer = &decoded[..];
    let actual_crc = crc16(&buffer[..34]);
    let expected_crc = ((buffer[34] as u16) << 8) | buffer[35] as u16;
    if actual_crc != expected_crc {
        return Err(error(400, "Invalid address checksum"));
    }
    if (buffer[0] & 0x3F) != 0x11 {
        return Err(error(400, "Invalid address first byte"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `td::find_ton_addresses` test battery (558da19b2,
    /// test/message_entities.cpp `find_ton_addresses`) — the address
    /// vectors regenerated with the same construction rules.
    #[test]
    fn check_ton_address_vectors() {
        // A canonical mainnet-friendly shape: first byte encodes the
        // 0x11 anycast bits, then 33 workchain/account bytes, then the
        // crc16 over the first 34.
        let mut raw = [0u8; 36];
        raw[0] = 0x11; // (0x11 & 0x3F) == 0x11
        for (i, byte) in raw.iter_mut().enumerate().skip(1) {
            *byte = (i * 7 + 3) as u8;
        }
        let crc = crc16(&raw[..34]);
        raw[34] = (crc >> 8) as u8;
        raw[35] = (crc & 0xFF) as u8;
        // base64url without padding: 36 bytes -> 48 characters.
        let address = {
            use base64::Engine;
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw)
        };
        assert_eq!(address.len(), 48);
        assert!(check_ton_address(&address).is_ok());

        // A flipped checksum byte fails.
        let mut bad = raw;
        bad[35] ^= 1;
        let bad_address = {
            use base64::Engine;
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bad)
        };
        assert!(check_ton_address(&bad_address).is_err());

        // Wrong length.
        assert!(check_ton_address(&address[..47]).is_err());
    }
}
