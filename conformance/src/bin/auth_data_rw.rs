//! S61 verification tool: the Rust mirror of
//! `conformance/mtproto_authdata_oracle/cpp/auth_data_rw.cpp`. Writes and
//! reads the same standalone auth-data file; the two implementations must
//! produce byte-identical files and identical canonical dumps.

use std::fs;

use tdmtproto::auth_data::ServerSalt;
use tdmtproto::auth_key::AuthKey;
use tdmtproto::dh_handshake::DhHandshake;
use tdmtproto::mtproto_api::tl;
use tdmtproto::persistence::{load_auth_data, save_auth_data};
use tdmtproto::utils;
use tdutils::bignum::{BigNum, BigNumContext};

const TG_PRIME_HEX: &str = "c71caeb9c6b1c9048e6c522f70f13f73980d40238e3e21c14934d037563d930f48198a0aa7c14058229493d22530f4dbfa336f6e0ac925139543aed44cce7c3720fd51f69458705ac68cd4fe6b6b13abdc9746512969328454f18faf8c595f642477fe96bb2a941d5bcd1d4ac8cc49880708fa9b378e3c4f3a9060bee67cf9a4a4a695811051907e162753b56b0f6b410dba74d8a84b2a14b3144e0ef1284754fd17ed950d5965b4b9dd46582db1178d169c6bc465b0d6ff9ca3928fef5b9ae4e418fc15e83ebea0f87fa9ff5eed70050ded2849f47bf959d956850ce929851f0d8115f635b105ee2e4e15d04b2454bf6f4fadf034b10403119cd8e3b92fcc5b";

/// The fixed DhHandshake state (restored through parse so the stored `b`
/// is deterministic): the Telegram prime, b = 0x42*32, g_a = 3^7 mod p.
fn make_handshake_state() -> DhHandshake {
    let prime = utils::hex_decode(TG_PRIME_HEX.as_bytes()).unwrap();
    let b = vec![0x42u8; 32];
    let mut ctx = BigNumContext::new();
    let mut g = BigNum::new();
    g.set_value(3);
    let prime_bn = BigNum::from_binary(&prime);
    let b_bn = BigNum::from_binary(&b);
    let g_b = BigNum::mod_exp(&g, &b_bn, &prime_bn, &mut ctx).to_binary(-1);
    let mut seven = BigNum::new();
    seven.set_value(7);
    let g_a = BigNum::mod_exp(&g, &seven, &prime_bn, &mut ctx).to_binary(-1);

    let mut state = Vec::new();
    tl::__i32(&mut state, 1 | 2); // HasConfig | HasGA
    tl::__string(&mut state, &prime);
    tl::__string(&mut state, &b);
    tl::__i32(&mut state, 3);
    tl::__string(&mut state, &g_b);
    tl::__string(&mut state, &g_a);
    let mut r = tl::Reader::new(&state);
    DhHandshake::parse(&mut r).unwrap()
}

fn hex_out(data: &[u8]) -> String {
    String::from_utf8(utils::hex_encode(data)).unwrap()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    assert_eq!(args.len(), 3, "usage: auth_data_rw --write|--read <path>");
    match args[1].as_str() {
        "--write" => {
            let mut key = AuthKey::new(0x0f0e0d0c0b0a0908, (0..=255).collect());
            key.set_auth_flag(true);
            key.set_created_at(1234.5);
            let handshake = make_handshake_state();
            let salt = ServerSalt {
                salt: 123456789,
                valid_since: 1000.5,
                valid_until: 2000.25,
            };
            let data = save_auth_data(&key, Some(&handshake), &salt, 42.5, -7);
            fs::write(&args[2], &data).unwrap();
            let hash = tdutils::crypto::sha256(&data);
            println!("w:sha256:{}", hex_out(&hash));
        }
        "--read" => {
            let data = fs::read(&args[2]).unwrap();
            let (key, handshake, salt, time_diff, session_id) = load_auth_data(&data).unwrap();
            let handshake = handshake.expect("handshake present in the oracle file");
            println!("a:key_id:{:x}", key.id());
            println!("a:created_at:{:.6}", key.created_at());
            println!("a:auth_flag:{}", i32::from(key.auth_flag()));
            println!("a:b:{}", hex_out(&handshake.get_g_b()));
            println!("a:g_a:{}", hex_out(&handshake.get_g_a()));
            println!("a:salt:{}", salt.salt);
            println!("a:salt_since:{:.6}", salt.valid_since);
            println!("a:salt_until:{:.6}", salt.valid_until);
            println!("a:time_diff:{:.6}", time_diff);
            println!("a:session:{session_id}");
        }
        other => panic!("unknown command {other}"),
    }
}
