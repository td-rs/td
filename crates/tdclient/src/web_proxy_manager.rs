//! The web proxy carrier manager — `td/telegram/net/WebProxyManager.{h,cpp}`
//! (upstream `adfdfb71b` + the `42c0f2e77` CHECK riders, the #125 family).
//!
//! A web proxy (`proxyTypeWeb`) is a stock MTProxy reached through an HTTPS
//! relay page, which the application loads in a hidden web view (the
//! `webProxyCarrier` contract in td_api). MTProto connects to a listener on
//! `127.0.0.1` as if it were the MTProxy; every accepted socket becomes one
//! stream of the relay frame protocol, and the frames are exchanged with the
//! page through `updateWebProxyCarrierData`/`processWebProxyCarrierMessage`.
//!
//! The collapsed-world shape follows the manager conventions (the
//! [`crate::transcription_manager`] pattern): the carrier plane is a plain
//! struct over an env, the update fan-out rides the `Td` outbox
//! (`Td::web_proxy_updates`), and the deadlines fold into the `Td` timeout
//! slot (`WebProxyManager::next_deadline` +
//! `WebProxyManager::timeout_expired`).
//!
//! Ported as real logic: the endpoint/carrier/stream registries, the carrier
//! lifecycle (`open_carrier`/`close_carrier`/`fail_carrier` + the retry
//! ladder), the page handshake (`on_carrier_text` — the
//! `tproxy-android-init` JSON protocol), the relay frame codec
//! (`send_frame`/`on_carrier_binary`/`on_carrier_frame`), the window
//! bookkeeping, the request arms' entry points
//! (`process_carrier_message`/`report_carrier_error`), and the temporary
//! (test-proxy) carrier plane. The whole carrier plane is request-reachable
//! end to end: `enableProxy(web)` opens a carrier, the application feeds the
//! page messages through `processWebProxyCarrierMessage`, frames leave as
//! `updateWebProxyCarrierData`, and `disableProxy` closes it — no sockets
//! involved.
//!
//! `[blocked: net]` parked pieces (the loopback socket plane; the manager's
//! registry halves they drive are real):
//! - `WebProxyListener` (cpp:81-141) — the `ServerSocketFd` accept actor;
//!   its bind/accept half rides
//!   `WebProxyManagerEnv::open_web_proxy_listener`, which parks (the seam
//!   never answers `WebProxyManager::on_listener_started`, so the endpoint
//!   address promises resolve with the C++ LambdaPromise drop).
//! - `WebProxyStream` (cpp:143-232) — the per-socket flush/poll actor; its
//!   creation and the `open`/`add_send_window`/`write`/`close` closures ride
//!   the `stream_*` env seams (the CLOSING_STREAM_TIMEOUT drain timeout and
//!   the `yield` re-poll are the actor's own halves).
//! - `WebProxyManager::register_connection` (cpp:240-249) — the static
//!   `init_socket_address` + `expect_connection` hop; the `42c0f2e77`
//!   `ip_address.get_port() <= 0` gate rides the same seam.
//! - `WebProxyManager::on_connection_accepted` (cpp:454-478) — the
//!   peer-address gate (`init_peer_address` + the `127.0.0.1` check) needs
//!   the accepted socket; its registry halves live in the ported
//!   `WebProxyManager::expect_connection` and
//!   `WebProxyManager::accept_connection`.
//!
//! Collapsed-time notes: the C++ `update_timeout()` arms (cpp:1051-1079)
//! fold into the `Td` deadline poll; the `send_closure_later` output flushes
//! (cpp:975-981) fold into the end-of-entry-point
//! `WebProxyManager::flush_scheduled` — the same one-message-per-batch
//! observable for the request-driven flows, without cross-request coalescing
//! (the collapsed world runs one manager entry point per request anyway).

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use tdmtproto::utils::base64url_encode;
use tdutils::json::{JsonObject, JsonValue, JsonValueType, json_decode};
use tdutils::promise::Promise;
use tdutils::random::secure_bytes;
use tdutils::status::Status;

use crate::connection_creator::Proxy;
use crate::td_api;

/// `INITIAL_WINDOW` (h:71) — the per-stream send/receive window.
const INITIAL_WINDOW: u32 = 4 << 20;

/// The relay frame protocol: `type:u8 | stream_id:u24 | length:u32 |
/// payload`, all integers big-endian (cpp:31-40).
const FRAME_OPEN: u8 = 0x01;
const FRAME_DATA: u8 = 0x02;
const FRAME_CLOSE: u8 = 0x03;
const FRAME_WINDOW: u8 = 0x04;
const FRAME_PING: u8 = 0x05;
const FRAME_PONG: u8 = 0x06;
const FRAME_HELLO: u8 = 0x10;
const FRAME_WELCOME: u8 = 0x11;
const FRAME_BYE: u8 = 0x1f;

const FRAME_HEADER_SIZE: usize = 8;
const MAX_FRAME_PAYLOAD_SIZE: usize = 1 << 20;
const MAX_PING_PAYLOAD_SIZE: usize = 64;
const MAX_CARRIER_DATA_SIZE: usize = 1 << 20;
const MAX_CARRIER_TEXT_SIZE: usize = 1 << 16;
/// The per-`flush_read` chunk bound (cpp:47) — the stream seams' budget
/// (the `WebProxyStream::loop` read half; `[blocked: net]`).
#[allow(dead_code)]
const DATA_CHUNK_SIZE: usize = 1 << 16;
const MAX_STREAM_ID: u32 = (1 << 24) - 1;
/// The per-endpoint stream-count gate (cpp:483-485) — the accept half rides
/// the net seam.
#[cfg_attr(not(test), allow(dead_code))]
const MAX_STREAMS: usize = 64;
const MAX_CLOSED_STREAM_IDS: usize = 4096;
/// The window-return thresholds (cpp:50-51) — the stream-written half rides
/// the net seam.
#[cfg_attr(not(test), allow(dead_code))]
const WINDOW_FLUSH_SIZE: u32 = 256 << 10;
#[cfg_attr(not(test), allow(dead_code))]
const WINDOW_FLUSH_DELAY: f64 = 0.02;

const HANDSHAKE_TIMEOUT: f64 = 10.0;
const MAX_HANDSHAKE_TIME: f64 = 45.0;
const MAX_TEMPORARY_HANDSHAKE_TIME: f64 = 10.0;
const RESPONSE_TIMEOUT: f64 = 10.0;
const MIN_RETRY_DELAY: f64 = 2.0;
const MAX_RETRY_DELAY: f64 = 30.0;
const PAUSED_RETRY_DELAY: f64 = 30.0;
const TEMPORARY_ENDPOINT_LIFETIME: f64 = 120.0;
/// The port-registry timeouts (cpp:61-62) — the `register_connection` hop
/// rides the net seam.
#[cfg_attr(not(test), allow(dead_code))]
const PENDING_CONNECTION_TIMEOUT: f64 = 5.0;
#[cfg_attr(not(test), allow(dead_code))]
const EXPECTED_PORT_TIMEOUT: f64 = 60.0;
/// The released stream's drain timeout (cpp:59) — the `WebProxyStream::close`
/// seam's own half (`[blocked: net]`).
#[allow(dead_code)]
const CLOSING_STREAM_TIMEOUT: f64 = 5.0;

/// `BRIDGE_OBJECT_NAME` (cpp:63).
const BRIDGE_OBJECT_NAME: &str = "TelegramWebProxy";

/// `read_uint32` (cpp:71-74) — the big-endian frame integer.
fn read_uint32(data: &[u8]) -> u32 {
    ((data[0] as u32) << 24) | ((data[1] as u32) << 16) | ((data[2] as u32) << 8) | (data[3] as u32)
}

/// `get_json_string_field` (cpp:76-79) — a failed read is the empty string.
fn get_json_string_field(object: &JsonObject, name: &str) -> String {
    object
        .get_optional_string_field(name, "")
        .unwrap_or_else(|_| String::new())
}

/// `WebProxyManager::TemporaryCarrier` (h:43-46) — the test-proxy answer;
/// the C++ `IPAddress` rides as the socket-address string (`None` is the
/// invalid default).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TemporaryCarrier {
    pub endpoint_id: i64,
    pub ip_address: Option<String>,
}

/// `CarrierState` (h:73).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum CarrierState {
    #[default]
    None,
    WaitInit,
    WaitWelcome,
    Ready,
}

/// `Stream` (h:75-83) — the manager-side registry entry; the C++
/// `ActorOwn<WebProxyStream>` is the socket half (the `stream_*` env seams
/// are keyed by the stream key, the C++ link token).
#[derive(Debug)]
struct Stream {
    /// The owning endpoint — read by the stream-callback halves (the net
    /// seam's callers).
    #[cfg_attr(not(test), allow(dead_code))]
    endpoint_id: i64,
    stream_id: u32,
    receive_window: u32,
    pending_window: u32,
    is_opened: bool,
    has_received_data: bool,
}

/// `Endpoint` (h:85-110) — one web proxy's carrier world. The C++
/// `ActorOwn<WebProxyListener>` is the bound-socket half (the
/// `listener_exists` flag + the listener seam); the C++ `IPAddress` is the
/// `Option<String>` (`None` is the invalid default).
struct Endpoint {
    endpoint_id: i64,
    proxy_id: i32,
    proxy: Proxy,
    is_temporary: bool,

    listener_exists: bool,
    ip_address: Option<String>,
    address_promises: Vec<Promise<String>>,
    temporary_promise: Option<Promise<TemporaryCarrier>>,

    carrier_id: i32,
    state: CarrierState,
    nonce: String,
    opened_at: f64,
    handshake_deadline: f64,
    unanswered_since: f64,
    window_flush_at: f64,

    retry_at: f64,
    retry_delay: f64,
    is_paused: bool,
    is_unsupported: bool,

    streams: HashMap<u32, u64>,
    closed_stream_ids: HashSet<u32>,
    closed_stream_id_order: VecDeque<u32>,
    /// The stream-id allocator's cursor (cpp:503-511) — the accept half
    /// rides the net seam.
    #[cfg_attr(not(test), allow(dead_code))]
    next_stream_id: u32,

    output: Vec<u8>,
    is_output_flush_scheduled: bool,

    expires_at: f64,
}

impl Default for Endpoint {
    fn default() -> Self {
        // the C++ member initializers (h:85-110); notably `uint32
        // next_stream_id = 1` (h:112) — id 0 is the session-frame channel,
        // so the allocator must start at 1
        Self {
            endpoint_id: 0,
            proxy_id: 0,
            proxy: Proxy::default(),
            is_temporary: false,
            listener_exists: false,
            ip_address: None,
            address_promises: Vec::new(),
            temporary_promise: None,
            carrier_id: 0,
            state: CarrierState::None,
            nonce: String::new(),
            opened_at: 0.0,
            handshake_deadline: 0.0,
            unanswered_since: 0.0,
            window_flush_at: 0.0,
            retry_at: 0.0,
            retry_delay: 0.0,
            is_paused: false,
            is_unsupported: false,
            streams: HashMap::new(),
            closed_stream_ids: HashSet::new(),
            closed_stream_id_order: VecDeque::new(),
            next_stream_id: 1,
            output: Vec::new(),
            is_output_flush_scheduled: false,
            expires_at: 0.0,
        }
    }
}

/// `PendingConnection` (h:127-133) — an accepted loopback socket parked
/// until its port registers; the C++ `SocketFd` rides the net seam.
#[derive(Debug)]
struct PendingConnection {
    endpoint_id: i64,
    /// The loopback peer's port — matched by the `register_connection` hop
    /// (the net seam's callers).
    #[cfg_attr(not(test), allow(dead_code))]
    port: i32,
    expires_at: f64,
}

/// The [`WebProxyManager`] seams — the C++ `G()->`/actor-send world (the
/// [`crate::background_manager::BackgroundManagerEnv`] convention).
pub(crate) trait WebProxyManagerEnv {
    /// `Time::now` — the collapsed managers read time through the env.
    fn now(&self) -> f64;

    /// `send_web_proxy_update` (cpp:66-68) — the `Td::send_update` hop; the
    /// collapsed world drains the `Td` outbox at the turn tail.
    fn send_web_proxy_update(&mut self, update: td_api::Update);

    /// `create_actor<WebProxyListener>` (cpp:91-92, cpp:315-317,
    /// cpp:358-359) — the bind-`127.0.0.1:0`-and-answer-
    /// `on_listener_started` half (`[blocked: net]`; the parked seam never
    /// answers, so the address promises ride the LambdaPromise drop).
    fn open_web_proxy_listener(&mut self, endpoint_id: i64);

    /// `create_actor<WebProxyStream>` + `start_up` (cpp:484-486) — the
    /// accepted-socket actor half (`[blocked: net]`; the manager's
    /// accept half is test-driven until the net step binds it).
    #[cfg_attr(not(test), allow(dead_code))]
    fn stream_created(&mut self, stream_key: u64);

    /// `WebProxyStream::open` (cpp:150-154).
    fn stream_open(&mut self, stream_key: u64);

    /// `WebProxyStream::add_send_window` (cpp:155-159).
    fn stream_add_send_window(&mut self, stream_key: u64, amount: u32);

    /// `WebProxyStream::write` (cpp:160-164).
    fn stream_write(&mut self, stream_key: u64, data: &[u8]);

    /// `WebProxyStream::close` (cpp:165-171) — the released-actor drain
    /// tail (the `CLOSING_STREAM_TIMEOUT` timeout rides the seam).
    fn stream_close(&mut self, stream_key: u64);

    /// `ConnectionCreator::on_web_proxy_address_closed` (cpp:1319-1330) —
    /// the resolve-plane consumer (`[blocked: net]`; the collapsed
    /// `ConnectionCreator` has no resolve state to drop).
    fn on_web_proxy_address_closed(&mut self, ip_address: &str);
}

/// The collapsed `WebProxyManager` (h:46-160) — the carrier-plane registry
/// and state machine; the socket plane parks at the env seams.
pub(crate) struct WebProxyManager {
    /// `network_flag_` (h:113).
    network_flag: bool,

    /// `max_endpoint_id_` / `active_endpoint_id_` / `endpoints_` (h:115-117;
    /// the C++ `FlatHashMap` is unordered — the `BTreeMap` keeps the
    /// collapsed-world sweeps deterministic).
    max_endpoint_id: i64,
    active_endpoint_id: i64,
    endpoints: BTreeMap<i64, Endpoint>,

    /// `max_carrier_id_` / `carrier_endpoint_ids_` (h:119-120).
    max_carrier_id: i32,
    carrier_endpoint_ids: HashMap<i32, i64>,

    /// `max_stream_key_` / `streams_` (h:122-123) — the key pairs the stream
    /// actor's link token; the live read is the net seam's half.
    #[cfg_attr(not(test), allow(dead_code))]
    max_stream_key: u64,
    streams: HashMap<u64, Stream>,

    /// `expected_ports_` (h:126) — loopback peers are accepted only if their
    /// port was registered by this TDLib instance.
    expected_ports: HashMap<i32, f64>,
    /// `pending_connections_` (h:127-133).
    pending_connections: Vec<PendingConnection>,

    /// The `send_closure_later` flush queue (cpp:975-981) — the endpoint ids
    /// with scheduled output; the collapsed world flushes at the
    /// entry-point tail.
    scheduled_flushes: Vec<i64>,
}

impl Default for WebProxyManager {
    fn default() -> Self {
        Self {
            network_flag: true,
            max_endpoint_id: 0,
            active_endpoint_id: 0,
            endpoints: BTreeMap::new(),
            max_carrier_id: 0,
            carrier_endpoint_ids: HashMap::new(),
            max_stream_key: 0,
            streams: HashMap::new(),
            expected_ports: HashMap::new(),
            pending_connections: Vec::new(),
            scheduled_flushes: Vec::new(),
        }
    }
}

impl WebProxyManager {
    /// `WebProxyManager::set_active_proxy` (cpp:289-302) — the
    /// `ConnectionCreator::on_proxy_changed` fan-out; a web proxy opens its
    /// carrier at once, a proxy change closes the old one.
    pub(crate) fn set_active_proxy(
        &mut self,
        env: &mut impl WebProxyManagerEnv,
        proxy_id: i32,
        proxy: Proxy,
    ) {
        let same = self
            .endpoints
            .get(&self.active_endpoint_id)
            .is_some_and(|endpoint| {
                proxy.use_web_proxy() && endpoint.proxy_id == proxy_id && endpoint.proxy == proxy
            });
        if same {
            return;
        }
        if self.active_endpoint_id != 0 {
            let active_endpoint_id = self.active_endpoint_id;
            self.destroy_endpoint(env, active_endpoint_id);
            self.active_endpoint_id = 0;
        }
        if proxy.use_web_proxy() {
            self.active_endpoint_id = self.create_endpoint(env, proxy_id, proxy, false);
        }
        self.flush_scheduled(env);
    }

    /// `WebProxyManager::get_active_proxy_address` (cpp:303-319) — the
    /// resolve-loop request (the C++ caller rides the `[blocked: net]`
    /// connect path, cpp:1336-1341); the address promise parks until the
    /// listener seam answers (which the parked seam never does). The C++
    /// `Promise<IPAddress>` rides as the socket-address string.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn get_active_proxy_address(
        &mut self,
        env: &mut impl WebProxyManagerEnv,
        mut promise: Promise<String>,
    ) {
        let active_endpoint_id = self.active_endpoint_id;
        let Some(endpoint) = self.endpoints.get_mut(&active_endpoint_id) else {
            return promise.set_error(Status::error(400, "Web proxy is not enabled"));
        };
        if endpoint.is_unsupported {
            return promise.set_error(Status::error(
                400,
                "Web proxy is unsupported by the application",
            ));
        }
        if let Some(ip_address) = endpoint.ip_address.clone() {
            return promise.set_value(ip_address);
        }
        endpoint.address_promises.push(promise);
        if !endpoint.listener_exists {
            let endpoint_id = endpoint.endpoint_id;
            endpoint.listener_exists = true;
            env.open_web_proxy_listener(endpoint_id);
        }
    }

    /// `WebProxyManager::on_network` (cpp:320-330) — the C++ caller rides
    /// the unported `StateManager`→`ConnectionCreator` network pump
    /// (cpp:533); the collapsed world drives it from tests and the net step.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn on_network(&mut self, env: &mut impl WebProxyManagerEnv, network_flag: bool) {
        self.network_flag = network_flag;
        if !network_flag {
            return;
        }
        let active_endpoint_id = self.active_endpoint_id;
        let should_open = self
            .endpoints
            .get(&active_endpoint_id)
            .is_some_and(|endpoint| endpoint.carrier_id == 0 && !endpoint.is_unsupported);
        if should_open {
            let endpoint = self.endpoints.get_mut(&active_endpoint_id).unwrap();
            endpoint.retry_delay = 0.0;
            self.open_carrier(env, active_endpoint_id);
        }
        self.flush_scheduled(env);
    }

    /// `WebProxyManager::open_temporary_carrier` (cpp:332-341) — the
    /// `ProxyChecker::test_proxy` web band (cpp:33-56) opens a carrier for
    /// the test handshake; the checker's socket/handshake tail rides the
    /// `[blocked: net]` `test_proxy_via_net` seam.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn open_temporary_carrier(
        &mut self,
        env: &mut impl WebProxyManagerEnv,
        proxy: Proxy,
        promise: Promise<TemporaryCarrier>,
    ) {
        if !proxy.use_web_proxy() {
            let mut promise = promise;
            return promise.set_error(Status::error(400, "Proxy is not a web proxy"));
        }
        let endpoint_id = self.create_endpoint(env, 0, proxy, true);
        debug_assert!(self.endpoints.contains_key(&endpoint_id));
        self.endpoints
            .get_mut(&endpoint_id)
            .unwrap()
            .temporary_promise = Some(promise);
        self.flush_scheduled(env);
    }

    /// `WebProxyManager::close_temporary_carrier` (cpp:342-348).
    pub(crate) fn close_temporary_carrier(
        &mut self,
        env: &mut impl WebProxyManagerEnv,
        endpoint_id: i64,
    ) {
        let is_temporary = self
            .endpoints
            .get(&endpoint_id)
            .is_some_and(|endpoint| endpoint.is_temporary);
        if is_temporary {
            self.destroy_endpoint(env, endpoint_id);
        }
        self.flush_scheduled(env);
    }

    /// `WebProxyManager::create_endpoint` (cpp:349-368).
    fn create_endpoint(
        &mut self,
        env: &mut impl WebProxyManagerEnv,
        proxy_id: i32,
        proxy: Proxy,
        is_temporary: bool,
    ) -> i64 {
        self.max_endpoint_id += 1;
        let endpoint_id = self.max_endpoint_id;
        let endpoint = Endpoint {
            endpoint_id,
            proxy_id,
            proxy,
            is_temporary,
            expires_at: if is_temporary {
                env.now() + TEMPORARY_ENDPOINT_LIFETIME
            } else {
                0.0
            },
            listener_exists: true,
            ..Endpoint::default()
        };
        env.open_web_proxy_listener(endpoint_id);
        let open_now = is_temporary || self.network_flag;
        self.endpoints.insert(endpoint_id, endpoint);
        if open_now {
            self.open_carrier(env, endpoint_id);
        }
        endpoint_id
    }

    /// `WebProxyManager::destroy_endpoint` (cpp:369-390).
    fn destroy_endpoint(&mut self, env: &mut impl WebProxyManagerEnv, endpoint_id: i64) {
        let Some(mut endpoint) = self.endpoints.remove(&endpoint_id) else {
            return;
        };
        if self.active_endpoint_id == endpoint_id {
            self.active_endpoint_id = 0;
        }

        self.close_carrier(env, &mut endpoint);
        self.close_all_streams(&mut endpoint);
        self.pending_connections
            .retain(|connection| connection.endpoint_id != endpoint_id);
        fail_promises(
            &mut endpoint.address_promises,
            &Status::error(400, "Web proxy has been disabled"),
        );
        if let Some(mut promise) = endpoint.temporary_promise.take() {
            promise.set_error(Status::error(400, "WEB_PROXY_CARRIER_FAILED"));
        }
    }

    /// `WebProxyManager::on_listener_started` (cpp:391-414) — the listener
    /// seam's answer; the error half's `send_closure_later`
    /// `close_temporary_carrier` (cpp:405-407) runs immediately in the
    /// collapsed world (the caller stack cannot still be using the
    /// endpoint).
    #[allow(dead_code)] // the listener seam's answer half; the parked seam never answers
    pub(crate) fn on_listener_started(
        &mut self,
        env: &mut impl WebProxyManagerEnv,
        endpoint_id: i64,
        r_ip_address: Result<String, Status>,
    ) {
        if !self.endpoints.contains_key(&endpoint_id) {
            return;
        }
        match r_ip_address {
            Ok(ip_address) => {
                let mut promises;
                let address = {
                    let endpoint = self.endpoints.get_mut(&endpoint_id).unwrap();
                    endpoint.ip_address = Some(ip_address);
                    promises = std::mem::take(&mut endpoint.address_promises);
                    endpoint.ip_address.clone().unwrap_or_default()
                };
                for promise in &mut promises {
                    promise.set_value(address.clone());
                }
                self.resolve_temporary_carrier(env, endpoint_id);
            }
            Err(error) => {
                tdutils::log_warning!("Failed to listen for web proxy connections: {error}");
                let (is_temporary, has_temporary_promise) = {
                    let endpoint = self.endpoints.get_mut(&endpoint_id).unwrap();
                    (endpoint.is_temporary, endpoint.temporary_promise.is_some())
                };
                self.close_listener(
                    env,
                    endpoint_id,
                    Status::error(400, "Failed to start web proxy"),
                );
                if is_temporary && has_temporary_promise {
                    let endpoint = self.endpoints.get_mut(&endpoint_id).unwrap();
                    if let Some(mut promise) = endpoint.temporary_promise.take() {
                        promise.set_error(Status::error(400, "WEB_PROXY_CARRIER_FAILED"));
                    }
                    self.close_temporary_carrier(env, endpoint_id);
                }
            }
        }
    }

    /// `WebProxyManager::close_listener` (cpp:415-429).
    fn close_listener(
        &mut self,
        env: &mut impl WebProxyManagerEnv,
        endpoint_id: i64,
        error: Status,
    ) {
        let Some(endpoint) = self.endpoints.get_mut(&endpoint_id) else {
            return;
        };
        // the C++ `listener.reset()` destroys the actor (and its socket);
        // the parked seam owns that half
        endpoint.listener_exists = false;
        self.pending_connections
            .retain(|connection| connection.endpoint_id != endpoint_id);
        fail_promises(&mut endpoint.address_promises, &error);
        if let Some(ip_address) = endpoint.ip_address.clone() {
            if !endpoint.is_temporary {
                // otherwise ConnectionCreator keeps connecting to the closed
                // port until it requests the address again (cpp:422-425)
                env.on_web_proxy_address_closed(&ip_address);
            }
            endpoint.ip_address = None;
        }
    }

    /// `WebProxyManager::resolve_temporary_carrier` (cpp:430-440).
    fn resolve_temporary_carrier(&mut self, _env: &mut impl WebProxyManagerEnv, endpoint_id: i64) {
        let is_ready = self.endpoints.get(&endpoint_id).is_some_and(|endpoint| {
            endpoint.is_temporary
                && endpoint.temporary_promise.is_some()
                && endpoint.state == CarrierState::Ready
                && endpoint.ip_address.is_some()
        });
        if !is_ready {
            return;
        }
        let mut promise = self
            .endpoints
            .get_mut(&endpoint_id)
            .unwrap()
            .temporary_promise
            .take()
            .expect("checked above");
        let result = TemporaryCarrier {
            endpoint_id,
            ip_address: self.endpoints[&endpoint_id].ip_address.clone(),
        };
        promise.set_value(result);
    }

    /// `WebProxyManager::expect_connection` (cpp:441-453) — the
    /// `register_connection` hop's registry half (the socket half parks).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn expect_connection(&mut self, env: &mut impl WebProxyManagerEnv, port: i32) {
        let position = self
            .pending_connections
            .iter()
            .position(|connection| connection.port == port);
        if let Some(position) = position {
            let connection = self.pending_connections.remove(position);
            return self.accept_connection(env, connection.endpoint_id);
        }
        self.expected_ports
            .insert(port, env.now() + EXPECTED_PORT_TIMEOUT);
    }

    /// `WebProxyManager::accept_connection` (cpp:479-502) — the C++
    /// SocketFd parameter rides the stream seam; the collapsed world
    /// registers the stream and opens it when the carrier is ready.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn accept_connection(
        &mut self,
        env: &mut impl WebProxyManagerEnv,
        endpoint_id: i64,
    ) {
        let is_full = self
            .endpoints
            .get(&endpoint_id)
            .is_some_and(|endpoint| endpoint.streams.len() >= MAX_STREAMS);
        if is_full {
            tdutils::log_warning!("Too many web proxy streams");
            return;
        }
        if !self.endpoints.contains_key(&endpoint_id) {
            return;
        }
        self.max_stream_key += 1;
        let stream_key = self.max_stream_key;
        let (state, stream_id) = {
            let endpoint = self.endpoints.get_mut(&endpoint_id).unwrap();
            let stream_id = allocate_stream_id(endpoint);
            endpoint.streams.insert(stream_id, stream_key);
            (endpoint.state, stream_id)
        };
        let stream = Stream {
            endpoint_id,
            stream_id,
            receive_window: INITIAL_WINDOW,
            pending_window: 0,
            is_opened: false,
            has_received_data: false,
        };
        self.streams.insert(stream_key, stream);
        env.stream_created(stream_key);
        if state == CarrierState::Ready {
            self.open_stream(env, endpoint_id, stream_key);
        }
        self.flush_scheduled(env);
    }

    /// `WebProxyManager::open_carrier` (cpp:512-554) — the carrier opens:
    /// the update carries the relay URL with the bridge capability and the
    /// base64url nonce fragment; "android" is the relay's frozen v1 name for
    /// its injected-bridge mode, used by every client (cpp:535-538).
    fn open_carrier(&mut self, env: &mut impl WebProxyManagerEnv, endpoint_id: i64) {
        let Some(endpoint) = self.endpoints.get_mut(&endpoint_id) else {
            return;
        };
        if endpoint.carrier_id != 0 {
            return;
        }
        if self.max_carrier_id == i32::MAX {
            self.max_carrier_id = 0;
        }
        self.max_carrier_id += 1;
        let carrier_id = self.max_carrier_id;
        endpoint.carrier_id = carrier_id;
        self.carrier_endpoint_ids
            .insert(carrier_id, endpoint.endpoint_id);

        let mut nonce = [0u8; 32];
        secure_bytes(&mut nonce);
        endpoint.nonce = String::from_utf8_lossy(&base64url_encode(&nonce)).into_owned();
        endpoint.state = CarrierState::WaitInit;
        endpoint.opened_at = env.now();
        endpoint.handshake_deadline = endpoint.opened_at + HANDSHAKE_TIMEOUT;
        endpoint.unanswered_since = 0.0;
        endpoint.window_flush_at = 0.0;
        endpoint.retry_at = 0.0;
        endpoint.is_paused = false;
        endpoint.closed_stream_ids.clear();
        endpoint.closed_stream_id_order.clear();
        endpoint.output.clear();

        let path_prefix = if endpoint.proxy.path.is_empty() {
            "/".to_string()
        } else {
            format!("/{}/", endpoint.proxy.path)
        };
        let origin = format!("https://{}", endpoint.proxy.server);
        let capability = endpoint.proxy.get_web_proxy_bridge_capability();
        let nonce = endpoint.nonce.clone();
        let url = format!("{origin}{path_prefix}?bridge={capability}#android={nonce}");
        // the C++ prints the `Proxy` object (its `operator<<` form)
        tdutils::log_info!(
            "Open web proxy carrier {carrier_id} for {}:{}",
            endpoint.proxy.server,
            endpoint.proxy.port
        );
        let proxy_id = endpoint.proxy_id;
        let is_temporary = endpoint.is_temporary;
        env.send_web_proxy_update(td_api::Update::updateWebProxyCarrierOpened(
            td_api::updateWebProxyCarrierOpened {
                carrier: Some(td_api::WebProxyCarrier {
                    id: carrier_id,
                    proxy_id,
                    url,
                    origin,
                    path_prefix,
                    bridge_object_name: BRIDGE_OBJECT_NAME.to_string(),
                    is_temporary,
                }),
            },
        ));
    }

    /// `WebProxyManager::close_carrier` (cpp:555-577).
    fn close_carrier(&mut self, env: &mut impl WebProxyManagerEnv, endpoint: &mut Endpoint) {
        if endpoint.carrier_id == 0 {
            return;
        }
        let carrier_id = endpoint.carrier_id;
        tdutils::log_info!("Close web proxy carrier {carrier_id}");

        self.close_all_streams(endpoint);
        endpoint.output.clear();
        env.send_web_proxy_update(td_api::Update::updateWebProxyCarrierClosed(
            td_api::updateWebProxyCarrierClosed { carrier_id },
        ));

        self.carrier_endpoint_ids.remove(&carrier_id);
        endpoint.carrier_id = 0;
        endpoint.state = CarrierState::None;
        endpoint.nonce.clear();
        endpoint.handshake_deadline = 0.0;
        endpoint.unanswered_since = 0.0;
        endpoint.window_flush_at = 0.0;
        endpoint.closed_stream_ids.clear();
        endpoint.closed_stream_id_order.clear();
    }

    /// `WebProxyManager::fail_carrier` (cpp:578-597).
    fn fail_carrier(
        &mut self,
        env: &mut impl WebProxyManagerEnv,
        endpoint_id: i64,
        reason: &str,
        is_refused: bool,
    ) {
        let mut endpoint = match self.endpoints.remove(&endpoint_id) {
            Some(endpoint) => endpoint,
            None => return,
        };
        if endpoint.carrier_id == 0 {
            self.endpoints.insert(endpoint_id, endpoint);
            return;
        }
        tdutils::log_info!("Web proxy carrier {} failed: {reason}", endpoint.carrier_id);
        self.close_carrier(env, &mut endpoint);

        if endpoint.is_temporary {
            let had_promise = endpoint.temporary_promise.is_some();
            if let Some(mut promise) = endpoint.temporary_promise.take() {
                promise.set_error(Status::error(
                    400,
                    if is_refused {
                        "WEB_PROXY_CARRIER_REFUSED"
                    } else {
                        "WEB_PROXY_CARRIER_FAILED"
                    },
                ));
            }
            self.endpoints.insert(endpoint_id, endpoint);
            if had_promise {
                // the caller may still use the endpoint, so it is destroyed
                // later (the C++ send_closure_later; cpp:590-593)
                self.close_temporary_carrier(env, endpoint_id);
            }
            return;
        }

        self.endpoints.insert(endpoint_id, endpoint);
        self.schedule_retry(env, endpoint_id);
    }

    /// `WebProxyManager::schedule_retry` (cpp:598-610).
    fn schedule_retry(&mut self, env: &mut impl WebProxyManagerEnv, endpoint_id: i64) {
        let Some(endpoint) = self.endpoints.get_mut(&endpoint_id) else {
            return;
        };
        if endpoint.is_unsupported {
            endpoint.retry_at = 0.0;
        } else if endpoint.is_paused {
            endpoint.retry_at = env.now() + PAUSED_RETRY_DELAY;
        } else {
            endpoint.retry_delay = if endpoint.retry_delay == 0.0 {
                MIN_RETRY_DELAY
            } else {
                (endpoint.retry_delay * 2.0).min(MAX_RETRY_DELAY)
            };
            endpoint.retry_at = env.now() + endpoint.retry_delay;
        }
    }

    /// `WebProxyManager::process_carrier_message` (cpp:611-635) — the
    /// `processWebProxyCarrierMessage` request arm.
    pub(crate) fn process_carrier_message(
        &mut self,
        env: &mut impl WebProxyManagerEnv,
        carrier_id: i32,
        message: Option<td_api::WebProxyCarrierMessage>,
        mut promise: Promise<()>,
    ) {
        let Some(message) = message else {
            return promise.set_error(Status::error(400, "Message must be non-empty"));
        };
        let Some(endpoint_id) = self.carrier_endpoint_ids.get(&carrier_id).copied() else {
            return self.fail_unknown_carrier(carrier_id, promise);
        };
        promise.set_value(());

        match message {
            td_api::WebProxyCarrierMessage::webProxyCarrierMessageText(text) => {
                self.on_carrier_text(env, endpoint_id, &text.text)
            }
            td_api::WebProxyCarrierMessage::webProxyCarrierMessageBinary(binary) => {
                self.on_carrier_binary(env, endpoint_id, &binary.data)
            }
        }
        self.flush_scheduled(env);
    }

    /// `WebProxyManager::fail_unknown_carrier` (cpp:636-643) — the
    /// application may still be passing messages of a carrier that is
    /// already closed.
    fn fail_unknown_carrier(&mut self, carrier_id: i32, mut promise: Promise<()>) {
        if carrier_id > 0 && carrier_id <= self.max_carrier_id {
            return promise.set_value(());
        }
        promise.set_error(Status::error(400, "WEB_PROXY_CARRIER_NOT_FOUND"));
    }

    /// `WebProxyManager::report_carrier_error` (cpp:644-680) — the
    /// `reportWebProxyCarrierError` request arm.
    pub(crate) fn report_carrier_error(
        &mut self,
        env: &mut impl WebProxyManagerEnv,
        carrier_id: i32,
        error: Option<td_api::WebProxyCarrierError>,
        mut promise: Promise<()>,
    ) {
        let Some(error) = error else {
            return promise.set_error(Status::error(400, "Error must be non-empty"));
        };
        let Some(endpoint_id) = self.carrier_endpoint_ids.get(&carrier_id).copied() else {
            return self.fail_unknown_carrier(carrier_id, promise);
        };
        promise.set_value(());

        match error {
            td_api::WebProxyCarrierError::webProxyCarrierErrorUnsupported(_) => {
                let is_temporary = {
                    let endpoint = self.endpoints.get_mut(&endpoint_id).unwrap();
                    endpoint.is_unsupported = true;
                    endpoint.is_temporary
                };
                if !is_temporary {
                    // fail MTProto connections at once instead of leaving
                    // them to wait for a carrier that will never open
                    // (cpp:658-662)
                    self.close_listener(
                        env,
                        endpoint_id,
                        Status::error(400, "Web proxy is unsupported by the application"),
                    );
                }
                self.fail_carrier(env, endpoint_id, "web views are unsupported", false);
            }
            td_api::WebProxyCarrierError::webProxyCarrierErrorPaused(_) => {
                let endpoint = self.endpoints.get_mut(&endpoint_id).unwrap();
                endpoint.is_paused = true;
                self.fail_carrier(env, endpoint_id, "paused", false);
            }
            td_api::WebProxyCarrierError::webProxyCarrierErrorLoadFailed(load_failed) => {
                let reason = format!("page load failed: {}", load_failed.message);
                self.fail_carrier(env, endpoint_id, &reason, false);
            }
            td_api::WebProxyCarrierError::webProxyCarrierErrorTerminated(_) => {
                self.fail_carrier(env, endpoint_id, "web view was terminated", false);
            }
            td_api::WebProxyCarrierError::webProxyCarrierErrorRefused(_) => {
                self.fail_carrier(env, endpoint_id, "refused", true);
            }
        }
        self.flush_scheduled(env);
    }

    /// `WebProxyManager::on_carrier_text` (cpp:681-716) — the page's JSON
    /// protocol; unknown messages are ignored, as by the other reference
    /// clients (cpp:691-693).
    fn on_carrier_text(&mut self, env: &mut impl WebProxyManagerEnv, endpoint_id: i64, text: &str) {
        if text.len() > MAX_CARRIER_TEXT_SIZE {
            return self.fail_carrier(env, endpoint_id, "too long text message", false);
        }
        let r_value = json_decode(text);
        if !r_value.is_ok() {
            // unknown page messages are ignored
            return;
        }
        let value = r_value.unwrap();
        if value.ty() != JsonValueType::Object {
            return;
        }
        let object = match value {
            JsonValue::Object(object) => object,
            _ => unreachable!("checked the type above"),
        };
        let message_type = get_json_string_field(&object, "t");
        if message_type == "tproxy-android-init" {
            let r_version = object.get_optional_int_field("v", 0);
            let nonce = get_json_string_field(&object, "nonce");
            let is_valid = {
                let endpoint = self.endpoints.get_mut(&endpoint_id).unwrap();
                endpoint.state == CarrierState::WaitInit
                    && r_version.is_ok()
                    && *r_version.ok() == 1
                    && nonce == endpoint.nonce
            };
            if !is_valid {
                return self.fail_carrier(env, endpoint_id, "invalid init message", false);
            }
            self.endpoints.get_mut(&endpoint_id).unwrap().state = CarrierState::WaitWelcome;
            self.send_frame(env, endpoint_id, FRAME_HELLO, 0, b"\x01");
        } else if message_type == "close" {
            self.fail_carrier(env, endpoint_id, "page closed the carrier", false);
        } else if message_type == "status" {
            let state = get_json_string_field(&object, "state");
            if state == "failed" {
                self.fail_carrier(
                    env,
                    endpoint_id,
                    "page failed to connect to the relay",
                    false,
                );
            } else if (state == "connecting" || state == "reconnecting")
                && self.endpoints.get(&endpoint_id).unwrap().state != CarrierState::Ready
            {
                let endpoint = self.endpoints.get_mut(&endpoint_id).unwrap();
                let max_deadline = endpoint.opened_at
                    + if endpoint.is_temporary {
                        MAX_TEMPORARY_HANDSHAKE_TIME
                    } else {
                        MAX_HANDSHAKE_TIME
                    };
                endpoint.handshake_deadline = endpoint
                    .handshake_deadline
                    .max((env.now() + HANDSHAKE_TIMEOUT).min(max_deadline));
            }
        }
    }

    /// `WebProxyManager::on_carrier_binary` (cpp:717-751) — the frame-batch
    /// parse.
    fn on_carrier_binary(
        &mut self,
        env: &mut impl WebProxyManagerEnv,
        endpoint_id: i64,
        data: &[u8],
    ) {
        let is_wait_init = self
            .endpoints
            .get(&endpoint_id)
            .is_some_and(|endpoint| endpoint.state == CarrierState::WaitInit);
        if is_wait_init {
            return self.fail_carrier(env, endpoint_id, "binary message before init", false);
        }
        if data.is_empty() {
            return self.fail_carrier(env, endpoint_id, "empty binary message", false);
        }
        self.endpoints
            .get_mut(&endpoint_id)
            .expect("checked by the WaitInit band")
            .unanswered_since = 0.0;

        let start_carrier_id = self.endpoints[&endpoint_id].carrier_id;
        let mut offset = 0;
        while offset < data.len() {
            if data.len() - offset < FRAME_HEADER_SIZE {
                return self.fail_carrier(env, endpoint_id, "partial frame header", false);
            }
            let header = &data[offset..offset + FRAME_HEADER_SIZE];
            let frame_type = header[0];
            let stream_id =
                ((header[1] as u32) << 16) | ((header[2] as u32) << 8) | (header[3] as u32);
            let length = read_uint32(&header[4..8]) as usize;
            if length > MAX_FRAME_PAYLOAD_SIZE || data.len() - offset - FRAME_HEADER_SIZE < length {
                return self.fail_carrier(env, endpoint_id, "invalid frame length", false);
            }
            let payload = &data[offset + FRAME_HEADER_SIZE..offset + FRAME_HEADER_SIZE + length];
            let status = self.on_carrier_frame(env, endpoint_id, frame_type, stream_id, payload);
            if let Err(message) = status {
                return self.fail_carrier(env, endpoint_id, &message, false);
            }
            if self.endpoints[&endpoint_id].carrier_id != start_carrier_id {
                return;
            }
            offset += FRAME_HEADER_SIZE + length;
        }
    }

    /// `WebProxyManager::on_carrier_frame` (cpp:752-828).
    fn on_carrier_frame(
        &mut self,
        env: &mut impl WebProxyManagerEnv,
        endpoint_id: i64,
        frame_type: u8,
        stream_id: u32,
        payload: &[u8],
    ) -> Result<(), String> {
        let state = self.endpoints[&endpoint_id].state;
        if state != CarrierState::Ready {
            if frame_type != FRAME_WELCOME || stream_id != 0 || !payload.is_empty() {
                return Err("Unexpected frame before welcome".to_string());
            }
            self.on_carrier_welcome(env, endpoint_id);
            return Ok(());
        }

        if stream_id == 0 {
            return match frame_type {
                FRAME_PING => {
                    if payload.len() > MAX_PING_PAYLOAD_SIZE {
                        return Err("Too long ping".to_string());
                    }
                    self.send_frame(env, endpoint_id, FRAME_PONG, 0, payload);
                    Ok(())
                }
                FRAME_BYE => Err("Relay said goodbye".to_string()),
                _ => Err(format!("Unexpected session frame {}", frame_type as i32)),
            };
        }

        let stream_key = {
            let endpoint = self.endpoints.get_mut(&endpoint_id).unwrap();
            match endpoint.streams.get(&stream_id) {
                Some(&stream_key) => stream_key,
                None => {
                    if !endpoint.closed_stream_ids.contains(&stream_id) {
                        return Err("Frame for an unknown stream".to_string());
                    }
                    // the frame crossed our CLOSE (cpp:776-787)
                    let is_valid = (frame_type == FRAME_DATA && !payload.is_empty())
                        || (frame_type == FRAME_WINDOW
                            && payload.len() == 4
                            && read_uint32(payload) != 0)
                        || (frame_type == FRAME_CLOSE && payload.is_empty());
                    if !is_valid {
                        return Err("Invalid frame for a closed stream".to_string());
                    }
                    return Ok(());
                }
            }
        };

        let receive_window = {
            let stream = self
                .streams
                .get(&stream_key)
                .expect("the endpoint stream registry and the stream map agree");
            debug_assert!(stream.is_opened);
            stream.receive_window
        };
        match frame_type {
            FRAME_DATA => {
                if payload.is_empty() || payload.len() > receive_window as usize {
                    return Err("Data beyond the receive window".to_string());
                }
                let stream = self.streams.get_mut(&stream_key).unwrap();
                stream.receive_window -= payload.len() as u32;
                stream.has_received_data = true;
                env.stream_write(stream_key, payload);
                Ok(())
            }
            FRAME_WINDOW => {
                if payload.len() != 4 {
                    return Err("Invalid window frame".to_string());
                }
                let amount = read_uint32(payload);
                if amount == 0 {
                    return Err("Empty window frame".to_string());
                }
                env.stream_add_send_window(stream_key, amount);
                Ok(())
            }
            FRAME_CLOSE => {
                if !payload.is_empty() {
                    return Err("Invalid close frame".to_string());
                }
                // released, because destroying the actor would drop the data
                // that is still being written (cpp:816-818)
                env.stream_close(stream_key);
                self.close_stream(env, endpoint_id, stream_key, false);
                Ok(())
            }
            _ => Err(format!("Unexpected stream frame {}", frame_type as i32)),
        }
    }

    /// `WebProxyManager::on_carrier_welcome` (cpp:829-841).
    fn on_carrier_welcome(&mut self, env: &mut impl WebProxyManagerEnv, endpoint_id: i64) {
        {
            let endpoint = self.endpoints.get_mut(&endpoint_id).unwrap();
            tdutils::log_info!("Web proxy carrier {} is ready", endpoint.carrier_id);
            endpoint.state = CarrierState::Ready;
            endpoint.handshake_deadline = 0.0;
            endpoint.retry_delay = 0.0;
        }
        let stream_keys: Vec<u64> = self.endpoints[&endpoint_id]
            .streams
            .values()
            .copied()
            .collect();
        for stream_key in stream_keys {
            debug_assert!(
                self.streams.contains_key(&stream_key),
                "the endpoint stream registry and the stream map agree"
            );
            self.open_stream(env, endpoint_id, stream_key);
        }
        self.resolve_temporary_carrier(env, endpoint_id);
    }

    /// `WebProxyManager::open_stream` (cpp:842-848).
    fn open_stream(
        &mut self,
        env: &mut impl WebProxyManagerEnv,
        endpoint_id: i64,
        stream_key: u64,
    ) {
        let stream_id = {
            let stream = self
                .streams
                .get_mut(&stream_key)
                .expect("open_stream runs on a registered stream");
            debug_assert!(!stream.is_opened);
            stream.is_opened = true;
            stream.stream_id
        };
        self.send_frame(env, endpoint_id, FRAME_OPEN, stream_id, &[]);
        env.stream_open(stream_key);
    }

    /// `WebProxyManager::on_stream_data` (cpp:849-866) — the stream actor's
    /// read half; the C++ link token is the stream key.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn on_stream_data(
        &mut self,
        env: &mut impl WebProxyManagerEnv,
        stream_key: u64,
        data: &[u8],
    ) {
        let Some(stream) = self.streams.get(&stream_key) else {
            return;
        };
        let endpoint_id = stream.endpoint_id;
        let stream_id = stream.stream_id;
        let is_opened = stream.is_opened;
        let has_received_data = stream.has_received_data;
        let Some(endpoint) = self.endpoints.get(&endpoint_id) else {
            return;
        };
        if endpoint.state != CarrierState::Ready || !is_opened {
            return;
        }
        if !has_received_data && endpoint.unanswered_since == 0.0 {
            // a new MTProto connection always gets a response, so silence
            // after its first bytes means the carrier is dead (cpp:860-864)
            self.endpoints
                .get_mut(&endpoint_id)
                .unwrap()
                .unanswered_since = env.now();
        }
        self.send_frame(env, endpoint_id, FRAME_DATA, stream_id, data);
        self.flush_scheduled(env);
    }

    /// `WebProxyManager::on_stream_written` (cpp:867-886).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn on_stream_written(
        &mut self,
        env: &mut impl WebProxyManagerEnv,
        stream_key: u64,
        size: usize,
    ) {
        let endpoint_id;
        let pending_window;
        {
            let Some(stream) = self.streams.get_mut(&stream_key) else {
                return;
            };
            // the endpoint/state/opened gate precedes the accounting
            // (cpp:869-878) — bytes of an unopened stream never window back
            let Some(endpoint) = self.endpoints.get(&stream.endpoint_id) else {
                return;
            };
            if endpoint.state != CarrierState::Ready || !stream.is_opened {
                return;
            }
            debug_assert!(size <= INITIAL_WINDOW as usize);
            stream.pending_window += size as u32;
            endpoint_id = stream.endpoint_id;
            pending_window = stream.pending_window;
        }
        if pending_window >= WINDOW_FLUSH_SIZE {
            self.flush_stream_window(env, endpoint_id, stream_key);
        } else {
            let endpoint = self.endpoints.get_mut(&endpoint_id).unwrap();
            if endpoint.window_flush_at == 0.0 {
                endpoint.window_flush_at = env.now() + WINDOW_FLUSH_DELAY;
            }
        }
        self.flush_scheduled(env);
    }

    /// `WebProxyManager::flush_stream_window` (cpp:887-901).
    fn flush_stream_window(
        &mut self,
        env: &mut impl WebProxyManagerEnv,
        endpoint_id: i64,
        stream_key: u64,
    ) {
        let frame = {
            let Some(stream) = self.streams.get_mut(&stream_key) else {
                return;
            };
            if stream.pending_window == 0 {
                return;
            }
            debug_assert!(stream.pending_window <= INITIAL_WINDOW - stream.receive_window);
            let pending_window = stream.pending_window;
            stream.receive_window += pending_window;
            stream.pending_window = 0;
            let stream_id = stream.stream_id;
            (
                stream_id,
                [
                    (pending_window >> 24) as u8,
                    (pending_window >> 16) as u8,
                    (pending_window >> 8) as u8,
                    pending_window as u8,
                ],
            )
        };
        let (stream_id, payload) = frame;
        self.send_frame(env, endpoint_id, FRAME_WINDOW, stream_id, &payload);
    }

    /// `WebProxyManager::flush_window` (cpp:902-910).
    fn flush_window(&mut self, env: &mut impl WebProxyManagerEnv, endpoint_id: i64) {
        self.endpoints
            .get_mut(&endpoint_id)
            .expect("flush_window runs on a live endpoint")
            .window_flush_at = 0.0;
        let stream_keys: Vec<u64> = self.endpoints[&endpoint_id]
            .streams
            .values()
            .copied()
            .collect();
        for stream_key in stream_keys {
            self.flush_stream_window(env, endpoint_id, stream_key);
        }
    }

    /// `WebProxyManager::on_stream_closed` (cpp:911-920) — the stream
    /// actor's hangup half (`hangup_shared`, cpp:265-268).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn on_stream_closed(&mut self, env: &mut impl WebProxyManagerEnv, stream_key: u64) {
        let Some(stream) = self.streams.get(&stream_key) else {
            return;
        };
        let endpoint_id = stream.endpoint_id;
        self.close_stream(env, endpoint_id, stream_key, true);
        self.flush_scheduled(env);
    }

    /// `WebProxyManager::close_stream` (cpp:921-934).
    fn close_stream(
        &mut self,
        env: &mut impl WebProxyManagerEnv,
        endpoint_id: i64,
        stream_key: u64,
        need_close_frame: bool,
    ) {
        let stream = self
            .streams
            .remove(&stream_key)
            .expect("close_stream runs on a registered stream");
        let (state, was_opened) = {
            let endpoint = self
                .endpoints
                .get_mut(&endpoint_id)
                .expect("the stream's endpoint outlives it");
            endpoint.streams.remove(&stream.stream_id);
            (endpoint.state, stream.is_opened)
        };
        if was_opened && state == CarrierState::Ready {
            if need_close_frame {
                self.send_frame(env, endpoint_id, FRAME_CLOSE, stream.stream_id, &[]);
            }
            remember_closed_stream(
                self.endpoints
                    .get_mut(&endpoint_id)
                    .expect("the stream's endpoint outlives it"),
                stream.stream_id,
            );
        }
    }

    /// `WebProxyManager::close_all_streams` (cpp:935-941).
    fn close_all_streams(&mut self, endpoint: &mut Endpoint) {
        for stream_key in endpoint.streams.values() {
            self.streams.remove(stream_key);
        }
        endpoint.streams.clear();
    }

    /// `WebProxyManager::send_frame` (cpp:954-982) — frames produced while
    /// handling one batch of events are sent to the page as one message;
    /// the C++ `send_closure_later` flush rides the entry-point tail.
    fn send_frame(
        &mut self,
        env: &mut impl WebProxyManagerEnv,
        endpoint_id: i64,
        frame_type: u8,
        stream_id: u32,
        payload: &[u8],
    ) {
        if !self.endpoints.contains_key(&endpoint_id) {
            return;
        }
        let needs_pre_flush = {
            let endpoint = self.endpoints.get_mut(&endpoint_id).unwrap();
            if endpoint.carrier_id == 0 {
                return;
            }
            debug_assert!(stream_id <= MAX_STREAM_ID);
            debug_assert!(payload.len() <= MAX_FRAME_PAYLOAD_SIZE);
            !endpoint.output.is_empty()
                && endpoint.output.len() + FRAME_HEADER_SIZE + payload.len() > MAX_CARRIER_DATA_SIZE
        };
        if needs_pre_flush {
            self.do_flush_output(env, endpoint_id);
        }

        let size = payload.len() as u32;
        let mut frame = Vec::with_capacity(FRAME_HEADER_SIZE + payload.len());
        frame.push(frame_type);
        frame.push((stream_id >> 16) as u8);
        frame.push((stream_id >> 8) as u8);
        frame.push(stream_id as u8);
        frame.push((size >> 24) as u8);
        frame.push((size >> 16) as u8);
        frame.push((size >> 8) as u8);
        frame.push(size as u8);
        frame.extend_from_slice(payload);

        let should_schedule = {
            let endpoint = self.endpoints.get_mut(&endpoint_id).unwrap();
            endpoint.output.extend_from_slice(&frame);
            if endpoint.is_output_flush_scheduled {
                false
            } else {
                endpoint.is_output_flush_scheduled = true;
                true
            }
        };
        if should_schedule {
            self.scheduled_flushes.push(endpoint_id);
        }
    }

    /// `WebProxyManager::flush_output` (cpp:983-991) — the collapsed
    /// end-of-entry-point flush.
    fn flush_scheduled(&mut self, env: &mut impl WebProxyManagerEnv) {
        let endpoint_ids = std::mem::take(&mut self.scheduled_flushes);
        for endpoint_id in endpoint_ids {
            if let Some(endpoint) = self.endpoints.get_mut(&endpoint_id) {
                endpoint.is_output_flush_scheduled = false;
            }
            self.do_flush_output(env, endpoint_id);
        }
    }

    /// `WebProxyManager::do_flush_output` (cpp:992-1004).
    fn do_flush_output(&mut self, env: &mut impl WebProxyManagerEnv, endpoint_id: i64) {
        let Some(endpoint) = self.endpoints.get_mut(&endpoint_id) else {
            return;
        };
        if endpoint.output.is_empty() {
            return;
        }
        if endpoint.carrier_id == 0 {
            endpoint.output.clear();
            return;
        }
        let carrier_id = endpoint.carrier_id;
        let output = std::mem::take(&mut endpoint.output);
        env.send_web_proxy_update(td_api::Update::updateWebProxyCarrierData(
            td_api::updateWebProxyCarrierData {
                carrier_id,
                data: output,
            },
        ));
    }

    /// `WebProxyManager::timeout_expired` (cpp:1005-1050) — the `Td` fired
    /// timeout.
    pub(crate) fn timeout_expired(&mut self, env: &mut impl WebProxyManagerEnv) {
        let now = env.now();

        self.expected_ports.retain(|_, at| *at > now);
        self.pending_connections
            .retain(|connection| connection.expires_at > now);

        let endpoint_ids: Vec<i64> = self.endpoints.keys().copied().collect();
        for endpoint_id in endpoint_ids {
            if !self.endpoints.contains_key(&endpoint_id) {
                continue;
            }
            let (is_temporary, expires_at, carrier_id, state, retry_at) = {
                let endpoint = self.endpoints.get_mut(&endpoint_id).unwrap();
                (
                    endpoint.is_temporary,
                    endpoint.expires_at,
                    endpoint.carrier_id,
                    endpoint.state,
                    endpoint.retry_at,
                )
            };
            if is_temporary && expires_at <= now {
                self.destroy_endpoint(env, endpoint_id);
                continue;
            }
            if carrier_id != 0 {
                if state != CarrierState::Ready {
                    let (handshake_deadline, opened_at, is_temporary) = {
                        let endpoint = self.endpoints.get_mut(&endpoint_id).unwrap();
                        (
                            endpoint.handshake_deadline,
                            endpoint.opened_at,
                            endpoint.is_temporary,
                        )
                    };
                    let max_deadline = opened_at
                        + if is_temporary {
                            MAX_TEMPORARY_HANDSHAKE_TIME
                        } else {
                            MAX_HANDSHAKE_TIME
                        };
                    if handshake_deadline.min(max_deadline) <= now {
                        self.fail_carrier(env, endpoint_id, "handshake timeout expired", false);
                        continue;
                    }
                } else {
                    let (unanswered_since, has_unanswered) = {
                        let endpoint = self.endpoints.get_mut(&endpoint_id).unwrap();
                        (endpoint.unanswered_since, endpoint.unanswered_since != 0.0)
                    };
                    if has_unanswered && unanswered_since + RESPONSE_TIMEOUT <= now {
                        self.fail_carrier(env, endpoint_id, "no response from the relay", false);
                        continue;
                    }
                }
                let (window_flush_at, has_window_flush) = {
                    let endpoint = self.endpoints.get_mut(&endpoint_id).unwrap();
                    (endpoint.window_flush_at, endpoint.window_flush_at != 0.0)
                };
                if has_window_flush && window_flush_at <= now {
                    self.flush_window(env, endpoint_id);
                }
            } else if !is_temporary && retry_at != 0.0 && retry_at <= now {
                self.endpoints.get_mut(&endpoint_id).unwrap().retry_at = 0.0;
                if self.network_flag {
                    self.open_carrier(env, endpoint_id);
                }
            }
        }

        self.flush_scheduled(env);
    }

    /// `WebProxyManager::update_timeout` (cpp:1051-1079) — the collapsed
    /// deadline poll; the `Td` timeout slot arms the earliest.
    pub(crate) fn next_deadline(&self) -> Option<f64> {
        let mut wakeup_at: Option<f64> = None;
        for at in self.expected_ports.values().copied() {
            relax_deadline(at, &mut wakeup_at);
        }
        for connection in &self.pending_connections {
            relax_deadline(connection.expires_at, &mut wakeup_at);
        }
        for endpoint in self.endpoints.values() {
            relax_deadline(endpoint.expires_at, &mut wakeup_at);
            if endpoint.carrier_id != 0 {
                if endpoint.state != CarrierState::Ready {
                    relax_deadline(endpoint.handshake_deadline, &mut wakeup_at);
                } else if endpoint.unanswered_since != 0.0 {
                    relax_deadline(endpoint.unanswered_since + RESPONSE_TIMEOUT, &mut wakeup_at);
                }
                relax_deadline(endpoint.window_flush_at, &mut wakeup_at);
            } else if self.network_flag {
                relax_deadline(endpoint.retry_at, &mut wakeup_at);
            }
        }
        wakeup_at
    }

    /// `WebProxyManager::hangup` (cpp:253-264) — the `Td::clear` teardown;
    /// every open carrier closes (with its `updateWebProxyCarrierClosed`).
    pub(crate) fn hangup(&mut self, env: &mut impl WebProxyManagerEnv) {
        let endpoint_ids: Vec<i64> = self.endpoints.keys().copied().collect();
        for endpoint_id in endpoint_ids {
            self.destroy_endpoint(env, endpoint_id);
        }
        self.pending_connections.clear();
        self.flush_scheduled(env);
    }
}

/// The `update_timeout` relax lambda (cpp:1055-1059).
fn relax_deadline(at: f64, wakeup_at: &mut Option<f64>) {
    if at != 0.0 && wakeup_at.is_none_or(|current| at < current) {
        *wakeup_at = Some(at);
    }
}

/// `remember_closed_stream` (cpp:942-953) + the `42c0f2e77`
/// `CHECK(stream_id != 0)` gate.
fn remember_closed_stream(endpoint: &mut Endpoint, stream_id: u32) {
    debug_assert!(stream_id != 0);
    if !endpoint.closed_stream_ids.insert(stream_id) {
        return;
    }
    endpoint.closed_stream_id_order.push_back(stream_id);
    if endpoint.closed_stream_id_order.len() > MAX_CLOSED_STREAM_IDS
        && let Some(front) = endpoint.closed_stream_id_order.pop_front()
    {
        endpoint.closed_stream_ids.remove(&front);
    }
}

/// `WebProxyManager::allocate_stream_id` (cpp:503-511).
#[cfg_attr(not(test), allow(dead_code))]
fn allocate_stream_id(endpoint: &mut Endpoint) -> u32 {
    loop {
        let stream_id = endpoint.next_stream_id;
        endpoint.next_stream_id = if stream_id >= MAX_STREAM_ID {
            1
        } else {
            stream_id + 1
        };
        if !endpoint.streams.contains_key(&stream_id)
            && !endpoint.closed_stream_ids.contains(&stream_id)
        {
            return stream_id;
        }
    }
}

/// `fail_promises` (the `Promise.h` helper; the autosave/user-manager
/// convention).
fn fail_promises<T: Send>(promises: &mut Vec<Promise<T>>, error: &Status) {
    for promise in promises.iter_mut() {
        promise.set_error(error.clone());
    }
    promises.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tdutils::promise::PromiseInterface;

    /// The settled-result cell the [`RecordingPromise`] writes.
    type RecordingResult<T> = Arc<Mutex<Option<Result<T, Status>>>>;

    /// The recording [`PromiseInterface`] — the exact settled value/error
    /// stays observable for the band assertions.
    struct RecordingPromise<T: Send + 'static> {
        result: RecordingResult<T>,
    }

    impl<T: Send + 'static> RecordingPromise<T> {
        fn create() -> (RecordingResult<T>, Promise<T>) {
            let result: RecordingResult<T> = Arc::new(Mutex::new(None));
            (
                result.clone(),
                Promise::from_interface(RecordingPromise { result }),
            )
        }
    }

    impl<T: Send + 'static> PromiseInterface<T> for RecordingPromise<T> {
        fn set_value(&mut self, value: T) {
            *self.result.lock().unwrap() = Some(Ok(value));
        }

        fn set_error(&mut self, error: Status) {
            *self.result.lock().unwrap() = Some(Err(error));
        }
    }

    /// The offline env: one recorded clock, the update outbox, and the
    /// recorded seam calls (the listener/stream halves park here).
    #[derive(Debug, Default)]
    struct TestWebProxyEnv {
        now: f64,
        updates: Vec<td_api::Update>,
        listener_starts: Vec<i64>,
        stream_created: Vec<u64>,
        stream_opened: Vec<u64>,
        stream_windows: Vec<(u64, u32)>,
        stream_writes: Vec<(u64, Vec<u8>)>,
        stream_closed: Vec<u64>,
        addresses_closed: Vec<String>,
    }

    impl WebProxyManagerEnv for TestWebProxyEnv {
        fn now(&self) -> f64 {
            self.now
        }

        fn send_web_proxy_update(&mut self, update: td_api::Update) {
            self.updates.push(update);
        }

        fn open_web_proxy_listener(&mut self, endpoint_id: i64) {
            self.listener_starts.push(endpoint_id);
        }

        fn stream_created(&mut self, stream_key: u64) {
            self.stream_created.push(stream_key);
        }

        fn stream_open(&mut self, stream_key: u64) {
            self.stream_opened.push(stream_key);
        }

        fn stream_add_send_window(&mut self, stream_key: u64, amount: u32) {
            self.stream_windows.push((stream_key, amount));
        }

        fn stream_write(&mut self, stream_key: u64, data: &[u8]) {
            self.stream_writes.push((stream_key, data.to_vec()));
        }

        fn stream_close(&mut self, stream_key: u64) {
            self.stream_closed.push(stream_key);
        }

        fn on_web_proxy_address_closed(&mut self, ip_address: &str) {
            self.addresses_closed.push(ip_address.to_string());
        }
    }

    fn web_proxy(server: &str) -> Proxy {
        Proxy::web(
            server.to_string(),
            String::new(),
            tdmtproto::proxy_secret::ProxySecret::from_raw(vec![1, 2, 3, 4]),
        )
    }

    /// The relay frame wire form (cpp:30).
    fn frame(frame_type: u8, stream_id: u32, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![
            frame_type,
            (stream_id >> 16) as u8,
            (stream_id >> 8) as u8,
            stream_id as u8,
        ];
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(payload);
        out
    }

    fn opened_carriers(env: &TestWebProxyEnv) -> Vec<td_api::WebProxyCarrier> {
        env.updates
            .iter()
            .filter_map(|update| match update {
                td_api::Update::updateWebProxyCarrierOpened(opened) => opened.carrier.clone(),
                _ => None,
            })
            .collect()
    }

    fn data_updates(env: &TestWebProxyEnv) -> Vec<(i32, Vec<u8>)> {
        env.updates
            .iter()
            .filter_map(|update| match update {
                td_api::Update::updateWebProxyCarrierData(data) => {
                    Some((data.carrier_id, data.data.clone()))
                }
                _ => None,
            })
            .collect()
    }

    fn closed_carrier_ids(env: &TestWebProxyEnv) -> Vec<i32> {
        env.updates
            .iter()
            .filter_map(|update| match update {
                td_api::Update::updateWebProxyCarrierClosed(closed) => Some(closed.carrier_id),
                _ => None,
            })
            .collect()
    }

    /// The nonce rides the carrier URL's fragment (cpp:538).
    fn nonce_from_url(url: &str) -> String {
        url.split("#android=")
            .nth(1)
            .expect("the nonce fragment")
            .to_string()
    }

    fn process_text(
        manager: &mut WebProxyManager,
        env: &mut TestWebProxyEnv,
        carrier_id: i32,
        text: &str,
    ) -> Option<Result<(), Status>> {
        let (result, promise) = RecordingPromise::<()>::create();
        manager.process_carrier_message(
            env,
            carrier_id,
            Some(td_api::WebProxyCarrierMessage::webProxyCarrierMessageText(
                td_api::webProxyCarrierMessageText {
                    text: text.to_string(),
                },
            )),
            promise,
        );
        result.lock().unwrap().clone()
    }

    fn process_binary(
        manager: &mut WebProxyManager,
        env: &mut TestWebProxyEnv,
        carrier_id: i32,
        data: &[u8],
    ) -> Option<Result<(), Status>> {
        let (result, promise) = RecordingPromise::<()>::create();
        manager.process_carrier_message(
            env,
            carrier_id,
            Some(
                td_api::WebProxyCarrierMessage::webProxyCarrierMessageBinary(
                    td_api::webProxyCarrierMessageBinary {
                        data: data.to_vec(),
                    },
                ),
            ),
            promise,
        );
        result.lock().unwrap().clone()
    }

    fn report_error(
        manager: &mut WebProxyManager,
        env: &mut TestWebProxyEnv,
        carrier_id: i32,
        error: td_api::WebProxyCarrierError,
    ) -> Option<Result<(), Status>> {
        let (result, promise) = RecordingPromise::<()>::create();
        manager.report_carrier_error(env, carrier_id, Some(error), promise);
        result.lock().unwrap().clone()
    }

    /// The opened carrier + the completed handshake: the standard Ready
    /// state the stream tests share.
    fn ready_carrier() -> (WebProxyManager, TestWebProxyEnv, i32) {
        let mut manager = WebProxyManager::default();
        let mut env = TestWebProxyEnv::default();
        manager.set_active_proxy(&mut env, 1, web_proxy("proxy.example.com"));
        let carrier = &opened_carriers(&env)[0];
        let carrier_id = carrier.id;
        let nonce = nonce_from_url(&carrier.url);
        let init = format!("{{\"t\":\"tproxy-android-init\",\"v\":1,\"nonce\":\"{nonce}\"}}");
        process_text(&mut manager, &mut env, carrier_id, &init);
        process_binary(
            &mut manager,
            &mut env,
            carrier_id,
            &frame(FRAME_WELCOME, 0, &[]),
        );
        (manager, env, carrier_id)
    }

    /// `Proxy::get_web_proxy_bridge_capability` (net/Proxy.cpp:165-177) —
    /// the URL carries the exact HMAC composition.
    #[test]
    fn carrier_url_carries_capability_and_nonce() {
        let mut manager = WebProxyManager::default();
        let mut env = TestWebProxyEnv::default();
        let proxy = web_proxy("proxy.example.com");
        manager.set_active_proxy(&mut env, 7, proxy.clone());

        let carriers = opened_carriers(&env);
        assert_eq!(carriers.len(), 1);
        let carrier = &carriers[0];
        assert_eq!(carrier.id, 1);
        assert_eq!(carrier.proxy_id, 7);
        assert_eq!(carrier.origin, "https://proxy.example.com");
        assert_eq!(carrier.path_prefix, "/");
        assert_eq!(carrier.bridge_object_name, "TelegramWebProxy");
        assert!(!carrier.is_temporary);

        let capability = proxy.get_web_proxy_bridge_capability();
        let expected_hash = tdutils::crypto::hmac_sha256(
            proxy.secret.get_raw_secret(),
            b"tdesktop-web-proxy-bridge-v1\nproxy.example.com",
        );
        assert_eq!(
            capability,
            String::from_utf8_lossy(&base64url_encode(&expected_hash))
        );
        assert_eq!(
            carrier.url,
            format!(
                "https://proxy.example.com/?bridge={capability}#android={}",
                nonce_from_url(&carrier.url)
            )
        );
        // the 32-byte secure nonce base64urls to 43 characters
        assert_eq!(nonce_from_url(&carrier.url).len(), 43);
    }

    /// The pathed relay URL (cpp:531-534) — the prefix wraps the path in
    /// slashes; the capability context switches to v2.
    #[test]
    fn pathed_carrier_url_uses_v2_context() {
        let mut manager = WebProxyManager::default();
        let mut env = TestWebProxyEnv::default();
        let proxy = Proxy::web(
            "proxy.example.com".to_string(),
            "relay".to_string(),
            tdmtproto::proxy_secret::ProxySecret::from_raw(vec![1, 2, 3, 4]),
        );
        manager.set_active_proxy(&mut env, 1, proxy.clone());
        let carrier = &opened_carriers(&env)[0];
        assert_eq!(carrier.path_prefix, "/relay/");
        let expected_hash = tdutils::crypto::hmac_sha256(
            proxy.secret.get_raw_secret(),
            b"tdesktop-web-proxy-bridge-v2\nproxy.example.com\nrelay",
        );
        let encoded_hash = base64url_encode(&expected_hash);
        let capability = String::from_utf8_lossy(&encoded_hash);
        assert!(carrier.url.starts_with(&format!(
            "https://proxy.example.com/relay/?bridge={capability}#android="
        )));
    }

    /// The same-proxy `set_active_proxy` is a no-op (cpp:291-295).
    #[test]
    fn same_active_proxy_skips_reopen() {
        let mut manager = WebProxyManager::default();
        let mut env = TestWebProxyEnv::default();
        let proxy = web_proxy("proxy.example.com");
        manager.set_active_proxy(&mut env, 1, proxy.clone());
        manager.set_active_proxy(&mut env, 1, proxy);
        assert_eq!(opened_carriers(&env).len(), 1);
    }

    /// A proxy change closes the old carrier and opens the new one; the
    /// disable closes the last one (cpp:296-301).
    #[test]
    fn proxy_change_cycles_carriers() {
        let mut manager = WebProxyManager::default();
        let mut env = TestWebProxyEnv::default();
        manager.set_active_proxy(&mut env, 1, web_proxy("a.example"));
        manager.set_active_proxy(&mut env, 2, web_proxy("b.example"));
        assert_eq!(closed_carrier_ids(&env), vec![1]);
        assert_eq!(opened_carriers(&env).len(), 2);
        assert_eq!(opened_carriers(&env)[1].proxy_id, 2);
        manager.set_active_proxy(&mut env, 0, Proxy::default());
        assert_eq!(closed_carrier_ids(&env), vec![1, 2]);
        assert!(manager.next_deadline().is_none());
    }

    /// A non-web `set_active_proxy` closes without opening (the C++
    /// `use_web_proxy` filter, cpp:299-301).
    #[test]
    fn non_web_active_proxy_only_closes() {
        let mut manager = WebProxyManager::default();
        let mut env = TestWebProxyEnv::default();
        manager.set_active_proxy(&mut env, 1, web_proxy("a.example"));
        manager.set_active_proxy(
            &mut env,
            2,
            Proxy {
                type_: crate::connection_creator::ProxyType::Socks5,
                server: "s.example".to_string(),
                port: 1080,
                ..Proxy::default()
            },
        );
        assert_eq!(closed_carrier_ids(&env), vec![1]);
        assert_eq!(opened_carriers(&env).len(), 1);
    }

    /// The init handshake advances to WaitWelcome and answers with the
    /// HELLO frame (cpp:694-701).
    #[test]
    fn init_handshake_emits_hello() {
        let mut manager = WebProxyManager::default();
        let mut env = TestWebProxyEnv::default();
        manager.set_active_proxy(&mut env, 1, web_proxy("proxy.example.com"));
        let carrier = &opened_carriers(&env)[0];
        let nonce = nonce_from_url(&carrier.url);
        let result = process_text(
            &mut manager,
            &mut env,
            carrier.id,
            &format!("{{\"t\":\"tproxy-android-init\",\"v\":1,\"nonce\":\"{nonce}\"}}"),
        );
        assert!(matches!(result, Some(Ok(()))));
        let data = data_updates(&env);
        assert_eq!(data.len(), 1);
        assert_eq!(data[0].0, carrier.id);
        assert_eq!(data[0].1, frame(FRAME_HELLO, 0, b"\x01"));
    }

    /// The wrong nonce/version/state fails the carrier (cpp:696-700).
    #[test]
    fn invalid_init_fails_carrier() {
        let mut manager = WebProxyManager::default();
        let mut env = TestWebProxyEnv::default();
        manager.set_active_proxy(&mut env, 1, web_proxy("proxy.example.com"));
        let carrier_id = opened_carriers(&env)[0].id;
        let nonce = nonce_from_url(&opened_carriers(&env)[0].url);

        let result = process_text(
            &mut manager,
            &mut env,
            carrier_id,
            "{\"t\":\"tproxy-android-init\",\"v\":1,\"nonce\":\"wrong\"}",
        );
        assert!(matches!(result, Some(Ok(()))));
        assert_eq!(closed_carrier_ids(&env), vec![carrier_id]);
        // the retry ladder starts at MIN_RETRY_DELAY (cpp:604-608)
        assert_eq!(manager.next_deadline(), Some(env.now + MIN_RETRY_DELAY));

        // a fresh carrier with the right nonce but the wrong version
        env.now += MIN_RETRY_DELAY + 0.1;
        manager.timeout_expired(&mut env);
        assert_eq!(opened_carriers(&env).len(), 2);
        let carrier_id = opened_carriers(&env)[1].id;
        let fresh_nonce = nonce_from_url(&opened_carriers(&env)[1].url);
        let result = process_text(
            &mut manager,
            &mut env,
            carrier_id,
            &format!("{{\"t\":\"tproxy-android-init\",\"v\":2,\"nonce\":\"{fresh_nonce}\"}}"),
        );
        assert!(matches!(result, Some(Ok(()))));
        assert_eq!(closed_carrier_ids(&env), vec![carrier_id - 1, carrier_id]);
        let _ = nonce;
    }

    /// The `close` page message fails the carrier (cpp:702-704).
    #[test]
    fn page_close_fails_carrier() {
        let (mut manager, mut env, carrier_id) = ready_carrier();
        let result = process_text(&mut manager, &mut env, carrier_id, "{\"t\":\"close\"}");
        assert!(matches!(result, Some(Ok(()))));
        assert_eq!(closed_carrier_ids(&env), vec![carrier_id]);
    }

    /// The status handshake extension band (cpp:706-714).
    #[test]
    fn status_connecting_extends_handshake_deadline() {
        let mut manager = WebProxyManager::default();
        let mut env = TestWebProxyEnv::default();
        manager.set_active_proxy(&mut env, 1, web_proxy("proxy.example.com"));
        let carrier_id = opened_carriers(&env)[0].id;
        env.now = 20.0;
        let result = process_text(
            &mut manager,
            &mut env,
            carrier_id,
            "{\"t\":\"status\",\"state\":\"connecting\"}",
        );
        assert!(matches!(result, Some(Ok(()))));
        // max(opened_at + 10, min(now + 10, opened_at + 45)) = 30.0
        assert_eq!(manager.next_deadline(), Some(30.0));
        // a failed page status still fails the carrier
        let result = process_text(
            &mut manager,
            &mut env,
            carrier_id,
            "{\"t\":\"status\",\"state\":\"failed\"}",
        );
        assert!(matches!(result, Some(Ok(()))));
        assert_eq!(closed_carrier_ids(&env), vec![carrier_id]);
    }

    /// Unknown page texts are ignored (cpp:689-693).
    #[test]
    fn unknown_text_messages_are_ignored() {
        let (mut manager, mut env, carrier_id) = ready_carrier();
        let before = env.updates.len();
        assert!(matches!(
            process_text(&mut manager, &mut env, carrier_id, "not-json"),
            Some(Ok(()))
        ));
        assert!(matches!(
            process_text(&mut manager, &mut env, carrier_id, "[1,2,3]"),
            Some(Ok(()))
        ));
        assert!(matches!(
            process_text(&mut manager, &mut env, carrier_id, "{\"t\":\"unknown\"}"),
            Some(Ok(()))
        ));
        assert_eq!(env.updates.len(), before);
    }

    /// Written bytes of a stream that is not open yet never window back
    /// (the endpoint/state/opened gate, cpp:869-878).
    #[test]
    fn written_bytes_of_unopened_stream_are_dropped() {
        let mut manager = WebProxyManager::default();
        let mut env = TestWebProxyEnv::default();
        manager.set_active_proxy(&mut env, 1, web_proxy("proxy.example.com"));
        // the handshake never completes — the accepted stream stays closed
        manager.accept_connection(&mut env, manager.active_endpoint_id);
        let stream_key = manager.max_stream_key;
        env.updates.clear();

        manager.on_stream_written(&mut env, stream_key, 100);
        assert_eq!(manager.streams[&stream_key].pending_window, 0);
        assert!(data_updates(&env).is_empty());
    }

    /// Binary before init fails; the welcome completes the handshake and
    /// opens the pre-accepted streams (cpp:717-721, cpp:753-759).
    #[test]
    fn welcome_completes_handshake_and_opens_streams() {
        let mut manager = WebProxyManager::default();
        let mut env = TestWebProxyEnv::default();
        manager.set_active_proxy(&mut env, 1, web_proxy("proxy.example.com"));
        let carrier_id = opened_carriers(&env)[0].id;

        // binary before init fails the carrier
        assert!(matches!(
            process_binary(
                &mut manager,
                &mut env,
                carrier_id,
                &frame(FRAME_PING, 0, b"x")
            ),
            Some(Ok(()))
        ));
        assert_eq!(closed_carrier_ids(&env), vec![carrier_id]);

        // the retry reopens; accept a stream before the welcome
        env.now += MIN_RETRY_DELAY + 0.1;
        manager.timeout_expired(&mut env);
        let carrier_id = opened_carriers(&env)[1].id;
        manager.accept_connection(&mut env, manager.active_endpoint_id);
        assert!(data_updates(&env).is_empty());

        let nonce = nonce_from_url(&opened_carriers(&env)[1].url);
        process_text(
            &mut manager,
            &mut env,
            carrier_id,
            &format!("{{\"t\":\"tproxy-android-init\",\"v\":1,\"nonce\":\"{nonce}\"}}"),
        );
        process_binary(
            &mut manager,
            &mut env,
            carrier_id,
            &frame(FRAME_WELCOME, 0, &[]),
        );
        // the pre-accepted stream opened with its FRAME_OPEN
        let data = data_updates(&env);
        assert_eq!(data.len(), 2);
        assert_eq!(data[1].1, frame(FRAME_OPEN, 1, &[]));
        assert_eq!(env.stream_created, vec![1]);
        assert_eq!(env.stream_opened, vec![1]);
    }

    /// The welcome-frame guard bands (cpp:754-757).
    #[test]
    fn welcome_guard_bands_fail_carrier() {
        let mut manager = WebProxyManager::default();
        let mut env = TestWebProxyEnv::default();
        manager.set_active_proxy(&mut env, 1, web_proxy("proxy.example.com"));
        let carrier_id = opened_carriers(&env)[0].id;
        let nonce = nonce_from_url(&opened_carriers(&env)[0].url);
        process_text(
            &mut manager,
            &mut env,
            carrier_id,
            &format!("{{\"t\":\"tproxy-android-init\",\"v\":1,\"nonce\":\"{nonce}\"}}"),
        );

        // a stream frame before the welcome
        assert!(matches!(
            process_binary(
                &mut manager,
                &mut env,
                carrier_id,
                &frame(FRAME_PING, 1, b"x")
            ),
            Some(Ok(()))
        ));
        assert_eq!(closed_carrier_ids(&env), vec![carrier_id]);
        env.now += MIN_RETRY_DELAY + 0.1;
        manager.timeout_expired(&mut env);
        let carrier_id = opened_carriers(&env)[1].id;
        let nonce = nonce_from_url(&opened_carriers(&env)[1].url);
        process_text(
            &mut manager,
            &mut env,
            carrier_id,
            &format!("{{\"t\":\"tproxy-android-init\",\"v\":1,\"nonce\":\"{nonce}\"}}"),
        );
        // a WELCOME with a nonzero stream id
        assert!(matches!(
            process_binary(
                &mut manager,
                &mut env,
                carrier_id,
                &frame(FRAME_WELCOME, 1, &[])
            ),
            Some(Ok(()))
        ));
        assert_eq!(closed_carrier_ids(&env), vec![carrier_id - 1, carrier_id]);
    }

    /// The ping/pong session frames and the BYE failure (cpp:761-775).
    #[test]
    fn session_frames_ping_pong_and_bye() {
        let (mut manager, mut env, carrier_id) = ready_carrier();
        env.updates.clear();

        assert!(matches!(
            process_binary(
                &mut manager,
                &mut env,
                carrier_id,
                &frame(FRAME_PING, 0, b"abc")
            ),
            Some(Ok(()))
        ));
        assert_eq!(
            data_updates(&env),
            vec![(carrier_id, frame(FRAME_PONG, 0, b"abc"))]
        );

        assert!(matches!(
            process_binary(
                &mut manager,
                &mut env,
                carrier_id,
                &frame(FRAME_BYE, 0, &[])
            ),
            Some(Ok(()))
        ));
        assert_eq!(closed_carrier_ids(&env), vec![carrier_id]);
    }

    /// A too-long ping fails the carrier (cpp:763-765).
    #[test]
    fn too_long_ping_fails_carrier() {
        let (mut manager, mut env, carrier_id) = ready_carrier();
        let payload = vec![b'x'; MAX_PING_PAYLOAD_SIZE + 1];
        assert!(matches!(
            process_binary(
                &mut manager,
                &mut env,
                carrier_id,
                &frame(FRAME_PING, 0, &payload)
            ),
            Some(Ok(()))
        ));
        assert_eq!(closed_carrier_ids(&env), vec![carrier_id]);
    }

    /// The frame-parse guard bands (cpp:725-740).
    #[test]
    fn frame_parse_bands_fail_carrier() {
        let (mut manager, mut env, carrier_id) = ready_carrier();

        // a partial header
        assert!(matches!(
            process_binary(&mut manager, &mut env, carrier_id, &[0u8; 4]),
            Some(Ok(()))
        ));
        assert_eq!(closed_carrier_ids(&env), vec![carrier_id]);
        env.now += MIN_RETRY_DELAY + 0.1;
        manager.timeout_expired(&mut env);
        let carrier_id = opened_carriers(&env)[1].id;
        let nonce = nonce_from_url(&opened_carriers(&env)[1].url);
        process_text(
            &mut manager,
            &mut env,
            carrier_id,
            &format!("{{\"t\":\"tproxy-android-init\",\"v\":1,\"nonce\":\"{nonce}\"}}"),
        );
        process_binary(
            &mut manager,
            &mut env,
            carrier_id,
            &frame(FRAME_WELCOME, 0, &[]),
        );

        // an over-long frame length
        let mut bad = frame(FRAME_PING, 0, b"");
        bad[4..8].copy_from_slice(&((MAX_FRAME_PAYLOAD_SIZE + 1) as u32).to_be_bytes());
        assert!(matches!(
            process_binary(&mut manager, &mut env, carrier_id, &bad),
            Some(Ok(()))
        ));
        assert_eq!(closed_carrier_ids(&env), vec![carrier_id - 1, carrier_id]);
        env.now += MIN_RETRY_DELAY + 0.1;
        manager.timeout_expired(&mut env);
        let carrier_id = opened_carriers(&env)[2].id;
        let nonce = nonce_from_url(&opened_carriers(&env)[2].url);
        process_text(
            &mut manager,
            &mut env,
            carrier_id,
            &format!("{{\"t\":\"tproxy-android-init\",\"v\":1,\"nonce\":\"{nonce}\"}}"),
        );
        process_binary(
            &mut manager,
            &mut env,
            carrier_id,
            &frame(FRAME_WELCOME, 0, &[]),
        );

        // a length beyond the batch
        let mut bad = frame(FRAME_PING, 0, b"abc");
        bad[4..8].copy_from_slice(&16u32.to_be_bytes());
        assert!(matches!(
            process_binary(&mut manager, &mut env, carrier_id, &bad),
            Some(Ok(()))
        ));
        assert_eq!(
            closed_carrier_ids(&env),
            vec![carrier_id - 2, carrier_id - 1, carrier_id]
        );

        // an empty binary message
        env.now += MIN_RETRY_DELAY + 0.1;
        manager.timeout_expired(&mut env);
        let carrier_id = opened_carriers(&env)[3].id;
        let nonce = nonce_from_url(&opened_carriers(&env)[3].url);
        process_text(
            &mut manager,
            &mut env,
            carrier_id,
            &format!("{{\"t\":\"tproxy-android-init\",\"v\":1,\"nonce\":\"{nonce}\"}}"),
        );
        process_binary(
            &mut manager,
            &mut env,
            carrier_id,
            &frame(FRAME_WELCOME, 0, &[]),
        );
        assert!(matches!(
            process_binary(&mut manager, &mut env, carrier_id, &[]),
            Some(Ok(()))
        ));
        assert_eq!(
            closed_carrier_ids(&env),
            vec![carrier_id - 3, carrier_id - 2, carrier_id - 1, carrier_id]
        );
    }

    /// One binary batch coalesces its replies into one data update
    /// (cpp:974-981).
    #[test]
    fn batch_replies_coalesce_into_one_update() {
        let (mut manager, mut env, carrier_id) = ready_carrier();
        env.updates.clear();
        let mut batch = frame(FRAME_PING, 0, b"a");
        batch.extend(frame(FRAME_PING, 0, b"bb"));
        assert!(matches!(
            process_binary(&mut manager, &mut env, carrier_id, &batch),
            Some(Ok(()))
        ));
        let mut expected = frame(FRAME_PONG, 0, b"a");
        expected.extend(frame(FRAME_PONG, 0, b"bb"));
        assert_eq!(data_updates(&env), vec![(carrier_id, expected)]);
    }

    /// The stream relay bands: data moves within the receive window and
    /// windows return to the relay (cpp:789-801, cpp:887-901).
    #[test]
    fn stream_data_and_window_relay() {
        let (mut manager, mut env, carrier_id) = ready_carrier();
        manager.accept_connection(&mut env, manager.active_endpoint_id);
        let stream_key = manager.max_stream_key;
        env.updates.clear();

        // relay data rides into the stream socket
        assert!(matches!(
            process_binary(
                &mut manager,
                &mut env,
                carrier_id,
                &frame(FRAME_DATA, 1, b"hello")
            ),
            Some(Ok(()))
        ));
        assert_eq!(env.stream_writes, vec![(stream_key, b"hello".to_vec())]);
        // the first stream bytes arm the unanswered clock (cpp:860-864)
        let unanswered = manager.endpoints[&manager.active_endpoint_id].unanswered_since;
        assert!((unanswered - env.now).abs() < 1e-9);

        // window frames reach the stream
        assert!(matches!(
            process_binary(
                &mut manager,
                &mut env,
                carrier_id,
                &frame(FRAME_WINDOW, 1, &1024u32.to_be_bytes()),
            ),
            Some(Ok(()))
        ));
        assert_eq!(env.stream_windows, vec![(stream_key, 1024)]);

        // a zero window frame fails the carrier
        assert!(matches!(
            process_binary(
                &mut manager,
                &mut env,
                carrier_id,
                &frame(FRAME_WINDOW, 1, &0u32.to_be_bytes()),
            ),
            Some(Ok(()))
        ));
        assert_eq!(closed_carrier_ids(&env), vec![carrier_id]);
    }

    /// Data beyond the receive window fails the carrier (cpp:790-792).
    #[test]
    fn data_beyond_receive_window_fails() {
        let (mut manager, mut env, carrier_id) = ready_carrier();
        manager.accept_connection(&mut env, manager.active_endpoint_id);
        let payload = vec![b'x'; INITIAL_WINDOW as usize + 1];
        assert!(matches!(
            process_binary(
                &mut manager,
                &mut env,
                carrier_id,
                &frame(FRAME_DATA, 1, &payload)
            ),
            Some(Ok(()))
        ));
        assert_eq!(closed_carrier_ids(&env), vec![carrier_id]);
    }

    /// The outbound side: stream reads relay as FRAME_DATA, and the written
    /// bytes return window space — immediate past the flush size, deferred
    /// under it (cpp:849-901).
    #[test]
    fn stream_reads_and_written_windows() {
        let (mut manager, mut env, carrier_id) = ready_carrier();
        manager.accept_connection(&mut env, manager.active_endpoint_id);
        let stream_key = manager.max_stream_key;
        env.updates.clear();

        manager.on_stream_data(&mut env, stream_key, b"outbound");
        assert_eq!(
            data_updates(&env),
            vec![(carrier_id, frame(FRAME_DATA, 1, b"outbound"))]
        );
        env.updates.clear();

        // written bytes are returned as a window grant, so the relay must
        // have delivered the data first (the cpp:891 CHECK guards exactly
        // this: pending_window <= INITIAL_WINDOW - receive_window)
        assert!(matches!(
            process_binary(
                &mut manager,
                &mut env,
                carrier_id,
                &frame(FRAME_DATA, 1, &[b'i'; 100])
            ),
            Some(Ok(()))
        ));
        env.updates.clear();

        // the deferred window flush rides the WINDOW_FLUSH_DELAY deadline
        manager.on_stream_written(&mut env, stream_key, 100);
        assert!(data_updates(&env).is_empty());
        let flush_at = manager.endpoints[&manager.active_endpoint_id].window_flush_at;
        assert!((flush_at - (env.now + WINDOW_FLUSH_DELAY)).abs() < 1e-9);
        env.now += WINDOW_FLUSH_DELAY + 0.001;
        manager.timeout_expired(&mut env);
        assert_eq!(
            data_updates(&env),
            vec![(carrier_id, frame(FRAME_WINDOW, 1, &100u32.to_be_bytes()))]
        );

        // past WINDOW_FLUSH_SIZE the window returns at once
        env.updates.clear();
        assert!(matches!(
            process_binary(
                &mut manager,
                &mut env,
                carrier_id,
                &frame(FRAME_DATA, 1, &vec![b'i'; WINDOW_FLUSH_SIZE as usize])
            ),
            Some(Ok(()))
        ));
        env.updates.clear();
        manager.on_stream_written(&mut env, stream_key, WINDOW_FLUSH_SIZE as usize);
        assert_eq!(
            data_updates(&env),
            vec![(
                carrier_id,
                frame(FRAME_WINDOW, 1, &WINDOW_FLUSH_SIZE.to_be_bytes())
            )]
        );
    }

    /// The relay-side CLOSE releases the stream to its drain tail and the
    /// closed-stream frames cross (cpp:802-819, cpp:776-787).
    #[test]
    fn relay_close_releases_stream_and_crossing_frames_pass() {
        let (mut manager, mut env, carrier_id) = ready_carrier();
        manager.accept_connection(&mut env, manager.active_endpoint_id);
        let stream_key = manager.max_stream_key;
        env.updates.clear();

        assert!(matches!(
            process_binary(
                &mut manager,
                &mut env,
                carrier_id,
                &frame(FRAME_CLOSE, 1, &[])
            ),
            Some(Ok(()))
        ));
        assert_eq!(env.stream_closed, vec![stream_key]);
        assert!(!manager.streams.contains_key(&stream_key));
        // the stream id is remembered closed
        assert!(
            manager.endpoints[&manager.active_endpoint_id]
                .closed_stream_ids
                .contains(&1)
        );

        // a late DATA for the closed stream crosses our CLOSE
        assert!(matches!(
            process_binary(
                &mut manager,
                &mut env,
                carrier_id,
                &frame(FRAME_DATA, 1, b"tail")
            ),
            Some(Ok(()))
        ));
        // a nonzero WINDOW crosses too
        assert!(matches!(
            process_binary(
                &mut manager,
                &mut env,
                carrier_id,
                &frame(FRAME_WINDOW, 1, &8u32.to_be_bytes()),
            ),
            Some(Ok(()))
        ));
        // a zero WINDOW does not
        assert!(matches!(
            process_binary(
                &mut manager,
                &mut env,
                carrier_id,
                &frame(FRAME_WINDOW, 1, &0u32.to_be_bytes()),
            ),
            Some(Ok(()))
        ));
        assert_eq!(closed_carrier_ids(&env), vec![carrier_id]);

        // a frame for a never-known stream fails a fresh carrier (the
        // mid-test updates.clear() leaves the reopened carrier first)
        env.now += MIN_RETRY_DELAY + 0.1;
        manager.timeout_expired(&mut env);
        let carrier_id = opened_carriers(&env)[0].id;
        let nonce = nonce_from_url(&opened_carriers(&env)[0].url);
        process_text(
            &mut manager,
            &mut env,
            carrier_id,
            &format!("{{\"t\":\"tproxy-android-init\",\"v\":1,\"nonce\":\"{nonce}\"}}"),
        );
        process_binary(
            &mut manager,
            &mut env,
            carrier_id,
            &frame(FRAME_WELCOME, 0, &[]),
        );
        assert!(matches!(
            process_binary(
                &mut manager,
                &mut env,
                carrier_id,
                &frame(FRAME_DATA, 9, b"x")
            ),
            Some(Ok(()))
        ));
        assert_eq!(closed_carrier_ids(&env), vec![carrier_id - 1, carrier_id]);
    }

    /// Our own stream close emits the CLOSE frame and remembers the id
    /// (cpp:921-934, cpp:942-953).
    #[test]
    fn own_close_emits_frame_and_remembers() {
        let (mut manager, mut env, _carrier_id) = ready_carrier();
        manager.accept_connection(&mut env, manager.active_endpoint_id);
        env.updates.clear();
        let stream_key = manager.max_stream_key;
        manager.on_stream_closed(&mut env, stream_key);
        assert_eq!(
            data_updates(&env),
            vec![(
                manager.endpoints[&manager.active_endpoint_id].carrier_id,
                frame(FRAME_CLOSE, 1, &[])
            )]
        );
    }

    /// The closed-stream-id ring caps at MAX_CLOSED_STREAM_IDS
    /// (cpp:942-953).
    #[test]
    fn closed_stream_ring_caps() {
        let (mut manager, mut env, carrier_id) = ready_carrier();
        for _ in 0..(MAX_CLOSED_STREAM_IDS + 1) {
            manager.accept_connection(&mut env, manager.active_endpoint_id);
            let stream_key = manager.max_stream_key;
            manager.on_stream_closed(&mut env, stream_key);
        }
        let endpoint = &manager.endpoints[&manager.active_endpoint_id];
        assert_eq!(endpoint.closed_stream_ids.len(), MAX_CLOSED_STREAM_IDS);
        // the ring evicted the oldest id (1) when 4097 arrived
        assert_eq!(endpoint.closed_stream_id_order.front(), Some(&2));
        env.updates.clear();
        // the evicted id is unknown now
        assert!(matches!(
            process_binary(
                &mut manager,
                &mut env,
                carrier_id,
                &frame(FRAME_DATA, 1, b"x")
            ),
            Some(Ok(()))
        ));
        assert_eq!(closed_carrier_ids(&env), vec![carrier_id]);
    }

    /// The MAX_STREAMS gate drops extra accepts (cpp:480-483).
    #[test]
    fn stream_count_gate_drops_extra_accepts() {
        let (mut manager, mut env, _carrier_id) = ready_carrier();
        for _ in 0..MAX_STREAMS {
            manager.accept_connection(&mut env, manager.active_endpoint_id);
        }
        let streams_before = manager.streams.len();
        manager.accept_connection(&mut env, manager.active_endpoint_id);
        assert_eq!(manager.streams.len(), streams_before);
        assert_eq!(env.stream_created.len(), MAX_STREAMS);
    }

    /// The expect_connection registry: a registered port accepts a parked
    /// connection; an unregistered one arms the timeout (cpp:441-453).
    #[test]
    fn expect_connection_registry() {
        let mut manager = WebProxyManager::default();
        let mut env = TestWebProxyEnv::default();
        manager.set_active_proxy(&mut env, 1, web_proxy("proxy.example.com"));

        manager.expect_connection(&mut env, 4444);
        // the registration arms its own EXPECTED_PORT_TIMEOUT sweep (the
        // global deadline is the endpoint's earlier handshake clock)
        assert_eq!(
            manager.expected_ports.get(&4444),
            Some(&(env.now + EXPECTED_PORT_TIMEOUT))
        );

        // the sweep drops the expired registration
        env.now += EXPECTED_PORT_TIMEOUT + 0.1;
        manager.timeout_expired(&mut env);
        assert!(!manager.expected_ports.contains_key(&4444));

        // a pending connection (accepted before its port registered)
        // accepts when the port arrives
        manager.accept_connection(&mut env, manager.active_endpoint_id);
        manager.pending_connections.push(PendingConnection {
            endpoint_id: manager.active_endpoint_id,
            port: 5555,
            expires_at: env.now + PENDING_CONNECTION_TIMEOUT,
        });
        manager.expect_connection(&mut env, 5555);
        assert!(manager.pending_connections.is_empty());
        assert_eq!(env.stream_created.len(), 2);
    }

    /// The pending-connection sweep drops expired entries (cpp:1012-1014).
    #[test]
    fn pending_connections_sweep() {
        let mut manager = WebProxyManager::default();
        let mut env = TestWebProxyEnv::default();
        manager.set_active_proxy(&mut env, 1, web_proxy("proxy.example.com"));
        manager.pending_connections.push(PendingConnection {
            endpoint_id: manager.active_endpoint_id,
            port: 1,
            expires_at: env.now + PENDING_CONNECTION_TIMEOUT,
        });
        env.now += PENDING_CONNECTION_TIMEOUT + 0.1;
        manager.timeout_expired(&mut env);
        assert!(manager.pending_connections.is_empty());
    }

    /// The request-arm bands: null message/error and the unknown-carrier
    /// ladder (cpp:614-616, cpp:636-643, cpp:647-650, cpp:882-884).
    #[test]
    fn request_arm_null_and_unknown_bands() {
        let (mut manager, mut env, carrier_id) = ready_carrier();

        let (result, promise) = RecordingPromise::<()>::create();
        manager.process_carrier_message(&mut env, carrier_id, None, promise);
        let error = result
            .lock()
            .unwrap()
            .clone()
            .expect("the promise settled")
            .unwrap_err();
        assert_eq!(error.code(), 400);
        assert_eq!(error.message(), "Message must be non-empty");

        let (result, promise) = RecordingPromise::<()>::create();
        manager.report_carrier_error(&mut env, carrier_id, None, promise);
        let error = result
            .lock()
            .unwrap()
            .clone()
            .expect("the promise settled")
            .unwrap_err();
        assert_eq!(error.message(), "Error must be non-empty");

        // the null-message band precedes the carrier lookup (cpp:614-618),
        // so the unknown-carrier probe carries a real message
        let (result, promise) = RecordingPromise::<()>::create();
        manager.process_carrier_message(
            &mut env,
            0,
            Some(td_api::WebProxyCarrierMessage::webProxyCarrierMessageText(
                td_api::webProxyCarrierMessageText {
                    text: String::new(),
                },
            )),
            promise,
        );
        let error = result
            .lock()
            .unwrap()
            .clone()
            .expect("the promise settled")
            .unwrap_err();
        assert_eq!(error.message(), "WEB_PROXY_CARRIER_NOT_FOUND");

        // a closed-but-known carrier id answers ok (cpp:638-641)
        manager.report_carrier_error(
            &mut env,
            carrier_id,
            Some(
                td_api::WebProxyCarrierError::webProxyCarrierErrorTerminated(
                    td_api::webProxyCarrierErrorTerminated,
                ),
            ),
            RecordingPromise::<()>::create().1,
        );
        assert_eq!(closed_carrier_ids(&env), vec![carrier_id]);
        let (result, promise) = RecordingPromise::<()>::create();
        manager.process_carrier_message(
            &mut env,
            carrier_id,
            Some(td_api::WebProxyCarrierMessage::webProxyCarrierMessageText(
                td_api::webProxyCarrierMessageText {
                    text: "{\"t\":\"unknown\"}".to_string(),
                },
            )),
            promise,
        );
        assert!(matches!(result.lock().unwrap().clone(), Some(Ok(()))));

        // the null-error band precedes the carrier lookup (cpp:647-651),
        // so the negative-id probe carries a real error
        let (result, promise) = RecordingPromise::<()>::create();
        manager.report_carrier_error(
            &mut env,
            -5,
            Some(td_api::WebProxyCarrierError::webProxyCarrierErrorPaused(
                td_api::webProxyCarrierErrorPaused,
            )),
            promise,
        );
        let error = result
            .lock()
            .unwrap()
            .clone()
            .expect("the promise settled")
            .unwrap_err();
        assert_eq!(error.message(), "WEB_PROXY_CARRIER_NOT_FOUND");
    }

    /// The report bands (cpp:652-679).
    #[test]
    fn report_error_bands() {
        // the paused band arms the PAUSED_RETRY_DELAY retry
        let (mut manager, mut env, carrier_id) = ready_carrier();
        assert!(matches!(
            report_error(
                &mut manager,
                &mut env,
                carrier_id,
                td_api::WebProxyCarrierError::webProxyCarrierErrorPaused(
                    td_api::webProxyCarrierErrorPaused
                ),
            ),
            Some(Ok(()))
        ));
        assert_eq!(closed_carrier_ids(&env), vec![carrier_id]);
        assert_eq!(manager.next_deadline(), Some(env.now + PAUSED_RETRY_DELAY));

        // the refused band differs for a temporary carrier
        let mut manager = WebProxyManager::default();
        let mut env = TestWebProxyEnv::default();
        let (result, promise) = RecordingPromise::<TemporaryCarrier>::create();
        manager.open_temporary_carrier(&mut env, web_proxy("proxy.example.com"), promise);
        let carrier_id = opened_carriers(&env)[0].id;
        assert!(result.lock().unwrap().clone().is_none());
        assert!(matches!(
            report_error(
                &mut manager,
                &mut env,
                carrier_id,
                td_api::WebProxyCarrierError::webProxyCarrierErrorRefused(
                    td_api::webProxyCarrierErrorRefused
                ),
            ),
            Some(Ok(()))
        ));
        let error = result
            .lock()
            .unwrap()
            .clone()
            .expect("the promise settled")
            .unwrap_err();
        assert_eq!(error.message(), "WEB_PROXY_CARRIER_REFUSED");
        assert!(manager.endpoints.is_empty());
    }

    /// The unsupported band: the listener closes, the retry never arms and
    /// a fresh address request answers the unsupported error (cpp:652-667).
    #[test]
    fn unsupported_band_closes_listener_and_never_retries() {
        let mut manager = WebProxyManager::default();
        let mut env = TestWebProxyEnv::default();
        manager.set_active_proxy(&mut env, 1, web_proxy("proxy.example.com"));
        let carrier_id = opened_carriers(&env)[0].id;
        let (address, promise) = RecordingPromise::<String>::create();
        manager.get_active_proxy_address(&mut env, promise);
        assert!(address.lock().unwrap().clone().is_none());

        assert!(matches!(
            report_error(
                &mut manager,
                &mut env,
                carrier_id,
                td_api::WebProxyCarrierError::webProxyCarrierErrorUnsupported(
                    td_api::webProxyCarrierErrorUnsupported,
                ),
            ),
            Some(Ok(()))
        ));
        assert_eq!(closed_carrier_ids(&env), vec![carrier_id]);
        assert!(manager.next_deadline().is_none());
        // the parked address promise failed with the close_listener error
        // (cpp:659-662)
        let error = address
            .lock()
            .unwrap()
            .clone()
            .expect("the promise settled")
            .unwrap_err();
        assert_eq!(
            error.message(),
            "Web proxy is unsupported by the application"
        );

        let (address, promise) = RecordingPromise::<String>::create();
        manager.get_active_proxy_address(&mut env, promise);
        let error = address
            .lock()
            .unwrap()
            .clone()
            .expect("the promise settled")
            .unwrap_err();
        assert_eq!(
            error.message(),
            "Web proxy is unsupported by the application"
        );
    }

    /// The temporary carrier: the expiry destroys the endpoint
    /// (cpp:1019-1022).
    #[test]
    fn temporary_carrier_expires() {
        let mut manager = WebProxyManager::default();
        let mut env = TestWebProxyEnv::default();
        let (result, promise) = RecordingPromise::<TemporaryCarrier>::create();
        manager.open_temporary_carrier(&mut env, web_proxy("proxy.example.com"), promise);
        let carrier = &opened_carriers(&env)[0];
        assert_eq!(carrier.proxy_id, 0);
        assert!(carrier.is_temporary);
        // the lifetime registration (the global deadline is the endpoint's
        // earlier handshake clock, as by the C++ update_timeout relax set)
        assert_eq!(
            manager.endpoints.values().next().map(|e| e.expires_at),
            Some(env.now + TEMPORARY_ENDPOINT_LIFETIME)
        );

        env.now += TEMPORARY_ENDPOINT_LIFETIME + 0.1;
        manager.timeout_expired(&mut env);
        assert!(manager.endpoints.is_empty());
        let error = result
            .lock()
            .unwrap()
            .clone()
            .expect("the promise settled")
            .unwrap_err();
        assert_eq!(error.message(), "WEB_PROXY_CARRIER_FAILED");
        assert_eq!(closed_carrier_ids(&env), vec![carrier.id]);
    }

    /// A non-web `open_temporary_carrier` answers the 400 (cpp:334-336).
    #[test]
    fn temporary_carrier_rejects_non_web() {
        let mut manager = WebProxyManager::default();
        let mut env = TestWebProxyEnv::default();
        let (result, promise) = RecordingPromise::<TemporaryCarrier>::create();
        manager.open_temporary_carrier(
            &mut env,
            Proxy {
                type_: crate::connection_creator::ProxyType::Socks5,
                server: "s.example".to_string(),
                port: 1080,
                ..Proxy::default()
            },
            promise,
        );
        let error = result
            .lock()
            .unwrap()
            .clone()
            .expect("the promise settled")
            .unwrap_err();
        assert_eq!(error.message(), "Proxy is not a web proxy");
        assert!(manager.endpoints.is_empty());
    }

    /// The get_active_proxy_address bands (cpp:303-319).
    #[test]
    fn address_request_bands() {
        let mut manager = WebProxyManager::default();
        let mut env = TestWebProxyEnv::default();
        let (result, promise) = RecordingPromise::<String>::create();
        manager.get_active_proxy_address(&mut env, promise);
        let error = result
            .lock()
            .unwrap()
            .clone()
            .expect("the promise settled")
            .unwrap_err();
        assert_eq!(error.message(), "Web proxy is not enabled");
    }

    /// The retry ladder doubles to the cap and the on_network half resets
    /// it (cpp:598-610, cpp:320-330).
    #[test]
    fn retry_ladder_and_network_reset() {
        let mut manager = WebProxyManager::default();
        let mut env = TestWebProxyEnv::default();
        manager.set_active_proxy(&mut env, 1, web_proxy("proxy.example.com"));
        let carrier_id = opened_carriers(&env)[0].id;

        manager.report_carrier_error(
            &mut env,
            carrier_id,
            Some(
                td_api::WebProxyCarrierError::webProxyCarrierErrorLoadFailed(
                    td_api::webProxyCarrierErrorLoadFailed {
                        message: "net down".to_string(),
                    },
                ),
            ),
            RecordingPromise::<()>::create().1,
        );
        assert_eq!(manager.next_deadline(), Some(env.now + MIN_RETRY_DELAY));

        // the network-down half: on_network(false) parks the reopen
        manager.on_network(&mut env, false);
        env.now += MIN_RETRY_DELAY + 0.1;
        manager.timeout_expired(&mut env);
        assert_eq!(opened_carriers(&env).len(), 1);
        manager.on_network(&mut env, true);
        assert_eq!(opened_carriers(&env).len(), 2);
        assert_eq!(
            manager.endpoints[&manager.active_endpoint_id].retry_delay,
            0.0
        );

        // the ladder from the on_network reset (retry_delay = 0):
        // 2 → 4 → 8 … capped at 30
        let mut expected_delay = 0.0;
        for _ in 0..6 {
            let carrier_id = manager.endpoints[&manager.active_endpoint_id].carrier_id;
            manager.report_carrier_error(
                &mut env,
                carrier_id,
                Some(
                    td_api::WebProxyCarrierError::webProxyCarrierErrorLoadFailed(
                        td_api::webProxyCarrierErrorLoadFailed {
                            message: String::new(),
                        },
                    ),
                ),
                RecordingPromise::<()>::create().1,
            );
            expected_delay = if expected_delay == 0.0 {
                MIN_RETRY_DELAY
            } else {
                (expected_delay * 2.0).min(MAX_RETRY_DELAY)
            };
            assert_eq!(manager.next_deadline(), Some(env.now + expected_delay));
            env.now += expected_delay + 0.1;
            manager.timeout_expired(&mut env);
        }
    }

    /// The handshake deadline fires the carrier (cpp:1024-1032).
    #[test]
    fn handshake_timeout_fails_carrier() {
        let mut manager = WebProxyManager::default();
        let mut env = TestWebProxyEnv::default();
        manager.set_active_proxy(&mut env, 1, web_proxy("proxy.example.com"));
        let carrier_id = opened_carriers(&env)[0].id;
        env.now += HANDSHAKE_TIMEOUT + 0.1;
        manager.timeout_expired(&mut env);
        assert_eq!(closed_carrier_ids(&env), vec![carrier_id]);
        assert_eq!(manager.next_deadline(), Some(env.now + MIN_RETRY_DELAY));
    }

    /// The MAX_HANDSHAKE_TIME clamp on the status extensions
    /// (cpp:706-714, cpp:1024-1032).
    #[test]
    fn handshake_extension_clamps_at_max() {
        let mut manager = WebProxyManager::default();
        let mut env = TestWebProxyEnv::default();
        manager.set_active_proxy(&mut env, 1, web_proxy("proxy.example.com"));
        let carrier_id = opened_carriers(&env)[0].id;
        env.now = MAX_HANDSHAKE_TIME - HANDSHAKE_TIMEOUT + 1.0;
        process_text(
            &mut manager,
            &mut env,
            carrier_id,
            "{\"t\":\"status\",\"state\":\"reconnecting\"}",
        );
        // max(10, min(36 + 10, opened_at + 45)) = max(10, 45) = 45
        assert_eq!(manager.next_deadline(), Some(MAX_HANDSHAKE_TIME));
    }

    /// The unanswered-response timeout fires a Ready carrier
    /// (cpp:1033-1036).
    #[test]
    fn unanswered_response_timeout_fails_carrier() {
        let (mut manager, mut env, carrier_id) = ready_carrier();
        manager.accept_connection(&mut env, manager.active_endpoint_id);
        let stream_key = manager.max_stream_key;
        // 0.0 is the "unset" sentinel, so the clock must be past the epoch
        // (the real Time::now always is)
        env.now = 100.0;
        manager.on_stream_data(&mut env, stream_key, b"x");
        assert_eq!(
            manager.endpoints[&manager.active_endpoint_id].unanswered_since,
            env.now
        );
        env.now += RESPONSE_TIMEOUT + 0.1;
        manager.timeout_expired(&mut env);
        assert_eq!(closed_carrier_ids(&env), vec![carrier_id]);
    }

    /// The output pre-flush at MAX_CARRIER_DATA_SIZE (cpp:962-964) — two
    /// over-cap frames in one batch flush the pending output first.
    #[test]
    fn oversized_output_preflushes() {
        let (mut manager, mut env, carrier_id) = ready_carrier();
        env.updates.clear();

        // a batch of pings whose pong output crosses MAX_CARRIER_DATA_SIZE
        // mid-batch: send_frame pre-flushes the pending output (cpp:962-964)
        // and the batch tail flushes the rest — two data updates
        let pongs_per_flush =
            MAX_CARRIER_DATA_SIZE / (FRAME_HEADER_SIZE + MAX_PING_PAYLOAD_SIZE / 2);
        let ping = frame(FRAME_PING, 0, &[b'p'; MAX_PING_PAYLOAD_SIZE / 2]);
        let mut batch = Vec::new();
        for _ in 0..(pongs_per_flush + 3) {
            batch.extend_from_slice(&ping);
        }
        assert!(matches!(
            process_binary(&mut manager, &mut env, carrier_id, &batch),
            Some(Ok(()))
        ));
        let data = data_updates(&env);
        assert_eq!(data.len(), 2);
        assert_eq!(data[0].0, carrier_id);
        assert_eq!(
            data[0].1.len(),
            pongs_per_flush * (FRAME_HEADER_SIZE + MAX_PING_PAYLOAD_SIZE / 2)
        );
        assert_eq!(
            data[1].1.len(),
            3 * (FRAME_HEADER_SIZE + MAX_PING_PAYLOAD_SIZE / 2)
        );
    }

    /// The hangup closes every open carrier (cpp:253-264).
    #[test]
    fn hangup_closes_all_carriers() {
        let mut manager = WebProxyManager::default();
        let mut env = TestWebProxyEnv::default();
        manager.set_active_proxy(&mut env, 1, web_proxy("a.example"));
        manager.set_active_proxy(&mut env, 2, web_proxy("b.example"));
        manager.destroy_endpoint(&mut env, 1);
        manager.hangup(&mut env);
        assert!(manager.endpoints.is_empty());
        assert_eq!(closed_carrier_ids(&env), vec![1, 2]);
        assert!(manager.next_deadline().is_none());
    }

    /// The stream-id allocator skips live and closed ids (cpp:503-511).
    #[test]
    fn stream_id_allocator_skips_taken_ids() {
        let (mut manager, mut env, _carrier_id) = ready_carrier();
        manager.accept_connection(&mut env, manager.active_endpoint_id);
        let stream_key = manager.max_stream_key;
        manager.on_stream_closed(&mut env, stream_key);
        // the fresh accept rides past the closed id 1
        manager.accept_connection(&mut env, manager.active_endpoint_id);
        assert_eq!(manager.streams[&manager.max_stream_key].stream_id, 2);
    }
}
