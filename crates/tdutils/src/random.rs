//! Port of `td/utils/Random.h` + `td/utils/Random.cpp`.
//!
//! The secure API draws from the OS CSPRNG via `getrandom` (replacing
//! OpenSSL's `RAND_bytes`), and the fast API uses a thread-local
//! [`Xorshift128plus`] seeded from the OS CSPRNG (replacing the
//! nondeterministically seeded `std::mt19937` — the sequence was never
//! reproducible, and the distribution contract is unchanged). The seeded
//! [`Xorshift128plus`] used throughout tdlib is a bit-exact port.

use std::cell::RefCell;

/// `td::Random::secure_bytes(dest)`: fills `dest` from the OS CSPRNG.
pub fn secure_bytes(dest: &mut [u8]) {
    getrandom::fill(dest).expect("getrandom failed: OS CSPRNG unavailable");
}

/// `td::Random::secure_int32`.
pub fn secure_int32() -> i32 {
    let mut buf = [0u8; 4];
    secure_bytes(&mut buf);
    i32::from_le_bytes(buf)
}

/// `td::Random::secure_int64`.
pub fn secure_int64() -> i64 {
    let mut buf = [0u8; 8];
    secure_bytes(&mut buf);
    i64::from_le_bytes(buf)
}

/// `td::Random::secure_uint32`.
pub fn secure_uint32() -> u32 {
    let mut buf = [0u8; 4];
    secure_bytes(&mut buf);
    u32::from_le_bytes(buf)
}

/// `td::Random::secure_uint64`.
pub fn secure_uint64() -> u64 {
    let mut buf = [0u8; 8];
    secure_bytes(&mut buf);
    u64::from_le_bytes(buf)
}

/// `td::Random::add_seed(bytes, entropy)`: a no-op here — `getrandom` always
/// draws from the OS entropy pool, which cannot be reseeded from userspace
/// (the C++ version feeds OpenSSL's internal mixer).
pub fn add_seed(_bytes: &[u8], _entropy: f64) {}

/// `td::Random::secure_cleanup`: a no-op — there is no thread-local secure
/// buffer to zero (`getrandom` fills caller buffers directly).
pub fn secure_cleanup() {}

/// `td::Random::shuffle` with the secure RNG.
pub fn secure_shuffle<T>(v: &mut [T]) {
    for i in 1..v.len() {
        let pos = (secure_uint32() as usize) % (i + 1);
        v.swap(i, pos);
    }
}

/// `td::Random::shuffle(v, rnd)`.
pub fn shuffle<T>(v: &mut [T], rnd: &mut impl FnMut() -> u64) {
    for i in 1..v.len() {
        let pos = (rnd() as usize) % (i + 1);
        v.swap(i, pos);
    }
}

/// `td::Random::Xorshift128plus` — a bit-exact port of the C++ class
/// (splitmix64 seeding + the V8 xorshift128+ core).
pub struct Xorshift128plus {
    seed: [u64; 2],
}

fn splitmix64(seed: &mut u64) -> u64 {
    *seed = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *seed;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

impl Xorshift128plus {
    /// `Xorshift128plus(uint64 seed)`: seeds via splitmix64.
    pub fn from_seed(seed: u64) -> Self {
        let mut seed = seed;
        let a = splitmix64(&mut seed);
        let b = splitmix64(&mut seed);
        Self { seed: [a, b] }
    }

    /// `Xorshift128plus(uint64 seed_a, uint64 seed_b)`.
    pub fn from_seeds(seed_a: u64, seed_b: u64) -> Self {
        Self {
            seed: [seed_a, seed_b],
        }
    }

    pub fn next_u64(&mut self) -> u64 {
        let x = self.seed[0];
        let y = self.seed[1];
        self.seed[0] = y;
        let x = x ^ (x << 23);
        self.seed[1] = x ^ y ^ (x >> 17) ^ (y >> 26);
        self.seed[1].wrapping_add(y)
    }

    /// `Xorshift128plus::fast(min_value, max_value)` — inclusive bounds,
    /// `min` bumped by one for the full `i32` range (C++ overflow guard).
    pub fn fast(&mut self, min_value: i32, max_value: i32) -> i32 {
        let mut min_value = min_value;
        if min_value == i32::MIN && max_value == i32::MAX {
            min_value += 1;
        }
        debug_assert!(min_value <= max_value);
        let range = i64::from(max_value) - i64::from(min_value) + 1;
        min_value.wrapping_add((self.next_u64() % range as u64) as i64 as i32)
    }

    /// `Xorshift128plus::fast64(min_value, max_value)`.
    pub fn fast64(&mut self, min_value: i64, max_value: i64) -> i64 {
        let mut min_value = min_value;
        if min_value == i64::MIN && max_value == i64::MAX {
            min_value += 1;
        }
        let range = (max_value as u64)
            .wrapping_sub(min_value as u64)
            .wrapping_add(1);
        (self.next_u64() % range).wrapping_add(min_value as u64) as i64
    }

    /// `Xorshift128plus::bytes(dest)`: little-endian bytes of successive
    /// outputs.
    pub fn bytes(&mut self, dest: &mut [u8]) {
        let mut cnt = 0;
        let mut buf = 0u64;
        for c in dest {
            if cnt == 0 {
                buf = self.next_u64();
                cnt = 8;
            }
            cnt -= 1;
            *c = (buf & 255) as u8;
            buf >>= 8;
        }
    }
}

thread_local! {
    static FAST_RNG: RefCell<Option<Xorshift128plus>> = const { RefCell::new(None) };
}

fn with_fast_rng<R>(f: impl FnOnce(&mut Xorshift128plus) -> R) -> R {
    FAST_RNG.with(|cell| {
        let mut cell = cell.borrow_mut();
        let rng = cell.get_or_insert_with(|| {
            // `std::mt19937` in C++ is seeded from 12 `random_device` draws;
            // the OS CSPRNG replaces both here.
            Xorshift128plus::from_seeds(secure_uint64(), secure_uint64())
        });
        f(rng)
    })
}

/// `td::Random::fast_uint64`.
pub fn fast_uint64() -> u64 {
    with_fast_rng(Xorshift128plus::next_u64)
}

/// `td::Random::fast_uint32`.
pub fn fast_uint32() -> u32 {
    fast_uint64() as u32
}

/// `td::Random::fast(min_value, max_value)` — inclusive bounds, not uniform
/// like in C++ (modulo rejection-free mapping).
pub fn fast(min_value: i32, max_value: i32) -> i32 {
    let mut min_value = min_value;
    if min_value == i32::MIN && max_value == i32::MAX {
        min_value += 1;
    }
    debug_assert!(min_value <= max_value);
    let range = i64::from(max_value) - i64::from(min_value) + 1;
    min_value + (u64::from(fast_uint32()) % range as u64) as i64 as i32
}

/// `td::Random::fast(min_value, max_value)` for doubles.
pub fn fast_f64(min_value: f64, max_value: f64) -> f64 {
    debug_assert!(min_value <= max_value);
    min_value + f64::from(fast_uint32()) / f64::from(u32::MAX) * (max_value - min_value)
}

/// `td::Random::fast_bool`.
pub fn fast_bool() -> bool {
    fast_uint32() & 1 != 0
}

/// `td::rand_string(from, to, len)` with the global fast RNG (the S05
/// harness provides the rng-injected version).
pub fn rand_string(from: i32, to: i32, len: usize) -> Vec<u8> {
    crate::tests::rand_string_with(&mut |from, to| fast(from, to), from, to, len)
}

/// `td::rand_split(data)` with the global fast RNG.
pub fn rand_split(data: &[u8]) -> Vec<Vec<u8>> {
    crate::tests::rand_split_with(&mut |from, to| fast(from, to), data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xorshift128plus_from_seed_matches_reference() {
        let mut rng = Xorshift128plus::from_seed(12345);
        assert_eq!(
            rng.seed,
            [0x2211_8258_A9D1_11A0, 0x346E_DCE5_F713_F8ED],
            "splitmix64 seeding"
        );
        let expected = [
            0x6E9A_8E00_1731_E5C5,
            0xB6EA_7861_A20C_7F4C,
            0x4843_EA70_BFA3_1BB6,
            0xE080_9FD3_BCCA_15D0,
            0x60DC_26F9_A0DE_6D97,
        ];
        for (i, want) in expected.iter().enumerate() {
            assert_eq!(rng.next_u64(), *want, "output {i}");
        }
    }

    #[test]
    fn xorshift128plus_from_seeds_matches_reference() {
        let mut rng = Xorshift128plus::from_seeds(1, 2);
        assert_eq!(rng.next_u64(), 0x800045);
        assert_eq!(rng.next_u64(), 0x2000104);
        assert_eq!(rng.next_u64(), 0x4000020010C3);
    }

    #[test]
    fn fast_range_is_inclusive_and_bounded() {
        let mut rng = Xorshift128plus::from_seed(12345);
        let values: Vec<i32> = (0..6).map(|_| rng.fast(0, 5)).collect();
        assert_eq!(values, [1, 2, 4, 0, 3, 4]);

        for _ in 0..1000 {
            let v = rng.fast(-3, 3);
            assert!((-3..=3).contains(&v));
        }
        assert_eq!(rng.fast(7, 7), 7, "degenerate range");
    }

    #[test]
    fn fast_does_not_overflow_on_full_range() {
        let mut rng = Xorshift128plus::from_seed(7);
        for _ in 0..100 {
            let _ = rng.fast(i32::MIN, i32::MAX);
        }
    }

    #[test]
    fn fast64_handles_negative_bounds() {
        let mut rng = Xorshift128plus::from_seed(7);
        for _ in 0..1000 {
            let v = rng.fast64(-5, 5);
            assert!((-5..=5).contains(&v));
        }
        let mut rng = Xorshift128plus::from_seed(7);
        for _ in 0..100 {
            let _ = rng.fast64(i64::MIN, i64::MAX);
        }
    }

    #[test]
    fn bytes_emit_little_endian_outputs() {
        let mut rng = Xorshift128plus::from_seed(12345);
        let mut buf = [0u8; 16];
        rng.bytes(&mut buf);
        assert_eq!(
            buf,
            [
                197, 229, 49, 23, 0, 142, 154, 110, 76, 127, 12, 162, 97, 120, 234, 182,
            ]
        );
        let mut empty = [0u8; 0];
        rng.bytes(&mut empty);
    }

    #[test]
    fn secure_bytes_fills_and_types_wrap() {
        let mut buf = [0u8; 64];
        secure_bytes(&mut buf);
        assert!(buf.iter().any(|&b| b != 0), "all-zero CSPRNG output");
        let _ = secure_uint32();
        let _ = secure_uint64();
        let _ = secure_int32();
        let _ = secure_int64();
        add_seed(b"entropy", 1.0);
        secure_cleanup();
    }

    #[test]
    fn shuffle_permutes_with_injected_rng() {
        let mut values = vec![10u32, 11, 12, 13, 14];
        let original = values.clone();
        let mut queue = [1u64, 2, 3, 4].into_iter();
        shuffle(&mut values, &mut || queue.next().unwrap());
        assert_eq!(values.len(), original.len());
        let mut sorted = values.clone();
        sorted.sort_unstable();
        let mut sorted_original = original.clone();
        sorted_original.sort_unstable();
        assert_eq!(sorted, sorted_original);
    }

    #[test]
    fn fast_rng_global_helpers() {
        let s = rand_string(b'a' as i32, b'z' as i32, 100);
        assert_eq!(s.len(), 100);
        assert!(s.iter().all(|&b| b.is_ascii_lowercase()));
        let data: Vec<u8> = (0..=255u8).cycle().take(300).collect();
        let chunks = rand_split(&data);
        assert!(chunks.iter().flatten().copied().eq(data.iter().copied()));
        let _ = fast_bool();
        let _ = fast_f64(0.0, 1.0);
    }
}
