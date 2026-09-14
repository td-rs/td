//! S60 handshake oracle: the Rust client — runs the full auth-key
//! generation against the mock DC and reports success (the mock validates
//! every message on the wire).
//!
//! Usage: handshake_client <port> <public-key.pem>

use tdmtproto::dh_callback::DhCallback;
use tdmtproto::handshake::{AuthKeyHandshake, AuthKeyHandshakeContext};
use tdmtproto::handshake_actor::run_handshake;
use tdmtproto::proxy_secret::ProxySecret;
use tdmtproto::rsa::{PublicRsaKeyInterface, Rsa, RsaKey};
use tdmtproto::transport_type::{TransportType, TransportTypeEnum};
use tdutils::status::{Result, Status};
use tokio::net::TcpStream;

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
        // The mock prime is the known-good Telegram prime; the C++ client
        // runs the full primality checks and passes either way.
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

fn main() {
    let args: Vec<String> = std::env::args().collect();
    assert_eq!(
        args.len(),
        3,
        "usage: handshake_client <port> <public-key.pem>"
    );
    let port: u16 = args[1].parse().unwrap();
    let pem = std::fs::read_to_string(&args[2]).unwrap();

    let rsa = Rsa::from_pem_public_key(&pem).unwrap();
    let mut context = Context {
        rsa: RsaInterface { rsa },
    };
    let mut handshake = AuthKeyHandshake::new(2, 0);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let transport_type = TransportType::new(TransportTypeEnum::Tcp, 2, ProxySecret::default());
        match run_handshake(stream, transport_type, &mut context, 30.0, &mut handshake).await {
            Result::Ok(_connection) => {
                println!("handshake:ok:key_id={}", handshake.get_auth_key().id());
                println!("handshake:ok:salt={}", handshake.get_server_salt());
                println!(
                    "handshake:ok:time_diff={:.3}",
                    handshake.get_server_time_diff()
                );
            }
            Result::Err(status) => {
                println!("handshake:error:{}", status.message());
                std::process::exit(1);
            }
        }
    });
}
