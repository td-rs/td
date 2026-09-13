//! Port of `td/mtproto/TlsInit.{h,cpp}` — the MTProxy fake-TLS ClientHello
//! — on tokio.
//!
//! `TlsObfusaction::generate_header` is ported verbatim (ops table, scope
//! back-patching, the X25519-hardened `key` generation over BigNum, the
//! ML-KEM-768 packed coefficients, padding, permutation shuffling and the
//! `hmac_sha256(secret, data)` time-stamped finish); the `TlsInit` actor
//! becomes [`connect`], an async function with the exact
//! `wait_hello_response` parser and error strings.
//!
//! Documented deviations from the C++ actor:
//! * Callback/Promise → `Result<TcpStream>`; dropping the future is the
//!   C++ `hangup()` → "Canceled" path; the inherited 10-second
//!   `TransparentProxy` timeout is kept ("Connection timeout expired").
//! * The C++ `TD_DARWIN` hello table variant is not ported; the Linux
//!   table is used on every platform.
//! * The C++ hello table is built once per process, baking the
//!   `ech_payload` length (and, on Darwin, the `random_value` choice) at
//!   first use — the quirk is preserved via a process-wide `LazyLock`.
//! * Lives in `tdnet` for now (the plan's Track F placement); migrates to
//!   `tdmtproto` with the rest of `td/mtproto` in Track G.

use std::sync::LazyLock;

use tokio::net::TcpStream;

use tdutils::bignum::{BigNum, BigNumContext};
use tdutils::crypto::hmac_sha256;
use tdutils::random;
use tdutils::status::{Result, Status};
use tdutils::time;

use crate::transparent::{read_some, reject_early_data, with_timeout, write_all};

/// C++ `ProxySecret::MAX_DOMAIN_LENGTH` — must be small enough to not
/// overflow the TLS-hello length.
pub const MAX_DOMAIN_LENGTH: usize = 182;

fn init_grease(res: &mut [u8]) {
    random::secure_bytes(res);
    for c in res.iter_mut() {
        *c = (*c & 0xF0) + 0x0A;
    }
    for i in (1..res.len()).step_by(2) {
        if res[i] == res[i - 1] {
            res[i] ^= 0x10;
        }
    }
}

/// One operation of the hello description (`TlsHello::Op`). `RandomValue`
/// is not ported: it only appears in the C++ `TD_DARWIN` table.
#[derive(Clone)]
enum Op {
    Str(&'static [u8]),
    Random(usize),
    Zero(usize),
    Domain,
    Grease(usize),
    Key,
    MlKem768Key,
    BeginScope,
    EndScope,
    Permutation(Vec<Vec<Op>>),
    /// `Op::ech_payload` with the length resolved at table construction.
    EchPayload(usize),
    Padding,
}

fn str_op(s: &'static [u8]) -> Op {
    Op::Str(s)
}

/// The non-Darwin `TlsHello::get_default()` table, verbatim.
fn build_default_hello() -> Vec<Op> {
    use Op::{BeginScope, Domain, EndScope, Grease, Key, MlKem768Key, Padding, Random, Zero};
    vec![
        str_op(b"\x16\x03\x01"),
        BeginScope,
        str_op(b"\x01\x00"),
        BeginScope,
        str_op(b"\x03\x03"),
        Zero(32),
        str_op(b"\x20"),
        Random(32),
        str_op(b"\x00\x20"),
        Grease(0),
        str_op(b"\x13\x01\x13\x02\x13\x03\xc0\x2b\xc0\x2f\xc0\x2c\xc0\x30\xcc\xa9\xcc\xa8\xc0\x13\xc0\x14\x00\x9c\x00\x9d\x00\x2f\x00\x35\x01\x00"),
        BeginScope,
        Grease(2),
        str_op(b"\x00\x00"),
        Op::Permutation(vec![
            vec![
                str_op(b"\x00\x00"),
                BeginScope,
                BeginScope,
                str_op(b"\x00"),
                BeginScope,
                Domain,
                EndScope,
                EndScope,
                EndScope,
            ],
            vec![str_op(b"\x00\x05\x00\x05\x01\x00\x00\x00\x00")],
            vec![
                str_op(b"\x00\x0a\x00\x0c\x00\x0a"),
                Grease(4),
                str_op(b"\x11\xec\x00\x1d\x00\x17\x00\x18"),
            ],
            vec![str_op(b"\x00\x0b\x00\x02\x01\x00")],
            vec![str_op(b"\x00\x0d\x00\x18\x00\x16\x09\x04\x09\x05\x09\x06\x04\x03\x08\x04\x04\x01\x05\x03\x08\x05\x05\x01\x08\x06\x06\x01")],
            vec![str_op(b"\x00\x10\x00\x0e\x00\x0c\x02\x68\x32\x08\x68\x74\x74\x70\x2f\x31\x2e\x31")],
            vec![str_op(b"\x00\x12\x00\x00")],
            vec![str_op(b"\x00\x17\x00\x00")],
            vec![str_op(b"\x00\x1b\x00\x03\x02\x00\x02")],
            vec![str_op(b"\x00\x23\x00\x00")],
            vec![
                str_op(b"\x00\x2b\x00\x07\x06"),
                Grease(6),
                str_op(b"\x03\x04\x03\x03"),
            ],
            vec![str_op(b"\x00\x2d\x00\x02\x01\x01")],
            vec![
                str_op(b"\x00\x33\x04\xef\x04\xed"),
                Grease(4),
                str_op(b"\x00\x01\x00\x11\xec\x04\xc0"),
                MlKem768Key,
                Key,
                str_op(b"\x00\x1d\x00\x20"),
                Key,
            ],
            vec![str_op(b"\x44\xcd\x00\x05\x00\x03\x02\x68\x32")],
            vec![
                str_op(b"\xfe\x0d"),
                BeginScope,
                str_op(b"\x00\x00\x01\x00\x01"),
                Random(1),
                str_op(b"\x00\x20"),
                Key,
                BeginScope,
                Op::EchPayload(random::fast(0, 3) as usize * 32 + 144),
                EndScope,
                EndScope,
            ],
            vec![str_op(b"\xff\x01\x00\x01\x00")],
        ]),
        Grease(3),
        str_op(b"\x00\x01\x00"),
        Padding,
        EndScope,
        EndScope,
        EndScope,
    ]
}

/// The C++ hello table is a function-local static — constructed once per
/// process, baking the `ech_payload` length at first use. The quirk is
/// preserved (it is invisible on the wire: the length is random anyway).
static DEFAULT_HELLO: LazyLock<Vec<Op>> = LazyLock::new(build_default_hello);

/// `TlsHello::get_grease_size()`.
const GREASE_SIZE: usize = 7;

struct TlsHelloContext {
    grease: Vec<u8>,
    domain: Vec<u8>,
}

impl TlsHelloContext {
    fn new(domain: &str) -> Self {
        let mut grease = vec![0u8; GREASE_SIZE];
        init_grease(&mut grease);
        Self {
            grease,
            domain: domain.as_bytes()[..domain.len().min(MAX_DOMAIN_LENGTH)].to_vec(),
        }
    }
}

/// `TlsHelloCalcLength`.
struct TlsHelloCalcLength {
    size: usize,
    status: Status,
    scope_offset: Vec<usize>,
}

impl TlsHelloCalcLength {
    fn new() -> Self {
        Self {
            size: 0,
            status: Status::ok(),
            scope_offset: Vec::new(),
        }
    }

    fn on_error(&mut self, error: Status) {
        if self.status.is_ok() {
            self.status = error;
        }
    }

    fn do_op(&mut self, op: &Op, context: &TlsHelloContext) {
        if self.status.is_error() {
            return;
        }
        match op {
            Op::Str(data) => self.size += data.len(),
            Op::Random(length) => {
                if *length == 0 || *length > 2048 {
                    self.on_error(Status::error_message("Invalid random length"));
                    return;
                }
                self.size += length;
            }
            Op::Zero(length) => {
                if *length == 0 || *length > 2048 {
                    self.on_error(Status::error_message("Invalid zero length"));
                    return;
                }
                self.size += length;
            }
            Op::Domain => self.size += context.domain.len(),
            Op::Grease(seed) => {
                if *seed >= context.grease.len() {
                    self.on_error(Status::error_message("Invalid grease seed"));
                    return;
                }
                self.size += 2;
            }
            Op::Key => self.size += 32,
            Op::MlKem768Key => self.size += 1184,
            Op::BeginScope => {
                self.size += 2;
                self.scope_offset.push(self.size);
            }
            Op::EndScope => {
                let Some(begin_offset) = self.scope_offset.pop() else {
                    self.on_error(Status::error_message("Unbalanced scopes"));
                    return;
                };
                if self.size - begin_offset >= (1 << 14) {
                    self.on_error(Status::error_message("Scope is too big"));
                }
            }
            Op::Permutation(parts) => {
                for part in parts {
                    for nested_op in part {
                        self.do_op(nested_op, context);
                    }
                }
            }
            Op::EchPayload(length) => self.size += length,
            Op::Padding => {
                if self.size < 513 {
                    self.size = 517;
                }
            }
        }
    }

    /// `finish()` — `Ok(size)` or the first error.
    fn finish(&mut self) -> Result<usize> {
        if self.status.is_error() {
            return Result::Err(std::mem::take(&mut self.status));
        }
        if !self.scope_offset.is_empty() {
            return Result::Err(Status::error_message("Unbalanced scopes"));
        }
        if self.size < 11 + 32 {
            return Result::Err(Status::error_message("Too small for hash"));
        }
        Result::Ok(self.size)
    }
}

/// `TlsHelloStore`, writing into `data` at a cursor.
struct TlsHelloStore<'a> {
    data: &'a mut [u8],
    offset: usize,
    scope_offset: Vec<usize>,
}

impl<'a> TlsHelloStore<'a> {
    fn new(data: &'a mut [u8]) -> Self {
        Self {
            data,
            offset: 0,
            scope_offset: Vec::new(),
        }
    }

    fn do_op(&mut self, op: &Op, context: &TlsHelloContext) {
        match op {
            Op::Str(data) => {
                self.data[self.offset..self.offset + data.len()].copy_from_slice(data);
                self.offset += data.len();
            }
            Op::Random(length) => {
                random::secure_bytes(&mut self.data[self.offset..self.offset + *length]);
                self.offset += length;
            }
            Op::Zero(length) => {
                self.data[self.offset..self.offset + length].fill(0);
                self.offset += length;
            }
            Op::Domain => {
                let domain = &context.domain;
                self.data[self.offset..self.offset + domain.len()].copy_from_slice(domain);
                self.offset += domain.len();
            }
            Op::Grease(seed) => {
                let grease = context.grease[*seed];
                self.data[self.offset] = grease;
                self.data[self.offset + 1] = grease;
                self.offset += 2;
            }
            Op::Key => {
                generate_curve25519_hardened_key(&mut self.data[self.offset..self.offset + 32]);
                self.offset += 32;
            }
            Op::MlKem768Key => {
                for _ in 0..384 {
                    let a = (random::secure_uint32() % 3329) as usize;
                    let b = (random::secure_uint32() % 3329) as usize;
                    let dest = &mut self.data[self.offset..self.offset + 3];
                    dest[0] = (a & 255) as u8;
                    dest[1] = (((a >> 8) + ((b & 15) << 4)) & 255) as u8;
                    dest[2] = (b >> 4) as u8;
                    self.offset += 3;
                }
                self.do_op(&Op::Random(32), context);
            }
            Op::BeginScope => {
                self.scope_offset.push(self.offset);
                self.offset += 2;
            }
            Op::EndScope => {
                let begin_offset = self.scope_offset.pop().expect("unbalanced scopes");
                let size = self.offset - begin_offset - 2;
                assert!(size < (1 << 14), "scope is too big");
                self.data[begin_offset] = ((size >> 8) & 0xff) as u8;
                self.data[begin_offset + 1] = (size & 0xff) as u8;
            }
            Op::Permutation(parts) => {
                let mut encoded = Vec::with_capacity(parts.len());
                for part in parts {
                    let mut calc_length = TlsHelloCalcLength::new();
                    for nested_op in part {
                        calc_length.do_op(nested_op, context);
                    }
                    let length = calc_length.size;
                    let mut part_data = vec![0u8; length];
                    let mut storer = TlsHelloStore::new(&mut part_data);
                    for nested_op in part {
                        storer.do_op(nested_op, context);
                    }
                    assert!(storer.offset == part_data.len(), "length mismatch");
                    encoded.push(part_data);
                }
                random::shuffle(&mut encoded, &mut || random::fast_uint64());
                for part in encoded {
                    self.data[self.offset..self.offset + part.len()].copy_from_slice(&part);
                    self.offset += part.len();
                }
            }
            Op::EchPayload(length) => {
                random::secure_bytes(&mut self.data[self.offset..self.offset + *length]);
                self.offset += *length;
            }
            Op::Padding => {
                if let Some(size) = 513usize.checked_sub(self.offset).filter(|s| *s > 0) {
                    self.do_op(&Op::Str(b"\x00\x15"), context);
                    self.do_op(&Op::BeginScope, context);
                    self.do_op(&Op::Zero(size), context);
                    self.do_op(&Op::EndScope, context);
                }
            }
        }
    }

    /// `finish(secret, unix_time)`: the hello MAC, stamped with the time.
    fn finish(&mut self, secret: &[u8], unix_time: i32) {
        let hash = hmac_sha256(secret, self.data);
        self.data[11..43].copy_from_slice(&hash);
        let old = i32::from_le_bytes(self.data[39..43].try_into().unwrap());
        let stamped = old ^ unix_time;
        self.data[39..43].copy_from_slice(&stamped.to_le_bytes());
        assert!(self.offset == self.data.len(), "hello underfilled");
    }
}

/// The `Type::Key` body: a random curve25519 x-coordinate, hardened by
/// requiring y² to be a quadratic residue and tripling the point.
fn generate_curve25519_hardened_key(key: &mut [u8]) {
    let module = curve25519_module();
    let mut context = BigNumContext::new();
    loop {
        random::secure_bytes(key);
        key[31] &= 127;

        let mut x = BigNum::from_binary(key);
        let y2 = get_y2(&x, &module, &mut context);
        if is_quadratic_residue(&y2) {
            for _ in 0..3 {
                x = get_double_x(&x, &module, &mut context);
            }
            key.copy_from_slice(&x.to_le_binary(32));
            break;
        }
    }
}

/// 2^255 - 19, as in the C++ `from_hex("7fff...ed")`.
fn curve25519_module() -> BigNum {
    BigNum::from_hex("7fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffed")
        .move_as_ok()
}

/// returns y = x^3 + 486662 * x^2 + x
fn get_y2(x: &BigNum, module: &BigNum, context: &mut BigNumContext) -> BigNum {
    let mut y = x.clone();
    let coef = BigNum::from_decimal("486662").move_as_ok();
    y = BigNum::mod_add(&y, &coef, module, context);
    y = BigNum::mod_mul(&y, x, module, context);
    let one = BigNum::from_decimal("1").move_as_ok();
    y = BigNum::mod_add(&y, &one, module, context);
    y = BigNum::mod_mul(&y, x, module, context);
    y
}

/// returns x_2 = (x^2 - 1)^2/(4*y^2)
fn get_double_x(x: &BigNum, module: &BigNum, context: &mut BigNumContext) -> BigNum {
    let mut denominator = get_y2(x, module, context);
    let four = BigNum::from_decimal("4").move_as_ok();
    denominator = BigNum::mod_mul(&denominator, &four, module, context);

    let mut numerator = BigNum::mod_mul(x, x, module, context);
    let one = BigNum::from_decimal("1").move_as_ok();
    numerator = BigNum::mod_sub(&numerator, &one, module, context);
    numerator = BigNum::mod_mul(&numerator, &numerator, module, context);

    let inverse = match BigNum::mod_inverse(&denominator, module, context) {
        Result::Ok(value) => value,
        // The C++ logs and keeps the un-inverted denominator; the caller
        // re-rolls on the next loop iteration.
        Result::Err(_) => denominator,
    };
    BigNum::mod_mul(&numerator, &inverse, module, context)
}

fn is_quadratic_residue(a: &BigNum) -> bool {
    // 2^255 - 19
    let module = curve25519_module();
    // (mod - 1) / 2 = 2^254 - 10
    let pow = BigNum::from_hex("3ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff6")
        .move_as_ok();

    let mut context = BigNumContext::new();
    let r = BigNum::mod_exp(a, &pow, &module, &mut context);

    r.to_decimal() == "1"
}

/// `TlsObfusaction::generate_header(domain, secret, unix_time)`.
pub fn generate_header(domain: &str, secret: &[u8], unix_time: i32) -> Vec<u8> {
    assert!(!domain.is_empty(), "empty domain");
    assert!(secret.len() == 16, "secret must be 16 bytes");

    let hello = &*DEFAULT_HELLO;
    let context = TlsHelloContext::new(domain);
    let mut calc_length = TlsHelloCalcLength::new();
    for op in hello {
        calc_length.do_op(op, &context);
    }
    let length = calc_length.finish().move_as_ok();
    let mut data = vec![0u8; length];
    let mut storer = TlsHelloStore::new(&mut data);
    for op in hello {
        storer.do_op(op, &context);
    }
    storer.finish(secret, unix_time);
    data
}

/// `td::mtproto::TlsInit` — run the fake-TLS handshake with an MTProxy
/// over an already-connected stream (`domain` + 16-byte `secret` from the
/// proxy link, `server_time_difference` as in the C++ constructor).
pub async fn connect(
    mut stream: TcpStream,
    domain: &str,
    secret: &str,
    server_time_difference: f64,
) -> Result<TcpStream> {
    let mut leftover = Vec::new();
    tdutils::try_result!(
        status,
        with_timeout(tls_init_handshake(
            &mut stream,
            domain,
            secret,
            server_time_difference,
            &mut leftover,
        ))
        .await
    );
    if status.is_error() {
        return Result::from_status(status);
    }
    // The inherited C++ `tear_down` check: nothing may follow the hello
    // response until the tunnel is up.
    if !leftover.is_empty() {
        return Result::Err(Status::error_message("Proxy has sent too many data"));
    }
    tdutils::try_status!(reject_early_data(&mut stream).await);
    Result::Ok(stream)
}

async fn tls_init_handshake(
    stream: &mut TcpStream,
    domain: &str,
    secret: &str,
    server_time_difference: f64,
    leftover: &mut Vec<u8>,
) -> Status {
    // `TlsInit::send_hello`:
    // `static_cast<int32>(Time::now() + server_time_difference_)`.
    let unix_time = (time::now() + server_time_difference) as i32;
    let hello = generate_header(domain, secret.as_bytes(), unix_time);
    let hello_rand: [u8; 32] = hello[11..43].try_into().unwrap();
    tdutils::try_status!(write_all(stream, &hello).await);

    // `TlsInit::wait_hello_response` — two framed sections, then the MAC
    // check over everything consumed so far.
    let mut received: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 512];
    let mut consumed = 0usize;
    for prefix in [
        &b"\x16\x03\x03"[..],
        &b"\x14\x03\x03\x00\x01\x01\x17\x03\x03"[..],
    ] {
        while received.len() < consumed + prefix.len() + 2 {
            tdutils::try_status!(read_some(stream, &mut received, &mut chunk).await);
        }
        if received[consumed..consumed + prefix.len()] != *prefix {
            return Status::error_message("First part of response to hello is invalid");
        }
        let skip_size = ((received[consumed + prefix.len()] as usize) << 8)
            | received[consumed + prefix.len() + 1] as usize;
        consumed += prefix.len() + 2 + skip_size;
        while received.len() < consumed {
            tdutils::try_status!(read_some(stream, &mut received, &mut chunk).await);
        }
    }

    let mut response = received[..consumed].to_vec();
    *leftover = received.split_off(consumed);
    let response_rand: [u8; 32] = response[11..43].try_into().unwrap();
    response[11..43].fill(0);
    let mut mac_input = Vec::with_capacity(32 + response.len());
    mac_input.extend_from_slice(&hello_rand);
    mac_input.extend_from_slice(&response);
    let hash = hmac_sha256(secret.as_bytes(), &mac_input);
    if hash != response_rand {
        return Status::error_message("Response hash mismatch");
    }
    Status::ok()
}
