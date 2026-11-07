//! `DhConfig` + `DhCache` (td/telegram/DhConfig.h, DhCache.{h,cpp}) — the
//! M.32 Wave-3 DH cache for the call plane.

use tdmtproto::dh_callback::DhCallback;

/// `td::DhConfig` (DhConfig.h) — the DH parameters with their version.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DhConfig {
    pub version: i32,
    pub prime: Vec<u8>,
    pub g: i32,
}

impl DhConfig {
    /// `DhConfig::empty` (DhConfig.h:13-15).
    pub fn empty(&self) -> bool {
        self.prime.is_empty()
    }
}

/// The `G()`/`td_` surface of the cache — the binlog pmc.
pub trait DhCacheEnv {
    fn pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8>;
    fn pmc_set_raw(&mut self, key: &[u8], value: &[u8]);
}

/// `good_prime_key` (DhCache.cpp:9-13).
fn good_prime_key(prime_str: &[u8]) -> Vec<u8> {
    let mut key = b"good_prime:".to_vec();
    key.extend_from_slice(prime_str);
    key
}

/// The built-in good prime (DhCache.cpp:16-33) — the 2048-bit group prime
/// hex-decoded (extracted from the source).
const BUILT_IN_GOOD_PRIME: [u8; 256] = [
    0xc7, 0x1c, 0xae, 0xb9, 0xc6, 0xb1, 0xc9, 0x04, 0x8e, 0x6c, 0x52, 0x2f, 0x70, 0xf1, 0x3f, 0x73,
    0x98, 0x0d, 0x40, 0x23, 0x8e, 0x3e, 0x21, 0xc1, 0x49, 0x34, 0xd0, 0x37, 0x56, 0x3d, 0x93, 0x0f,
    0x48, 0x19, 0x8a, 0x0a, 0xa7, 0xc1, 0x40, 0x58, 0x22, 0x94, 0x93, 0xd2, 0x25, 0x30, 0xf4, 0xdb,
    0xfa, 0x33, 0x6f, 0x6e, 0x0a, 0xc9, 0x25, 0x13, 0x95, 0x43, 0xae, 0xd4, 0x4c, 0xce, 0x7c, 0x37,
    0x20, 0xfd, 0x51, 0xf6, 0x94, 0x58, 0x70, 0x5a, 0xc6, 0x8c, 0xd4, 0xfe, 0x6b, 0x6b, 0x13, 0xab,
    0xdc, 0x97, 0x46, 0x51, 0x29, 0x69, 0x32, 0x84, 0x54, 0xf1, 0x8f, 0xaf, 0x8c, 0x59, 0x5f, 0x64,
    0x24, 0x77, 0xfe, 0x96, 0xbb, 0x2a, 0x94, 0x1d, 0x5b, 0xcd, 0x1d, 0x4a, 0xc8, 0xcc, 0x49, 0x88,
    0x07, 0x08, 0xfa, 0x9b, 0x37, 0x8e, 0x3c, 0x4f, 0x3a, 0x90, 0x60, 0xbe, 0xe6, 0x7c, 0xf9, 0xa4,
    0xa4, 0xa6, 0x95, 0x81, 0x10, 0x51, 0x90, 0x7e, 0x16, 0x27, 0x53, 0xb5, 0x6b, 0x0f, 0x6b, 0x41,
    0x0d, 0xba, 0x74, 0xd8, 0xa8, 0x4b, 0x2a, 0x14, 0xb3, 0x14, 0x4e, 0x0e, 0xf1, 0x28, 0x47, 0x54,
    0xfd, 0x17, 0xed, 0x95, 0x0d, 0x59, 0x65, 0xb4, 0xb9, 0xdd, 0x46, 0x58, 0x2d, 0xb1, 0x17, 0x8d,
    0x16, 0x9c, 0x6b, 0xc4, 0x65, 0xb0, 0xd6, 0xff, 0x9c, 0xa3, 0x92, 0x8f, 0xef, 0x5b, 0x9a, 0xe4,
    0xe4, 0x18, 0xfc, 0x15, 0xe8, 0x3e, 0xbe, 0xa0, 0xf8, 0x7f, 0xa9, 0xff, 0x5e, 0xed, 0x70, 0x05,
    0x0d, 0xed, 0x28, 0x49, 0xf4, 0x7b, 0xf9, 0x59, 0xd9, 0x56, 0x85, 0x0c, 0xe9, 0x29, 0x85, 0x1f,
    0x0d, 0x81, 0x15, 0xf6, 0x35, 0xb1, 0x05, 0xee, 0x2e, 0x4e, 0x15, 0xd0, 0x4b, 0x24, 0x54, 0xbf,
    0x6f, 0x4f, 0xad, 0xf0, 0x34, 0xb1, 0x04, 0x03, 0x11, 0x9c, 0xd8, 0xe3, 0xb9, 0x2f, 0xcc, 0x5b,
];

/// `td::DhCache` (DhCache.cpp) — the `DhCallback` over the binlog pmc; the
/// C++ process-wide singleton collapses to a value carrying the env at the
/// call sites (the managers pass their `G()`-equivalent in).
#[derive(Clone, Copy, Debug, Default)]
pub struct DhCache;

impl DhCache {
    /// `DhCache::instance` (DhCache.h:13-15) — the collapsed singleton.
    pub fn instance() -> DhCache {
        DhCache
    }

    /// The built-in seed config — the `G()->get_dh_config()` analog for
    /// the planes whose `messages.getDhConfig` fetch family is still
    /// parked (the call plane fetch, the secret-chat fetch): the
    /// 2048-bit group prime with `g = 2` (DhCache.cpp:16-33).
    pub fn built_in_dh_config() -> DhConfig {
        DhConfig {
            version: 0,
            prime: BUILT_IN_GOOD_PRIME.to_vec(),
            g: 2,
        }
    }

    /// `is_good_prime` (DhCache.cpp:15-34).
    pub fn is_good_prime(&self, env: &mut impl DhCacheEnv, prime_str: &[u8]) -> i32 {
        if prime_str == BUILT_IN_GOOD_PRIME {
            return 1;
        }
        let value = env.pmc_get_raw(&good_prime_key(prime_str));
        if value == b"good" {
            return 1;
        }
        if value == b"bad" {
            return 0;
        }
        // the C++ CHECKs the value is empty before answering unknown
        debug_assert!(value.is_empty(), "unexpected dh prime cache value");
        -1
    }

    /// `add_good_prime` (DhCache.cpp:36-38).
    pub fn add_good_prime(&self, env: &mut impl DhCacheEnv, prime_str: &[u8]) {
        env.pmc_set_raw(&good_prime_key(prime_str), b"good");
    }

    /// `add_bad_prime` (DhCache.cpp:39-41).
    pub fn add_bad_prime(&self, env: &mut impl DhCacheEnv, prime_str: &[u8]) {
        env.pmc_set_raw(&good_prime_key(prime_str), b"bad");
    }
}

impl DhCallback for DhCache {
    fn is_good_prime(&self, prime_str: &[u8]) -> i32 {
        // The trait form has no env; the known built-in prime still answers
        // without the pmc (the unknown/bad answers need the env-carrying
        // form above — the call sites use it).
        if prime_str == BUILT_IN_GOOD_PRIME {
            1
        } else {
            -1
        }
    }

    fn add_good_prime(&self, _prime_str: &[u8]) {}

    fn add_bad_prime(&self, _prime_str: &[u8]) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestEnv(std::collections::BTreeMap<Vec<u8>, Vec<u8>>);

    impl DhCacheEnv for TestEnv {
        fn pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8> {
            self.0.get(key).cloned().unwrap_or_default()
        }

        fn pmc_set_raw(&mut self, key: &[u8], value: &[u8]) {
            self.0.insert(key.to_vec(), value.to_vec());
        }
    }

    #[test]
    fn built_in_prime_is_known_good() {
        let mut env = TestEnv(std::collections::BTreeMap::new());
        assert_eq!(
            DhCache::instance().is_good_prime(&mut env, &BUILT_IN_GOOD_PRIME),
            1
        );
        assert_eq!(DhCache::instance().is_good_prime(&mut env, b"unknown"), -1);
    }

    #[test]
    fn cache_round_trips_through_the_pmc() {
        let mut env = TestEnv(std::collections::BTreeMap::new());
        DhCache::instance().add_bad_prime(&mut env, b"bad-prime");
        assert_eq!(DhCache::instance().is_good_prime(&mut env, b"bad-prime"), 0);
        DhCache::instance().add_good_prime(&mut env, b"good-prime");
        assert_eq!(
            DhCache::instance().is_good_prime(&mut env, b"good-prime"),
            1
        );
    }

    #[test]
    fn dh_config_empty_checks_the_prime() {
        assert!(DhConfig::default().empty());
        assert!(
            !DhConfig {
                prime: vec![1],
                ..DhConfig::default()
            }
            .empty()
        );
    }
}
