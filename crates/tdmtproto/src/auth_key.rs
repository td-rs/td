//! Port of `td/mtproto/AuthKey.h`: an MTProto authorization key with its
//! bookkeeping flags and the binary (de)serialization used by the AuthData
//! persistence format (S61).

use crate::mtproto_api::tl;
use tdutils::time;

/// `td::mtproto::AuthKey`.
#[derive(Debug, Default, Clone)]
pub struct AuthKey {
    auth_key_id: u64,
    auth_key: Vec<u8>,
    auth_flag: bool,
    have_header: bool,
    header_expires_at: f64,
    expires_at: f64,
    created_at: f64,
}

/// `AuthKey::AUTH_FLAG` / `HAS_CREATED_AT` / `HAS_EXPIRES_AT` — the
/// persisted flags word.
pub const AUTH_FLAG: i32 = 1;
pub const HAS_CREATED_AT: i32 = 4;
pub const HAS_EXPIRES_AT: i32 = 8;

impl AuthKey {
    pub fn new(auth_key_id: u64, auth_key: Vec<u8>) -> Self {
        Self {
            auth_key_id,
            auth_key,
            have_header: true,
            ..Self::default()
        }
    }

    /// `AuthKey::break_key()`: invalidates the key. The C++ code increments
    /// `key[0]` unchecked; panicking instead of UB is the Rust equivalent.
    pub fn break_key(&mut self) {
        self.auth_key_id += 1;
        self.auth_key[0] += 1;
    }

    pub fn empty(&self) -> bool {
        self.auth_key.is_empty()
    }

    pub fn key(&self) -> &[u8] {
        &self.auth_key
    }

    pub fn id(&self) -> u64 {
        self.auth_key_id
    }

    pub fn auth_flag(&self) -> bool {
        self.auth_flag
    }

    pub fn set_auth_flag(&mut self, new_auth_flag: bool) {
        self.auth_flag = new_auth_flag;
    }

    pub fn need_header(&self) -> bool {
        self.have_header || time::now() < self.header_expires_at
    }

    pub fn remove_header(&mut self) {
        if self.auth_flag && self.have_header {
            self.have_header = false;
            self.header_expires_at = time::now() + 3.0;
        }
    }

    pub fn restore_header(&mut self) {
        self.have_header = true;
    }

    pub fn expires_at(&self) -> f64 {
        self.expires_at
    }

    pub fn created_at(&self) -> f64 {
        self.created_at
    }

    pub fn set_expires_at(&mut self, expires_at: f64) {
        self.expires_at = expires_at;
    }

    pub fn set_created_at(&mut self, created_at: f64) {
        self.created_at = created_at;
    }

    pub fn clear(&mut self) {
        self.auth_key.clear();
    }

    /// `AuthKey::store`: `id`, flags, key, optional created_at and the
    /// remaining lifetime plus the wall-clock time it was written.
    pub fn store(&self, out: &mut Vec<u8>) {
        tl::__i64(out, self.auth_key_id as i64);
        let has_created_at = self.created_at != 0.0;
        let has_expires_at = self.expires_at != 0.0;
        let mut flags = 0;
        if self.auth_flag {
            flags |= AUTH_FLAG;
        }
        if has_created_at {
            flags |= HAS_CREATED_AT;
        }
        if has_expires_at {
            flags |= HAS_EXPIRES_AT;
        }
        tl::__i32(out, flags);
        tl::__string(out, &self.auth_key);
        if has_created_at {
            tl::__f64(out, self.created_at);
        }
        if has_expires_at {
            let time_left = (self.expires_at - time::now()).max(0.0);
            tl::__f64(out, time_left);
            tl::__f64(out, system_now());
        }
    }

    /// `AuthKey::parse`.
    pub fn parse(r: &mut tl::Reader<'_>) -> Result<Self, &'static str> {
        let auth_key_id = r.i64()? as u64;
        let flags = r.i32()?;
        let auth_flag = flags & AUTH_FLAG != 0;
        let auth_key = r.bytes()?;
        let created_at = if flags & HAS_CREATED_AT != 0 {
            r.f64()?
        } else {
            0.0
        };
        let expires_at = if flags & HAS_EXPIRES_AT != 0 {
            let mut time_left = r.f64()?;
            let old_server_time = r.f64()?;
            let passed_server_time = (system_now() - old_server_time).max(0.0);
            time_left = (time_left - passed_server_time).max(0.0);
            time::now() + time_left
        } else {
            0.0
        };
        // just in case
        Ok(Self {
            auth_key_id,
            auth_key,
            auth_flag,
            have_header: true,
            header_expires_at: 0.0,
            expires_at,
            created_at,
        })
    }
}

/// `Clocks::system()` — wall-clock seconds since the Unix epoch.
fn system_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}
