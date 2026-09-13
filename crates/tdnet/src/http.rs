//! Port of the tdnet HTTP pieces the MTProto HTTP transport needs (S56):
//! `HttpQuery` (`td/net/HttpQuery.{h,cpp}`), `HttpHeaderCreator`
//! (`td/net/HttpHeaderCreator.{h,cpp}`), `HttpReader` (`td/net/HttpReader.cpp`
//! — the response-reading path) with the content-length and chunked content
//! decoders (`td/net/HttpContentLengthByteFlow.cpp`,
//! `td/net/HttpChunkedByteFlow.cpp`).
//!
//! Divergences: the C++ reader wires its input through `ByteFlowSource` +
//! flow nodes; the Rust reader owns the input per call, so the content
//! decoders are per-call state machines with the same acceptance semantics
//! and error texts. The server-only multipart/args/temp-file states are not
//! ported (S48's server reader will add them). Gzip `Content-Encoding` is
//! decoded in one buffer via `tdutils::gzip::gzdecode` instead of a
//! streaming flow (the MTProto HTTP transport never requests it).

use tdutils::buffer::{BufferSlice, ChainBufferReader, ChainBufferWriter};
use tdutils::status::{Result, Status};

/// `td::HttpQuery::Type`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum HttpQueryType {
    #[default]
    Empty,
    Get,
    Post,
    Response,
}

/// `td::HttpQuery` — the client-side subset (container, status line,
/// headers, content).
#[derive(Default)]
pub struct HttpQuery {
    pub container: Vec<BufferSlice>,
    pub type_: HttpQueryType,
    pub code: i32,
    pub reason: Vec<u8>,
    pub keep_alive: bool,
    pub headers: Vec<(Vec<u8>, Vec<u8>)>,
    pub content: Option<BufferSlice>,
}

impl HttpQuery {
    /// `HttpQuery::get_header`.
    pub fn get_header(&self, key: &[u8]) -> &[u8] {
        self.headers
            .iter()
            .find(|(name, _)| name == key)
            .map_or(&[], |(_, value)| value)
    }
}

/// `td::HttpHeaderCreator` — the request-header builder (4096-byte cap).
pub struct HttpHeaderCreator {
    header: Vec<u8>,
}

/// `HttpHeaderCreator::MAX_HEADER`.
pub const MAX_HEADER: usize = 4096;

impl HttpHeaderCreator {
    pub fn new() -> Self {
        Self {
            header: Vec::with_capacity(MAX_HEADER),
        }
    }

    /// `HttpHeaderCreator::init_post(url)`.
    pub fn init_post(&mut self, url: &str) {
        self.header.clear();
        self.header.extend_from_slice(b"POST ");
        self.header.extend_from_slice(url.as_bytes());
        self.header.extend_from_slice(b" HTTP/1.1\r\n");
    }

    /// `HttpHeaderCreator::init_get(url)`.
    pub fn init_get(&mut self, url: &str) {
        self.header.clear();
        self.header.extend_from_slice(b"GET ");
        self.header.extend_from_slice(url.as_bytes());
        self.header.extend_from_slice(b" HTTP/1.1\r\n");
    }

    /// `HttpHeaderCreator::add_header`.
    pub fn add_header(&mut self, key: &str, value: &str) {
        self.add_header_bytes(key.as_bytes(), value.as_bytes());
    }

    /// The byte form — td's `add_header(Slice, Slice)` (header values like
    /// the HTTP-transport secret host are not necessarily UTF-8).
    pub fn add_header_bytes(&mut self, key: &[u8], value: &[u8]) {
        self.header.extend_from_slice(key);
        self.header.extend_from_slice(b": ");
        self.header.extend_from_slice(value);
        self.header.extend_from_slice(b"\r\n");
    }

    /// `HttpHeaderCreator::set_content_size`.
    pub fn set_content_size(&mut self, size: usize) {
        self.add_header("Content-Length", &size.to_string());
    }

    /// `HttpHeaderCreator::set_keep_alive`.
    pub fn set_keep_alive(&mut self) {
        self.add_header("Connection", "keep-alive");
    }

    /// `HttpHeaderCreator::finish(content)` — the closing `\r\n` plus the
    /// optional body; `"Too many headers"` past the 4096-byte cap.
    pub fn finish(&mut self, content: &[u8]) -> Result<&[u8]> {
        self.header.extend_from_slice(b"\r\n");
        if !content.is_empty() {
            self.header.extend_from_slice(content);
        }
        if self.header.len() > MAX_HEADER {
            return Result::Err(Status::error_message("Too many headers"));
        }
        Result::Ok(&self.header)
    }
}

impl Default for HttpHeaderCreator {
    fn default() -> Self {
        Self::new()
    }
}

/// `td::HttpContentLengthByteFlow` — per-call form; returns `Ok(true)` once
/// the whole `len` bytes have been consumed.
pub struct HttpContentLengthByteFlow {
    len: usize,
}

impl HttpContentLengthByteFlow {
    /// `HttpContentLengthByteFlow::MIN_UPDATE_SIZE`.
    pub const MIN_UPDATE_SIZE: usize = 1 << 14;

    pub fn new(len: usize) -> Self {
        Self { len }
    }

    /// One pass; `Ok(0)` once the whole content has been consumed, otherwise
    /// the expected additional input size (the C++ `set_need_size` value).
    pub fn feed(&mut self, input: &mut ChainBufferReader, output: &mut ChainBufferWriter) -> Result<usize> {
        let mut ready_size = input.size();
        if ready_size > self.len {
            ready_size = self.len;
        }
        let need_size = Self::MIN_UPDATE_SIZE.min(self.len);
        if ready_size < need_size {
            // set_need_size(need_size); wait for more input
            return Result::Ok(need_size);
        }
        output.append_reader(input.cut_head_offset(ready_size));
        self.len -= ready_size;
        if self.len == 0 {
            return Result::Ok(0); // finish(Status::OK())
        }
        Result::Ok(Self::MIN_UPDATE_SIZE.min(self.len))
    }
}

/// `td::HttpChunkedByteFlow` — per-call form; returns `Ok(true)` once the
/// terminating zero-length chunk has been consumed.
pub struct HttpChunkedByteFlow {
    state: ChunkedState,
    len: usize,
    save_len: usize,
    total_size: usize,
    /// Search progress for the chunk-length line (the C++ `find_boundary`
    /// `already_read` in/out parameter).
    search_pos: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChunkedState {
    ReadChunkLength,
    ReadChunkContent,
    Done,
}

impl HttpChunkedByteFlow {
    /// `HttpChunkedByteFlow::MAX_SIZE` — some reasonable limit.
    pub const MAX_SIZE: usize = u32::MAX as usize;
    /// `HttpChunkedByteFlow::MIN_UPDATE_SIZE`.
    pub const MIN_UPDATE_SIZE: usize = 1 << 14;

    pub fn new() -> Self {
        Self {
            state: ChunkedState::ReadChunkLength,
            len: 0,
            save_len: 0,
            total_size: 0,
            search_pos: 0,
        }
    }

    /// One pass; `Ok(0)` once the terminating zero-length chunk has been
    /// consumed, otherwise the expected additional input size.
    pub fn feed(&mut self, input: &mut ChainBufferReader, output: &mut ChainBufferWriter) -> Result<usize> {
        if self.state == ChunkedState::ReadChunkLength {
            match find_boundary(input.clone(), b"\r\n", &mut self.search_pos) {
                Some(pos) => {
                    if pos > 8 {
                        let cut = input.cut_head_offset(pos).move_as_buffer_slice();
                        return Result::Err(Status::error_message(&format!(
                            "Too long length in chunked {:?}",
                            cut.as_slice()
                        )));
                    }
                    let s_len = input.cut_head_offset(pos).move_as_buffer_slice();
                    input.advance(2, &mut []);
                    self.len = hex_to_integer(s_len.as_slice());
                    self.save_len = self.len;
                    self.search_pos = 0;
                    self.state = ChunkedState::ReadChunkContent;
                }
                None => {
                    // set_need_size(input.size() + 1); wait for more input
                    return Result::Ok(input.size() + 1);
                }
            }
        }

        let size = input.size();
        let ready = self.len.min(size);
        let need_size = Self::MIN_UPDATE_SIZE.min(self.len) + 2;
        if size < need_size {
            // set_need_size(need_size)
            return Result::Ok(need_size);
        }
        if self.total_size > Self::MAX_SIZE - ready {
            return Result::Err(Status::error_message(&format!(
                "Too big query [size = {}]",
                input.size()
            )));
        }
        self.total_size += ready;

        output.append_reader(input.cut_head_offset(ready));
        self.len -= ready;

        if self.len == 0 {
            if input.size() < 2 {
                // set_need_size(2)
                return Result::Ok(2);
            }
            input.advance(2, &mut []);
            self.total_size += 2;
            if self.save_len == 0 {
                self.state = ChunkedState::Done;
                return Result::Ok(0); // finish(Status::OK())
            }
            self.state = ChunkedState::ReadChunkLength;
        }
        Result::Ok(Self::MIN_UPDATE_SIZE.min(self.len) + 2)
    }
}

impl Default for HttpChunkedByteFlow {
    fn default() -> Self {
        Self::new()
    }
}

/// `td::find_boundary(range, boundary, already_read)`: scans `range` (owned,
/// the caller passes a clone) from `already_read` for `boundary`; on success
/// `already_read` is the boundary's start offset.
pub fn find_boundary(mut range: ChainBufferReader, boundary: &[u8], already_read: &mut usize) -> Option<usize> {
    range.advance(*already_read, &mut []);
    assert!(boundary.len() <= 74, "boundary too long");

    while !range.empty() {
        let ready = range.prepare_read();
        if ready[0] == boundary[0] {
            if range.size() < boundary.len() {
                return None;
            }
            let save_range = range.clone();
            let mut x = vec![0u8; boundary.len()];
            range.advance(boundary.len(), &mut x);
            if x == boundary {
                return Some(*already_read);
            }
            // not a boundary, restoring previous state and skip one symbol
            range = save_range;
            range.advance(1, &mut []);
            *already_read += 1;
        } else {
            let shift = ready
                .iter()
                .position(|&c| c == boundary[0])
                .unwrap_or(ready.len());
            *already_read += shift;
            range.advance(shift, &mut []);
        }
    }
    None
}

/// `td::hex_to_integer<size_t>`: parses leading hex digits, stops at the
/// first non-hex digit.
fn hex_to_integer(str: &[u8]) -> usize {
    let mut value = 0usize;
    for &c in str {
        match c {
            b'0'..=b'9' => value = value * 16 + (c - b'0') as usize,
            b'a'..=b'f' => value = value * 16 + (c - b'a' + 10) as usize,
            b'A'..=b'F' => value = value * 16 + (c - b'A' + 10) as usize,
            _ => break,
        }
    }
    value
}

/// A minimal `td::Parser`: a cursor over one header buffer with a sticky
/// error status (the C++ `Parser::status()` semantics).
struct Parser<'a> {
    data: &'a [u8],
    pos: usize,
    status: Status,
}

impl<'a> Parser<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            status: Status::ok(),
        }
    }

    fn status(&self) -> &Status {
        &self.status
    }

    fn is_ok(&self) -> bool {
        self.status.is_ok()
    }

    fn empty(&self) -> bool {
        self.pos >= self.data.len()
    }

    /// `Parser::read_till(delim)` — the slice up to (not including) the
    /// delimiter; sets the sticky error when no delimiter remains.
    fn read_till(&mut self, delim: u8) -> &'a [u8] {
        let start = self.pos;
        while self.pos < self.data.len() && self.data[self.pos] != delim {
            self.pos += 1;
        }
        if self.pos >= self.data.len() {
            self.status = Status::error(400, "Bad Request");
        }
        &self.data[start..self.pos]
    }

    fn skip(&mut self, delim: u8) {
        if self.pos < self.data.len() && self.data[self.pos] == delim {
            self.pos += 1;
        }
    }

    fn peek_char(&self) -> Option<u8> {
        self.data.get(self.pos).copied()
    }
}

/// `td::HttpReader` — the response-reading states.
pub struct HttpReader {
    state: ReaderState,
    headers_read_length: usize,
    content_length: i64,
    transfer_encoding: Vec<u8>,
    content_encoding: Vec<u8>,
    content_length_flow: HttpContentLengthByteFlow,
    chunked_flow: HttpChunkedByteFlow,
    /// Buffer the content flows append to before the query takes it.
    content: ChainBufferWriter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReaderState {
    ReadHeaders,
    ReadContent,
    ReadContentChunked,
}

/// `HttpReader::MAX_CONTENT_SIZE` / `MAX_TOTAL_HEADERS_LENGTH`.
const MAX_CONTENT_SIZE: usize = u32::MAX as usize;
const MAX_TOTAL_HEADERS_LENGTH: usize = 1 << 18;

impl HttpReader {
    pub fn new() -> Self {
        Self {
            state: ReaderState::ReadHeaders,
            headers_read_length: 0,
            content_length: -1,
            transfer_encoding: Vec::new(),
            content_encoding: Vec::new(),
            content_length_flow: HttpContentLengthByteFlow::new(0),
            chunked_flow: HttpChunkedByteFlow::new(),
            content: ChainBufferWriter::new(),
        }
    }

    /// `HttpReader::read_next(query)`: 0 when a complete query was parsed,
    /// otherwise the expected additional input size.
    pub fn read_next(&mut self, input: &mut ChainBufferReader, query: &mut HttpQuery) -> Result<usize> {
        loop {
            match self.state {
                ReaderState::ReadHeaders => {
                    query.container.clear();
                    match find_boundary(input.clone(), b"\r\n\r\n", &mut self.headers_read_length) {
                        Some(_) => {
                            let mut a = input.cut_head_offset(self.headers_read_length + 2);
                            query.container.push(a.move_as_buffer_slice());
                            input.advance(2, &mut []);
                            let head = query.container[0].clone();
                            let mut status = self.parse_head(&head, query);
                            if status.is_error() {
                                return Result::Err(status.move_as_error());
                            }
                            self.headers_read_length = 0;

                            if self.transfer_encoding.is_empty() && self.content_length <= 0 {
                                return Result::Ok(0);
                            }
                            if self.content_length >= MAX_CONTENT_SIZE as i64 {
                                return Result::Err(Status::error(
                                    413,
                                    &format!(
                                        "Request Entity Too Large: content length is {}",
                                        self.content_length
                                    ),
                                ));
                            }
                            if self.transfer_encoding.is_empty() {
                                self.content_length_flow =
                                    HttpContentLengthByteFlow::new(self.content_length as usize);
                                self.state = ReaderState::ReadContent;
                            } else if self.transfer_encoding == b"chunked" {
                                self.chunked_flow = HttpChunkedByteFlow::new();
                                self.state = ReaderState::ReadContentChunked;
                            } else {
                                return Result::Err(Status::error(
                                    501,
                                    "Unimplemented: unsupported transfer-encoding",
                                ));
                            }
                            continue;
                        }
                        None => {
                            if input.size() > MAX_TOTAL_HEADERS_LENGTH {
                                return Result::Err(Status::error(
                                    431,
                                    "Request Header Fields Too Large: total headers size exceeded",
                                ));
                            }
                            return Result::Ok(input.size() + 1);
                        }
                    }
                }
                ReaderState::ReadContent => {
                    let need = match self.content_length_flow.feed(input, &mut self.content) {
                        Result::Ok(need) => need,
                        Result::Err(status) => return Result::Err(status),
                    };
                    if need != 0 {
                        return Result::Ok(need);
                    }
                    self.state = ReaderState::ReadHeaders;
                    query
                        .container
                        .push(self.content.extract_reader().move_as_buffer_slice());
                    self.content = ChainBufferWriter::new();
                    return Result::Ok(0);
                }
                ReaderState::ReadContentChunked => {
                    let need = match self.chunked_flow.feed(input, &mut self.content) {
                        Result::Ok(need) => need,
                        Result::Err(status) => return Result::Err(status),
                    };
                    if need != 0 {
                        return Result::Ok(need);
                    }
                    self.state = ReaderState::ReadHeaders;
                    query
                        .container
                        .push(self.content.extract_reader().move_as_buffer_slice());
                    self.content = ChainBufferWriter::new();
                    return Result::Ok(0);
                }
            }
        }
    }

    /// `HttpReader::split_header` + `parse_head`: the head slice is in
    /// `query.container[0]`.
    fn parse_head(&mut self, head: &BufferSlice, query: &mut HttpQuery) -> Result<()> {
        let mut parser = Parser::new(head.as_slice());

        let type_slice = parser.read_till(b' ');
        parser.skip(b' ');
        if type_slice == b"GET" {
            query.type_ = HttpQueryType::Get;
        } else if type_slice == b"POST" {
            query.type_ = HttpQueryType::Post;
        } else if type_slice.len() >= 4 && &type_slice[..4] == b"HTTP" {
            let status = Self::parse_http_version(type_slice, query);
            if status.is_error() {
                return status;
            }
            query.type_ = HttpQueryType::Response;
        } else {
            return Result::Err(Status::error(501, "Not Implemented"));
        }

        if query.type_ == HttpQueryType::Response {
            let code = parser.read_till(b' ');
            parser.skip(b' ');
            query.code = lenient_to_integer(code) as i32;
            query.reason = parser.read_till(b'\r').to_vec();
        } else {
            let url_version = parser.read_till(b'\r');
            let space_pos = url_version.iter().rposition(|&c| c == b' ');
            let Some(space_pos) = space_pos else {
                return Result::Err(Status::error(400, "Bad Request: wrong request line"));
            };
            let status = Self::parse_http_version(&url_version[space_pos + 1..], query);
            if status.is_error() {
                return status;
            }
        }
        parser.skip(b'\r');
        parser.skip(b'\n');

        self.content_length = -1;
        self.transfer_encoding.clear();
        self.content_encoding.clear();
        query.headers.clear();
        query.content = None;

        while parser.is_ok() && !parser.empty() {
            let header_name = parser.read_till(b':');
            parser.skip(b':');
            let value_begin = parser.pos;
            // A header value may continue on lines starting with a space or
            // tab (the C++ do/while over read_till('\r') + skip).
            let mut value_end: usize;
            loop {
                parser.read_till(b'\r');
                value_end = parser.pos;
                parser.skip(b'\r');
                parser.skip(b'\n');
                if parser.peek_char() == Some(b' ') || parser.peek_char() == Some(b'\t') {
                    continue;
                }
                break;
            }
            let status = self.process_header(
                trim(header_name),
                trim(&head.as_slice()[value_begin..value_end]),
                query,
            );
            if status.is_error() {
                return status;
            }
        }
        if parser.is_ok() {
            Result::Ok(())
        } else {
            Result::Err(parser.status().clone())
        }
    }

    fn parse_http_version(version: &[u8], query: &mut HttpQuery) -> Result<()> {
        if version == b"HTTP/1.1" {
            query.keep_alive = true;
        } else if version == b"HTTP/1.0" {
            query.keep_alive = false;
        } else {
            return Result::Err(Status::error(505, "HTTP Version Not Supported"));
        }
        Result::Ok(())
    }

    /// `HttpReader::process_header` — the client-side subset.
    fn process_header(&mut self, header_name: &[u8], header_value: &[u8], query: &mut HttpQuery) -> Result<()> {
        let header_name = to_lower(header_name);
        let header_value = to_lower(header_value);
        query
            .headers
            .push((header_name.clone(), header_value.clone()));
        if header_name == b"content-length" {
            let content_length = lenient_to_integer(&header_value) as i64;
            if content_length > MAX_CONTENT_SIZE as i64 {
                return Result::Err(Status::error(400, "Bad Request: Content-Length is too big"));
            }
            if self.content_length >= 0 && self.content_length != content_length {
                return Result::Err(Status::error(
                    400,
                    "Bad Request: conflicting duplicate Content-Length headers",
                ));
            }
            self.content_length = content_length;
        } else if header_name == b"connection" {
            query.keep_alive = header_value != b"close";
        } else if header_name == b"content-encoding" {
            self.content_encoding = header_value;
        } else if header_name == b"transfer-encoding" {
            self.transfer_encoding = header_value;
        }
        Result::Ok(())
    }
}

impl Default for HttpReader {
    fn default() -> Self {
        Self::new()
    }
}

/// `td::trim`.
fn trim(data: &[u8]) -> &[u8] {
    let mut begin = 0;
    let mut end = data.len();
    while begin < end && matches!(data[begin], b' ' | b'\t' | b'\r' | b'\n') {
        begin += 1;
    }
    while end > begin && matches!(data[end - 1], b' ' | b'\t' | b'\r' | b'\n') {
        end -= 1;
    }
    &data[begin..end]
}

/// `td::to_lower_inplace`.
fn to_lower(data: &[u8]) -> Vec<u8> {
    data.iter()
        .map(|&c| if c.is_ascii_uppercase() { c + 32 } else { c })
        .collect()
}

/// `td::to_integer<T>` — the lenient prefix parse (0 when nothing parses).
fn lenient_to_integer(data: &[u8]) -> i64 {
    let mut value: i64 = 0;
    let mut iter = data.iter().peekable();
    let mut sign = 1i64;
    if let Some(&&c) = iter.peek() {
        if c == b'-' {
            sign = -1;
            iter.next();
        } else if c == b'+' {
            iter.next();
        }
    }
    for &c in iter {
        if !c.is_ascii_digit() {
            break;
        }
        value = value.saturating_mul(10).saturating_add((c - b'0') as i64);
    }
    sign * value
}
