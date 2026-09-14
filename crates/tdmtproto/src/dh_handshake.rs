//! Port of `td/mtproto/DhHandshake.{h,cpp}` (S59): the Diffie-Hellman half
//! of the auth-key generation — 2048-bit safe-prime validation, g_a/g_b
//! bounds checks and the auth key derivation.

use crate::dh_callback::DhCallback;
use crate::mtproto_api::tl;
use tdutils::bignum::{BigNum, BigNumContext};
use tdutils::crypto::{sha1, sha256};
use tdutils::status::Status;

/// `td::mtproto::DhHandshake`.
pub struct DhHandshake {
    prime_str: Vec<u8>,
    prime: BigNum,
    g: BigNum,
    g_int: i32,
    b: BigNum,
    g_b: BigNum,
    g_a: BigNum,
    g_a_hash: Vec<u8>,
    has_g_a_hash: bool,
    ok_g_a_hash: bool,
    has_config: bool,
    has_g_a: bool,
    ctx: BigNumContext,
}

/// `DhHandshake::Flags { HasConfig = 1, HasGA = 2 }`.
const HAS_CONFIG: i32 = 1;
const HAS_GA: i32 = 2;

impl DhHandshake {
    pub fn new() -> Self {
        Self {
            prime_str: Vec::new(),
            prime: BigNum::new(),
            g: BigNum::new(),
            g_int: 0,
            b: BigNum::new(),
            g_b: BigNum::new(),
            g_a: BigNum::new(),
            g_a_hash: Vec::new(),
            has_g_a_hash: false,
            ok_g_a_hash: false,
            has_config: false,
            has_g_a: false,
            ctx: BigNumContext::new(),
        }
    }

    /// `DhHandshake::set_config`: stores the prime and generates the
    /// random 2048-bit `b` and `g_b = g^b mod p`.
    pub fn set_config(&mut self, g_int: i32, prime_str: &[u8]) {
        self.has_config = true;
        self.prime = BigNum::from_binary(prime_str);
        self.prime_str = prime_str.to_vec();

        self.b = BigNum::new();
        self.g_b = BigNum::new();

        self.b = BigNum::random(2048, -1, 0);

        self.g_int = g_int;
        self.g.set_value(g_int as u32);

        self.g_b = BigNum::mod_exp(&self.g, &self.b, &self.prime, &mut self.ctx);
    }

    /// `DhHandshake::check_config` (static).
    pub fn check_config(g_int: i32, prime_str: &[u8], callback: &dyn DhCallback) -> Status {
        let mut ctx = BigNumContext::new();
        let prime = BigNum::from_binary(prime_str);
        Self::check_config_impl(prime_str, &prime, g_int, &mut ctx, callback)
    }

    fn check_config_impl(
        prime_str: &[u8],
        prime: &BigNum,
        g_int: i32,
        ctx: &mut BigNumContext,
        callback: &dyn DhCallback,
    ) -> Status {
        // check that 2^2047 <= p < 2^2048
        if prime.get_num_bits() != 2048 {
            return Status::error_message("p is not 2048-bit number");
        }

        // g generates a cyclic subgroup of prime order (p - 1) / 2, i.e. is a quadratic residue mod p.
        // Since g is always equal to 2, 3, 4, 5, 6 or 7, this is easily done using quadratic reciprocity law,
        // yielding a simple condition on
        // * p mod 4g - namely, p mod 8 = 7 for g = 2; p mod 3 = 2 for g = 3;
        // * no extra condition for g = 4;
        // * p mod 5 = 1 or 4 for g = 5;
        // * p mod 24 = 19 or 23 for g = 6;
        // * p mod 7 = 3, 5 or 6 for g = 7.
        let mod_ok = match g_int {
            2 => prime.mod_word(8) == 7,
            3 => prime.mod_word(3) == 2,
            4 => true,
            5 => {
                let mod_r = prime.mod_word(5);
                mod_r == 1 || mod_r == 4
            }
            6 => {
                let mod_r = prime.mod_word(24);
                mod_r == 19 || mod_r == 23
            }
            7 => {
                let mod_r = prime.mod_word(7);
                mod_r == 3 || mod_r == 5 || mod_r == 6
            }
            _ => false,
        };
        if !mod_ok {
            return Status::error_message("Bad prime mod 4g");
        }

        // check whether p is a safe prime (meaning that both p and (p - 1) / 2 are prime)
        let is_good_prime = callback.is_good_prime(prime_str);
        if is_good_prime != -1 {
            return if is_good_prime != 0 {
                Status::ok()
            } else {
                Status::error_message("p or (p - 1) / 2 is not a prime number")
            };
        }
        if !prime.is_prime(ctx) {
            callback.add_bad_prime(prime_str);
            return Status::error_message("p is not a prime number");
        }

        let mut half_prime = prime.clone();
        half_prime.sub_word(1);
        half_prime.div_word(2);
        if !half_prime.is_prime(ctx) {
            callback.add_bad_prime(prime_str);
            return Status::error_message("(p - 1) / 2 is not a prime number");
        }
        callback.add_good_prime(prime_str);
        Status::ok()
    }

    /// `DhHandshake::dh_check` (static): both g_a and g_b must lie in
    /// `[2^{2048-64}, dh_prime - 2^{2048-64}]`.
    fn dh_check(prime: &BigNum, g_a: &BigNum, g_b: &BigNum) -> Status {
        assert_eq!(prime.get_num_bits(), 2048);
        let mut left = BigNum::new();
        left.set_value(0);
        left.set_bit(2048 - 64);

        let right = BigNum::sub(prime, &left);

        if BigNum::compare(&left, g_a) > 0
            || BigNum::compare(g_a, &right) > 0
            || BigNum::compare(&left, g_b) > 0
            || BigNum::compare(g_b, &right) > 0
        {
            return Status::error_message(
                "g^a or g^b is not between 2^{2048-64} and dh_prime - 2^{2048-64}",
            );
        }

        Status::ok()
    }

    pub fn has_config(&self) -> bool {
        self.has_config
    }

    /// `DhHandshake::set_g_a_hash`.
    pub fn set_g_a_hash(&mut self, g_a_hash: &[u8]) {
        self.has_g_a_hash = true;
        self.ok_g_a_hash = false;
        assert!(!self.has_g_a);
        self.g_a_hash = g_a_hash.to_vec();
    }

    /// `DhHandshake::set_g_a` — verifies the previously set SHA-256 hash.
    pub fn set_g_a(&mut self, g_a_str: &[u8]) {
        self.has_g_a = true;
        if self.has_g_a_hash {
            let g_a_hash = sha256(g_a_str);
            self.ok_g_a_hash = g_a_hash.as_slice() == self.g_a_hash.as_slice();
        }
        self.g_a = BigNum::from_binary(g_a_str);
    }

    pub fn has_g_a(&self) -> bool {
        self.has_g_a
    }

    pub fn get_g_a(&self) -> Vec<u8> {
        assert!(self.has_g_a);
        self.g_a.to_binary(-1)
    }

    pub fn get_g_b(&self) -> Vec<u8> {
        assert!(self.has_config);
        self.g_b.to_binary(-1)
    }

    pub fn get_g_b_hash(&self) -> [u8; 32] {
        sha256(&self.get_g_b())
    }

    /// `DhHandshake::run_checks`.
    pub fn run_checks(&mut self, skip_config_check: bool, callback: &dyn DhCallback) -> Status {
        assert!(self.has_g_a && self.has_config);

        if self.has_g_a_hash && !self.ok_g_a_hash {
            return Status::error_message("g_a_hash mismatch");
        }

        if !skip_config_check {
            let status = Self::check_config_impl(
                &self.prime_str,
                &self.prime,
                self.g_int,
                &mut self.ctx,
                callback,
            );
            if status.is_error() {
                return status;
            }
        }

        Self::dh_check(&self.prime, &self.g_a, &self.g_b)
    }

    pub fn get_g(&self) -> BigNum {
        assert!(self.has_config);
        self.g.clone()
    }

    pub fn get_p(&self) -> BigNum {
        assert!(self.has_config);
        self.prime.clone()
    }

    pub fn get_b(&self) -> BigNum {
        assert!(self.has_config);
        self.b.clone()
    }

    /// `DhHandshake::get_g_ab` — `g_a^b mod p`.
    pub fn get_g_ab(&mut self) -> BigNum {
        assert!(self.has_g_a && self.has_config);
        BigNum::mod_exp(&self.g_a, &self.b, &self.prime, &mut self.ctx)
    }

    /// `DhHandshake::gen_key`: the 256-byte auth key and its id.
    pub fn gen_key(&mut self) -> (i64, Vec<u8>) {
        let key = self.get_g_ab().to_binary(2048 / 8);
        let key_id = Self::calc_key_id(&key);
        (key_id, key)
    }

    /// `DhHandshake::calc_key_id` (static): little-endian i64 of
    /// `SHA-1(auth_key)[12..20]`.
    pub fn calc_key_id(auth_key: &[u8]) -> i64 {
        let auth_key_sha1 = sha1(auth_key);
        i64::from_le_bytes(auth_key_sha1[12..20].try_into().expect("8 bytes"))
    }

    /// `DhHandshake::store` — the TL binary state (used by the S59 oracle
    /// and the S60 handshake persistence).
    pub fn store(&self, out: &mut Vec<u8>) {
        let mut flags = 0;
        if self.has_config {
            flags |= HAS_CONFIG;
        }
        if self.has_g_a {
            flags |= HAS_GA;
        }
        tl::__i32(out, flags);

        if self.has_config {
            // prime_, prime_str_, b_, g_, g_int_, g_b_
            tl::__string(out, &self.prime_str);
            tl::__string(out, self.b.to_binary(-1).as_slice());
            tl::__i32(out, self.g_int);
            tl::__string(out, self.g_b.to_binary(-1).as_slice());
        }
        if self.has_g_a {
            tl::__string(out, self.g_a.to_binary(-1).as_slice());
        }
    }

    /// `DhHandshake::parse`.
    pub fn parse(r: &mut tl::Reader<'_>) -> Result<Self, &'static str> {
        let flags = r.i32()?;
        let has_config = flags & HAS_CONFIG != 0;
        let has_g_a = flags & HAS_GA != 0;
        let (prime_str, prime, b, g, g_int, g_b) = if has_config {
            let prime_str = r.bytes()?;
            let prime = BigNum::from_binary(&prime_str);
            let b = BigNum::from_binary(&r.bytes()?);
            let g_int = r.i32()?;
            let mut g = BigNum::new();
            g.set_value(g_int as u32);
            let g_b = BigNum::from_binary(&r.bytes()?);
            (prime_str, prime, b, g, g_int, g_b)
        } else {
            let prime_str = Vec::new();
            let prime = BigNum::new();
            let b = BigNum::new();
            let mut g = BigNum::new();
            g.set_value(0);
            (prime_str, prime, b, g, 0, BigNum::new())
        };
        let g_a = if has_g_a {
            BigNum::from_binary(&r.bytes()?)
        } else {
            BigNum::new()
        };
        Ok(Self {
            prime_str,
            prime,
            g,
            g_int,
            b,
            g_b,
            g_a,
            g_a_hash: Vec::new(),
            has_g_a_hash: false,
            ok_g_a_hash: false,
            has_config,
            has_g_a,
            ctx: BigNumContext::new(),
        })
    }
}

impl Default for DhHandshake {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The standard Telegram 2048-bit safe prime
    /// (`td/telegram/DhCache.cpp` built-in good prime).
    const TG_DH_PRIME_HEX: &str = "c71caeb9c6b1c9048e6c522f70f13f73980d40238e3e21c14934d037563d930f48198a0aa7c14058229493d22530f4dbfa336f6e0ac925139543aed44cce7c3720fd51f69458705ac68cd4fe6b6b13abdc9746512969328454f18faf8c595f642477fe96bb2a941d5bcd1d4ac8cc49880708fa9b378e3c4f3a9060bee67cf9a4a4a695811051907e162753b56b0f6b410dba74d8a84b2a14b3144e0ef1284754fd17ed950d5965b4b9dd46582db1178d169c6bc465b0d6ff9ca3928fef5b9ae4e418fc15e83ebea0f87fa9ff5eed70050ded2849f47bf959d956850ce929851f0d8115f635b105ee2e4e15d04b2454bf6f4fadf034b10403119cd8e3b92fcc5b";

    fn tg_prime() -> Vec<u8> {
        crate::utils::hex_decode(TG_DH_PRIME_HEX.as_bytes()).unwrap()
    }

    struct NoCallback;
    impl DhCallback for NoCallback {
        fn is_good_prime(&self, _prime_str: &[u8]) -> i32 {
            1 // the Telegram prime is known-good; skip the slow checks
        }
        fn add_good_prime(&self, _prime_str: &[u8]) {}
        fn add_bad_prime(&self, _prime_str: &[u8]) {}
    }

    #[test]
    fn g_b_g_ab_and_key_derivation_known_answer() {
        // g = 3, g_a = 3^7 mod p over the Telegram prime; the handshake
        // generates a random b, so the expected values are derived from its
        // own b — the DH math itself is pinned by the S59 oracle
        // (`--dh-gen` with a fixed b via the stored-state round trip).
        let prime = BigNum::from_binary(&tg_prime());
        let mut g = BigNum::new();
        g.set_value(3);
        let mut ctx = BigNumContext::new();
        let mut seven = BigNum::new();
        seven.set_value(7);
        let g_a = BigNum::mod_exp(&g, &seven, &prime, &mut ctx);

        let mut handshake = DhHandshake::new();
        handshake.set_config(3, &tg_prime());
        handshake.set_g_a(&g_a.to_binary(-1));
        let expected_g_b = BigNum::mod_exp(&g, &handshake.get_b(), &prime, &mut ctx);
        assert_eq!(handshake.get_g_b(), expected_g_b.to_binary(-1));
        let expected_g_ab = BigNum::mod_exp(&g_a, &handshake.get_b(), &prime, &mut ctx);
        let (key_id, key) = handshake.gen_key();
        assert_eq!(key, expected_g_ab.to_binary(256));
        assert_eq!(key_id, DhHandshake::calc_key_id(&key));
        assert_eq!(key.len(), 256);
    }

    #[test]
    fn config_checks_validate_the_telegram_prime() {
        let status = DhHandshake::check_config(3, &tg_prime(), &NoCallback);
        assert!(status.is_ok(), "{}", status.message());
        // g = 2 with the same prime fails the mod-4g rule (p mod 8 != 7).
        let status = DhHandshake::check_config(2, &tg_prime(), &NoCallback);
        assert!(status.is_error());
        assert_eq!(status.message(), "Bad prime mod 4g");
        // A short prime is rejected.
        let status = DhHandshake::check_config(3, &[1u8; 128], &NoCallback);
        assert!(status.is_error());
        assert_eq!(status.message(), "p is not 2048-bit number");
    }

    #[test]
    fn g_a_hash_mismatch_is_rejected() {
        let mut handshake = DhHandshake::new();
        handshake.set_config(3, &tg_prime());
        handshake.set_g_a_hash(&[0u8; 32]);
        let mut g_a = BigNum::new();
        g_a.set_value(7);
        let mut ctx = BigNumContext::new();
        let mut g = BigNum::new();
        g.set_value(3);
        let prime = BigNum::from_binary(&tg_prime());
        let g_a = BigNum::mod_exp(&g, &g_a, &prime, &mut ctx);
        handshake.set_g_a(&g_a.to_binary(-1));
        let status = handshake.run_checks(true, &NoCallback);
        assert!(status.is_error());
        assert_eq!(status.message(), "g_a_hash mismatch");
    }

    #[test]
    fn store_parse_roundtrip() {
        let mut handshake = DhHandshake::new();
        handshake.set_config(3, &tg_prime());
        let mut g_a = BigNum::new();
        g_a.set_value(7);
        let mut ctx = BigNumContext::new();
        let mut g = BigNum::new();
        g.set_value(3);
        let prime = BigNum::from_binary(&tg_prime());
        let g_a = BigNum::mod_exp(&g, &g_a, &prime, &mut ctx);
        handshake.set_g_a(&g_a.to_binary(-1));

        let mut out = Vec::new();
        handshake.store(&mut out);
        let mut r = tl::Reader::new(&out);
        let parsed = DhHandshake::parse(&mut r).expect("valid state");
        assert_eq!(parsed.get_g_b(), handshake.get_g_b());
        assert_eq!(parsed.get_g_a(), handshake.get_g_a());
    }

    #[test]
    fn calc_key_id_matches_the_cpp_push_notification_vector() {
        // td/test/mtproto.cpp TEST(Mtproto, notifications): the push key and
        // its expected key id (computed by C++ DhHandshake::calc_key_id).
        let key = crate::utils::base64url_decode(
            b"uBa5yu01a-nJJeqsR3yeqMs6fJLYXjecYzFcvS6jIwS3nefBIr95LWrTm-IbRBNDLrkISz1Sv0KYpDzhU8WFRk1D0V_qyO7XsbDPyrYxRBpGxofJUINSjb1uCxoSdoh1_F0UXEA2fWWKKVxL0DKUQssZfbVj3AbRglsWpH-jDK1oc6eBydRiS3i4j-H0yJkEMoKRgaF9NaYI4u26oIQ-Ez46kTVU-R7e3acdofOJKm7HIKan_5ZMg82Dvec2M6vc_I54Vs28iBx8IbBO1y5z9WSScgW3JCvFFKP2MXIu7Jow5-cpUx6jXdzwRUb9RDApwAFKi45zpv8eb3uPCDAmIQ",
        )
        .unwrap();
        assert_eq!(key.len(), 256);
        let key_id = DhHandshake::calc_key_id(&key);
        // The C++ test asserts equality with NotificationManager's receiver
        // id; the value below is the SHA-1-derived id for this key
        // (computed independently with python's hashlib).
        assert_eq!(key_id, 2_886_285_856_283_441_188);
    }
}
