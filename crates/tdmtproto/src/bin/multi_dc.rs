//! S64 multi-DC: per-DC auth keys and sessions. Handshakes against TWO real
//! Telegram test DCs, persists each generated auth state through the S61
//! byte-compatible layout (save → load → use the loaded key), then
//! validates both persisted keys with encrypted ping-pong sessions (S63's
//! `run_ping`). This is the tdmtproto-layer core of the C++
//! AuthManager/AuthDataShared pair; the td/telegram/net actor plumbing
//! (SessionMultiProxy, DcAuthManager, MultiSequenceDispatcher) stays
//! deferred behind the S52 SessionSink/TdSink plug-in points, landing with
//! the Track H client.
//!
//! Usage: multi_dc

use tdmtproto::auth_data::{AuthData, ServerSalt};
use tdmtproto::dh_callback::DhCallback;
use tdmtproto::handshake::{AuthKeyHandshake, AuthKeyHandshakeContext};
use tdmtproto::handshake_actor::run_handshake;
use tdmtproto::persistence::{load_auth_data, save_auth_data};
use tdmtproto::ping::run_ping;
use tdmtproto::proxy_secret::ProxySecret;
use tdmtproto::rsa::{PublicRsaKeyInterface, Rsa, RsaKey};
use tdmtproto::transport_type::{TransportType, TransportTypeEnum};
use tdutils::status::{Result, Status};
use tokio::net::TcpStream;

/// The tdlib test-DC key (`PublicRsaKeySharedMain.cpp`).
const TEST_DC_KEY: &str = "-----BEGIN RSA PUBLIC KEY-----\n\
MIIBCgKCAQEAyMEdY1aR+sCR3ZSJrtztKTKqigvO/vBfqACJLZtS7QMgCGXJ6XIR\n\
yy7mx66W0/sOFa7/1mAZtEoIokDP3ShoqF4fVNb6XeqgQfaUHd8wJpDWHcR2OFwv\n\
plUUI1PLTktZ9uW2WE23b+ixNwJjJGwBDJPQEQFBE+vfmH0JP503wr5INS1poWg/\n\
j25sIWeYPHYeOrFp/eXaqhISP6G+q2IeTaWTXpwZj4LzXq5YOpk4bYEQ6mvRq7D1\n\
aHWfYmlEGepfaYR8Q0YqvvhYtMte3ITnuSJs171+GDqpdKcSwHnd6FudwGO4pcCO\n\
j4WcDuXc2CTHgH8gFTNhp/Y8/SpDOhvn9QIDAQAB\n\
-----END RSA PUBLIC KEY-----";

/// The test DCs (ip, handshake dc field). 10000+N = test DC N, main keys.
const TEST_DCS: [(&str, i32); 2] = [("149.154.167.40", 10002), ("149.154.175.117", 10003)];

struct RsaInterface {
    rsa: Rsa,
}

impl PublicRsaKeyInterface for RsaInterface {
    fn get_rsa_key(&self, fingerprints: &[i64]) -> Result<RsaKey> {
        let fingerprint = self.rsa.get_fingerprint();
        if fingerprints.contains(&fingerprint) {
            Result::Ok(RsaKey {
                rsa: self.rsa.clone(),
                fingerprint,
            })
        } else {
            Result::Err(Status::error_message("No matching RSA key"))
        }
    }

    fn drop_keys(&mut self) {}
}

struct Context {
    rsa: RsaInterface,
}

impl AuthKeyHandshakeContext for Context {
    fn get_dh_callback(&self) -> &dyn DhCallback {
        struct Callback;
        impl DhCallback for Callback {
            fn is_good_prime(&self, _prime_str: &[u8]) -> i32 {
                1
            }
            fn add_good_prime(&self, _prime_str: &[u8]) {}
            fn add_bad_prime(&self, _prime_str: &[u8]) {}
        }
        static CALLBACK: Callback = Callback;
        &CALLBACK
    }

    fn get_public_rsa_key_interface(&mut self) -> &mut dyn PublicRsaKeyInterface {
        &mut self.rsa
    }
}

/// One DC's live state: the persisted, reloaded auth data.
struct DcState {
    dc_number: i32,
    ip: &'static str,
    auth_data: AuthData,
}

/// Rebuilds an `AuthData` from its persisted form.
fn reload_auth_data(saved: &[u8]) -> AuthData {
    let (key, _handshake, salt, time_diff, session_id) = load_auth_data(saved).unwrap();
    let mut data = AuthData::new();
    // The default is PFS (temp keys); this flow stores main keys.
    data.set_use_pfs(false);
    data.set_main_auth_key(key);
    data.set_server_salt(salt.salt, tdutils::time::now());
    data.reset_server_time_difference(time_diff);
    data.set_session_id(session_id as u64);
    data.set_auth_flag(true);
    data
}

/// Connects, handshakes, and persists+reloads the auth state for one DC.
async fn establish(rsa: Rsa, (ip, hs_dc): (&'static str, i32)) -> Result<DcState> {
    let stream = TcpStream::connect((ip, 443)).await.unwrap();
    let mut context = Context {
        rsa: RsaInterface { rsa },
    };
    let mut handshake = AuthKeyHandshake::new(hs_dc, 0);
    let transport_type =
        TransportType::new(TransportTypeEnum::Tcp, hs_dc as i16, ProxySecret::default());
    // Consume the handshake; the resulting connection is dropped — the
    // session below re-dials with the persisted state.
    let raw_connection =
        match run_handshake(stream, transport_type, &mut context, 30.0, &mut handshake).await {
            Result::Ok(connection) => connection,
            Result::Err(status) => return Result::Err(status),
        };
    let _ = raw_connection;
    let auth_key = handshake.release_auth_key();
    let server_salt = handshake.get_server_salt();
    let time_diff = handshake.get_server_time_diff();

    let mut auth_data = AuthData::new();
    auth_data.set_main_auth_key(auth_key);
    auth_data.set_server_salt(server_salt as i64, tdutils::time::now());
    auth_data.reset_server_time_difference(time_diff);
    let mut session_id = 0u64;
    while session_id == 0 {
        session_id = tdutils::random::secure_uint64();
    }
    auth_data.set_session_id(session_id);
    auth_data.set_auth_flag(true);

    // Persist through the S61 byte-compatible layout and reload.
    let now = tdutils::time::now();
    let salt = ServerSalt {
        salt: server_salt as i64,
        valid_since: now,
        valid_until: now + 1.0e9,
    };
    let saved = save_auth_data(
        auth_data.get_main_auth_key(),
        None,
        &salt,
        time_diff,
        session_id as i64,
    );
    Result::Ok(DcState {
        dc_number: hs_dc - 10000,
        ip,
        auth_data: reload_auth_data(&saved),
    })
}

/// Validates a persisted key with an encrypted ping-pong against its DC.
async fn ping_dc(_rsa: Rsa, state: &mut DcState) -> Result<f64> {
    let stream = TcpStream::connect((state.ip, 443)).await.unwrap();
    let transport_type = TransportType::new(
        TransportTypeEnum::Tcp,
        (state.dc_number + 10000) as i16,
        ProxySecret::default(),
    );
    // run_ping mutates its copy; clone through save/load to keep the
    // manager's state pristine.
    let now = tdutils::time::now();
    let salt = ServerSalt {
        salt: state.auth_data.get_server_salt(now),
        valid_since: now,
        valid_until: now + 1.0e9,
    };
    let saved = save_auth_data(
        state.auth_data.get_main_auth_key(),
        None,
        &salt,
        state.auth_data.get_server_time_difference(),
        state.auth_data.get_session_id() as i64,
    );
    let mut auth_data = reload_auth_data(&saved);
    auth_data.set_header(Vec::new());
    let connection = match run_ping(stream, transport_type, Some(auth_data)).await {
        Result::Ok(connection) => connection,
        Result::Err(status) => return Result::Err(status),
    };
    Result::Ok(connection.extra().rtt)
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let rsa = Rsa::from_pem_public_key(TEST_DC_KEY).unwrap();
    let rsa2 = rsa.clone();

    // Establish both DCs concurrently.
    let (r1, r2) = tokio::join!(
        establish(rsa, TEST_DCS[0]),
        establish(rsa2.clone(), TEST_DCS[1])
    );
    let mut states = [r1.unwrap(), r2.unwrap()];
    for state in &states {
        println!(
            "multi_dc:dc{}:handshake:ok key_id={:016x}",
            state.dc_number,
            state.auth_data.get_main_auth_key().id()
        );
    }

    // Encrypted ping-pong on both live keys.
    for state in &mut states {
        match ping_dc(rsa2.clone(), state).await {
            Result::Ok(rtt) => println!("multi_dc:dc{}:ping:ok rtt={:.3}", state.dc_number, rtt),
            Result::Err(status) => {
                println!(
                    "multi_dc:dc{}:ping:error:{}",
                    state.dc_number,
                    status.message()
                );
                std::process::exit(1);
            }
        }
    }
    println!("S64 multi-DC: ALL GREEN");
}
