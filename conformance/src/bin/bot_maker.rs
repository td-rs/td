//! The S76 harness phone-account driver (the S66 `bot_maker`, ported to the
//! S76 codegen): drives the persisted S66 session (`TD_PEER_STATE`, the
//! harness phone account) to create a test-DC bot through @BotFather (once)
//! and to send messages to it (the walking-skeleton receive half).
//!
//! Usage:
//!   bot_maker <state-file> [--fetch]                          (print the bot token)
//!   bot_maker <state-file> --send <peer username> <text>      (send one message)
//!   bot_maker <state-file> --password-info                    (2FA state + hint)
//!   bot_maker <state-file> --wait-code                        (print LOGIN_CODE=<n>)
//!
//! Prints `BOT_TOKEN=<token>`/`BOT_USERNAME=<username>` (fetch), or
//! `SENT to <username>` (send). Bot tokens are reusable — unlike phone
//! codes — so the C++ recorder and the Rust replayer both log in with the
//! same token.
//!
//! The S75 user-path modes ride the account's own service chat (777000):
//! a tdlib phone login delivers its code to the active sessions as a
//! message from 777000, so `--wait-code` reads it over the persisted
//! session instead of asking the operator to retype it. Push-only by
//! design: the test DC does not resolve the service account's username
//! (`USERNAME_NOT_OCCUPIED`), so there is no access hash for history
//! reads — but pushes are real-time on a fresh session over the existing
//! auth key, which is exactly the freshness guarantee the caller needs
//! (start `--wait-code` BEFORE the login request fires; every code
//! message it then sees is that login's). `--password-info` calls
//! account.getPassword read-only (whether a cloud password gates the
//! login, and its hint).

use std::time::{Duration, Instant};

use tdmtproto::auth_data::AuthData;
use tdmtproto::message_id::MessageId;
use tdmtproto::proxy_secret::ProxySecret;
use tdmtproto::raw_connection::RawConnection;
use tdmtproto::session_connection::{SessionConnection, SessionConnectionCallback, SessionMode};
use tdmtproto::telegram_api;
use tdmtproto::transport_type::{TransportType, TransportTypeEnum};
use tdutils::buffer::BufferSlice;
use tdutils::random;
use tdutils::status::{Result, Status};
use tokio::net::TcpStream;

/// The Telegram test DCs (ip; the `p_q_inner_data_dc` field and the
/// obfuscation dc are 10000 + n, main keys).
const TEST_DCS: [&str; 3] = ["149.154.175.10", "149.154.167.40", "149.154.175.117"];

fn test_dc(dc: u8) -> (&'static str, i16) {
    let ip = TEST_DCS[(dc as usize - 1).clamp(0, 2)];
    (ip, (10000 + dc as i32) as i16)
}

/// Every API query rides in `invokeWithLayer{initConnection{...}}` — without
/// it the DC answers RPC_ERROR CONNECTION_NOT_INITED (the S66 finding).
fn wrap_query(api_id: i32, query: telegram_api::Function) -> Vec<u8> {
    let mut out = Vec::new();
    telegram_api::invokeWithLayer {
        layer: 229,
        query: Some(Box::new(telegram_api::Function::initConnection(
            telegram_api::initConnection {
                flags: 0,
                api_id,
                device_model: "td-rs".to_string(),
                system_version: "linux".to_string(),
                app_version: "0.1.0".to_string(),
                system_lang_code: "en".to_string(),
                lang_pack: String::new(),
                lang_code: "en".to_string(),
                proxy: Some(telegram_api::InputClientProxy {
                    address: String::new(),
                    port: 0,
                }),
                params: Some(Box::new(telegram_api::JSONValue::jsonNull(
                    telegram_api::jsonNull {},
                ))),
                query: Some(Box::new(query)),
            },
        ))),
    }
    .store_boxed(&mut out);
    out
}

// ---------------------------------------------------------------------------
// The persisted S66 state (the MTA2 layout of `mtproto_auth`).
// ---------------------------------------------------------------------------

const STATE_MAGIC: &[u8; 4] = b"MTA2";
struct SavedState {
    dc: u8,
    wall_anchor: f64,
    api_id: i32,
    _api_hash: String,
    blob: Vec<u8>,
}

fn load_state(path: &str) -> SavedState {
    fn take_str(data: &mut &[u8]) -> String {
        let len = u32::from_le_bytes(data[..4].try_into().unwrap()) as usize;
        let s = String::from_utf8_lossy(&data[4..4 + len]).into_owned();
        *data = &data[4 + len..];
        s
    }
    let data = std::fs::read(path).unwrap_or_else(|error| {
        eprintln!("failed to read {path}: {error}");
        std::process::exit(1);
    });
    let mut data = data.as_slice();
    if data.get(..4) != Some(STATE_MAGIC) {
        eprintln!("{path}: not an mtproto_auth state file");
        std::process::exit(1);
    }
    data = &data[4..];
    let dc = data[0];
    data = &data[1..];
    let wall_anchor = f64::from_le_bytes(data[..8].try_into().unwrap());
    data = &data[8..];
    let _phone = take_str(&mut data);
    let api_id = i32::from_le_bytes(data[..4].try_into().unwrap());
    data = &data[4..];
    let api_hash = take_str(&mut data);
    let _phone_code_hash = take_str(&mut data);
    let blob_len = u32::from_le_bytes(data[..4].try_into().unwrap()) as usize;
    let blob = data[4..4 + blob_len].to_vec();
    SavedState {
        dc,
        wall_anchor,
        api_id,
        _api_hash: api_hash,
        blob,
    }
}

// ---------------------------------------------------------------------------
// The driver.
// ---------------------------------------------------------------------------

/// Which answer the pending query awaits (the one-at-a-time driver sends a
/// query and derives the next action from its answer).
#[derive(Clone, Copy)]
enum Awaiting {
    Resolve,
    Send,
    History,
    PasswordInfo,
    Diff,
    State,
}

#[derive(Clone, Copy)]
enum Stage {
    SendNewBot,
    AwaitNamePrompt,
    SendName,
    AwaitUsernamePrompt,
    SendUsername,
    AwaitToken,
    /// `--fetch`: scan the BotFather history for a previously created
    /// bot's token (creation is rate-limited; reading is not).
    FetchToken,
}

struct BotCallback {
    api_id: i32,
    bot_name: String,
    bot_username: String,
    botfather_peer: Option<telegram_api::InputPeer>,
    botfather_user_id: i64,
    stage: Stage,
    attempts: usize,
    /// The newest BotFather reply id seen — only newer ids drive the FSM
    /// (old history entries must not re-trigger stages).
    last_botfather_msg_id: i32,
    /// The next time a history poll may go out (flood pacing).
    next_poll_at: Option<Instant>,
    last_botfather_text: String,
    token: Option<String>,
    done: bool,
    closed: bool,
    fatal: Option<String>,
    /// Set once the resolve answer acted (a resent query can be answered
    /// twice — the duplicate must not restart the flow).
    resolved: bool,
    /// `--send <username> <text>` mode: the resolved peer lands in
    /// `send_peer` and the message goes out.
    send_text: Option<String>,
    send_peer: Option<telegram_api::InputPeer>,
    /// message id → what its answer means (a resent query can be answered
    /// twice — the late duplicate is dropped by this routing).
    sent: std::collections::HashMap<u64, Awaiting>,
    awaiting: Option<Awaiting>,
    pending: Option<Vec<u8>>,
    /// The last-sent query — resend after a stale-salt rejection (the
    /// session layer installs the fresh salt before failing the query;
    /// the C++ NetQueryDispatcher retries it at the dispatch level).
    last_awaiting: Option<Awaiting>,
    last_query: Option<Vec<u8>>,
    salt_retries: usize,
    /// `--wait-code`: listen for the 777000 login-code push (push-only —
    /// the service account has no resolvable username on the test DC).
    wait_code: bool,
    login_code: Option<String>,
    /// `--wait-code` freshness: wall-clock start (unix) — only messages
    /// dated at/after it count, so codes from earlier login requests
    /// (sitting in the account backlog) are never picked up. The slack
    /// covers a code requested moments BEFORE the listener started (the
    /// login may already be sitting in WaitCode when the listener comes
    /// up) — Telegram invalidates older codes when a new one is issued,
    /// so the newest backlog code is always the live login's.
    started_at: i32,
    /// `--wait-code` catch-up: the pts the wait-side last held (None =
    /// nothing synced — diff from zero).
    last_pts: Option<i32>,
}

impl BotCallback {
    fn queue(&mut self, awaiting: Awaiting, query: telegram_api::Function) {
        let bytes = wrap_query(self.api_id, query);
        self.last_awaiting = Some(awaiting);
        self.last_query = Some(bytes.clone());
        self.awaiting = Some(awaiting);
        self.pending = Some(bytes);
    }

    fn resend_last(&mut self) {
        self.salt_retries += 1;
        if self.salt_retries > 3 {
            self.fatal = Some("bad server salt persists after retries".to_string());
            return;
        }
        self.awaiting = self.last_awaiting;
        self.pending = self.last_query.clone();
    }

    fn queue_send(&mut self, text: &str) {
        let mut random_id = 0i64;
        while random_id == 0 {
            random_id = random::secure_uint64() as i64;
        }
        let query =
            telegram_api::Function::messages_sendMessage(telegram_api::messages_sendMessage {
                flags: 0,
                no_webpage: false,
                silent: false,
                background: false,
                clear_draft: false,
                noforwards: false,
                update_stickersets_order: false,
                invert_media: false,
                allow_paid_floodskip: false,
                peer: Some(Box::new(
                    self.send_peer
                        .clone()
                        .or_else(|| self.botfather_peer.clone())
                        .expect("resolved peer"),
                )),
                reply_to: None,
                message: text.to_string(),
                random_id,
                reply_markup: None,
                entities: Vec::new(),
                schedule_date: 0,
                schedule_repeat_period: 0,
                send_as: None,
                quick_reply_shortcut: None,
                effect: 0,
                allow_paid_stars: 0,
                rich_message: None,
                suggested_post: None,
            });
        self.queue(Awaiting::Send, query);
    }

    fn queue_history(&mut self) {
        let query = telegram_api::Function::messages_getHistory(telegram_api::messages_getHistory {
            peer: Some(Box::new(self.botfather_peer.clone().expect("resolved peer"))),
            offset_id: 0,
            offset_date: 0,
            add_offset: 0,
            limit: if matches!(self.stage, Stage::FetchToken) {
                100
            } else {
                10
            },
            max_id: 0,
            min_id: 0,
            hash: 0,
        });
        self.queue(Awaiting::History, query);
    }

    /// A 777000 service-chat text arrived — as a real-time push or inside
    /// a getDifference backlog walk. The date guard is the freshness
    /// boundary: only messages from at/after the wait started count, so
    /// codes from earlier login requests never match. Returns true when
    /// it carried the code.
    fn on_code_text(&mut self, date: i32, text: &str) -> bool {
        if date + 900 < self.started_at {
            return false; // older than the wait (slack) — stale code
        }
        match extract_login_code(text) {
            Some(code) => {
                eprintln!("777000: login code message received (date {date})");
                self.login_code = Some(code);
                self.done = true;
                true
            }
            None => false,
        }
    }

    /// `--wait-code` catch-up: run updates.getDifference from the last
    /// held pts (zero when nothing synced yet) — a stale account makes
    /// the server answer pushes with updatesTooLong, and the messages
    /// (the code included) sit behind the difference.
    fn queue_diff(&mut self) {
        let pts = self.last_pts.unwrap_or(0);
        eprintln!("fetching the update difference from pts {pts}");
        self.queue(
            Awaiting::Diff,
            telegram_api::Function::updates_getDifference(
                telegram_api::updates_getDifference {
                    flags: 0,
                    pts,
                    pts_limit: 0,
                    pts_total_limit: 0,
                    date: 0,
                    qts: 0,
                    qts_limit: 0,
                },
            ),
        );
    }

    /// The FSM step after a fresh BotFather message arrives. Returns true
    /// when the message advanced the flow (a send was queued or the token
    /// landed).
    fn on_botfather_text(&mut self, text: &str) -> bool {
        if text == self.last_botfather_text {
            return true; // the push and the history poll see the same reply
        }
        self.last_botfather_text = text.to_string();
        self.attempts = 0;
        match self.stage {
            Stage::AwaitNamePrompt if text.contains("call it") => {
                eprintln!("botfather: name prompt received — sending {name}", name = self.bot_name);
                self.stage = Stage::SendName;
                self.queue_send(&self.bot_name.clone());
                true
            }
            Stage::AwaitUsernamePrompt if text.contains("username") => {
                eprintln!("botfather: username prompt received — sending {username}", username = self.bot_username);
                self.stage = Stage::SendUsername;
                self.queue_send(&self.bot_username.clone());
                true
            }
            Stage::AwaitUsernamePrompt | Stage::AwaitToken
                if text.contains("taken") || text.contains("invalid") =>
            {
                eprintln!("botfather: username rejected — trying a fresh one");
                self.bot_username = format!("tdrs76_{:x}bot", random::secure_uint64() & 0xfffff);
                self.stage = Stage::SendUsername;
                self.queue_send(&self.bot_username.clone());
                true
            }
            Stage::AwaitToken => match extract_token(text) {
                Some(token) => {
                    eprintln!("botfather: token received");
                    self.token = Some(token);
                    self.done = true;
                    true
                }
                None => false,
            },
            _ => false,
        }
    }

    /// Walks a getDifference page for the 777000 login-code message.
    fn scan_diff_messages(&mut self, messages: &[telegram_api::Message]) {
        for message in messages {
            let telegram_api::Message::message(m) = message else {
                continue;
            };
            if m.out || m.message.is_empty() {
                continue;
            }
            if self.on_code_text(m.date, &m.message) {
                return;
            }
        }
    }

    /// The poll stage: re-query the history until the prompt appears.
    fn poll(&mut self) {
        // `--wait-code` never polls (no resolved peer for the service
        // chat — push-only); the guard also keeps the idle-re-arm in
        // `on_update` from reaching `queue_history`'s peer expect.
        if self.wait_code {
            return;
        }
        if let Some(next) = self.next_poll_at {
            if Instant::now() < next {
                return; // paced — the main loop re-polls when due
            }
        }
        self.attempts += 1;
        if self.attempts > 150 {
            self.fatal = Some("BotFather flow stalled".to_string());
            return;
        }
        // Pacing: the test DC floods faster polling (FLOOD_WAIT_3 was
        // observed at ~5 polls/s); one history query per ~1.2 s.
        self.next_poll_at = Some(Instant::now() + Duration::from_millis(1200));
        self.queue_history();
    }

    /// True when no query is queued or in flight (the re-arm guard for the
    /// history poll — a push can consume a reply before the poll sees it).
    fn idle(&self) -> bool {
        self.pending.is_none() && self.awaiting.is_none() && !self.done
    }

    /// True when the stage awaits BotFather prompts and the next poll is
    /// due (the pacing lives here, so the main loop re-polls while idle).
    fn poll_due(&self) -> bool {
        if !matches!(
            self.stage,
            Stage::AwaitNamePrompt | Stage::AwaitUsernamePrompt | Stage::AwaitToken
        ) {
            return false;
        }
        if !self.idle() {
            return false;
        }
        match self.next_poll_at {
            Some(next) => Instant::now() >= next,
            None => true,
        }
    }
}

/// Scans a 777000 service message for the login code — "Login code:
/// 12345. Do not give this code to anyone…". The digit run right after
/// the marker words is the code (4–8 digits).
fn extract_login_code(text: &str) -> Option<String> {
    let (_, rest) = text.split_once("Login code")?;
    let digits: String = rest
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if (4..=8).contains(&digits.len()) {
        Some(digits)
    } else {
        None
    }
}

/// Scans a BotFather reply for the bot token (`123456789:AA...`). Walks
/// every digit run (usernames like `tdrs76_9507bot` carry digits too) and
/// matches the one followed by `:` and a long token part.
fn extract_token(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let mut j = i;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            j += 1;
        }
        // `j` sits after the digit run; a token needs `:` right here.
        if j >= bytes.len() || bytes[j] != b':' {
            i = j;
            continue;
        }
        let mut k = j + 1;
        while k < bytes.len()
            && (bytes[k].is_ascii_alphanumeric() || bytes[k] == b'_' || bytes[k] == b'-')
        {
            k += 1;
        }
        let part_len = k - (j + 1);
        if part_len >= 20 {
            return Some(text[i..k].to_string());
        }
        i = k;
    }
    None
}

impl SessionConnectionCallback for BotCallback {
    fn on_connected(&mut self) {
        // The `--wait-code` orchestrator waits for this line before it
        // fires the login: the code push only reaches sessions that are
        // up when the server sends it.
        if self.wait_code {
            eprintln!("bot_maker: session up — listening for the login-code push");
        }
    }

    fn on_closed(&mut self, status: Status) {
        eprintln!("session closed: {}", status.message());
        self.closed = true;
    }

    fn on_server_salt_updated(&mut self) {}

    fn on_server_time_difference_updated(&mut self, _force: bool) {}

    fn on_new_session_created(&mut self, _unique_id: u64, _first_message_id: MessageId) {}

    fn on_session_failed(&mut self, status: Status) {
        eprintln!("session failed: {}", status.message());
    }

    fn on_container_sent(
        &mut self,
        _container_message_id: MessageId,
        _message_ids: Vec<MessageId>,
    ) {
    }

    fn on_pong(&mut self, _ping_time: f64, _pong_time: f64, _current_time: f64) -> Status {
        Status::ok()
    }

    fn on_update(&mut self, packet: BufferSlice) -> Status {
        // BotFather replies can arrive as pushes; feed the plain-text
        // messages into the FSM (the history poll covers the rest and
        // dedups through the text guard).
        let data = packet.as_slice();
        if self.wait_code {
            // Diagnostics + the gap catch-up: a stale account (pending
            // updates) makes the server answer pushes with
            // updatesTooLong — the actual messages sit behind
            // updates.getDifference, which the wait mode runs from zero
            // (a fresh session tracks no pts of its own).
            let ctor =
                i32::from_le_bytes(data.get(..4).and_then(|b| b.try_into().ok()).unwrap_or([0; 4]));
            eprintln!("push ctor 0x{ctor:08x} ({} bytes)", data.len());
            if ctor == telegram_api::updatesTooLong::ID {
                self.queue_diff();
                return Status::ok();
            }
        }
        let Ok(updates) = telegram_api::Updates::parse(data) else {
            return Status::ok();
        };
        let mut texts: Vec<String> = Vec::new();
        // `--wait-code`: the 777000 code message arrives as a push.
        let mut code_texts: Vec<(i32, String)> = Vec::new();
        match updates {
            telegram_api::Updates::updateShortMessage(m) => {
                if self.wait_code && (m.user_id == 777000 || m.user_id == 0) {
                    code_texts.push((m.date, m.message));
                } else if m.user_id == self.botfather_user_id || m.user_id == 0 {
                    texts.push(m.message);
                }
            }
            telegram_api::Updates::updateShortChatMessage(m) => {
                if m.chat_id == self.botfather_user_id || m.chat_id == 0 {
                    texts.push(m.message);
                }
            }
            telegram_api::Updates::updates(m) => {
                for update in m.updates {
                    let telegram_api::Update::updateNewMessage(u) = update else {
                        continue;
                    };
                    let Some(message) = u.message else {
                        continue;
                    };
                    let telegram_api::Message::message(m) = message else {
                        continue;
                    };
                    if !m.out && !m.message.is_empty() {
                        if self.wait_code {
                            code_texts.push((m.date, m.message));
                        } else {
                            texts.push(m.message);
                        }
                    }
                }
            }
            telegram_api::Updates::updatesCombined(m) => {
                for update in m.updates {
                    let telegram_api::Update::updateNewMessage(u) = update else {
                        continue;
                    };
                    let Some(message) = u.message else {
                        continue;
                    };
                    let telegram_api::Message::message(m) = message else {
                        continue;
                    };
                    if !m.out && !m.message.is_empty() {
                        if self.wait_code {
                            code_texts.push((m.date, m.message));
                        } else {
                            texts.push(m.message);
                        }
                    }
                }
            }
            _ => {}
        }
        for (date, text) in code_texts {
            self.on_code_text(date, &text);
        }
        for text in texts {
            self.on_botfather_text(&text);
        }
        if self.idle() {
            self.poll();
        }
        Status::ok()
    }

    fn on_message_ack(&mut self, _message_id: MessageId) {}

    fn on_message_result_ok(
        &mut self,
        message_id: MessageId,
        packet: BufferSlice,
        _original_size: usize,
    ) -> Status {
        let data = packet.as_slice();
        let Some(_ctor) = data.get(..4) else {
            return Status::ok();
        };
        let awaiting = match self.sent.remove(&message_id.get()) {
            Some(awaiting) => awaiting,
            None => {
                eprintln!("late/duplicate answer for message {} — dropped", message_id.get());
                return Status::ok();
            }
        };
        match awaiting {
            Awaiting::Resolve => match telegram_api::contacts_ResolvedPeer::parse(data) {
                Ok(resolved) => {
                    let Some(telegram_api::Peer::peerUser(p)) = resolved.peer else {
                        self.fatal = Some("the peer is not a user".to_string());
                        return Status::ok();
                    };
                    // The `peerUser` carries no access hash; take it from
                    // the resolved users vector (the full user object).
                    let mut access_hash = 0i64;
                    for user in &resolved.users {
                        if let telegram_api::User::user(u) = user {
                            if u.id == p.user_id {
                                access_hash = u.access_hash;
                                break;
                            }
                        }
                    }
                    self.botfather_user_id = p.user_id;
                    self.botfather_peer = Some(telegram_api::InputPeer::inputPeerUser(
                        telegram_api::inputPeerUser {
                            user_id: p.user_id,
                            access_hash,
                        },
                    ));
                    eprintln!("peer resolved: user_id={}", p.user_id);
                    if self.resolved {
                        eprintln!("duplicate resolve — dropped");
                        return Status::ok();
                    }
                    self.resolved = true;
                    if let Some(text) = self.send_text.clone() {
                        // `--send` mode: the message goes straight out.
                        self.send_peer = self.botfather_peer.clone();
                        self.queue_send(&text);
                        return Status::ok();
                    }
                    match self.stage {
                        Stage::FetchToken => {
                            eprintln!("fetch mode: scanning the BotFather history for a token");
                            self.queue_history();
                        }
                        _ => {
                            self.stage = Stage::SendNewBot;
                            self.queue_send("/newbot");
                            eprintln!("sent: /newbot");
                        }
                    }
                }
                Err(error) => {
                    self.fatal = Some(format!("contacts.resolvedPeer parse: {error}"));
                }
            },
            Awaiting::Send => match telegram_api::Updates::parse(data) {
                Ok(_) => {
                    if self.send_text.is_some() {
                        // `--send` mode: the message is out — done.
                        eprintln!("message sent");
                        self.done = true;
                        return Status::ok();
                    }
                    self.stage = match self.stage {
                        Stage::SendNewBot => Stage::AwaitNamePrompt,
                        Stage::SendName => Stage::AwaitUsernamePrompt,
                        Stage::SendUsername => Stage::AwaitToken,
                        _ => self.stage,
                    };
                    self.queue_history();
                }
                Err(error) => {
                    let ctor = data
                        .get(..4)
                        .map(|b| i32::from_le_bytes(b.try_into().unwrap()))
                        .unwrap_or(0);
                    self.fatal = Some(format!(
                        "sendMessage answer parse: {error} (ctor 0x{ctor:08x}, {} bytes)",
                        data.len()
                    ));
                }
            },
            Awaiting::History => match telegram_api::messages_Messages::parse(data) {
                Ok(messages) => {
                    let list = match messages {
                        telegram_api::messages_Messages::messages_messages(m) => m.messages,
                        telegram_api::messages_Messages::messages_channelMessages(m) => m.messages,
                        telegram_api::messages_Messages::messages_messagesSlice(m) => m.messages,
                        telegram_api::messages_Messages::messages_messagesNotModified(_) => {
                            Vec::new()
                        }
                    };
                    if matches!(self.stage, Stage::FetchToken) {
                        // Scan the whole page for a token (the creation is
                        // rate-limited; earlier creations left their token
                        // messages in this chat).
                        for message in list {
                            let telegram_api::Message::message(m) = message else {
                                continue;
                            };
                            if m.out || m.message.is_empty() {
                                continue;
                            }
                            // The congratulations message carries the bot
                            // username: "You will find it at t.me/<name>."
                            if let Some(rest) = m.message.split("t.me/").nth(1) {
                                let name: String = rest
                                    .chars()
                                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                                    .collect();
                                if !name.is_empty() {
                                    self.bot_username = name;
                                }
                            }
                            if let Some(token) = extract_token(&m.message) {
                                eprintln!("botfather: token received");
                                self.token = Some(token);
                                self.done = true;
                                break;
                            }
                        }
                        if !self.done {
                            self.fatal =
                                Some("no bot token found in the BotFather history".to_string());
                        }
                        return Status::ok();
                    }
                    let mut fresh = None;
                    for message in list {
                        let telegram_api::Message::message(m) = message else {
                            continue;
                        };
                        if m.out {
                            continue; // my own sent messages
                        }
                        if m.message.is_empty() {
                            continue;
                        }
                        if m.id <= self.last_botfather_msg_id {
                            continue; // already processed
                        }
                        self.last_botfather_msg_id = m.id;
                        eprintln!(
                            "botfather: id={} flags={:x} text={:?}",
                            m.id,
                            m.flags,
                            m.message.chars().take(80).collect::<String>()
                        );
                        fresh = Some(m.message);
                        break;
                    }
                    match fresh {
                        Some(text) => {
                            self.on_botfather_text(&text);
                            if self.idle() {
                                self.poll();
                            }
                        }
                        None => self.poll(),
                    }
                }
                Err(error) => {
                    self.fatal = Some(format!("messages.Messages parse: {error}"));
                }
            },
            Awaiting::PasswordInfo => match telegram_api::account_Password::parse(data) {
                Ok(password) => {
                    println!("HAS_PASSWORD={}", password.has_password);
                    println!("HINT={}", password.hint);
                    self.done = true;
                }
                Err(error) => {
                    self.fatal = Some(format!("account.Password parse: {error}"));
                }
            },
            Awaiting::State => match telegram_api::updates_State::parse(data) {
                Ok(state) => {
                    self.last_pts = Some(state.pts);
                    eprintln!(
                        "state: pts={} qts={} seq={} — in the update flow, listening",
                        state.pts, state.qts, state.seq
                    );
                }
                Err(error) => {
                    self.fatal = Some(format!("updates.State parse: {error}"));
                }
            },
            Awaiting::Diff => match telegram_api::updates_Difference::parse(data) {
                Ok(difference) => match difference {
                    telegram_api::updates_Difference::updates_differenceEmpty(_) => {
                        // Caught up — live pushes carry the code from here.
                        eprintln!("difference empty — caught up, listening");
                    }
                    telegram_api::updates_Difference::updates_difference(m) => {
                        self.scan_diff_messages(&m.new_messages);
                        if let Some(state) = &m.state {
                            self.last_pts = Some(state.pts);
                        }
                        if !self.done {
                            eprintln!("difference complete — listening");
                        }
                    }
                    telegram_api::updates_Difference::updates_differenceSlice(m) => {
                        self.scan_diff_messages(&m.new_messages);
                        if let Some(state) = &m.intermediate_state {
                            self.last_pts = Some(state.pts);
                        }
                        if !self.done {
                            // More backlog: keep walking from the
                            // intermediate state's pts.
                            self.queue_diff();
                        }
                    }
                    telegram_api::updates_Difference::updates_differenceTooLong(m) => {
                        // The gap exceeds the limit — jump to the current
                        // pts (old history is lost, fresh messages are
                        // what the wait needs anyway).
                        self.last_pts = Some(m.pts);
                        if !self.done {
                            self.queue_diff();
                        }
                    }
                },
                Err(error) => {
                    self.fatal = Some(format!("updates.Difference parse: {error}"));
                }
            },
        }
        Status::ok()
    }

    fn on_message_result_error(&mut self, _message_id: MessageId, code: i32, message: String) {
        eprintln!("RPC error {code}: {message}");
        if message == "Bad server salt" {
            eprintln!("stale salt — resending with the fresh salt");
            self.resend_last();
            return;
        }
        if let Some(seconds) = message
            .strip_prefix("FLOOD_WAIT_")
            .and_then(|s| s.parse::<u64>().ok())
        {
            eprintln!("flood wait — sleeping {seconds}s before the retry");
            std::thread::sleep(std::time::Duration::from_secs(seconds + 1));
            self.resend_last();
            return;
        }
        self.fatal = Some(format!("RPC error {code}: {message}"));
    }

    fn on_message_failed(&mut self, message_id: MessageId, status: Status) {
        eprintln!("message failed: {}", status.message());
        self.sent.remove(&message_id.get());
        if status.message() == "Bad server salt" {
            eprintln!("stale salt — resending with the fresh salt");
            self.resend_last();
            return;
        }
        self.fatal = Some(format!("message failed: {}", status.message()));
    }

    fn on_message_info(
        &mut self,
        _message_id: MessageId,
        _state: i32,
        _answer_message_id: MessageId,
        _answer_size: i32,
        _source: i32,
    ) {
    }

    fn on_destroy_auth_key(&mut self) -> Status {
        Status::ok()
    }
}

/// Wall-clock epoch seconds minus `time::now()` (the process-anchored
/// monotonic clock) — the re-basing anchor for persisted time diffs.
fn wall_minus_monotonic() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
        - tdutils::time::now()
}

fn main() {
    let mut args: Vec<String> = std::env::args().collect();
    let mut bot_name = "td-rs S76".to_string();
    let mut bot_username = format!("tdrs76_{:x}bot", random::secure_uint64() & 0xffff);
    if let Some(pos) = args.iter().position(|a| a == "--name") {
        if args.len() > pos + 1 {
            bot_name = args[pos + 1].clone();
            args.drain(pos..=pos + 1);
        }
    }
    if let Some(pos) = args.iter().position(|a| a == "--username") {
        if args.len() > pos + 1 {
            bot_username = args[pos + 1].clone();
            args.drain(pos..=pos + 1);
        }
    }
    let fetch = if let Some(pos) = args.iter().position(|a| a == "--fetch") {
        args.remove(pos);
        true
    } else {
        false
    };
    let send = if let Some(pos) = args.iter().position(|a| a == "--send") {
        let username = args.get(pos + 1).cloned().unwrap_or_default();
        let text = args.get(pos + 2).cloned().unwrap_or_default();
        args.drain(pos..=pos + 2);
        Some((username, text))
    } else {
        None
    };
    let password_info = if let Some(pos) = args.iter().position(|a| a == "--password-info") {
        args.remove(pos);
        true
    } else {
        false
    };
    let wait_code = if let Some(pos) = args.iter().position(|a| a == "--wait-code") {
        args.remove(pos);
        true
    } else {
        false
    };
    let wait_budget = if let Some(pos) = args.iter().position(|a| a == "--budget") {
        let budget = args
            .get(pos + 1)
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(300);
        args.drain(pos..=pos + 1);
        budget
    } else {
        300
    };
    let modes = fetch as u8 + password_info as u8 + wait_code as u8 + send.is_some() as u8;
    if args.len() != 2 || modes != 1 {
        eprintln!(
            "usage: bot_maker <state-file> [--name <bot name>] [--username <bot username>] [--fetch] [--send <peer username> <text>] [--password-info] [--wait-code [--budget <secs>]]"
        );
        std::process::exit(2);
    }
    let state_path = &args[1];

    let saved = load_state(state_path);
    let (key, _handshake, salt, time_diff, _session_id) =
        match tdmtproto::persistence::load_auth_data(&saved.blob) {
            Result::Ok(loaded) => loaded,
            Result::Err(status) => {
                eprintln!("failed to load the auth state: {}", status.message());
                std::process::exit(1);
            }
        };
    eprintln!("session key reloaded: id={:016x}", key.id());

    let mut auth_data = AuthData::new();
    auth_data.set_use_pfs(false);
    auth_data.set_main_auth_key(key);
    auth_data.set_server_salt(salt.salt, tdutils::time::now());
    let time_diff = time_diff + wall_minus_monotonic() - saved.wall_anchor;
    auth_data.reset_server_time_difference(time_diff);
    let mut session_id = 0u64;
    while session_id == 0 {
        session_id = random::secure_uint64();
    }
    auth_data.set_session_id(session_id);
    auth_data.set_auth_flag(true);

    let (ip, tx_dc) = test_dc(saved.dc);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async move {
        let stream = TcpStream::connect((ip, 443)).await.unwrap();
        let raw_connection = RawConnection::new(
            stream,
            TransportType::new(TransportTypeEnum::Tcp, tx_dc, ProxySecret::default()),
            None,
        );
        let mut connection = SessionConnection::new(SessionMode::Tcp, raw_connection, auth_data);
        connection.set_online(true, true);

        let mut callback = BotCallback {
            api_id: saved.api_id,
            bot_name: bot_name.clone(),
            bot_username: bot_username.clone(),
            botfather_peer: None,
            botfather_user_id: 0,
            stage: if fetch {
                Stage::FetchToken
            } else {
                Stage::SendNewBot
            },
            attempts: 0,
            last_botfather_msg_id: 0,
            next_poll_at: None,
            last_botfather_text: String::new(),
            token: None,
            done: false,
            closed: false,
            fatal: None,
            awaiting: None,
            pending: None,
            last_awaiting: None,
            last_query: None,
            salt_retries: 0,
            resolved: false,
            send_text: send.as_ref().map(|(_, text)| text.clone()),
            send_peer: None,
            sent: std::collections::HashMap::new(),
            wait_code,
            login_code: None,
            started_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i32)
                .unwrap_or(0),
            last_pts: None,
        };
        if password_info {
            // No peer needed — the 2FA state is account-wide.
            callback.queue(
                Awaiting::PasswordInfo,
                telegram_api::Function::account_getPassword(telegram_api::account_getPassword {}),
            );
        } else if wait_code {
            // The S66 finding: a session that never queried the update
            // state receives NO pushes — updates.getState puts this
            // session into the account's update distribution, and the
            // returned pts anchors any later updatesTooLong catch-up.
            // No request otherwise: the wait listens for the 777000 code.
            callback.queue(
                Awaiting::State,
                telegram_api::Function::updates_getState(telegram_api::updates_getState {}),
            );
        } else {
            callback.queue(
                Awaiting::Resolve,
                telegram_api::Function::contacts_resolveUsername(
                    telegram_api::contacts_resolveUsername {
                        flags: 0,
                        username: send
                            .as_ref()
                            .map(|(username, _)| username.clone())
                            .unwrap_or_else(|| "botfather".to_string()),
                        referer: String::new(),
                    },
                ),
            );
        }

        let budget =
            tokio::time::Instant::now() + Duration::from_secs(if wait_code { wait_budget } else { 240 });
        loop {
            let _ = connection.flush(&mut callback).await;
            if let Some(query) = callback.pending.take() {
                let awaiting = callback.awaiting.take().expect("awaiting with a query");
                let message_id = connection.send_query(
                    BufferSlice::from_slice_bytes(&query),
                    false,
                    MessageId::default(),
                    Vec::new(),
                    false,
                );
                callback.sent.insert(message_id.get(), awaiting);
                let _ = connection.flush(&mut callback).await;
            }
            if let Some(fatal) = callback.fatal.take() {
                eprintln!("bot_maker: FAILED: {fatal}");
                std::process::exit(1);
            }
            if callback.done {
                if let Some((username, _text)) = send.as_ref() {
                    println!("SENT to {username}");
                    return;
                }
                if wait_code {
                    if let Some(code) = callback.login_code.clone() {
                        println!("LOGIN_CODE={code}");
                    } else {
                        eprintln!("bot_maker: no login-code push arrived");
                        std::process::exit(3);
                    }
                    return;
                }
                if password_info {
                    return; // printed by the answer handler
                }
                let token = callback.token.expect("token");
                println!("BOT_USERNAME={}", callback.bot_username);
                println!("BOT_TOKEN={token}");
                return;
            }
            if callback.closed {
                eprintln!("bot_maker: session closed before the token arrived");
                std::process::exit(1);
            }
            if callback.poll_due() {
                callback.poll();
            }
            if tokio::time::Instant::now() >= budget {
                eprintln!("bot_maker: flow exceeded the budget");
                std::process::exit(2);
            }
            tokio::select! {
                _ = connection.raw_connection().stream().readable() => {}
                _ = tokio::time::sleep(Duration::from_millis(300)) => {}
            }
        }
    });
}
