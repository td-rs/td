//! The `td::store`/`td::parse` primitives of `tdutils/td/utils/tl_helpers.h`
//! plus the log-event framing of `td/telegram/logevent/LogEvent.h`: the
//! byte format every manager object persisted through a pmc uses (options,
//! users, secret chats, dialogs...).
//!
//! Layout rules mirrored here:
//! - integers/doubles are raw little-endian (`store_binary`);
//! - bools are a full `int32`;
//! - strings use the TL framing of `TlStorerUnsafe::store_string` —
//!   1-byte length below 254, `0xfe`-prefixed 24-bit / `0xff`-prefixed
//!   32-bit forms above, zero padding to a 4-byte boundary (pinned
//!   against the compiled C++ in the tests below);
//! - flag blocks are one `uint32` per ≤31 booleans, first flag = bit 0;
//!   `END_PARSE_FLAGS` rejects any unused upper bits;
//! - every pmc log-event blob is prefixed with the writer's `Version` —
//!   `LogEventStorerUnsafe` stores `Version::Next - 1` and the parser
//!   `LOG_CHECK`s the value below `Version::Next`.

/// `td/telegram/Version.h` — the entries the ported formats branch on.
/// Keep in sync with the C++ enum; `Version::Next` is the one-past value.
pub mod version {
    pub const ADD_KEY_HASH_TO_SECRET_CHAT: i32 = 2;
    pub const ADD_ACCESS_HASH_TO_SECRET_CHAT: i32 = 5;
    pub const FIX_MIN_USERS: i32 = 10;
    pub const SUPPORT_BANNED_CHANNELS: i32 = 31;
    pub const SUPPORT_64_BIT_IDS: i32 = 33;
    pub const MAKE_PARTICIPANT_FLAGS_64_BIT: i32 = 46;
    pub const ADD_USER_FLAGS2: i32 = 48;
    pub const ADD_CONTACT_VCARD: i32 = 16;
    pub const SUPPORT_EPHEMERAL_MESSAGES: i32 = 61;
    /// `Version::Next` of tdlib 1.8.67 (`SupportEphemeralMessages` + 1).
    pub const NEXT: i32 = 62;
    /// What `LogEventStorerUnsafe` writes (`Version::Next - 1`).
    pub const LOG_EVENT_STORE_VERSION: i32 = NEXT - 1;
}

pub fn store_i32(out: &mut Vec<u8>, value: i32) {
    out.extend_from_slice(&value.to_le_bytes());
}

pub fn store_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

pub fn store_i64(out: &mut Vec<u8>, value: i64) {
    out.extend_from_slice(&value.to_le_bytes());
}

pub fn store_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

pub fn store_f64(out: &mut Vec<u8>, value: f64) {
    out.extend_from_slice(&value.to_le_bytes());
}

pub fn store_bool(out: &mut Vec<u8>, value: bool) {
    store_i32(out, value as i32);
}

/// `TlStorerUnsafe::store_string` — the TL string framing `td::store(string)`
/// routes through: a 1-byte length below 254, then `0xfe`-prefixed 24-bit
/// and `0xff`-prefixed 32-bit forms, always zero-padded to a 4-byte
/// boundary (verified against the C++ with the /tmp string probe).
pub fn store_string(out: &mut Vec<u8>, value: &str) {
    let bytes = value.as_bytes();
    let len = bytes.len();
    let header = if len < 254 {
        out.push(len as u8);
        1
    } else if len < (1 << 24) {
        out.push(0xfe);
        out.extend_from_slice(&(len as u32).to_le_bytes()[..3]);
        4
    } else if len < (1usize << 32) {
        out.push(0xff);
        out.extend_from_slice(&(len as u64).to_le_bytes()[..4]);
        out.extend_from_slice(&[0, 0, 0]);
        8
    } else {
        panic!("String size {len} is too big to be stored");
    };
    out.extend_from_slice(bytes);
    let pad = (4 - (header + len) % 4) % 4;
    out.extend(std::iter::repeat_n(0u8, pad));
}

/// `td::store(vector<string>)`.
pub fn store_string_vec(out: &mut Vec<u8>, values: &[String]) {
    store_i32(out, values.len() as i32);
    for value in values {
        store_string(out, value);
    }
}

/// `td::store(vector<int32>)`.
pub fn store_i32_vec(out: &mut Vec<u8>, values: &[i32]) {
    store_i32(out, values.len() as i32);
    for value in values {
        store_i32(out, *value);
    }
}

/// `BEGIN_STORE_FLAGS`/`STORE_FLAG`/`END_STORE_FLAGS` — one little-endian
/// `uint32` per flag block; the C++ CHECKs `bit_offset < 31`.
pub fn store_flags<const N: usize>(out: &mut Vec<u8>, flags: [bool; N]) {
    assert!(N < 31, "the C++ END_STORE_FLAGS CHECKs bit_offset < 31");
    let mut bits: u32 = 0;
    for (offset, flag) in flags.iter().enumerate() {
        bits |= (*flag as u32) << offset;
    }
    store_u32(out, bits);
}

/// `logevent::LogEventStorerUnsafe` framing: the version prefix.
pub fn store_log_event_version(out: &mut Vec<u8>) {
    store_i32(out, version::LOG_EVENT_STORE_VERSION);
}

/// `logevent::LogEventParser` — the version-prefixed reader.
pub struct Parser<'a> {
    data: &'a [u8],
    pos: usize,
    version: i32,
}

impl<'a> Parser<'a> {
    /// A reader over a bare value (no version prefix); used by formats
    /// stored without the log-event framing.
    pub fn new(data: &'a [u8]) -> Parser<'a> {
        Parser {
            data,
            pos: 0,
            version: version::NEXT,
        }
    }

    /// `log_event_parse` entry: fetch and CHECK the version prefix.
    pub fn new_log_event(data: &'a [u8]) -> Result<Parser<'a>, &'static str> {
        let mut parser = Parser {
            data,
            pos: 0,
            version: 0,
        };
        parser.version = parser.i32_()?;
        if parser.version >= version::NEXT {
            return Err("Wrong version");
        }
        Ok(parser)
    }

    pub fn version(&self) -> i32 {
        self.version
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], &'static str> {
        if self.data.len() < self.pos + len {
            return Err("Failed to read fixed-size data");
        }
        let slice = &self.data[self.pos..self.pos + len];
        self.pos += len;
        Ok(slice)
    }

    pub fn i32_(&mut self) -> Result<i32, &'static str> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn u32_(&mut self) -> Result<u32, &'static str> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn i64_(&mut self) -> Result<i64, &'static str> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub fn u64_(&mut self) -> Result<u64, &'static str> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub fn f64_(&mut self) -> Result<f64, &'static str> {
        Ok(f64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub fn bool(&mut self) -> Result<bool, &'static str> {
        Ok(self.i32_()? != 0)
    }

    /// `TlParser::fetch_string` (tl_parsers.h:127) — the raw TL string
    /// reader of the `LogEventParser` (no NUL/UTF-8 post-processing; the
    /// Rust `String` still rejects invalid UTF-8, a documented divergence
    /// on corrupted blobs).
    pub fn string(&mut self) -> Result<String, &'static str> {
        let bytes = self.tl_string_bytes()?;
        String::from_utf8(bytes.to_vec()).map_err(|_| "Invalid UTF-8 string")
    }

    fn tl_string_bytes(&mut self) -> Result<&'a [u8], &'static str> {
        if self.data.len() < self.pos + 4 {
            return Err("Failed to read string length");
        }
        let first = self.data[self.pos];
        let (result_len, header, aligned_len) = if first < 254 {
            let len = first as usize;
            (len, 4usize, (len >> 2) << 2)
        } else if first == 254 {
            if self.data.len() < self.pos + 4 {
                return Err("Failed to read string length");
            }
            let len = (self.data[self.pos + 1] as usize)
                | ((self.data[self.pos + 2] as usize) << 8)
                | ((self.data[self.pos + 3] as usize) << 16);
            (len, 4, ((len + 3) >> 2) << 2)
        } else {
            if self.data.len() < self.pos + 8 {
                return Err("Failed to read string length");
            }
            let mut len = 0u64;
            for i in (1..8).rev() {
                len = (len << 8) | self.data[self.pos + i] as u64;
            }
            if len > (usize::MAX - 3) as u64 {
                return Err("Too big string found");
            }
            (len as usize, 8, ((len as usize + 3) >> 2) << 2)
        };
        if self.data.len() < self.pos + header + aligned_len {
            return Err("Failed to read string data");
        }
        let begin = self.pos
            + if first == 255 {
                8
            } else if first == 254 {
                4
            } else {
                1
            };
        let bytes = &self.data[begin..begin + result_len];
        self.pos += header + aligned_len;
        Ok(bytes)
    }

    /// `TlParser::get_left_len` — the bytes left in the input. The
    /// vector readers gate counts against it before allocating, like the
    /// C++ `parse(vector<T>&)` (`tl_helpers.h:170`).
    pub fn left_len(&self) -> usize {
        self.data.len() - self.pos
    }

    pub fn string_vec(&mut self) -> Result<Vec<String>, &'static str> {
        let len = self.i32_()?;
        // The C++ reads the size as uint32 and rejects anything above
        // the input left (negatives wrap huge and fail the same gate);
        // the length gate must run BEFORE the capacity allocation or a
        // hostile `us`/`usf`/`sc` row can abort the process on start.
        if len < 0 || self.left_len() < len as usize {
            return Err("Wrong vector length");
        }
        let mut values = Vec::with_capacity(len as usize);
        for _ in 0..len {
            values.push(self.string()?);
        }
        Ok(values)
    }

    pub fn i32_vec(&mut self) -> Result<Vec<i32>, &'static str> {
        let len = self.i32_()?;
        if len < 0 || self.left_len() < len as usize {
            return Err("Wrong vector length");
        }
        let mut values = Vec::with_capacity(len as usize);
        for _ in 0..len {
            values.push(self.i32_()?);
        }
        Ok(values)
    }

    /// `BEGIN_PARSE_FLAGS`/`PARSE_FLAG`/`END_PARSE_FLAGS`.
    pub fn parse_flags<const N: usize>(&mut self) -> Result<[bool; N], &'static str> {
        assert!(N < 31, "the C++ END_PARSE_FLAGS CHECKs bit_offset < 31");
        let bits = self.u32_()?;
        if bits & !((1u32 << N) - 1) != 0 {
            return Err("Invalid flags left");
        }
        let mut flags = [false; N];
        for (offset, flag) in flags.iter_mut().enumerate() {
            *flag = (bits >> offset) & 1 != 0;
        }
        Ok(flags)
    }

    /// `ParserT::fetch_end` — no trailing bytes may remain.
    pub fn fetch_end(&self) -> Result<(), &'static str> {
        if self.pos != self.data.len() {
            return Err("Extra data in end of parsed string");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The TL string framing, pinned against the compiled C++
    /// (`td::store(std::string)` through `TlStorerUnsafe`/`
    /// TlStorerCalcLength`, conformance probe 2026-09-16):
    /// "auth" -> `04 61 75 74 68 00 00 00`, "ok" -> `02 6f 6b 00`,
    /// "A" -> `01 41 00 00`.
    #[test]
    fn string_framing_matches_the_cpp_probe() {
        let mut out = Vec::new();
        store_string(&mut out, "auth");
        assert_eq!(out, [0x04, b'a', b'u', b't', b'h', 0, 0, 0]);
        let mut out = Vec::new();
        store_string(&mut out, "ok");
        assert_eq!(out, [0x02, b'o', b'k', 0]);
        let mut out = Vec::new();
        store_string(&mut out, "A");
        assert_eq!(out, [0x01, b'A', 0, 0]);

        // The 0xfe form kicks in at 254 bytes (length little-endian u24).
        let long = "x".repeat(300);
        let mut out = Vec::new();
        store_string(&mut out, &long);
        // 300 = 0x00012c, little-endian u24 after the 0xfe marker.
        assert_eq!(&out[..4], &[0xfeu8, 0x2c, 0x01, 0x00]);
        assert_eq!(out.len() % 4, 0, "total must stay 4-byte aligned");
        let mut parser = Parser {
            data: &out,
            pos: 0,
            version: version::NEXT,
        };
        assert_eq!(parser.string().unwrap(), long);
        parser.fetch_end().unwrap();
    }

    #[test]
    fn primitives_round_trip() {
        let mut out = Vec::new();
        store_i32(&mut out, -7);
        store_i64(&mut out, 1 << 40);
        store_f64(&mut out, 2.5);
        store_bool(&mut out, true);
        store_string(&mut out, "héllo");
        store_string_vec(&mut out, vec!["a".to_owned(), "b".to_owned()].as_slice());
        store_i32_vec(&mut out, &[1, 2, 3]);

        let mut parser = Parser {
            data: &out,
            pos: 0,
            version: version::NEXT,
        };
        assert_eq!(parser.i32_().unwrap(), -7);
        assert_eq!(parser.i64_().unwrap(), 1 << 40);
        assert_eq!(parser.f64_().unwrap(), 2.5);
        assert!(parser.bool().unwrap());
        assert_eq!(parser.string().unwrap(), "héllo");
        assert_eq!(parser.string_vec().unwrap(), ["a", "b"]);
        assert_eq!(parser.i32_vec().unwrap(), [1, 2, 3]);
        parser.fetch_end().unwrap();
    }

    #[test]
    fn vector_counts_gate_against_the_input_left() {
        // `parse(vector<T>&)` (tl_helpers.h:170) rejects a count above
        // the input left BEFORE allocating — a hostile `us`/`usf`/`sc`
        // row must not be able to abort the process on start.
        let mut out = Vec::new();
        store_i32(&mut out, 1 << 30); // a billion elements claimed
        store_i32(&mut out, 0); // one element's worth of input
        let mut parser = Parser {
            data: &out,
            pos: 0,
            version: version::NEXT,
        };
        assert_eq!(parser.string_vec(), Err("Wrong vector length"));
        let mut parser = Parser {
            data: &out,
            pos: 0,
            version: version::NEXT,
        };
        assert_eq!(parser.i32_vec(), Err("Wrong vector length"));
        // Negative counts keep the pre-existing rejection (the C++
        // uint32 wrap fails the same gate).
        store_i32(&mut out, -1);
        let mut parser = Parser {
            data: &out,
            pos: 0,
            version: version::NEXT,
        };
        assert_eq!(parser.i32_vec(), Err("Wrong vector length"));
    }

    #[test]
    fn flag_blocks_reject_unused_bits() {
        let mut out = Vec::new();
        store_flags(&mut out, [true, false, true]);
        let mut parser = Parser {
            data: &out,
            pos: 0,
            version: version::NEXT,
        };
        let flags = parser.parse_flags::<3>().unwrap();
        assert_eq!(flags, [true, false, true]);

        // A C++ writer of a newer layout sets bit 3 — reject like
        // END_PARSE_FLAGS.
        let mut out = Vec::new();
        store_u32(&mut out, 0b1001);
        let mut parser = Parser {
            data: &out,
            pos: 0,
            version: version::NEXT,
        };
        assert!(parser.parse_flags::<3>().is_err());
    }

    #[test]
    fn log_event_framing_checks_the_version() {
        let mut out = Vec::new();
        store_log_event_version(&mut out);
        store_string(&mut out, "payload");
        let mut parser = Parser::new_log_event(&out).unwrap();
        assert_eq!(parser.version(), version::LOG_EVENT_STORE_VERSION);
        assert_eq!(parser.string().unwrap(), "payload");
        parser.fetch_end().unwrap();

        // A version at or beyond Version::Next is rejected.
        let mut out = Vec::new();
        store_i32(&mut out, version::NEXT);
        assert!(Parser::new_log_event(&out).is_err());
    }

    #[test]
    fn truncation_and_negative_lengths_error() {
        let mut out = Vec::new();
        store_i32(&mut out, -1);
        let mut parser = Parser {
            data: &out,
            pos: 0,
            version: version::NEXT,
        };
        assert!(parser.string().is_err());

        let mut parser = Parser {
            data: &out,
            pos: 0,
            version: version::NEXT,
        };
        assert!(parser.i64_().is_err());
    }
}
