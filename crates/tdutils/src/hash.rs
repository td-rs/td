//! The integer hash utilities of `td/utils/HashTableUtils.h`.

/// `td::randomize_hash` (HashTableUtils.h:19-28) — the murmur3-style
/// finalizer behind the `Hash` specializations.
pub fn randomize_hash(mut h: u32) -> u32 {
    h ^= h >> 16;
    h = h.wrapping_mul(0x85eb_ca6b);
    h ^= h >> 13;
    h = h.wrapping_mul(0xc2b2_ae35);
    h ^= h >> 16;
    h
}

/// `td::Hash<int64>` (HashTableUtils.h:58-60): the fold `value +
/// (value >> 32)` — an arithmetic shift on the signed value — truncated
/// to the low 32 bits, then randomized.
pub fn hash_int64(value: i64) -> u32 {
    randomize_hash(value.wrapping_add(value >> 32) as u32)
}

/// `td::Hash<uint32>` (HashTableUtils.h:53-55).
pub fn hash_uint32(value: u32) -> u32 {
    randomize_hash(value)
}

/// `td::combine_hashes` (HashTableUtils.h:70-72).
pub fn combine_hashes(first_hash: u32, second_hash: u32) -> u32 {
    first_hash
        .wrapping_mul(2_023_654_985u32)
        .wrapping_add(second_hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Vectors from the C++ bodies compiled standalone (g++, -O2) — the
    /// port must stay bit-exact: the dc-option sort order rides on it.
    #[test]
    fn randomize_hash_matches_cpp() {
        assert_eq!(randomize_hash(0x0000_0000), 0x0000_0000);
        assert_eq!(randomize_hash(0x0000_0001), 0x514e_28b7);
        assert_eq!(randomize_hash(0x1234_5678), 0xe37c_d1bc);
        assert_eq!(randomize_hash(0xffff_ffff), 0x81f1_6f39);
        // 149.154.175.50 packed big-endian — a dc-option address.
        assert_eq!(randomize_hash(0x959a_af32), 0x5a17_2b97);
        assert_eq!(randomize_hash(0x5fa1_c864), 0xbf03_4a43);
    }

    #[test]
    fn hash_int64_matches_cpp() {
        assert_eq!(hash_int64(0), 0x0000_0000);
        assert_eq!(hash_int64(1), 0x514e_28b7);
        // -1 + (-1) = -2 → 0xfffffffe: the arithmetic shift feeds the fold.
        assert_eq!(hash_int64(-1), 0x7995_c304);
        assert_eq!(hash_int64(42), 0x087f_cd5c);
        // Values under 2^32 fold to themselves (value >> 32 == 0).
        assert_eq!(hash_int64(0x959a_af32), 0x5a17_2b97);
        // 0x959aaf32 + the my_id-style seed.
        assert_eq!(hash_int64(0x959a_af32 + 123_456_789), 0x0fbc_11e7);
        assert_eq!(hash_int64(-987_654_321), 0x82c9_5210);
        assert_eq!(hash_int64(0x7fff_ffff_ffff_1234), 0x1ee2_f5f0);
    }

    #[test]
    fn hash_uint32_is_randomize_hash() {
        assert_eq!(hash_uint32(0x959a_af32), randomize_hash(0x959a_af32));
    }

    #[test]
    fn combine_hashes_matches_cpp() {
        assert_eq!(combine_hashes(0, 0), 0);
        assert_eq!(combine_hashes(1, 2), 2_023_654_987u32);
        assert_eq!(combine_hashes(0x5a17_2b97, 0xbf03_4a43), 0x3fa1_c252);
    }
}
