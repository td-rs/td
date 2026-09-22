//! `td::zero_encode` family (tdutils/td/utils/misc.cpp:238-277).
//!
//! The RLE over zero bytes used by the file persistent/unique id strings:
//! every zero byte is followed by a run-length count byte (1-250).

/// `td::zero_encode` (misc.cpp:238-253): each `0` byte is emitted followed by
/// the length of its run (capped at 250). Non-zero bytes are copied as-is.
pub fn zero_encode(data: &[u8]) -> Vec<u8> {
    let mut res = Vec::with_capacity(data.len());
    let n = data.len();
    let mut i = 0;
    while i < n {
        res.push(data[i]);
        if data[i] == 0 {
            let mut cnt: u8 = 1;
            let mut run = cnt as usize;
            while run < 250 && i + run < n && data[i + run] == data[i] {
                cnt += 1;
                run = cnt as usize;
            }
            res.push(cnt);
            i += cnt as usize - 1;
        }
        i += 1;
    }
    res
}

/// `td::zero_decode` (misc.cpp:255-277): the inverse of [`zero_encode`].
pub fn zero_decode(data: &[u8]) -> Vec<u8> {
    let mut res = Vec::with_capacity(data.len());
    let n = data.len();
    let mut i = 0;
    while i < n {
        if data[i] == 0 && i + 1 < n {
            let cnt = data[i + 1] as usize;
            res.extend(std::iter::repeat_n(0u8, cnt));
            i += 1;
        } else {
            res.push(data[i]);
        }
        i += 1;
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `TEST(Misc, zero_encode)` (tdutils/test/misc.cpp:282-306).
    fn check(data: &[u8], expected_zero: &[u8]) {
        let encoded = zero_encode(data);
        assert_eq!(&encoded, expected_zero);
        assert_eq!(&zero_decode(&encoded), data);
    }

    #[test]
    fn zero_encode_matches_the_cpp_vectors() {
        let str_: Vec<u8> = (1u8..255).collect();
        check(&str_, &str_);

        check(b"", b"");
        check(b"\x00", b"\x00\x01");
        check(&[0, 0xff][..].repeat(7), &[0, 1, 0xff][..].repeat(7));
        check(
            &[0, 0, 0xff, 0xff][..].repeat(6),
            &[0, 2, 0xff, 0xff][..].repeat(6),
        );
        check(
            &[0, 0, 0, 0, 0, 0xff, 0xff, 0xff, 0xff, 0xff],
            &[0, 5, 0xff, 0xff, 0xff, 0xff, 0xff],
        );
    }

    #[test]
    fn zero_encode_run_is_capped_at_250() {
        let data = vec![0u8; 600];
        let encoded = zero_encode(&data);
        // 250 + 250 + 100
        assert_eq!(encoded, vec![0, 250, 0, 250, 0, 100]);
        assert_eq!(zero_decode(&encoded), data);
    }
}
