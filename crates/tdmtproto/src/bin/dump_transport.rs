//! S54 verification tool: the Rust mirror of
//! `conformance/mtproto_transport_oracle/cpp/dump_transport.cpp`. Prints the
//! same lines for the same scenarios; verify.sh diffs the outputs and feeds
//! each side's padded frames to the other.

use std::fs;
use std::io::Write as _;

use tdmtproto::transport::tcp::IntermediateTransport;
use tdmtproto::utils;
use tdutils::buffer::{BufferSlice, BufferWriter, ChainBufferWriter};

fn hex_out(data: &[u8]) -> String {
    String::from_utf8(utils::hex_encode(data)).expect("hex is ASCII")
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
