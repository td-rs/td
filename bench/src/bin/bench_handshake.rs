//! Port of `benchmark/bench_handshake.cpp` (S79): full DH handshake pairs
//! (two `DhHandshake` instances exchanging `g_b`/`g_a`, checks + key
//! generation) against the standard Telegram 2048-bit prime, with the
//! C++ `FakeDhCallback` prime cache. A pair is counted as one operation:
//! `run(n)` performs `n / 2` pairs, exactly like the C++ bench.

use std::cell::RefCell;
use std::collections::HashMap;

use base64::Engine as _;
use bench::benchmark::{Benchmark, bench};
use tdmtproto::dh_callback::DhCallback;
use tdmtproto::dh_handshake::DhHandshake;

const G: i32 = 3;

/// The C++ `prime_base64` (the standard Telegram 2048-bit safe prime).
const PRIME_BASE64: &str = "xxyuucaxyQSObFIvcPE_c5gNQCOOPiHBSTTQN1Y9kw9IGYoKp8FAWCKUk9IlMPTb-jNvbgrJJROVQ67UTM58NyD9UfaUWHBaxozU_mtrE6vcl0ZRKWkyhFTxj6-MWV9kJHf-lrsqlB1bzR1KyMxJiAcI-ps3jjxPOpBgvuZ8-aSkppWBEFGQfhYnU7VrD2tBDbp02KhLKhSzFE4O8ShHVP0X7ZUNWWW0ud1GWC2xF40WnGvEZbDW_5yjko_vW5rk5Bj8Feg-vqD4f6n_Xu1wBQ3tKEn0e_lZ2VaFDOkphR8NgRX2NbEF7i5OFdBLJFS_b0-t8DSxBAMRnNjjuS_MWw";

/// The `FakeDhCallback`: a prime cache over interior mutability (the C++
/// `mutable std::map` member).
struct FakeDhCallback {
    cache: RefCell<HashMap<Vec<u8>, i32>>,
}

impl DhCallback for FakeDhCallback {
    fn is_good_prime(&self, prime_str: &[u8]) -> i32 {
        self.cache.borrow().get(prime_str).copied().unwrap_or(-1)
    }
    fn add_good_prime(&self, prime_str: &[u8]) {
        self.cache.borrow_mut().insert(prime_str.to_vec(), 1);
    }
    fn add_bad_prime(&self, prime_str: &[u8]) {
        self.cache.borrow_mut().insert(prime_str.to_vec(), 0);
    }
}

struct HandshakeBench {
    prime: Vec<u8>,
    callback: FakeDhCallback,
}

impl Benchmark for HandshakeBench {
    fn get_description(&self) -> String {
        "Handshake".into()
    }
    fn run(&mut self, n: usize) {
        DhHandshake::check_config(G, &self.prime, &self.callback).ensure();
        for _ in (0..n).step_by(2) {
            let mut a = DhHandshake::new();
            let mut b = DhHandshake::new();
            a.set_config(G, &self.prime);
            b.set_config(G, &self.prime);
            let g_b = b.get_g_b();
            b.set_g_a(&a.get_g_b());
            a.set_g_a(&g_b);
            a.run_checks(true, &self.callback).ensure();
            b.run_checks(true, &self.callback).ensure();
            let a_key = a.gen_key();
            let b_key = b.gen_key();
            assert_eq!(a_key.0, b_key.0);
        }
    }
}

fn main() {
    let prime = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(PRIME_BASE64)
        .expect("prime base64url");
    bench(&mut HandshakeBench {
        prime,
        callback: FakeDhCallback {
            cache: RefCell::new(HashMap::new()),
        },
    });
}
