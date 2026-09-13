//! Port of `td/utils/Gzip.h` + `Gzip.cpp` on `flate2` (replacing zlib).
//!
//! Despite the name, tdlib's streams are zlib-format: encode uses
//! `deflateInit2(..., 15, ...)` (zlib header) and decode uses
//! `inflateInit2(MAX_WBITS + 32)` (gzip/zlib auto-detect), exactly what
//! `flate2::Compress`/`Decompress` provide.

use crate::buffer::{BufferSlice, BufferWriter, ChainBufferWriter};
use crate::status::{Result, Status};

use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress};

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
    Encode(Compress),
    Decode(Decompress),
}

impl GzipInner {
    fn total_in(&self) -> u64 {
        match self {
            Self::Encode(compress) => compress.total_in(),
            Self::Decode(decompress) => decompress.total_in(),
        }
    }

    fn total_out(&self) -> u64 {
        match self {
            Self::Encode(compress) => compress.total_out(),
            Self::Decode(decompress) => decompress.total_out(),
        }
    }
}

/// `td::Gzip` — a streaming zlib codec over caller-provided buffers. The
/// buffers passed to [`Gzip::set_input`] / [`Gzip::set_output`] must stay
/// alive until their contents are flushed (the C++ pointer contract).
pub struct Gzip {
    inner: Option<GzipInner>,
    mode: GzipMode,
    input: *const u8,
    input_left: usize,
    input_total: usize,
    output: *mut u8,
    output_left: usize,
    output_total: usize,
    close_input_flag: bool,
}

impl Gzip {
    pub fn new() -> Self {
        Self {
            inner: None,
            mode: GzipMode::Empty,
            input: std::ptr::null(),
            input_left: 0,
            input_total: 0,
            output: std::ptr::null_mut(),
            output_left: 0,
            output_total: 0,
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

    /// `td::Gzip::init_encode` — zlib format, level 6.
    pub fn init_encode(&mut self) -> Status {
        assert!(self.mode == GzipMode::Empty, "init_encode on a live codec");
        self.init_common();
        self.mode = GzipMode::Encode;
        self.inner = Some(GzipInner::Encode(Compress::new(Compression::new(6), false)));
        Status::ok()
    }

    /// `td::Gzip::init_decode` — gzip/zlib auto-detect.
    pub fn init_decode(&mut self) -> Status {
        assert!(self.mode == GzipMode::Empty, "init_decode on a live codec");
        self.init_common();
        self.mode = GzipMode::Decode;
        self.inner = Some(GzipInner::Decode(Decompress::new(true)));
        Status::ok()
    }

    /// `td::Gzip::set_input` — the slice must outlive `run` calls until the
    /// data is consumed (tracked by [`Gzip::used_input`]).
    pub fn set_input(&mut self, input: &[u8]) {
        assert_eq!(self.input_left, 0, "set_input with unconsumed input");
        assert!(!self.close_input_flag, "set_input after close_input");
        self.input = input.as_ptr();
        self.input_left = input.len();
        self.input_total = input.len();
    }

    /// `td::Gzip::set_output` — the slice must outlive `run` calls until the
    /// data is flushed (tracked by [`Gzip::used_output`]).
    pub fn set_output(&mut self, output: &mut [u8]) {
        assert_eq!(self.output_left, 0, "set_output with unflushed output");
        self.output = output.as_mut_ptr();
        self.output_left = output.len();
        self.output_total = output.len();
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

    pub fn left_input(&self) -> usize {
        self.input_left
    }

    pub fn left_output(&self) -> usize {
        self.output_left
    }

    /// `td::Gzip::used_input`.
    pub fn used_input(&self) -> usize {
        self.input_total - self.left_input()
    }

    /// `td::Gzip::used_output`.
    pub fn used_output(&self) -> usize {
        self.output_total - self.left_output()
    }

    /// `td::Gzip::flush_input` — consumes the remaining input, returning the
    /// number of bytes used.
    pub fn flush_input(&mut self) -> usize {
        let res = self.used_input();
        self.input = std::ptr::null();
        self.input_left = 0;
        self.input_total = 0;
        res
    }

    /// `td::Gzip::flush_output` — consumes the remaining output, returning
    /// the number of bytes produced.
    pub fn flush_output(&mut self) -> usize {
        let res = self.used_output();
        self.output = std::ptr::null_mut();
        self.output_left = 0;
        self.output_total = 0;
        res
    }

    /// `td::Gzip::run()` — drives the codec; `Running` means more buffers
    /// may be needed, `Done` means the stream finished (the codec resets).
    pub fn run(&mut self) -> Result<GzipState> {
        let inner = self.inner.as_mut().expect("run before init");
        let before_in = inner.total_in();
        let before_out = inner.total_out();

        // SAFETY: the caller keeps the buffers set via set_input/set_output
        // alive until flushed (the C++ contract).
        let input = if self.input_left == 0 {
            &mut [][..]
        } else {
            unsafe { std::slice::from_raw_parts_mut(self.input as *mut u8, self.input_left) }
        };
        let output = if self.output_left == 0 {
            &mut [][..]
        } else {
            unsafe { std::slice::from_raw_parts_mut(self.output, self.output_left) }
        };

        let status = match self.inner.as_mut().expect("checked above") {
            GzipInner::Decode(decompress) => {
                // flate2's decompress is io-fallible; zlib surfaces corrupt
                // data as Z_DATA_ERROR (-3), matching the message below.
                match decompress.decompress(input, output, FlushDecompress::None) {
                    Ok(status) => status,
                    Err(_) => {
                        self.clear();
                        return Result::Err(Status::error_message("zlib error -3"));
                    }
                }
            }
            GzipInner::Encode(compress) => {
                let flush = if self.close_input_flag {
                    FlushCompress::Finish
                } else {
                    FlushCompress::None
                };
                // deflate stream errors surface as Z_STREAM_ERROR (-2).
                match compress.compress(input, output, flush) {
                    Ok(status) => status,
                    Err(_) => {
                        self.clear();
                        return Result::Err(Status::error_message("zlib error -2"));
                    }
                }
            }
        };

        let inner = self.inner.as_mut().expect("checked above");
        let consumed = (inner.total_in() - before_in) as usize;
        let produced = (inner.total_out() - before_out) as usize;
        // SAFETY: consumed/produced never exceed the set buffer sizes.
        self.input = unsafe { self.input.add(consumed) };
        self.input_left -= consumed;
        self.output = unsafe { self.output.add(produced) };
        self.output_left -= produced;

        match status {
            flate2::Status::Ok => Result::Ok(GzipState::Running),
            flate2::Status::StreamEnd => {
                self.clear();
                Result::Ok(GzipState::Done)
            }
            flate2::Status::BufError => {
                // zlib returns Z_BUF_ERROR (-5) when no progress is possible;
                // tdlib treats it as an error.
                self.clear();
                Result::Err(Status::error_message("zlib error -5"))
            }
        }
    }

    fn init_common(&mut self) {
        self.input = std::ptr::null();
        self.input_left = 0;
        self.input_total = 0;
        self.output = std::ptr::null_mut();
        self.output_left = 0;
        self.output_total = 0;
        self.close_input_flag = false;
    }

    fn clear(&mut self) {
        self.inner = None;
        self.mode = GzipMode::Empty;
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
        let state = match gzip.run() {
            Result::Ok(state) => state,
            Result::Err(_) => return BufferSlice::null(),
        };
        if state == GzipState::Done {
            message.confirm_append(gzip.flush_output());
            break;
        }
        if gzip.need_input() {
            return BufferSlice::null();
        }
        if gzip.need_output() {
            message.confirm_append(gzip.flush_output());
            k *= 1.5;
            gzip.set_output(message.prepare_append((gzip.left_input() as f64 * k) as usize));
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

    #[test]
    fn gzencode_compresses_and_uses_zlib_format() {
        let s = vec![b'a'; 100_000];
        let encoded = gzencode(&s, 2.0);
        assert!(encoded.size() < s.len() / 10);
        // deflateInit2 with windowBits 15 → zlib header (0x78 ...).
        assert_eq!(encoded.as_slice()[0], 0x78);
    }

    #[test]
    fn golden_vectors_from_the_conformance_corpus() {
        // The C++ test data blobs and their zlib decodings, pinned in S04.
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../conformance/vectors/gzip.bin"
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
            "/../conformance/vectors/gzip_bomb.bin"
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
        gzip.set_output(&mut out[produced..]);
        let state = gzip.run().unwrap();
        assert_eq!(state, GzipState::Done);
        let produced2 = gzip.flush_output();
        assert_eq!(&out[..produced + produced2], &data[..]);
    }
}
