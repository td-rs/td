//! Port of `td/utils/Gzip.h` + `Gzip.cpp` directly on `libz-sys`, which
//! compiles the same vendored stock zlib tdlib links against — the codec is
//! literally the `z_stream` state machine the C++ drives.
//!
//! Despite the name, tdlib's streams are zlib-format: encode uses
//! `deflateInit2(..., 15, ...)` (zlib header) and decode uses
//! `inflateInit2(MAX_WBITS + 32)` (gzip/zlib auto-detect). Stock zlib is
//! required on both counts — pure-Rust backends are chunk-sensitive on encode
//! (breaking the byte-identity the C++ `Gzip` flow test asserts) and flate2's
//! safe API cannot express the auto-detect window bits; the golden gzip test
//! guards the auto-detect.

use crate::buffer::{BufferSlice, BufferWriter, ChainBufferWriter};
use crate::status::{Result, Status};

use libz_sys::{
    Bytef, Z_DEFAULT_STRATEGY, Z_DEFLATED, Z_FINISH, Z_NO_FLUSH, Z_OK, Z_STREAM_END, deflate,
    deflateEnd, deflateInit2_, inflate, inflateEnd, inflateInit2_, uInt, z_stream, zlibVersion,
};

/// `td::Gzip::Mode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GzipMode {
    Empty,
    Encode,
    Decode,
}

/// `td::Gzip::State`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GzipState {
    Running,
    Done,
}

enum GzipInner {
    Encode(Box<z_stream>),
    Decode(Box<z_stream>),
}

/// A `z_stream` with every field at its C zero — exactly the `std::memset`
/// the C++ `init_common` does before `deflateInit2`/`inflateInit2`; the null
/// `zalloc`/`zfree` are zlib's `Z_NULL`, selecting the default allocator.
/// Built field-by-field: `mem::zeroed` trips the runtime invalid-value check
/// on the function-pointer fields.
fn new_stream() -> Box<z_stream> {
    // SAFETY: `Option<fn>` has the exact layout of the bare fn pointer
    // (null niche), so `None` transmutes to the null `Z_NULL` allocator.
    let null_alloc: libz_sys::alloc_func =
        unsafe { std::mem::transmute::<Option<libz_sys::alloc_func>, libz_sys::alloc_func>(None) };
    let null_free: libz_sys::free_func =
        unsafe { std::mem::transmute::<Option<libz_sys::free_func>, libz_sys::free_func>(None) };
    Box::new(z_stream {
        next_in: std::ptr::null_mut(),
        avail_in: 0,
        total_in: 0,
        next_out: std::ptr::null_mut(),
        avail_out: 0,
        total_out: 0,
        msg: std::ptr::null_mut(),
        state: std::ptr::null_mut(),
        zalloc: null_alloc,
        zfree: null_free,
        opaque: std::ptr::null_mut(),
        data_type: 0,
        adler: 0,
        reserved: 0,
    })
}

impl GzipInner {
    fn stream_ref(&self) -> &z_stream {
        match self {
            Self::Encode(stream) | Self::Decode(stream) => stream,
        }
    }

    fn stream_mut(&mut self) -> &mut z_stream {
        match self {
            Self::Encode(stream) | Self::Decode(stream) => stream,
        }
    }
}

/// `td::Gzip` — a streaming zlib codec over caller-provided buffers. The
/// buffers passed to [`Gzip::set_input`] / [`Gzip::set_output`] must stay
/// alive until their contents are flushed (the C++ pointer contract); the
/// `z_stream` lives in a `Box` so libz can keep internal pointers stable.
pub struct Gzip {
    inner: Option<GzipInner>,
    mode: GzipMode,
    /// `td::Gzip::input_size_` — total bytes currently set as input.
    input_size: usize,
    /// `td::Gzip::output_size_` — total bytes currently set as output.
    output_size: usize,
    close_input_flag: bool,
}

impl Gzip {
    pub fn new() -> Self {
        Self {
            inner: None,
            mode: GzipMode::Empty,
            input_size: 0,
            output_size: 0,
            close_input_flag: false,
        }
    }

    /// `td::Gzip::init(mode)`.
    pub fn init(&mut self, mode: GzipMode) -> Status {
        match mode {
            GzipMode::Encode => self.init_encode(),
            GzipMode::Decode => self.init_decode(),
            GzipMode::Empty => {
                self.clear();
                Status::ok()
            }
        }
    }

    /// `td::Gzip::init_encode` — `deflateInit2(6, Z_DEFLATED, 15,
    /// MAX_MEM_LEVEL, Z_DEFAULT_STRATEGY)`: zlib format, level 6.
    pub fn init_encode(&mut self) -> Status {
        assert!(self.mode == GzipMode::Empty, "init_encode on a live codec");
        self.init_common();
        let mut stream = new_stream();
        let ret = unsafe {
            deflateInit2_(
                &mut *stream,
                6,
                Z_DEFLATED,
                15,
                8, // MAX_MEM_LEVEL
                Z_DEFAULT_STRATEGY,
                zlibVersion(),
                std::mem::size_of::<z_stream>() as std::ffi::c_int,
            )
        };
        if ret != Z_OK {
            return Status::error_message(&format!("zlib deflate init failed: {ret}"));
        }
        self.mode = GzipMode::Encode;
        self.inner = Some(GzipInner::Encode(stream));
        Status::ok()
    }

    /// `td::Gzip::init_decode` — `inflateInit2(MAX_WBITS + 32)`: gzip/zlib
    /// auto-detect (windowBits 47).
    pub fn init_decode(&mut self) -> Status {
        assert!(self.mode == GzipMode::Empty, "init_decode on a live codec");
        self.init_common();
        let mut stream = new_stream();
        let ret = unsafe {
            inflateInit2_(
                &mut *stream,
                47, // MAX_WBITS + 32
                zlibVersion(),
                std::mem::size_of::<z_stream>() as std::ffi::c_int,
            )
        };
        if ret != Z_OK {
            return Status::error_message(&format!("zlib inflate init failed: {ret}"));
        }
        self.mode = GzipMode::Decode;
        self.inner = Some(GzipInner::Decode(stream));
        Status::ok()
    }

    /// `td::Gzip::set_input` — the slice must outlive `run` calls until the
    /// data is consumed (tracked by [`Gzip::used_input`]).
    pub fn set_input(&mut self, input: &[u8]) {
        assert_eq!(self.input_size, 0, "set_input with unconsumed input");
        assert!(!self.close_input_flag, "set_input after close_input");
        assert!(
            input.len() <= uInt::MAX as usize,
            "input too large for zlib"
        );
        self.input_size = input.len();
        let stream = self.stream_mut();
        stream.next_in = input.as_ptr() as *mut Bytef;
        stream.avail_in = input.len() as uInt;
    }

    /// `td::Gzip::set_output` — the slice must outlive `run` calls until the
    /// data is flushed (tracked by [`Gzip::used_output`]).
    pub fn set_output(&mut self, output: &mut [u8]) {
        assert_eq!(self.output_size, 0, "set_output with unflushed output");
        assert!(
            output.len() <= uInt::MAX as usize,
            "output too large for zlib"
        );
        self.output_size = output.len();
        let stream = self.stream_mut();
        stream.next_out = output.as_mut_ptr();
        stream.avail_out = output.len() as uInt;
    }

    /// `td::Gzip::close_input`.
    pub fn close_input(&mut self) {
        self.close_input_flag = true;
    }

    pub fn need_input(&self) -> bool {
        self.left_input() == 0
    }

    pub fn need_output(&self) -> bool {
        self.left_output() == 0
    }

    /// `td::Gzip::left_input` — `impl_->stream_.avail_in`.
    pub fn left_input(&self) -> usize {
        self.inner
            .as_ref()
            .map_or(0, |inner| inner.stream_ref().avail_in as usize)
    }

    /// `td::Gzip::left_output` — `impl_->stream_.avail_out`.
    pub fn left_output(&self) -> usize {
        self.inner
            .as_ref()
            .map_or(0, |inner| inner.stream_ref().avail_out as usize)
    }

    /// `td::Gzip::used_input`.
    pub fn used_input(&self) -> usize {
        self.input_size - self.left_input()
    }

    /// `td::Gzip::used_output`.
    pub fn used_output(&self) -> usize {
        self.output_size - self.left_output()
    }

    /// `td::Gzip::flush_input` — consumes the remaining input, returning the
    /// number of bytes used; the unconsumed remainder becomes the new input.
    pub fn flush_input(&mut self) -> usize {
        let res = self.used_input();
        self.input_size = self.left_input();
        res
    }

    /// `td::Gzip::flush_output` — consumes the remaining output, returning
    /// the number of bytes produced.
    pub fn flush_output(&mut self) -> usize {
        let res = self.used_output();
        self.output_size = self.left_output();
        res
    }

    /// `td::Gzip::run()` — drives the codec; `Running` means more buffers
    /// may be needed, `Done` means the stream finished (the codec resets).
    pub fn run(&mut self) -> Result<GzipState> {
        // SAFETY: the caller keeps the buffers set via set_input/set_output
        // alive until flushed (the C++ contract); zlib updates next_in/
        // next_out/avail_* in place.
        let inner = self.inner.as_mut().expect("run before init");
        let stream = inner.stream_mut();
        let ret = match self.mode {
            // C++ inflate always uses Z_NO_FLUSH, even on close_input.
            GzipMode::Decode => unsafe { inflate(stream, Z_NO_FLUSH) },
            // deflate switches to Z_FINISH once close_input was called.
            GzipMode::Encode => unsafe {
                deflate(
                    stream,
                    if self.close_input_flag {
                        Z_FINISH
                    } else {
                        Z_NO_FLUSH
                    },
                )
            },
            GzipMode::Empty => unreachable!("inner exists only after init"),
        };
        if ret == Z_OK {
            Result::Ok(GzipState::Running)
        } else if ret == Z_STREAM_END {
            // TODO(now): fail if input is not empty (the C++ TODO).
            self.clear();
            Result::Ok(GzipState::Done)
        } else {
            self.clear();
            Result::Err(Status::error_message(&format!("zlib error {ret}")))
        }
    }

    fn init_common(&mut self) {
        self.input_size = 0;
        self.output_size = 0;
        self.close_input_flag = false;
    }

    fn clear(&mut self) {
        // `td::Gzip::clear` — inflateEnd/deflateEnd, then back to Empty. The
        // z_stream struct itself stays (the C++ `impl_` does too), so
        // `left_input`/`left_output` keep reporting the last avail counts.
        if let Some(inner) = self.inner.as_mut() {
            match (self.mode, inner) {
                (GzipMode::Decode, GzipInner::Decode(stream)) => unsafe {
                    inflateEnd(&mut **stream);
                },
                (GzipMode::Encode, GzipInner::Encode(stream)) => unsafe {
                    deflateEnd(&mut **stream);
                },
                _ => {}
            }
        }
        self.mode = GzipMode::Empty;
    }

    fn stream_mut(&mut self) -> &mut z_stream {
        self.inner
            .as_mut()
            .expect("stream access before init")
            .stream_mut()
    }
}

impl Default for Gzip {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Gzip {
    fn drop(&mut self) {
        self.clear();
    }
}

/// `td::gzdecode(s)` — decodes a zlib/gzip stream into a fresh buffer;
/// returns an empty slice on malformed input.
pub fn gzdecode(s: &[u8]) -> BufferSlice {
    let mut gzip = Gzip::new();
    gzip.init_decode().ensure();
    let mut message = ChainBufferWriter::new();
    gzip.set_input(s);
    gzip.close_input();
    let mut k = 2.0;
    gzip.set_output(message.prepare_append((s.len() as f64 * k) as usize));
    loop {
        let before_in = gzip.used_input();
        let before_out = gzip.used_output();
        let state = match gzip.run() {
            Result::Ok(state) => state,
            Result::Err(_) => return BufferSlice::null(),
        };
        if state == GzipState::Done {
            message.confirm_append(gzip.flush_output());
            break;
        }
        if gzip.need_output() {
            message.confirm_append(gzip.flush_output());
            k *= 1.5;
            gzip.set_output(message.prepare_append((gzip.left_input() as f64 * k) as usize));
        } else if gzip.need_input()
            && gzip.used_input() == before_in
            && gzip.used_output() == before_out
        {
            // Unlike zlib, miniz_oxide consumes the whole input into its
            // internal state while the output is full, so "input exhausted"
            // alone is not truncation. Only a round with no input left and
            // zero progress means the stream is truncated.
            return BufferSlice::null();
        }
    }
    message.extract_reader().move_as_buffer_slice()
}

/// `td::gzencode(s, max_compression_ratio)` — compresses into a fresh buffer
/// of at most `s.len() * max_compression_ratio` bytes; returns an empty slice
/// when the ratio is too small or the codec fails.
pub fn gzencode(s: &[u8], max_compression_ratio: f64) -> BufferSlice {
    let mut gzip = Gzip::new();
    gzip.init_encode().ensure();
    gzip.set_input(s);
    gzip.close_input();
    let max_size = (s.len() as f64 * max_compression_ratio) as usize;
    let mut message = BufferWriter::new(max_size);
    gzip.set_output(message.prepare_append());
    let state = match gzip.run() {
        Result::Ok(state) => state,
        Result::Err(_) => return BufferSlice::null(),
    };
    if state != GzipState::Done {
        return BufferSlice::null();
    }
    message.confirm_append(gzip.flush_output());
    message.as_buffer_slice()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::sha256;
    use crate::random::{Xorshift128plus, rand_string};

    fn hex(bytes: &[u8]) -> String {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut out = String::with_capacity(bytes.len() * 2);
        for &byte in bytes {
            out.push(DIGITS[(byte >> 4) as usize] as char);
            out.push(DIGITS[(byte & 0xf) as usize] as char);
        }
        out
    }

    fn encode_decode(s: &[u8]) {
        let r = gzencode(s, 2.0);
        assert!(!r.empty(), "encode of {} bytes", s.len());
        assert_eq!(s, gzdecode(r.as_slice()).as_slice());
    }

    #[test]
    fn gzencode_gzdecode_roundtrips() {
        let mut rng = Xorshift128plus::from_seed(1);
        let mut random = vec![0u8; 1000];
        rng.bytes(&mut random);
        encode_decode(&random);
        encode_decode(&rand_string(b'a' as i32, b'z' as i32, 1_000_000));
        encode_decode(&vec![b'a'; 1_000_000]);
    }

    /// Port of `TEST(Gzip, zlib)` (fb464979a) — both the zlib-format and
    /// the gzip-format encodings of "abacaba" decode through the
    /// auto-detecting reader.
    #[test]
    fn gzdecode_zlib_format() {
        fn hex_decode(hex: &str) -> Vec<u8> {
            (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                .collect()
        }
        assert_eq!(
            gzdecode(&hex_decode("78da4b4c4a4c4e4c4a04000ab302ac")).as_slice(),
            b"abacaba"
        );
        assert_eq!(
            gzdecode(&hex_decode(
                "1f8b080000000000020a4b4c4a4c4e4c4a040092c93b5c07000000"
            ))
            .as_slice(),
            b"abacaba"
        );
    }

    /// Port of `TEST(Gzip, gzencode)` — size sweep (1 → 10^7 bytes) across
    /// the three string families, each within the C++ ratio budget
    /// `max(2, 100 / len)` (integer division).
    #[test]
    fn gzencode_size_sweep() {
        for exponent in 0..=7u32 {
            let len = 10usize.pow(exponent);
            for &(from, to) in &[
                (b'a' as i32, b'a' as i32),
                (b'a' as i32, b'z' as i32),
                (0, 255),
            ] {
                let s = rand_string(from, to, len);
                let ratio = std::cmp::max(2, 100 / len) as f64;
                let r = gzencode(&s, ratio);
                assert!(!r.empty(), "encode of {len} bytes in [{from}, {to}]");
            }
        }
    }

    #[test]
    fn gzencode_compresses_and_uses_zlib_format() {
        let s = vec![b'a'; 100_000];
        let encoded = gzencode(&s, 2.0);
        assert!(encoded.size() < s.len() / 10);
        // deflateInit2 with windowBits 15 → zlib header (0x78 ...).
        assert_eq!(encoded.as_slice()[0], 0x78);
    }

    /// `inflateInit2(MAX_WBITS + 32)` also accepts gzip-format streams.
    /// Golden blob produced by system `gzip` (stock zlib), not by tdlib;
    /// a zlib-only decoder would reject it.
    #[test]
    fn gzip_format_stream_autodetect() {
        let blob: &[u8] = &[
            0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0x4b, 0xcf, 0xcf, 0x49,
            0x49, 0xcd, 0x53, 0x48, 0xaf, 0xca, 0x2c, 0x50, 0x28, 0x2e, 0x29, 0x4a, 0x4d, 0xcc,
            0x55, 0x48, 0xcb, 0x2f, 0x52, 0x28, 0x49, 0x29, 0x2d, 0xc9, 0xcc, 0x29, 0x56, 0x28,
            0x49, 0x2d, 0x2e, 0x29, 0xe6, 0x02, 0x00, 0x4e, 0x75, 0x34, 0xe4, 0x25, 0x00, 0x00,
            0x00,
        ];
        // gzip magic; the backend must not misread this as zlib.
        assert_eq!(&blob[..2], &[0x1f, 0x8b]);
        let decoded = gzdecode(blob);
        assert_eq!(
            decoded.as_slice(),
            b"golden gzip stream for tdutils tests\n"
        );
    }

    #[test]
    fn golden_vectors_from_the_conformance_corpus() {
        // The C++ test data blobs and their zlib decodings, pinned in S04.
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../conformance/vectors/gzip.bin"
        );
        let blob = std::fs::read(path).expect("conformance vectors present");
        let decoded = gzdecode(&blob);
        assert!(!decoded.empty());
        assert_eq!(decoded.size(), 22523);
        assert_eq!(
            hex(&sha256(decoded.as_slice())),
            "148f8c91977e6fca26252124b90d7d710b315fdc15f5721971abf99c6f827a3a"
        );

        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../conformance/vectors/gzip_bomb.bin"
        );
        let bomb = std::fs::read(path).expect("conformance vectors present");
        let decoded = gzdecode(&bomb);
        assert!(!decoded.empty());
        assert_eq!(decoded.size(), 49226);
        assert_eq!(
            hex(&sha256(decoded.as_slice())),
            "03ee2a341c7a3f6c69062931ea8e427ae34a43ad7c68675d12a5e8c50911af36"
        );
    }

    #[test]
    fn malformed_input_gives_empty() {
        assert!(gzdecode(b"not a zlib stream at all").empty());
        assert!(gzdecode(b"").empty());
        let encoded = gzencode(b"hello world", 2.0);
        let mut truncated = encoded.as_slice().to_vec();
        truncated.pop();
        assert!(gzdecode(&truncated).empty());
    }

    #[test]
    fn streaming_gzip_state_machine() {
        let mut gzip = Gzip::new();
        gzip.init_decode().ensure();
        let data = b"streaming test data streaming test data";
        let encoded = gzencode(data, 2.0);
        let encoded = encoded.as_slice();

        let mut out = vec![0u8; 64];
        gzip.set_input(&encoded[..10]);
        gzip.set_output(&mut out);
        let state = gzip.run().unwrap();
        assert_eq!(state, GzipState::Running);
        let produced = gzip.flush_output();
        let consumed = gzip.flush_input();
        assert_eq!(consumed, 10);
        assert_eq!(&out[..produced], &data[..produced]);

        gzip.set_input(&encoded[10..]);
        gzip.close_input();
        // The C++ contract: one set_output window, `run` resumes at the
        // zlib-advanced next_out — re-setting output requires the previous
        // window to be fully used (`output_size_ == 0`).
        let state = gzip.run().unwrap();
        assert_eq!(state, GzipState::Done);
        let produced2 = gzip.flush_output();
        assert_eq!(&out[..produced + produced2], &data[..]);
    }
}
