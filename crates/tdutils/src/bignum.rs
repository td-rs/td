//! Port of `td/utils/BigNum.h` + `td/utils/BigNum.cpp` on top of
//! `num-bigint` (replacing OpenSSL's `BIGNUM`). `BigNumContext` exists only
//! for API parity — the Rust backend needs no scratch context.

use crate::status::{Result, Status};
use num_bigint::{BigInt, BigUint, Sign};
use num_integer::Integer;
use num_traits::{One, ToPrimitive, Zero};

/// Limb budget of the Montgomery fast path (4,096-bit operands).
const MONT_LIMBS: usize = 64;

/// `base^exponent mod modulus` through `crypto-bigint`'s Montgomery
/// `MontyForm`, zero-padding each operand into the `LIMBS`-limb budget
/// (`from_le_slice` wants exactly that many bytes). The modulus must be
/// odd — callers gate on that. Written as per-width functions because
/// `MontyParams::new` needs `Concat`/`Split` bounds a generic `LIMBS`
/// cannot name.
macro_rules! montgomery_modpow_fn {
    ($name:ident, $LIMBS:literal) => {
        fn $name(base: &BigInt, exponent: &BigInt, modulus: &BigInt) -> BigInt {
            let to_padded = |value: &BigUint| -> Vec<u8> {
                let mut out = vec![0u8; $LIMBS * 8];
                let bytes = value.to_bytes_le();
                out[..bytes.len()].copy_from_slice(&bytes);
                out
            };
            let params = crypto_bigint::modular::MontyParams::<$LIMBS>::new(
                crypto_bigint::Odd::new(crypto_bigint::Uint::<$LIMBS>::from_le_slice(&to_padded(
                    modulus.magnitude(),
                )))
                .unwrap(),
            );
            let reduced = crypto_bigint::modular::MontyForm::<$LIMBS>::new(
                &crypto_bigint::Uint::<$LIMBS>::from_le_slice(&to_padded(
                    (base % modulus).magnitude(),
                )),
                params,
            )
            .pow(&crypto_bigint::Uint::<$LIMBS>::from_le_slice(&to_padded(
                exponent.magnitude(),
            )))
            .retrieve();
            BigUint::from_bytes_le(&reduced.to_le_bytes()).into()
        }
    };
}
montgomery_modpow_fn!(montgomery_modpow_32, 32);
montgomery_modpow_fn!(montgomery_modpow_64, 64);

/// `td::BigNumContext` — no-op scratch context kept for API parity.
#[derive(Clone, Copy, Debug, Default)]
pub struct BigNumContext;

impl BigNumContext {
    pub fn new() -> Self {
        Self
    }
}

/// `td::BigNum` — an arbitrary-precision integer. Internally a [`BigInt`]
/// (OpenSSL's `BIGNUM` can hold a sign, though tdlib uses non-negative
/// values).
#[derive(Clone, Debug)]
pub struct BigNum {
    value: BigInt,
}

impl BigNum {
    /// `td::BigNum::BigNum()` — zero.
    pub fn new() -> Self {
        Self {
            value: BigInt::zero(),
        }
    }

    /// `td::BigNum::from_binary(str)` — big-endian bytes.
    pub fn from_binary(bytes: &[u8]) -> Self {
        Self {
            value: BigInt::from_bytes_be(Sign::Plus, bytes),
        }
    }

    /// `td::BigNum::from_le_binary(str)` — little-endian bytes.
    pub fn from_le_binary(bytes: &[u8]) -> Self {
        Self {
            value: BigInt::from_bytes_le(Sign::Plus, bytes),
        }
    }

    /// `td::BigNum::from_decimal(str)`.
    pub fn from_decimal(str: &str) -> Result<Self> {
        // BigInt::from_str rejects anything but an optional sign and digits,
        // exactly like BN_dec2bn consuming the whole string.
        match str.parse::<BigInt>() {
            Ok(value) => Result::Ok(Self { value }),
            Err(_) => Result::Err(Status::error_message(&format!(
                "Failed to parse \"{str}\" as BigNum"
            ))),
        }
    }

    /// `td::BigNum::from_hex(str)`.
    pub fn from_hex(str: &str) -> Result<Self> {
        let clean = str.strip_prefix("0x").unwrap_or(str);
        match BigInt::parse_bytes(clean.as_bytes(), 16) {
            Some(value) if !clean.is_empty() => {
                // BN_hex2bn consumes the whole string or errors; verify the
                // round-trip length to reject trailing garbage.
                let reparsed = format!("{value:x}");
                if reparsed.len() == clean.trim_start_matches('0').len().max(1) {
                    Result::Ok(Self { value })
                } else {
                    Result::Err(Status::error_message(&format!(
                        "Failed to parse \"{str}\" as hexadecimal BigNum"
                    )))
                }
            }
            _ => Result::Err(Status::error_message(&format!(
                "Failed to parse \"{str}\" as hexadecimal BigNum"
            ))),
        }
    }

    /// `td::BigNum::set_value(new_value)`.
    pub fn set_value(&mut self, new_value: u32) {
        self.value = BigInt::from(new_value);
    }

    /// `td::BigNum::get_num_bits()`.
    pub fn get_num_bits(&self) -> usize {
        self.value.bits() as usize
    }

    /// `td::BigNum::get_num_bytes()`.
    pub fn get_num_bytes(&self) -> usize {
        (self.value.bits() as usize).div_ceil(8)
    }

    /// `td::BigNum::set_bit(num)` — bit 0 is the LSB.
    pub fn set_bit(&mut self, num: usize) {
        self.value.set_bit(num as u64, true);
    }

    /// `td::BigNum::clear_bit(num)`.
    pub fn clear_bit(&mut self, num: usize) {
        self.value.set_bit(num as u64, false);
    }

    /// `td::BigNum::is_bit_set(num)`.
    pub fn is_bit_set(&self, num: usize) -> bool {
        self.value.bit(num as u64)
    }

    /// `td::BigNum::is_prime(context)` — Miller–Rabin via `num-prime`
    /// (trial division + strong pseudoprime rounds, like `BN_is_prime_ex`).
    pub fn is_prime(&self, _context: &mut BigNumContext) -> bool {
        let Some(value) = self.value.to_biguint() else {
            return false;
        };
        matches!(
            num_prime::nt_funcs::is_prime(&value, None),
            num_prime::Primality::Yes | num_prime::Primality::Probable(_)
        )
    }

    /// `td::BigNum::to_binary(exact_size)` — big-endian, left-padded with
    /// zeros when `exact_size` is given (which must be >= the natural size).
    pub fn to_binary(&self, exact_size: isize) -> Vec<u8> {
        let bytes = self.value.to_bytes_be().1;
        pad_bytes(bytes, exact_size)
    }

    /// `td::BigNum::to_le_binary(exact_size)` — little-endian, right-padded.
    pub fn to_le_binary(&self, exact_size: isize) -> Vec<u8> {
        let bytes = self.value.to_bytes_le().1;
        pad_bytes_le(bytes, exact_size)
    }

    /// `td::BigNum::to_decimal()`.
    pub fn to_decimal(&self) -> String {
        self.value.to_string()
    }

    /// `td::BigNum::operator+=(uint32)`.
    pub fn add_word(&mut self, value: u32) {
        self.value += BigInt::from(value);
    }

    /// `td::BigNum::operator-=(uint32)`.
    pub fn sub_word(&mut self, value: u32) {
        self.value -= BigInt::from(value);
    }

    /// `td::BigNum::operator*=(uint32)`.
    pub fn mul_word(&mut self, value: u32) {
        self.value *= BigInt::from(value);
    }

    /// `td::BigNum::operator/=(uint32)`.
    pub fn div_word(&mut self, value: u32) {
        assert!(value != 0, "division by zero");
        self.value /= BigInt::from(value);
    }

    /// `td::BigNum::operator%(uint32)`.
    pub fn mod_word(&self, value: u32) -> u32 {
        assert!(value != 0, "modulo by zero");
        (&self.value % BigInt::from(value))
            .to_u32()
            .expect("mod word fits u32")
    }

    /// `td::BigNum::random(r, bits, top, bottom)` — uniform in `bits` bits;
    /// `top = 0` forces the MSB, `top = 1` the top two bits, `bottom = 1`
    /// an odd result (the BN_rand semantics).
    pub fn random(bits: usize, top: i32, bottom: i32) -> Self {
        let mut value = BigUint::zero();
        let mut remaining = bits;
        while remaining > 0 {
            let chunk = remaining.min(64);
            let word = crate::random::secure_uint64();
            value <<= chunk;
            value |= BigUint::from(word >> (64 - chunk));
            remaining -= chunk;
        }
        if top == 0 || top == 1 {
            value.set_bit(bits as u64 - 1, true);
        }
        if top == 1 {
            value.set_bit(bits as u64 - 2, true);
        }
        if bottom == 1 {
            value.set_bit(0, true);
        }
        Self {
            value: BigInt::from_biguint(Sign::Plus, value),
        }
    }

    /// `td::BigNum::add(r, a, b)`.
    pub fn add(a: &BigNum, b: &BigNum) -> Self {
        Self {
            value: &a.value + &b.value,
        }
    }

    /// `td::BigNum::sub(r, a, b)`.
    pub fn sub(a: &BigNum, b: &BigNum) -> Self {
        Self {
            value: &a.value - &b.value,
        }
    }

    /// `td::BigNum::mul(r, a, b, context)`.
    pub fn mul(a: &BigNum, b: &BigNum, _context: &mut BigNumContext) -> Self {
        Self {
            value: &a.value * &b.value,
        }
    }

    /// `td::BigNum::mod_add(r, a, b, m, context)`.
    pub fn mod_add(a: &BigNum, b: &BigNum, m: &BigNum, _context: &mut BigNumContext) -> Self {
        Self {
            value: (&a.value + &b.value).mod_floor(&m.value),
        }
    }

    /// `td::BigNum::mod_sub(r, a, b, m, context)`.
    pub fn mod_sub(a: &BigNum, b: &BigNum, m: &BigNum, _context: &mut BigNumContext) -> Self {
        Self {
            value: (&a.value - &b.value).mod_floor(&m.value),
        }
    }

    /// `td::BigNum::mod_mul(r, a, b, m, context)`.
    pub fn mod_mul(a: &BigNum, b: &BigNum, m: &BigNum, _context: &mut BigNumContext) -> Self {
        Self {
            value: (&a.value * &b.value).mod_floor(&m.value),
        }
    }

    /// `td::BigNum::mod_inverse(a, m, context)` — fails when the inverse
    /// does not exist (gcd(a, m) != 1), like `BN_mod_inverse`.
    pub fn mod_inverse(a: &BigNum, m: &BigNum, _context: &mut BigNumContext) -> Result<Self> {
        let modulus = m.value.clone();
        let gcd = a.value.extended_gcd(&modulus);
        if gcd.gcd != BigInt::one() {
            return Result::Err(Status::error_message("Failed to compute modulo inverse"));
        }
        Result::Ok(Self {
            value: gcd.x.mod_floor(&modulus),
        })
    }

    /// `td::BigNum::div(quotient, remainder, dividend, divisor, context)`.
    pub fn div(
        dividend: &BigNum,
        divisor: &BigNum,
        _context: &mut BigNumContext,
    ) -> Result<(BigNum, BigNum)> {
        if divisor.value.is_zero() {
            return Result::Err(Status::error_message("Failed to compute quotient"));
        }
        let (quotient, remainder) = dividend.value.div_rem(&divisor.value);
        Result::Ok((Self { value: quotient }, Self { value: remainder }))
    }

    /// `td::BigNum::mod_exp(r, a, p, m, context)`.
    ///
    /// Montgomery fast path (`crypto-bigint`) for the DH-shaped inputs —
    /// odd modulus with base/exponent/modulus within the 4,096-bit limb
    /// budget, base pre-reduced. That is exactly the `g^x mod p` shape of
    /// the handshake (S59/S60), where the S79 gate measured num-bigint's
    /// generic `modpow` at ~3.9× slower than OpenSSL's Montgomery
    /// arithmetic. Everything else falls back to `modpow` verbatim. The
    /// vartime form matches the C++ (tdlib never sets `BN_FLG_CONSTTIME`).
    pub fn mod_exp(a: &BigNum, p: &BigNum, m: &BigNum, _context: &mut BigNumContext) -> Self {
        let eligible = m.value.bits() <= (MONT_LIMBS * 64) as u64
            && p.value.bits() <= (MONT_LIMBS * 64) as u64
            && a.value.bits() <= (MONT_LIMBS * 64) as u64
            && !m.value.is_zero()
            && m.value.bit(0);
        if !eligible {
            return Self {
                value: a.value.modpow(&p.value, &m.value),
            };
        }
        // The Montgomery working width must match the modulus width: the
        // limb count drives every multiplication, so a 2,048-bit prime in a
        // 4,096-bit budget pays ~4x for nothing (the first S79 cut). 32
        // limbs covers the Telegram primes; 64 is the fallback for larger.
        if m.value.bits() <= 2048 {
            return Self {
                value: montgomery_modpow_32(&a.value, &p.value, &m.value),
            };
        }
        Self {
            value: montgomery_modpow_64(&a.value, &p.value, &m.value),
        }
    }

    /// `td::BigNum::gcd(r, a, b, context)`.
    pub fn gcd(a: &BigNum, b: &BigNum, _context: &mut BigNumContext) -> Self {
        Self {
            value: a.value.gcd(&b.value),
        }
    }

    /// `td::BigNum::compare(a, b)` — the sign of `a - b`.
    pub fn compare(a: &BigNum, b: &BigNum) -> i32 {
        a.value.cmp(&b.value) as i32
    }
}

impl Default for BigNum {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for BigNum {
    /// `td::operator<<(StringBuilder, BigNum)` — decimal.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_decimal())
    }
}

fn pad_bytes(bytes: Vec<u8>, exact_size: isize) -> Vec<u8> {
    let num_size = bytes.len();
    let exact_size = if exact_size == -1 {
        num_size
    } else {
        assert!(
            exact_size >= 0 && exact_size as usize >= num_size,
            "exact_size too small"
        );
        exact_size as usize
    };
    let mut result = vec![0u8; exact_size];
    result[exact_size - num_size..].copy_from_slice(&bytes);
    result
}

fn pad_bytes_le(bytes: Vec<u8>, exact_size: isize) -> Vec<u8> {
    let num_size = bytes.len();
    let exact_size = if exact_size == -1 {
        num_size
    } else {
        assert!(
            exact_size >= 0 && exact_size as usize >= num_size,
            "exact_size too small"
        );
        exact_size as usize
    };
    let mut result = vec![0u8; exact_size];
    result[..num_size].copy_from_slice(&bytes);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 3526 group 14 (2048-bit MODP) prime — the same constant the
    /// `telegram_api.tl` scheme's `dh_prime` uses.
    const P_HEX: &str = "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74020BBEA63B139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F14374FE1356D6D51C245E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7EDEE386BFB5A899FA5AE9F24117C4B1FE649286651ECE45B3DC2007CB8A163BF0598DA48361C55D39A69163FA8FD24CF5F83655D23DCA3AD961C62F356208552BB9ED529077096966D670C354E4ABC9804F1746C08CA18217C32905E462E36CE3BE39E772C180E86039B2783A2EC07A28FB5C55DF06F4C52C9DE2BCBF6955817183995497CEA956AE515D2261898FA051015728E5A8AACAA68FFFFFFFFFFFFFFFF";

    fn hex(bytes: &[u8]) -> String {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut out = String::with_capacity(bytes.len() * 2);
        for &byte in bytes {
            out.push(DIGITS[(byte >> 4) as usize] as char);
            out.push(DIGITS[(byte & 0xf) as usize] as char);
        }
        out
    }

    #[test]
    fn parse_and_format() {
        let p = BigNum::from_hex(P_HEX).unwrap();
        assert_eq!(p.get_num_bits(), 2048);
        assert_eq!(p.get_num_bytes(), 256);
        assert_eq!(p.to_decimal().len(), 617, "2048-bit prime decimal length");

        let n = BigNum::from_decimal("123456789012345678901234567890").unwrap();
        assert_eq!(n.to_decimal(), "123456789012345678901234567890");
        assert_eq!(
            hex(&n.to_binary(-1)),
            "018ee90ff6c373e0ee4e3f0ad2",
            "big-endian bytes"
        );
        assert_eq!(hex(&n.to_le_binary(-1)), "d20a3f4eeee073c3f60fe98e01");

        // exact-size padding
        assert_eq!(n.to_binary(16).len(), 16);
        assert_eq!(
            n.to_binary(16)[..16 - n.get_num_bytes()],
            vec![0u8; 16 - n.get_num_bytes()][..]
        );
        assert_eq!(n.to_le_binary(20).len(), 20);

        assert!(BigNum::from_decimal("12x3").is_error());
        assert!(BigNum::from_hex("zz").is_error());
        assert!(BigNum::from_decimal("").is_error());
        assert_eq!(
            BigNum::from_decimal("12x3").error().message(),
            "Failed to parse \"12x3\" as BigNum"
        );
    }

    #[test]
    fn dh_mod_exp_vectors() {
        let mut context = BigNumContext::new();
        let p = BigNum::from_hex(P_HEX).unwrap();
        let g = {
            let mut g = BigNum::new();
            g.set_value(2);
            g
        };
        let a = BigNum::from_hex(&"a1b2c3d4".repeat(16)).unwrap();
        let b = BigNum::from_hex(&"e5f6a7b8".repeat(16)).unwrap();

        // The Montgomery fast path must agree with num-bigint's modpow
        // byte-exactly on random inputs before the DH-shaped cases below.
        let mut seed = 0x5eed_u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for case in 0..24 {
            let modulus = BigNum::from_binary(
                &(0..64)
                    .map(|_| (next() & 0xff) as u8)
                    .chain([1u8]) // odd
                    .collect::<Vec<u8>>(),
            );
            let base =
                BigNum::from_binary(&(0..32).map(|_| (next() & 0xff) as u8).collect::<Vec<u8>>());
            let exponent =
                BigNum::from_binary(&(0..32).map(|_| (next() & 0xff) as u8).collect::<Vec<u8>>());
            let fast = BigNum::mod_exp(&base, &exponent, &modulus, &mut context);
            let reference = base.value.modpow(&exponent.value, &modulus.value);
            assert_eq!(
                fast.value, reference,
                "Montgomery fast path diverged on case {case}"
            );
        }

        let a_public = BigNum::mod_exp(&g, &a, &p, &mut context);
        let b_public = BigNum::mod_exp(&g, &b, &p, &mut context);
        let shared_a = BigNum::mod_exp(&b_public, &a, &p, &mut context);
        let shared_b = BigNum::mod_exp(&a_public, &b, &p, &mut context);
        assert_eq!(shared_a.to_decimal(), shared_b.to_decimal());

        // Reference values computed independently with Python's pow().
        assert_eq!(
            hex(&a_public.to_binary(-1)),
            "f85d34a0f19959ea2006919e700606a20d428d53f693b2e0f3d545613b568bee7cb1e3765c5962b67c9f87c54084a4c13acb0a96db49a9feca50d936b4ee4566c09f9e3ced7ace31842c36a61826c2da5476885685224c433477d7fafde12c042fa6c7e2a381b3f6409a2b8bc7cad4f39eabd53bd71ffbd5d4d90c57a9847054c205519742bf815dda8c9bb7505b0ce85e4741c208ed6815b26f56326c85568b89c3ef3299e3bc14607d764d0e68b8864a5fdb181e1d4275cf054be482322e296d9990108605ce5f06f19f4db7a5644d417457c05f009253dcfaa2695a41e31254aed8b849cc787e2370e842058061cf9f2469365d5d42068ef7816af3eee9f1"
        );
        assert_eq!(
            hex(&shared_a.to_binary(-1)),
            "e0d7080bbee8d52035d0f2d42ff3c89fd6ed8350217a22747ef52a7ca13ddb1b9565ddaf11bdb894f53dfadd15632605a4aa74282cde2ae20d89e50cd1197dff646a6944a3437740d04d8d26d8342810890ea39f24440e33b1e9728687d89e2cec943468e2132f56e7a3c6120310c2511cd80b1c0d2b295cd8d59d31ff3edd6f250d0b38943a4741879c132c6308bedf56e85d78d3f55fd09e6c2141a613483ecc7d3d1cac438fc3a08d815a6d79bdc577d4509f34e971e522197db8982788849fb84d119ca28694ef36ce500d4c7314191b939660b98eb80604cb23e55f471b6cfd3df6f824751ccec3449cef81c3ef44539caf1686f384d7a6524b37d1ec1b"
        );
    }

    #[test]
    fn modular_arithmetic() {
        let mut context = BigNumContext::new();
        let p = BigNum::from_hex(P_HEX).unwrap();
        let three = {
            let mut v = BigNum::new();
            v.set_value(3);
            v
        };
        let inverse = BigNum::mod_inverse(&three, &p, &mut context).unwrap();
        let product = BigNum::mod_mul(&three, &inverse, &p, &mut context);
        let one = {
            let mut v = BigNum::new();
            v.set_value(1);
            v
        };
        assert_eq!(BigNum::compare(&product, &one), 0, "3 * 3^-1 ≡ 1 (mod p)");

        // 2^3072 mod p = 2^1024 (since 2^2048 ≡ 2^1024 + ... check via pow identity: 2^2048 mod p = 2^1024 - 1)
        let two = {
            let mut v = BigNum::new();
            v.set_value(2);
            v
        };
        let e1 = {
            let mut v = BigNum::new();
            v.set_value(2048);
            v
        };
        let e2 = {
            let mut v = BigNum::new();
            v.set_value(1024);
            v
        };
        let pow1 = BigNum::mod_exp(&two, &e1, &p, &mut context);
        let pow2 = BigNum::mod_exp(&two, &e2, &p, &mut context);
        // (2^1024)^2 mod p via mod_mul must equal 2^2048 mod p via mod_exp.
        let squared = BigNum::mod_mul(&pow2, &pow2, &p, &mut context);
        assert_eq!(
            BigNum::compare(&pow1, &squared),
            0,
            "mod_mul(g^x, g^x) == mod_exp(g, 2x)"
        );

        assert!(BigNum::mod_inverse(&BigNum::new(), &p, &mut context).is_error());
    }

    #[test]
    fn word_ops_and_bits() {
        let mut n = BigNum::new();
        n.add_word(5);
        n.mul_word(3);
        n.add_word(2);
        assert_eq!(n.to_decimal(), "17");
        n.div_word(2);
        assert_eq!(n.to_decimal(), "8");
        n.sub_word(3);
        assert_eq!(n.to_decimal(), "5");
        assert_eq!(n.mod_word(3), 2);

        n.set_value(0b1010);
        assert!(n.is_bit_set(1));
        assert!(!n.is_bit_set(0));
        assert!(!n.is_bit_set(2));
        n.clear_bit(1);
        assert_eq!(n.to_decimal(), "8");
        n.set_bit(2);
        assert_eq!(n.to_decimal(), "12");
    }

    #[test]
    fn primality() {
        let mut context = BigNumContext::new();
        let p = BigNum::from_hex(P_HEX).unwrap();
        assert!(p.is_prime(&mut context), "RFC 3526 group 14 prime");

        let composite = {
            let mut v = BigNum::new();
            v.set_value(100_000);
            v
        };
        assert!(!composite.is_prime(&mut context));

        let prime17 = {
            let mut v = BigNum::new();
            v.set_value(17);
            v
        };
        assert!(prime17.is_prime(&mut context));
    }

    #[test]
    fn div_and_gcd() {
        let mut context = BigNumContext::new();
        let a = BigNum::from_decimal("123456789").unwrap();
        let b = BigNum::from_decimal("1234").unwrap();
        let (q, r) = BigNum::div(&a, &b, &mut context).unwrap();
        assert_eq!(q.to_decimal(), "100046");
        assert_eq!(r.to_decimal(), "25");

        let gcd = BigNum::gcd(&a, &b, &mut context);
        assert_eq!(gcd.to_decimal(), "1");

        let c = BigNum::from_decimal("12").unwrap();
        let d = BigNum::from_decimal("18").unwrap();
        assert_eq!(BigNum::gcd(&c, &d, &mut context).to_decimal(), "6");

        let zero = BigNum::new();
        assert!(BigNum::div(&a, &zero, &mut context).is_error());
    }

    #[test]
    fn random_respects_top_and_bottom() {
        for _ in 0..16 {
            let r = BigNum::random(128, 0, 1);
            assert_eq!(r.get_num_bits(), 128, "top=0 keeps the MSB");
            assert!(r.is_bit_set(0), "bottom=1 forces odd");
            let r = BigNum::random(129, 1, 0);
            assert!(
                r.is_bit_set(128) && r.is_bit_set(127),
                "top=1 forces two MSBs"
            );
        }
    }
}
