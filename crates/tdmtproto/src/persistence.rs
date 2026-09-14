//! Auth-data persistence (S61): the on-disk form of a generated auth key
//! with the handshake state, salts and the server-time correction — the
//! fields tdlib persists in its `DbState`, written here as one standalone
//! file. Byte-compatible with the C++ `store`/`parse` methods of the same
//! types (verified by `conformance/mtproto_authdata_oracle`).

use crate::auth_data::{ServerSalt, parse_server_salt, store_server_salt};
use crate::auth_key::AuthKey;
use crate::dh_handshake::DhHandshake;
use crate::mtproto_api::tl;
use tdutils::status::{Result, Status};

/// The file format version.
const VERSION: i32 = 1;

/// `save_auth_data`: version, auth key, handshake state, server salt,
/// server-time difference, session id.
pub fn save_auth_data(
    auth_key: &AuthKey,
    handshake: Option<&DhHandshake>,
    server_salt: &ServerSalt,
    server_time_difference: f64,
    session_id: i64,
) -> Vec<u8> {
    let mut out = Vec::new();
    tl::__i32(&mut out, VERSION);
    auth_key.store(&mut out);
    match handshake {
        Some(handshake) => {
            tl::__i32(&mut out, 1);
            handshake.store(&mut out);
        }
        None => tl::__i32(&mut out, 0),
    }
    store_server_salt(&mut out, server_salt);
    tl::__f64(&mut out, server_time_difference);
    tl::__i64(&mut out, session_id);
    out
}

/// `load_auth_data`.
pub fn load_auth_data(data: &[u8]) -> Result<(AuthKey, Option<DhHandshake>, ServerSalt, f64, i64)> {
    let mut r = tl::Reader::new(data);
    let version = match r.i32() {
        Ok(version) => version,
        Err(error) => return Result::Err(Status::error_message(error)),
    };
    if version != VERSION {
        return Result::Err(Status::error_message(&format!(
            "Unsupported auth data version {version}"
        )));
    }
    let auth_key = match AuthKey::parse(&mut r) {
        Ok(key) => key,
        Err(error) => return Result::Err(Status::error_message(error)),
    };
    let handshake = match r.i32() {
        Ok(1) => match DhHandshake::parse(&mut r) {
            Ok(handshake) => Some(handshake),
            Err(error) => return Result::Err(Status::error_message(error)),
        },
        Ok(0) => None,
        Ok(_) => return Result::Err(Status::error_message("Invalid handshake flag")),
        Err(error) => return Result::Err(Status::error_message(error)),
    };
    let server_salt = match parse_server_salt(&mut r) {
        Ok(salt) => salt,
        Err(error) => return Result::Err(Status::error_message(error)),
    };
    let server_time_difference = match r.f64() {
        Ok(value) => value,
        Err(error) => return Result::Err(Status::error_message(error)),
    };
    let session_id = match r.i64() {
        Ok(value) => value,
        Err(error) => return Result::Err(Status::error_message(error)),
    };
    Result::Ok((
        auth_key,
        handshake,
        server_salt,
        server_time_difference,
        session_id,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_with_and_without_handshake() {
        let auth_key = AuthKey::new(0x0f0e0d0c0b0a0908, (0..=255).collect());
        let salt = ServerSalt {
            salt: 123456789,
            valid_since: 1000.5,
            valid_until: 2000.25,
        };
        for handshake in [None, Some(DhHandshake::new())] {
            let data = save_auth_data(&auth_key, handshake.as_ref(), &salt, 42.5, -7);
            let (key, parsed_handshake, parsed_salt, time_diff, session_id) =
                load_auth_data(&data).unwrap();
            assert_eq!(key.id(), auth_key.id());
            assert_eq!(key.key(), auth_key.key());
            assert_eq!(parsed_salt, salt);
            assert_eq!(time_diff, 42.5);
            assert_eq!(session_id, -7);
            assert_eq!(parsed_handshake.is_some(), handshake.is_some());
        }
    }

    #[test]
    fn rejects_unknown_versions() {
        let mut data = Vec::new();
        tl::__i32(&mut data, 999);
        assert!(load_auth_data(&data).is_error());
    }
}
