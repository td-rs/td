//! Port of `td/mtproto/IStreamTransport.{h,cpp}` and the TCP transports from
//! `td/mtproto/TcpTransport.{h,cpp}`: the abridged (a.k.a. intermediate)
//! framing, its padded variant and the stream-transport interface.
//!
//! The C++ transports keep `ChainBufferReader*`/`ChainBufferWriter*` to the
//! socket buffers (set by `init`); Rust cannot store those borrows inside a
//! boxed object owned by the same struct that owns the buffers
//! (self-reference), so the buffers are passed per call instead — the
//! semantics are unchanged, `init` only writes the transport magic.

use crate::proxy_secret::ProxySecret;
use crate::tls_reader_byte_flow::TlsReaderByteFlow;
use crate::transport_type::{TransportType, TransportTypeEnum};
use tdutils::buffer::{
    BufferBuilder, BufferSlice, BufferWriter, ChainBufferReader, ChainBufferWriter,
};
use tdutils::byte_flow::{AesCtrByteFlow, ByteFlowInterface, ByteFlowSink};
use tdutils::crypto::{AesCtrState, sha256};
use tdutils::random;
use tdutils::status::Result;

/// `td::mtproto::IStreamTransport`. Each transport owns its input reader
/// (set by `init`, the C++ `input_` pointer); the output writer stays
/// per-call because the connection owns it. The `error_code` out-parameter
/// is only set by the HTTP transport (S56); the TCP transports leave it
/// untouched.
pub trait IStreamTransport {
    fn read_next(
        &mut self,
        message: &mut BufferSlice,
        quick_ack: &mut u32,
        error_code: &mut i32,
    ) -> Result<usize>;
    fn support_quick_ack(&self) -> bool;
    fn write(&mut self, message: BufferWriter, quick_ack: bool, output: &mut ChainBufferWriter);
    fn init(&mut self, input: ChainBufferReader, output: &mut ChainBufferWriter);
    fn can_read(&self) -> bool;
    fn can_write(&self) -> bool;
    fn max_prepend_size(&self) -> usize;
    fn max_append_size(&self) -> usize;
    fn get_type(&self) -> TransportType;
    fn use_random_padding(&self) -> bool;
}

/// `td::mtproto::create_transport`.
pub fn create_transport(transport_type: TransportType) -> Box<dyn IStreamTransport> {
    match transport_type.type_ {
        TransportTypeEnum::Tcp => Box::new(tcp::OldTransport::new()),
        TransportTypeEnum::ObfuscatedTcp => Box::new(tcp::ObfuscatedTransport::new(
            transport_type.dc_id,
            transport_type.secret,
        )),
        TransportTypeEnum::Http => {
            new_http_transport(transport_type.secret.get_raw_secret().to_vec())
        }
    }
}

/// The HTTP transport rides raw-socket HTTP framing (`http_transport.rs`),
/// which is not linked on wasm.
#[cfg(not(target_family = "wasm"))]
fn new_http_transport(secret: Vec<u8>) -> Box<dyn IStreamTransport> {
    Box::new(crate::http_transport::HttpTransport::new(secret))
}

#[cfg(target_family = "wasm")]
fn new_http_transport(_secret: Vec<u8>) -> Box<dyn IStreamTransport> {
    panic!("HTTP transport is not linked on this platform")
}

pub mod tcp {
    use super::*;

    /// `td::mtproto::tcp::IntermediateTransport`: 4-byte little-endian
    /// length framing (full packet length), quick-ack via the high bit of
    /// the length word, optional random padding on write.
    pub struct IntermediateTransport {
        with_padding: bool,
    }

    impl IntermediateTransport {
        pub fn new(with_padding: bool) -> Self {
            Self { with_padding }
        }

        /// `IntermediateTransport::read_from_stream`. Returns 0 if
        /// everything is ok, and the expected stream size otherwise.
        pub fn read_from_stream(
            &self,
            stream: &mut ChainBufferReader,
            message: &mut BufferSlice,
            quick_ack: &mut u32,
        ) -> usize {
            const HEADER_SIZE: usize = 4;
            let stream_size = stream.size();
            if stream_size < HEADER_SIZE {
                return HEADER_SIZE;
            }
            let mut header_bytes = [0u8; HEADER_SIZE];
            {
                let mut it = stream.begin().clone();
                it.advance(HEADER_SIZE, &mut header_bytes);
            }
            let data_size = u32::from_le_bytes(header_bytes);
            if data_size & (1 << 31) != 0 {
                *quick_ack = data_size;
                stream.advance(HEADER_SIZE, &mut []);
                return 0;
            }

            let total_size = data_size as usize + HEADER_SIZE;
            if stream_size < total_size {
                return total_size;
            }

            stream.advance(HEADER_SIZE, &mut []);
            *message = stream
                .cut_head_offset(data_size as usize)
                .move_as_buffer_slice();
            0
        }

        /// `IntermediateTransport::write_prepare_inplace`: writes the length
        /// header in place (with the quick-ack bit) and, when padding is on,
        /// appends a random 0..16-byte tail.
        pub fn write_prepare_inplace(&self, message: &mut BufferWriter, quick_ack: bool) {
            let mut size = message.size();
            assert!(size.is_multiple_of(4));
            assert!(size < (1 << 24));
            if quick_ack {
                size |= 1usize << 31;
            }

            const PREPEND_SIZE: usize = 4;
            {
                let prepend = message.prepare_prepend();
                assert!(prepend.len() >= PREPEND_SIZE);
            }
            message.confirm_prepend(PREPEND_SIZE);

            let mut append_size = 0;
            if self.with_padding() {
                append_size = random::secure_uint32() as usize % 16;
                {
                    let append = message.prepare_append();
                    let append = &mut append[..append_size];
                    random::secure_bytes(append);
                }
                message.confirm_append(append_size);
            }

            let header = (size + append_size) as u32;
            message.as_mutable_slice()[..4].copy_from_slice(&header.to_le_bytes());
        }

        /// `IntermediateTransport::init_output_stream`: the transport magic.
        pub fn init_output_stream(&self, stream: &mut ChainBufferWriter) {
            let magic: u32 = if self.with_padding() {
                0xdddddddd
            } else {
                0xeeeeeeee
            };
            stream.append(&magic.to_le_bytes(), 0);
        }

        pub fn with_padding(&self) -> bool {
            self.with_padding
        }
    }

    /// `td::mtproto::tcp::OldTransport`: the abridged transport used for
    /// plain TCP connections.
    pub struct OldTransport {
        impl_: IntermediateTransport,
        input: Option<ChainBufferReader>,
    }

    impl OldTransport {
        pub fn new() -> Self {
            Self {
                impl_: IntermediateTransport::new(false),
                input: None,
            }
        }
    }

    impl Default for OldTransport {
        fn default() -> Self {
            Self::new()
        }
    }

    impl IStreamTransport for OldTransport {
        fn read_next(
            &mut self,
            message: &mut BufferSlice,
            quick_ack: &mut u32,
            _error_code: &mut i32,
        ) -> Result<usize> {
            let input = self.input.as_mut().expect("init");
            input.sync_with_writer();
            Result::Ok(self.impl_.read_from_stream(input, message, quick_ack))
        }

        fn support_quick_ack(&self) -> bool {
            true
        }

        fn write(
            &mut self,
            mut message: BufferWriter,
            quick_ack: bool,
            output: &mut ChainBufferWriter,
        ) {
            self.impl_.write_prepare_inplace(&mut message, quick_ack);
            output.append_buffer_slice(message.as_buffer_slice());
        }

        fn init(&mut self, input: ChainBufferReader, output: &mut ChainBufferWriter) {
            self.input = Some(input);
            self.impl_.init_output_stream(output);
        }

        fn can_read(&self) -> bool {
            true
        }

        fn can_write(&self) -> bool {
            true
        }

        fn max_prepend_size(&self) -> usize {
            4
        }

        fn max_append_size(&self) -> usize {
            15
        }

        fn get_type(&self) -> TransportType {
            TransportType::new(TransportTypeEnum::Tcp, 0, ProxySecret::default())
        }

        fn use_random_padding(&self) -> bool {
            false
        }
    }

    /// `td::mtproto::tcp::ObfuscatedTransport` — the obfuscated transport of
    /// S55: a random 64-byte header (with avoidance rules), AES-CTR for the
    /// whole stream, optional random padding, and optional TLS-record
    /// emulation for MTProxy. The first 56 header bytes travel in plaintext;
    /// the last 8 (magic + dc_id) are encrypted with the stream key.
    pub struct ObfuscatedTransport<'a> {
        dc_id: i16,
        is_first_tls_packet: bool,
        secret: ProxySecret,
        header: Vec<u8>,
        impl_: IntermediateTransport,
        // The flows are boxed: they hold raw parent pointers into each
        // other, so their addresses must stay stable while the transport
        // struct itself may move (Box<dyn IStreamTransport>).
        tls_reader_byte_flow: Option<Box<TlsReaderByteFlow<'a>>>,
        aes_ctr_byte_flow: Option<Box<AesCtrByteFlow<'a>>>,
        byte_flow_sink: Option<Box<ByteFlowSink>>,
        output_key: [u8; 32],
        output_state: AesCtrState,
    }

    /// `TcpTransport.cpp`: `MAX_TLS_PACKET_LENGTH`.
    const MAX_TLS_PACKET_LENGTH: usize = 2878;

    /// The `fix_key` lambda of `ObfuscatedTransport::init`: with a non-empty
    /// proxy secret, `key = SHA256(key || proxy_secret)`.
    fn fix_key(key: &mut [u8; 32], proxy_secret: &[u8]) {
        if !proxy_secret.is_empty() {
            let mut data = Vec::with_capacity(32 + proxy_secret.len());
            data.extend_from_slice(key);
            data.extend_from_slice(proxy_secret);
            *key = sha256(&data);
        }
    }

    impl<'a> ObfuscatedTransport<'a> {
        pub fn new(dc_id: i16, secret: ProxySecret) -> Self {
            let use_padding = secret.use_random_padding();
            Self {
                dc_id,
                is_first_tls_packet: true,
                secret,
                header: Vec::new(),
                impl_: IntermediateTransport::new(use_padding),
                tls_reader_byte_flow: None,
                aes_ctr_byte_flow: None,
                byte_flow_sink: None,
                output_key: [0u8; 32],
                output_state: AesCtrState::new(),
            }
        }

        /// `ObfuscatedTransport::init`: generates the header, derives the
        /// stream keys and wires the byteflow pipeline.
        fn init_impl(&mut self, input: ChainBufferReader) {
            const HEADER_SIZE: usize = 64;
            let mut header = vec![0u8; HEADER_SIZE];
            let mut try_cnt = 0;
            loop {
                try_cnt += 1;
                assert!(try_cnt < 10);
                random::secure_bytes(&mut header);
                if self.secret.emulate_tls() {
                    break;
                }
                if header[0] == 0xef {
                    continue;
                }
                let first_int = u32::from_le_bytes(header[..4].try_into().unwrap());
                if first_int == 0x44414548
                    || first_int == 0x54534f50
                    || first_int == 0x20544547
                    || first_int == 0x4954504f
                    || first_int == 0xdddddddd
                    || first_int == 0xeeeeeeee
                    || first_int == 0x02010316
                {
                    continue;
                }
                let second_int = u32::from_le_bytes(header[4..8].try_into().unwrap());
                if second_int == 0 {
                    continue;
                }
                break;
            }
            let magic: u32 = if self.impl_.with_padding() {
                0xdddddddd
            } else {
                0xeeeeeeee
            };
            header[56..60].copy_from_slice(&magic.to_le_bytes());
            if self.dc_id != 0 {
                header[60..62].copy_from_slice(&self.dc_id.to_le_bytes());
            }

            let mut rheader = header.clone();
            rheader.reverse();
            let mut key: [u8; 32] = rheader[8..40].try_into().unwrap();
            let proxy_secret = self.secret.get_proxy_secret();
            fix_key(&mut key, proxy_secret);
            let iv: [u8; 16] = rheader[40..56].try_into().unwrap();

            let mut aes = Box::new(AesCtrByteFlow::new());
            aes.init(&key, &iv);
            let mut sink = Box::new(ByteFlowSink::default());
            if self.secret.emulate_tls() {
                let mut tls = Box::new(TlsReaderByteFlow::new());
                tls.set_input(input);
                tls.set_parent(&mut *aes);
                self.tls_reader_byte_flow = Some(tls);
                aes.set_parent(&mut *sink);
            } else {
                aes.set_input(input);
                aes.set_parent(&mut *sink);
            }
            self.aes_ctr_byte_flow = Some(aes);
            self.byte_flow_sink = Some(sink);

            let mut output_key: [u8; 32] = header[8..40].try_into().unwrap();
            fix_key(&mut output_key, proxy_secret);
            let output_iv: [u8; 16] = header[40..56].try_into().unwrap();
            let mut output_state = AesCtrState::new();
            output_state.init(&output_key, &output_iv);
            self.output_key = output_key;
            self.output_state = output_state;

            self.header = header;
            // The header is encrypted with the output stream; only the
            // encrypted last 8 bytes (magic + dc_id) are kept.
            let mut encrypted_header = self.header.clone();
            self.output_state.encrypt_in_place(&mut encrypted_header);
            let tail = encrypted_header[56..].to_vec();
            self.header[56..].copy_from_slice(&tail);
        }

        /// Test support (and the S55 oracle): re-derives all keys, the
        /// output state and the read pipeline from a previously generated
        /// header. Real clients never reuse headers — the server-side oracle
        /// and the read-path tests need a deterministic key schedule.
        pub fn reuse_header(&mut self, header: &[u8], input: ChainBufferReader) {
            assert_eq!(header.len(), 64, "obfuscated header is 64 bytes");
            let mut rheader = header.to_vec();
            rheader.reverse();
            let proxy_secret = self.secret.get_proxy_secret();
            let mut input_key: [u8; 32] = rheader[8..40].try_into().unwrap();
            fix_key(&mut input_key, proxy_secret);
            let input_iv: [u8; 16] = rheader[40..56].try_into().unwrap();

            let mut output_key: [u8; 32] = header[8..40].try_into().unwrap();
            fix_key(&mut output_key, proxy_secret);
            let output_iv: [u8; 16] = header[40..56].try_into().unwrap();

            let mut aes = Box::new(AesCtrByteFlow::new());
            aes.init(&input_key, &input_iv);
            let mut sink = Box::new(ByteFlowSink::default());
            if self.secret.emulate_tls() {
                let mut tls = Box::new(TlsReaderByteFlow::new());
                tls.set_input(input);
                tls.set_parent(&mut *aes);
                self.tls_reader_byte_flow = Some(tls);
                aes.set_parent(&mut *sink);
            } else {
                aes.set_input(input);
                aes.set_parent(&mut *sink);
            }
            self.aes_ctr_byte_flow = Some(aes);
            self.byte_flow_sink = Some(sink);

            self.output_key = output_key;
            self.output_state = AesCtrState::new();
            self.output_state.init(&output_key, &output_iv);
            self.header = header.to_vec();
            let mut encrypted_header = self.header.clone();
            self.output_state.encrypt_in_place(&mut encrypted_header);
            let tail = encrypted_header[56..].to_vec();
            self.header[56..].copy_from_slice(&tail);
        }

        /// `ObfuscatedTransport::do_write_main`: the obfuscated-only path —
        /// the (encrypted) header is prepended to the first packet.
        fn do_write_main(&mut self, message: BufferWriter, output: &mut ChainBufferWriter) {
            let mut builder = BufferBuilder::default();
            builder.append_buffer_slice(message.as_buffer_slice());
            if !self.header.is_empty() {
                builder.prepend(&self.header);
                self.header.clear();
            }
            output.append_buffer_slice(builder.extract());
        }

        /// `ObfuscatedTransport::do_write_tls(BufferWriter&&)`: splits the
        /// packet into TLS records of at most `MAX_TLS_PACKET_LENGTH` bytes.
        fn do_write_tls(&mut self, message: BufferWriter, output: &mut ChainBufferWriter) {
            assert!(self.header.len() <= MAX_TLS_PACKET_LENGTH);
            if message.size() + self.header.len() > MAX_TLS_PACKET_LENGTH {
                let buffer_slice = message.as_buffer_slice();
                let mut slice = buffer_slice.as_slice();
                while !slice.is_empty() {
                    let n = MAX_TLS_PACKET_LENGTH - self.header.len();
                    let buf = buffer_slice.from_slice(&slice[..n]);
                    slice = &slice[n..];
                    let mut builder = BufferBuilder::default();
                    builder.append_buffer_slice(buf);
                    self.do_write_tls_builder(builder, output);
                }
                return;
            }
            let mut builder = BufferBuilder::default();
            builder.append_buffer_slice(message.as_buffer_slice());
            self.do_write_tls_builder(builder, output);
        }

        /// `ObfuscatedTransport::do_write_tls(BufferBuilder&&)`: wraps one
        /// builder into a TLS application-data record (and the fake
        /// ChangeCipherSpec prefix for the very first packet).
        fn do_write_tls_builder(
            &mut self,
            mut builder: BufferBuilder,
            output: &mut ChainBufferWriter,
        ) {
            if !self.header.is_empty() {
                builder.prepend(&self.header);
                self.header.clear();
            }

            let size = builder.size();
            assert!(size <= MAX_TLS_PACKET_LENGTH);

            let mut buf = [0x17, 0x03, 0x03, 0, 0];
            buf[3] = (size >> 8) as u8;
            buf[4] = size as u8;
            builder.prepend(&buf);

            if self.is_first_tls_packet {
                self.is_first_tls_packet = false;
                builder.prepend(b"\x14\x03\x03\x00\x01\x01");
            }

            output.append_buffer_slice(builder.extract());
        }
    }

    impl<'a> IStreamTransport for ObfuscatedTransport<'a> {
        fn read_next(
            &mut self,
            message: &mut BufferSlice,
            quick_ack: &mut u32,
            _error_code: &mut i32,
        ) -> Result<usize> {
            if self.secret.emulate_tls() {
                self.tls_reader_byte_flow.as_mut().expect("init").wakeup();
            } else {
                self.aes_ctr_byte_flow.as_mut().expect("init").wakeup();
            }
            let r = self.impl_.read_from_stream(
                self.byte_flow_sink.as_mut().expect("init").get_output(),
                message,
                quick_ack,
            );
            Result::Ok(r)
        }

        fn support_quick_ack(&self) -> bool {
            true
        }

        fn write(
            &mut self,
            mut message: BufferWriter,
            quick_ack: bool,
            output: &mut ChainBufferWriter,
        ) {
            self.impl_.write_prepare_inplace(&mut message, quick_ack);
            {
                let slice = message.as_slice();
                let len = slice.len();
                // SAFETY: in-place AES-CTR over the writer's own buffer —
                // the C++ `output_state_.encrypt(message.as_slice(),
                // message.as_mutable_slice())`.
                self.output_state.encrypt_in_place(unsafe {
                    std::slice::from_raw_parts_mut(slice.as_ptr() as *mut u8, len)
                });
            }
            if self.secret.emulate_tls() {
                self.do_write_tls(message, output);
            } else {
                self.do_write_main(message, output);
            }
        }

        fn init(&mut self, input: ChainBufferReader, _output: &mut ChainBufferWriter) {
            self.init_impl(input);
        }

        fn can_read(&self) -> bool {
            true
        }

        fn can_write(&self) -> bool {
            true
        }

        fn max_prepend_size(&self) -> usize {
            let mut res = 4;
            if self.secret.emulate_tls() {
                res += 5;
                if self.is_first_tls_packet {
                    res += 6;
                }
            }
            res += self.header.len();
            if res & 3 != 0 {
                res += 4 - (res & 3);
            }
            res
        }

        fn max_append_size(&self) -> usize {
            15
        }

        fn get_type(&self) -> TransportType {
            TransportType::new(
                TransportTypeEnum::ObfuscatedTcp,
                self.dc_id,
                self.secret.clone(),
            )
        }

        fn use_random_padding(&self) -> bool {
            self.secret.use_random_padding()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn written_frame(payload: &[u8], quick_ack: bool) -> Vec<u8> {
        let mut output = ChainBufferWriter::new();
        let mut transport = tcp::OldTransport::new();
        transport.init(ChainBufferReader::null(), &mut output);
        let mut message = BufferWriter::with_reserve(payload.len(), 4, 15);
        message.as_mutable_slice().copy_from_slice(payload);
        transport.write(message, quick_ack, &mut output);
        let mut reader = output.extract_reader();
        // 4 magic bytes + 4 header bytes + payload.
        assert_eq!(reader.size(), 4 + 4 + payload.len());
        let mut all = vec![0u8; reader.size()];
        let all_len = all.len();
        assert_eq!(reader.advance(all_len, &mut all), all_len);
        // The magic must precede any packet.
        assert_eq!(&all[..4], &0xeeee_eeeeu32.to_le_bytes());
        all[4..].to_vec()
    }

    #[test]
    fn intermediate_write_produces_known_bytes() {
        // Header = payload length (12) with no quick-ack bit.
        let frame = written_frame(b"\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\x0c", false);
        assert_eq!(frame.len(), 4 + 12);
        assert_eq!(u32::from_le_bytes(frame[..4].try_into().unwrap()), 12);
        assert_eq!(
            &frame[4..],
            b"\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\x0c"
        );
    }

    #[test]
    fn quick_ack_bit_is_set_in_the_length_word() {
        let frame = written_frame(b"AAAA", true);
        let header = u32::from_le_bytes(frame[..4].try_into().unwrap());
        assert_eq!(header, 4 | (1 << 31));
    }

    #[test]
    fn intermediate_read_roundtrips() {
        let frame = written_frame(b"hello world!", false);
        // Feed the frame through a fresh reader owned by the transport.
        let mut writer = ChainBufferWriter::new();
        writer.append(&frame, 0);
        let input = writer.extract_reader();
        let mut output = ChainBufferWriter::new();
        let mut transport = tcp::OldTransport::new();
        transport.init(input, &mut output);
        let mut message = BufferSlice::null();
        let mut quick_ack = 0;
        let mut error_code = 0;
        let wait = transport
            .read_next(&mut message, &mut quick_ack, &mut error_code)
            .unwrap();
        assert_eq!(wait, 0);
        assert_eq!(message.as_slice(), b"hello world!");
        assert_eq!(quick_ack, 0);
        // Everything consumed: the next read needs a fresh header.
        let wait = transport
            .read_next(&mut message, &mut quick_ack, &mut error_code)
            .unwrap();
        assert_eq!(wait, 4);
    }

    #[test]
    fn read_reports_expected_size_for_partial_frames() {
        let mut writer = ChainBufferWriter::new();
        let header = 20u32.to_le_bytes();
        writer.append(&header, 0);
        writer.append(b"partial", 0);
        let input = writer.extract_reader();
        let mut output = ChainBufferWriter::new();
        let mut transport = tcp::OldTransport::new();
        transport.init(input, &mut output);
        let mut message = BufferSlice::null();
        let mut quick_ack = 0;
        let mut error_code = 0;
        let wait = transport
            .read_next(&mut message, &mut quick_ack, &mut error_code)
            .unwrap();
        assert_eq!(wait, 24);
        assert!(message.is_null(), "nothing consumed while incomplete");
    }

    #[test]
    fn read_handles_quick_ack_frames() {
        let mut writer = ChainBufferWriter::new();
        let header = (0x4u32 | (1 << 31)).to_le_bytes();
        writer.append(&header, 0);
        let input = writer.extract_reader();
        let mut output = ChainBufferWriter::new();
        let mut transport = tcp::OldTransport::new();
        transport.init(input, &mut output);
        let mut message = BufferSlice::null();
        let mut quick_ack = 0;
        let mut error_code = 0;
        let wait = transport
            .read_next(&mut message, &mut quick_ack, &mut error_code)
            .unwrap();
        assert_eq!(wait, 0);
        assert_eq!(quick_ack, 4 | (1 << 31));
        assert!(message.is_null());
    }

    #[test]
    fn padded_transport_frames_stay_in_bounds() {
        // The padded variant is used by the obfuscated transport (S55); its
        // framing must be identical modulo the 0..16 random tail.
        let mut output = ChainBufferWriter::new();
        let transport = tcp::IntermediateTransport::new(true);
        transport.init_output_stream(&mut output);
        let mut message = BufferWriter::with_reserve(4, 4, 15);
        message.as_mutable_slice().copy_from_slice(b"ABCD");
        transport.write_prepare_inplace(&mut message, false);
        let header = u32::from_le_bytes(message.as_slice()[..4].try_into().unwrap());
        assert!((4..=4 + 15).contains(&header));
        assert_eq!(&message.as_slice()[4..8], b"ABCD");
    }

    #[test]
    fn http_transport_turn_flips_after_a_complete_response() {
        // The S78 fuzz finding: the C++ driver (`RawConnection::flush_read`)
        // guards its read loop with `while (transport_->can_read())`; the
        // HTTP transport ends the read turn when a response completes, and a
        // faithful caller must write the next query before reading again
        // (`read_next` CHECKs `can_read()` for exactly this reason).
        let mut output = ChainBufferWriter::new();
        let mut transport = crate::http_transport::HttpTransport::new(Vec::new());
        let mut message = BufferWriter::with_reserve(4, transport.max_prepend_size(), 0);
        message.as_mutable_slice().copy_from_slice(b"ABCD");
        transport.write(message, false, &mut output);
        assert!(transport.can_read());

        let mut writer = ChainBufferWriter::new();
        writer.append(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nABCD", 0);
        transport.init(writer.extract_reader(), &mut output);

        let mut message = BufferSlice::null();
        let mut quick_ack = 0;
        let mut error_code = 0;
        let wait = transport
            .read_next(&mut message, &mut quick_ack, &mut error_code)
            .unwrap();
        assert_eq!(wait, 0);
        assert_eq!(message.as_slice(), b"ABCD");
        assert_eq!(error_code, 200);
        // The response completed the turn: reading again is a caller bug.
        assert!(!transport.can_read());
        assert!(transport.can_write());
    }
}
