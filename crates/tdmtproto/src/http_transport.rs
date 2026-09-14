//! Port of `td/mtproto/HttpTransport.{h,cpp}` (S56): the HTTP transport —
//! every query is a POST to the proxy (`/api` or `HTTP://host:80/api`) whose
//! response body is the payload. The secret doubles as the proxy
//! `host|proxy_authorization` pair.

use crate::proxy_secret::ProxySecret;
use crate::transport::IStreamTransport;
use crate::transport_type::{TransportType, TransportTypeEnum};
use tdnet::http::{HttpHeaderCreator, HttpQuery, HttpQueryType, HttpReader};
use tdutils::buffer::{BufferSlice, BufferWriter, ChainBufferReader, ChainBufferWriter};
use tdutils::status::{Result, Status};

/// `td::mtproto::http::Transport`.
pub struct HttpTransport {
    secret: Vec<u8>,
    reader: HttpReader,
    http_query: HttpQuery,
    turn: HttpTurn,
    input: Option<ChainBufferReader>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HttpTurn {
    Write,
    Read,
}

impl HttpTransport {
    pub fn new(secret: Vec<u8>) -> Self {
        Self {
            secret,
            reader: HttpReader::new(),
            http_query: HttpQuery::default(),
            turn: HttpTurn::Write,
            input: None,
        }
    }
}

impl IStreamTransport for HttpTransport {
    fn read_next(
        &mut self,
        message: &mut BufferSlice,
        _quick_ack: &mut u32,
        error_code: &mut i32,
    ) -> Result<usize> {
        assert!(self.can_read());
        let input = self.input.as_mut().expect("init");
        input.sync_with_writer();
        let r_size = match self.reader.read_next(input, &mut self.http_query) {
            Result::Ok(size) => size,
            Result::Err(status) => return Result::Err(status),
        };
        if r_size != 0 {
            return Result::Ok(r_size);
        }
        if self.http_query.type_ != HttpQueryType::Response {
            return Result::Err(Status::error_message("Unexpected HTTP query type"));
        }
        if self.http_query.container.len() != 2 {
            return Result::Err(Status::error_message("Wrong response"));
        }
        *message = self.http_query.container[1].clone();
        *error_code = self.http_query.code;
        self.turn = HttpTurn::Write;
        Result::Ok(0)
    }

    fn support_quick_ack(&self) -> bool {
        false
    }

    fn write(
        &mut self,
        mut message: BufferWriter,
        quick_ack: bool,
        output: &mut ChainBufferWriter,
    ) {
        assert!(self.can_write());
        assert!(!quick_ack);
        /*
         * POST /api HTTP/1.1
         * Content-Length: [message->size()]
         * Host: url
         */
        let mut hc = HttpHeaderCreator::new();
        let (host, proxy_authorization) = match self.secret.iter().position(|&c| c == b'|') {
            Some(pos) => (&self.secret[..pos], &self.secret[pos + 1..]),
            None => (&self.secret[..], &[][..]),
        };
        if host.is_empty() {
            hc.init_post("/api");
            hc.add_header("Host", "");
            hc.set_keep_alive();
        } else {
            let url = format!("HTTP://{}:80/api", String::from_utf8_lossy(host));
            hc.init_post(&url);
            hc.add_header_bytes(b"Host", host);
            hc.add_header("User-Agent", "curl/7.35.0");
            hc.add_header("Accept", "*/*");
            hc.add_header("Proxy-Connection", "keep-alive");
            if !proxy_authorization.is_empty() {
                hc.add_header_bytes(b"Proxy-Authorization", proxy_authorization);
            }
        }
        hc.set_content_size(message.size());
        let head = hc.finish(&[]).unwrap().to_vec();
        {
            let prepend = message.prepare_prepend();
            let start = prepend.len() - head.len();
            prepend[start..].copy_from_slice(&head);
        }
        message.confirm_prepend(head.len());
        output.append_buffer_slice(message.as_buffer_slice());
        self.turn = HttpTurn::Read;
    }

    fn init(&mut self, input: ChainBufferReader, _output: &mut ChainBufferWriter) {
        self.input = Some(input);
    }

    fn can_read(&self) -> bool {
        self.turn == HttpTurn::Read
    }

    fn can_write(&self) -> bool {
        self.turn == HttpTurn::Write
    }

    fn max_prepend_size(&self) -> usize {
        if self.secret.is_empty() {
            96
        } else {
            self.secret.len().div_ceil(2) * 4 + 156
        }
    }

    fn max_append_size(&self) -> usize {
        0
    }

    fn get_type(&self) -> TransportType {
        TransportType::new(
            TransportTypeEnum::Http,
            0,
            ProxySecret::from_raw(self.secret.clone()),
        )
    }

    fn use_random_padding(&self) -> bool {
        false
    }
}
