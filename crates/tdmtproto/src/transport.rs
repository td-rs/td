//! Port of `td/mtproto/IStreamTransport.{h,cpp}` and the TCP transports from
//! `td/mtproto/TcpTransport.{h,cpp}`: the abridged (a.k.a. intermediate)
//! framing, its padded variant and the stream-transport interface.
//!
//! The C++ transports keep `ChainBufferReader*`/`ChainBufferWriter*` to the
//! socket buffers (set by `init`); Rust cannot store those borrows inside a
//! boxed object owned by the same struct that owns the buffers
//! (self-reference), so the buffers are passed per call instead — the
//! semantics are unchanged, `init` only wires the input reader and writes
//! the transport magic.

use crate::proxy_secret::ProxySecret;
use crate::transport_type::{TransportType, TransportTypeEnum};
use tdutils::buffer::{BufferSlice, BufferWriter, ChainBufferReader, ChainBufferWriter};
use tdutils::random;
use tdutils::status::Result;

/// `td::mtproto::IStreamTransport`. The `error_code` out-parameter is only
/// set by the HTTP transport (S56); the TCP transports leave it untouched.
pub trait IStreamTransport {
    fn read_next(
        &mut self,
        input: &mut ChainBufferReader,
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
        TransportTypeEnum::ObfuscatedTcp => {
            // S55: the obfuscated transport lands with the AES-CTR byteflow.
            panic!("ObfuscatedTcp transport is implemented in S55")
        }
        TransportTypeEnum::Http => {
            // S56: the HTTP transport lands with the HTTP reader.
            panic!("Http transport is implemented in S56")
        }
    }
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
            assert!(size % 4 == 0);
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
    }

    impl OldTransport {
        pub fn new() -> Self {
            Self {
                impl_: IntermediateTransport::new(false),
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
            input: &mut ChainBufferReader,
            message: &mut BufferSlice,
            quick_ack: &mut u32,
            _error_code: &mut i32,
        ) -> Result<usize> {
            Result::Ok(self.impl_.read_from_stream(input, message, quick_ack))
        }

        fn support_quick_ack(&self) -> bool {
            true
        }

        fn write(&mut self, mut message: BufferWriter, quick_ack: bool, output: &mut ChainBufferWriter) {
            self.impl_.write_prepare_inplace(&mut message, quick_ack);
            output.append_buffer_slice(message.as_buffer_slice());
        }

        fn init(&mut self, _input: ChainBufferReader, output: &mut ChainBufferWriter) {
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
        assert_eq!(&all[..4], &0xeeeeee_eeu32.to_le_bytes());
        all[4..].to_vec()
    }

    #[test]
    fn intermediate_write_produces_known_bytes() {
        // Header = payload length (12) with no quick-ack bit.
        let frame = written_frame(b"\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\x0c", false);
        assert_eq!(frame.len(), 4 + 12);
        assert_eq!(u32::from_le_bytes(frame[..4].try_into().unwrap()), 12);
        assert_eq!(&frame[4..], b"\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\x0c");
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
        // Feed the frame through a fresh reader.
        let mut writer = ChainBufferWriter::new();
        writer.append(&frame, 0);
        let mut input = writer.extract_reader();
        let mut transport = tcp::OldTransport::new();
        let mut message = BufferSlice::null();
        let mut quick_ack = 0;
        let mut error_code = 0;
        // The transport does not care about the magic; it is consumed by the
        // connection layer before the first read.
        let wait = transport
            .read_next(&mut input, &mut message, &mut quick_ack, &mut error_code)
            .unwrap();
        assert_eq!(wait, 0);
        assert_eq!(message.as_slice(), b"hello world!");
        assert_eq!(quick_ack, 0);
        assert!(input.empty());
    }

    #[test]
    fn read_reports_expected_size_for_partial_frames() {
        let mut writer = ChainBufferWriter::new();
        let header = 20u32.to_le_bytes();
        writer.append(&header, 0);
        writer.append(b"partial", 0);
        let mut input = writer.extract_reader();
        let mut transport = tcp::OldTransport::new();
        let mut message = BufferSlice::null();
        let mut quick_ack = 0;
        let mut error_code = 0;
        let wait = transport
            .read_next(&mut input, &mut message, &mut quick_ack, &mut error_code)
            .unwrap();
        assert_eq!(wait, 24);
        assert_eq!(input.size(), 11, "nothing consumed while incomplete");
    }

    #[test]
    fn read_handles_quick_ack_frames() {
        let mut writer = ChainBufferWriter::new();
        let header = (0x4u32 | (1 << 31)).to_le_bytes();
        writer.append(&header, 0);
        let mut input = writer.extract_reader();
        let mut transport = tcp::OldTransport::new();
        let mut message = BufferSlice::null();
        let mut quick_ack = 0;
        let mut error_code = 0;
        let wait = transport
            .read_next(&mut input, &mut message, &mut quick_ack, &mut error_code)
            .unwrap();
        assert_eq!(wait, 0);
        assert_eq!(quick_ack, 4 | (1 << 31));
        assert!(message.is_null());
        assert!(input.empty());
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
        assert!(header >= 4 && header <= 4 + 15);
        assert_eq!(&message.as_slice()[4..8], b"ABCD");
    }
}
