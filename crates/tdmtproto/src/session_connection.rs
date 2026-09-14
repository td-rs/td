//! Port of `td/mtproto/SessionConnection.{h,cpp}` (S62): the encrypted
//! MTProto session state machine — queries, acks, containers, pings, salts,
//! seq_no/message-id discipline, service queries (resend/get_state_info)
//! and the flush-timing logic. The C++ poll-driven flush keeps its
//! synchronous packet logic; only the socket I/O is async (the driver
//! awaits readability/timeouts around `flush`).

use std::collections::HashMap;

use crate::auth_data::AuthData;
use crate::message_id::MessageId;
use crate::mtproto_api::{self, tl};
use crate::packet_info::PacketInfo;
use crate::raw_connection::{RawConnection, RawConnectionCallback};
use tdutils::buffer::BufferSlice;
use tdutils::gzip::gzdecode;
use tdutils::random;
use tdutils::status::Status;
use tdutils::time;

/// `SessionConnection::Mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionMode {
    Tcp,
    Http,
    HttpLongPoll,
}

/// `SessionConnection::Callback`.
pub trait SessionConnectionCallback {
    fn on_connected(&mut self);
    fn on_closed(&mut self, status: Status);

    fn on_server_salt_updated(&mut self);
    fn on_server_time_difference_updated(&mut self, force: bool);

    fn on_new_session_created(&mut self, unique_id: u64, first_message_id: MessageId);
    fn on_session_failed(&mut self, status: Status);

    fn on_container_sent(&mut self, container_message_id: MessageId, message_ids: Vec<MessageId>);
    fn on_pong(&mut self, ping_time: f64, pong_time: f64, current_time: f64) -> Status;

    fn on_update(&mut self, packet: BufferSlice) -> Status;

    fn on_message_ack(&mut self, message_id: MessageId);
    fn on_message_result_ok(
        &mut self,
        message_id: MessageId,
        packet: BufferSlice,
        original_size: usize,
    ) -> Status;
    fn on_message_result_error(&mut self, message_id: MessageId, code: i32, message: String);
    fn on_message_failed(&mut self, message_id: MessageId, status: Status);
    fn on_message_info(
        &mut self,
        message_id: MessageId,
        state: i32,
        answer_message_id: MessageId,
        answer_size: i32,
        source: i32,
    );

    fn on_destroy_auth_key(&mut self) -> Status;
}

/// One queued outgoing query (`MtprotoQuery`).
#[derive(Clone)]
pub struct MtprotoQuery {
    pub message_id: MessageId,
    pub seq_no: i32,
    pub packet: BufferSlice,
    pub gzip_flag: bool,
    pub invoke_after_message_ids: Vec<MessageId>,
    pub use_quick_ack: bool,
}

/// A parsed inner message (`MsgInfo`).
#[derive(Debug, Clone, Copy)]
struct MsgInfo {
    message_id: MessageId,
    seq_no: i32,
    size: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServiceQueryType {
    GetStateInfo,
    ResendAnswer,
}

#[derive(Debug, Clone)]
struct ServiceQuery {
    type_: ServiceQueryType,
    container_message_id: MessageId,
    msg_ids: Vec<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionState {
    Init,
    Run,
    Closed,
}

/// `SessionConnection::ACK_DELAY` / `QUERY_DELAY` / `RESEND_ANSWER_DELAY`.
const ACK_DELAY: f64 = 30.0;
const QUERY_DELAY: f64 = 0.001;
const RESEND_ANSWER_DELAY: f64 = 0.001;
const HTTP_MAX_AFTER: i32 = 10; // 0.01s
const HTTP_MAX_DELAY: i32 = 30; // 0.03s

/// `td::mtproto::SessionConnection`.
pub struct SessionConnection {
    online_flag: bool,
    is_main: bool,

    to_send: Vec<MtprotoQuery>,
    to_ack_message_ids: Vec<MessageId>,
    force_send_at: f64,

    to_resend_answer_message_ids: Vec<MessageId>,
    to_cancel_answer_message_ids: Vec<MessageId>,
    to_get_state_info_message_ids: Vec<MessageId>,
    service_queries: HashMap<u64, ServiceQuery>,

    // nobody cleans up this map. But it should be really small.
    container_to_service_message_id: HashMap<u64, Vec<MessageId>>,

    random_delay: f64,
    last_read_at: f64,
    last_ping_at: f64,
    last_pong_at: f64,
    real_last_read_at: f64,
    real_last_pong_at: f64,
    last_ping_message_id: MessageId,
    last_ping_container_message_id: MessageId,

    last_read_size: u64,
    last_write_size: u64,

    need_destroy_auth_key: bool,
    sent_destroy_auth_key: bool,
    destroy_auth_key_send_time: f64,

    flush_packet_at: f64,

    last_get_future_salt_at: f64,
    state: SessionState,
    mode: SessionMode,
    connected_flag: bool,

    container_message_id: MessageId,
    main_message_id: MessageId,

    raw_connection: RawConnection,
    auth_data: AuthData,
    /// The C++ `callback_` pointer — valid for the duration of a `flush`
    /// call.
    callback: *mut (dyn SessionConnectionCallback + 'static),
}

impl SessionConnection {
    pub fn new(mode: SessionMode, raw_connection: RawConnection, auth_data: AuthData) -> Self {
        Self {
            online_flag: false,
            is_main: false,
            to_send: Vec::new(),
            to_ack_message_ids: Vec::new(),
            force_send_at: 0.0,
            to_resend_answer_message_ids: Vec::new(),
            to_cancel_answer_message_ids: Vec::new(),
            to_get_state_info_message_ids: Vec::new(),
            service_queries: HashMap::new(),
            container_to_service_message_id: HashMap::new(),
            random_delay: random::fast(0, 5_000_000) as f64 * 1e-6,
            last_read_at: 0.0,
            last_ping_at: 0.0,
            last_pong_at: 0.0,
            real_last_read_at: 0.0,
            real_last_pong_at: 0.0,
            last_ping_message_id: MessageId::default(),
            last_ping_container_message_id: MessageId::default(),
            last_read_size: 0,
            last_write_size: 0,
            need_destroy_auth_key: false,
            sent_destroy_auth_key: false,
            destroy_auth_key_send_time: 0.0,
            flush_packet_at: 0.0,
            last_get_future_salt_at: 0.0,
            state: SessionState::Init,
            mode,
            connected_flag: false,
            container_message_id: MessageId::default(),
            main_message_id: MessageId::default(),
            raw_connection,
            auth_data,
            callback: null_callback(),
        }
    }

    pub fn auth_data(&mut self) -> &mut AuthData {
        &mut self.auth_data
    }

    pub fn raw_connection(&mut self) -> &mut RawConnection {
        &mut self.raw_connection
    }

    /// `SessionConnection::move_as_raw_connection` — consuming (the C++
    /// moves the unique_ptr out, leaving the session unusable).
    pub fn move_as_raw_connection(mut self) -> RawConnection {
        self.callback = null_callback();
        self.raw_connection
    }

    pub fn get_debug_str(&self) -> &str {
        &self.raw_connection.extra().debug_str
    }

    fn rtt(&self) -> f64 {
        (self.raw_connection.extra().rtt * 1.5 + 1.0).max(2.0)
    }

    fn read_disconnect_delay(&self) -> f64 {
        if self.online_flag {
            self.rtt() * 3.5
        } else {
            135.0 + self.random_delay
        }
    }

    fn ping_disconnect_delay(&self) -> f64 {
        if self.online_flag && self.is_main {
            self.rtt() * 2.5
        } else {
            135.0 + self.random_delay
        }
    }

    fn ping_may_delay(&self) -> f64 {
        if self.online_flag {
            self.rtt() * 0.5
        } else {
            30.0 + self.random_delay
        }
    }

    fn ping_must_delay(&self) -> f64 {
        if self.online_flag {
            self.rtt()
        } else {
            60.0 + self.random_delay
        }
    }

    fn http_max_wait(&self) -> f64 {
        25.0 // 25s. Longer could be closed by proxy
    }

    /// `SessionConnection::send_query`.
    pub fn send_query(
        &mut self,
        buffer: BufferSlice,
        gzip_flag: bool,
        mut message_id: MessageId,
        invoke_after_message_ids: Vec<MessageId>,
        use_quick_ack: bool,
    ) -> MessageId {
        assert_ne!(self.mode, SessionMode::HttpLongPoll); // "LongPoll connection is only for http_wait"
        if message_id == MessageId::default() {
            message_id = self.auth_data.next_message_id(time::now_cached());
        }
        let seq_no = self.auth_data.next_seq_no(true);
        if self.to_send.is_empty() {
            self.send_before(time::now_cached() + QUERY_DELAY);
        }
        self.to_send.push(MtprotoQuery {
            message_id,
            seq_no,
            packet: buffer,
            gzip_flag,
            invoke_after_message_ids,
            use_quick_ack,
        });
        message_id
    }

    pub fn get_state_info(&mut self, message_id: MessageId) {
        if self.to_get_state_info_message_ids.is_empty() {
            self.send_before(time::now_cached());
        }
        self.to_get_state_info_message_ids.push(message_id);
    }

    pub fn resend_answer(&mut self, message_id: MessageId) {
        if self.to_resend_answer_message_ids.is_empty() {
            self.send_before(time::now_cached() + RESEND_ANSWER_DELAY);
        }
        self.to_resend_answer_message_ids.push(message_id);
    }

    pub fn cancel_answer(&mut self, message_id: MessageId) {
        if self.to_cancel_answer_message_ids.is_empty() {
            self.send_before(time::now_cached() + RESEND_ANSWER_DELAY);
        }
        self.to_cancel_answer_message_ids.push(message_id);
    }

    pub fn destroy_key(&mut self) {
        tdutils::log_info!("Set need_destroy_auth_key to true");
        self.need_destroy_auth_key = true;
    }

    pub fn set_online(&mut self, online_flag: bool, is_main: bool) {
        tdutils::log_debug!("Set online to {online_flag}");
        let need_ping = online_flag || !self.online_flag;
        self.online_flag = online_flag;
        self.is_main = is_main;
        let now = time::now();
        if need_ping {
            self.last_pong_at = now - self.ping_disconnect_delay() + self.rtt();
            self.last_read_at = now - self.read_disconnect_delay() + self.rtt();
        } else {
            self.last_pong_at = now;
            self.last_read_at = now;
        }
        self.last_ping_at = 0.0;
        self.last_ping_message_id = MessageId::default();
        self.last_ping_container_message_id = MessageId::default();
    }

    fn with_callback<R>(&self, f: impl FnOnce(&mut dyn SessionConnectionCallback) -> R) -> R {
        let callback = self.callback;
        assert!(!callback.is_null(), "callback must be set during flush");
        // SAFETY: the caller passes a valid callback for the flush call.
        f(unsafe { &mut *callback })
    }

    fn do_close(&mut self, status: Status) {
        self.state = SessionState::Closed;
        // NB: this could be destroyed after on_closed
        let callback = self.callback;
        self.callback = null_callback();
        if !callback.is_null() {
            // SAFETY: the caller passes a valid callback for the flush call.
            unsafe { (&mut *callback).on_closed(status) };
        }
    }

    fn send_crypto(&mut self, data: &[u8], quick_ack_token: u64) {
        assert_ne!(self.state, SessionState::Closed);
        let session_id = self.auth_data.get_session_id();
        let salt = self.auth_data.get_server_salt(time::now_cached());
        let auth_key = self.auth_data.get_auth_key().clone();
        self.last_write_size +=
            self.raw_connection
                .send_crypto(data, session_id, salt, &auth_key, quick_ack_token) as u64;
    }

    fn send_ack(&mut self, message_id: MessageId) {
        if self.to_ack_message_ids.is_empty() {
            self.send_before(time::now_cached() + ACK_DELAY);
        }
        // an easiest way to eliminate duplicated acknowledgements for gzipped packets
        if self.to_ack_message_ids.is_empty()
            || *self.to_ack_message_ids.last().unwrap() != message_id
        {
            self.to_ack_message_ids.push(message_id);

            const MAX_UNACKED_PACKETS: usize = 100;
            if self.to_ack_message_ids.len() >= MAX_UNACKED_PACKETS {
                self.send_before(time::now_cached());
            }
        }
    }

    fn send_before(&mut self, tm: f64) {
        if self.force_send_at == 0.0 || self.force_send_at > tm {
            self.force_send_at = tm;
        }
    }

    fn may_ping(&self) -> bool {
        self.last_ping_at == 0.0
            || (self.mode != SessionMode::HttpLongPoll
                && self.last_ping_at + self.ping_may_delay() < time::now_cached())
    }

    fn must_ping(&self) -> bool {
        self.last_ping_at == 0.0
            || (self.mode != SessionMode::HttpLongPoll
                && self.last_ping_at + self.ping_must_delay() < time::now_cached())
    }

    fn must_flush_packet(&mut self) -> bool {
        self.flush_packet_at = 0.0;

        // we need key to send just something
        if !self.auth_data.has_auth_key(time::now_cached()) {
            return false;
        }

        // transport is ready
        if !self.raw_connection.can_send() {
            return false;
        }

        let has_salt = self.auth_data.has_salt(time::now_cached());
        // do not send anything in long poll connection before we have salt
        if self.mode == SessionMode::HttpLongPoll && !has_salt {
            return false;
        }

        // http_wait
        if self.mode == SessionMode::HttpLongPoll {
            return true;
        }
        // queries and acks (+ resend & get_info)
        if has_salt && self.force_send_at != 0.0 {
            if time::now_cached() > self.force_send_at {
                return true;
            } else {
                self.flush_packet_at = self.force_send_at;
            }
        }

        // ping
        if has_salt {
            if self.must_ping() {
                return true;
            }
            let at = self.last_ping_at + self.ping_must_delay();
            if self.flush_packet_at == 0.0 || self.flush_packet_at > at {
                self.flush_packet_at = at;
            }
        }
        // get_future_salt
        if !has_salt {
            if self.last_get_future_salt_at == 0.0 {
                return true;
            }
            let get_future_salts_at = self.last_get_future_salt_at + 60.0;
            if get_future_salts_at < time::now_cached() {
                return true;
            }
            if self.flush_packet_at == 0.0 || self.flush_packet_at > get_future_salts_at {
                self.flush_packet_at = get_future_salts_at;
            }
        }

        if has_salt && self.need_destroy_auth_key && !self.sent_destroy_auth_key {
            return true;
        }

        false
    }

    fn flush_packet(&mut self) {
        let has_salt = self.auth_data.has_salt(time::now_cached());
        // ping
        let mut container_message_id = MessageId::default();
        let mut ping_id = 0i64;
        if has_salt && self.may_ping() {
            self.last_ping_at = time::now_cached();
            ping_id = self.auth_data.next_message_id(self.last_ping_at).get() as i64;
        }

        // http_wait
        let mut max_delay = -1;
        let mut max_after = -1;
        let mut max_wait = -1;
        if self.mode == SessionMode::HttpLongPoll {
            max_delay = HTTP_MAX_DELAY;
            max_after = HTTP_MAX_AFTER;
            let time_to_disconnect = (self.ping_disconnect_delay() + self.last_pong_at)
                .min(self.read_disconnect_delay() + self.last_read_at)
                - time::now_cached();
            max_wait = (1000.0 * (time_to_disconnect - self.rtt()).clamp(0.1, self.http_max_wait()))
                as i32;
        } else if self.mode == SessionMode::Http {
            max_delay = HTTP_MAX_DELAY;
            max_after = HTTP_MAX_AFTER;
            max_wait = 0;
        }

        // future salts
        let mut future_salt_n = 0;
        if self.mode != SessionMode::HttpLongPoll
            && self.auth_data.need_future_salts(time::now_cached())
            && (self.last_get_future_salt_at == 0.0
                || self.last_get_future_salt_at + 60.0 < time::now_cached())
        {
            self.last_get_future_salt_at = time::now_cached();
            future_salt_n = 64;
        }

        const MAX_QUERY_COUNT: usize = 1000;
        let mut send_till = 0usize;
        let mut send_size = 0usize;
        if has_salt {
            // send at most MAX_QUERY_COUNT queries, of total size up to 2^15
            while send_till < self.to_send.len()
                && send_till < MAX_QUERY_COUNT
                && send_size < (1 << 15)
            {
                send_size += self.to_send[send_till].packet.size();
                send_till += 1;
            }
        }
        let queries: Vec<MtprotoQuery> = if send_till == self.to_send.len() {
            std::mem::take(&mut self.to_send)
        } else {
            self.to_send.drain(..send_till).collect()
        };

        let destroy_auth_key = self.need_destroy_auth_key && !self.sent_destroy_auth_key;

        if queries.is_empty()
            && self.to_ack_message_ids.is_empty()
            && ping_id == 0
            && max_delay < 0
            && future_salt_n == 0
            && self.to_resend_answer_message_ids.is_empty()
            && self.to_cancel_answer_message_ids.is_empty()
            && self.to_get_state_info_message_ids.is_empty()
            && !destroy_auth_key
        {
            self.force_send_at = 0.0;
            return;
        }

        if destroy_auth_key && !self.sent_destroy_auth_key {
            self.sent_destroy_auth_key = true;
            self.destroy_auth_key_send_time = time::now();
        }

        fn cut_tail(message_ids: &mut Vec<MessageId>, size: usize) -> Vec<i64> {
            if size >= message_ids.len() {
                let result: Vec<i64> = message_ids.iter().map(|id| id.get() as i64).collect();
                message_ids.clear();
                return result;
            }
            tdutils::log_warning!(
                "Too many message identifiers in container: {} instead of {size}",
                message_ids.len()
            );
            let new_size = message_ids.len() - size;
            let result: Vec<i64> = message_ids[new_size..]
                .iter()
                .map(|id| id.get() as i64)
                .collect();
            message_ids.truncate(new_size);
            result
        }

        // no more than 8192 message identifiers per container..
        let to_resend_answer = cut_tail(&mut self.to_resend_answer_message_ids, 8192);
        let mut resend_answer_message_id = MessageId::default();
        let to_cancel_answer = cut_tail(
            &mut self.to_cancel_answer_message_ids,
            MAX_QUERY_COUNT - queries.len(),
        );
        let to_get_state_info = cut_tail(&mut self.to_get_state_info_message_ids, 8192);
        let mut get_state_info_message_id = MessageId::default();
        let to_ack = cut_tail(&mut self.to_ack_message_ids, 8192);
        let mut ping_message_id = MessageId::default();

        let use_quick_ack = queries.iter().any(|query| query.use_quick_ack);

        let header = self.auth_data.get_header().to_vec();
        let (packet_data, parent_message_id) = build_crypto_packet(
            &queries,
            &header,
            &to_ack,
            ping_id,
            (self.ping_disconnect_delay() + 2.0) as i32,
            max_delay,
            max_after,
            max_wait,
            future_salt_n,
            &to_get_state_info,
            &to_resend_answer,
            &to_cancel_answer,
            destroy_auth_key,
            &mut self.auth_data,
            &mut container_message_id,
            &mut get_state_info_message_id,
            &mut resend_answer_message_id,
            &mut ping_message_id,
        );

        let quick_ack_token = if use_quick_ack {
            parent_message_id.get()
        } else {
            0
        };
        self.send_crypto(&packet_data, quick_ack_token);

        if resend_answer_message_id != MessageId::default() {
            self.service_queries.insert(
                resend_answer_message_id.get(),
                ServiceQuery {
                    type_: ServiceQueryType::ResendAnswer,
                    container_message_id,
                    msg_ids: to_resend_answer,
                },
            );
        }
        if get_state_info_message_id != MessageId::default() {
            self.service_queries.insert(
                get_state_info_message_id.get(),
                ServiceQuery {
                    type_: ServiceQueryType::GetStateInfo,
                    container_message_id,
                    msg_ids: to_get_state_info,
                },
            );
        }
        if ping_id != 0 {
            self.last_ping_container_message_id = container_message_id;
            self.last_ping_message_id = ping_message_id;
        }

        if container_message_id != MessageId::default() {
            let message_ids: Vec<MessageId> = queries.iter().map(|q| q.message_id).collect();
            self.with_callback(|callback| {
                callback.on_container_sent(container_message_id, message_ids)
            });

            if resend_answer_message_id != MessageId::default() {
                self.container_to_service_message_id
                    .entry(container_message_id.get())
                    .or_default()
                    .push(resend_answer_message_id);
            }
            if get_state_info_message_id != MessageId::default() {
                self.container_to_service_message_id
                    .entry(container_message_id.get())
                    .or_default()
                    .push(get_state_info_message_id);
            }
        }

        if self.to_send.is_empty()
            && self.to_ack_message_ids.is_empty()
            && self.to_get_state_info_message_ids.is_empty()
            && self.to_resend_answer_message_ids.is_empty()
            && self.to_cancel_answer_message_ids.is_empty()
        {
            self.force_send_at = 0.0;
        }
    }

    fn reset_server_time_difference(&mut self, message_id: MessageId) {
        self.auth_data
            .reset_server_time_difference((message_id.get() >> 32) as u32 as f64 - time::now());
        self.with_callback(|callback| callback.on_server_time_difference_updated(true));
    }

    // ---- parsing ----

    fn parse_message(
        parser: &mut tl::Reader<'_>,
        info: &mut MsgInfo,
        packet: &mut BufferSlice,
    ) -> Status {
        // msg_id:long seqno:int bytes:int
        if parser.remaining() < 16 {
            return Status::error_message(
                "Failed to parse mtproto_api::message: Not enough data to read",
            );
        }
        info.message_id = MessageId::new(parser.i64().unwrap() as u64);
        info.seq_no = parser.i32().unwrap();
        let bytes = parser.i32().unwrap() as u32;

        if !bytes.is_multiple_of(4) {
            return Status::error_message(&format!(
                "Failed to parse mtproto_api::message: size of message [{bytes}] is not divisible by 4"
            ));
        }
        match parser.raw(bytes as usize) {
            Ok(data) => {
                *packet = BufferSlice::from_slice_bytes(data);
                info.size = bytes as usize;
                Status::ok()
            }
            Err(error) => {
                Status::error_message(&format!("Failed to parse mtproto_api::message: {error}"))
            }
        }
    }

    fn on_slice_packet(&mut self, info: &MsgInfo, packet: &[u8]) -> Status {
        if info.seq_no & 1 != 0 {
            self.send_ack(info.message_id);
        }
        if packet.len() < 4 {
            let status = Status::error_message(&format!("Receive packet of size {}", packet.len()));
            self.with_callback(|callback| {
                callback.on_session_failed(Status::error_message("Receive too small packet"))
            });
            return status;
        }

        let constructor_id = i32::from_le_bytes(packet[..4].try_into().unwrap());
        if constructor_id == MSG_CONTAINER_ID {
            return self.on_packet_container(info, &packet[4..]);
        }
        if constructor_id == RPC_RESULT_ID {
            return self.on_packet_rpc_result(info, &packet[4..]);
        }

        let status = self.on_typed_packet(info, constructor_id, &packet[4..]);
        if status.message() != "not-a-service-packet" {
            return status;
        }

        // It is an update... I hope.
        let update_status = self.auth_data.check_update(info.message_id);
        let recheck_status = self.auth_data.recheck_update(info.message_id);
        if recheck_status.is_error() && recheck_status.code() == 2 {
            tdutils::log_warning!("Receive very old update");
        }
        if update_status.is_error() {
            if update_status.code() == 2 {
                tdutils::log_warning!("Receive too old update: {}", update_status.message());
                let status = Status::error_message("Receive too old update");
                self.with_callback(|callback| callback.on_session_failed(status.clone()));
                return status;
            }
            return Status::ok(); // Skip duplicate update.
        }
        self.with_callback(|callback| callback.on_update(BufferSlice::from_slice_bytes(packet)))
    }

    fn on_typed_packet(&mut self, info: &MsgInfo, constructor_id: i32, data: &[u8]) -> Status {
        use mtproto_api::*;
        // `data` arrives with the constructor already consumed by
        // on_slice_packet, so the bare (fields-only) parse is required —
        // the boxed parse would re-check the consumed id and fail.
        macro_rules! parse {
            ($ty:ident) => {{
                match $ty::parse_bare(data) {
                    Ok(m) => m,
                    Err(e) => return Status::error_message(e),
                }
            }};
        }
        match constructor_id {
            NewSession::ID => {
                let msg = parse!(NewSession);
                let mut first_message_id = MessageId::new(msg.first_msg_id as u64);
                if let Some(query) = self.service_queries.get(&first_message_id.get()) {
                    first_message_id = query.container_message_id;
                }
                self.with_callback(|callback| {
                    callback.on_new_session_created(msg.unique_id as u64, first_message_id)
                });
                Status::ok()
            }
            bad_msg_notification::ID => {
                let msg = parse!(bad_msg_notification);
                let bad_info = MsgInfo {
                    message_id: MessageId::new(msg.bad_msg_id as u64),
                    seq_no: msg.bad_msg_seqno,
                    size: 0,
                };
                match msg.error_code {
                    16 => {
                        // MsgIdTooLow
                        tdutils::log_warning!("MessageId is too low. Message will be re-sent");
                        self.on_message_failed(
                            bad_info.message_id,
                            Status::error_message("MessageId is too low"),
                        );
                        Status::ok()
                    }
                    17 => {
                        // MsgIdTooHigh
                        tdutils::log_warning!("MessageId is too high. Session will be closed");
                        self.to_send.clear();
                        self.reset_server_time_difference(info.message_id);
                        let status = Status::error_message("MessageId is too high");
                        self.with_callback(|callback| callback.on_session_failed(status.clone()));
                        status
                    }
                    18 => Status::error_message("MessageId is not divisible by 4"),
                    19 => Status::error_message("Container and older message MessageId collision"),
                    20 => {
                        // MsgIdTooOld
                        tdutils::log_warning!("MessageId is too old. Message will be re-sent");
                        self.on_message_failed(
                            bad_info.message_id,
                            Status::error_message("MessageId is too old"),
                        );
                        Status::ok()
                    }
                    32 => Status::error_message("SeqNo is too low"),
                    33 => Status::error_message("SeqNo is too high"),
                    34 => Status::error_message("SeqNo is not even for an irrelevant message"),
                    35 => Status::error_message("SeqNo is not odd for a relevant message"),
                    64 => Status::error_message("Invalid Container"),
                    _ => Status::error_message("Unknown error code"),
                }
            }
            bad_server_salt::ID => {
                let msg = parse!(bad_server_salt);
                self.auth_data
                    .set_server_salt(msg.new_server_salt, time::now_cached());
                self.with_callback(|callback| callback.on_server_salt_updated());
                self.on_message_failed(
                    MessageId::new(msg.bad_msg_id as u64),
                    Status::error_message("Bad server salt"),
                );
                Status::ok()
            }
            MsgsAck::ID => {
                let msg = parse!(MsgsAck);
                for message_id in &msg.msg_ids {
                    let message_id = MessageId::new(*message_id as u64);
                    self.with_callback(|callback| callback.on_message_ack(message_id));
                }
                Status::ok()
            }
            GzipPacked::ID => {
                let msg = parse!(GzipPacked);
                let res = gzdecode(&msg.packed_data);
                self.on_slice_packet(info, res.as_slice())
            }
            Pong::ID => {
                let msg = parse!(Pong);
                if info.message_id.get() < (msg.msg_id as u64).wrapping_sub(15u64 << 32) {
                    self.reset_server_time_difference(info.message_id);
                }

                if self.sent_destroy_auth_key
                    && self.destroy_auth_key_send_time < time::now() - 60.0
                {
                    return Status::error_message(&format!(
                        "No response for destroy_auth_key sent from auth key {}",
                        self.auth_data.get_auth_key().id()
                    ));
                }

                self.last_pong_at = time::now_cached();
                self.real_last_pong_at = self.last_pong_at;
                let get_time = |msg_id: i64| msg_id as f64 / (1u64 << 32) as f64;
                let current_time = self.auth_data.get_server_time(time::now_cached());
                self.with_callback(|callback| {
                    callback.on_pong(
                        get_time(msg.ping_id),
                        get_time(info.message_id.get() as i64),
                        current_time,
                    )
                })
            }
            FutureSalts::ID => {
                let msg = parse!(FutureSalts);
                let new_salts: Vec<crate::auth_data::ServerSalt> = msg
                    .salts
                    .iter()
                    .map(|it| crate::auth_data::ServerSalt {
                        salt: it.salt,
                        valid_since: it.valid_since as f64,
                        valid_until: it.valid_until as f64,
                    })
                    .collect();
                let now = time::now_cached();
                self.auth_data.set_future_salts(new_salts, now);
                self.with_callback(|callback| callback.on_server_salt_updated());
                Status::ok()
            }
            MsgsStateInfo::ID => {
                let msg = parse!(MsgsStateInfo);
                let message_id = MessageId::new(msg.req_msg_id as u64);
                let query = match self.service_queries.remove(&message_id.get()) {
                    Some(query) => query,
                    None => return Status::error_message("Unknown msgs_state_info"),
                };
                if query.type_ != ServiceQueryType::GetStateInfo {
                    return Status::error_message(
                        "Receive msgs_state_info in response not to GetStateInfo",
                    );
                }
                self.on_msgs_state_info(&query.msg_ids, &msg.info)
            }
            MsgsAllInfo::ID => {
                let msg = parse!(MsgsAllInfo);
                self.on_msgs_state_info(&msg.msg_ids, &msg.info)
            }
            msg_detailed_info::ID => {
                let msg = parse!(msg_detailed_info);
                self.with_callback(|callback| {
                    callback.on_message_info(
                        MessageId::new(msg.msg_id as u64),
                        msg.status,
                        MessageId::new(msg.answer_msg_id as u64),
                        msg.bytes,
                        2,
                    )
                });
                Status::ok()
            }
            msg_new_detailed_info::ID => {
                let msg = parse!(msg_new_detailed_info);
                self.with_callback(|callback| {
                    callback.on_message_info(
                        MessageId::default(),
                        0,
                        MessageId::new(msg.answer_msg_id as u64),
                        msg.bytes,
                        0,
                    )
                });
                Status::ok()
            }
            destroy_auth_key_ok::ID | destroy_auth_key_none::ID | destroy_auth_key_fail::ID => {
                if !self.need_destroy_auth_key {
                    tdutils::log_error!("Receive unexpected destroy_auth_key response");
                    return Status::ok();
                }
                self.with_callback(|callback| callback.on_destroy_auth_key())
            }
            _ => Status::error_message("not-a-service-packet"),
        }
    }

    fn on_message_failed(&mut self, message_id: MessageId, status: Status) {
        self.with_callback(|callback| callback.on_message_failed(message_id, status.clone()));

        self.sent_destroy_auth_key = false;
        self.destroy_auth_key_send_time = 0.0;

        if message_id == self.last_ping_message_id
            || message_id == self.last_ping_container_message_id
        {
            // restart ping immediately
            self.last_ping_at = 0.0;
            self.last_ping_message_id = MessageId::default();
            self.last_ping_container_message_id = MessageId::default();
        }

        let inner = if let Some(message_ids) =
            self.container_to_service_message_id.get(&message_id.get())
        {
            message_ids.clone()
        } else {
            vec![message_id]
        };
        for inner_message_id in inner {
            let query = match self.service_queries.remove(&inner_message_id.get()) {
                Some(query) => query,
                None => continue,
            };
            match query.type_ {
                ServiceQueryType::ResendAnswer => {
                    for msg_id in query.msg_ids {
                        self.resend_answer(MessageId::new(msg_id as u64));
                    }
                }
                ServiceQueryType::GetStateInfo => {
                    for msg_id in query.msg_ids {
                        self.get_state_info(MessageId::new(msg_id as u64));
                    }
                }
            }
        }
        let _ = status;
    }

    fn on_msgs_state_info(&mut self, msg_ids: &[i64], info: &[u8]) -> Status {
        if msg_ids.len() != info.len() {
            return Status::error_message(&format!(
                "message count {} != info.size() {}",
                msg_ids.len(),
                info.len()
            ));
        }
        for (i, msg_id) in msg_ids.iter().enumerate() {
            self.with_callback(|callback| {
                callback.on_message_info(
                    MessageId::new(*msg_id as u64),
                    info[i] as i32,
                    MessageId::default(),
                    0,
                    1,
                )
            });
        }
        Status::ok()
    }

    fn on_packet_container(&mut self, info: &MsgInfo, packet: &[u8]) -> Status {
        let old_container_message_id = self.container_message_id;
        self.container_message_id = info.message_id;
        let mut parser = tl::Reader::new(packet);
        let size = match parser.i32() {
            Ok(size) => size,
            Err(error) => {
                self.container_message_id = old_container_message_id;
                return Status::error_message(&format!(
                    "Failed to parse mtproto_api::rpc_container: {error}"
                ));
            }
        };
        for _ in 0..size {
            let mut inner_info = MsgInfo {
                message_id: MessageId::default(),
                seq_no: 0,
                size: 0,
            };
            let mut inner_packet = BufferSlice::null();
            let status = Self::parse_message(&mut parser, &mut inner_info, &mut inner_packet);
            if status.is_error() {
                self.container_message_id = old_container_message_id;
                return status;
            }
            let status = self.on_slice_packet(&inner_info, inner_packet.as_slice());
            if status.is_error() {
                self.container_message_id = old_container_message_id;
                return status;
            }
        }
        self.container_message_id = old_container_message_id;
        Status::ok()
    }

    fn on_packet_rpc_result(&mut self, info: &MsgInfo, packet: &[u8]) -> Status {
        use mtproto_api::*;
        if packet.len() < 8 {
            return Status::error_message(
                "Failed to parse mtproto_api::rpc_result: Not enough data to read",
            );
        }
        let req_msg_id = u64::from_le_bytes(packet[..8].try_into().unwrap());
        if req_msg_id == 0 {
            tdutils::log_error!("Receive an update in rpc_result");
            return Status::error_message("Receive an update in rpc_result");
        }

        if info.message_id.get() < req_msg_id.wrapping_sub(15u64 << 32) {
            self.reset_server_time_difference(info.message_id);
        }

        let constructor_id = i32::from_le_bytes(packet[8..12].try_into().unwrap());
        if constructor_id == RpcError::ID {
            let msg = match RpcError::parse(&packet[8..]) {
                Ok(m) => m,
                Err(error) => return Status::error_message(error),
            };
            self.with_callback(|callback| {
                callback.on_message_result_error(
                    MessageId::new(req_msg_id),
                    msg.error_code,
                    String::from_utf8_lossy(&msg.error_message).into_owned(),
                )
            });
            Status::ok()
        } else if constructor_id == mtproto_api::GzipPacked::ID {
            let msg = match mtproto_api::GzipPacked::parse(&packet[8..]) {
                Ok(m) => m,
                Err(error) => return Status::error_message(error),
            };
            // yep, gzip in rpc_result
            let object = gzdecode(&msg.packed_data);
            self.with_callback(|callback| {
                callback.on_message_result_ok(MessageId::new(req_msg_id), object, info.size)
            })
        } else {
            self.with_callback(|callback| {
                callback.on_message_result_ok(
                    MessageId::new(req_msg_id),
                    BufferSlice::from_slice_bytes(&packet[8..]),
                    info.size,
                )
            })
        }
    }

    /// `SessionConnection::flush`: the synchronous C++ packet logic plus
    /// the async socket I/O. Returns the next wakeup timestamp (0 = closed).
    pub async fn flush(&mut self, callback: &mut dyn SessionConnectionCallback) -> f64 {
        // SAFETY: the pointer is only dereferenced while `flush` runs (the
        // callback outlives the call); the transmute erases the borrow
        // lifetime, exactly like the C++ pointer.
        self.callback = unsafe {
            std::mem::transmute::<
                &mut dyn SessionConnectionCallback,
                *mut (dyn SessionConnectionCallback + 'static),
            >(callback)
        };
        let status = self.do_flush().await;
        // check error
        if status.is_error() {
            tdutils::log_debug!("Close session because of {}", status.message());
            self.do_close(status);
            return 0.0;
        }

        let mut wakeup_at = 0.0;
        // three independent timeouts
        // 1. close connection after ping_disconnect_delay() after last pong
        // 2. close connection after read_disconnect_delay() after last read
        // 3. the one returned by must_flush_packet
        fn relax(wakeup_at: &mut f64, at: f64) {
            if *wakeup_at == 0.0 || *wakeup_at > at {
                *wakeup_at = at;
            }
        }
        relax(
            &mut wakeup_at,
            self.last_pong_at + self.ping_disconnect_delay() + 0.002,
        );
        relax(
            &mut wakeup_at,
            self.last_read_at + self.read_disconnect_delay() + 0.002,
        );
        relax(&mut wakeup_at, self.flush_packet_at);

        wakeup_at
    }

    pub fn force_close(&mut self, callback: &mut dyn SessionConnectionCallback) {
        assert_ne!(self.state, SessionState::Closed);
        // SAFETY: the pointer is only dereferenced inside `do_close` below.
        self.callback = unsafe {
            std::mem::transmute::<
                &mut dyn SessionConnectionCallback,
                *mut (dyn SessionConnectionCallback + 'static),
            >(callback)
        };
        self.do_close(Status::ok());
    }

    async fn do_flush(&mut self) -> Status {
        assert_ne!(self.state, SessionState::Closed);
        if self.state == SessionState::Init {
            self.last_pong_at = time::now_cached();
            self.last_read_at = time::now_cached();
            self.state = SessionState::Run;
        }
        if !self.auth_data.has_auth_key(time::now_cached()) {
            return Status::error_message("No auth key");
        }

        self.last_read_size = 0;
        self.last_write_size = 0;
        let start_time = time::now();
        let auth_key = self.auth_data.get_auth_key().clone();
        // The raw connection and the callback borrow disjoint parts of this
        // struct; the C++ holds raw pointers for exactly this reason, and
        // the split borrow is not expressible in safe Rust.
        let raw = &mut self.raw_connection as *mut RawConnection;
        // SAFETY: `raw` aliases only `self.raw_connection`; the callback
        // (`self`) touches the other fields, never the connection itself.
        let result = unsafe { (&mut *raw).flush(&auth_key, self).await };
        let elapsed_time = time::now() - start_time;
        if elapsed_time >= 0.1 {
            tdutils::log_warning!(
                "RawConnection::flush took {elapsed_time} seconds, written {}, read {} and returned {}",
                self.last_write_size,
                self.last_read_size,
                result.message()
            );
        }
        if result.is_error() {
            return result;
        }

        if self.last_pong_at + self.ping_disconnect_delay() < time::now_cached() {
            return Status::error_message(&format!(
                "Ping timeout of {} seconds expired; last pong was received",
                self.ping_disconnect_delay()
            ));
        }

        if self.last_read_at + self.read_disconnect_delay() < time::now_cached() {
            return Status::error_message(&format!(
                "Read timeout of {} seconds expired; last read was received",
                self.read_disconnect_delay()
            ));
        }

        Status::ok()
    }
}

impl RawConnectionCallback for SessionConnection {
    fn on_raw_packet(&mut self, packet_info: &PacketInfo, packet: BufferSlice) -> Status {
        let old_main_message_id = self.main_message_id;
        self.main_message_id = packet_info.message_id;

        let status = self.on_raw_packet_impl(packet_info, packet);
        self.main_message_id = old_main_message_id;
        status
    }

    fn on_quick_ack(&mut self, quick_ack_token: u64) -> Status {
        self.with_callback(|callback| callback.on_message_ack(MessageId::new(quick_ack_token)));
        Status::ok()
    }

    fn before_write(&mut self) -> Status {
        assert!(!self.raw_connection_moved());
        while self.must_flush_packet() {
            self.flush_packet();
        }
        Status::ok()
    }

    fn on_read(&mut self, size: usize) {
        self.last_read_at = time::now_cached();
        self.real_last_read_at = self.last_read_at;
        self.last_read_size += size as u64;
    }
}

impl SessionConnection {
    fn raw_connection_moved(&self) -> bool {
        false
    }

    fn on_raw_packet_impl(&mut self, packet_info: &PacketInfo, packet: BufferSlice) -> Status {
        if packet_info.no_crypto_flag {
            return Status::error_message("Unexpected unencrypted packet");
        }

        let (status, time_difference_was_updated) = self.auth_data.check_packet(
            packet_info.session_id,
            packet_info.message_id,
            time::now_cached(),
        );
        if time_difference_was_updated {
            self.with_callback(|callback| callback.on_server_time_difference_updated(false));
        }
        if status.is_error() {
            if status.code() == 1 {
                tdutils::log_info!("Packet is ignored: {}", status.message());
                self.send_ack(packet_info.message_id);
                return Status::ok();
            } else if status.code() == 2 {
                tdutils::log_warning!("Receive too old packet: {}", status.message());
                let status = Status::error_message("Receive too old packet");
                self.with_callback(|callback| callback.on_session_failed(status.clone()));
                return status;
            } else {
                return status;
            }
        }

        self.on_main_packet(packet_info, packet)
    }

    fn on_main_packet(&mut self, packet_info: &PacketInfo, packet: BufferSlice) -> Status {
        // Update pong here too. Real pong can be delayed by many big packets
        self.last_pong_at = time::now_cached();
        self.real_last_pong_at = self.last_pong_at;

        if !self.connected_flag {
            self.connected_flag = true;
            self.with_callback(|callback| callback.on_connected());
        }

        let mut parser = tl::Reader::new(packet.as_slice());
        let mut info = MsgInfo {
            message_id: MessageId::default(),
            seq_no: 0,
            size: 0,
        };
        let mut inner = BufferSlice::null();
        let status = Self::parse_message(&mut parser, &mut info, &mut inner);
        if status.is_error() {
            return status;
        }
        let status = self.on_slice_packet(&info, inner.as_slice());
        if status.is_error() {
            return status;
        }
        if parser.remaining() != 0 {
            return Status::error_message(&format!(
                "Failed to parse packet: {} bytes left",
                parser.remaining()
            ));
        }
        let _ = packet_info;
        Status::ok()
    }
}

/// `rpc_result` constructor id (`mtproto_api::rpc_result`).
/// `mtproto_api::msg_container` — a hand-written stub in the C++ too (the
/// scheme comments the constructor out; the session parses containers
/// manually).
const MSG_CONTAINER_ID: i32 = 0x73f1_f8dc;
/// `mtproto_api::rpc_result` — the same stub pattern.
const RPC_RESULT_ID: i32 = -212_046_591;

/// A null callback pointer (raw trait-object pointers cannot go through
/// `std::ptr::null_mut` because of the Sized bound).
#[allow(invalid_value)]
fn null_callback() -> *mut (dyn SessionConnectionCallback + 'static) {
    // SAFETY: a (null-data, null-vtable) fat pointer is only ever compared
    // against null.
    unsafe { std::mem::transmute::<(usize, usize), _>((0, 0)) }
}

/// Port of `CryptoImpl` (`td/mtproto/CryptoStorer.h`): builds the outgoing
/// packet — one query/ack/ping/... object or a `msg_container` of them.
#[allow(clippy::too_many_arguments)]
fn build_crypto_packet(
    queries: &[MtprotoQuery],
    header: &[u8],
    to_ack: &[i64],
    ping_id: i64,
    ping_timeout: i32,
    max_delay: i32,
    max_after: i32,
    max_wait: i32,
    future_salt_n: i32,
    get_info: &[i64],
    resend: &[i64],
    cancel: &[i64],
    destroy_key: bool,
    auth_data: &mut AuthData,
    container_message_id: &mut MessageId,
    get_info_message_id: &mut MessageId,
    resend_message_id: &mut MessageId,
    ping_message_id: &mut MessageId,
) -> (Vec<u8>, MessageId) {
    use mtproto_api::*;

    /// `ObjectImpl::do_store`: message_id + seq_no + len + object.
    fn wrap_object(auth_data: &mut AuthData, body: &[u8]) -> (Vec<u8>, MessageId) {
        let message_id = auth_data.next_message_id(time::now_cached());
        let seq_no = auth_data.next_seq_no(false);
        let mut out = Vec::with_capacity(16 + body.len());
        out.extend_from_slice(&message_id.get().to_le_bytes());
        out.extend_from_slice(&seq_no.to_le_bytes());
        out.extend_from_slice(&(body.len() as i32).to_le_bytes());
        out.extend_from_slice(body);
        (out, message_id)
    }

    let now = time::now_cached();
    let mut parts: Vec<u8> = Vec::new();
    let mut count = 0i32;
    let mut parent_message_id = MessageId::default();

    // The parent-message precedence chain of the C++ Only* ladder.
    if !queries.is_empty() {
        parent_message_id = queries[0].message_id;
    }

    // queries (QueryImpl: msg_id, seq_no, len, invokeAfter + header + body)
    for query in queries {
        count += 1;
        let mut body = Vec::new();
        let invoke_after = &query.invoke_after_message_ids;
        if !invoke_after.is_empty() {
            if invoke_after.len() == 1 {
                body.extend_from_slice(&0xcb9f_372du32.to_le_bytes());
                body.extend_from_slice(&invoke_after[0].get().to_le_bytes());
            } else {
                body.extend_from_slice(&0x3dc4_b4f0u32.to_le_bytes());
                body.extend_from_slice(&tl::VECTOR_ID.to_le_bytes());
                body.extend_from_slice(&(invoke_after.len() as i32).to_le_bytes());
                for id in invoke_after {
                    body.extend_from_slice(&id.get().to_le_bytes());
                }
            }
        }
        body.extend_from_slice(header);
        if query.gzip_flag {
            let mut g = Vec::new();
            GzipPacked {
                packed_data: query.packet.as_slice().to_vec(),
            }
            .store_boxed(&mut g);
            body.extend_from_slice(&g);
        } else {
            body.extend_from_slice(query.packet.as_slice());
        }
        let mut wrapped = Vec::with_capacity(16 + body.len());
        wrapped.extend_from_slice(&query.message_id.get().to_le_bytes());
        wrapped.extend_from_slice(&query.seq_no.to_le_bytes());
        wrapped.extend_from_slice(&(body.len() as i32).to_le_bytes());
        wrapped.extend_from_slice(&body);
        parts.extend_from_slice(&wrapped);
    }

    // ack
    if !to_ack.is_empty() {
        count += 1;
        let mut body = Vec::new();
        MsgsAck {
            msg_ids: to_ack.to_vec(),
        }
        .store_boxed(&mut body);
        let (wrapped, message_id) = wrap_object(auth_data, &body);
        parts.extend_from_slice(&wrapped);
        if queries.is_empty() {
            parent_message_id = message_id;
        }
    }

    // http_wait
    if max_delay >= 0 {
        count += 1;
        let mut body = Vec::new();
        http_wait {
            max_delay,
            wait_after: max_after,
            max_wait,
        }
        .store_boxed(&mut body);
        let (wrapped, message_id) = wrap_object(auth_data, &body);
        parts.extend_from_slice(&wrapped);
        if queries.is_empty() && to_ack.is_empty() {
            parent_message_id = message_id;
        }
    }

    // get_future_salts
    if future_salt_n > 0 {
        count += 1;
        let mut body = Vec::new();
        get_future_salts { num: future_salt_n }.store_boxed(&mut body);
        let (wrapped, message_id) = wrap_object(auth_data, &body);
        parts.extend_from_slice(&wrapped);
        if queries.is_empty() && to_ack.is_empty() && max_delay < 0 {
            parent_message_id = message_id;
        }
    }

    // get_info
    if !get_info.is_empty() {
        count += 1;
        let mut body = Vec::new();
        MsgsStateReq {
            msg_ids: get_info.to_vec(),
        }
        .store_boxed(&mut body);
        let (wrapped, message_id) = wrap_object(auth_data, &body);
        parts.extend_from_slice(&wrapped);
        *get_info_message_id = message_id;
        if queries.is_empty() && to_ack.is_empty() && max_delay < 0 && future_salt_n == 0 {
            parent_message_id = message_id;
        }
    }

    // resend
    if !resend.is_empty() {
        count += 1;
        let mut body = Vec::new();
        MsgResendReq {
            msg_ids: resend.to_vec(),
        }
        .store_boxed(&mut body);
        let (wrapped, message_id) = wrap_object(auth_data, &body);
        parts.extend_from_slice(&wrapped);
        *resend_message_id = message_id;
        if queries.is_empty()
            && to_ack.is_empty()
            && max_delay < 0
            && future_salt_n == 0
            && get_info.is_empty()
        {
            parent_message_id = message_id;
        }
    }

    // cancel (one rpc_drop_answer object per id)
    for request_id in cancel {
        count += 1;
        let mut body = Vec::new();
        rpc_drop_answer {
            req_msg_id: *request_id,
        }
        .store_boxed(&mut body);
        let (wrapped, message_id) = wrap_object(auth_data, &body);
        parts.extend_from_slice(&wrapped);
        if queries.is_empty()
            && to_ack.is_empty()
            && max_delay < 0
            && future_salt_n == 0
            && get_info.is_empty()
            && resend.is_empty()
        {
            parent_message_id = message_id;
        }
    }

    // destroy
    if destroy_key {
        count += 1;
        let mut body = Vec::new();
        destroy_auth_key {}.store_boxed(&mut body);
        let (wrapped, message_id) = wrap_object(auth_data, &body);
        parts.extend_from_slice(&wrapped);
        if queries.is_empty()
            && to_ack.is_empty()
            && max_delay < 0
            && future_salt_n == 0
            && get_info.is_empty()
            && resend.is_empty()
            && cancel.is_empty()
        {
            parent_message_id = message_id;
        }
    }

    // ping (last in the container, per the C++ concatenation order)
    if ping_id != 0 {
        count += 1;
        let mut body = Vec::new();
        ping_delay_disconnect {
            ping_id,
            disconnect_delay: ping_timeout,
        }
        .store_boxed(&mut body);
        let (wrapped, message_id) = wrap_object(auth_data, &body);
        parts.extend_from_slice(&wrapped);
        *ping_message_id = message_id;
        if queries.is_empty()
            && to_ack.is_empty()
            && max_delay < 0
            && future_salt_n == 0
            && get_info.is_empty()
            && resend.is_empty()
            && cancel.is_empty()
            && !destroy_key
        {
            parent_message_id = message_id;
        }
    }

    if count > 1
        || (!queries.is_empty() && !auth_data.is_valid_outbound_msg_id(queries[0].message_id, now))
    {
        // Mixed: wrap everything into a msg_container.
        let message_id = auth_data.next_message_id(now);
        let seq_no = auth_data.next_seq_no(false);
        *container_message_id = message_id;
        parent_message_id = message_id;
        let mut container = Vec::with_capacity(8 + parts.len());
        container.extend_from_slice(&MSG_CONTAINER_ID.to_le_bytes());
        container.extend_from_slice(&count.to_le_bytes());
        container.extend_from_slice(&parts);
        let mut out = Vec::with_capacity(16 + container.len());
        out.extend_from_slice(&message_id.get().to_le_bytes());
        out.extend_from_slice(&seq_no.to_le_bytes());
        out.extend_from_slice(&(container.len() as i32).to_le_bytes());
        out.extend_from_slice(&container);
        (out, parent_message_id)
    } else {
        // A single wrapped object, sent as-is.
        (parts, parent_message_id)
    }
}
