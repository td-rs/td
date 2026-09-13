//! S54 verification tool: the Rust mirror of
//! `conformance/mtproto_transport_oracle/cpp/dump_transport.cpp`. Prints the
//! same lines for the same scenarios; verify.sh diffs the outputs and feeds
//! each side's padded frames to the other.

use std::fs;
use std::io::Write as _;

use tdmtproto::auth_key::AuthKey;
use tdmtproto::dh_handshake::DhHandshake;
use tdmtproto::http_transport::HttpTransport;
use tdmtproto::mtproto_api::tl;
use tdmtproto::packet::{self, ReadResult};
use tdmtproto::packet_info::{PacketInfo, PacketType};
use tdmtproto::proxy_secret::ProxySecret;
use tdmtproto::transport::IStreamTransport;
use tdmtproto::transport::tcp::{IntermediateTransport, ObfuscatedTransport};
use tdmtproto::utils;
use tdutils::bignum::{BigNum, BigNumContext};
use tdutils::buffer::{BufferSlice, BufferWriter, ChainBufferReader, ChainBufferWriter};
use tdutils::status::Result;

fn hex_out(data: &[u8]) -> String {
    String::from_utf8(utils::hex_encode(data)).expect("hex is ASCII")
}

/// `--crypto-read[{-server}] <frame-hex> <version 1|2>`.
fn crypto_read_impl(args: &[String], x: usize) {
    assert_eq!(args.len(), 4);
    let mut frame = utils::hex_decode(args[2].as_bytes()).unwrap();
    let version: i32 = args[3].parse().unwrap();
    let key: Vec<u8> = (0..=255).collect();
    let auth_key = AuthKey::new(0x0f0e0d0c0b0a0908, key);
    let mut pi = PacketInfo::new();
    pi.version = version;
    pi.no_crypto_flag = false;
    pi.type_ = PacketType::Common;
    match packet::read_with_x(&mut frame, 0, &auth_key, &mut pi, x) {
        Result::Err(status) => println!("c:r:error:{}", status.message()),
        Result::Ok(ReadResult::Packet { offset, len }) => {
            println!(
                "c:r:ok:{}:{:x}:{:x}:{:x}:{}",
                hex_out(&frame[offset..offset + len]),
                pi.salt,
                pi.session_id,
                pi.message_id.get(),
                pi.seq_no
            );
        }
        Result::Ok(_) => println!("c:r:error:unexpected read result"),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    assert!(args.len() >= 3, "usage: dump_transport <command> ...");
    match args[1].as_str() {
        "--abridged-write" => {
            // --abridged-write <payload-hex> <quick_ack 0|1>
            assert_eq!(args.len(), 4);
            let transport = IntermediateTransport::new(false);
            let payload = utils::hex_decode(args[2].as_bytes()).unwrap();
            let mut message = BufferWriter::from_slice_with_reserve(&payload, 4, 15);
            let quick_ack = args[3] == "1";
            transport.write_prepare_inplace(&mut message, quick_ack);
            println!("w:{}", hex_out(message.as_slice()));
        }
        "--abridged-read" => {
            // --abridged-read <frame-hex>
            assert_eq!(args.len(), 3);
            let transport = IntermediateTransport::new(false);
            let frame = utils::hex_decode(args[2].as_bytes()).unwrap();
            let mut writer = ChainBufferWriter::new();
            writer.append(&frame, 0);
            let mut input = writer.extract_reader();
            let mut message = BufferSlice::null();
            let mut quick_ack = 0;
            let wait = transport.read_from_stream(&mut input, &mut message, &mut quick_ack);
            if wait != 0 {
                println!("r:wait:{wait}");
            } else {
                println!("r:ok:{}:{quick_ack}", hex_out(message.as_slice()));
            }
        }
        "--padded-write-file" => {
            // --padded-write-file <path> <count>
            assert_eq!(args.len(), 4);
            let transport = IntermediateTransport::new(true);
            let count: usize = args[3].parse().unwrap();
            let mut out = Vec::new();
            for i in 0..count {
                let payload_size = 4 + 4 * (i % 8);
                let payload: Vec<u8> = (0..payload_size)
                    .map(|j| b'a' + ((i + j) % 26) as u8)
                    .collect();
                let mut message = BufferWriter::from_slice_with_reserve(&payload, 4, 15);
                transport.write_prepare_inplace(&mut message, false);
                out.extend_from_slice(message.as_slice());
            }
            let mut f = fs::File::create(&args[2]).unwrap();
            f.write_all(&out).unwrap();
        }
        "--obfuscated-write-file" => {
            // --obfuscated-write-file <path> <count> <secret-hex>
            assert_eq!(args.len(), 5);
            let secret =
                ProxySecret::from_binary(&utils::hex_decode(args[4].as_bytes()).unwrap(), false)
                    .unwrap();
            let mut transport = ObfuscatedTransport::new(2, secret);
            let mut output = ChainBufferWriter::new();
            transport.init(ChainBufferReader::null(), &mut output);
            let count: usize = args[3].parse().unwrap();
            for i in 0..count {
                let payload_size = 4 + 4 * (i % 8);
                let payload: Vec<u8> = (0..payload_size)
                    .map(|j| b'a' + ((i + j) % 26) as u8)
                    .collect();
                let message = BufferWriter::from_slice_with_reserve(
                    &payload,
                    transport.max_prepend_size(),
                    transport.max_append_size(),
                );
                transport.write(message, false, &mut output);
            }
            let mut reader = output.extract_reader();
            let mut all = vec![0u8; reader.size()];
            let all_len = all.len();
            assert_eq!(reader.advance(all_len, &mut all), all_len);
            fs::write(&args[2], all).unwrap();
        }
        "--obfuscated-read-file" => {
            // --obfuscated-read-file <path> <secret-hex>
            // Server-side read: the stream starts with the client's 64-byte
            // header (first 56 bytes plaintext); the AES-CTR state derives
            // from the reversed header, optionally TLS-record-wrapped.
            assert_eq!(args.len(), 4);
            let secret_bytes = utils::hex_decode(args[3].as_bytes()).unwrap();
            let proxy_secret = ProxySecret::from_binary(&secret_bytes, false).unwrap();
            let data = fs::read(&args[2]).unwrap();
            assert!(data.len() >= 64);
            let header = &data[..64];
            let mut rheader = header.to_vec();
            rheader.reverse();
            let mut key: [u8; 32] = rheader[8..40].try_into().unwrap();
            let proxy_secret_slice = proxy_secret.get_proxy_secret();
            if !proxy_secret_slice.is_empty() {
                let mut input = Vec::with_capacity(32 + proxy_secret_slice.len());
                input.extend_from_slice(&key);
                input.extend_from_slice(proxy_secret_slice);
                key = tdutils::crypto::sha256(&input);
            }
            let iv: [u8; 16] = rheader[40..56].try_into().unwrap();
            let mut stream = data[64..].to_vec();
            if proxy_secret.emulate_tls() {
                // Strip the TLS record framing: optionally the
                // ChangeCipherSpec prefix, then 5-byte record headers.
                let mut stripped = Vec::new();
                let mut pos = 0;
                if stream.len() >= 6 && &stream[..6] == b"\x14\x03\x03\x00\x01\x01" {
                    pos = 6;
                }
                while pos < stream.len() {
                    if pos + 5 > stream.len() || &stream[pos..pos + 3] != b"\x17\x03\x03" {
                        println!("o:error:invalid tls record framing");
                        return;
                    }
                    let len = ((stream[pos + 3] as usize) << 8) | stream[pos + 4] as usize;
                    pos += 5;
                    if pos + len > stream.len() {
                        println!("o:error:truncated tls record");
                        return;
                    }
                    stripped.extend_from_slice(&stream[pos..pos + len]);
                    pos += len;
                }
                stream = stripped;
            }
            let mut aes = tdutils::crypto::AesCtrState::new();
            aes.init(&key, &iv);
            aes.decrypt_in_place(&mut stream);
            let transport = IntermediateTransport::new(false);
            let mut writer = ChainBufferWriter::new();
            writer.append(&stream, 0);
            let mut input = writer.extract_reader();
            while !input.empty() {
                let mut message = BufferSlice::null();
                let mut quick_ack = 0;
                let wait = transport.read_from_stream(&mut input, &mut message, &mut quick_ack);
                if wait != 0 {
                    println!("o:wait:{wait}");
                    break;
                }
                println!("o:ok:{}:{quick_ack}", hex_out(message.as_slice()));
            }
        }
        "--http-write" => {
            // --http-write <payload-hex> [secret-hex]
            assert!(args.len() == 3 || args.len() == 4);
            let secret = if args.len() == 4 {
                utils::hex_decode(args[3].as_bytes()).unwrap()
            } else {
                Vec::new()
            };
            let mut transport = HttpTransport::new(secret);
            let mut output = ChainBufferWriter::new();
            transport.init(ChainBufferReader::null(), &mut output);
            let payload = utils::hex_decode(args[2].as_bytes()).unwrap();
            let message = BufferWriter::from_slice_with_reserve(
                &payload,
                transport.max_prepend_size(),
                transport.max_append_size(),
            );
            transport.write(message, false, &mut output);
            let mut reader = output.extract_reader();
            let mut all = vec![0u8; reader.size()];
            let all_len = all.len();
            assert_eq!(reader.advance(all_len, &mut all), all_len);
            println!("h:w:{}", hex_out(&all));
        }
        "--http-read" => {
            // --http-read <frame-hex> [secret-hex]
            assert!(args.len() == 3 || args.len() == 4);
            let secret = if args.len() == 4 {
                utils::hex_decode(args[3].as_bytes()).unwrap()
            } else {
                Vec::new()
            };
            let mut transport = HttpTransport::new(secret);
            let data = utils::hex_decode(args[2].as_bytes()).unwrap();
            let mut writer = ChainBufferWriter::new();
            writer.append(&data, 0);
            let input = writer.extract_reader();
            let mut output = ChainBufferWriter::new();
            transport.init(input, &mut output);
            // Flip turn to Read with a dummy write (the transport starts in Write).
            let dummy = BufferWriter::from_slice_with_reserve(
                &[],
                transport.max_prepend_size(),
                transport.max_append_size(),
            );
            transport.write(dummy, false, &mut output);
            let mut message = BufferSlice::null();
            let mut quick_ack = 0;
            let mut error_code = 0;
            match transport.read_next(&mut message, &mut quick_ack, &mut error_code) {
                Result::Err(status) => println!("h:error:{}", status.message()),
                Result::Ok(wait) if wait != 0 => println!("h:wait:{wait}"),
                Result::Ok(_) => println!("h:ok:{}:{error_code}", hex_out(message.as_slice())),
            }
        }
        "--crypto-write" => {
            // --crypto-write <body-hex> <version 1|2> [random-pad 0|1]
            assert!(args.len() == 4 || args.len() == 5);
            let body = utils::hex_decode(args[2].as_bytes()).unwrap();
            let version: i32 = args[3].parse().unwrap();
            let key: Vec<u8> = (0..=255).collect();
            let auth_key = AuthKey::new(0x0f0e0d0c0b0a0908, key);
            let mut pi = PacketInfo::new();
            pi.version = version;
            pi.no_crypto_flag = false;
            pi.type_ = PacketType::Common;
            pi.salt = 0x0102030405060708;
            pi.session_id = 0x1112131415161718;
            if args.len() == 5 {
                pi.use_random_padding = args[4] == "1";
            }
            let packet = packet::write(&body, &auth_key, &mut pi, 0, 0);
            println!("c:w:{}", hex_out(packet.as_slice()));
        }
        "--crypto-read" => {
            // --crypto-read <frame-hex> <version 1|2> — the client-side read
            // (X=8).
            crypto_read_impl(&args, 8);
        }
        "--crypto-read-server" => {
            // --crypto-read-server <frame-hex> <version 1|2> — the
            // server-side read (X=0).
            crypto_read_impl(&args, 0);
        }
        "--dh-gen" => {
            // --dh-gen <g-int> <prime-hex> <b-hex> <g_a-hex>
            assert_eq!(args.len(), 6);
            let g_int: i32 = args[2].parse().unwrap();
            let prime = utils::hex_decode(args[3].as_bytes()).unwrap();
            let b = utils::hex_decode(args[4].as_bytes()).unwrap();
            let g_a = utils::hex_decode(args[5].as_bytes()).unwrap();

            let mut ctx = BigNumContext::new();
            let mut g = BigNum::new();
            g.set_value(g_int as u32);
            let prime_bn = BigNum::from_binary(&prime);
            let b_bn = BigNum::from_binary(&b);
            let g_b = BigNum::mod_exp(&g, &b_bn, &prime_bn, &mut ctx).to_binary(-1);

            let mut state = Vec::new();
            tl::__i32(&mut state, 1 | 2); // HasConfig | HasGA
            tl::__string(&mut state, &prime);
            tl::__string(&mut state, &b);
            tl::__i32(&mut state, g_int);
            tl::__string(&mut state, &g_b);
            tl::__string(&mut state, &g_a);

            let mut r = tl::Reader::new(&state);
            let mut handshake = DhHandshake::parse(&mut r).unwrap();
            println!("d:g_b:{}", hex_out(&handshake.get_g_b()));
            println!(
                "d:g_ab:{}",
                hex_out(&handshake.get_g_ab().to_binary(2048 / 8))
            );
            let (key_id, key) = handshake.gen_key();
            println!("d:key:{}", hex_out(&key));
            println!("d:key_id:{key_id}");
            struct NoCallback;
            impl tdmtproto::dh_callback::DhCallback for NoCallback {
                fn is_good_prime(&self, _prime_str: &[u8]) -> i32 {
                    -1
                }
                fn add_good_prime(&self, _prime_str: &[u8]) {}
                fn add_bad_prime(&self, _prime_str: &[u8]) {}
            }
            let status = handshake.run_checks(true, &NoCallback);
            println!(
                "d:check:{}",
                if status.is_ok() {
                    "ok"
                } else {
                    status.message()
                }
            );
        }
        "--read-file" => {
            // --read-file <path> [padded 0|1]
            assert!(args.len() == 3 || args.len() == 4);
            let padded = args.len() == 4 && args[3] == "1";
            let transport = IntermediateTransport::new(padded);
            let data = fs::read(&args[2]).unwrap();
            let mut writer = ChainBufferWriter::new();
            writer.append(&data, 0);
            let mut input = writer.extract_reader();
            while !input.empty() {
                let mut message = BufferSlice::null();
                let mut quick_ack = 0;
                let wait = transport.read_from_stream(&mut input, &mut message, &mut quick_ack);
                if wait != 0 {
                    println!("f:wait:{wait}");
                    break;
                }
                println!("f:ok:{}:{quick_ack}", hex_out(message.as_slice()));
            }
        }
        other => panic!("unknown command {other}"),
    }
}
