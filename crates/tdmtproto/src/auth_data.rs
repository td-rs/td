//! Port of `td/mtproto/AuthData.{h,cpp}`: the session framing state —
//! salts, message ids, sequence numbers and the duplicate checkers.
//!
//! This is the client half of the MTProto transport framing rules (msg_id
//! parity, msg_id time windows, salt rotation, seq_no progression) with the
//! exact C++ acceptance criteria, error codes and error texts.

use crate::auth_key::AuthKey;
use crate::message_id::MessageId;
use crate::mtproto_api::tl;
use tdutils::random;
use tdutils::status::Status;
use tdutils::time;

/// `td::mtproto::ServerSalt`: one `future_salt`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ServerSalt {
    pub salt: i64,
    pub valid_since: f64,
    pub valid_until: f64,
}

/// `store(ServerSalt)`.
pub fn store_server_salt(out: &mut Vec<u8>, salt: &ServerSalt) {
    tl::__i64(out, salt.salt);
    tl::__f64(out, salt.valid_since);
    tl::__f64(out, salt.valid_until);
}

/// `parse(ServerSalt)`.
pub fn parse_server_salt(r: &mut tl::Reader<'_>) -> Result<ServerSalt, &'static str> {
    Ok(ServerSalt {
        salt: r.i64()?,
        valid_since: r.f64()?,
        valid_until: r.f64()?,
    })
}

/// `check_message_id_duplicates`: the bounded ring of the last `max_size`
/// received message ids. Returns error code 1 for an already processed
/// message id and code 2 for one older than everything in the ring; the
/// messages replicate the C++ texts.
pub fn check_message_id_duplicates(
    saved_message_ids: &mut [MessageId],
    max_size: usize,
    end_pos: &mut usize,
    message_id: MessageId,
) -> Status {
    if *end_pos == 2 * max_size {
        saved_message_ids.copy_within(max_size..2 * max_size, 0);
        *end_pos = max_size;
    }
    if *end_pos == 0 || message_id > saved_message_ids[*end_pos - 1] {
        // fast path
        saved_message_ids[*end_pos] = message_id;
        *end_pos += 1;
        return Status::ok();
    }
    if *end_pos >= max_size && message_id < saved_message_ids[0] {
        return Status::error(
            2,
            &format!(
                "Ignore very old {message_id} older than the oldest known {}",
                saved_message_ids[0]
            ),
        );
    }
    let pos = saved_message_ids[..*end_pos].partition_point(|&saved| saved < message_id);
    if saved_message_ids[pos] == message_id {
        return Status::error(1, &format!("Ignore already processed {message_id}"));
    }
    saved_message_ids.copy_within(pos..*end_pos, pos + 1);
    saved_message_ids[pos] = message_id;
    *end_pos += 1;
    Status::ok()
}

/// `td::mtproto::MessageIdDuplicateChecker<max_size>`. The C++ class holds
/// a `std::array<MessageId, 2 * max_size>`; const generics cannot size a
/// Rust array, so the ring is a `Box<[MessageId]>` of the same length.
pub struct MessageIdDuplicateChecker<const MAX_SIZE: usize> {
    saved_message_ids: Box<[MessageId]>,
    end_pos: usize,
}

impl<const MAX_SIZE: usize> Default for MessageIdDuplicateChecker<MAX_SIZE> {
    fn default() -> Self {
        Self {
            saved_message_ids: vec![MessageId::default(); 2 * MAX_SIZE].into_boxed_slice(),
            end_pos: 0,
        }
    }
}

impl<const MAX_SIZE: usize> MessageIdDuplicateChecker<MAX_SIZE> {
    pub fn check(&mut self, message_id: MessageId) -> Status {
        check_message_id_duplicates(&mut self.saved_message_ids, MAX_SIZE, &mut self.end_pos, message_id)
    }
}

/// `td::mtproto::AuthData`.
pub struct AuthData {
    use_pfs: bool,
    main_auth_key: AuthKey,
    tmp_auth_key: AuthKey,
    server_time_difference_was_updated: bool,
    server_time_difference: f64,
    server_salt: ServerSalt,
    last_message_id: MessageId,
    seq_no: i32,
    header: Vec<u8>,
    session_id: u64,
    future_salts: Vec<ServerSalt>,
    duplicate_checker: MessageIdDuplicateChecker<1000>,
    updates_duplicate_checker: MessageIdDuplicateChecker<1000>,
    updates_duplicate_rechecker: MessageIdDuplicateChecker<100>,
}

impl AuthData {
    /// `AuthData::AuthData()`.
    pub fn new() -> Self {
        Self {
            use_pfs: true,
            main_auth_key: AuthKey::default(),
            tmp_auth_key: AuthKey::default(),
            server_time_difference_was_updated: false,
            server_time_difference: 0.0,
            server_salt: ServerSalt {
                salt: random::secure_int64(),
                valid_since: -1e10,
                valid_until: -1e10,
            },
            last_message_id: MessageId::default(),
            seq_no: 0,
            header: Vec::new(),
            session_id: 0,
            future_salts: Vec::new(),
            duplicate_checker: MessageIdDuplicateChecker::default(),
            updates_duplicate_checker: MessageIdDuplicateChecker::default(),
            updates_duplicate_rechecker: MessageIdDuplicateChecker::default(),
        }
    }

    pub fn is_ready(&mut self, now: f64) -> bool {
        if !self.has_main_auth_key() {
            tdutils::log_info!("Need main auth key");
            return false;
        }
        if self.use_pfs() && !self.has_tmp_auth_key(now) {
            tdutils::log_info!("Need tmp auth key");
            return false;
        }
        if !self.has_salt(now) {
            tdutils::log_info!("Need salt");
            return false;
        }
        true
    }

    pub fn set_main_auth_key(&mut self, auth_key: AuthKey) {
        self.main_auth_key = auth_key;
    }

    pub fn break_main_auth_key(&mut self) {
        self.main_auth_key.break_key();
    }

    pub fn get_main_auth_key(&self) -> &AuthKey {
        &self.main_auth_key
    }

    pub fn has_main_auth_key(&self) -> bool {
        !self.main_auth_key.empty()
    }

    pub fn need_main_auth_key(&self) -> bool {
        !self.has_main_auth_key()
    }

    pub fn set_tmp_auth_key(&mut self, auth_key: AuthKey) {
        assert!(!auth_key.empty());
        self.tmp_auth_key = auth_key;
    }

    pub fn get_tmp_auth_key(&self) -> &AuthKey {
        &self.tmp_auth_key
    }

    pub fn was_tmp_auth_key(&self) -> bool {
        self.use_pfs() && !self.tmp_auth_key.empty()
    }

    pub fn need_tmp_auth_key(&self, now: f64, refresh_margin: f64) -> bool {
        if !self.use_pfs() {
            return false;
        }
        if self.tmp_auth_key.empty() {
            return true;
        }
        if now > self.tmp_auth_key.expires_at() - refresh_margin {
            return true;
        }
        false
    }

    pub fn drop_main_auth_key(&mut self) {
        self.main_auth_key = AuthKey::default();
    }

    pub fn drop_tmp_auth_key(&mut self) {
        self.tmp_auth_key = AuthKey::default();
    }

    pub fn has_tmp_auth_key(&self, now: f64) -> bool {
        if !self.use_pfs() {
            return false;
        }
        if self.tmp_auth_key.empty() {
            return false;
        }
        if now > self.tmp_auth_key.expires_at() {
            return false;
        }
        true
    }

    pub fn get_auth_key(&self) -> &AuthKey {
        if self.use_pfs() {
            self.get_tmp_auth_key()
        } else {
            self.get_main_auth_key()
        }
    }

    pub fn has_auth_key(&self, now: f64) -> bool {
        if self.use_pfs() {
            self.has_tmp_auth_key(now)
        } else {
            self.has_main_auth_key()
        }
    }

    pub fn get_auth_flag(&self) -> bool {
        self.main_auth_key.auth_flag()
    }

    pub fn set_auth_flag(&mut self, auth_flag: bool) {
        self.main_auth_key.set_auth_flag(auth_flag);
        if !auth_flag {
            self.drop_tmp_auth_key();
        }
    }

    pub fn get_bind_flag(&self) -> bool {
        !self.use_pfs() || self.tmp_auth_key.auth_flag()
    }

    pub fn on_bind(&mut self) {
        assert!(self.use_pfs());
        self.tmp_auth_key.set_auth_flag(true);
    }

    pub fn get_header(&self) -> &[u8] {
        if self.use_pfs() {
            if self.tmp_auth_key.need_header() {
                &self.header
            } else {
                &[]
            }
        } else if self.main_auth_key.need_header() {
            &self.header
        } else {
            &[]
        }
    }

    pub fn set_header(&mut self, header: Vec<u8>) {
        self.header = header;
    }

    pub fn on_api_response(&mut self) {
        if self.use_pfs() {
            self.tmp_auth_key.remove_header();
        } else {
            self.main_auth_key.remove_header();
        }
    }

    pub fn on_connection_not_inited(&mut self) {
        if self.use_pfs() {
            self.tmp_auth_key.restore_header();
        } else {
            self.main_auth_key.restore_header();
        }
    }

    pub fn set_session_id(&mut self, session_id: u64) {
        self.session_id = session_id;
    }

    pub fn get_session_id(&self) -> u64 {
        assert_ne!(self.session_id, 0);
        self.session_id
    }

    pub fn get_server_time(&self, now: f64) -> f64 {
        self.server_time_difference + now
    }

    pub fn get_server_time_difference(&self) -> f64 {
        self.server_time_difference
    }

    /// diff == msg_id / 2^32 - now == old_server_now - now <= server_now - now
    /// server_time_difference >= max{diff}
    pub fn update_server_time_difference(&mut self, diff: f64) -> bool {
        if !self.server_time_difference_was_updated {
            tdutils::log_debug!(
                "Set server time difference: {} -> {}",
                self.server_time_difference,
                diff
            );
            self.server_time_difference_was_updated = true;
            self.server_time_difference = diff;
        } else if self.server_time_difference + 1e-4 < diff {
            tdutils::log_debug!(
                "Update server time difference: {} -> {}",
                self.server_time_difference,
                diff
            );
            self.server_time_difference = diff;
        } else {
            return false;
        }
        tdutils::log_debug!(
            "New server time: {}",
            self.get_server_time(time::now_cached())
        );
        true
    }

    pub fn reset_server_time_difference(&mut self, diff: f64) {
        tdutils::log_debug!(
            "Reset server time difference: {} -> {}",
            self.server_time_difference,
            diff
        );
        self.server_time_difference_was_updated = false;
        self.server_time_difference = diff;
    }

    pub fn get_server_salt(&mut self, now: f64) -> i64 {
        self.update_salt(now);
        self.server_salt.salt
    }

    pub fn set_server_salt(&mut self, salt: i64, now: f64) {
        self.server_salt.salt = salt;
        let server_time = self.get_server_time(now);
        self.server_salt.valid_since = server_time;
        self.server_salt.valid_until = server_time + 60.0 * 10.0;
        self.future_salts.clear();
    }

    pub fn is_server_salt_valid(&self, now: f64) -> bool {
        self.server_salt.valid_until > self.get_server_time(now) + 60.0
    }

    pub fn has_salt(&mut self, now: f64) -> bool {
        self.update_salt(now);
        self.is_server_salt_valid(now)
    }

    pub fn need_future_salts(&mut self, now: f64) -> bool {
        self.update_salt(now);
        self.future_salts.is_empty() || !self.is_server_salt_valid(now)
    }

    pub fn set_future_salts(&mut self, salts: Vec<ServerSalt>, now: f64) {
        if salts.is_empty() {
            return;
        }
        self.future_salts = salts;
        self.future_salts
            .sort_by(|a, b| b.valid_since.partial_cmp(&a.valid_since).unwrap_or(std::cmp::Ordering::Equal));
        self.update_salt(now);
    }

    pub fn get_future_salts(&self) -> Vec<ServerSalt> {
        let mut res = self.future_salts.clone();
        res.push(self.server_salt);
        res
    }

    /// `AuthData::next_message_id`: a new even, monotonic message id based
    /// on the current server time with randomized low bits.
    pub fn next_message_id(&mut self, now: f64) -> MessageId {
        let server_time = self.get_server_time(now);
        let t = (server_time * (1u64 << 32) as f64) as u64;

        // randomize lower bits for clocks with low precision
        let rx = random::secure_int32();
        let to_xor = rx & ((1 << 22) - 1);

        let t = t ^ to_xor as u64;
        let mut result = MessageId::new(t & !3u64);
        if self.last_message_id >= result {
            let to_mul = ((rx >> 22) & 1023) + 1;
            result = MessageId::new(self.last_message_id.get() + 8 * to_mul as u64);
        }
        tdutils::log_debug!("Create identifier for {result} at {now}");
        self.last_message_id = result;
        result
    }

    pub fn is_valid_outbound_msg_id(&self, message_id: MessageId, now: f64) -> bool {
        let server_time = self.get_server_time(now);
        let id_time = message_id.get() as f64 / (1u64 << 32) as f64;
        server_time - 150.0 < id_time && id_time < server_time + 30.0
    }

    pub fn is_valid_inbound_msg_id(&self, message_id: MessageId, now: f64) -> bool {
        let server_time = self.get_server_time(now);
        let id_time = message_id.get() as f64 / (1u64 << 32) as f64;
        server_time - 300.0 < id_time && id_time < server_time + 30.0
    }

    pub fn check_packet(
        &mut self,
        session_id: u64,
        message_id: MessageId,
        now: f64,
    ) -> (Status, bool) {
        // Client is to check that the session_id field in the decrypted message indeed equals to that of an active session
        // created by the client.
        if self.get_session_id() != session_id {
            return (
                Status::error_message(&format!(
                    "Receive packet from different session {session_id} in session {}",
                    self.get_session_id()
                )),
                false,
            );
        }

        // Client must check that msg_id has even parity for messages from client to server, and odd parity for messages
        // from server to client.
        if message_id.get() & 1 == 0 {
            return (Status::error_message(&format!("Receive invalid {message_id}")), false);
        }

        let status = self.duplicate_checker.check(message_id);
        if status.is_error() {
            return (status, false);
        }

        tdutils::log_debug!(
            "Receive packet in {message_id} from session {session_id} at {now}"
        );
        let time_difference_was_updated =
            self.update_server_time_difference((message_id.get() >> 32) as u32 as f64 - now);

        // In addition, msg_id values that belong over 30 seconds in the future or over 300 seconds in the past are to be
        // ignored (recall that msg_id approximately equals unixtime * 2^32). This is especially important for the server.
        // The client would also find this useful (to protect from a replay attack), but only if it is certain of its time
        // (for example, if its time has been synchronized with that of the server).
        if self.server_time_difference_was_updated && !self.is_valid_inbound_msg_id(message_id, now) {
            return (
                Status::error_message(&format!("Ignore too old or too new {message_id}")),
                time_difference_was_updated,
            );
        }

        (Status::ok(), time_difference_was_updated)
    }

    pub fn check_update(&mut self, message_id: MessageId) -> Status {
        self.updates_duplicate_checker.check(message_id)
    }

    pub fn recheck_update(&mut self, message_id: MessageId) -> Status {
        self.updates_duplicate_rechecker.check(message_id)
    }

    pub fn next_seq_no(&mut self, is_content_related: bool) -> i32 {
        let res = self.seq_no;
        if is_content_related {
            self.seq_no += 2;
            return res | 1;
        }
        res
    }

    pub fn clear_seq_no(&mut self) {
        self.seq_no = 0;
    }

    pub fn set_use_pfs(&mut self, use_pfs: bool) {
        self.use_pfs = use_pfs;
    }

    pub fn use_pfs(&self) -> bool {
        self.use_pfs
    }

    /// `AuthData::update_salt`.
    fn update_salt(&mut self, now: f64) {
        let server_time = self.get_server_time(now);
        while let Some(salt) = self.future_salts.last() {
            if salt.valid_since < server_time {
                self.server_salt = *salt;
                self.future_salts.pop();
            } else {
                break;
            }
        }
    }
}

impl Default for AuthData {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth_data() -> AuthData {
        let mut res = AuthData::new();
        res.set_session_id(123456789);
        // A fresh main key: 256 zero bytes is enough for the framing tests
        // (id never checked here).
        res.set_main_auth_key(AuthKey::new(0x1122334455667788, vec![0u8; 256]));
        res.set_use_pfs(false);
        res
    }

    #[test]
    fn next_message_id_is_even_monotonic_and_in_window() {
        let mut data = auth_data();
        // `time::now()` counts from process start (microseconds here), so
        // shift the server-time base to a sane epoch first — the msg_id math
        // in Rust saturates on negative float→int casts.
        data.reset_server_time_difference(1e6);
        let now = time::now();
        let first = data.next_message_id(now);
        assert_eq!(first.get() & 3, 0, "msg_id must be divisible by 4");
        let second = data.next_message_id(now);
        assert!(second > first, "msg ids must grow");
        assert_eq!(second.get() & 3, 0);
        assert!(data.is_valid_outbound_msg_id(first, now));
        assert!(data.is_valid_outbound_msg_id(second, now));
        // Very far future/past ids are rejected.
        let server_time = data.get_server_time(now);
        let too_old = MessageId::new((((server_time - 400.0) * (1u64 << 32) as f64) as u64) & !3);
        let too_new = MessageId::new((((server_time + 400.0) * (1u64 << 32) as f64) as u64) & !3);
        assert!(!data.is_valid_outbound_msg_id(too_old, now));
        assert!(!data.is_valid_outbound_msg_id(too_new, now));
    }

    #[test]
    fn seq_no_progression_matches_cpp() {
        let mut data = auth_data();
        // Irrelevant messages consume no seq_no; content-related ones take
        // odd values and advance by 2.
        assert_eq!(data.next_seq_no(false), 0);
        assert_eq!(data.next_seq_no(false), 0);
        assert_eq!(data.next_seq_no(true), 1);
        assert_eq!(data.next_seq_no(false), 2);
        assert_eq!(data.next_seq_no(true), 3);
        data.clear_seq_no();
        assert_eq!(data.next_seq_no(true), 1);
    }

    #[test]
    fn check_packet_rejects_other_session_and_even_ids() {
        let mut data = auth_data();
        let now = time::now();
        let odd = MessageId::new(((now * (1u64 << 32) as f64) as u64) | 1);
        // Wrong session.
        let (status, _) = data.check_packet(42, odd, now);
        assert!(status.is_error());
        // Even parity from the server is invalid.
        let even = MessageId::new(odd.get() & !1);
        let (status, _) = data.check_packet(123456789, even, now);
        assert!(status.is_error());
        assert!(status.message().contains("Receive invalid"));
    }

    #[test]
    fn duplicate_checker_ring_semantics() {
        let mut saved = [MessageId::default(); 2 * 4];
        let mut end_pos = 0;
        for i in 0..8 {
            let id = MessageId::new(4 + 4 * i as u64);
            assert!(check_message_id_duplicates(&mut saved, 4, &mut end_pos, id).is_ok());
        }
        // Ring wrapped: oldest known is message 0x14.
        let duplicate = MessageId::new(0x10);
        let status = check_message_id_duplicates(&mut saved, 4, &mut end_pos, duplicate);
        assert!(status.is_error());
        assert_eq!(status.code(), 2, "too old, not 'already processed'");
        assert!(status.message().contains("Ignore very old"));
        let duplicate = MessageId::new(0x1c);
        let status = check_message_id_duplicates(&mut saved, 4, &mut end_pos, duplicate);
        assert!(status.is_error());
        assert_eq!(status.code(), 1);
        assert!(status.message().contains("Ignore already processed"));
        // Insertion in the middle keeps the array sorted.
        let status = check_message_id_duplicates(&mut saved, 4, &mut end_pos, MessageId::new(0x16));
        assert!(status.is_ok());
        assert!(saved[..end_pos].windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn future_salts_rotate_by_valid_since() {
        let mut data = auth_data();
        let now = time::now();
        data.set_future_salts(
            vec![
                ServerSalt {
                    salt: 11,
                    valid_since: now - 100.0,
                    valid_until: now + 100.0,
                },
                ServerSalt {
                    salt: 22,
                    valid_since: now + 100.0,
                    valid_until: now + 1000.0,
                },
            ],
            now,
        );
        // The already-valid future salt (11) becomes current immediately.
        assert_eq!(data.get_server_salt(now), 11);
        // The later one stays pending until its window opens.
        data.reset_server_time_difference(200.0);
        assert_eq!(data.get_server_salt(now), 22);
    }

    #[test]
    fn server_salt_store_parse_roundtrip() {
        let salt = ServerSalt {
            salt: -1234567890123456789,
            valid_since: 12345.5,
            valid_until: 54321.25,
        };
        let mut out = Vec::new();
        store_server_salt(&mut out, &salt);
        let mut r = tl::Reader::new(&out);
        let parsed = parse_server_salt(&mut r).expect("valid salt");
        assert_eq!(parsed, salt);
    }

    #[test]
    fn salt_window_is_ten_minutes() {
        let mut data = auth_data();
        let now = time::now();
        data.set_server_salt(777, now);
        assert!(data.is_server_salt_valid(now));
        assert!(data.is_server_salt_valid(now + 9.0 * 60.0 - 60.0));
        assert!(!data.is_server_salt_valid(now + 9.0 * 60.0 + 1.0));
    }
}
