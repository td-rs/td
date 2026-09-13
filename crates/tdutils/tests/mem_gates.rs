//! Memory gates for the gzip byte-flow pipeline — ports of
//! `TEST(Gzip, encode_decode_flow_big)` and `TEST(Gzip, decode_encode_flow_bomb)`.
//!
//! These assert EXACT global buffer-accounting equality
//! (`td::BufferAllocator::get_buffer_mem`), which the C++ test runner gets for
//! free because td tests run sequentially in one thread. Rust runs tests in
//! parallel across threads AND separate processes, so the accounting gates
//! live in this dedicated binary as a single test that runs both gates
//! sequentially on one thread — a second test thread would race its TLS slab
//! teardown (a 16 KiB charge) against the baseline measurement.

use tdutils::buffer::{ChainBufferWriter, create_reader, get_buffer_mem};
use tdutils::byte_flow::{
    ByteFlowInterface, ByteFlowSink, ByteFlowSource, GzipByteFlow, Options, Watermark,
};
use tdutils::gzip::GzipMode;
use tdutils::status::Status;

/// Port of `TEST(Gzip, encode_decode_flow_big)` — 40 MB streamed through
/// four gzip flows while the sink output is drained incrementally; live
/// buffer memory must stay a small fraction of the streamed volume, and
/// every tracked byte must be back when the pipeline is dropped.
fn encode_decode_flow_big() {
    // C++ calls clear_thread_locals() before measuring; the Rust
    // equivalent is to force the per-thread 16 KiB slab to exist first,
    // so the baseline already includes it.
    drop(create_reader(8));
    let start_mem = get_buffer_mem();

    {
        let s = vec![b'a'; 200_000];
        let mut input_writer = ChainBufferWriter::new();
        let mut source = ByteFlowSource::new(input_writer.extract_reader());
        let mut encode = GzipByteFlow::with_mode(GzipMode::Encode);
        let mut decode = GzipByteFlow::with_mode(GzipMode::Decode);
        let mut encode2 = GzipByteFlow::with_mode(GzipMode::Encode);
        let mut decode2 = GzipByteFlow::with_mode(GzipMode::Decode);
        let mut sink = ByteFlowSink::default();
        source.set_parent(&mut encode);
        encode.set_parent(&mut decode);
        decode.set_parent(&mut encode2);
        encode2.set_parent(&mut decode2);
        decode2.set_parent(&mut sink);

        assert!(!sink.is_ready());
        let n = 200;
        let mut left_size = n * s.len();
        let validate = |chunk: &[u8], left_size: &mut usize| {
            assert!(chunk.len() <= *left_size);
            *left_size -= chunk.len();
            assert!(chunk.iter().all(|&c| c == b'a'));
        };

        for _ in 0..n {
            input_writer.append(&s, 0);
            source.wakeup();
            // The bound means nothing on its own: it just checks we do
            // not buffer anywhere near the 40 MB streamed through.
            let extra_mem = get_buffer_mem() - start_mem;
            assert!(extra_mem < 10 << 20, "extra_mem = {extra_mem}");

            let size = sink.get_output().size();
            let mut head = sink.get_output().cut_head_offset(size);
            let chunk = head.move_as_buffer_slice();
            validate(chunk.as_slice(), &mut left_size);
        }
        assert!(!sink.is_ready());
        source.close_input(Status::ok());
        assert!(sink.is_ready());
        assert!(sink.status().is_ok(), "{}", sink.status());
        let tail = sink.result().move_as_buffer_slice();
        validate(tail.as_slice(), &mut left_size);
        assert_eq!(left_size, 0);
    }
    // C++ does clear_thread_locals() and asserts equality; the Rust TLS slab
    // is permanent, so the baseline includes it instead.
    assert_eq!(start_mem, get_buffer_mem());
}

/// Port of `TEST(Gzip, decode_encode_flow_bomb)` — a compressed 12.5 MB bomb
/// streamed through a watermarked encode/decode pipeline while drained;
/// live memory must stay bounded and the output must be all 'a's.
fn decode_encode_flow_bomb() {
    drop(create_reader(8));
    let start_mem = get_buffer_mem();

    let bomb = {
        let mut input_writer = ChainBufferWriter::new();
        let mut source = ByteFlowSource::new(input_writer.extract_reader());
        let mut encode = GzipByteFlow::with_mode(GzipMode::Encode);
        let mut sink = ByteFlowSink::default();
        source.set_parent(&mut encode);
        encode.set_parent(&mut sink);

        let s = vec![b'a'; 1 << 16];
        for _ in 0..200 {
            input_writer.append(&s, 0);
            source.wakeup();
        }
        source.close_input(Status::ok());
        assert!(sink.is_ready());
        assert!(sink.status().is_ok(), "{}", sink.status());
        let bomb = sink.result().move_as_buffer_slice().as_slice().to_vec();
        // 12.5 MB of 'a' compresses to a few KB — that is the bomb.
        assert!(
            bomb.len() < 1 << 20,
            "bomb compressed to {} bytes",
            bomb.len()
        );
        bomb
    };

    {
        let mut input_writer = ChainBufferWriter::new();
        let mut source = ByteFlowSource::new(input_writer.extract_reader());
        let decode_options = Options {
            write_watermark: Watermark {
                low: 2 << 20,
                high: 4 << 20,
            },
            read_watermark: Watermark::default(),
        };
        let encode_options = Options {
            write_watermark: Watermark::default(),
            read_watermark: Watermark {
                low: 2 << 20,
                high: 4 << 20,
            },
        };
        let mut decode = GzipByteFlow::with_mode(GzipMode::Decode);
        decode.set_options(decode_options);
        let mut encode2 = GzipByteFlow::with_mode(GzipMode::Encode);
        encode2.set_options(encode_options);
        let mut decode2 = GzipByteFlow::with_mode(GzipMode::Decode);
        decode2.set_options(decode_options);
        let mut encode3 = GzipByteFlow::with_mode(GzipMode::Encode);
        encode3.set_options(encode_options);
        let mut decode3 = GzipByteFlow::with_mode(GzipMode::Decode);
        decode3.set_options(decode_options);
        let mut sink = ByteFlowSink::default();

        source.set_parent(&mut decode);
        decode.set_parent(&mut encode2);
        encode2.set_parent(&mut decode2);
        decode2.set_parent(&mut encode3);
        encode3.set_parent(&mut decode3);
        decode3.set_parent(&mut sink);

        assert!(!sink.is_ready());
        let mut left_size = 200 * (1 << 16);
        input_writer.append(&bomb, 0);
        source.close_input(Status::ok());

        loop {
            decode3.wakeup();
            decode2.wakeup();
            decode.wakeup();
            source.wakeup();
            let extra_mem = get_buffer_mem() - start_mem;
            // The bound means nothing on its own: it just checks we do not
            // buffer anywhere near the 12.5 MB decoded through the chain.
            assert!(extra_mem < 5 << 20, "extra_mem = {extra_mem}");
            let size = sink.get_output().size();
            let mut head = sink.get_output().cut_head_offset(size);
            let chunk = head.move_as_buffer_slice();
            assert!(chunk.size() <= left_size);
            assert!(chunk.as_slice().iter().all(|&c| c == b'a'));
            left_size -= chunk.size();
            if sink.is_ready() {
                break;
            }
        }
        assert_eq!(left_size, 0);
        assert!(sink.status().is_ok());
    }
    assert_eq!(start_mem, get_buffer_mem());
}

/// Both gates, sequentially on this one thread — the C++ runner semantics.
#[test]
fn memory_gates() {
    decode_encode_flow_bomb();
    encode_decode_flow_big();
}
