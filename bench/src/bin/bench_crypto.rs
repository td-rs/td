//! Port of `benchmark/bench_crypto.cpp` (S79): AES CTR/CBC/IGE/ECB, pbkdf2,
//! the RNG suite, short SHA/HMAC, CRC32/CRC64 — measured by the ported
//! `bench` driver on the Rust crypto stack (RustCrypto `aes` + the hand
//! -rolled IGE, `sha2`, `crc32fast`, `getrandom`).
//!
//! Divergences from the C++ benchmark, documented in `bench/README.md`:
//!
//! - `AesCtrOpenSSLBench` (raw `EVP_EncryptUpdate`) and the OpenSSL ≤ 1.0
//!   `SHA1Bench` are skipped — they compare td wrappers against raw OpenSSL
//!   call overhead, a distinction the Rust stack does not have; `AesCtrBench`
//!   covers the same operation.
//! - `std_rand` (libc `std::rand()`) is skipped — no Rust counterpart;
//!   `mt19937_rand` is kept as a faithful MT19937 implementation.
//! - The AES benches write from a fixed source snapshot instead of mutating
//!   the buffer in place (in-place `src == dst` aliasing is unrepresentable
//!   in safe Rust; the crypto work per iteration is identical).
//! - Descriptions drop the "OpenSSL" suffix (the backend is not OpenSSL);
//!   everything else matches the C++ labels row for row. The SHA1/ECB rows
//!   measure tdlib's protocol primitives, not production data protection.

use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use bench::benchmark::{Benchmark, bench};
use tdutils::crypto;
use tdutils::random;

const DATA_SIZE: usize = 8 << 10;
const SHORT_DATA_SIZE: usize = 64;

/// The C++ `alignas(64)` data member.
#[repr(align(64))]
struct Aligned<const N: usize>([u8; N]);

impl<const N: usize> Aligned<N> {
    fn filled(byte: u8) -> Self {
        Aligned([byte; N])
    }
}

/// `td::UInt256`/`UInt128` key material, filled from the CSPRNG per bench.
struct Key<const N: usize>([u8; N]);

impl<const N: usize> Key<N> {
    fn secure() -> Self {
        let mut key = Key([0u8; N]);
        random::secure_bytes(&mut key.0);
        key
    }
    fn as_slice(&self) -> &[u8] {
        &self.0
    }
}

struct AesCtrBench {
    src: Aligned<DATA_SIZE>,
    key: Key<32>,
    iv: Key<16>,
}

impl Benchmark for AesCtrBench {
    fn get_description(&self) -> String {
        format!("AES CTR [{}KB]", DATA_SIZE >> 10)
    }
    fn start_up(&mut self) {
        self.src = Aligned::filled(123);
        self.key = Key::secure();
        self.iv = Key::secure();
    }
    fn run(&mut self, n: usize) {
        let mut state = crypto::AesCtrState::new();
        state.init(self.key.as_slice(), self.iv.as_slice());
        let mut data = self.src.0;
        for _ in 0..n {
            state.encrypt_in_place(&mut data);
        }
        black_box(data);
    }
}

struct AesCbcBench {
    decrypt: bool,
    src: Aligned<DATA_SIZE>,
    key: Key<32>,
    iv: Key<16>,
}

impl Benchmark for AesCbcBench {
    fn get_description(&self) -> String {
        format!(
            "AES CBC {} [{}KB]",
            if self.decrypt { "Decrypt" } else { "Encrypt" },
            DATA_SIZE >> 10
        )
    }
    fn start_up(&mut self) {
        self.src = Aligned::filled(123);
        self.key = Key::secure();
        self.iv = Key::secure();
    }
    fn run(&mut self, n: usize) {
        let mut data = self.src.0;
        for _ in 0..n {
            let mut iv = self.iv.0;
            if self.decrypt {
                crypto::aes_cbc_decrypt(self.key.as_slice(), &mut iv, &self.src.0, &mut data);
            } else {
                crypto::aes_cbc_encrypt(self.key.as_slice(), &mut iv, &self.src.0, &mut data);
            }
        }
        black_box(data);
    }
}

/// `AesIgeShortBench<use_state>`: 64-byte IGE decrypt, either through a
/// fresh `AesIgeState` per iteration (the C++ EVP form) or the one-shot
/// `aes_ige_decrypt` function (the C++ C form).
struct AesIgeShortBench {
    use_state: bool,
    src: Aligned<SHORT_DATA_SIZE>,
    key: Key<32>,
    iv: Key<32>,
}

impl Benchmark for AesIgeShortBench {
    fn get_description(&self) -> String {
        format!(
            "AES IGE {} [{}B]",
            if self.use_state { "state" } else { "c-fn" },
            SHORT_DATA_SIZE
        )
    }
    fn start_up(&mut self) {
        self.src = Aligned::filled(123);
        self.key = Key::secure();
        self.iv = Key::secure();
    }
    fn run(&mut self, n: usize) {
        let mut data = self.src.0;
        for _ in 0..n {
            let mut iv = self.iv.0;
            if self.use_state {
                let mut ige = crypto::AesIgeState::new();
                ige.init(self.key.as_slice(), &iv, false);
                ige.decrypt(&self.src.0, &mut data);
            } else {
                crypto::aes_ige_decrypt(self.key.as_slice(), &mut iv, &self.src.0, &mut data);
            }
        }
        black_box(data);
    }
}

struct AesIgeBench {
    encrypt: bool,
    src: Aligned<DATA_SIZE>,
    key: Key<32>,
    iv: Key<32>,
}

impl Benchmark for AesIgeBench {
    fn get_description(&self) -> String {
        format!(
            "AES IGE {} [{}KB]",
            if self.encrypt { "encrypt" } else { "decrypt" },
            DATA_SIZE >> 10
        )
    }
    fn start_up(&mut self) {
        self.src = Aligned::filled(123);
        self.key = Key::secure();
        self.iv = Key::secure();
    }
    fn run(&mut self, n: usize) {
        let mut state = crypto::AesIgeState::new();
        state.init(self.key.as_slice(), &self.iv.0, self.encrypt);
        let mut data = self.src.0;
        for _ in 0..n {
            if self.encrypt {
                state.encrypt(&self.src.0, &mut data);
            } else {
                state.decrypt(&self.src.0, &mut data);
            }
        }
        black_box(data);
    }
}

struct AesEcbBench {
    src: Aligned<DATA_SIZE>,
    key: Key<32>,
}

impl Benchmark for AesEcbBench {
    fn get_description(&self) -> String {
        format!("AES ECB [{}KB]", DATA_SIZE >> 10)
    }
    fn start_up(&mut self) {
        self.src = Aligned::filled(123);
        self.key = Key::secure();
    }
    fn run(&mut self, n: usize) {
        let mut state = crypto::AesState::new();
        state.init(self.key.as_slice(), true);
        let mut data = self.src.0;
        for _ in 0..=n {
            for offset in (0..DATA_SIZE).step_by(16) {
                state.encrypt(
                    &self.src.0[offset..offset + 16],
                    &mut data[offset..offset + 16],
                );
            }
        }
        black_box(data);
    }
}

/// `Pbkdf2Bench`: one `pbkdf2_sha256` call per pass, with the pass size `n`
/// as the iteration count — the C++ `run(int n)` semantic.
struct Pbkdf2Bench;

impl Benchmark for Pbkdf2Bench {
    fn get_description(&self) -> String {
        "pbkdf2".into()
    }
    fn run(&mut self, n: usize) {
        let password = b"cucumber";
        let salt = b"abcdefghijklmnopqrstuvw";
        let mut key = [b' '; 32];
        crypto::pbkdf2_sha256(password, salt, n as u32, &mut key);
        black_box(key);
    }
}

/// `CppRandBench`: a faithful MT19937 (the C++ `std::mt19937`), the
/// stdlib-RNG baseline the fast RNG is measured against. The twist is the
/// canonical split-loop form libstdc++ uses (first `N-M` words, then the
/// wraparound range, final word separately — no modulo indexing, and the
/// matrix term selected branchlessly); verified bit-identical to the
/// reference recurrence over 2M outputs × 4 seeds and against the
/// `std::mt19937(5489)` known answers.
struct Mt19937 {
    state: [u32; 624],
    index: usize,
}

impl Mt19937 {
    fn new(seed: u32) -> Self {
        let mut state = [0u32; 624];
        state[0] = seed;
        for i in 1..624 {
            state[i] = 1812433253u32
                .wrapping_mul(state[i - 1] ^ (state[i - 1] >> 30))
                .wrapping_add(i as u32);
        }
        Mt19937 { state, index: 624 }
    }

    fn twist(&mut self) {
        const N: usize = 624;
        const M: usize = 397;
        const MATRIX: u32 = 0x9908_b0df;
        const UPPER: u32 = 0x8000_0000;
        const LOWER: u32 = 0x7fff_ffff;
        let mag = |y: u32| (y & 1).wrapping_neg() & MATRIX;
        for i in 0..N - M {
            let y = (self.state[i] & UPPER) | (self.state[i + 1] & LOWER);
            self.state[i] = self.state[i + M] ^ (y >> 1) ^ mag(y);
        }
        for i in N - M..N - 1 {
            let y = (self.state[i] & UPPER) | (self.state[i + 1] & LOWER);
            self.state[i] = self.state[i + M - N] ^ (y >> 1) ^ mag(y);
        }
        let y = (self.state[N - 1] & UPPER) | (self.state[0] & LOWER);
        self.state[N - 1] = self.state[M - 1] ^ (y >> 1) ^ mag(y);
        self.index = 0;
    }

    fn next(&mut self) -> u32 {
        if self.index >= 624 {
            self.twist();
        }
        let mut y = self.state[self.index];
        self.index += 1;
        y ^= y >> 11;
        y ^= (y << 7) & 0x9d2c_5680;
        y ^= (y << 15) & 0xefc6_0000;
        y ^= y >> 18;
        y
    }
}

struct CppRandBench;

impl Benchmark for CppRandBench {
    fn get_description(&self) -> String {
        "mt19937_rand".into()
    }
    fn run(&mut self, n: usize) {
        let mut g = Mt19937::new(123);
        let mut res: u32 = 0;
        for _ in 0..n {
            res ^= g.next();
        }
        black_box(res);
    }
}

struct TdRand32Bench;

impl Benchmark for TdRand32Bench {
    fn get_description(&self) -> String {
        "td_rand_fast32".into()
    }
    fn run(&mut self, n: usize) {
        let mut res: u32 = 0;
        for _ in 0..n {
            res ^= random::fast_uint32();
        }
        black_box(res);
    }
}

/// The C `RAND_MAX`.
const RAND_MAX: i32 = 2147483647;

struct TdRandFastBench;

impl Benchmark for TdRandFastBench {
    fn get_description(&self) -> String {
        "td_rand_fast".into()
    }
    fn run(&mut self, n: usize) {
        let mut res: i32 = 0;
        for _ in 0..n {
            res ^= random::fast(0, RAND_MAX);
        }
        black_box(res);
    }
}

/// `SslRandBench`: 3 threads of `secure_int32`, summed through an atomic —
/// the threaded-CSPRNG-throughput probe.
struct SslRandBench;

impl Benchmark for SslRandBench {
    fn get_description(&self) -> String {
        "ssl_rand_int32".into()
    }
    fn run(&mut self, n: usize) {
        let sum = Arc::new(AtomicU32::new(0));
        let mut threads = Vec::new();
        for _ in 0..3 {
            let sum = Arc::clone(&sum);
            threads.push(std::thread::spawn(move || {
                let mut res: i32 = 0;
                for _ in 0..n {
                    res ^= random::secure_int32();
                }
                sum.fetch_add(res as u32, Ordering::Relaxed);
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }
        black_box(sum.load(Ordering::Relaxed));
    }
}

struct SslRandBufBench;

impl Benchmark for SslRandBufBench {
    fn get_description(&self) -> String {
        "ssl_rand_bytes".into()
    }
    fn run(&mut self, n: usize) {
        let mut res: i32 = 0;
        let mut buf = [0u8; 4000]; // 1000 × i32, the C++ int-array fill
        let mut i = 0;
        while i < n {
            random::secure_bytes(&mut buf);
            for chunk in buf.as_chunks::<4>().0 {
                res ^= i32::from_le_bytes(*chunk);
            }
            i += 1000;
        }
        black_box(res);
    }
}

#[derive(Clone, Copy)]
enum ShortHashKind {
    Sha1,
    Sha256,
    Sha512,
    HmacSha256,
    HmacSha512,
}

struct ShortHashBench {
    kind: ShortHashKind,
    src: Aligned<SHORT_DATA_SIZE>,
}

impl Benchmark for ShortHashBench {
    fn get_description(&self) -> String {
        match self.kind {
            ShortHashKind::Sha1 => format!("SHA1 [{}B]", SHORT_DATA_SIZE),
            ShortHashKind::Sha256 => format!("SHA256 [{}B]", SHORT_DATA_SIZE),
            ShortHashKind::Sha512 => format!("SHA512 [{}B]", SHORT_DATA_SIZE),
            ShortHashKind::HmacSha256 => format!("HMAC-SHA256 [{}B]", SHORT_DATA_SIZE),
            ShortHashKind::HmacSha512 => format!("HMAC-SHA512 [{}B]", SHORT_DATA_SIZE),
        }
    }
    fn start_up(&mut self) {
        self.src = Aligned::filled(123);
    }
    fn run(&mut self, n: usize) {
        let data = self.src.0;
        for _ in 0..n {
            match self.kind {
                ShortHashKind::Sha1 => {
                    black_box(crypto::sha1(&data));
                }
                ShortHashKind::Sha256 => {
                    black_box(crypto::sha256(&data));
                }
                ShortHashKind::Sha512 => {
                    black_box(crypto::sha512(&data));
                }
                ShortHashKind::HmacSha256 => {
                    black_box(crypto::hmac_sha256(&data[..32], &data));
                }
                ShortHashKind::HmacSha512 => {
                    black_box(crypto::hmac_sha512(&data[..64], &data));
                }
            }
        }
    }
}

struct CrcBench {
    crc64: bool,
    src: Aligned<DATA_SIZE>,
}

impl Benchmark for CrcBench {
    fn get_description(&self) -> String {
        format!(
            "{} [{}KB]",
            if self.crc64 {
                "CRC64 Anton"
            } else {
                "CRC32 zlib"
            },
            DATA_SIZE >> 10
        )
    }
    fn start_up(&mut self) {
        self.src = Aligned::filled(123);
    }
    fn run(&mut self, n: usize) {
        let mut res: u64 = 0;
        for _ in 0..n {
            res = res.wrapping_add(if self.crc64 {
                crypto::crc64(&self.src.0)
            } else {
                u64::from(crc32fast::hash(&self.src.0))
            });
        }
        black_box(res);
    }
}

fn main() {
    bench(&mut AesCtrBench {
        src: Aligned::filled(123),
        key: Key::secure(),
        iv: Key::secure(),
    });
    bench(&mut AesCbcBench {
        decrypt: true,
        src: Aligned::filled(123),
        key: Key::secure(),
        iv: Key::secure(),
    });
    bench(&mut AesCbcBench {
        decrypt: false,
        src: Aligned::filled(123),
        key: Key::secure(),
        iv: Key::secure(),
    });
    bench(&mut AesIgeShortBench {
        use_state: true,
        src: Aligned::filled(123),
        key: Key::secure(),
        iv: Key::secure(),
    });
    bench(&mut AesIgeShortBench {
        use_state: false,
        src: Aligned::filled(123),
        key: Key::secure(),
        iv: Key::secure(),
    });
    bench(&mut AesIgeBench {
        encrypt: true,
        src: Aligned::filled(123),
        key: Key::secure(),
        iv: Key::secure(),
    });
    bench(&mut AesIgeBench {
        encrypt: false,
        src: Aligned::filled(123),
        key: Key::secure(),
        iv: Key::secure(),
    });
    bench(&mut AesEcbBench {
        src: Aligned::filled(123),
        key: Key::secure(),
    });

    bench(&mut Pbkdf2Bench);
    bench(&mut CppRandBench);
    bench(&mut TdRand32Bench);
    bench(&mut TdRandFastBench);
    bench(&mut SslRandBench);
    bench(&mut SslRandBufBench);

    for kind in [
        ShortHashKind::Sha1,
        ShortHashKind::Sha256,
        ShortHashKind::Sha512,
        ShortHashKind::HmacSha256,
        ShortHashKind::HmacSha512,
    ] {
        bench(&mut ShortHashBench {
            kind,
            src: Aligned::filled(123),
        });
    }

    bench(&mut CrcBench {
        crc64: false,
        src: Aligned::filled(123),
    });
    bench(&mut CrcBench {
        crc64: true,
        src: Aligned::filled(123),
    });
}
