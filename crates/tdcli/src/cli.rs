//! M.14 — the `CliClient` (`td/telegram/cli.cpp` `class CliClient`).
//!
//! M.14.1: stdin line pump, command queue, the `on_cmd` dispatch frame (the
//! 19 sequential op-blocks with the C++ unknown-command counting), quit/close
//! lifecycle, and the `instance_` equivalent.
//! M.14.2: the client plane — `create_td`/`init_td` over the S70
//! `ClientManager` (the transport the C++ CLI drives through `ClientActor`),
//! `send_request` with the static query counter, the `on_result`/`on_error`
//! receive machinery (generation tracking, the `td_requests`-gated
//! `Receive result`/`Receive error` prints in the C++ `TlStorerToString`
//! debug format), the update registrations, and the file-generation pump.
//!
//! Threading: the C++ actor-on-scheduler collapses to two OS threads — the
//! pump thread (the epoll stdin reader) and the main loop thread (the
//! actor); observable behavior (tick-drain order, EOF semantics) is ported,
//! not the epoll mechanics. Responses are drained from the manager's output
//! queue on every loop tick (the C++ scheduler delivers them as actor
//! events; the 100 ms tick is the same run_main granularity as `td::main`).

use crate::tokenizer;
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use tdclient::client::{ClientManager, Response, get_manager_singleton};
use tdclient::td_api::{
    self, AuthorizationState, Function, MessageTopic, NetworkType, Object, OptionValue,
    SearchMessagesFilter,
};
use tdutils::logging;
use tdutils::{random, time};

/// `cli.cpp:908` `const char *prompt = "td_cli> "` — printed on a tty only
/// (readline shows the prompt via `rl_set_prompt`; without readline nothing
/// is printed).
const PROMPT: &str = "td_cli> ";

/// `struct CliClient::User` (cli.cpp:248) — the registry entry the command
/// arms print through `print_user`.
struct User {
    first_name: String,
    last_name: String,
    username: String,
}

/// `struct CliClient::FileGeneration` (cli.cpp:399).
struct FileGeneration {
    id: i64,
    destination: String,
    source: String,
    part_size: i64,
    local_size: i64,
    size: i64,
    test_local_size_decrease: bool,
}

pub struct CliClient {
    /// `std::queue<string> cmd_queue_`.
    cmd_queue: Mutex<VecDeque<String>>,
    /// The scheduler tick: the pump wakes the loop when a line arrives.
    condvar: Condvar,
    /// `bool close_flag_`.
    close_flag: AtomicBool,
    /// `bool ready_to_stop_`.
    ready_to_stop: AtomicBool,
    /// `bool is_stdin_reader_stopped_`.
    stdin_reader_stopped: AtomicBool,
    /// stdin(0) is a terminal — gates the prompt and the readline Ctrl-D
    /// "Closed" path.
    tty: bool,

    // ---- M.14.2: the client plane --------------------------------------
    /// The process-wide `ClientManager` singleton (the S70 layer the C++
    /// `ConcurrentScheduler` + `ClientActor` pair collapses to).
    manager: &'static ClientManager,
    /// `ActorOwn<ClientActor> td_client_` — the live client id, if any.
    td_client: Mutex<Option<i32>>,
    /// client id → the generation it was created under (`TdCallbackImpl`'s
    /// captured generation).
    generations: Mutex<HashMap<i32, u64>>,
    /// `uint64 generation_`.
    generation: AtomicU64,
    /// `uint64 closed_td_`.
    closed_td: AtomicU64,
    /// `static uint64 query_num = 1` in `send_request` (process-wide).
    query_num: AtomicU64,

    // ---- the main() flags (set through argv in M.14.6) ------------------
    use_test_dc: AtomicBool,
    get_chat_list: AtomicBool,
    disable_network: AtomicBool,
    api_id: AtomicI32,
    api_hash: Mutex<String>,

    // ---- the update registrations (cli.cpp fields) ----------------------
    /// `int64 my_id_`.
    #[allow(dead_code)] // readers land with the M.14.3 command arms
    my_id: AtomicI64,
    /// `FlatHashMap<int64, unique_ptr<User>> users_`.
    users: Mutex<HashMap<i64, User>>,
    /// `FlatHashMap<string, int64> username_to_user_id_`.
    #[allow(dead_code)] // readers land with the M.14.3 as_user coercer
    username_to_user_id: Mutex<HashMap<String, i64>>,
    /// `FlatHashMap<string, int64> username_to_supergroup_id_`.
    #[allow(dead_code)] // readers land with the M.14.4 as_supergroup_id
    username_to_supergroup_id: Mutex<HashMap<String, i64>>,
    /// `FlatHashSet<int64> channel_ids_`.
    #[allow(dead_code)] // readers land with the M.14.4 coercers
    channel_ids: Mutex<std::collections::BTreeSet<i64>>,
    /// `vector<string> authentication_tokens_`.
    #[allow(dead_code)] // readers land with the M.14.3 auth arms
    authentication_tokens: Mutex<Vec<String>>,
    /// `td_api::object_ptr<td_api::AuthorizationState> authorization_state_`.
    authorization_state: Mutex<Option<AuthorizationState>>,

    // ---- the replay/pump state (cli.cpp fields) -------------------------
    /// `int64 get_history_chat_id_`.
    get_history_chat_id: AtomicI64,
    /// `int64 search_chat_id_`.
    search_chat_id: AtomicI64,
    /// `int64 get_log_chat_id_`.
    get_log_chat_id: AtomicI64,
    /// `FlatHashMap<int32, double> being_downloaded_files_` (start times).
    being_downloaded_files: Mutex<HashMap<i32, f64>>,
    /// `vector<FileGeneration> pending_file_generations_`.
    pending_file_generations: Mutex<Vec<FileGeneration>>,
    /// `int64 saved_messages_topic_id_` (cli.cpp:9165).
    #[allow(dead_code)] // writers land with the M.14.4 commands
    saved_messages_topic_id: AtomicI64,
    /// `int64 direct_messages_chat_topic_id_`.
    #[allow(dead_code)] // writers land with the M.14.4 commands
    direct_messages_chat_topic_id: AtomicI64,
    /// `int32 forum_topic_id_`.
    #[allow(dead_code)] // writers land with the M.14.4 commands
    forum_topic_id: AtomicI32,
    /// `int64 message_thread_id_`.
    #[allow(dead_code)] // writers land with the M.14.4 commands
    message_thread_id: AtomicI64,
    /// `int64 opened_chat_id_` — the `.` target of the id coercers (writers
    /// land with the M.14.4 open-chat commands).
    opened_chat_id: AtomicI64,
}

/// `CliClient *CliClient::instance_` — the process-wide instance
/// (`quit_instance` target; the readline callbacks used it in C++).
static INSTANCE: OnceLock<CliClient> = OnceLock::new();

/// `CliClient::quit_instance`.
#[allow(dead_code)] // the C++ free `quit()` has no live caller either (the
// readline signal path used it); kept for parity
pub fn quit_instance() {
    instance().quit();
}

/// `CliClient::init`'s `instance_ = this`.
pub fn instance() -> &'static CliClient {
    INSTANCE.get_or_init(|| CliClient {
        cmd_queue: Mutex::new(VecDeque::new()),
        condvar: Condvar::new(),
        close_flag: AtomicBool::new(false),
        ready_to_stop: AtomicBool::new(false),
        stdin_reader_stopped: AtomicBool::new(false),
        tty: unsafe { libc::isatty(0) } == 1,
        manager: get_manager_singleton(),
        td_client: Mutex::new(None),
        generations: Mutex::new(HashMap::new()),
        generation: AtomicU64::new(0),
        closed_td: AtomicU64::new(0),
        query_num: AtomicU64::new(0),
        use_test_dc: AtomicBool::new(false),
        get_chat_list: AtomicBool::new(false),
        disable_network: AtomicBool::new(false),
        api_id: AtomicI32::new(0),
        api_hash: Mutex::new(String::new()),
        my_id: AtomicI64::new(0),
        users: Mutex::new(HashMap::new()),
        username_to_user_id: Mutex::new(HashMap::new()),
        username_to_supergroup_id: Mutex::new(HashMap::new()),
        channel_ids: Mutex::new(std::collections::BTreeSet::new()),
        authentication_tokens: Mutex::new(Vec::new()),
        authorization_state: Mutex::new(None),
        get_history_chat_id: AtomicI64::new(0),
        search_chat_id: AtomicI64::new(0),
        get_log_chat_id: AtomicI64::new(0),
        being_downloaded_files: Mutex::new(HashMap::new()),
        pending_file_generations: Mutex::new(Vec::new()),
        saved_messages_topic_id: AtomicI64::new(0),
        direct_messages_chat_topic_id: AtomicI64::new(0),
        forum_topic_id: AtomicI32::new(0),
        message_thread_id: AtomicI64::new(0),
        opened_chat_id: AtomicI64::new(0),
    })
}

/// `td::to_lower` — the per-byte ASCII lowercase (usernames are ASCII; the
/// C++ helper never touches non-ASCII bytes either).
fn to_lower(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_uppercase() { b + 32 } else { b })
        .map(|b| b as char)
        .collect()
}

/// `td::oneline` — `\n` and `\r` become spaces.
fn oneline(s: &str) -> String {
    s.chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect()
}

/// `td::is_space` (the `td::trim` set): space, tab, LF, CR, FF, VT.
fn is_td_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{c}' | '\u{b}')
}

/// `td::trim` (ASCII whitespace only — `str::trim` would also strip Unicode).
fn trim_td(s: &str) -> &str {
    s.trim_matches(is_td_space)
}

/// `CliClient::get_delimiter` (cli.cpp:482): the single non-word character
/// of the argument list, or a space (each word then stands alone).
fn get_delimiter(s: &str) -> char {
    let mut chars = std::collections::BTreeSet::<char>::new();
    for c in trim_td(s).chars() {
        if !c.is_ascii_alphanumeric()
            && !matches!(c, '_' | '-' | '@' | '.' | '/' | '\0' | '$')
            && (c as u32) <= 127
        {
            chars.insert(c);
        }
    }
    match chars.len() {
        0 => ' ',
        1 => *chars.iter().next().unwrap(),
        _ => {
            tdutils::log_error!("Failed to determine delimiter in \"{}\"", s);
            ' '
        }
    }
}

/// `CliClient::autosplit` (cli.cpp:500).
fn autosplit(s: &str) -> Vec<String> {
    tokenizer::full_split(trim_td(s), get_delimiter(s))
}

/// `CliClient::as_email_address_authentication` (cli.cpp:2741).
fn as_email_address_authentication(arg: &str) -> Option<td_api::EmailAddressAuthentication> {
    if let Some(rest) = arg.strip_prefix("a ") {
        return Some(
            td_api::EmailAddressAuthentication::emailAddressAuthenticationAppleId(
                td_api::emailAddressAuthenticationAppleId {
                    token: rest.to_owned(),
                },
            ),
        );
    }
    if let Some(rest) = arg.strip_prefix("g ") {
        return Some(
            td_api::EmailAddressAuthentication::emailAddressAuthenticationGoogleId(
                td_api::emailAddressAuthenticationGoogleId {
                    token: rest.to_owned(),
                },
            ),
        );
    }
    if !arg.is_empty() {
        return Some(
            td_api::EmailAddressAuthentication::emailAddressAuthenticationCode(
                td_api::emailAddressAuthenticationCode {
                    code: arg.to_owned(),
                },
            ),
        );
    }
    None
}

/// `CliClient::as_passport_element_type` (cli.cpp:2752) — the unknown spell
/// falls back to `passportElementTypePassport`, exactly like the C++.
fn as_passport_element_type(t: &str) -> td_api::PassportElementType {
    use td_api::PassportElementType as Pe;
    if t == "address" || t == "a" {
        return Pe::passportElementTypeAddress(td_api::passportElementTypeAddress);
    }
    if t == "email" || t == "e" {
        return Pe::passportElementTypeEmailAddress(td_api::passportElementTypeEmailAddress);
    }
    if t == "phone" || t == "p" {
        return Pe::passportElementTypePhoneNumber(td_api::passportElementTypePhoneNumber);
    }
    if t == "pd" {
        return Pe::passportElementTypePersonalDetails(td_api::passportElementTypePersonalDetails);
    }
    if t == "dl" {
        return Pe::passportElementTypeDriverLicense(td_api::passportElementTypeDriverLicense);
    }
    if t == "ip" {
        return Pe::passportElementTypeInternalPassport(
            td_api::passportElementTypeInternalPassport,
        );
    }
    if t == "ic" {
        return Pe::passportElementTypeIdentityCard(td_api::passportElementTypeIdentityCard);
    }
    if t == "ra" {
        return Pe::passportElementTypeRentalAgreement(td_api::passportElementTypeRentalAgreement);
    }
    if t == "pr" {
        return Pe::passportElementTypePassportRegistration(
            td_api::passportElementTypePassportRegistration,
        );
    }
    if t == "tr" {
        return Pe::passportElementTypeTemporaryRegistration(
            td_api::passportElementTypeTemporaryRegistration,
        );
    }
    Pe::passportElementTypePassport(td_api::passportElementTypePassport)
}

/// `CliClient::as_passport_element_types` (cli.cpp:2786).
fn as_passport_element_types(types: &str) -> Vec<td_api::PassportElementType> {
    autosplit(types)
        .iter()
        .map(|t| as_passport_element_type(t))
        .collect()
}

/// The `HttpReader::parse_args` URL decoding: `%XX` becomes the byte,
/// everything else (including `+`) passes through.
fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() + 1 && i + 2 < bytes.len() + 1 {
            let hex = bytes.get(i + 1..i + 3);
            if let Some(hex) = hex
                && let Ok(byte) = u8::from_str_radix(std::str::from_utf8(hex).unwrap_or(""), 16)
            {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `td::base64url_decode(...).move_as_ok()` — the URL-safe alphabet, padded
/// or unpadded (the `base64` crate wants the choice up front; the C++
/// decoder accepts both shapes at once).
fn base64url_decode(s: &str) -> Vec<u8> {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE
        .decode(s)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s))
        .unwrap_or_else(|error| panic!("base64url_decode failed for {:?}: {}", s, error))
}

impl CliClient {
    /// `CliClient::quit` (cli.cpp:2041): idempotent via `close_flag_`, logs
    /// `QUIT`, drops the client, unsubscribes stdin.
    fn quit(&self) {
        if self.close_flag.swap(true, Ordering::SeqCst) {
            return;
        }
        tdutils::log_warning!("QUIT");
        // `td_client_.reset()` — the actor drop; the close notification
        // arrives through the response queue and drives `on_closed`.
        if let Some(client_id) = self.td_client.lock().unwrap().take() {
            self.manager.close(client_id);
        }
        // Scheduler::unsubscribe(stdin_) →
        self.stdin_reader_stopped.store(true, Ordering::SeqCst);
        self.condvar.notify_all();
    }

    /// `CliClient::on_closed` (cli.cpp:2031).
    fn on_closed(&self, generation: u64) {
        tdutils::log_warning!("Td with generation {} is closed", generation);
        self.closed_td.fetch_add(1, Ordering::SeqCst);
        if self.closed_td.load(Ordering::SeqCst) == self.generation.load(Ordering::SeqCst) {
            tdutils::log_warning!("Ready to stop");
            self.ready_to_stop.store(true, Ordering::SeqCst);
            // The C++ `yield()` re-ticks the actor; the loop wakes below.
            if self.close_flag.load(Ordering::SeqCst) {
                self.condvar.notify_all();
            }
        }
    }

    /// `CliClient::create_td` (cli.cpp:2083).
    fn create_td(&self, name: &str) {
        if self.ready_to_stop.load(Ordering::SeqCst) {
            return;
        }
        let generation = self.generation.load(Ordering::SeqCst) + 1;
        self.generation.store(generation, Ordering::SeqCst);
        tdutils::log_warning!("Creating new Td {} with generation {}", name, generation);
        // The C++ `TdCallbackImpl` capture becomes a client-id → generation
        // map entry; its destructor's `on_closed` is the close notification.
        let client_id = self.manager.create_client_id();
        self.generations
            .lock()
            .unwrap()
            .insert(client_id, generation);
        *self.td_client.lock().unwrap() = Some(client_id);

        if self.get_chat_list.load(Ordering::SeqCst) {
            self.send_request(Function::getChats(td_api::getChats {
                chat_list: None,
                limit: 10000,
            }));
        }
        if self.disable_network.load(Ordering::SeqCst) {
            self.send_request(Function::setNetworkType(td_api::setNetworkType {
                r#type: Some(NetworkType::networkTypeNone(td_api::networkTypeNone)),
            }));
        }
    }

    /// `CliClient::init_td` (cli.cpp:2129) — the flags reset + the first
    /// client. The `test_init` smoke block is dead C++ code (`bool test_init
    /// = false`) and is not ported.
    fn init_td(&self) {
        self.close_flag.store(false, Ordering::SeqCst);
        self.ready_to_stop.store(false, Ordering::SeqCst);
        self.generation.store(0, Ordering::SeqCst);
        self.closed_td.store(0, Ordering::SeqCst);

        self.create_td("ClientActor1");
    }

    /// `CliClient::send_request` (cli.cpp:2969).
    fn send_request(&self, request: Function) -> u64 {
        let client_id = self.td_client.lock().unwrap().as_ref().copied();
        if let Some(client_id) = client_id {
            let id = self.query_num.fetch_add(1, Ordering::SeqCst) + 1;
            self.manager.send(client_id, id, request);
            id
        } else {
            tdutils::log_error!("Failed to send: {}", request.to_debug_string());
            0
        }
    }

    /// `CliClient::get_log_tag_verbosity_level` (cli.cpp:2979) — through the
    /// synchronous execute path, like the C++ `ClientActor::execute`.
    fn get_log_tag_verbosity_level(&self, name: &str) -> i32 {
        let level = ClientManager::execute(Function::getLogTagVerbosityLevel(
            td_api::getLogTagVerbosityLevel {
                tag: name.to_owned(),
            },
        ));
        match level {
            Object::logVerbosityLevel(level) => level.verbosity_level,
            Object::error(_) => -1,
            _ => {
                debug_assert!(false, "unexpected getLogTagVerbosityLevel response");
                -1
            }
        }
    }

    /// The `td_requests`-gated receive print (cli.cpp:1867/2026): results
    /// and errors are only logged when the tag verbosity is above the first
    /// log's level (`lts td_requests N` raises it).
    fn requests_are_logged(&self) -> bool {
        self.combined_log_first_verbosity() < self.get_log_tag_verbosity_level("td_requests")
    }

    /// `combined_log.get_first_verbosity_level()`.
    fn combined_log_first_verbosity(&self) -> i32 {
        crate::log::COMBINED_LOG.get_first_verbosity_level()
    }

    /// `CliClient::on_result` (cli.cpp:1850) — the receive switch.
    fn on_result(&self, generation: u64, id: u64, result: Option<Object>) {
        // The two custom shapes (the sticker-set list compact prints), then
        // the generic `to_string(result)` — in that order, like the C++.
        let result_str = match &result {
            Some(Object::stickerSets(sets)) => {
                let mut out = format!(
                    "StickerSets {{ total_count = {}, count = {}",
                    sets.total_count,
                    sets.sets.len()
                );
                for set in &sets.sets {
                    out += &format!(", {}", set.name);
                }
                out += " }";
                out
            }
            Some(Object::trendingStickerSets(sets)) => {
                let mut out = format!(
                    "TrendingStickerSets {{ is_premium = {}, total_count = {}, count = {}",
                    sets.is_premium,
                    sets.total_count,
                    sets.sets.len()
                );
                for set in &sets.sets {
                    out += &format!(", {}", set.name);
                }
                out += " }";
                out
            }
            Some(object) => object.to_debug_string(),
            None => "null".to_owned(),
        };

        if id > 0 && self.requests_are_logged() {
            tdutils::log_error!("Receive result [{}][id={}] {}", generation, id, result_str);
        }

        if generation != self.generation.load(Ordering::SeqCst) {
            tdutils::log_info!("Drop received from previous Client {}", result_str);
            return;
        }

        // The three send-timing lambdas (`query_id_to_send_message_info_`)
        // feed the M.14.6 perf-stat dev commands and land with them.

        let Some(result) = result else {
            // A null result switches on id 0 — the default arm.
            return;
        };
        match result {
            Object::updateUser(update) => {
                self.register_user(update.user.as_ref().expect("updateUser carries a user"));
            }
            Object::updateSupergroup(update) => self.register_supergroup(
                update
                    .supergroup
                    .as_ref()
                    .expect("updateSupergroup carries a supergroup"),
            ),
            Object::users(users) => self.update_users(&users),
            Object::updateOption(option) => self.update_option(&option),
            Object::message(message) => self.on_get_message(&message),
            Object::messages(messages) => self.on_get_messages(&messages),
            Object::chatEvents(events) => self.on_get_chat_events(&events),
            Object::updateFileGenerationStart(update) => self.on_file_generation_start(&update),
            Object::updateAuthorizationState(mut update) => {
                tdutils::log_warning!("{}", result_str);
                self.on_update_authorization_state(update.authorization_state.take());
            }
            Object::updateChatLastMessage(_) => {
                // The C++ arm only feeds a commented-out trace.
            }
            Object::updateNewMessage(_) => {
                // The C++ `/start` responder is commented out on this tree.
            }
            // The business-message welcome and the test-DC pre-checkout
            // answer land with their M.14.4/M.14.6 command arms.
            Object::updateConnectionState(_) => {
                tdutils::log_warning!("{}", result_str);
            }
            Object::file(file) => self.on_get_file(&file),
            Object::updateFile(update) => {
                self.on_get_file(update.file.as_ref().expect("updateFile carries a file"));
            }
            _ => {}
        }
    }

    /// `CliClient::on_error` (cli.cpp:2025).
    fn on_error(&self, generation: u64, id: u64, error: td_api::Error) {
        if id > 0 && self.requests_are_logged() {
            tdutils::log_error!(
                "Receive error [{}][id={}] {}",
                generation,
                id,
                Object::error(error).to_debug_string()
            );
        }
    }

    /// `CliClient::register_user` (cli.cpp:259).
    fn register_user(&self, user: &td_api::User) {
        let mut users = self.users.lock().unwrap();
        let entry = users.entry(user.id).or_insert_with(|| User {
            first_name: String::new(),
            last_name: String::new(),
            username: String::new(),
        });
        entry.first_name = user.first_name.clone();
        entry.last_name = user.last_name.clone();
        entry.username = String::new();
        if let Some(usernames) = user.usernames.as_ref() {
            for username in &usernames.active_usernames {
                self.username_to_user_id
                    .lock()
                    .unwrap()
                    .insert(to_lower(username), user.id);
            }
            if let Some(first) = usernames.active_usernames.first() {
                entry.username = first.clone();
            }
        }
    }

    /// `CliClient::print_user` (cli.cpp:278).
    fn print_user(&self, user_id: i64) -> String {
        let users = self.users.lock().unwrap();
        let user = users
            .get(&user_id)
            .unwrap_or_else(|| panic!("print_user on unregistered user {}", user_id));
        format!("{} {} #{}", user.first_name, user.last_name, user_id)
    }

    /// `CliClient::update_users` (cli.cpp:284) — the PLAIN print.
    fn update_users(&self, users: &td_api::Users) {
        for user_id in &users.user_ids {
            if *user_id == 0 {
                continue;
            }
            tdutils::log_impl!(logging::PLAIN, "", "{}\n", self.print_user(*user_id));
        }
    }

    /// `CliClient::register_supergroup` (cli.cpp:298).
    fn register_supergroup(&self, supergroup: &td_api::Supergroup) {
        if let Some(usernames) = supergroup.usernames.as_ref() {
            let mut map = self.username_to_supergroup_id.lock().unwrap();
            for username in &usernames.active_usernames {
                map.insert(to_lower(username), supergroup.id);
            }
        }
        if supergroup.is_channel {
            assert!(supergroup.id != 0, "a channel has a non-zero id");
            self.channel_ids.lock().unwrap().insert(supergroup.id);
        }
    }

    /// `CliClient::update_option` (cli.cpp:310).
    fn update_option(&self, option: &td_api::updateOption) {
        if option.name == "my_id"
            && let Some(OptionValue::optionValueInteger(value)) = option.value.as_ref()
        {
            self.my_id.store(value.value, Ordering::SeqCst);
            tdutils::log_info!("Set my user identifier to {}", value.value);
        }
        if option.name == "authentication_token"
            && let Some(OptionValue::optionValueString(value)) = option.value.as_ref()
        {
            self.authentication_tokens
                .lock()
                .unwrap()
                .insert(0, value.value.clone());
        }
    }

    /// `CliClient::on_get_chat_events` (cli.cpp:322) — the event-log replay.
    fn on_get_chat_events(&self, events: &td_api::ChatEvents) {
        let log_chat_id = self.get_log_chat_id.load(Ordering::SeqCst);
        if log_chat_id == 0 {
            return;
        }
        let mut last_event_id = 0i64;
        for event in &events.events {
            if let Some(td_api::MessageSender::messageSenderUser(sender)) = event.member_id.as_ref()
            {
                tdutils::log_impl!(logging::PLAIN, "", "{} {}\n", event.date, sender.user_id);
            }
            last_event_id = event.id;
        }
        if last_event_id > 0 {
            self.send_request(Function::getChatEventLog(td_api::getChatEventLog {
                chat_id: log_chat_id,
                query: String::new(),
                from_event_id: last_event_id,
                limit: 100,
                filters: None,
                user_ids: Vec::new(),
            }));
        } else {
            self.get_log_chat_id.store(0, Ordering::SeqCst);
        }
    }

    /// `CliClient::on_get_messages` (cli.cpp:344) — the history/search
    /// replay pumps.
    fn on_get_messages(&self, messages: &td_api::Messages) {
        let history_chat_id = self.get_history_chat_id.load(Ordering::SeqCst);
        if history_chat_id != 0 {
            let mut last_message_id = 0i64;
            let mut last_message_date = 0i32;
            for message in &messages.messages {
                // LOG(PLAIN) << to_string(m); — commented out in the C++.
                if let Some(td_api::MessageContent::messageText(text)) = message.content.as_ref() {
                    let body = text.text.as_ref().expect("messageText carries text");
                    tdutils::log_impl!(logging::PLAIN, "", "{}\n", oneline(&body.text));
                }
                last_message_id = message.id;
                last_message_date = message.date;
            }
            if last_message_id > 0 && last_message_date > 1660000000 {
                self.send_request(Function::getChatHistory(td_api::getChatHistory {
                    chat_id: history_chat_id,
                    from_message_id: last_message_id,
                    offset: 0,
                    limit: 100,
                    only_local: false,
                }));
            } else {
                self.get_history_chat_id.store(0, Ordering::SeqCst);
            }
        }
        let search_chat_id = self.search_chat_id.load(Ordering::SeqCst);
        if search_chat_id != 0 {
            if let Some(last) = messages.messages.last() {
                let last_message_id = last.id;
                tdutils::log_error!("{}", last_message_id >> 20);
                self.send_request(Function::searchChatMessages(td_api::searchChatMessages {
                    chat_id: search_chat_id,
                    topic_id: self.get_message_topic_id(),
                    query: String::new(),
                    sender_id: None,
                    from_message_id: last_message_id,
                    offset: 0,
                    limit: 100,
                    filter: as_search_messages_filter("pvi"),
                }));
            } else {
                self.search_chat_id.store(0, Ordering::SeqCst);
            }
        }
    }

    /// `CliClient::on_get_message` (cli.cpp:376) — the body is commented out
    /// on this tree (the pending-send delete probe).
    fn on_get_message(&self, _message: &td_api::Message) {}

    /// `CliClient::on_get_file` (cli.cpp:383) — the download timing probes.
    fn on_get_file(&self, file: &td_api::File) {
        let local = file.local.as_ref().expect("file carries local state");
        let mut being_downloaded = self.being_downloaded_files.lock().unwrap();
        if !being_downloaded.contains_key(&file.id) && local.is_downloading_active {
            being_downloaded.insert(file.id, time::now());
        }
        if being_downloaded.contains_key(&file.id) && !local.is_downloading_active {
            let start = being_downloaded.remove(&file.id).expect("checked above");
            let elapsed_time = time::now() - start;
            if local.is_downloading_completed {
                tdutils::log_error!(
                    "File {} was downloaded in {:.6} seconds",
                    file.id,
                    elapsed_time
                );
            } else {
                tdutils::log_error!(
                    "File {} has failed to download in {:.6} seconds",
                    file.id,
                    elapsed_time
                );
            }
        }
    }

    /// `CliClient::on_file_generation_start` (cli.cpp:411). The happy path
    /// queues the generation for the `timeout_expired` pump; `skip` is
    /// silently dropped; a stat failure reports through
    /// `finishFileGeneration`.
    fn on_file_generation_start(&self, update: &td_api::updateFileGenerationStart) {
        let mut file_generation = FileGeneration {
            id: update.generation_id,
            destination: update.destination_path.clone(),
            source: String::new(),
            part_size: 0,
            local_size: 0,
            size: 0,
            test_local_size_decrease: false,
        };
        if update.conversion == "#url#" || update.conversion == "url" {
            // TODO: actually download (the C++ carries the same TODO).
            file_generation.source = "test.jpg".to_owned();
            file_generation.part_size = 1000000;
        } else if update.conversion == "skip" {
            return;
        } else {
            file_generation.source = update.original_path.clone();
            file_generation.part_size = tokenizer::to_integer_i64(&update.conversion);
            file_generation.test_local_size_decrease =
                !update.conversion.is_empty() && update.conversion.ends_with('t');
        }

        let stat = std::fs::metadata(&file_generation.source)
            .map_err(|error| error.to_string())
            .and_then(|metadata| {
                let size = metadata.len() as i64;
                if size <= 0 {
                    Err("File is empty".to_owned())
                } else if size > (4000i64 << 20) {
                    Err("File is too big".to_owned())
                } else {
                    Ok(size)
                }
            });
        let size = match stat {
            Ok(size) => size,
            Err(message) => {
                self.send_request(Function::finishFileGeneration(
                    td_api::finishFileGeneration {
                        generation_id: update.generation_id,
                        error: Some(td_api::Error { code: 400, message }),
                    },
                ));
                return;
            }
        };
        file_generation.size = size;
        if file_generation.part_size <= 0 {
            file_generation.part_size = file_generation.size;
        }
        self.pending_file_generations
            .lock()
            .unwrap()
            .push(file_generation);
        // The C++ `timeout_expired()` nudge — pump immediately.
        self.timeout_expired();
    }

    /// `CliClient::timeout_expired` (cli.cpp:9044) — the file-generation
    /// pump: read the next part, write it locally or send it through
    /// `writeGeneratedFilePart`, report progress, finish at the end.
    fn timeout_expired(&self) {
        if self.close_flag.load(Ordering::SeqCst) {
            return;
        }
        let mut pending = self.pending_file_generations.lock().unwrap();
        let mut i = 0;
        while i < pending.len() {
            let it = &mut pending[i];
            let left_size = it.size - it.local_size;
            assert!(left_size > 0, "a pending generation has bytes left");
            if it.part_size > left_size {
                it.part_size = left_size;
            }
            let mut block = vec![0u8; it.part_size as usize];
            {
                let mut source = std::fs::File::open(&it.source).expect("the source is readable");
                source
                    .seek(SeekFrom::Start(it.local_size as u64))
                    .expect("the source is seekable");
                source
                    .read_exact(&mut block)
                    .expect("the source has the promised bytes");
            }
            if random::fast_bool() {
                let mut options = std::fs::OpenOptions::new();
                options.write(true);
                if it.local_size == 0 {
                    options.truncate(true).create(true);
                }
                let mut destination = options
                    .open(&it.destination)
                    .expect("the destination is writable");
                destination
                    .seek(SeekFrom::Start(it.local_size as u64))
                    .expect("the destination is seekable");
                destination
                    .write_all(&block)
                    .expect("the destination accepts the part");
            } else {
                self.send_request(Function::writeGeneratedFilePart(
                    td_api::writeGeneratedFilePart {
                        generation_id: it.id,
                        offset: it.local_size,
                        data: block,
                    },
                ));
            }
            it.local_size += it.part_size;
            if it.local_size == it.size {
                self.send_request(Function::setFileGenerationProgress(
                    td_api::setFileGenerationProgress {
                        generation_id: it.id,
                        expected_size: it.size,
                        local_prefix_size: it.size,
                    },
                ));
                self.send_request(Function::finishFileGeneration(
                    td_api::finishFileGeneration {
                        generation_id: it.id,
                        error: None,
                    },
                ));
                pending.remove(i);
            } else {
                let mut local_size = it.local_size;
                if it.test_local_size_decrease && local_size > it.size / 2 {
                    local_size = local_size * 2 - it.size;
                }
                self.send_request(Function::setFileGenerationProgress(
                    td_api::setFileGenerationProgress {
                        generation_id: it.id,
                        expected_size: (it.size + 3 * it.local_size) / 4,
                        local_prefix_size: local_size,
                    },
                ));
                i += 1;
            }
        }
    }

    /// `CliClient::get_message_topic_id` (cli.cpp:1246).
    #[allow(clippy::let_and_return)] // the C++ shape, kept arm for arm
    fn get_message_topic_id(&self) -> Option<MessageTopic> {
        let saved_messages_topic_id = self.saved_messages_topic_id.load(Ordering::SeqCst);
        if saved_messages_topic_id != 0 {
            // `as_saved_messages_topic_id` (cli.cpp:1230).
            let saved = if saved_messages_topic_id == -1 {
                2666000
            } else {
                saved_messages_topic_id
            };
            return Some(MessageTopic::messageTopicSavedMessages(
                td_api::messageTopicSavedMessages {
                    saved_messages_topic_id: saved,
                },
            ));
        }
        let direct = self.direct_messages_chat_topic_id.load(Ordering::SeqCst);
        if direct != 0 {
            return Some(MessageTopic::messageTopicDirectMessages(
                td_api::messageTopicDirectMessages {
                    direct_messages_chat_topic_id: direct,
                },
            ));
        }
        let forum = self.forum_topic_id.load(Ordering::SeqCst);
        if forum != 0 {
            return Some(MessageTopic::messageTopicForum(td_api::messageTopicForum {
                forum_topic_id: forum,
            }));
        }
        let thread = self.message_thread_id.load(Ordering::SeqCst);
        if thread != 0 {
            return Some(MessageTopic::messageTopicThread(
                td_api::messageTopicThread {
                    message_thread_id: thread,
                },
            ));
        }
        None
    }

    /// `CliClient::on_update_authorization_state` (cli.cpp:447).
    fn on_update_authorization_state(&self, state: Option<AuthorizationState>) {
        let state = state.expect("the update carries an authorization state");
        match &state {
            AuthorizationState::authorizationStateWaitTdlibParameters(_) => {
                self.send_request(Function::setOption(td_api::setOption {
                    name: "use_pfs".to_owned(),
                    value: Some(OptionValue::optionValueBoolean(
                        td_api::optionValueBoolean { value: true },
                    )),
                }));
                self.send_request(Function::setTdlibParameters(td_api::setTdlibParameters {
                    use_test_dc: self.use_test_dc.load(Ordering::SeqCst),
                    use_message_database: true,
                    use_chat_info_database: true,
                    use_secret_chats: true,
                    api_id: self.api_id.load(Ordering::SeqCst),
                    api_hash: self.api_hash.lock().unwrap().clone(),
                    system_language_code: "en".to_owned(),
                    device_model: "Desktop".to_owned(),
                    application_version: "1.0".to_owned(),
                    ..Default::default()
                }));
            }
            AuthorizationState::authorizationStateReady(_) => {
                tdutils::log_info!("Logged in");
            }
            AuthorizationState::authorizationStateClosed(_) => {
                tdutils::log_warning!("Td closed");
                *self.td_client.lock().unwrap() = None;
                if !self.close_flag.load(Ordering::SeqCst) {
                    self.create_td("ClientActor3");
                }
            }
            _ => {}
        }
        *self.authorization_state.lock().unwrap() = Some(state);
    }

    /// `CliClient::add_cmd` (cli.cpp:9116).
    fn add_cmd(&self, cmd: String) {
        self.cmd_queue.lock().unwrap().push_back(cmd);
        self.condvar.notify_all();
    }

    /// `CliClient::on_cmd` (cli.cpp:3058) — the input preprocessing, the
    /// op/args split, and the 19 sequential op-blocks. The C++ dispatcher is
    /// a chain of independent `if (op == ...) ... else op_not_found_count++;`
    /// blocks: a command takes the arms of *every* matching block (no early
    /// return), and the unknown-command report fires only when all 19 are
    /// unmatched — hence `OP_BLOCK_COUNT = 19` (pinned empirically: every
    /// unknown command reports, cli.cpp:9020).
    fn on_cmd(&self, cmd: String) {
        let mut cmd = tokenizer::strip_ansi(cmd);
        cmd = tokenizer::remove_control_chars(cmd);
        tdutils::log_info!("CMD:[{cmd}]");

        let (op, args) = tokenizer::split(&cmd, ' ');

        const OP_BLOCK_COUNT: i32 = 19;
        let mut op_not_found_count = 0;

        // The 19 blocks, in cli.cpp order; the census fixture
        // (`testdata/cli_census.txt`, 1294 arms) pins the per-block command
        // inventory, and the slices fill them in: M.14.3 auth, M.14.4
        // messages/dialogs, M.14.5 files, M.14.6 misc/dev.
        if !self.op_block_auth(&op, &args) {
            op_not_found_count += 1; // cli.cpp:3098 — the auth/account family (M.14.3)
        }
        if !self.op_block_recovery(&op, &args) {
            op_not_found_count += 1; // cli.cpp:3290 — recovery/imported contacts (M.14.3)
        }
        if !self.op_block_chats(&op, &args) {
            op_not_found_count += 1; // cli.cpp:3747 — chat list (M.14.4)
        }
        if !self.op_block_messages(&op, &args) {
            op_not_found_count += 1; // cli.cpp:4397 — send/edit/forward/... (M.14.4)
        }
        if !self.op_block_dialogs(&op, &args) {
            op_not_found_count += 1; // cli.cpp:4510 — dialog ops (M.14.4)
        }
        if !self.op_block_history(&op, &args) {
            op_not_found_count += 1; // cli.cpp:4821 — history/view/search (M.14.4)
        }
        if !self.op_block_files(&op, &args) {
            op_not_found_count += 1; // cli.cpp:5053 — download/upload/cancel (M.14.5)
        }
        if !self.op_block_stickers(&op, &args) {
            op_not_found_count += 1; // cli.cpp:5510 — sticker probes (M.14.6)
        }
        if !self.op_block_web_pages(&op, &args) {
            op_not_found_count += 1; // cli.cpp:5840 — webpage/rich-media probes (M.14.6)
        }
        if !self.op_block_misc(&op, &args) {
            op_not_found_count += 1; // cli.cpp:6061 — misc dev commands (M.14.6)
        }
        if !self.op_block_network(&op, &args) {
            op_not_found_count += 1; // cli.cpp:6840 — network/proxy probes (M.14.6)
        }
        if !self.op_block_logging(&op, &args) {
            op_not_found_count += 1; // cli.cpp:7362 — log/verbosity (M.14.6)
        }
        if !self.op_block_drafts(&op, &args) {
            op_not_found_count += 1; // cli.cpp:7554 — drafts/typing (M.14.4)
        }
        if !self.op_block_media(&op, &args) {
            op_not_found_count += 1; // cli.cpp:7714 — media/album extras (M.14.4)
        }
        if !self.op_block_dev_a(&op, &args) {
            op_not_found_count += 1; // cli.cpp:8000 — dev probes A (M.14.6)
        }
        if !self.op_block_dev_b(&op, &args) {
            op_not_found_count += 1; // cli.cpp:8200 — dev probes B (M.14.6)
        }
        if !self.op_block_dev_c(&op, &args) {
            op_not_found_count += 1; // cli.cpp:8500 — dev probes C (M.14.6)
        }
        if !self.op_block_stats(&op, &args) {
            op_not_found_count += 1; // cli.cpp:8850 — perf counters (M.14.6)
        }
        if !self.op_block_quit(&op, &args) {
            op_not_found_count += 1; // cli.cpp:8985 — q/dnq/alog/fatal/unreachable (M.14.6)
        }

        if op_not_found_count == OP_BLOCK_COUNT && !cmd.is_empty() {
            tdutils::log_error!("Unknown command \"{cmd}\" of length {}", cmd.len());
        }
    }

    /// `CliClient::as_user_id` (cli.cpp:684) — `me`, `.` (the opened chat),
    /// `@username` through the registry, else the plain integer.
    fn as_user_id(&self, s: &str) -> i64 {
        let s = trim_td(s);
        if s == "me" {
            return self.my_id.load(Ordering::SeqCst);
        }
        if s == "." {
            return self.opened_chat_id.load(Ordering::SeqCst);
        }
        let s = s.strip_prefix('@').unwrap_or(s);
        if s.as_bytes()
            .first()
            .is_some_and(|b| b.is_ascii_alphabetic())
        {
            if let Some(user_id) = self.username_to_user_id.lock().unwrap().get(&to_lower(s)) {
                return *user_id;
            }
            tdutils::log_error!("Can't find user {}", s);
            return 0;
        }
        tokenizer::to_integer_i64(s)
    }

    /// `CliClient::as_user_ids` (cli.cpp:708).
    fn as_user_ids(&self, s: &str) -> Vec<i64> {
        autosplit(s).iter().map(|t| self.as_user_id(t)).collect()
    }

    /// `as_phone_number_authentication_settings` (cli.cpp:2953).
    fn as_phone_number_authentication_settings(&self) -> td_api::PhoneNumberAuthenticationSettings {
        td_api::PhoneNumberAuthenticationSettings {
            allow_flash_call: false,
            allow_missed_call: true,
            is_current_phone_number: false,
            has_unknown_phone_number: false,
            allow_sms_retriever_api: false,
            firebase_authentication_settings: None,
            authentication_tokens: self.authentication_tokens.lock().unwrap().clone(),
        }
    }

    /// cli.cpp:3098 — the auth/account chain (M.14.3): the login/code/
    /// passkey/password family, the account tools, the password state, the
    /// Passport form probes, and the e-mail verification family. The C++
    /// `get_args(args, a, b, …)` variadic is sugar for word-splits with the
    /// last argument taking the remainder — spelled out per arm here.
    fn op_block_auth(&self, op: &str, args: &str) -> bool {
        // `get_args`'s word split: consume one space-separated word, the
        // rest stays in `args` (the last argument takes the remainder).
        macro_rules! words {
            ($args:ident => $($word:ident),+ ; $rest:ident) => {{
                let mut rest: String = $args.to_owned();
                $(
                    let ($word, tail) = tokenizer::split(&rest, ' ');
                    rest = tail;
                    let $word = $word;
                )+
                let $rest = rest;
                ($($word),+, $rest)
            }};
        }

        if op == "gas" {
            let state = self.authorization_state.lock().unwrap();
            tdutils::log_error!(
                "{}",
                match &*state {
                    Some(state) => state.to_debug_string(),
                    None => "null".to_owned(),
                }
            );
            true
        } else if op == "sap" || op == "sapn" {
            self.send_request(Function::setAuthenticationPhoneNumber(
                td_api::setAuthenticationPhoneNumber {
                    phone_number: args.to_owned(),
                    settings: Some(self.as_phone_number_authentication_settings()),
                },
            ));
            true
        } else if op == "capp" {
            let (premium_day_count, currency, amount) =
                words!(args => premium_day_count, currency ; amount);
            self.send_request(Function::checkAuthenticationPremiumPurchase(
                td_api::checkAuthenticationPremiumPurchase {
                    premium_day_count: tokenizer::to_integer_i32(&premium_day_count),
                    currency,
                    amount: tokenizer::to_integer_i64(&amount),
                },
            ));
            true
        } else if op == "sae" || op == "saea" {
            self.send_request(Function::setAuthenticationEmailAddress(
                td_api::setAuthenticationEmailAddress {
                    email_address: args.to_owned(),
                },
            ));
            true
        } else if op == "rac" {
            self.send_request(Function::resendAuthenticationCode(
                td_api::resendAuthenticationCode { reason: None },
            ));
            true
        } else if op == "sdek" {
            self.send_request(Function::setDatabaseEncryptionKey(
                td_api::setDatabaseEncryptionKey {
                    new_encryption_key: args.as_bytes().to_vec(),
                },
            ));
            true
        } else if op == "caec" {
            self.send_request(Function::checkAuthenticationEmailCode(
                td_api::checkAuthenticationEmailCode {
                    code: as_email_address_authentication(args),
                },
            ));
            true
        } else if op == "cac" {
            self.send_request(Function::checkAuthenticationCode(
                td_api::checkAuthenticationCode {
                    code: args.to_owned(),
                },
            ));
            true
        } else if op == "racmg" {
            self.send_request(Function::reportAuthenticationCodeMissing(
                td_api::reportAuthenticationCodeMissing {
                    mobile_network_code: args.to_owned(),
                },
            ));
            true
        } else if op == "ru" || op == "rus" {
            let (first_name, last_name) = tokenizer::split(args, ' ');
            self.send_request(Function::registerUser(td_api::registerUser {
                first_name,
                last_name,
                disable_notification: op == "rus",
            }));
            true
        } else if op == "cap" {
            self.send_request(Function::checkAuthenticationPassword(
                td_api::checkAuthenticationPassword {
                    password: args.to_owned(),
                },
            ));
            true
        } else if op == "cabt" {
            self.send_request(Function::checkAuthenticationBotToken(
                td_api::checkAuthenticationBotToken {
                    token: args.to_owned(),
                },
            ));
            true
        } else if op == "qr" {
            self.send_request(Function::requestQrCodeAuthentication(
                td_api::requestQrCodeAuthentication {
                    other_user_ids: self.as_user_ids(args),
                },
            ));
            true
        } else if op == "gapo" {
            self.send_request(Function::getAuthenticationPasskeyParameters(
                td_api::getAuthenticationPasskeyParameters,
            ));
            true
        } else if op == "capk" {
            let (credential_id, client_data, authenticator_data, signature, user_handle) = words!(args => credential_id, client_data, authenticator_data, signature ; user_handle);
            self.send_request(Function::checkAuthenticationPasskey(
                td_api::checkAuthenticationPasskey {
                    credential_id,
                    client_data,
                    authenticator_data: base64url_decode(&authenticator_data),
                    signature: base64url_decode(&signature),
                    user_handle: base64url_decode(&user_handle),
                },
            ));
            true
        } else if op == "cawt" {
            let (token, dc_id) = words!(args => token ; dc_id);
            self.send_request(Function::checkAuthenticationWebToken(
                td_api::checkAuthenticationWebToken {
                    token,
                    dc_id: tokenizer::to_integer_i32(&dc_id),
                },
            ));
            true
        } else if op == "cqr" {
            self.send_request(Function::confirmQrCodeAuthentication(
                td_api::confirmQrCodeAuthentication {
                    link: args.to_owned(),
                },
            ));
            true
        } else if op == "gcs" {
            self.send_request(Function::getCurrentState(td_api::getCurrentState));
            true
        } else if op == "raea" {
            self.send_request(Function::resetAuthenticationEmailAddress(
                td_api::resetAuthenticationEmailAddress,
            ));
            true
        } else if op == "rapr" {
            self.send_request(Function::requestAuthenticationPasswordRecovery(
                td_api::requestAuthenticationPasswordRecovery,
            ));
            true
        } else if op == "caprc" {
            self.send_request(Function::checkAuthenticationPasswordRecoveryCode(
                td_api::checkAuthenticationPasswordRecoveryCode {
                    recovery_code: args.to_owned(),
                },
            ));
            true
        } else if op == "rap" {
            let (recovery_code, new_password, new_hint) =
                words!(args => recovery_code, new_password ; new_hint);
            self.send_request(Function::recoverAuthenticationPassword(
                td_api::recoverAuthenticationPassword {
                    recovery_code,
                    new_password,
                    new_hint,
                },
            ));
            true
        } else if op == "lo" {
            self.send_request(Function::logOut(td_api::logOut));
            true
        } else if op == "destroy" {
            self.send_request(Function::destroy(td_api::destroy));
            true
        } else if op == "reset" {
            // `td_client_.reset()` — the actor drop; the close notification
            // arrives through the response queue and drives `on_closed`.
            if let Some(client_id) = self.td_client.lock().unwrap().take() {
                self.manager.close(client_id);
            }
            true
        } else if op == "close_td" {
            self.send_request(Function::close(td_api::close));
            true
        } else if op == "DeleteAccountYesIReallyWantToDeleteMyAccount" {
            let (password, reason) = tokenizer::split(args, ' ');
            self.send_request(Function::deleteAccount(td_api::deleteAccount {
                reason,
                password,
            }));
            true
        } else if op == "gps" {
            self.send_request(Function::getPasswordState(td_api::getPasswordState));
            true
        } else if op == "spass" {
            let (mut password, mut new_password, mut new_hint, mut recovery_email_address) =
                words!(args => password, new_password, new_hint ; recovery_email_address);
            // The `#` spell stands for the empty string (the C++ `if (x ==
            // "#") x = ""` quartet).
            for value in [
                &mut password,
                &mut new_password,
                &mut new_hint,
                &mut recovery_email_address,
            ] {
                if value == "#" {
                    *value = String::new();
                }
            }
            self.send_request(Function::setPassword(td_api::setPassword {
                old_password: password,
                new_password,
                new_hint,
                set_recovery_email_address: true,
                new_recovery_email_address: recovery_email_address,
            }));
            true
        } else if op == "gpafhttp" {
            // The C++ arm drives the request through `HttpReader` and reads
            // the parsed query args; the Rust tdnet port carries no URL-arg
            // parsing, so the same `?a=b&c=d` split (percent-decoded, first
            // match wins — `HttpQuery::get_arg`) is spelled out here.
            let mut query_args: Vec<(String, String)> = Vec::new();
            if let Some((_, query)) = args.split_once('?') {
                for pair in query.split('&') {
                    let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
                    query_args.push((url_decode(key), url_decode(value)));
                }
            }
            let get_arg = |key: &str| -> &str {
                query_args
                    .iter()
                    .find(|(k, _)| k == key)
                    .map(|(_, v)| v.as_str())
                    .unwrap_or("")
            };
            let bot_user_id = get_arg("bot_id");
            let scope = get_arg("scope");
            let public_key = get_arg("public_key");
            let payload = get_arg("payload");
            tdutils::log_info!("Callback URL:{}", get_arg("callback_url"));
            self.send_request(Function::getPassportAuthorizationForm(
                td_api::getPassportAuthorizationForm {
                    bot_user_id: self.as_user_id(bot_user_id),
                    scope: scope.to_owned(),
                    public_key: public_key.to_owned(),
                    nonce: payload.to_owned(),
                },
            ));
            true
        } else if op == "gpaf" {
            let (bot_user_id, scope, payload) = words!(args => bot_user_id, scope ; payload);
            const PUBLIC_KEY: &str = concat!(
                "-----BEGIN PUBLIC KEY-----\n",
                "MIICIjANBgkqhkiG9w0BAQEFAAOCAg8AMIICCgKCAgEAzmgKr0fPP4rB/TsNEweC\n",
                "hoG3ntUxuBTmHsFBW6CpABGdaTmKZSjAI/cTofhBgtRQIOdX0YRGHHHhwyLf49Wv\n",
                "9l+XexbJOa0lTsJSNMj8Y/9sZbqUl5ur8ZOTM0sxbXC0XKexu1tM9YavH+Lbrobk\n",
                "jt0+cmo/zEYZWNtLVihnR2IDv+7tSgiDoFWi/koAUdfJ1VMw+hReUaLg3vE9CmPK\n",
                "tQiTy+NvmrYaBPb75I0Jz3Lrz1+mZSjLKO25iT84RIsxarBDd8iYh2avWkCmvtiR\n",
                "Lcif8wLxi2QWC1rZoCA3Ip+Hg9J9vxHlzl6xT01WjUStMhfwrUW6QBpur7FJ+aKM\n",
                "oaMoHieFNCG4qIkWVEHHSsUpLum4SYuEnyNH3tkjbrdldZanCvanGq+TZyX0buRt\n",
                "4zk7FGcu8iulUkAP/o/WZM0HKinFN/vuzNVA8iqcO/BBhewhzpqmmTMnWmAO8WPP\n",
                "DJMABRtXJnVuPh1CI5pValzomLJM4/YvnJGppzI1QiHHNA9JtxVmj2xf8jaXa1LJ\n",
                "WUNJK+RvUWkRUxpWiKQQO9FAyTPLRtDQGN9eUeDR1U0jqRk/gNT8smHGN6I4H+NR\n",
                "3X3/1lMfcm1dvk654ql8mxjCA54IpTPr/icUMc7cSzyIiQ7Tp9PZTl1gHh281ZWf\n",
                "P7d2+fuJMlkjtM7oAwf+tI8CAwEAAQ==\n",
                "-----END PUBLIC KEY-----",
            );
            self.send_request(Function::getPassportAuthorizationForm(
                td_api::getPassportAuthorizationForm {
                    bot_user_id: self.as_user_id(&bot_user_id),
                    scope,
                    public_key: PUBLIC_KEY.to_owned(),
                    nonce: payload,
                },
            ));
            true
        } else if op == "gpafae" {
            let (form_id, password) = words!(args => form_id ; password);
            self.send_request(Function::getPassportAuthorizationFormAvailableElements(
                td_api::getPassportAuthorizationFormAvailableElements {
                    authorization_form_id: tokenizer::to_integer_i32(&form_id),
                    password,
                },
            ));
            true
        } else if op == "spaf" {
            let (form_id, types) = words!(args => form_id ; types);
            self.send_request(Function::sendPassportAuthorizationForm(
                td_api::sendPassportAuthorizationForm {
                    authorization_form_id: tokenizer::to_integer_i32(&form_id),
                    types: as_passport_element_types(&types),
                },
            ));
            true
        } else if op == "gpcl" {
            self.send_request(Function::getPreferredCountryLanguage(
                td_api::getPreferredCountryLanguage {
                    country_code: args.to_owned(),
                },
            ));
            true
        } else if op == "seavc" {
            self.send_request(Function::sendEmailAddressVerificationCode(
                td_api::sendEmailAddressVerificationCode {
                    email_address: args.to_owned(),
                },
            ));
            true
        } else if op == "ceavc" {
            self.send_request(Function::checkEmailAddressVerificationCode(
                td_api::checkEmailAddressVerificationCode {
                    code: args.to_owned(),
                },
            ));
            true
        } else if op == "reavc" {
            self.send_request(Function::resendEmailAddressVerificationCode(
                td_api::resendEmailAddressVerificationCode,
            ));
            true
        } else if op == "ilear" {
            self.send_request(Function::isLoginEmailAddressRequired(
                td_api::isLoginEmailAddressRequired,
            ));
            true
        } else if op == "slea" {
            self.send_request(Function::setLoginEmailAddress(
                td_api::setLoginEmailAddress {
                    new_login_email_address: args.to_owned(),
                },
            ));
            true
        } else if op == "rleac" {
            self.send_request(Function::resendLoginEmailAddressCode(
                td_api::resendLoginEmailAddressCode,
            ));
            true
        } else if op == "cleac" {
            self.send_request(Function::checkLoginEmailAddressCode(
                td_api::checkLoginEmailAddressCode {
                    code: as_email_address_authentication(args),
                },
            ));
            true
        } else if op == "srea" {
            let (password, recovery_email_address) = tokenizer::split(args, ' ');
            self.send_request(Function::setRecoveryEmailAddress(
                td_api::setRecoveryEmailAddress {
                    password,
                    new_recovery_email_address: recovery_email_address,
                },
            ));
            true
        } else if op == "grea" {
            self.send_request(Function::getRecoveryEmailAddress(
                td_api::getRecoveryEmailAddress {
                    password: args.to_owned(),
                },
            ));
            true
        } else if op == "creac" {
            self.send_request(Function::checkRecoveryEmailAddressCode(
                td_api::checkRecoveryEmailAddressCode {
                    code: args.to_owned(),
                },
            ));
            true
        } else if op == "rreac" {
            self.send_request(Function::resendRecoveryEmailAddressCode(
                td_api::resendRecoveryEmailAddressCode,
            ));
            true
        } else if op == "creav" {
            self.send_request(Function::cancelRecoveryEmailAddressVerification(
                td_api::cancelRecoveryEmailAddressVerification,
            ));
            true
        } else {
            false
        }
    }

    /// cli.cpp:3289 — the recovery chain head (`rpr`/`cprc`/`rp`); the rest
    /// of the chain lands with its M.14 slices.
    fn op_block_recovery(&self, op: &str, args: &str) -> bool {
        if op == "rpr" {
            self.send_request(Function::requestPasswordRecovery(
                td_api::requestPasswordRecovery,
            ));
            true
        } else if op == "cprc" {
            self.send_request(Function::checkPasswordRecoveryCode(
                td_api::checkPasswordRecoveryCode {
                    recovery_code: args.to_owned(),
                },
            ));
            true
        } else if op == "rp" {
            let (recovery_code, tail) = tokenizer::split(args, ' ');
            let (new_password, new_hint) = tokenizer::split(&tail, ' ');
            self.send_request(Function::recoverAuthenticationPassword(
                td_api::recoverAuthenticationPassword {
                    recovery_code,
                    new_password,
                    new_hint,
                },
            ));
            true
        } else {
            false
        }
    }

    fn op_block_chats(&self, _op: &str, _args: &str) -> bool {
        false
    }

    fn op_block_messages(&self, _op: &str, _args: &str) -> bool {
        false
    }

    fn op_block_dialogs(&self, _op: &str, _args: &str) -> bool {
        false
    }

    fn op_block_history(&self, _op: &str, _args: &str) -> bool {
        false
    }

    fn op_block_files(&self, _op: &str, _args: &str) -> bool {
        false
    }

    fn op_block_stickers(&self, _op: &str, _args: &str) -> bool {
        false
    }

    fn op_block_web_pages(&self, _op: &str, _args: &str) -> bool {
        false
    }

    fn op_block_misc(&self, _op: &str, _args: &str) -> bool {
        false
    }

    fn op_block_network(&self, _op: &str, _args: &str) -> bool {
        false
    }

    fn op_block_logging(&self, _op: &str, _args: &str) -> bool {
        false
    }

    fn op_block_drafts(&self, _op: &str, _args: &str) -> bool {
        false
    }

    fn op_block_media(&self, _op: &str, _args: &str) -> bool {
        false
    }

    fn op_block_dev_a(&self, _op: &str, _args: &str) -> bool {
        false
    }

    fn op_block_dev_b(&self, _op: &str, _args: &str) -> bool {
        false
    }

    fn op_block_dev_c(&self, _op: &str, _args: &str) -> bool {
        false
    }

    fn op_block_stats(&self, _op: &str, _args: &str) -> bool {
        false
    }

    /// cli.cpp:8985 — `q` (the rest of the block — `dnq`, `alog`/`aloge`,
    /// `fatal`, `unreachable` — lands in M.14.6).
    fn op_block_quit(&self, op: &str, _args: &str) -> bool {
        if op == "q" {
            self.quit();
            true
        } else {
            false
        }
    }

    /// Drains the manager's response queue: results/errors dispatch through
    /// `on_result`/`on_error` (keyed by the client's generation). The close
    /// accounting rides the manager's receive adaptation (`Impl::receive`):
    /// the C++ `TdCallbackImpl` destructor's `{client_id, 0, nullptr}`
    /// notification is delivered as a synthesized
    /// `updateAuthorizationState(authorizationStateClosed)` carrying the
    /// client id (and the Td's own organic Closed update is consumed into an
    /// empty response), so the synthesized update is what fires `on_closed`
    /// here.
    fn drain_responses(&self) {
        loop {
            let response = self.manager.receive(0.0);
            let Response {
                client_id,
                request_id: id,
                object,
            } = response;
            let is_closed_update = matches!(
                &object,
                Some(Object::updateAuthorizationState(update))
                    if matches!(
                        update.authorization_state,
                        Some(AuthorizationState::authorizationStateClosed(_))
                    )
            );
            let generation = self
                .generations
                .lock()
                .unwrap()
                .get(&client_id)
                .copied()
                .unwrap_or(0);
            match object {
                Some(Object::error(error)) if id > 0 => self.on_error(generation, id, error),
                Some(object) => {
                    self.on_result(generation, id, Some(object));
                    if is_closed_update && client_id != 0 {
                        // The destructor event: the generation accounting
                        // (`closed_td_ == generation_` → `ready_to_stop_`).
                        self.on_closed(generation);
                    }
                }
                // An empty queue — or the manager's consumed organic Closed
                // update; both are "nothing to do this tick".
                None => break,
            }
        }
    }
}

/// `td::as_search_messages_filter` (cli.cpp:2407) — the filter coercer.
fn as_search_messages_filter(filter: &str) -> Option<SearchMessagesFilter> {
    let mut filter = filter.trim().to_lowercase();
    if let Some(stripped) = filter.strip_prefix("search") {
        filter = stripped.to_owned();
    }
    macro_rules! filter_arm {
        ($names:literal $(| $names_rest:literal)?, $ctor:ident) => {
            if filter == $names $(|| filter == $names_rest)? {
                return Some(SearchMessagesFilter::$ctor(td_api::$ctor));
            }
        };
    }
    filter_arm!("an" | "animation", searchMessagesFilterAnimation);
    filter_arm!("au" | "audio", searchMessagesFilterAudio);
    filter_arm!("d" | "document", searchMessagesFilterDocument);
    filter_arm!("p" | "photo", searchMessagesFilterPhoto);
    filter_arm!("vi" | "video", searchMessagesFilterVideo);
    filter_arm!("vo" | "voice", searchMessagesFilterVoiceNote);
    filter_arm!("pvi", searchMessagesFilterPhotoAndVideo);
    filter_arm!("u" | "url", searchMessagesFilterUrl);
    filter_arm!("cp" | "chatphoto", searchMessagesFilterChatPhoto);
    filter_arm!("vn" | "videonote", searchMessagesFilterVideoNote);
    filter_arm!(
        "vvn" | "voicevideonote",
        searchMessagesFilterVoiceAndVideoNote
    );
    filter_arm!("m" | "mention", searchMessagesFilterMention);
    filter_arm!("um" | "umention", searchMessagesFilterUnreadMention);
    filter_arm!("ur" | "ureaction", searchMessagesFilterUnreadReaction);
    filter_arm!("upv" | "uvote", searchMessagesFilterUnreadPollVote);
    filter_arm!("f" | "failed", searchMessagesFilterFailedToSend);
    filter_arm!("pi" | "pinned", searchMessagesFilterPinned);
    filter_arm!("poll", searchMessagesFilterPoll);
    if !filter.is_empty() {
        tdutils::log_error!("Unsupported message filter {}", filter);
    }
    None
}

/// `CliClient::loop` (cli.cpp:9021): init once, pump stdin lines through the
/// queue, drain the queue while not closing, drain the response queue, exit
/// when quit completes.
pub fn run(client: &'static CliClient) {
    // The C++ CliClient actor runs on scheduler thread 1 — its logs carry
    // `[t 1]`; the actor context supplies the `!CliClient` tag.
    logging::set_thread_id(1);
    logging::set_log_tag2("CliClient");
    if client.tty {
        eprint!("{PROMPT}");
    }

    spawn_pump(client);

    let mut is_inited = false;
    loop {
        if !is_inited {
            is_inited = true;
            client.init_td();
        }

        while !client.close_flag.load(Ordering::SeqCst) {
            let cmd = client.cmd_queue.lock().unwrap().pop_front();
            match cmd {
                Some(cmd) => {
                    client.on_cmd(cmd);
                    if client.tty {
                        eprint!("{PROMPT}");
                    }
                }
                None => break,
            }
        }

        client.drain_responses();
        client.timeout_expired();

        if client.ready_to_stop.load(Ordering::SeqCst)
            && client.close_flag.load(Ordering::SeqCst)
            && client.stdin_reader_stopped.load(Ordering::SeqCst)
        {
            break;
        }

        // Idle tick — the C++ scheduler granularity (`run_main` at 100 ms in
        // `td::main`); a pending file generation re-ticks at its 10 ms
        // `set_timeout_in(0.01)`.
        let tick = if !client.pending_file_generations.lock().unwrap().is_empty() {
            std::time::Duration::from_millis(10)
        } else {
            std::time::Duration::from_millis(100)
        };
        let queue = client.cmd_queue.lock().unwrap();
        if queue.is_empty() && !client.close_flag.load(Ordering::SeqCst) {
            let _ = client.condvar.wait_timeout(queue, tick).unwrap();
        }
    }
}

/// The stdin reader thread: the C++ epoll reader. Blocks until a line is
/// available, queues it, then drains whatever else is immediately readable
/// (the C++ `flush_read` + `while buffer non-empty` tick) so that piped
/// input is fully queued before processing — the observable drop-after-quit
/// ordering depends on it.
fn spawn_pump(client: &'static CliClient) {
    std::thread::Builder::new()
        .name("cli-stdin".to_string())
        .spawn(move || pump(client))
        .expect("failed to spawn the stdin pump thread");
}

fn pump(client: &'static CliClient) {
    let mut reader = StdinReader::new();
    'outer: loop {
        if client.close_flag.load(Ordering::SeqCst) {
            break;
        }
        // Blocking wait for data (the C++ idle tick on the pollable fd).
        if !reader.poll_blocking() {
            break;
        }
        loop {
            match reader.read_once() {
                Ok(true) => {}
                Ok(false) => {
                    // EOF. readline semantics: the Ctrl-D char on an empty
                    // line surfaces as the nullptr callback → FATAL "Closed"
                    // (tty only); a partial line is delivered first; piped
                    // EOF is silent — the process stays alive (pinned
                    // empirically against the C++ tg_cli, 2026-09-19).
                    if client.tty {
                        if !reader.has_pending() {
                            tdutils::log_fatal!("Closed");
                        }
                        if let Some(line) = reader.take_pending_line() {
                            if client.close_flag.load(Ordering::SeqCst) {
                                break 'outer;
                            }
                            client.add_cmd(line);
                            continue 'outer;
                        }
                    }
                    break 'outer;
                }
                Err(_) => break 'outer,
            }
            let mut delivered = false;
            while let Some(line) = reader.next_line() {
                delivered = true;
                if client.close_flag.load(Ordering::SeqCst) {
                    break 'outer;
                }
                client.add_cmd(line);
            }
            if reader.poll_zero() {
                // more input is immediately available — keep draining
                continue;
            }
            if !delivered {
                // a partial line is buffered — wait for the rest
                break;
            }
            break;
        }
    }
    client.stdin_reader_stopped.store(true, Ordering::SeqCst);
    client.condvar.notify_all();
}

/// A raw-fd(0) line reader with the C++ `BufferedStdin`-style semantics:
/// `poll` for readability, `read` only when readable, lines terminated by
/// `\n` (a trailing `\r` is stripped like `process_stdin`), partial lines
/// held until newline or EOF.
struct StdinReader {
    buffer: Vec<u8>,
    start: usize,
}

impl StdinReader {
    fn new() -> Self {
        Self {
            buffer: Vec::with_capacity(4096),
            start: 0,
        }
    }

    /// `poll` with a blocking timeout; `false` on poll error (read would not
    /// help — treated like EOF by the caller).
    fn poll_blocking(&self) -> bool {
        poll_fd(0, -1)
    }

    /// `poll` with a zero timeout — is input available right now?
    fn poll_zero(&self) -> bool {
        poll_fd(0, 0)
    }

    /// Read once; `Ok(true)` — some bytes, `Ok(false)` — EOF.
    fn read_once(&mut self) -> std::io::Result<bool> {
        let mut tmp = [0u8; 4096];
        loop {
            let n = unsafe { libc::read(0, tmp.as_mut_ptr() as *mut libc::c_void, tmp.len()) };
            if n < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            if n == 0 {
                return Ok(false);
            }
            self.buffer.extend_from_slice(&tmp[..n as usize]);
            return Ok(true);
        }
    }

    /// The next `\n`-terminated line (trailing `\r` stripped).
    fn next_line(&mut self) -> Option<String> {
        let pos = self.buffer[self.start..].iter().position(|&b| b == b'\n')?;
        let end = self.start + pos;
        let mut line = String::from_utf8_lossy(&self.buffer[self.start..end]).into_owned();
        self.start = end + 1;
        if self.start == self.buffer.len() {
            self.start = 0;
            self.buffer.clear();
        }
        if line.ends_with('\r') {
            line.pop();
        }
        Some(line)
    }

    /// A partial (newline-less) line is buffered.
    fn has_pending(&self) -> bool {
        self.start < self.buffer.len()
    }

    /// The partial line, for the tty Ctrl-D delivery path.
    fn take_pending_line(&mut self) -> Option<String> {
        if !self.has_pending() {
            return None;
        }
        let line = String::from_utf8_lossy(&self.buffer[self.start..]).into_owned();
        self.start = 0;
        self.buffer.clear();
        Some(line)
    }
}

fn poll_fd(fd: libc::c_int, timeout_ms: libc::c_int) -> bool {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        let r = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if r < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return false;
        }
        if r == 0 {
            return false;
        }
        return pfd.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tdutils::logging::LogInterface;

    struct Sink {
        lines: Mutex<Vec<(i32, String)>>,
    }

    impl LogInterface for Sink {
        fn do_append(&self, log_level: i32, slice: &str) {
            self.lines
                .lock()
                .unwrap()
                .push((log_level, slice.to_string()));
        }
    }

    static SINK: Sink = Sink {
        lines: Mutex::new(Vec::new()),
    };

    /// The on_cmd preprocessing + dispatch frame, pinned against the C++
    /// observable behavior (empirical tg_cli runs, 2026-09-19): every unknown
    /// non-empty command reports, empty commands don't.
    #[test]
    fn dispatch_frame_unknown_quirk() {
        let guard = TEST_LOCK.lock().unwrap();
        SINK.lines.lock().unwrap().clear();
        logging::set_log_interface(Some(&SINK));

        let client = test_client();
        client.add_cmd("hello".to_string());
        client.add_cmd(String::new());
        while let Some(cmd) = client.cmd_queue.lock().unwrap().pop_front() {
            if client.close_flag.load(Ordering::SeqCst) {
                break;
            }
            client.on_cmd(cmd);
        }
        logging::set_log_interface(None);

        let lines = SINK.lines.lock().unwrap();
        let mut out = Vec::new();
        for (level, line) in lines.iter() {
            if (*level == logging::INFO && line.contains("CMD:["))
                || (*level == logging::ERROR && line.contains("Unknown command"))
            {
                let body = line.rsplit('\t').next().unwrap();
                out.push(body.trim_end_matches('\n').to_string());
            }
        }
        assert_eq!(
            out,
            vec![
                "CMD:[hello]",
                "Unknown command \"hello\" of length 5",
                "CMD:[]",
            ]
        );
        drop(guard);
    }

    /// The M.14.2 census gate: the fixture pins all 1294 command arms of the
    /// C++ `on_cmd` (regenerate with
    /// `conformance/cli_gate/extract_cli_census.sh`); every arm whose slice
    /// has landed must be routed by the dispatcher, every not-yet-ported arm
    /// must still report `Unknown command` exactly once (the 19-block frame).
    /// The `PORTED` ledger grows per M.14 slice, like the M.11 flip census.
    #[test]
    fn on_cmd_routes_the_cpp_census_arms() {
        const CENSUS: &str = include_str!("../testdata/cli_census.txt");
        assert_eq!(
            CENSUS.lines().count(),
            1294,
            "the C++ tree changed — regenerate the census with \
             conformance/cli_gate/extract_cli_census.sh"
        );
        // (op, chain) pairs whose arms are ported so far: M.14.1 (q) +
        // M.14.3 (the whole auth/account chain and the recovery head).
        const PORTED: &[(&str, u32)] = &[
            ("gas", 0),
            ("sap", 0),
            ("sapn", 0),
            ("capp", 0),
            ("sae", 0),
            ("saea", 0),
            ("rac", 0),
            ("sdek", 0),
            ("caec", 0),
            ("cac", 0),
            ("racmg", 0),
            ("ru", 0),
            ("rus", 0),
            ("cap", 0),
            ("cabt", 0),
            ("qr", 0),
            ("gapo", 0),
            ("capk", 0),
            ("cawt", 0),
            ("cqr", 0),
            ("gcs", 0),
            ("raea", 0),
            ("rapr", 0),
            ("caprc", 0),
            ("rap", 0),
            ("lo", 0),
            ("destroy", 0),
            ("reset", 0),
            ("close_td", 0),
            ("DeleteAccountYesIReallyWantToDeleteMyAccount", 0),
            ("gps", 0),
            ("spass", 0),
            ("gpafhttp", 0),
            ("gpaf", 0),
            ("gpafae", 0),
            ("spaf", 0),
            ("gpcl", 0),
            ("seavc", 0),
            ("ceavc", 0),
            ("reavc", 0),
            ("ilear", 0),
            ("slea", 0),
            ("rleac", 0),
            ("cleac", 0),
            ("srea", 0),
            ("grea", 0),
            ("creac", 0),
            ("rreac", 0),
            ("creav", 0),
            ("rpr", 1),
            ("cprc", 1),
            ("rp", 1),
            ("q", 18),
        ];

        let guard = TEST_LOCK.lock().unwrap();
        for line in CENSUS.lines() {
            let Some((op, chain)) = line.split_once(' ') else {
                panic!("malformed census line: {line:?}");
            };
            let chain: u32 = chain.parse().expect("census chain index");
            SINK.lines.lock().unwrap().clear();
            logging::set_log_interface(Some(&SINK));
            let client = test_client();
            client.on_cmd(op.to_string());
            logging::set_log_interface(None);
            let unknown = SINK
                .lines
                .lock()
                .unwrap()
                .iter()
                .filter(|(level, line)| {
                    *level == logging::ERROR && line.contains("Unknown command")
                })
                .count();
            if PORTED.contains(&(op, chain)) {
                assert_eq!(unknown, 0, "{op} (chain {chain}) must be routed");
            } else {
                assert_eq!(
                    unknown, 1,
                    "{op} (chain {chain}) must report unknown until its slice lands"
                );
            }
        }
        drop(guard);
    }

    /// `send_request` without a client reports through the C++ path
    /// (`Failed to send: ` + the `TlStorerToString` format) and returns 0.
    #[test]
    fn send_without_a_client_reports_the_request() {
        let guard = TEST_LOCK.lock().unwrap();
        SINK.lines.lock().unwrap().clear();
        logging::set_log_interface(Some(&SINK));

        let client = test_client();
        let id = client.send_request(Function::getMe(td_api::getMe));
        logging::set_log_interface(None);

        assert_eq!(id, 0);
        let lines = SINK.lines.lock().unwrap();
        let errors: Vec<&str> = lines
            .iter()
            .filter(|(level, _)| *level == logging::ERROR)
            .map(|(_, line)| line.rsplit('\t').next().unwrap())
            .collect();
        assert_eq!(errors, vec!["Failed to send: getMe {\n}\n".to_string()]);
        drop(guard);
    }

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn test_client() -> CliClient {
        CliClient {
            cmd_queue: Mutex::new(VecDeque::new()),
            condvar: Condvar::new(),
            close_flag: AtomicBool::new(false),
            ready_to_stop: AtomicBool::new(false),
            stdin_reader_stopped: AtomicBool::new(false),
            tty: false,
            manager: get_manager_singleton(),
            td_client: Mutex::new(None),
            generations: Mutex::new(HashMap::new()),
            generation: AtomicU64::new(0),
            closed_td: AtomicU64::new(0),
            query_num: AtomicU64::new(0),
            use_test_dc: AtomicBool::new(false),
            get_chat_list: AtomicBool::new(false),
            disable_network: AtomicBool::new(false),
            api_id: AtomicI32::new(0),
            api_hash: Mutex::new(String::new()),
            my_id: AtomicI64::new(0),
            users: Mutex::new(HashMap::new()),
            username_to_user_id: Mutex::new(HashMap::new()),
            username_to_supergroup_id: Mutex::new(HashMap::new()),
            channel_ids: Mutex::new(std::collections::BTreeSet::new()),
            authentication_tokens: Mutex::new(Vec::new()),
            authorization_state: Mutex::new(None),
            get_history_chat_id: AtomicI64::new(0),
            search_chat_id: AtomicI64::new(0),
            get_log_chat_id: AtomicI64::new(0),
            being_downloaded_files: Mutex::new(HashMap::new()),
            pending_file_generations: Mutex::new(Vec::new()),
            saved_messages_topic_id: AtomicI64::new(0),
            direct_messages_chat_topic_id: AtomicI64::new(0),
            forum_topic_id: AtomicI32::new(0),
            message_thread_id: AtomicI64::new(0),
            opened_chat_id: AtomicI64::new(0),
        }
    }
}
