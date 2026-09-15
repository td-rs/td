//! Fuzz target: the stream-transport frame decoders (old/abridged,
//! intermediate, intermediate-with-padding, HTTP) — arbitrary bytes framed
//! through `ChainBufferWriter` and read via the transport selected by the
//! first seed byte. The write→read alternation mirrors the real session
//! flow (the transports assert `can_read`/`can_write` like the C++
//! `CHECK`s); frame parsing must report wait/error states, never panic.

#![no_main]

use libfuzzer_sys::fuzz_target;
use tdmtproto::transport::create_transport;
use tdmtproto::transport_type::{TransportType, TransportTypeEnum};
use tdutils::buffer::{BufferSlice, BufferWriter, ChainBufferWriter};

fuzz_target!(|data: &[u8]| {
    if data.len() < 2 {
        return;
    }
    let type_ = match data[0] % 4 {
        0 => TransportTypeEnum::Tcp,
        1 => TransportTypeEnum::ObfuscatedTcp,
        2 => TransportTypeEnum::Http,
        _ => TransportTypeEnum::Tcp,
    };
    let mut transport = create_transport(TransportType::new(type_, 2, Default::default()));

    // The real flow writes a query first (the HTTP transport alternates
    // write/read turns; the tcp ones ignore the turn).
    let prepend = transport.max_prepend_size();
    let append = transport.max_append_size();
    let mut message = BufferWriter::with_reserve(4, prepend, append);
    message.as_mutable_slice().copy_from_slice(b"ABCD");
    let mut output = ChainBufferWriter::new();
    transport.write(message, false, &mut output);

    let mut writer = ChainBufferWriter::new();
    writer.append(&data[1..], 0);
    let input = writer.extract_reader();
    transport.init(input, &mut output);

    // Drain: each read either consumes a frame, asks for more bytes or
    // reports a transport error — all three are fine; a panic is not.
    // The `can_read` guard mirrors the C++ driver (`RawConnection::flush_read`:
    // `while (transport_->can_read())`) — the HTTP transport flips the turn
    // when a response completes, and read_next may only be called again after
    // the next write. The iteration cap keeps a fixed, exhausted buffer from
    // spinning (the real driver instead waits on the event loop).
    for _ in 0..64 {
        if !transport.can_read() {
            break;
        }
        let mut message = BufferSlice::null();
        let mut quick_ack = 0u32;
        let mut error_code = 0i32;
        match transport.read_next(&mut message, &mut quick_ack, &mut error_code) {
            tdutils::status::Result::Ok(0) => {}
            tdutils::status::Result::Ok(_) => break, // needs more bytes than we have
            tdutils::status::Result::Err(_) => break,
        }
    }
});
