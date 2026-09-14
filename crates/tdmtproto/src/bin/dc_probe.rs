//! S66 debugging: sweep transport/DC/port combos against the real Telegram
//! test DC and report which ones get the auth-key handshake accepted.
//! One line per combo: `combo: <result>` where result is ok / the
//! handshake error / timeout (no server response).
//!
//! Usage: dc_probe

use std::time::Duration;

use tdmtproto::dh_callback::DhCallback;
use tdmtproto::handshake::{AuthKeyHandshake, AuthKeyHandshakeContext};
use tdmtproto::handshake_actor::run_handshake;
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

async fn probe(label: &str, port: u16, handshake_dc: i32, transport_type: TransportType) {
    let timeout = Duration::from_secs(20);
    let result = tokio::time::timeout(timeout, async {
        let stream = match TcpStream::connect(("149.154.167.40", port)).await {
            Ok(stream) => stream,
            Err(error) => return format!("connect-fail:{error}"),
        };
        let rsa = Rsa::from_pem_public_key(TEST_DC_KEY).unwrap();
        let mut context = Context {
            rsa: RsaInterface { rsa },
        };
        let mut handshake = AuthKeyHandshake::new(handshake_dc, 0);
        match run_handshake(stream, transport_type, &mut context, 15.0, &mut handshake).await {
            Result::Ok(_connection) => {
                format!("ok key={:016x}", handshake.get_auth_key().id())
            }
            Result::Err(status) => format!("error:{}", status.message()),
        }
    })
    .await;
    let result = result.unwrap_or_else(|_| "timeout".to_string());
    println!("combo:{label}: {result}");
}

fn main() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let secret = ProxySecret::default();
        // Handshake dc-field variants: the C++ live test uses 10002
        // (test DC2, main keys) in p_q_inner_data_dc.
        for (label, port, hs_dc, tx_dc) in [
            ("hs10002-plain-p443", 443u16, 10002i32, 2i16),
            ("hs10002-plain-p80", 80, 10002, 2),
            ("hs-2-plain-p443", 443, -2, 2),
            ("hs2-obf-p443", 443, 2, 2),
            ("hs10002-obf-p443", 443, 10002, 10002),
        ] {
            probe(
                label,
                port,
                hs_dc,
                TransportType::new(TransportTypeEnum::Tcp, tx_dc, secret.clone()),
            )
            .await;
        }
        // Obfuscated transports: the dc goes into the obfuscation header.
        for (label, port, hs_dc, dc) in [
            ("obf-int-dc2", 443, 2i32, 2i16),
            ("obf-int-dc-2", 443, 2, -2),
            ("obf-int-dc10002", 443, 10002i32, 10002i16),
            ("obf-int-dc-10002", 443, 10002, -10002),
        ] {
            probe(
                label,
                port,
                hs_dc,
                TransportType::new(TransportTypeEnum::ObfuscatedTcp, dc, secret.clone()),
            )
            .await;
        }
    });
}
