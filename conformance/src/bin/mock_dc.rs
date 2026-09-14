//! S60 handshake oracle: a deterministic mock MTProto server speaking the
//! server half of the auth-key generation handshake. Both the C++ td client
//! and the Rust port run the full handshake against it; every client message
//! is validated (RSA blob decryption, DH math, nonces, SHA-1 wrapping) and
//! the per-message log must come out identical for both clients.
//!
//! Usage: mock_dc <port> <public-key.pem> <private-key.pem>
//! The RSA keypair is generated per run and written to the two PEM files.

use rsa::pkcs1::{EncodeRsaPrivateKey, EncodeRsaPublicKey, LineEnding};
use rsa::traits::{PrivateKeyParts, PublicKeyParts};
use rsa::{RsaPrivateKey, RsaPublicKey};
use tdmtproto::kdf::tmp_kdf;
use tdmtproto::mtproto_api::{self, Int128};
use tdutils::bignum::{BigNum, BigNumContext};
use tdutils::crypto::{aes_ige_decrypt, aes_ige_encrypt, sha1, sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// The classic MTProto test pq (494C553B × 53911073).
const PQ: u64 = 0x17ED_4894_1A08_F981;
/// The standard Telegram 2048-bit safe prime.
const TG_PRIME_HEX: &str = "c71caeb9c6b1c9048e6c522f70f13f73980d40238e3e21c14934d037563d930f48198a0aa7c14058229493d22530f4dbfa336f6e0ac925139543aed44cce7c3720fd51f69458705ac68cd4fe6b6b13abdc9746512969328454f18faf8c595f642477fe96bb2a941d5bcd1d4ac8cc49880708fa9b378e3c4f3a9060bee67cf9a4a4a695811051907e162753b56b0f6b410dba74d8a84b2a14b3144e0ef1284754fd17ed950d5965b4b9dd46582db1178d169c6bc465b0d6ff9ca3928fef5b9ae4e418fc15e83ebea0f87fa9ff5eed70050ded2849f47bf959d956850ce929851f0d8115f635b105ee2e4e15d04b2454bf6f4fadf034b10403119cd8e3b92fcc5b";
/// The mock's fixed DH exponent.
const SERVER_B: [u8; 32] = [0x42; 32];
/// Fixed server nonce and "server time".
const SERVER_NONCE: [u8; 16] = [0x11; 16];
const SERVER_TIME: i32 = 1_700_000_000;

fn tl_string(out: &mut Vec<u8>, data: &[u8]) {
    tdmtproto::mtproto_api::tl::__string(out, data);
}

fn log(line: &str) {
    println!("{line}");
}

#[allow(dead_code)] // kept for the protocol-error branches of the server half
fn fail(line: &str) -> ! {
    log(&format!("error:{line}"));
    std::process::exit(1);
}

fn i32_at(data: &[u8], pos: usize) -> i32 {
    i32::from_le_bytes(data[pos..pos + 4].try_into().unwrap())
}

fn i64_at(data: &[u8], pos: usize) -> i64 {
    i64::from_le_bytes(data[pos..pos + 8].try_into().unwrap())
}

/// Reads one abridged-framed message from the stream.
async fn read_frame(stream: &mut tokio::net::TcpStream) -> Vec<u8> {
    let mut header = [0u8; 4];
    stream.read_exact(&mut header).await.unwrap();
    let len = u32::from_le_bytes(header) as usize;
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).await.unwrap();
    payload
}

async fn write_frame(stream: &mut tokio::net::TcpStream, body: &[u8]) {
    // No-crypto server frame, intermediate-transport framed:
    // [inner_len u32][auth_key_id 0][odd msg_id][inner_len][body].
    // The transport length word covers everything after itself.
    let mut frame = Vec::new();
    frame.extend_from_slice(&0u64.to_le_bytes());
    frame.extend_from_slice(&0x0123_4567_89ab_cdefu64.to_le_bytes());
    frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
    frame.extend_from_slice(body);
    let mut framed = Vec::with_capacity(4 + frame.len());
    framed.extend_from_slice(&(frame.len() as u32).to_le_bytes());
    framed.extend_from_slice(&frame);
    stream.write_all(&framed).await.unwrap();
}

/// Parses a client no-crypto packet body (the transport layer unwraps the
/// [msg_id][len] prefix for the handshake connection).
fn parse_client_packet(packet: &[u8]) -> Vec<u8> {
    // [auth_key_id 8][msg_id 8][len 4][body...][pad]
    assert_eq!(i64_at(packet, 0), 0, "handshake packets are unencrypted");
    let len = i32_at(packet, 16) as usize;
    assert!(packet.len() >= 20 + len, "short packet");
    packet[20..20 + len].to_vec()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    assert_eq!(args.len(), 4, "usage: mock_dc <port> <public.pem> <private.pem>");
    let port: u16 = args[1].parse().unwrap();

    // Generate the RSA keypair and write the PEMs for the clients.
    let mut rng = rsa::rand_core::OsRng;
    let private = RsaPrivateKey::new(&mut rng, 2048).expect("key generation");
    let public = RsaPublicKey::from(&private);
    let public_pem = public.to_pkcs1_pem(LineEnding::LF).expect("public pem");
    let private_pem = private.to_pkcs1_pem(LineEnding::LF).expect("private pem");
    std::fs::write(&args[2], public_pem.as_bytes()).unwrap();
    std::fs::write(&args[3], private_pem.as_bytes()).unwrap();
    let fingerprint = {
        let rsa = tdmtproto::rsa::Rsa::from_pem_public_key(&public_pem).unwrap();
        rsa.get_fingerprint()
    };

    let prime = BigNum::from_binary(
        &tdmtproto::utils::hex_decode(TG_PRIME_HEX.as_bytes()).unwrap(),
    );
    let mut ctx = BigNumContext::new();
    let mut g = BigNum::new();
    g.set_value(3);
    let b_server = BigNum::from_binary(&SERVER_B);
    let g_a = BigNum::mod_exp(&g, &b_server, &prime, &mut ctx).to_binary(-1);
    let p_q = PQ.to_be_bytes().to_vec();
    let p = (PQ / 0x494C_553B) as u64;
    let q = (PQ / 0x5391_1073) as u64;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let listener = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
        // Signals the verify script (which polls the mock log) that clients
        // may connect — RSA keygen above can take tens of seconds in debug
        // builds, longer than any fixed sleep.
        log("listening:ok");
        let (mut stream, _) = listener.accept().await.unwrap();

        // The client's transport writes the 0xeeeeeeee magic first — a raw
        // 4-byte word, not a length-prefixed frame.
        let mut magic = [0u8; 4];
        stream.read_exact(&mut magic).await.unwrap();
        assert_eq!(magic, 0xeeee_eeeeu32.to_le_bytes());
        log("magic:ok");

        // 1. req_pq_multi
        let packet = read_frame(&mut stream).await;
        let body = parse_client_packet(&packet);
        assert_eq!(i32_at(&body, 0), 0xbe7e_8ef1u32 as i32, "req_pq_multi id");
        let nonce: [u8; 16] = body[4..20].try_into().unwrap();
        log("req_pq_multi:ok");

        // 2. resPQ
        let mut resp = Vec::new();
        resp.extend_from_slice(&0x0516_2463u32.to_le_bytes());
        resp.extend_from_slice(&nonce);
        resp.extend_from_slice(&SERVER_NONCE);
        tl_string(&mut resp, &p_q);
        resp.extend_from_slice(&mtproto_api::tl::VECTOR_ID.to_le_bytes());
        resp.extend_from_slice(&1i32.to_le_bytes());
        resp.extend_from_slice(&fingerprint.to_le_bytes());
        write_frame(&mut stream, &resp).await;

        // 3. req_DH_params
        let packet = read_frame(&mut stream).await;
        let body = parse_client_packet(&packet);
        assert_eq!(i32_at(&body, 0), 0xd712_e4beu32 as i32, "req_DH_params id");
        let client_nonce: [u8; 16] = body[4..20].try_into().unwrap();
        let server_nonce: [u8; 16] = body[20..36].try_into().unwrap();
        assert_eq!(client_nonce, nonce, "nonce");
        assert_eq!(server_nonce, SERVER_NONCE, "server_nonce");
        let mut pos = 36;
        let (p_str, adv) = read_tl_string(&body, pos);
        pos = adv;
        let (q_str, adv) = read_tl_string(&body, pos);
        pos = adv;
        // pq_factorize may return the factors in either order, and TL
        // string encoding carries minimal-width bytes; compare numerically.
        fn be_value(bytes: &[u8]) -> u64 {
            bytes.iter().fold(0u64, |acc, &b| (acc << 8) | u64::from(b))
        }
        assert!(
            (be_value(&p_str) == p && be_value(&q_str) == q)
                || (be_value(&p_str) == q && be_value(&q_str) == p),
            "p/q"
        );
        let fp = i64_at(&body, pos);
        pos += 8;
        assert_eq!(fp, fingerprint, "fingerprint");
        let (encrypted_data, _) = read_tl_string(&body, pos);
        assert_eq!(encrypted_data.len(), 256, "encrypted_data size");
        log("req_DH_params:pq:ok");

        // Decrypt the RSA blob.
        let n = BigNum::from_binary(&private.n().to_bytes_be());
        let d = BigNum::from_binary(&private.d().to_bytes_be());
        let c = BigNum::from_binary(&encrypted_data);
        let decrypted = BigNum::mod_exp(&c, &d, &n, &mut ctx).to_binary(256);
        let hash = sha256(&decrypted[32..]);
        let mut aes_key = [0u8; 32];
        for i in 0..32 {
            aes_key[i] = decrypted[i] ^ hash[i];
        }
        let aes_iv = [0u8; 32];
        let mut aes_iv = aes_iv;
        let mut data_with_hash = decrypted[32..].to_vec();
        tdutils::crypto::aes_ige_decrypt_in_place(&aes_key, &mut aes_iv, &mut data_with_hash);
        data_with_hash[..192].reverse();
        assert_eq!(
            &data_with_hash[192..],
            &sha256(&[aes_key.as_slice(), &data_with_hash[..192]].concat()),
            "data hash"
        );
        let inner = &data_with_hash[..192];
        assert_eq!(i32_at(inner, 0), 0xa9f5_5f95u32 as i32, "p_q_inner_data id");
        let mut pos = 4;
        let (pq_in, adv) = read_tl_string(inner, pos);
        pos = adv;
        let (p_in, adv) = read_tl_string(inner, pos);
        pos = adv;
        let (q_in, adv) = read_tl_string(inner, pos);
        pos = adv;
        assert_eq!(pq_in, p_q, "inner pq");
        assert!(
            (be_value(&p_in) == p && be_value(&q_in) == q)
                || (be_value(&p_in) == q && be_value(&q_in) == p),
            "inner p/q"
        );
        let nonce_in: [u8; 16] = inner[pos..pos + 16].try_into().unwrap();
        pos += 16;
        let server_nonce_in: [u8; 16] = inner[pos..pos + 16].try_into().unwrap();
        pos += 16;
        let new_nonce: [u8; 32] = inner[pos..pos + 32].try_into().unwrap();
        assert_eq!(nonce_in, nonce, "inner nonce");
        assert_eq!(server_nonce_in, SERVER_NONCE, "inner server_nonce");
        log("req_DH_params:rsa:ok");
        log("req_DH_params:inner:ok");

        // 4. server_DH_params_ok
        let mut inner = Vec::new();
        mtproto_api::Server_DH_inner_data {
            nonce: Int128 { words: words128(&nonce) },
            server_nonce: Int128 { words: words128(&SERVER_NONCE) },
            g: 3,
            dh_prime: prime.to_binary(-1),
            g_a: g_a.clone(),
            server_time: SERVER_TIME,
        }
        .store_boxed(&mut inner);
        let mut answer = sha1(&inner).to_vec();
        answer.extend_from_slice(&inner);
        while answer.len() % 16 != 0 {
            answer.push(0);
        }
        let (tmp_key, mut tmp_iv) = tmp_kdf(&SERVER_NONCE, &new_nonce);
        tdutils::crypto::aes_ige_encrypt_in_place(&tmp_key, &mut tmp_iv, &mut answer);
        let mut resp = Vec::new();
        mtproto_api::Server_DH_Params {
            nonce: Int128 { words: words128(&nonce) },
            server_nonce: Int128 { words: words128(&SERVER_NONCE) },
            encrypted_answer: answer,
        }
        .store_boxed(&mut resp);
        write_frame(&mut stream, &resp).await;

        // 5. set_client_DH_params
        let packet = read_frame(&mut stream).await;
        let body = parse_client_packet(&packet);
        assert_eq!(i32_at(&body, 0), 0xf504_5f1fu32 as i32, "set_client_DH_params id");
        let nonce_in: [u8; 16] = body[4..20].try_into().unwrap();
        let server_nonce_in: [u8; 16] = body[20..36].try_into().unwrap();
        assert_eq!(nonce_in, nonce, "nonce");
        assert_eq!(server_nonce_in, SERVER_NONCE, "server_nonce");
        let (encrypted, _) = read_tl_string(&body, 36);
        assert!(encrypted.len() % 16 == 0, "encrypted size");
        let (tmp_key, mut tmp_iv) = tmp_kdf(&SERVER_NONCE, &new_nonce);
        let mut data = encrypted;
        tdutils::crypto::aes_ige_decrypt_in_place(&tmp_key, &mut tmp_iv, &mut data);
        // SHA1(inner) + client_DH_inner_data#6643b654 nonce server_nonce
        // retry_id g_b + 0..15 bytes of padding.
        assert_eq!(i32_at(&data, 20), 0x6643_b654u32 as i32, "client_DH_inner_data id");
        let nonce_in: [u8; 16] = data[24..40].try_into().unwrap();
        let server_nonce_in: [u8; 16] = data[40..56].try_into().unwrap();
        assert_eq!(nonce_in, nonce, "nonce");
        assert_eq!(server_nonce_in, SERVER_NONCE, "server_nonce");
        let (g_b_client, inner_end) = read_tl_string(&data, 64);
        let inner_size = inner_end - 20;
        assert!(data.len() - 20 - inner_size < 16, "pad size");
        assert_eq!(sha1(&data[20..20 + inner_size]), &data[..20], "sha1");
        log("set_client_DH_params:sha1:ok");
        log("set_client_DH_params:nonce:ok");

        // The shared key: g_b_client ^ b_server mod p.
        let g_b_bn = BigNum::from_binary(&g_b_client);
        let auth_key = BigNum::mod_exp(&g_b_bn, &b_server, &prime, &mut ctx).to_binary(256);
        let auth_key_sha1 = sha1(&auth_key);
        let mut hash_input = Vec::with_capacity(16 + 1 + 8);
        hash_input.extend_from_slice(&new_nonce);
        hash_input.push(1);
        hash_input.extend_from_slice(&auth_key_sha1[..8]);
        let new_nonce_hash1 = sha1(&hash_input);
        log("set_client_DH_params:dh:ok");

        // 6. dh_gen_ok
        let mut resp = Vec::new();
        mtproto_api::dh_gen_ok {
            nonce: Int128 { words: words128(&nonce) },
            server_nonce: Int128 { words: words128(&SERVER_NONCE) },
            new_nonce_hash1: Int128 {
                words: words128(new_nonce_hash1[4..20].try_into().unwrap()),
            },
        }
        .store_boxed(&mut resp);
        write_frame(&mut stream, &resp).await;
        log("dh_gen_ok:sent");
    });
    // Exit explicitly after the one handshake — the verify script waits on
    // this process, and a lingering runtime must not wedge it.
    std::process::exit(0);
}

fn words128(bytes: &[u8; 16]) -> [i32; 4] {
    std::array::from_fn(|i| i32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap()))
}

/// Reads a TL string at `pos`, returns (payload, next_pos).
fn read_tl_string(data: &[u8], pos: usize) -> (Vec<u8>, usize) {
    let first = data[pos] as usize;
    let (len, header) = if first < 254 {
        (first, 1)
    } else if first == 254 {
        (
            data[pos + 1] as usize | (data[pos + 2] as usize) << 8 | (data[pos + 3] as usize) << 16,
            4,
        )
    } else {
        (
            data[pos + 1] as usize
                | (data[pos + 2] as usize) << 8
                | (data[pos + 3] as usize) << 16
                | (data[pos + 4] as usize) << 24,
            8,
        )
    };
    let payload = data[pos + header..pos + header + len].to_vec();
    // The padding follows the length header *and* the payload together.
    (payload, pos + (header + len).div_ceil(4) * 4)
}
