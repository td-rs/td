//! `td::hex_encode` (tdutils/td/utils/misc.h:266, misc.cpp:157-166).

/// `td::hex_encode` (misc.cpp:157-166): each byte as two lowercase hex
/// digits, high nibble first, no separators.
pub fn hex_encode(data: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut res = String::with_capacity(2 * data.len());
    for &c in data {
        res.push(HEX[(c >> 4) as usize] as char);
        res.push(HEX[(c & 15) as usize] as char);
    }
    res
}

#[cfg(test)]
mod tests {
    use super::hex_encode;

    /// `TEST(Misc, hex_encode)` (tdutils/test/misc.cpp) — the C++ test
    /// round-trips against hex_decode; the shape check here pins the
    /// lowercase two-digit-per-byte form.
    #[test]
    fn hex_encode_shapes() {
        assert_eq!(hex_encode(b""), "");
        assert_eq!(hex_encode(b"\x00\x0f\xf0\xff"), "000ff0ff");
        assert_eq!(hex_encode(b"abc"), "616263");
    }
}
