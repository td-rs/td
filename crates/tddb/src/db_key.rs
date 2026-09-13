//! Port of `td/db/DbKey.h`: a database encryption key — empty, raw 32-byte key,
//! or password. Kept binary-safe (`Vec<u8>`) like the C++ `string data_`.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DbKeyType {
    Empty,
    RawKey,
    Password,
}

/// `td::DbKey`
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DbKey {
    ty: DbKeyType,
    data: Vec<u8>,
}

impl DbKey {
    /// `DbKey::raw_key(string)` — raw 32-byte key.
    pub fn raw_key(raw_key: impl Into<Vec<u8>>) -> Self {
        DbKey {
            ty: DbKeyType::RawKey,
            data: raw_key.into(),
        }
    }

    /// `DbKey::password(string)`.
    pub fn password(password: impl Into<Vec<u8>>) -> Self {
        DbKey {
            ty: DbKeyType::Password,
            data: password.into(),
        }
    }

    /// `DbKey::empty()`.
    pub fn empty() -> Self {
        DbKey {
            ty: DbKeyType::Empty,
            data: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.ty == DbKeyType::Empty
    }

    pub fn is_raw_key(&self) -> bool {
        self.ty == DbKeyType::RawKey
    }

    pub fn is_password(&self) -> bool {
        self.ty == DbKeyType::Password
    }

    /// `DbKey::data()` — `CSlice` in C++; raw bytes here.
    pub fn data(&self) -> &[u8] {
        &self.data
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_key_types_round_trip() {
        let empty = DbKey::empty();
        assert!(empty.is_empty());
        assert!(empty.data().is_empty());

        let password = DbKey::password("cucu'\"mb er");
        assert!(password.is_password());
        assert_eq!(password.data(), b"cucu'\"mb er");

        let raw = DbKey::raw_key(vec![7u8; 32]);
        assert!(raw.is_raw_key());
        assert_eq!(raw.data(), &[7u8; 32]);
        assert_ne!(empty, password);
        assert_ne!(password, raw);
    }
}
