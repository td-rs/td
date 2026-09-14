//! Port of `td/mtproto/DhCallback.h` (S59): the prime-tracking callback of
//! the DH handshake.

/// `td::mtproto::DhCallback`.
pub trait DhCallback {
    /// Returns 1 for a known-good prime, 0 for a known-bad one and -1 when
    /// unknown (the caller then runs the primality checks).
    fn is_good_prime(&self, prime_str: &[u8]) -> i32;
    fn add_good_prime(&self, prime_str: &[u8]);
    fn add_bad_prime(&self, prime_str: &[u8]);
}
