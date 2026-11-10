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

/// `struct SendMessageInfo` (cli.cpp:209) — per-message send timing probe.
/// Tracks the wall-clock instant when `sendMessage` was dispatched
/// (`start_time`), when the server acknowledged it
/// (`updateMessageSendAcknowledged`; `quick_ack_time`), and when the server
/// confirmed delivery (`updateMessageSendSucceeded`; `ack_time`).
#[derive(Clone, Default)]
struct SendMessageInfo {
    start_time: f64,
    quick_ack_time: f64,
    ack_time: f64,
}

impl std::fmt::Display for SendMessageInfo {
    /// `operator<<` (cli.cpp:218) — only the non-zero deltas are printed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.quick_ack_time != 0.0 {
            write!(f, "quick_ack:{} ", self.quick_ack_time - self.start_time)?;
        }
        if self.ack_time != 0.0 {
            write!(f, "ack:{}", self.ack_time - self.start_time)?;
        }
        Ok(())
    }
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
    /// `FlatHashMap<uint64, SendMessageInfo> query_id_to_send_message_info_`
    /// (cli.cpp:245) — keyed by the `send_request` id returned for a plain
    /// `sendMessage`; consulted by the first timing lambda.
    query_id_to_send_message_info: Mutex<HashMap<u64, SendMessageInfo>>,
    /// `FlatHashMap<uint64, SendMessageInfo> message_id_to_send_message_info_`
    /// (cli.cpp:246) — keyed by the server-assigned message id; consulted by
    /// the quick-ack and full-ack lambdas.
    message_id_to_send_message_info: Mutex<HashMap<i64, SendMessageInfo>>,

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
    /// `string business_connection_id_` (empty = the plain request path).
    business_connection_id: Mutex<String>,
    /// `vector<int32> added_sticker_file_ids_`.
    added_sticker_file_ids: Mutex<Vec<i32>>,
    /// `int32 start_timestamp_`.
    #[allow(dead_code)] // readers land with the M.14.5 video arms
    start_timestamp: AtomicI32,
    /// `string thumbnail_` (the `t` option of the media commands).
    #[allow(dead_code)] // readers land with the M.14.5 media arms
    thumbnail: Mutex<String>,
    /// `string cover_`.
    #[allow(dead_code)] // readers land with the M.14.5 media arms
    cover: Mutex<String>,
    /// `string rich_message_media_` (cli.cpp:9171).
    #[allow(dead_code)]
    rich_message_media: Mutex<String>,
    // ---- the send-state block (cli.cpp:9133-9169) — writers land with the
    // option/effect command arms; the send family reads them.
    schedule_date: Mutex<String>,
    #[allow(dead_code)]
    schedule_repeat_period: AtomicI32,
    #[allow(dead_code)]
    paid_message_star_count: AtomicI64,
    #[allow(dead_code)]
    message_effect_id: AtomicI64,
    #[allow(dead_code)]
    only_preview: AtomicBool,
    #[allow(dead_code)]
    has_spoiler: AtomicBool,
    #[allow(dead_code)]
    message_self_destruct_time: AtomicI32,
    #[allow(dead_code)]
    reply_chat_id: AtomicI64,
    #[allow(dead_code)]
    reply_message_id: AtomicI64,
    #[allow(dead_code)]
    reply_quote: Mutex<String>,
    #[allow(dead_code)]
    reply_quote_position: AtomicI32,
    #[allow(dead_code)]
    reply_checklist_task_id: AtomicI32,
    #[allow(dead_code)]
    reply_poll_option_id: Mutex<String>,
    #[allow(dead_code)]
    reply_story_chat_id: AtomicI64,
    #[allow(dead_code)]
    reply_story_id: AtomicI32,
    #[allow(dead_code)]
    link_preview_url: Mutex<String>,
    #[allow(dead_code)]
    link_preview_is_disabled: AtomicBool,
    #[allow(dead_code)]
    link_preview_force_small_media: AtomicBool,
    #[allow(dead_code)]
    link_preview_force_large_media: AtomicBool,
    #[allow(dead_code)]
    link_preview_show_above_text: AtomicBool,
    #[allow(dead_code)]
    show_caption_above_media: AtomicBool,
    #[allow(dead_code)]
    send_welcome_message: AtomicBool,
    #[allow(dead_code)]
    edit_welcome_message_id: AtomicI32,
    #[allow(dead_code)]
    quick_reply_shortcut_name: Mutex<String>,
    #[allow(dead_code)]
    ephemeral_message_receiver_id: AtomicI64,
    #[allow(dead_code)]
    caption: Mutex<String>,
    /// `GiftCollectionId gift_collection_id_` (writer lands with the chain-9
    /// dev arms at cli.cpp:6549).
    gift_collection_id: AtomicI32,
    /// `vector<string> upgraded_gift_attribute_ids_` (writer lands with the
    /// chain-9 dev arms at cli.cpp:6573).
    upgraded_gift_attribute_ids: Mutex<Vec<String>>,
    /// `ChatId reposted_story_chat_id_` / `StoryId reposted_story_id_`
    /// (cli.cpp:9154-9155; the `srsfi` writer is a chain-9 arm).
    reposted_story_chat_id: AtomicI64,
    reposted_story_id: AtomicI32,
    /// `string story_music_` (cli.cpp:9156; the `ssmu` writer feeds
    /// postStory's 1.8.70 music leg).
    story_music: Mutex<String>,
    /// `int32 group_call_source_ = Random::fast(1, 1000000000)` (cli.cpp:9193).
    group_call_source: AtomicI32,
    /// `SuggestedPostPrice suggested_post_price_` — the raw spell
    /// (cli.cpp:9175; the `sspp` writer is a chain-10 arm).
    suggested_post_price: Mutex<String>,
    /// `int32 suggested_post_send_date_ = -1` (cli.cpp:9176).
    suggested_post_send_date: AtomicI32,
    /// The `cpu_counter_` member of the `cpu` dev arm.
    cpu_counter: AtomicU64,
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
        query_id_to_send_message_info: Mutex::new(HashMap::new()),
        message_id_to_send_message_info: Mutex::new(HashMap::new()),
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
        business_connection_id: Mutex::new(String::new()),
        added_sticker_file_ids: Mutex::new(Vec::new()),
        start_timestamp: AtomicI32::new(0),
        thumbnail: Mutex::new(String::new()),
        cover: Mutex::new(String::new()),
        rich_message_media: Mutex::new(String::new()),
        schedule_date: Mutex::new(String::new()),
        schedule_repeat_period: AtomicI32::new(0),
        paid_message_star_count: AtomicI64::new(0),
        message_effect_id: AtomicI64::new(0),
        only_preview: AtomicBool::new(false),
        has_spoiler: AtomicBool::new(false),
        message_self_destruct_time: AtomicI32::new(0),
        reply_chat_id: AtomicI64::new(0),
        reply_message_id: AtomicI64::new(0),
        reply_quote: Mutex::new(String::new()),
        reply_quote_position: AtomicI32::new(0),
        reply_checklist_task_id: AtomicI32::new(0),
        reply_poll_option_id: Mutex::new(String::new()),
        reply_story_chat_id: AtomicI64::new(0),
        reply_story_id: AtomicI32::new(0),
        link_preview_url: Mutex::new(String::new()),
        link_preview_is_disabled: AtomicBool::new(false),
        link_preview_force_small_media: AtomicBool::new(false),
        link_preview_force_large_media: AtomicBool::new(false),
        link_preview_show_above_text: AtomicBool::new(false),
        show_caption_above_media: AtomicBool::new(false),
        send_welcome_message: AtomicBool::new(false),
        edit_welcome_message_id: AtomicI32::new(0),
        quick_reply_shortcut_name: Mutex::new(String::new()),
        ephemeral_message_receiver_id: AtomicI64::new(0),
        caption: Mutex::new(String::new()),
        gift_collection_id: AtomicI32::new(0),
        upgraded_gift_attribute_ids: Mutex::new(Vec::new()),
        reposted_story_chat_id: AtomicI64::new(0),
        reposted_story_id: AtomicI32::new(0),
        story_music: Mutex::new(String::new()),
        group_call_source: AtomicI32::new(random::fast(1, 1000000000)),
        suggested_post_price: Mutex::new(String::new()),
        suggested_post_send_date: AtomicI32::new(-1),
        cpu_counter: AtomicU64::new(0),
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

/// `CliClient::as_limit` (cli.cpp:677).
fn as_limit(s: &str, default_limit: i32) -> i32 {
    if s.is_empty() {
        return default_limit;
    }
    tokenizer::to_integer_i32(trim_td(s))
}

/// `CliClient::as_bool` (cli.cpp:976).
fn as_bool(s: &str) -> bool {
    let s = to_lower(trim_td(s));
    s == "true" || s == "1"
}

/// `CliClient::as_chat_folder_id` (cli.cpp:539).
fn as_chat_folder_id(s: &str) -> i32 {
    tokenizer::to_integer_i32(trim_td(s))
}

/// `CliClient::as_chat_list` (cli.cpp:547) — the trailing `a` picks the
/// archive, a `-folder` suffix the folder, the default is the main list.
fn as_chat_list(op: &str) -> Option<td_api::ChatList> {
    if op.ends_with('a') {
        return Some(td_api::ChatList::chatListArchive(td_api::chatListArchive));
    }
    if let Some(pos) = op.find('-') {
        return Some(td_api::ChatList::chatListFolder(td_api::chatListFolder {
            chat_folder_id: as_chat_folder_id(&op[pos + 1..]),
        }));
    }
    Some(td_api::ChatList::chatListMain(td_api::chatListMain))
}

/// `CliClient::as_message_id` (cli.cpp:581) — the trailing `s` marks a
/// server message id (`to_integer` stops at it, then the `<< 20` shift).
fn as_message_id(s: &str) -> i64 {
    let s = trim_td(s);
    let id = tokenizer::to_integer_i64(s);
    if s.ends_with('s') {
        return id << 20;
    }
    id
}

/// `CliClient::as_reaction_type` (cli.cpp:945) — empty → none, `$` → paid,
/// a strict integer → custom emoji, else the emoji itself.
fn as_reaction_type(t: &str) -> Option<td_api::ReactionType> {
    let t = trim_td(t);
    if t.is_empty() {
        return None;
    }
    if t == "$" {
        return Some(td_api::ReactionType::reactionTypePaid(
            td_api::reactionTypePaid,
        ));
    }
    if let Ok(custom_emoji_id) = t.parse::<i64>() {
        return Some(td_api::ReactionType::reactionTypeCustomEmoji(
            td_api::reactionTypeCustomEmoji { custom_emoji_id },
        ));
    }
    Some(td_api::ReactionType::reactionTypeEmoji(
        td_api::reactionTypeEmoji {
            emoji: t.to_owned(),
        },
    ))
}

/// The `CliClient::SearchQuery` get_args (cli.cpp:1001): one space split on
/// the trimmed input; a positive strict integer is the limit, otherwise the
/// whole input is the query with the default limit.
fn get_search_query(args: &str) -> (i32, String) {
    let trimmed = trim_td(args);
    match trimmed.split_once(' ') {
        Some((limit, query)) => {
            if let Ok(limit) = limit.parse::<i32>()
                && limit > 0
            {
                (limit, query.to_owned())
            } else {
                (10, args.to_owned())
            }
        }
        None => match trimmed.parse::<i32>() {
            Ok(limit) if limit > 0 => (limit, String::new()),
            _ => (10, args.to_owned()),
        },
    }
}

/// `as_language_pack_info` (cli.cpp:2861) — the dev language-pack fixture.
#[allow(clippy::too_many_arguments)] // wait — no fields; keep the shape
fn as_language_pack_info(
    language_code: &str,
    name: &str,
    native_name: &str,
) -> td_api::LanguagePackInfo {
    td_api::LanguagePackInfo {
        id: language_code.to_owned(),
        base_language_pack_id: "test".to_owned(),
        name: name.to_owned(),
        native_name: native_name.to_owned(),
        plural_code: "en".to_owned(),
        is_official: true,
        is_rtl: true,
        is_beta: true,
        is_installed: true,
        total_string_count: -1,
        translated_string_count: 5,
        local_string_count: 3,
        translation_url: "abacaba".to_owned(),
    }
}

/// `is_base64url` (cli.cpp) — the URL-safe alphabet only.
fn is_base64url(s: &str) -> bool {
    s.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// `CliClient::as_input_file` (cli.cpp:790) — the remote/local/generated/id
/// spell resolution.
fn as_input_file(s: &str) -> Option<td_api::InputFile> {
    let s = trim_td(s);
    if (s.len() >= 20 && is_base64url(s)) || s.starts_with("http") {
        return Some(td_api::InputFile::inputFileRemote(
            td_api::inputFileRemote { id: s.to_owned() },
        ));
    }
    if let Ok(file_id) = s.parse::<i32>() {
        return Some(td_api::InputFile::inputFileId(td_api::inputFileId {
            id: file_id,
        }));
    }
    if let Some((original_path, conversion)) = s.split_once(';') {
        return Some(td_api::InputFile::inputFileGenerated(
            td_api::inputFileGenerated {
                original_path: original_path.to_owned(),
                conversion: conversion.to_owned(),
                expected_size: 0,
            },
        ));
    }
    Some(td_api::InputFile::inputFileLocal(td_api::inputFileLocal {
        path: s.to_owned(),
    }))
}

/// `as_sticker_type` (cli.cpp:658) — the trailing `e`/`m` picks the type,
/// otherwise a coin flip decides plain-regular vs the null (unset) type.
fn as_sticker_type(op: &str) -> Option<td_api::StickerType> {
    if op.ends_with('e') {
        return Some(td_api::StickerType::stickerTypeCustomEmoji(
            td_api::stickerTypeCustomEmoji,
        ));
    }
    if op.ends_with('m') {
        return Some(td_api::StickerType::stickerTypeMask(
            td_api::stickerTypeMask,
        ));
    }
    if random::fast_bool() {
        None
    } else {
        Some(td_api::StickerType::stickerTypeRegular(
            td_api::stickerTypeRegular,
        ))
    }
}

/// `as_block_list` (cli.cpp:567).
fn as_block_list(s: &str) -> Option<td_api::BlockList> {
    if s.is_empty() {
        return None;
    }
    if s.ends_with('s') {
        return Some(td_api::BlockList::blockListStories(
            td_api::blockListStories,
        ));
    }
    Some(td_api::BlockList::blockListMain(td_api::blockListMain))
}

/// `as_background_fill` overloads (cli.cpp:2901-2911); the two-color
/// gradient rotates by `Random::fast(1, 7) * 45` degrees.
fn background_fill_solid(color: i32) -> Option<td_api::BackgroundFill> {
    Some(td_api::BackgroundFill::backgroundFillSolid(
        td_api::backgroundFillSolid { color },
    ))
}

fn background_fill_gradient(top_color: i32, bottom_color: i32) -> Option<td_api::BackgroundFill> {
    Some(td_api::BackgroundFill::backgroundFillGradient(
        td_api::backgroundFillGradient {
            top_color,
            bottom_color,
            rotation_angle: random::fast(1, 7) * 45,
        },
    ))
}

fn background_fill_freeform(colors: Vec<i32>) -> Option<td_api::BackgroundFill> {
    Some(td_api::BackgroundFill::backgroundFillFreeformGradient(
        td_api::backgroundFillFreeformGradient { colors },
    ))
}

fn as_wallpaper_background(is_blurred: bool, is_moving: bool) -> td_api::BackgroundType {
    td_api::BackgroundType::backgroundTypeWallpaper(td_api::backgroundTypeWallpaper {
        is_blurred,
        is_moving,
    })
}

fn as_gradient_pattern_background(
    top_color: i32,
    bottom_color: i32,
    intensity: i32,
    is_inverted: bool,
    is_moving: bool,
) -> td_api::BackgroundType {
    td_api::BackgroundType::backgroundTypePattern(td_api::backgroundTypePattern {
        fill: background_fill_gradient(top_color, bottom_color),
        intensity,
        is_inverted,
        is_moving,
    })
}

fn as_solid_pattern_background(
    color: i32,
    intensity: i32,
    is_moving: bool,
) -> td_api::BackgroundType {
    as_gradient_pattern_background(color, color, intensity, false, is_moving)
}

fn as_freeform_gradient_pattern_background(
    colors: Vec<i32>,
    intensity: i32,
    is_inverted: bool,
    is_moving: bool,
) -> td_api::BackgroundType {
    td_api::BackgroundType::backgroundTypePattern(td_api::backgroundTypePattern {
        fill: background_fill_freeform(colors),
        intensity,
        is_inverted,
        is_moving,
    })
}

fn as_solid_background(color: i32) -> td_api::BackgroundType {
    td_api::BackgroundType::backgroundTypeFill(td_api::backgroundTypeFill {
        fill: background_fill_solid(color),
    })
}

fn as_gradient_background(top_color: i32, bottom_color: i32) -> td_api::BackgroundType {
    td_api::BackgroundType::backgroundTypeFill(td_api::backgroundTypeFill {
        fill: background_fill_gradient(top_color, bottom_color),
    })
}

fn as_freeform_gradient_background(colors: Vec<i32>) -> td_api::BackgroundType {
    td_api::BackgroundType::backgroundTypeFill(td_api::backgroundTypeFill {
        fill: background_fill_freeform(colors),
    })
}

fn as_chat_theme_background(theme_name: &str) -> td_api::BackgroundType {
    td_api::BackgroundType::backgroundTypeChatTheme(td_api::backgroundTypeChatTheme {
        theme_name: theme_name.to_owned(),
    })
}

/// `as_theme_parameters` (cli.cpp:2891) — the fixed probe theme.
fn as_theme_parameters() -> td_api::ThemeParameters {
    td_api::ThemeParameters {
        background_color: 0,
        secondary_background_color: 1,
        header_background_color: -1,
        bottom_bar_background_color: 123,
        section_background_color: 256,
        section_separator_color: 65536,
        text_color: 123456789,
        accent_text_color: 65535,
        section_header_text_color: 5,
        subtitle_text_color: 55,
        destructive_text_color: 555,
        hint_color: 5555,
        link_color: 55555,
        button_color: 555555,
        button_text_color: 123,
    }
}

/// `CliClient::GiftResalePrice` (cli.cpp:1578) — `g<N>` grams / `s<N>` stars.
fn as_gift_resale_price(price: &str) -> Option<td_api::GiftResalePrice> {
    let rest = price.strip_prefix(['g', 's'])?;
    let amount = tokenizer::to_integer_i64(rest);
    if price.starts_with('g') {
        return Some(td_api::GiftResalePrice::giftResalePriceGram(
            td_api::giftResalePriceGram {
                gram_cent_count: amount,
            },
        ));
    }
    Some(td_api::GiftResalePrice::giftResalePriceStar(
        td_api::giftResalePriceStar { star_count: amount },
    ))
}

/// `as_gift_collection_id` (cli.cpp:634).
fn as_gift_collection_id(s: &str) -> i32 {
    tokenizer::to_integer_i32(trim_td(s))
}

/// `as_gift_collection_ids` (cli.cpp:639).
fn as_gift_collection_ids(s: &str) -> Vec<i32> {
    autosplit(s)
        .iter()
        .map(|t| as_gift_collection_id(t))
        .collect()
}

/// `as_user_privacy_setting` (cli.cpp:2365).
fn as_user_privacy_setting(setting: &str) -> Option<td_api::UserPrivacySetting> {
    use td_api::UserPrivacySetting as Ups;
    let setting = to_lower(trim_td(setting));
    match setting.as_str() {
        "invite" => Some(Ups::userPrivacySettingAllowChatInvites(
            td_api::userPrivacySettingAllowChatInvites,
        )),
        "status" => Some(Ups::userPrivacySettingShowStatus(
            td_api::userPrivacySettingShowStatus,
        )),
        "call" => Some(Ups::userPrivacySettingAllowCalls(
            td_api::userPrivacySettingAllowCalls,
        )),
        "p2p" => Some(Ups::userPrivacySettingAllowPeerToPeerCalls(
            td_api::userPrivacySettingAllowPeerToPeerCalls,
        )),
        "forward" => Some(Ups::userPrivacySettingShowLinkInForwardedMessages(
            td_api::userPrivacySettingShowLinkInForwardedMessages,
        )),
        "photo" => Some(Ups::userPrivacySettingShowProfilePhoto(
            td_api::userPrivacySettingShowProfilePhoto,
        )),
        "phone_number" => Some(Ups::userPrivacySettingShowPhoneNumber(
            td_api::userPrivacySettingShowPhoneNumber,
        )),
        "bio" => Some(Ups::userPrivacySettingShowBio(
            td_api::userPrivacySettingShowBio,
        )),
        "find" => Some(Ups::userPrivacySettingAllowFindingByPhoneNumber(
            td_api::userPrivacySettingAllowFindingByPhoneNumber,
        )),
        "birth" => Some(Ups::userPrivacySettingShowBirthdate(
            td_api::userPrivacySettingShowBirthdate,
        )),
        "gift" => Some(Ups::userPrivacySettingAutosaveGifts(
            td_api::userPrivacySettingAutosaveGifts,
        )),
        "music" => Some(Ups::userPrivacySettingShowProfileAudio(
            td_api::userPrivacySettingShowProfileAudio,
        )),
        _ => None,
    }
}

/// The `CliClient::PrivacyRules` conversion (cli.cpp:1717) — a char spell:
/// `a`/`A`/`c`/`C` are the all/contacts rules, `u`/`U`/`m`/`M` take the
/// integer parsed from the rest of the spell, digits and `-` are skipped,
/// anything else stops the scan with an error.
fn as_user_privacy_setting_rules(args: &str) -> td_api::UserPrivacySettingRules {
    use td_api::UserPrivacySettingRule as Rule;
    let rules_str = {
        let s = trim_td(args);
        if s.is_empty() {
            "a".to_owned()
        } else {
            s.to_owned()
        }
    };
    let mut rules = Vec::new();
    for i in 0..rules_str.len() {
        let ch = rules_str.as_bytes()[i];
        let arg = vec![tokenizer::to_integer_i64(&rules_str[i + 1..])];
        match ch {
            b'a' => rules.push(Rule::userPrivacySettingRuleAllowAll(
                td_api::userPrivacySettingRuleAllowAll,
            )),
            b'A' => rules.push(Rule::userPrivacySettingRuleRestrictAll(
                td_api::userPrivacySettingRuleRestrictAll,
            )),
            b'c' => rules.push(Rule::userPrivacySettingRuleAllowContacts(
                td_api::userPrivacySettingRuleAllowContacts,
            )),
            b'C' => rules.push(Rule::userPrivacySettingRuleRestrictContacts(
                td_api::userPrivacySettingRuleRestrictContacts,
            )),
            b'u' => rules.push(Rule::userPrivacySettingRuleAllowUsers(
                td_api::userPrivacySettingRuleAllowUsers { user_ids: arg },
            )),
            b'U' => rules.push(Rule::userPrivacySettingRuleRestrictUsers(
                td_api::userPrivacySettingRuleRestrictUsers { user_ids: arg },
            )),
            b'm' => rules.push(Rule::userPrivacySettingRuleAllowChatMembers(
                td_api::userPrivacySettingRuleAllowChatMembers { chat_ids: arg },
            )),
            b'M' => rules.push(Rule::userPrivacySettingRuleRestrictChatMembers(
                td_api::userPrivacySettingRuleRestrictChatMembers { chat_ids: arg },
            )),
            c if !c.is_ascii_digit() && c != b'-' => {
                tdutils::log_error!(
                    "Invalid character {} in privacy rules {rules_str}",
                    c as char
                );
                break;
            }
            _ => {}
        }
    }
    td_api::UserPrivacySettingRules { rules }
}

/// `as_input_passport_element` (cli.cpp:2790) — the fixed probe documents;
/// `spes` moves the last file to the selfie slot.
fn as_input_passport_element(
    passport_element_type: &str,
    arg: &str,
    with_selfie: bool,
) -> Option<td_api::InputPassportElement> {
    use td_api::InputPassportElement as Ipe;
    let mut input_files = Vec::new();
    let mut selfie = None;
    if !arg.is_empty() {
        let mut files = autosplit(arg);
        assert!(!files.is_empty());
        if with_selfie {
            selfie = as_input_file(files.pop().unwrap().as_str());
        }
        for file in &files {
            // C++ pushes a possibly-null element; the Rust vector model
            // can't hold one, so it is dropped (the S76 decision).
            if let Some(file) = as_input_file(file) {
                input_files.push(file);
            }
        }
    }
    if passport_element_type == "address" || passport_element_type == "a" {
        return Some(Ipe::inputPassportElementAddress(
            td_api::inputPassportElementAddress {
                address: Some(td_api::Address {
                    country_code: "US".to_owned(),
                    state: "CA".to_owned(),
                    city: "Los Angeles".to_owned(),
                    street_line1: "Washington".to_owned(),
                    street_line2: String::new(),
                    postal_code: "90001".to_owned(),
                }),
            },
        ));
    }
    if passport_element_type == "email" || passport_element_type == "e" {
        return Some(Ipe::inputPassportElementEmailAddress(
            td_api::inputPassportElementEmailAddress {
                email_address: arg.to_owned(),
            },
        ));
    }
    if passport_element_type == "phone" || passport_element_type == "p" {
        return Some(Ipe::inputPassportElementPhoneNumber(
            td_api::inputPassportElementPhoneNumber {
                phone_number: arg.to_owned(),
            },
        ));
    }
    if passport_element_type == "pd" {
        return Some(Ipe::inputPassportElementPersonalDetails(
            td_api::inputPassportElementPersonalDetails {
                personal_details: Some(td_api::PersonalDetails {
                    first_name: "Mike".to_owned(),
                    middle_name: "Jr".to_owned(),
                    last_name: "Towers".to_owned(),
                    native_first_name: "Mike\u{2708}".to_owned(),
                    native_middle_name: "Jr\u{26fd}".to_owned(),
                    native_last_name: "Towers\u{2757}".to_owned(),
                    birthdate: Some(td_api::Date {
                        day: 29,
                        month: 2,
                        year: 2000,
                    }),
                    gender: "male".to_owned(),
                    country_code: "US".to_owned(),
                    residence_country_code: "GB".to_owned(),
                }),
            },
        ));
    }
    if passport_element_type == "driver_license" || passport_element_type == "dl" {
        if input_files.len() >= 2 {
            let front_side = input_files.remove(0);
            let reverse_side = input_files.remove(0);
            return Some(Ipe::inputPassportElementDriverLicense(
                td_api::inputPassportElementDriverLicense {
                    driver_license: Some(td_api::InputIdentityDocument {
                        number: "1234567890".to_owned(),
                        expiration_date: Some(td_api::Date {
                            day: 1,
                            month: 3,
                            year: 2029,
                        }),
                        front_side: Some(front_side),
                        reverse_side: Some(reverse_side),
                        selfie,
                        translation: input_files,
                    }),
                },
            ));
        }
    } else if passport_element_type == "identity_card" || passport_element_type == "ic" {
        if input_files.len() >= 2 {
            let front_side = input_files.remove(0);
            let reverse_side = input_files.remove(0);
            return Some(Ipe::inputPassportElementIdentityCard(
                td_api::inputPassportElementIdentityCard {
                    identity_card: Some(td_api::InputIdentityDocument {
                        number: "1234567890".to_owned(),
                        expiration_date: None,
                        front_side: Some(front_side),
                        reverse_side: Some(reverse_side),
                        selfie,
                        translation: input_files,
                    }),
                },
            ));
        }
    } else if passport_element_type == "internal_passport" || passport_element_type == "ip" {
        if !input_files.is_empty() {
            let front_side = input_files.remove(0);
            return Some(Ipe::inputPassportElementInternalPassport(
                td_api::inputPassportElementInternalPassport {
                    internal_passport: Some(td_api::InputIdentityDocument {
                        number: "1234567890".to_owned(),
                        expiration_date: None,
                        front_side: Some(front_side),
                        reverse_side: None,
                        selfie,
                        translation: input_files,
                    }),
                },
            ));
        }
    } else if passport_element_type == "rental_agreement" || passport_element_type == "ra" {
        let mut translation = Vec::new();
        if let Some(selfie) = selfie {
            translation.push(selfie);
        }
        return Some(Ipe::inputPassportElementRentalAgreement(
            td_api::inputPassportElementRentalAgreement {
                rental_agreement: Some(td_api::InputPersonalDocument {
                    files: input_files,
                    translation,
                }),
            },
        ));
    }
    tdutils::log_error!("Unsupported passport element type {passport_element_type}");
    None
}

/// The parsed `CliClient::InputBackground` get_args (cli.cpp:1617).
#[derive(Default)]
enum ParsedInputBackground {
    #[default]
    None,
    File(String),
    Id(i64),
    Message(i64),
}

fn parse_input_background(args: &str) -> ParsedInputBackground {
    let args = trim_td(args);
    if args.is_empty() {
        return ParsedInputBackground::None;
    }
    if args.parse::<i64>().is_ok() {
        return ParsedInputBackground::Id(tokenizer::to_integer_i64(args));
    }
    if args.ends_with('s') && args[..args.len() - 1].parse::<i32>().is_ok() {
        return ParsedInputBackground::Message(as_message_id(args));
    }
    ParsedInputBackground::File(args.to_owned())
}

/// The `InputBackground` conversion (cli.cpp:1603).
fn input_background(parsed: ParsedInputBackground) -> Option<td_api::InputBackground> {
    match parsed {
        ParsedInputBackground::File(file) => Some(td_api::InputBackground::inputBackgroundLocal(
            td_api::inputBackgroundLocal {
                background: as_input_file(&file),
            },
        )),
        ParsedInputBackground::Id(id) if id != 0 => {
            Some(td_api::InputBackground::inputBackgroundRemote(
                td_api::inputBackgroundRemote { background_id: id },
            ))
        }
        ParsedInputBackground::Message(message_id) if message_id != 0 => {
            Some(td_api::InputBackground::inputBackgroundPrevious(
                td_api::inputBackgroundPrevious { message_id },
            ))
        }
        _ => None,
    }
}

/// `CliClient::as_story_list` (cli.cpp:557) — the trailing `a` picks the
/// archive list, `e` (and empty) the null default.
fn as_story_list(story_list: &str) -> Option<td_api::StoryList> {
    match story_list.as_bytes().last() {
        Some(b'e') | None => None,
        Some(b'a') => Some(td_api::StoryList::storyListArchive(
            td_api::storyListArchive,
        )),
        Some(_) => Some(td_api::StoryList::storyListMain(td_api::storyListMain)),
    }
}

/// `CliClient::as_story_id` (cli.cpp:619).
fn as_story_id(s: &str) -> i32 {
    tokenizer::to_integer_i32(trim_td(s))
}

/// `CliClient::as_forum_topic_id` (cli.cpp:604).
fn as_forum_topic_id(s: &str) -> i32 {
    tokenizer::to_integer_i32(s)
}

/// `CliClient::as_message_ids` (cli.cpp:589).
fn as_message_ids(s: &str) -> Vec<i64> {
    autosplit(s).iter().map(|t| as_message_id(t)).collect()
}

/// The `CliClient::SuggestedPostPrice` conversion (cli.cpp:1560) — `g<N>`
/// grams, `s<N>` stars, anything else (including empty) the null price.
fn suggested_post_price(price: &str) -> Option<td_api::SuggestedPostPrice> {
    match price.as_bytes().first() {
        Some(b'g') => Some(td_api::SuggestedPostPrice::suggestedPostPriceGram(
            td_api::suggestedPostPriceGram {
                gram_cent_count: tokenizer::to_integer_i64(&price[1..]),
            },
        )),
        Some(b's') => Some(td_api::SuggestedPostPrice::suggestedPostPriceStar(
            td_api::suggestedPostPriceStar {
                star_count: tokenizer::to_integer_i64(&price[1..]),
            },
        )),
        _ => None,
    }
}

/// `CliClient::as_file_ids` (cli.cpp:768).
fn as_file_ids(s: &str) -> Vec<i32> {
    autosplit(s)
        .iter()
        .map(|t| tokenizer::to_integer_i32(trim_td(t)))
        .collect()
}

/// `as_notification_settings_scope` (cli.cpp:2353).
fn as_notification_settings_scope(scope: &str) -> Option<td_api::NotificationSettingsScope> {
    if scope.is_empty() {
        return None;
    }
    if scope == "channels" || scope == "ch" {
        return Some(
            td_api::NotificationSettingsScope::notificationSettingsScopeChannelChats(
                td_api::notificationSettingsScopeChannelChats,
            ),
        );
    }
    if scope == "chats" || scope == "groups" || as_bool(scope) {
        return Some(
            td_api::NotificationSettingsScope::notificationSettingsScopeGroupChats(
                td_api::notificationSettingsScopeGroupChats,
            ),
        );
    }
    Some(
        td_api::NotificationSettingsScope::notificationSettingsScopePrivateChats(
            td_api::notificationSettingsScopePrivateChats,
        ),
    )
}

/// The `CliClient::ReactionNotificationSource` conversion (cli.cpp:1696).
fn reaction_notification_source(source: &str) -> Option<td_api::ReactionNotificationSource> {
    match source {
        "none" | "n" => Some(
            td_api::ReactionNotificationSource::reactionNotificationSourceNone(
                td_api::reactionNotificationSourceNone,
            ),
        ),
        "contacts" | "c" => Some(
            td_api::ReactionNotificationSource::reactionNotificationSourceContacts(
                td_api::reactionNotificationSourceContacts,
            ),
        ),
        "all" | "a" => Some(
            td_api::ReactionNotificationSource::reactionNotificationSourceAll(
                td_api::reactionNotificationSourceAll,
            ),
        ),
        _ => None,
    }
}

/// `as_report_reason` (cli.cpp:2651).
fn as_report_reason(reason: &str) -> Option<td_api::ReportReason> {
    let reason = trim_td(reason);
    use td_api::ReportReason as R;
    match reason {
        "null" => None,
        "spam" => Some(R::reportReasonSpam(td_api::reportReasonSpam)),
        "violence" => Some(R::reportReasonViolence(td_api::reportReasonViolence)),
        "porno" => Some(R::reportReasonPornography(td_api::reportReasonPornography)),
        "ca" => Some(R::reportReasonChildAbuse(td_api::reportReasonChildAbuse)),
        "copyright" => Some(R::reportReasonCopyright(td_api::reportReasonCopyright)),
        "geo" | "location" => Some(R::reportReasonUnrelatedLocation(
            td_api::reportReasonUnrelatedLocation,
        )),
        "fake" => Some(R::reportReasonFake(td_api::reportReasonFake)),
        "drugs" => Some(R::reportReasonIllegalDrugs(
            td_api::reportReasonIllegalDrugs,
        )),
        "pd" => Some(R::reportReasonPersonalDetails(
            td_api::reportReasonPersonalDetails,
        )),
        _ => Some(R::reportReasonCustom(td_api::reportReasonCustom)),
    }
}

/// `as_proxy_id` (cli.cpp:937).
fn as_proxy_id(s: &str) -> i32 {
    tokenizer::to_integer_i32(trim_td(s))
}

/// `td::to_integers<int32>` over the CLI autosplit.
fn to_integers_i32(s: &str) -> Vec<i32> {
    autosplit(s)
        .iter()
        .map(|t| tokenizer::to_integer_i32(t))
        .collect()
}

/// The parsed `CliClient::StoryPrivacySettings` get_args (cli.cpp:1771).
struct ParsedStoryPrivacySettings {
    settings: String,
    user_ids: Vec<i64>,
}

/// The `StoryPrivacySettings` conversion (cli.cpp:1774).
fn story_privacy_settings(parsed: ParsedStoryPrivacySettings) -> td_api::StoryPrivacySettings {
    use td_api::StoryPrivacySettings as Sps;
    if parsed.settings == "f" || parsed.settings == "cf" {
        return Sps::storyPrivacySettingsCloseFriends(td_api::storyPrivacySettingsCloseFriends);
    }
    if let Some(first) = parsed.settings.as_bytes().first() {
        match first {
            b'a' | b'e' => {
                return Sps::storyPrivacySettingsEveryone(td_api::storyPrivacySettingsEveryone {
                    except_user_ids: parsed.user_ids,
                });
            }
            b'c' => {
                return Sps::storyPrivacySettingsContacts(td_api::storyPrivacySettingsContacts {
                    except_user_ids: parsed.user_ids,
                });
            }
            b'u' => {
                return Sps::storyPrivacySettingsSelectedUsers(
                    td_api::storyPrivacySettingsSelectedUsers {
                        user_ids: parsed.user_ids,
                    },
                );
            }
            _ => {}
        }
    }
    Sps::storyPrivacySettingsContacts(td_api::storyPrivacySettingsContacts {
        except_user_ids: Vec::new(),
    })
}

/// `get_args(args, StoryPrivacySettings)` (cli.cpp:1785) — the settings
/// spell minus its first character feeds `as_user_ids`.
fn as_story_privacy_settings(settings: &str, user_ids: Vec<i64>) -> td_api::StoryPrivacySettings {
    story_privacy_settings(ParsedStoryPrivacySettings {
        settings: settings.to_owned(),
        user_ids,
    })
}

/// The `CliClient::InputStoryAreas` conversion (cli.cpp:1788) — an empty
/// spell yields the null areas; every area position is randomized exactly
/// like the C++ probe.
fn get_input_story_areas(areas: &str) -> Option<td_api::InputStoryAreas> {
    let areas = trim_td(areas);
    if areas.is_empty() {
        return None;
    }
    let mut result_areas = Vec::new();
    for area in tokenizer::full_split(areas, ';') {
        if area.is_empty() {
            continue;
        }
        let position = td_api::AreaPosition {
            x_percentage: f64::from(random::fast(1, 99)),
            y_percentage: f64::from(random::fast(1, 99)),
            width_percentage: f64::from(random::fast(1, 99)),
            height_percentage: f64::from(random::fast(1, 99)),
            rotation_angle: f64::from(random::fast(0, 360)),
            corner_radius_percentage: f64::from(random::fast(1, 19)),
        };
        let first = area.as_bytes()[0];
        let rest = &area[1..];
        let type_ = if area == "l" {
            Some(td_api::InputStoryAreaType::inputStoryAreaTypeLocation(
                td_api::inputStoryAreaTypeLocation {
                    location: Some(td_api::Location {
                        latitude: f64::from(random::fast(-50, 50)),
                        longitude: f64::from(random::fast(-50, 50)),
                        horizontal_accuracy: 0.0,
                    }),
                    address: Some(td_api::LocationAddress {
                        country_code: "US".to_owned(),
                        state: "ZZ".to_owned(),
                        city: "Deniles".to_owned(),
                        street: "Road".to_owned(),
                    }),
                },
            ))
        } else if first == b'p' {
            // 1.8.70 dropped the 'v' found-venue form with the scheme
            // type; 'p' (previous venue) is the only venue input left.
            let (venue_provider, venue_id) = tokenizer::split(rest, ':');
            Some(td_api::InputStoryAreaType::inputStoryAreaTypePreviousVenue(
                td_api::inputStoryAreaTypePreviousVenue {
                    venue_provider,
                    venue_id,
                },
            ))
        } else if first == b'r' {
            Some(
                td_api::InputStoryAreaType::inputStoryAreaTypeSuggestedReaction(
                    td_api::inputStoryAreaTypeSuggestedReaction {
                        reaction_type: as_reaction_type(rest),
                        is_dark: random::fast_bool(),
                        is_flipped: random::fast_bool(),
                    },
                ),
            )
        } else if first == b'm' {
            let (chat_id, message_id) = tokenizer::split(rest, ':');
            Some(td_api::InputStoryAreaType::inputStoryAreaTypeMessage(
                td_api::inputStoryAreaTypeMessage {
                    chat_id: tokenizer::to_integer_i64(&chat_id),
                    message_id: as_message_id(&message_id),
                },
            ))
        } else if first == b'u' {
            Some(td_api::InputStoryAreaType::inputStoryAreaTypeLink(
                td_api::inputStoryAreaTypeLink {
                    url: rest.to_owned(),
                },
            ))
        } else if first == b'w' {
            Some(td_api::InputStoryAreaType::inputStoryAreaTypeWeather(
                td_api::inputStoryAreaTypeWeather {
                    temperature: 20.1,
                    emoji: "\u{2600}\u{FE0F}".to_owned(),
                    background_color: tokenizer::to_integer_i32(rest),
                },
            ))
        } else if first == b'g' {
            Some(td_api::InputStoryAreaType::inputStoryAreaTypeUpgradedGift(
                td_api::inputStoryAreaTypeUpgradedGift {
                    gift_name: rest.to_owned(),
                },
            ))
        } else {
            None
        };
        result_areas.push(td_api::InputStoryArea {
            position: Some(position),
            r#type: type_,
        });
    }
    Some(td_api::InputStoryAreas {
        areas: result_areas,
    })
}

/// The parsed `CliClient::BackgroundType` get_args (cli.cpp:1674).
#[derive(Default)]
struct ParsedBackgroundType {
    kind: u8, // 0 null, 1 wallpaper, 2 solid pattern, 3 gradient pattern,
    // 4 freeform gradient pattern, 5 fill, 6 chat theme
    colors: Vec<i32>,
    theme_name: String,
}

fn parse_background_type(args: &str) -> ParsedBackgroundType {
    let args = trim_td(args);
    let mut parsed = ParsedBackgroundType::default();
    if args.is_empty() {
        return parsed;
    }
    if args == "w" {
        parsed.kind = 1;
    } else if args == "sp" {
        parsed.kind = 2;
    } else if args == "gp" {
        parsed.kind = 3;
    } else if args == "fgp" {
        parsed.kind = 4;
    } else if args.starts_with('t') {
        parsed.kind = 6;
        parsed.theme_name = args.strip_prefix('t').unwrap_or("").to_owned();
    } else {
        parsed.kind = 5;
        parsed.colors = autosplit(args)
            .iter()
            .map(|c| tokenizer::to_integer_i32(trim_td(c)))
            .collect();
    }
    parsed
}

/// The `BackgroundType` conversion (cli.cpp:1645).
fn background_type(parsed: ParsedBackgroundType) -> Option<td_api::BackgroundType> {
    match parsed.kind {
        1 => Some(as_wallpaper_background(
            random::fast_bool(),
            random::fast_bool(),
        )),
        2 => Some(as_solid_pattern_background(0xabcdef, 49, true)),
        3 => Some(as_gradient_pattern_background(
            0xABCDEF,
            0xFE,
            51,
            random::fast_bool(),
            false,
        )),
        4 => Some(as_freeform_gradient_pattern_background(
            vec![0xABCDEF, 0xFE, 0xFF0000],
            52,
            random::fast_bool(),
            random::fast_bool(),
        )),
        5 => match parsed.colors.len() {
            1 => Some(as_solid_background(parsed.colors[0])),
            2 => Some(as_gradient_background(parsed.colors[0], parsed.colors[1])),
            _ => Some(as_freeform_gradient_background(parsed.colors)),
        },
        6 => Some(as_chat_theme_background(&parsed.theme_name)),
        _ => None,
    }
}

/// `as_input_file_id` (cli.cpp:772).
fn as_input_file_id(s: &str) -> Option<td_api::InputFile> {
    Some(td_api::InputFile::inputFileId(td_api::inputFileId {
        id: tokenizer::to_integer_i32(trim_td(s)),
    }))
}

/// `as_location` (cli.cpp:930).
fn as_location(latitude: &str, longitude: &str, accuracy: &str) -> Option<td_api::Location> {
    if trim_td(latitude).is_empty() && trim_td(longitude).is_empty() {
        return None;
    }
    Some(td_api::Location {
        latitude: latitude.trim().parse().unwrap_or(0.0),
        longitude: longitude.trim().parse().unwrap_or(0.0),
        horizontal_accuracy: accuracy.trim().parse().unwrap_or(0.0),
    })
}

/// `as_chat_administrator_rights` (cli.cpp:2563) — the 18-flag rights pack;
/// callers land with the chain-13 member-status arms.
#[allow(dead_code, clippy::too_many_arguments)]
fn as_chat_administrator_rights(
    can_manage_chat: bool,
    can_change_info: bool,
    can_post_messages: bool,
    can_edit_messages: bool,
    can_delete_messages: bool,
    can_invite_users: bool,
    can_restrict_members: bool,
    can_pin_messages: bool,
    can_manage_topics: bool,
    can_promote_members: bool,
    can_manage_video_chats: bool,
    can_post_stories: bool,
    can_edit_stories: bool,
    can_delete_stories: bool,
    can_manage_direct_messages: bool,
    can_manage_tags: bool,
    can_send_welcome_messages: bool,
    is_anonymous: bool,
) -> td_api::ChatAdministratorRights {
    td_api::ChatAdministratorRights {
        can_manage_chat,
        can_change_info,
        can_post_messages,
        can_edit_messages,
        can_delete_messages,
        can_invite_users,
        can_restrict_members,
        can_pin_messages,
        can_manage_topics,
        can_promote_members,
        can_manage_video_chats,
        can_post_stories,
        can_edit_stories,
        can_delete_stories,
        can_manage_direct_messages,
        can_manage_tags,
        can_send_welcome_messages,
        is_anonymous,
    }
}

/// `chatPermissions` from the C++ 16 positional bools; callers land with
/// the chain-13 member-status arms.
#[allow(dead_code, clippy::fn_params_excessive_bools)]
fn chat_permissions(b: [bool; 16]) -> td_api::ChatPermissions {
    td_api::ChatPermissions {
        can_send_basic_messages: b[0],
        can_send_audios: b[1],
        can_send_documents: b[2],
        can_send_photos: b[3],
        can_send_videos: b[4],
        can_send_video_notes: b[5],
        can_send_voice_notes: b[6],
        can_send_polls: b[7],
        can_send_other_messages: b[8],
        can_add_link_previews: b[9],
        can_react_to_messages: b[10],
        can_edit_tag: b[11],
        can_change_info: b[12],
        can_invite_users: b[13],
        can_pin_messages: b[14],
        can_create_topics: b[15],
    }
}

/// `as_sticker_format` (cli.cpp:648) — the trailing `a`/`v` picks
/// tgs/webm, the default is webp.
fn as_sticker_format(op: &str) -> td_api::StickerFormat {
    if op.ends_with('a') {
        return td_api::StickerFormat::stickerFormatTgs(td_api::stickerFormatTgs);
    }
    if op.ends_with('v') {
        return td_api::StickerFormat::stickerFormatWebm(td_api::stickerFormatWebm);
    }
    td_api::StickerFormat::stickerFormatWebp(td_api::stickerFormatWebp)
}

/// `as_mask_position` (cli.cpp:668) — the trailing `m` puts the eyes mask
/// with random offsets, itself dropped by another coin flip.
fn as_mask_position(op: &str) -> Option<td_api::MaskPosition> {
    if !op.ends_with('m') {
        return None;
    }
    let position = td_api::MaskPosition {
        point: Some(td_api::MaskPoint::maskPointEyes(td_api::maskPointEyes)),
        x_shift: random::fast(-5, 5) as f64,
        y_shift: random::fast(-5, 5) as f64,
        scale: 1.0,
    };
    if random::fast_bool() {
        None
    } else {
        Some(position)
    }
}

/// `as_top_chat_category` (cli.cpp:2576) — lowercase, a trailing `s` is
/// stripped, and everything unknown is the users category.
fn as_top_chat_category(category: &str) -> td_api::TopChatCategory {
    let category = to_lower(trim_td(category));
    let category = category.strip_suffix('s').unwrap_or(&category);
    match category {
        "bot" => td_api::TopChatCategory::topChatCategoryBots(td_api::topChatCategoryBots),
        "group" => td_api::TopChatCategory::topChatCategoryGroups(td_api::topChatCategoryGroups),
        "channel" => {
            td_api::TopChatCategory::topChatCategoryChannels(td_api::topChatCategoryChannels)
        }
        "inline" => {
            td_api::TopChatCategory::topChatCategoryInlineBots(td_api::topChatCategoryInlineBots)
        }
        "guest" => {
            td_api::TopChatCategory::topChatCategoryGuestBots(td_api::topChatCategoryGuestBots)
        }
        "app" => {
            td_api::TopChatCategory::topChatCategoryWebAppBots(td_api::topChatCategoryWebAppBots)
        }
        "call" => td_api::TopChatCategory::topChatCategoryCalls(td_api::topChatCategoryCalls),
        "forward" => td_api::TopChatCategory::topChatCategoryForwardChats(
            td_api::topChatCategoryForwardChats,
        ),
        _ => td_api::TopChatCategory::topChatCategoryUsers(td_api::topChatCategoryUsers),
    }
}

/// `as_network_type` (cli.cpp:2686).
fn as_network_type(t: &str) -> td_api::NetworkType {
    match to_lower(trim_td(t)).as_str() {
        "none" => td_api::NetworkType::networkTypeNone(td_api::networkTypeNone),
        "mobile" => td_api::NetworkType::networkTypeMobile(td_api::networkTypeMobile),
        "roaming" => {
            td_api::NetworkType::networkTypeMobileRoaming(td_api::networkTypeMobileRoaming)
        }
        "wifi" => td_api::NetworkType::networkTypeWiFi(td_api::networkTypeWiFi),
        _ => td_api::NetworkType::networkTypeOther(td_api::networkTypeOther),
    }
}

/// `as_chat_action` (cli.cpp:2603) — the typing-action spell; the callers
/// land with the chain-12 draft/typing arms.
#[allow(dead_code)]
fn as_chat_action(action: &str) -> Option<td_api::ChatAction> {
    use td_api::ChatAction as Ca;
    let action = to_lower(trim_td(action));
    match action.as_str() {
        "c" | "cancel" => Some(Ca::chatActionCancel(td_api::chatActionCancel)),
        "rvi" | "record_video" => Some(Ca::chatActionRecordingVideo(
            td_api::chatActionRecordingVideo,
        )),
        "uvi" | "upload_video" => Some(Ca::chatActionUploadingVideo(
            td_api::chatActionUploadingVideo { progress: 50 },
        )),
        "rvo" | "record_voice" => Some(Ca::chatActionRecordingVoiceNote(
            td_api::chatActionRecordingVoiceNote,
        )),
        "uvo" | "upload_voice" => Some(Ca::chatActionUploadingVoiceNote(
            td_api::chatActionUploadingVoiceNote { progress: 50 },
        )),
        "up" | "upload_photo" => Some(Ca::chatActionUploadingPhoto(
            td_api::chatActionUploadingPhoto { progress: 50 },
        )),
        "ud" | "upload_document" => Some(Ca::chatActionUploadingDocument(
            td_api::chatActionUploadingDocument { progress: 50 },
        )),
        "fl" | "find_location" => Some(Ca::chatActionChoosingLocation(
            td_api::chatActionChoosingLocation,
        )),
        "cc" | "choose_contact" => Some(Ca::chatActionChoosingContact(
            td_api::chatActionChoosingContact,
        )),
        "spg" | "start_play_game" => Some(Ca::chatActionStartPlayingGame(
            td_api::chatActionStartPlayingGame,
        )),
        "rvn" | "record_video_note" => Some(Ca::chatActionRecordingVideoNote(
            td_api::chatActionRecordingVideoNote,
        )),
        "uvn" | "upload_video_note" => Some(Ca::chatActionUploadingVideoNote(
            td_api::chatActionUploadingVideoNote { progress: 50 },
        )),
        _ => None,
    }
}

/// `td::base64url_decode(...).move_as_ok()`
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

    /// `CliClient::execute` (cli.cpp:2958) — the synchronous path with the
    /// same `td_requests`-gated request/response prints.
    fn execute(&self, request: Function) -> Object {
        if self.requests_are_logged() {
            tdutils::log_error!("Execute request: {}", request.to_debug_string());
        }
        let response = ClientManager::execute(request);
        if self.requests_are_logged() {
            tdutils::log_error!("Execute response: {}", response.to_debug_string());
        }
        response
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

        // cli.cpp:1902-1916 — the three send-timing lambdas.
        // Lambda 1: on sendMessage reply carrying a message object, move the
        // SendMessageInfo from the query map to the message map.
        if id != 0
            && let Some(info) = self
                .query_id_to_send_message_info
                .lock()
                .unwrap()
                .remove(&id)
            && let Some(Object::message(ref msg)) = result
        {
            self.message_id_to_send_message_info
                .lock()
                .unwrap()
                .insert(msg.id, info);
        }
        // Lambda 2: on updateMessageSendAcknowledged, stamp quick_ack_time.
        if let Some(Object::updateMessageSendAcknowledged(ref upd)) = result {
            let mut map = self.message_id_to_send_message_info.lock().unwrap();
            if let Some(info) = map.get_mut(&upd.message_id) {
                info.quick_ack_time = time::now();
            }
        }
        // Lambda 3: on updateMessageSendSucceeded, stamp ack_time and log.
        if let Some(Object::updateMessageSendSucceeded(ref upd)) = result {
            let info = self
                .message_id_to_send_message_info
                .lock()
                .unwrap()
                .remove(&upd.old_message_id);
            if let Some(mut info) = info {
                info.ack_time = time::now();
                tdutils::log_info!("{}", info);
            }
        }

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
            // cli.cpp:1990-2003 — on an incoming business message while
            // use_test_dc is set, echo "Welcome!" back over the connection.
            Object::updateNewBusinessMessage(update)
                if update
                    .message
                    .as_ref()
                    .is_some_and(|m| m.message.as_ref().is_some_and(|m| !m.is_outgoing))
                    && self.use_test_dc.load(Ordering::SeqCst) =>
            {
                let conn_id = update.connection_id.clone();
                let old =
                    std::mem::replace(&mut *self.business_connection_id.lock().unwrap(), conn_id);
                self.on_cmd("gbc".to_owned());
                if let Some(msg) = update.message.as_ref().and_then(|m| m.message.as_ref()) {
                    self.send_message(
                        msg.chat_id,
                        td_api::InputMessageContent::inputMessageText(td_api::inputMessageText {
                            text: Some(td_api::FormattedText {
                                text: "Welcome!".to_owned(),
                                entities: vec![],
                            }),
                            link_preview_options: self.get_link_preview_options(),
                            clear_draft: true,
                        }),
                    );
                }
                *self.business_connection_id.lock().unwrap() = old;
            }
            // cli.cpp:2005-2009 — auto-answer pre-checkout queries on test_dc.
            Object::updateNewPreCheckoutQuery(update)
                if self.use_test_dc.load(Ordering::SeqCst) =>
            {
                self.send_request(Function::answerPreCheckoutQuery(
                    td_api::answerPreCheckoutQuery {
                        pre_checkout_query_id: update.id,
                        error_message: String::new(),
                    },
                ));
            }
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

    /// `get_upgraded_gift_attribute_ids` (cli.cpp:831) — the `m`/`s`/`b`
    /// prefix spell over `upgraded_gift_attribute_ids_`; C++ pushes a null
    /// element for an unknown prefix, which the Rust model can't represent,
    /// so the element is dropped (the S76 non-nullable-vector decision).
    fn get_upgraded_gift_attribute_ids(&self) -> Vec<td_api::UpgradedGiftAttributeId> {
        use td_api::UpgradedGiftAttributeId as Id;
        self.upgraded_gift_attribute_ids
            .lock()
            .unwrap()
            .iter()
            .filter_map(|attribute_id| match attribute_id.as_bytes().first() {
                Some(b'm') => Some(Id::upgradedGiftAttributeIdModel(
                    td_api::upgradedGiftAttributeIdModel {
                        sticker_id: tokenizer::to_integer_i64(&attribute_id[1..]),
                    },
                )),
                Some(b's') => Some(Id::upgradedGiftAttributeIdSymbol(
                    td_api::upgradedGiftAttributeIdSymbol {
                        sticker_id: tokenizer::to_integer_i64(&attribute_id[1..]),
                    },
                )),
                Some(b'b') => Some(Id::upgradedGiftAttributeIdBackdrop(
                    td_api::upgradedGiftAttributeIdBackdrop {
                        backdrop_id: tokenizer::to_integer_i32(&attribute_id[1..]),
                    },
                )),
                _ => None,
            })
            .collect()
    }

    /// The `CliClient::InputInvoice` get_args + conversion (cli.cpp:1297).
    fn get_input_invoice(&self, args: &str) -> td_api::InputInvoice {
        let first = args.as_bytes().first().copied();
        if args.len() > 1 && (first == Some(b'#') || first == Some(b'$')) {
            return td_api::InputInvoice::inputInvoiceName(td_api::inputInvoiceName {
                name: args[1..].to_owned(),
            });
        }
        if first == Some(b'+') || args.starts_with("https://t.me/+") {
            return td_api::InputInvoice::inputInvoiceTelegram(td_api::inputInvoiceTelegram {
                purpose: Some(
                    td_api::TelegramPaymentPurpose::telegramPaymentPurposeJoinChat(
                        td_api::telegramPaymentPurposeJoinChat {
                            invite_link: args.to_owned(),
                        },
                    ),
                ),
            });
        }
        if args.len() > 1 && first == Some(b'%') {
            let (user_id, text) = tokenizer::split(&args[1..], ',');
            let (amount, text) = tokenizer::split(&text, ',');
            let (month_count, text) = tokenizer::split(&text, ',');
            return td_api::InputInvoice::inputInvoiceTelegram(td_api::inputInvoiceTelegram {
                purpose: Some(
                    td_api::TelegramPaymentPurpose::telegramPaymentPurposePremiumGift(
                        td_api::telegramPaymentPurposePremiumGift {
                            currency: "XTR".to_owned(),
                            amount: tokenizer::to_integer_i64(&amount),
                            user_id: tokenizer::to_integer_i64(&user_id),
                            month_count: tokenizer::to_integer_i32(&month_count),
                            text: Some(self.as_formatted_text(&text)),
                        },
                    ),
                ),
            });
        }
        let (chat_id, message_id) = tokenizer::split(args, get_delimiter(args));
        td_api::InputInvoice::inputInvoiceMessage(td_api::inputInvoiceMessage {
            chat_id: self.as_chat_id(&chat_id),
            message_id: as_message_id(&message_id),
        })
    }

    /// `get_reposted_story_full_id` (cli.cpp:1222).
    fn get_reposted_story_full_id(&self) -> Option<td_api::StoryFullId> {
        let poster_chat_id = self.reposted_story_chat_id.load(Ordering::SeqCst);
        let story_id = self.reposted_story_id.load(Ordering::SeqCst);
        if poster_chat_id != 0 || story_id != 0 {
            Some(td_api::StoryFullId {
                poster_chat_id,
                story_id,
            })
        } else {
            None
        }
    }

    /// `get_args(args, StoryPrivacySettings)` (cli.cpp:1785) — the spell
    /// minus its leading character feeds `as_user_ids`.
    fn get_story_privacy_settings(&self, args: &str) -> td_api::StoryPrivacySettings {
        let settings = trim_td(args);
        let user_ids = if !settings.is_empty() && settings != "cf" {
            self.as_user_ids(&settings[1..])
        } else {
            Vec::new()
        };
        as_story_privacy_settings(settings, user_ids)
    }

    /// The `CliClient::InputCall` get_args + conversion (cli.cpp:863-886) —
    /// a two-word spell selects `inputCallFromMessage`, an empty spell the
    /// null call.
    fn get_input_call(&self, args: &str) -> Option<td_api::InputCall> {
        if args.is_empty() {
            return None;
        }
        let message_full_id = autosplit(args);
        if message_full_id.len() == 2 {
            return Some(td_api::InputCall::inputCallFromMessage(
                td_api::inputCallFromMessage {
                    chat_id: self.as_chat_id(&message_full_id[0]),
                    message_id: as_message_id(&message_full_id[1]),
                },
            ));
        }
        Some(td_api::InputCall::inputCallDiscarded(
            td_api::inputCallDiscarded {
                call_id: tokenizer::to_integer_i32(trim_td(args)),
            },
        ))
    }

    /// The `CliClient::InputGroupCall` get_args + conversion
    /// (cli.cpp:1344-1384).
    fn get_input_group_call(&self, args: &str) -> td_api::InputGroupCall {
        if args.starts_with("https://t.me/call") || args.starts_with("tg://call") {
            td_api::InputGroupCall::inputGroupCallLink(td_api::inputGroupCallLink {
                link: args.to_owned(),
            })
        } else {
            let (chat_id, message_id) = tokenizer::split(args, get_delimiter(args));
            td_api::InputGroupCall::inputGroupCallMessage(td_api::inputGroupCallMessage {
                chat_id: self.as_chat_id(&chat_id),
                message_id: as_message_id(&message_id),
            })
        }
    }

    /// `get_group_call_join_payload` (cli.cpp:902) — the fixed SDP probe.
    fn get_group_call_join_payload(&self, for_video: bool, for_screen_sharing: bool) -> String {
        let mut payload = String::from(
            "{\"ufrag\":\"ufrag\",\"pwd\":\"pwd\",\"fingerprints\":[{\"hash\":\"hash\",\"setup\":\"setup\",\"fingerprint\":\"fingerprint\"},{\"hash\":\"h2\",\"setup\":\"s2\",\"fingerprint\":\"fingerprint2\"}],\"ssrc\":",
        );
        payload.push_str(&self.group_call_source.load(Ordering::SeqCst).to_string());
        payload.push(',');
        if !for_video && !for_screen_sharing {
            payload.pop();
            payload.push('}');
        } else {
            let sim_sources = if for_screen_sharing { "[5,6]" } else { "[1,2]" };
            let fid_sources = if for_screen_sharing { "[7,8]" } else { "[3,4]" };
            payload.push_str("\"payload-types\":[{\"id\":12345,\"name\":\"opus\",\"clockrate\":48000,\"channels\":2,\"rtcp-fbs\":[{\"type\":\"transport-cc\",\"subtype\":\"subtype1\"},{\"type\":\"type2\",\"subtype\":\"subtype2\"}],\"parameters\":{\"minptime\":\"10\",\"useinbandfec\":\"1\"}}],\"rtp-hdrexts\":[{\"id\":1,\"uri\":\"urn:ietf:params:rtp-hdrext:ssrc-audio-level\"}],\"ssrc-groups\":[{\"sources\":");
            payload.push_str(sim_sources);
            payload.push_str(",\"semantics\":\"SIM\"},{\"sources\":");
            payload.push_str(fid_sources);
            payload.push_str(",\"semantics\":\"FID\"}]}");
        }
        payload
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
        // (`testdata/cli_census.txt`, 1382 arms) pins the per-block command
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
        } else if op == "resetp" {
            self.send_request(Function::resetPassword(td_api::resetPassword));
            true
        } else if op == "cpr" {
            self.send_request(Function::cancelPasswordReset(td_api::cancelPasswordReset));
            true
        } else if op == "gtp" {
            self.send_request(Function::getTemporaryPasswordState(
                td_api::getTemporaryPasswordState,
            ));
            true
        } else if op == "ctp" {
            self.send_request(Function::createTemporaryPassword(
                td_api::createTemporaryPassword {
                    password: args.to_owned(),
                    valid_for: 60 * 6,
                },
            ));
            true
        } else if op == "gpe" {
            let (password, passport_element_type) = tokenizer::split(args, ' ');
            self.send_request(Function::getPassportElement(td_api::getPassportElement {
                r#type: Some(as_passport_element_type(&passport_element_type)),
                password,
            }));
            true
        } else if op == "gape" {
            self.send_request(Function::getAllPassportElements(
                td_api::getAllPassportElements {
                    password: args.to_owned(),
                },
            ));
            true
        } else if op == "spe" || op == "spes" {
            let (password, rest0) = tokenizer::split(args, ' ');
            let (passport_element_type, arg) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::setPassportElement(td_api::setPassportElement {
                element: as_input_passport_element(&passport_element_type, &arg, op == "spes"),
                password,
            }));
            true
        } else if op == "dpe" {
            self.send_request(Function::deletePassportElement(
                td_api::deletePassportElement {
                    r#type: Some(as_passport_element_type(trim_td(args))),
                },
            ));
            true
        } else if op == "ppn" {
            self.send_request(Function::processPushNotification(
                td_api::processPushNotification {
                    payload: args.to_owned(),
                },
            ));
            true
        } else if op == "gpri" {
            self.send_request(Function::getPushReceiverId(td_api::getPushReceiverId {
                payload: args.to_owned(),
            }));
            true
        } else if op == "rda" {
            self.send_request(Function::registerDevice(td_api::registerDevice {
                device_token: Some(td_api::DeviceToken::deviceTokenApplePush(
                    td_api::deviceTokenApplePush {
                        device_token: args.to_owned(),
                        is_app_sandbox: true,
                    },
                )),
                other_user_ids: self.as_user_ids(""),
            }));
            true
        } else if op == "rdb" {
            self.send_request(Function::registerDevice(td_api::registerDevice {
                device_token: Some(td_api::DeviceToken::deviceTokenBlackBerryPush(
                    td_api::deviceTokenBlackBerryPush {
                        token: args.to_owned(),
                    },
                )),
                other_user_ids: self.as_user_ids(""),
            }));
            true
        } else if op == "rdf" {
            self.send_request(Function::registerDevice(td_api::registerDevice {
                device_token: Some(td_api::DeviceToken::deviceTokenFirebaseCloudMessaging(
                    td_api::deviceTokenFirebaseCloudMessaging {
                        token: args.to_owned(),
                        encrypt: true,
                    },
                )),
                other_user_ids: self.as_user_ids(""),
            }));
            true
        } else if op == "rdt" {
            let (token, other_user_ids_str) = tokenizer::split(args, ' ');
            self.send_request(Function::registerDevice(td_api::registerDevice {
                device_token: Some(td_api::DeviceToken::deviceTokenTizenPush(
                    td_api::deviceTokenTizenPush { reg_id: token },
                )),
                other_user_ids: self.as_user_ids(&other_user_ids_str),
            }));
            true
        } else if op == "rdu" {
            let (token, other_user_ids_str) = tokenizer::split(args, ' ');
            self.send_request(Function::registerDevice(td_api::registerDevice {
                device_token: Some(td_api::DeviceToken::deviceTokenUbuntuPush(
                    td_api::deviceTokenUbuntuPush { token },
                )),
                other_user_ids: self.as_user_ids(&other_user_ids_str),
            }));
            true
        } else if op == "rdw" {
            let (endpoint, rest0) = tokenizer::split(args, ' ');
            let (key, rest1) = tokenizer::split(&rest0, ' ');
            let (secret, other_user_ids_str) = tokenizer::split(&rest1, ' ');
            self.send_request(Function::registerDevice(td_api::registerDevice {
                device_token: Some(td_api::DeviceToken::deviceTokenWebPush(
                    td_api::deviceTokenWebPush {
                        endpoint,
                        p256dh_base64url: key,
                        auth_base64url: secret,
                    },
                )),
                other_user_ids: self.as_user_ids(&other_user_ids_str),
            }));
            true
        } else if op == "gbci" {
            self.send_request(Function::getBankCardInfo(td_api::getBankCardInfo {
                bank_card_number: args.to_owned(),
            }));
            true
        } else if op == "gpf" {
            self.send_request(Function::getPaymentForm(td_api::getPaymentForm {
                input_invoice: Some(self.get_input_invoice(args)),
                theme: Some(as_theme_parameters()),
            }));
            true
        } else if op == "voi" {
            let (input_invoice, allow_save) = tokenizer::split(args, ' ');
            self.send_request(Function::validateOrderInfo(td_api::validateOrderInfo {
                input_invoice: Some(self.get_input_invoice(&input_invoice)),
                order_info: None,
                allow_save: as_bool(&allow_save),
            }));
            true
        } else if op == "spfs" {
            let (input_invoice, rest0) = tokenizer::split(args, ' ');
            let (tip_amount, rest1) = tokenizer::split(&rest0, ' ');
            let (payment_form_id, rest2) = tokenizer::split(&rest1, ' ');
            let (order_info_id, rest3) = tokenizer::split(&rest2, ' ');
            let (shipping_option_id, saved_credentials_id) = tokenizer::split(&rest3, ' ');
            self.send_request(Function::sendPaymentForm(td_api::sendPaymentForm {
                input_invoice: Some(self.get_input_invoice(&input_invoice)),
                payment_form_id: tokenizer::to_integer_i64(&payment_form_id),
                order_info_id,
                shipping_option_id,
                credentials: Some(td_api::InputCredentials::inputCredentialsSaved(
                    td_api::inputCredentialsSaved {
                        saved_credentials_id,
                    },
                )),
                tip_amount: tokenizer::to_integer_i64(&tip_amount),
            }));
            true
        } else if op == "spfn" {
            let (input_invoice, rest0) = tokenizer::split(args, ' ');
            let (tip_amount, rest1) = tokenizer::split(&rest0, ' ');
            let (payment_form_id, rest2) = tokenizer::split(&rest1, ' ');
            let (order_info_id, rest3) = tokenizer::split(&rest2, ' ');
            let (shipping_option_id, data) = tokenizer::split(&rest3, ' ');
            self.send_request(Function::sendPaymentForm(td_api::sendPaymentForm {
                input_invoice: Some(self.get_input_invoice(&input_invoice)),
                payment_form_id: tokenizer::to_integer_i64(&payment_form_id),
                order_info_id,
                shipping_option_id,
                credentials: Some(td_api::InputCredentials::inputCredentialsNew(
                    td_api::inputCredentialsNew {
                        data,
                        allow_save: true,
                    },
                )),
                tip_amount: tokenizer::to_integer_i64(&tip_amount),
            }));
            true
        } else if op == "spfstar" {
            let (input_invoice, payment_form_id) = tokenizer::split(args, ' ');
            self.send_request(Function::sendPaymentForm(td_api::sendPaymentForm {
                input_invoice: Some(self.get_input_invoice(&input_invoice)),
                payment_form_id: tokenizer::to_integer_i64(&payment_form_id),
                order_info_id: String::new(),
                shipping_option_id: String::new(),
                credentials: None,
                tip_amount: 0,
            }));
            true
        } else if op == "gpre" {
            let (chat_id, message_id) = tokenizer::split(args, ' ');
            self.send_request(Function::getPaymentReceipt(td_api::getPaymentReceipt {
                chat_id: self.as_chat_id(&chat_id),
                message_id: as_message_id(&message_id),
            }));
            true
        } else if op == "gsoi" {
            self.send_request(Function::getSavedOrderInfo(td_api::getSavedOrderInfo));
            true
        } else if op == "dsoi" {
            self.send_request(Function::deleteSavedOrderInfo(td_api::deleteSavedOrderInfo));
            true
        } else if op == "dsc" {
            self.send_request(Function::deleteSavedCredentials(
                td_api::deleteSavedCredentials,
            ));
            true
        } else if op == "sgss" {
            let (show_button, rest0) = tokenizer::split(args, ' ');
            let (unlimited_gifts, rest1) = tokenizer::split(&rest0, ' ');
            let (limited_gifts, rest2) = tokenizer::split(&rest1, ' ');
            let (upgraded_gifts, rest3) = tokenizer::split(&rest2, ' ');
            let (gifts_from_channels, premium_subscription) = tokenizer::split(&rest3, ' ');
            self.send_request(Function::setGiftSettings(td_api::setGiftSettings {
                settings: Some(td_api::GiftSettings {
                    show_gift_button: as_bool(&show_button),
                    accepted_gift_types: Some(td_api::AcceptedGiftTypes {
                        unlimited_gifts: as_bool(&unlimited_gifts),
                        limited_gifts: as_bool(&limited_gifts),
                        upgraded_gifts: as_bool(&upgraded_gifts),
                        gifts_from_channels: as_bool(&gifts_from_channels),
                        premium_subscription: as_bool(&premium_subscription),
                    }),
                }),
            }));
            true
        } else if op == "gag" {
            self.send_request(Function::getAvailableGifts(td_api::getAvailableGifts));
            true
        } else if op == "csg" {
            self.send_request(Function::canSendGift(td_api::canSendGift {
                gift_id: tokenizer::to_integer_i64(trim_td(args)),
            }));
            true
        } else if op == "sendg" || op == "sendgp" || op == "sgift" {
            let (gift_id, rest0) = tokenizer::split(args, ' ');
            let (owner_id, rest1) = tokenizer::split(&rest0, ' ');
            let (pay_for_upgrade, text) = tokenizer::split(&rest1, ' ');
            self.send_request(Function::sendGift(td_api::sendGift {
                gift_id: tokenizer::to_integer_i64(&gift_id),
                owner_id: self.as_message_sender(&owner_id),
                text: Some(self.as_formatted_text(&text)),
                is_private: op == "sendgp",
                pay_for_upgrade: as_bool(&pay_for_upgrade),
            }));
            true
        } else if op == "ggas" {
            self.send_request(Function::getGiftAuctionState(td_api::getGiftAuctionState {
                auction_id: trim_td(args).to_owned(),
            }));
            true
        } else if op == "ggaag" {
            self.send_request(Function::getGiftAuctionAcquiredGifts(
                td_api::getGiftAuctionAcquiredGifts {
                    gift_id: tokenizer::to_integer_i64(trim_td(args)),
                },
            ));
            true
        } else if op == "oga" {
            self.send_request(Function::openGiftAuction(td_api::openGiftAuction {
                gift_id: tokenizer::to_integer_i64(trim_td(args)),
            }));
            true
        } else if op == "cga" {
            self.send_request(Function::closeGiftAuction(td_api::closeGiftAuction {
                gift_id: tokenizer::to_integer_i64(trim_td(args)),
            }));
            true
        } else if op == "sellg" {
            self.send_request(Function::sellGift(td_api::sellGift {
                business_connection_id: self.business_connection_id.lock().unwrap().clone(),
                received_gift_id: trim_td(args).to_owned(),
            }));
            true
        } else if op == "pgab" || op == "pgabp" {
            let (gift_id, rest0) = tokenizer::split(args, ' ');
            let (star_count, rest1) = tokenizer::split(&rest0, ' ');
            let (user_id, text) = tokenizer::split(&rest1, ' ');
            self.send_request(Function::placeGiftAuctionBid(td_api::placeGiftAuctionBid {
                gift_id: tokenizer::to_integer_i64(&gift_id),
                star_count: tokenizer::to_integer_i64(&star_count),
                user_id: self.as_user_id(&user_id),
                text: Some(self.as_formatted_text(&text)),
                is_private: op == "pgabp",
            }));
            true
        } else if op == "igab" {
            let (gift_id, star_count) = tokenizer::split(args, ' ');
            self.send_request(Function::increaseGiftAuctionBid(
                td_api::increaseGiftAuctionBid {
                    gift_id: tokenizer::to_integer_i64(&gift_id),
                    star_count: tokenizer::to_integer_i64(&star_count),
                },
            ));
            true
        } else if op == "tgis" {
            let (received_gift_id, is_saved) = tokenizer::split(args, ' ');
            self.send_request(Function::toggleGiftIsSaved(td_api::toggleGiftIsSaved {
                received_gift_id,
                is_saved: as_bool(&is_saved),
            }));
            true
        } else if op == "spg" {
            let (owner_id, received_gift_ids) = tokenizer::split(args, ' ');
            self.send_request(Function::setPinnedGifts(td_api::setPinnedGifts {
                owner_id: self.as_message_sender(&owner_id),
                received_gift_ids: autosplit(&received_gift_ids),
            }));
            true
        } else if op == "tcgn" {
            let (chat_id, are_enabled) = tokenizer::split(args, ' ');
            self.send_request(Function::toggleChatGiftNotifications(
                td_api::toggleChatGiftNotifications {
                    chat_id: self.as_chat_id(&chat_id),
                    are_enabled: as_bool(&are_enabled),
                },
            ));
            true
        } else if op == "ggup" {
            self.send_request(Function::getGiftUpgradePreview(
                td_api::getGiftUpgradePreview {
                    regular_gift_id: tokenizer::to_integer_i64(trim_td(args)),
                },
            ));
            true
        } else if op == "gugv" || op == "gugvu" || op == "gugvc" {
            self.send_request(Function::getUpgradedGiftVariants(
                td_api::getUpgradedGiftVariants {
                    regular_gift_id: tokenizer::to_integer_i64(trim_td(args)),
                    return_upgrade_models: op != "gugvc",
                    return_craft_models: op != "gugvu",
                },
            ));
            true
        } else if op == "ug" {
            let (received_gift_id, rest0) = tokenizer::split(args, ' ');
            let (keep_original_details, star_count) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::upgradeGift(td_api::upgradeGift {
                business_connection_id: self.business_connection_id.lock().unwrap().clone(),
                received_gift_id,
                keep_original_details: as_bool(&keep_original_details),
                star_count: tokenizer::to_integer_i64(&star_count),
            }));
            true
        } else if op == "bgu" {
            let (owner_id, rest0) = tokenizer::split(args, ' ');
            let (prepaid_upgrade_hash, star_count) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::buyGiftUpgrade(td_api::buyGiftUpgrade {
                owner_id: self.as_message_sender(&owner_id),
                prepaid_upgrade_hash,
                star_count: tokenizer::to_integer_i64(&star_count),
            }));
            true
        } else if op == "cg" {
            self.send_request(Function::craftGift(td_api::craftGift {
                received_gift_ids: autosplit(args),
            }));
            true
        } else if op == "tg" {
            let (received_gift_id, rest0) = tokenizer::split(args, ' ');
            let (new_owner_id, star_count) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::transferGift(td_api::transferGift {
                business_connection_id: self.business_connection_id.lock().unwrap().clone(),
                received_gift_id,
                new_owner_id: self.as_message_sender(&new_owner_id),
                star_count: tokenizer::to_integer_i64(&star_count),
            }));
            true
        } else if op == "dgod" {
            let (received_gift_id, star_count) = tokenizer::split(args, ' ');
            self.send_request(Function::dropGiftOriginalDetails(
                td_api::dropGiftOriginalDetails {
                    received_gift_id,
                    star_count: tokenizer::to_integer_i64(&star_count),
                },
            ));
            true
        } else if op == "srg" || op == "srgp" {
            let (gift_name, rest0) = tokenizer::split(args, ' ');
            let (owner_id, rest1) = tokenizer::split(&rest0, ' ');
            let (price, text) = tokenizer::split(&rest1, ' ');
            self.send_request(Function::sendResoldGift(td_api::sendResoldGift {
                gift_name,
                owner_id: self.as_message_sender(&owner_id),
                price: as_gift_resale_price(&price),
                text: Some(self.as_formatted_text(&text)),
                is_private: op == "srg",
            }));
            true
        } else if op == "sgpo" {
            let (owner_id, rest0) = tokenizer::split(args, ' ');
            let (gift_name, rest1) = tokenizer::split(&rest0, ' ');
            let (price, duration) = tokenizer::split(&rest1, ' ');
            self.send_request(Function::sendGiftPurchaseOffer(
                td_api::sendGiftPurchaseOffer {
                    owner_id: self.as_message_sender(&owner_id),
                    gift_name,
                    price: as_gift_resale_price(&price),
                    duration: tokenizer::to_integer_i32(&duration),
                    paid_message_star_count: self.paid_message_star_count.load(Ordering::SeqCst),
                },
            ));
            true
        } else if op == "pgpoa" || op == "pgpod" {
            self.send_request(Function::processGiftPurchaseOffer(
                td_api::processGiftPurchaseOffer {
                    message_id: as_message_id(args),
                    accept: op == "pgpoa",
                },
            ));
            true
        } else if op == "grgs" || op == "grgsp" {
            let (owner_id, rest0) = tokenizer::split(args, ' ');
            let (limit, rest1) = tokenizer::split(&rest0, ' ');
            let (offset, rest2) = tokenizer::split(&rest1, ' ');
            let (exclude_unsaved, rest3) = tokenizer::split(&rest2, ' ');
            let (exclude_saved, rest4) = tokenizer::split(&rest3, ' ');
            let (exclude_unlimited, rest5) = tokenizer::split(&rest4, ' ');
            let (exclude_upgradable, rest6) = tokenizer::split(&rest5, ' ');
            let (exclude_non_upgradable, rest7) = tokenizer::split(&rest6, ' ');
            let (exclude_upgraded, rest8) = tokenizer::split(&rest7, ' ');
            let (exclude_without_colors, exclude_hosted) = tokenizer::split(&rest8, ' ');
            self.send_request(Function::getReceivedGifts(td_api::getReceivedGifts {
                business_connection_id: self.business_connection_id.lock().unwrap().clone(),
                owner_id: self.as_message_sender(&owner_id),
                collection_id: self.gift_collection_id.load(Ordering::SeqCst),
                exclude_unsaved: as_bool(&exclude_unsaved),
                exclude_saved: as_bool(&exclude_saved),
                exclude_unlimited: as_bool(&exclude_unlimited),
                exclude_upgradable: as_bool(&exclude_upgradable),
                exclude_non_upgradable: as_bool(&exclude_non_upgradable),
                exclude_upgraded: as_bool(&exclude_upgraded),
                exclude_without_colors: as_bool(&exclude_without_colors),
                exclude_hosted: as_bool(&exclude_hosted),
                sort_by_price: op == "grgsp",
                offset,
                limit: as_limit(&limit, 10),
            }));
            true
        } else if op == "grg" {
            self.send_request(Function::getReceivedGift(td_api::getReceivedGift {
                received_gift_id: trim_td(args).to_owned(),
            }));
            true
        } else if op == "ggfc" {
            let (gift_id, rest0) = tokenizer::split(args, ' ');
            let (limit, offset) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::getGiftsForCrafting(td_api::getGiftsForCrafting {
                regular_gift_id: tokenizer::to_integer_i64(&gift_id),
                offset,
                limit: as_limit(&limit, 10),
            }));
            true
        } else if op == "gug" {
            self.send_request(Function::getUpgradedGift(td_api::getUpgradedGift {
                name: trim_td(args).to_owned(),
            }));
            true
        } else if op == "gugvi" {
            self.send_request(Function::getUpgradedGiftValueInfo(
                td_api::getUpgradedGiftValueInfo {
                    name: trim_td(args).to_owned(),
                },
            ));
            true
        } else if op == "sgrp" {
            let (received_gift_id, price) = tokenizer::split(args, ' ');
            self.send_request(Function::setGiftResalePrice(td_api::setGiftResalePrice {
                received_gift_id,
                price: as_gift_resale_price(&price),
            }));
            true
        } else if op == "gugwu" {
            // 1.8.70 dropped the request password — the saved temporary
            // password gates the wire query.
            let (received_gift_id, _password) = tokenizer::split(args, ' ');
            self.send_request(Function::getUpgradedGiftWithdrawalUrl(
                td_api::getUpgradedGiftWithdrawalUrl { received_gift_id },
            ));
            true
        } else if op == "gugpa" {
            self.send_request(Function::getUpgradedGiftsPromotionalAnimation(
                td_api::getUpgradedGiftsPromotionalAnimation,
            ));
            true
        } else if op == "sgfr" || op == "sgfrd" || op == "sgfrn" || op == "spfrc" {
            let (gift_id, rest0) = tokenizer::split(args, ' ');
            let (limit, rest1) = tokenizer::split(&rest0, ' ');
            let (offset, for_stars) = tokenizer::split(&rest1, ' ');
            let order = match op {
                "sgfrd" => td_api::GiftForResaleOrder::giftForResaleOrderPriceChangeDate(
                    td_api::giftForResaleOrderPriceChangeDate,
                ),
                "sgfrn" => td_api::GiftForResaleOrder::giftForResaleOrderNumber(
                    td_api::giftForResaleOrderNumber,
                ),
                _ => td_api::GiftForResaleOrder::giftForResaleOrderPrice(
                    td_api::giftForResaleOrderPrice,
                ),
            };
            self.send_request(Function::searchGiftsForResale(
                td_api::searchGiftsForResale {
                    gift_id: tokenizer::to_integer_i64(&gift_id),
                    order: Some(order),
                    for_crafting: op == "spfrc",
                    for_stars: as_bool(&for_stars),
                    attributes: self.get_upgraded_gift_attribute_ids(),
                    offset,
                    limit: as_limit(&limit, 10),
                },
            ));
            true
        } else if op == "ggic" {
            self.send_request(Function::getGiftCollections(td_api::getGiftCollections {
                owner_id: self.as_message_sender(args),
            }));
            true
        } else if op == "cgic" {
            let (owner_id, rest0) = tokenizer::split(args, ' ');
            let (name, received_gift_ids) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::createGiftCollection(
                td_api::createGiftCollection {
                    owner_id: self.as_message_sender(&owner_id),
                    name,
                    received_gift_ids: autosplit(&received_gift_ids),
                },
            ));
            true
        } else if op == "sgicn" {
            let (owner_id, name) = tokenizer::split(args, ' ');
            self.send_request(Function::setGiftCollectionName(
                td_api::setGiftCollectionName {
                    owner_id: self.as_message_sender(&owner_id),
                    collection_id: self.gift_collection_id.load(Ordering::SeqCst),
                    name,
                },
            ));
            true
        } else if op == "agicg" {
            let (owner_id, received_gift_ids) = tokenizer::split(args, ' ');
            self.send_request(Function::addGiftCollectionGifts(
                td_api::addGiftCollectionGifts {
                    owner_id: self.as_message_sender(&owner_id),
                    collection_id: self.gift_collection_id.load(Ordering::SeqCst),
                    received_gift_ids: autosplit(&received_gift_ids),
                },
            ));
            true
        } else if op == "rgicg" {
            let (owner_id, received_gift_ids) = tokenizer::split(args, ' ');
            self.send_request(Function::removeGiftCollectionGifts(
                td_api::removeGiftCollectionGifts {
                    owner_id: self.as_message_sender(&owner_id),
                    collection_id: self.gift_collection_id.load(Ordering::SeqCst),
                    received_gift_ids: autosplit(&received_gift_ids),
                },
            ));
            true
        } else if op == "rogicg" {
            let (owner_id, received_gift_ids) = tokenizer::split(args, ' ');
            self.send_request(Function::reorderGiftCollectionGifts(
                td_api::reorderGiftCollectionGifts {
                    owner_id: self.as_message_sender(&owner_id),
                    collection_id: self.gift_collection_id.load(Ordering::SeqCst),
                    received_gift_ids: autosplit(&received_gift_ids),
                },
            ));
            true
        } else if op == "rogic" {
            let (owner_id, collection_ids) = tokenizer::split(args, ' ');
            self.send_request(Function::reorderGiftCollections(
                td_api::reorderGiftCollections {
                    owner_id: self.as_message_sender(&owner_id),
                    collection_ids: as_gift_collection_ids(&collection_ids),
                },
            ));
            true
        } else if op == "dgic" {
            let (owner_id, gift_collection_id) = tokenizer::split(args, ' ');
            self.send_request(Function::deleteGiftCollection(
                td_api::deleteGiftCollection {
                    owner_id: self.as_message_sender(&owner_id),
                    collection_id: as_gift_collection_id(&gift_collection_id),
                },
            ));
            true
        } else if op == "rsp" {
            let (user_id, telegram_payment_charge_id) = tokenizer::split(args, ' ');
            self.send_request(Function::refundStarPayment(td_api::refundStarPayment {
                user_id: self.as_user_id(&user_id),
                telegram_payment_charge_id,
            }));
            true
        } else if op == "gpr" {
            self.send_request(Function::getUserPrivacySettingRules(
                td_api::getUserPrivacySettingRules {
                    setting: as_user_privacy_setting(args),
                },
            ));
            true
        } else if op == "spr" {
            let (setting, rules) = tokenizer::split(args, ' ');
            self.send_request(Function::setUserPrivacySettingRules(
                td_api::setUserPrivacySettingRules {
                    setting: as_user_privacy_setting(&setting),
                    rules: Some(as_user_privacy_setting_rules(&rules)),
                },
            ));
            true
        } else if op == "spncc" {
            self.send_request(Function::sendPhoneNumberCode(td_api::sendPhoneNumberCode {
                phone_number: args.to_owned(),
                settings: None,
                r#type: Some(td_api::PhoneNumberCodeType::phoneNumberCodeTypeChange(
                    td_api::phoneNumberCodeTypeChange,
                )),
            }));
            true
        } else if op == "spncv" {
            self.send_request(Function::sendPhoneNumberCode(td_api::sendPhoneNumberCode {
                phone_number: args.to_owned(),
                settings: None,
                r#type: Some(td_api::PhoneNumberCodeType::phoneNumberCodeTypeVerify(
                    td_api::phoneNumberCodeTypeVerify,
                )),
            }));
            true
        } else if op == "spncco" {
            let (hash, phone_number) = tokenizer::split(args, ' ');
            self.send_request(Function::sendPhoneNumberCode(td_api::sendPhoneNumberCode {
                phone_number,
                settings: None,
                r#type: Some(
                    td_api::PhoneNumberCodeType::phoneNumberCodeTypeConfirmOwnership(
                        td_api::phoneNumberCodeTypeConfirmOwnership { hash },
                    ),
                ),
            }));
            true
        } else if op == "spnfs" {
            self.send_request(Function::sendPhoneNumberFirebaseSms(
                td_api::sendPhoneNumberFirebaseSms {
                    token: args.to_owned(),
                },
            ));
            true
        } else if op == "rpncm" {
            self.send_request(Function::reportPhoneNumberCodeMissing(
                td_api::reportPhoneNumberCodeMissing {
                    mobile_network_code: args.to_owned(),
                },
            ));
            true
        } else if op == "rpnc" {
            self.send_request(Function::resendPhoneNumberCode(
                td_api::resendPhoneNumberCode { reason: None },
            ));
            true
        } else if op == "cpnc" {
            self.send_request(Function::checkPhoneNumberCode(
                td_api::checkPhoneNumberCode {
                    code: args.to_owned(),
                },
            ));
            true
        } else if op == "gco" {
            if args.is_empty() {
                self.send_request(Function::getContacts(td_api::getContacts));
            } else {
                self.send_request(Function::searchContacts(td_api::searchContacts {
                    query: String::new(),
                    limit: as_limit(args, 10),
                }));
            }
            true
        } else if op == "gcfr" {
            self.send_request(Function::getCloseFriends(td_api::getCloseFriends));
            true
        } else if op == "scfr" {
            self.send_request(Function::setCloseFriends(td_api::setCloseFriends {
                user_ids: self.as_user_ids(args),
            }));
            true
        } else if op == "gul" || op == "gulf" || op == "gulfp" || op == "gulfr" || op == "gulfpr" {
            // cpp:3723 — the fee/permanent/revoke op matrix.
            self.send_request(Function::getUserLink(td_api::getUserLink {
                remove_messages_fee: op != "gul",
                return_permanent_link: op == "gulfp" || op == "gulfpr",
                revoke_previous_links: op == "gulfr" || op == "gulfpr",
            }));
            true
        } else if op == "subt" {
            self.send_request(Function::searchUserByToken(td_api::searchUserByToken {
                token: args.to_owned(),
            }));
            true
        } else if op == "aco" || op == "acon" {
            let (user_id, rest0) = tokenizer::split(args, ' ');
            let (first_name, last_name) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::addContact(td_api::addContact {
                user_id: self.as_user_id(&user_id),
                contact: Some(td_api::ImportedContact {
                    phone_number: String::new(),
                    first_name,
                    last_name,
                    note: if op == "acon" {
                        self.get_caption()
                    } else {
                        None
                    },
                }),
                share_phone_number: false,
            }));
            true
        } else if op == "subpn" || op == "subpnl" {
            self.send_request(Function::searchUserByPhoneNumber(
                td_api::searchUserByPhoneNumber {
                    phone_number: trim_td(args).to_owned(),
                    only_local: op == "subpnl",
                },
            ));
            true
        } else if op == "spn" {
            self.send_request(Function::sharePhoneNumber(td_api::sharePhoneNumber {
                user_id: self.as_user_id(args),
            }));
            true
        } else if op == "ic" || op == "cic" {
            let contacts = tokenizer::full_split(args, ';')
                .iter()
                .map(|c| {
                    let (phone_number, rest) = tokenizer::split(c, ',');
                    let (first_name, rest) = tokenizer::split(&rest, ',');
                    let (last_name, note) = tokenizer::split(&rest, ',');
                    td_api::ImportedContact {
                        phone_number,
                        first_name,
                        last_name,
                        note: if note == "-" {
                            None
                        } else {
                            Some(self.as_formatted_text(&note))
                        },
                    }
                })
                .collect();
            if op == "cic" {
                self.send_request(Function::changeImportedContacts(
                    td_api::changeImportedContacts { contacts },
                ));
            } else {
                self.send_request(Function::importContacts(td_api::importContacts {
                    contacts,
                }));
            }
            true
        } else if op == "rco" {
            self.send_request(Function::removeContacts(td_api::removeContacts {
                user_ids: self.as_user_ids(args),
            }));
            true
        } else if op == "gicc" {
            self.send_request(Function::getImportedContactCount(
                td_api::getImportedContactCount,
            ));
            true
        } else if op == "cico" {
            self.send_request(Function::clearImportedContacts(
                td_api::clearImportedContacts,
            ));
            true
        } else {
            false
        }
    }

    /// `CliClient::as_chat_id` (cli.cpp:508) — `me`, `.`, `@username`
    /// through both registries (a supergroup maps to
    /// `-1000000000000 - supergroup_id`), else the plain integer.
    fn as_chat_id(&self, s: &str) -> i64 {
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
            let lowered = to_lower(s);
            if let Some(user_id) = self.username_to_user_id.lock().unwrap().get(&lowered) {
                return *user_id;
            }
            if let Some(supergroup_id) =
                self.username_to_supergroup_id.lock().unwrap().get(&lowered)
            {
                return -1_000_000_000_000 - supergroup_id;
            }
            tdutils::log_error!("Can't resolve {}", s);
            return 0;
        }
        tokenizer::to_integer_i64(s)
    }

    /// `get_input_thumbnail` (cli.cpp:820).
    fn get_input_thumbnail(&self) -> Option<td_api::InputThumbnail> {
        let thumbnail = self.thumbnail.lock().unwrap();
        if thumbnail.is_empty() {
            return None;
        }
        Some(td_api::InputThumbnail {
            thumbnail: as_input_file(&thumbnail),
            width: 0,
            height: 0,
        })
    }

    /// `get_input_cover` (cli.cpp:813).
    fn get_input_cover(&self) -> Option<td_api::InputFile> {
        let cover = self.cover.lock().unwrap();
        if cover.is_empty() {
            return None;
        }
        as_input_file(&cover)
    }

    /// `get_added_sticker_file_ids` (cli.cpp:827).
    fn get_added_sticker_file_ids(&self) -> Vec<i32> {
        self.added_sticker_file_ids.lock().unwrap().clone()
    }

    /// `as_input_photo` (cli.cpp:2267).
    fn as_input_photo(&self, photo: &str) -> td_api::InputPhoto {
        td_api::InputPhoto {
            photo: as_input_file(photo),
            thumbnail: self.get_input_thumbnail(),
            video: None,
            added_sticker_file_ids: self.get_added_sticker_file_ids(),
            width: 0,
            height: 0,
        }
    }

    /// `as_input_video` (cli.cpp:2276).
    fn as_input_video(&self, video: &str) -> td_api::InputVideo {
        td_api::InputVideo {
            video: as_input_file(video),
            thumbnail: self.get_input_thumbnail(),
            cover: self.get_input_cover(),
            start_timestamp: self.start_timestamp.load(Ordering::SeqCst),
            added_sticker_file_ids: self.get_added_sticker_file_ids(),
            duration: 1,
            width: 2,
            height: 3,
            supports_streaming: true,
        }
    }

    /// `as_input_animation` (cli.cpp:2249).
    fn as_input_animation(&self, animation: &str) -> td_api::InputAnimation {
        td_api::InputAnimation {
            animation: as_input_file(animation),
            thumbnail: self.get_input_thumbnail(),
            added_sticker_file_ids: self.get_added_sticker_file_ids(),
            duration: 0,
            width: 0,
            height: 0,
        }
    }

    /// `CliClient::as_supergroup_id` (cli.cpp:724).
    fn as_supergroup_id(&self, s: &str) -> i64 {
        let s = trim_td(s);
        let s = s.strip_prefix('@').unwrap_or(s);
        if s.as_bytes()
            .first()
            .is_some_and(|b| b.is_ascii_alphabetic())
        {
            return self
                .username_to_supergroup_id
                .lock()
                .unwrap()
                .get(&to_lower(s))
                .copied()
                .unwrap_or(0);
        }
        let mut result = tokenizer::to_integer_i64(s);
        if s == "." {
            result = self.opened_chat_id.load(Ordering::SeqCst);
        }
        const SHIFT: i64 = -1_000_000_000_000;
        if result <= SHIFT {
            return SHIFT - result;
        }
        result
    }

    /// `CliClient::as_basic_group_id` (cli.cpp:712) — negative ids are
    /// absolute-valued, `.` is the opened chat.
    fn as_basic_group_id(&self, s: &str) -> i64 {
        let s = trim_td(s);
        let mut result = tokenizer::to_integer_i64(s);
        if s == "." {
            result = self.opened_chat_id.load(Ordering::SeqCst);
        }
        if result < 0 {
            return -result;
        }
        result
    }

    /// `as_custom_emoji_id` (cli.cpp:926).
    fn as_custom_emoji_id(&self, s: &str) -> i64 {
        tokenizer::to_integer_i64(trim_td(s))
    }

    /// `get_caption` (cli.cpp:806).
    fn get_caption(&self) -> Option<td_api::FormattedText> {
        let caption = self.caption.lock().unwrap();
        if caption.is_empty() {
            return None;
        }
        Some(self.as_formatted_text(&caption))
    }

    /// `get_message_self_destruct_type` (cli.cpp:2868).
    fn get_message_self_destruct_type(&self) -> Option<td_api::MessageSelfDestructType> {
        let time = self.message_self_destruct_time.load(Ordering::SeqCst);
        if time == -1 {
            return Some(
                td_api::MessageSelfDestructType::messageSelfDestructTypeImmediately(
                    td_api::messageSelfDestructTypeImmediately,
                ),
            );
        }
        if time > 0 {
            return Some(
                td_api::MessageSelfDestructType::messageSelfDestructTypeTimer(
                    td_api::messageSelfDestructTypeTimer {
                        self_destruct_time: time,
                    },
                ),
            );
        }
        None
    }

    /// `as_message_scheduling_state` (cli.cpp:2878).
    fn as_message_scheduling_state(&self) -> Option<td_api::MessageSchedulingState> {
        let date = self.schedule_date.lock().unwrap().clone();
        let date = trim_td(&date);
        if date.is_empty() {
            return None;
        }
        let send_date = tokenizer::to_integer_i32(date);
        if send_date == -1 {
            return Some(
                td_api::MessageSchedulingState::messageSchedulingStateSendWhenOnline(
                    td_api::messageSchedulingStateSendWhenOnline,
                ),
            );
        }
        Some(
            td_api::MessageSchedulingState::messageSchedulingStateSendAtDate(
                td_api::messageSchedulingStateSendAtDate {
                    send_date,
                    repeat_period: self.schedule_repeat_period.load(Ordering::SeqCst),
                },
            ),
        )
    }

    /// `get_input_message_reply_to` (cli.cpp:1204).
    fn get_input_message_reply_to(&self) -> Option<td_api::InputMessageReplyTo> {
        let reply_message_id = self.reply_message_id.load(Ordering::SeqCst);
        if reply_message_id != 0 {
            let quote = {
                let quote = self.reply_quote.lock().unwrap();
                if quote.is_empty() {
                    None
                } else {
                    Some(td_api::InputTextQuote {
                        text: Some(self.as_formatted_text(&quote)),
                        // 1.8.70 rich-quote plane — the CLI keeps the
                        // plain quote.
                        rich_texts: Vec::new(),
                        position: self.reply_quote_position.load(Ordering::SeqCst),
                    })
                }
            };
            let checklist_task_id = self.reply_checklist_task_id.load(Ordering::SeqCst);
            let poll_option_id = self.reply_poll_option_id.lock().unwrap().clone();
            let reply_chat_id = self.reply_chat_id.load(Ordering::SeqCst);
            if reply_chat_id == 0 {
                return Some(td_api::InputMessageReplyTo::inputMessageReplyToMessage(
                    td_api::inputMessageReplyToMessage {
                        message_id: reply_message_id,
                        quote,
                        checklist_task_id,
                        poll_option_id,
                    },
                ));
            }
            return Some(
                td_api::InputMessageReplyTo::inputMessageReplyToExternalMessage(
                    td_api::inputMessageReplyToExternalMessage {
                        chat_id: reply_chat_id,
                        message_id: reply_message_id,
                        quote,
                        checklist_task_id,
                        poll_option_id,
                    },
                ),
            );
        }
        let story_chat_id = self.reply_story_chat_id.load(Ordering::SeqCst);
        let story_id = self.reply_story_id.load(Ordering::SeqCst);
        if story_chat_id != 0 || story_id != 0 {
            return Some(td_api::InputMessageReplyTo::inputMessageReplyToStory(
                td_api::inputMessageReplyToStory {
                    story_poster_chat_id: story_chat_id,
                    story_id,
                },
            ));
        }
        None
    }

    /// `get_link_preview_options` (cli.cpp:1262); the callers land with the
    /// chain-16 text-send arms.
    #[allow(dead_code)]
    fn get_link_preview_options(&self) -> Option<td_api::LinkPreviewOptions> {
        let url = self.link_preview_url.lock().unwrap().clone();
        if !self.link_preview_is_disabled.load(Ordering::SeqCst)
            && url.is_empty()
            && !self.link_preview_force_small_media.load(Ordering::SeqCst)
            && !self.link_preview_force_large_media.load(Ordering::SeqCst)
            && !self.link_preview_show_above_text.load(Ordering::SeqCst)
        {
            return None;
        }
        Some(td_api::LinkPreviewOptions {
            is_disabled: self.link_preview_is_disabled.load(Ordering::SeqCst),
            url,
            force_small_media: self.link_preview_force_small_media.load(Ordering::SeqCst),
            force_large_media: self.link_preview_force_large_media.load(Ordering::SeqCst),
            show_above_text: self.link_preview_show_above_text.load(Ordering::SeqCst),
        })
    }

    /// `send_get_background_url` (cli.cpp:3054) — the fixed `"asd"` name.
    fn send_get_background_url(&self, background_type: td_api::BackgroundType) {
        self.send_request(Function::getBackgroundUrl(td_api::getBackgroundUrl {
            name: "asd".to_owned(),
            r#type: Some(background_type),
        }));
    }

    /// `as_input_audio` (cli.cpp:2255).
    fn as_input_audio(&self, audio: &str, duration: i32) -> td_api::InputAudio {
        td_api::InputAudio {
            audio: as_input_file(audio),
            album_cover_thumbnail: self.get_input_thumbnail(),
            duration,
            title: "title".to_owned(),
            performer: "performer".to_owned(),
        }
    }

    /// `get_story_music` (cli.cpp:1232-1237) — `None` with no `ssmu`
    /// state.
    fn get_story_music(&self) -> Option<td_api::InputAudio> {
        let story_music = self.story_music.lock().unwrap();
        if story_music.is_empty() {
            None
        } else {
            Some(self.as_input_audio(&story_music, 0))
        }
    }

    /// `as_input_document` (cli.cpp:2261).
    fn as_input_document(
        &self,
        document: &str,
        disable_content_type_detection: bool,
    ) -> td_api::InputDocument {
        td_api::InputDocument {
            document: as_input_file(document),
            thumbnail: self.get_input_thumbnail(),
            disable_content_type_detection,
        }
    }

    /// `as_input_sticker` (cli.cpp:2272).
    fn as_input_sticker(&self, sticker: &str) -> td_api::InputSticker {
        td_api::InputSticker {
            sticker: as_input_file(sticker),
            thumbnail: self.get_input_thumbnail(),
            width: 0,
            height: 0,
        }
    }

    /// `as_input_video_note` (cli.cpp:2281).
    fn as_input_video_note(&self, video_note: &str) -> td_api::InputVideoNote {
        td_api::InputVideoNote {
            video_note: as_input_file(video_note),
            thumbnail: self.get_input_thumbnail(),
            duration: 10,
            length: 5,
        }
    }

    /// `as_input_voice_note` (cli.cpp:2285).
    fn as_input_voice_note(&self, voice_note: &str) -> td_api::InputVoiceNote {
        td_api::InputVoiceNote {
            voice_note: as_input_file(voice_note),
            duration: 5,
            waveform: "abacaba".as_bytes().to_vec(),
        }
    }

    /// `as_live_location` (cli.cpp:938).
    fn as_live_location(
        &self,
        latitude: &str,
        longitude: &str,
        accuracy: &str,
        period: i32,
        heading: i32,
        proximity_alert_radius: i32,
    ) -> td_api::LiveLocation {
        td_api::LiveLocation {
            location: as_location(latitude, longitude, accuracy),
            live_period: period,
            heading,
            proximity_alert_radius,
        }
    }

    /// `CliClient::send_message` (cli.cpp:2998) — the central send dispatch:
    /// the welcome/ephemeral/business/quick-reply branches, then the plain
    /// sendMessage with the full messageSendOptions pack.
    fn send_message(&self, chat_id: i64, content: td_api::InputMessageContent) {
        self.send_message_opts(chat_id, content, false, false);
    }

    /// The `send_message` overload with the explicit option arguments
    /// (cli.cpp:2990).
    fn send_message_opts(
        &self,
        chat_id: i64,
        content: td_api::InputMessageContent,
        disable_notification: bool,
        from_background: bool,
    ) {
        if self.send_welcome_message.load(Ordering::SeqCst) {
            self.send_request(Function::addChatWelcomeMessage(
                td_api::addChatWelcomeMessage {
                    chat_id,
                    input_message_content: Some(Box::new(content)),
                },
            ));
            return;
        }
        let edit_welcome = self.edit_welcome_message_id.load(Ordering::SeqCst);
        if edit_welcome != 0 {
            self.send_request(Function::editChatWelcomeMessage(
                td_api::editChatWelcomeMessage {
                    chat_id,
                    welcome_message_id: edit_welcome,
                    input_message_content: Some(Box::new(content)),
                },
            ));
            return;
        }
        let ephemeral = self.ephemeral_message_receiver_id.load(Ordering::SeqCst);
        if ephemeral != 0 {
            self.send_request(Function::sendEphemeralMessage(
                td_api::sendEphemeralMessage {
                    chat_id,
                    topic_id: self.get_message_topic_id(),
                    receiver_user_id: ephemeral,
                    callback_query_id: 0,
                    replace_callback_query_message: false,
                    reply_to: self.get_input_message_reply_to(),
                    protect_content: false,
                    sending_id: 123,
                    only_preview: self.only_preview.load(Ordering::SeqCst),
                    reply_markup: None,
                    input_message_content: Some(Box::new(content)),
                },
            ));
            return;
        }
        let business = self.business_connection_id.lock().unwrap().clone();
        if !business.is_empty() {
            self.send_request(Function::sendBusinessMessage(td_api::sendBusinessMessage {
                business_connection_id: business,
                chat_id,
                reply_to: self.get_input_message_reply_to(),
                disable_notification,
                protect_content: random::fast_bool(),
                effect_id: self.message_effect_id.load(Ordering::SeqCst),
                reply_markup: None,
                input_message_content: Some(Box::new(content)),
            }));
            return;
        }
        let shortcut = self.quick_reply_shortcut_name.lock().unwrap().clone();
        if !shortcut.is_empty() {
            self.send_request(Function::addQuickReplyShortcutMessage(
                td_api::addQuickReplyShortcutMessage {
                    shortcut_name: shortcut,
                    reply_to_message_id: self.reply_message_id.load(Ordering::SeqCst),
                    input_message_content: Some(Box::new(content)),
                },
            ));
            return;
        }
        let options = td_api::MessageSendOptions {
            suggested_post_info: self.get_input_suggested_post_info(),
            disable_notification,
            from_background,
            protect_content: false,
            allow_paid_broadcast: self.use_test_dc.load(Ordering::SeqCst),
            paid_message_star_count: self.paid_message_star_count.load(Ordering::SeqCst),
            update_order_of_installed_sticker_sets: false,
            scheduling_state: self.as_message_scheduling_state(),
            effect_id: self.message_effect_id.load(Ordering::SeqCst),
            sending_id: random::fast(1, 1000),
            only_preview: self.only_preview.load(Ordering::SeqCst),
        };
        let id = self.send_request(Function::sendMessage(td_api::sendMessage {
            chat_id,
            topic_id: self.get_message_topic_id(),
            reply_to: self.get_input_message_reply_to(),
            options: Some(options),
            reply_markup: None,
            input_message_content: Some(Box::new(content)),
        }));
        // cli.cpp:3025 — record the start time keyed by the query id.
        if id != 0 {
            self.query_id_to_send_message_info
                .lock()
                .unwrap()
                .entry(id)
                .or_default()
                .start_time = time::now();
        }
    }

    /// `get_input_suggested_post_info` (cli.cpp:3028) — the suggested-post
    /// price is unset until the chain-9 `spsp` arms land.
    fn get_input_suggested_post_info(&self) -> Option<td_api::InputSuggestedPostInfo> {
        let price = suggested_post_price(&self.suggested_post_price.lock().unwrap());
        let send_date = self.suggested_post_send_date.load(Ordering::SeqCst);
        if price.is_none() && send_date == -1 {
            return None;
        }
        Some(td_api::InputSuggestedPostInfo {
            price,
            send_date: send_date.max(0),
        })
    }

    /// `set_draft_message` (cli.cpp:3045).
    fn set_draft_message(&self, chat_id: i64, content: Option<td_api::DraftMessageContent>) {
        let draft = content.map(|content| td_api::DraftMessage {
            reply_to: self.get_input_message_reply_to(),
            date: 0,
            content: Some(content),
            effect_id: self.message_effect_id.load(Ordering::SeqCst),
            suggested_post_info: self.get_input_suggested_post_info(),
        });
        self.send_request(Function::setChatDraftMessage(td_api::setChatDraftMessage {
            chat_id,
            topic_id: self.get_message_topic_id(),
            draft_message: draft,
        }));
    }

    /// `CliClient::as_chat_folder` (cli.cpp:2549) — the six-word filter
    /// spell (title, icon, pinned, included, excluded), 8 random exclusion
    /// flags.
    fn as_chat_folder(&self, filter: &str, is_shareable: bool) -> td_api::ChatFolder {
        let (title, icon_name, pinned_chat_ids, included_chat_ids, excluded_chat_ids) = {
            let (w0, rest0) = tokenizer::split(filter, ' ');
            let (w1, rest1) = tokenizer::split(&rest0, ' ');
            let (w2, rest2) = tokenizer::split(&rest1, ' ');
            let (w3, rest3) = tokenizer::split(&rest2, ' ');
            let (w4, _) = tokenizer::split(&rest3, ' ');
            (w0, w1, w2, w3, w4)
        };
        td_api::ChatFolder {
            name: Some(td_api::ChatFolderName {
                text: Some(td_api::FormattedText {
                    text: title,
                    entities: Vec::new(),
                }),
                animate_custom_emoji: true,
            }),
            icon: Some(td_api::ChatFolderIcon { name: icon_name }),
            color_id: -1,
            is_shareable,
            pinned_chat_ids: self.as_chat_ids(&pinned_chat_ids),
            included_chat_ids: self.as_chat_ids(&included_chat_ids),
            excluded_chat_ids: self.as_chat_ids(&excluded_chat_ids),
            exclude_muted: random::fast_bool(),
            exclude_read: random::fast_bool(),
            exclude_archived: random::fast_bool(),
            include_contacts: random::fast_bool(),
            include_non_contacts: random::fast_bool(),
            include_bots: random::fast_bool(),
            include_groups: random::fast_bool(),
            include_channels: random::fast_bool(),
        }
    }

    /// The parsed `InputChatPhoto` get_args (cli.cpp:1486) — the integer is
    /// the previous photo, `p` the static path, `a` an animation (with the
    /// main-frame timestamp), `s` a sticker.
    fn parse_input_chat_photo(&self, args: &str) -> Option<td_api::InputChatPhoto> {
        let args = trim_td(args);
        if args.is_empty() {
            return None;
        }
        if args.parse::<i64>().is_ok() {
            return Some(td_api::InputChatPhoto::inputChatPhotoPrevious(
                td_api::inputChatPhotoPrevious {
                    chat_photo_id: tokenizer::to_integer_i64(args),
                },
            ));
        }
        if args.starts_with('p') {
            return Some(td_api::InputChatPhoto::inputChatPhotoStatic(
                td_api::inputChatPhotoStatic {
                    photo: as_input_file(args.strip_prefix('p').unwrap_or("")),
                },
            ));
        }
        if args.starts_with('a') {
            let (photo, main_frame_timestamp) =
                tokenizer::split(args.strip_prefix('a').unwrap_or(""), get_delimiter(args));
            return Some(td_api::InputChatPhoto::inputChatPhotoAnimation(
                td_api::inputChatPhotoAnimation {
                    animation: as_input_file(&photo),
                    main_frame_timestamp: main_frame_timestamp.trim().parse().unwrap_or(0.0),
                },
            ));
        }
        if args.starts_with('s') {
            // ChatPhotoSticker get_args (cli.cpp:1446).
            let (sticker_set_id, sticker_id) =
                tokenizer::split(args.strip_prefix('s').unwrap_or(""), get_delimiter(args));
            let sticker = if sticker_id.is_empty() {
                // A custom-emoji sticker: the single id, 4-color fill.
                td_api::ChatPhotoSticker {
                    r#type: Some(
                        td_api::ChatPhotoStickerType::chatPhotoStickerTypeCustomEmoji(
                            td_api::chatPhotoStickerTypeCustomEmoji {
                                custom_emoji_id: tokenizer::to_integer_i64(&sticker_set_id),
                            },
                        ),
                    ),
                    background_fill: background_fill_freeform(vec![
                        0x000000, 0xFF0000, 0x00FF00, 0x0000FF,
                    ]),
                }
            } else {
                td_api::ChatPhotoSticker {
                    r#type: Some(
                        td_api::ChatPhotoStickerType::chatPhotoStickerTypeRegularOrMask(
                            td_api::chatPhotoStickerTypeRegularOrMask {
                                sticker_set_id: tokenizer::to_integer_i64(&sticker_set_id),
                                sticker_id: tokenizer::to_integer_i64(&sticker_id),
                            },
                        ),
                    ),
                    background_fill: background_fill_solid(0x7FFFFFFF),
                }
            };
            return Some(td_api::InputChatPhoto::inputChatPhotoSticker(
                td_api::inputChatPhotoSticker {
                    sticker: Some(sticker),
                },
            ));
        }
        None
    }

    /// `CliClient::as_secret_chat_id` (cli.cpp:747) — the -2e12 shift.
    fn as_secret_chat_id(&self, s: &str) -> i32 {
        let s = trim_td(s);
        let mut result = tokenizer::to_integer_i64(s);
        if s == "." {
            result = self.opened_chat_id.load(Ordering::SeqCst);
        }
        const SHIFT: i64 = -2_000_000_000_000;
        if result <= SHIFT + i32::MAX as i64 {
            return (result - SHIFT) as i32;
        }
        result as i32
    }

    /// `as_shortcut_id` (cli.cpp:1196).
    fn as_shortcut_id(&self, s: &str) -> i32 {
        tokenizer::to_integer_i32(trim_td(s))
    }

    /// `as_shortcut_ids` (cli.cpp:1200).
    fn as_shortcut_ids(&self, s: &str) -> Vec<i32> {
        autosplit(s)
            .iter()
            .map(|t| self.as_shortcut_id(t))
            .collect()
    }

    /// `as_chat_members_filter` (cli.cpp:2474).
    fn as_chat_members_filter(&self, filter: &str) -> Option<td_api::ChatMembersFilter> {
        use td_api::ChatMembersFilter as Cmf;
        let filter = to_lower(trim_td(filter));
        match filter.as_str() {
            "a" | "admin" | "administrators" => Some(Cmf::chatMembersFilterAdministrators(
                td_api::chatMembersFilterAdministrators,
            )),
            "b" | "banned" => Some(Cmf::chatMembersFilterBanned(
                td_api::chatMembersFilterBanned,
            )),
            "bot" | "bots" => Some(Cmf::chatMembersFilterBots(td_api::chatMembersFilterBots)),
            "c" | "contacts" => Some(Cmf::chatMembersFilterContacts(
                td_api::chatMembersFilterContacts,
            )),
            "m" | "members" => Some(Cmf::chatMembersFilterMembers(
                td_api::chatMembersFilterMembers,
            )),
            "@" => Some(Cmf::chatMembersFilterMention(
                td_api::chatMembersFilterMention {
                    topic_id: self.get_message_topic_id(),
                },
            )),
            "r" | "rest" | "restricted" => Some(Cmf::chatMembersFilterRestricted(
                td_api::chatMembersFilterRestricted,
            )),
            _ => {
                if !filter.is_empty() {
                    tdutils::log_error!("Unsupported chat member filter {}", filter);
                }
                None
            }
        }
    }

    /// `as_supergroup_members_filter` (cli.cpp:2504) — the get/search/
    /// supergroup prefix stripping and the query-aware filters.
    fn as_supergroup_members_filter(
        &self,
        filter: &str,
        query: &str,
    ) -> Option<td_api::SupergroupMembersFilter> {
        use td_api::SupergroupMembersFilter as Smf;
        let filter = to_lower(trim_td(filter));
        let filter = filter.strip_prefix("get").unwrap_or(&filter);
        let filter = filter.strip_prefix("search").unwrap_or(filter);
        let filter = filter.strip_prefix("supergroup").unwrap_or(filter);
        match filter {
            "administrators" => Some(Smf::supergroupMembersFilterAdministrators(
                td_api::supergroupMembersFilterAdministrators,
            )),
            "banned" => Some(Smf::supergroupMembersFilterBanned(
                td_api::supergroupMembersFilterBanned {
                    query: query.to_owned(),
                },
            )),
            "bots" => Some(Smf::supergroupMembersFilterBots(
                td_api::supergroupMembersFilterBots,
            )),
            "contacts" => Some(Smf::supergroupMembersFilterContacts(
                td_api::supergroupMembersFilterContacts {
                    query: query.to_owned(),
                },
            )),
            "members" => {
                if query.is_empty() {
                    Some(Smf::supergroupMembersFilterRecent(
                        td_api::supergroupMembersFilterRecent,
                    ))
                } else {
                    Some(Smf::supergroupMembersFilterSearch(
                        td_api::supergroupMembersFilterSearch {
                            query: query.to_owned(),
                        },
                    ))
                }
            }
            "restricted" => Some(Smf::supergroupMembersFilterRestricted(
                td_api::supergroupMembersFilterRestricted {
                    query: query.to_owned(),
                },
            )),
            "mentions" => Some(Smf::supergroupMembersFilterMention(
                td_api::supergroupMembersFilterMention {
                    query: query.to_owned(),
                    topic_id: self.get_message_topic_id(),
                },
            )),
            _ => None,
        }
    }

    /// `as_tone` (cli.cpp:960).
    fn as_tone(&self, tone: &str) -> String {
        match tone.chars().last() {
            Some('f') => "formal".to_owned(),
            Some('n') => "neutral".to_owned(),
            Some('c') => "casual".to_owned(),
            _ => String::new(),
        }
    }

    /// `as_input_rich_message` (cli.cpp:2289) — the `id;type media` list
    /// over rich_message_media_, the `;`→newline message spell, the coin-flip
    /// source selection.
    fn as_input_rich_message(&self, message: &str) -> td_api::InputRichMessage {
        let rich_media = self.rich_message_media.lock().unwrap().clone();
        let mut input_blocks: Vec<Box<td_api::InputPageBlock>> = Vec::new();
        let mut input_media: Vec<Box<td_api::InputRichMessageMedia>> = Vec::new();
        for rich_message_media in tokenizer::full_split(&rich_media, ' ') {
            let (id, media) = tokenizer::split(&rich_message_media, ';');
            if media.len() <= 1 {
                tdutils::log_error!("Ignore {}", rich_message_media);
                continue;
            }
            let media_type = media.as_bytes()[0];
            let media = &media[1..];
            let block: Option<Box<td_api::InputPageBlock>>;
            let content: Option<Box<td_api::InputMessageContent>>;
            match media_type {
                b'a' => {
                    let animation = self.as_input_animation(media);
                    block = Some(Box::new(td_api::InputPageBlock::inputPageBlockAnimation(
                        td_api::inputPageBlockAnimation {
                            animation: Some(animation.clone()),
                            caption: None,
                            has_spoiler: random::fast_bool(),
                        },
                    )));
                    content = Some(Box::new(
                        td_api::InputMessageContent::inputMessageAnimation(
                            td_api::inputMessageAnimation {
                                animation: Some(animation),
                                caption: None,
                                show_caption_above_media: false,
                                has_spoiler: false,
                            },
                        ),
                    ));
                }
                b'n' => {
                    let voice_note = self.as_input_voice_note(media);
                    block = Some(Box::new(td_api::InputPageBlock::inputPageBlockVoiceNote(
                        td_api::inputPageBlockVoiceNote {
                            voice_note: Some(voice_note.clone()),
                            caption: None,
                        },
                    )));
                    content = Some(Box::new(
                        td_api::InputMessageContent::inputMessageVoiceNote(
                            td_api::inputMessageVoiceNote {
                                voice_note: Some(voice_note),
                                caption: None,
                                self_destruct_type: None,
                            },
                        ),
                    ));
                }
                b'p' => {
                    let photo = self.as_input_photo(media);
                    block = Some(Box::new(td_api::InputPageBlock::inputPageBlockPhoto(
                        td_api::inputPageBlockPhoto {
                            photo: Some(photo.clone()),
                            caption: None,
                            has_spoiler: random::fast_bool(),
                        },
                    )));
                    content = Some(Box::new(td_api::InputMessageContent::inputMessagePhoto(
                        td_api::inputMessagePhoto {
                            photo: Some(photo),
                            caption: None,
                            show_caption_above_media: false,
                            self_destruct_type: None,
                            has_spoiler: false,
                        },
                    )));
                }
                b'u' => {
                    let audio = self.as_input_audio(media, 0);
                    block = Some(Box::new(td_api::InputPageBlock::inputPageBlockAudio(
                        td_api::inputPageBlockAudio {
                            audio: Some(audio.clone()),
                            caption: None,
                        },
                    )));
                    content = Some(Box::new(td_api::InputMessageContent::inputMessageAudio(
                        td_api::inputMessageAudio {
                            audio: Some(audio),
                            caption: None,
                        },
                    )));
                }
                b'v' => {
                    let video = self.as_input_video(media);
                    block = Some(Box::new(td_api::InputPageBlock::inputPageBlockVideo(
                        td_api::inputPageBlockVideo {
                            video: Some(video.clone()),
                            caption: None,
                            has_spoiler: random::fast_bool(),
                        },
                    )));
                    content = Some(Box::new(td_api::InputMessageContent::inputMessageVideo(
                        td_api::inputMessageVideo {
                            video: Some(video),
                            caption: None,
                            show_caption_above_media: false,
                            self_destruct_type: None,
                            has_spoiler: false,
                        },
                    )));
                }
                _ => {
                    block = None;
                    content = None;
                }
            }
            if let Some(block) = block {
                input_blocks.push(block);
            }
            input_media.push(Box::new(td_api::InputRichMessageMedia {
                id,
                media: content,
            }));
        }
        // The message `;`s become newlines (cli.cpp:2327), then the bold
        // plain-text paragraph closes the block list.
        let message = message.replace(';', "\n");
        input_blocks.push(Box::new(td_api::InputPageBlock::inputPageBlockParagraph(
            td_api::inputPageBlockParagraph {
                text: Some(Box::new(td_api::RichText::richTextBold(
                    td_api::richTextBold {
                        text: Some(Box::new(td_api::RichText::richTextPlain(
                            td_api::richTextPlain { text: message },
                        ))),
                    },
                ))),
            },
        )));
        if random::fast_bool() {
            td_api::InputRichMessage {
                source: Some(Box::new(
                    td_api::RichMessageSource::richMessageSourceBlocks(
                        td_api::richMessageSourceBlocks {
                            blocks: input_blocks,
                        },
                    ),
                )),
                is_rtl: random::fast_bool(),
                detect_automatic_blocks: random::fast_bool(),
            }
        } else {
            td_api::InputRichMessage {
                source: Some(Box::new(
                    td_api::RichMessageSource::richMessageSourceMarkdown(
                        td_api::richMessageSourceMarkdown {
                            text: String::new(),
                            media: input_media,
                        },
                    ),
                )),
                is_rtl: random::fast_bool(),
                detect_automatic_blocks: random::fast_bool(),
            }
        }
    }

    /// `default_message_send_options` (cli.cpp:3038).
    fn default_message_send_options(&self) -> td_api::MessageSendOptions {
        td_api::MessageSendOptions {
            suggested_post_info: self.get_input_suggested_post_info(),
            disable_notification: false,
            from_background: false,
            protect_content: false,
            allow_paid_broadcast: self.use_test_dc.load(Ordering::SeqCst),
            paid_message_star_count: self.paid_message_star_count.load(Ordering::SeqCst),
            update_order_of_installed_sticker_sets: true,
            scheduling_state: self.as_message_scheduling_state(),
            effect_id: self.message_effect_id.load(Ordering::SeqCst),
            sending_id: random::fast(1, 1000),
            only_preview: self.only_preview.load(Ordering::SeqCst),
        }
    }

    /// `CliClient::as_chat_ids` (cli.cpp:577).
    fn as_chat_ids(&self, s: &str) -> Vec<i64> {
        autosplit(s).iter().map(|t| self.as_chat_id(t)).collect()
    }

    /// `as_input_message` (cli.cpp:2343).
    fn as_input_message(&self, message: &str, is_rich: bool) -> td_api::InputMessageContent {
        if is_rich {
            td_api::InputMessageContent::inputMessageRichMessage(td_api::inputMessageRichMessage {
                message: Some(Box::new(self.as_input_rich_message(message))),
                clear_draft: true,
            })
        } else {
            td_api::InputMessageContent::inputMessageText(td_api::inputMessageText {
                text: Some(self.as_formatted_text(message)),
                link_preview_options: self.get_link_preview_options(),
                clear_draft: true,
            })
        }
    }

    /// `as_suggested_action` (cli.cpp:2707).
    fn as_suggested_action(&self, action: &str) -> Option<td_api::SuggestedAction> {
        use td_api::SuggestedAction as Sa;
        match action {
            "unarchive" => Some(Sa::suggestedActionEnableArchiveAndMuteNewChats(
                td_api::suggestedActionEnableArchiveAndMuteNewChats,
            )),
            "pass" => Some(Sa::suggestedActionCheckPassword(
                td_api::suggestedActionCheckPassword,
            )),
            "number" => Some(Sa::suggestedActionCheckPhoneNumber(
                td_api::suggestedActionCheckPhoneNumber,
            )),
            "checks" => Some(Sa::suggestedActionViewChecksHint(
                td_api::suggestedActionViewChecksHint,
            )),
            "extend" => Some(Sa::suggestedActionExtendPremium(
                td_api::suggestedActionExtendPremium {
                    manage_premium_subscription_url: String::new(),
                },
            )),
            "annual" => Some(Sa::suggestedActionSubscribeToAnnualPremium(
                td_api::suggestedActionSubscribeToAnnualPremium,
            )),
            _ => {
                if let Some(rest) = action.strip_prefix("giga") {
                    return Some(Sa::suggestedActionConvertToBroadcastGroup(
                        td_api::suggestedActionConvertToBroadcastGroup {
                            supergroup_id: self.as_supergroup_id(rest),
                        },
                    ));
                }
                if let Some(rest) = action.strip_prefix("spass") {
                    return Some(Sa::suggestedActionSetPassword(
                        td_api::suggestedActionSetPassword {
                            authorization_delay: tokenizer::to_integer_i32(rest),
                        },
                    ));
                }
                if let Some(rest) = action.strip_prefix("custom") {
                    let (name, url) = tokenizer::split(rest, ' ');
                    return Some(Sa::suggestedActionCustom(td_api::suggestedActionCustom {
                        name,
                        title: None,
                        description: None,
                        url,
                    }));
                }
                None
            }
        }
    }

    /// The `CliClient::KeyboardButtonSource` get_args + conversion
    /// (cli.cpp:1399-1428) — the `p...` spell selects the WebApp source.
    fn get_keyboard_button_source(&self, args: &str) -> td_api::KeyboardButtonSource {
        if args.starts_with('p') {
            let (bot_user_id, prepared_button_id) = tokenizer::split(args, get_delimiter(args));
            td_api::KeyboardButtonSource::keyboardButtonSourceWebApp(
                td_api::keyboardButtonSourceWebApp {
                    bot_user_id: self.as_user_id(&bot_user_id),
                    prepared_button_id,
                },
            )
        } else {
            let (chat_id, message_id) = tokenizer::split(args, get_delimiter(args));
            td_api::KeyboardButtonSource::keyboardButtonSourceMessage(
                td_api::keyboardButtonSourceMessage {
                    chat_id: self.as_chat_id(&chat_id),
                    message_id: as_message_id(&message_id),
                },
            )
        }
    }

    /// `as_web_app_open_parameters` (cli.cpp:2896).
    fn as_web_app_open_parameters(&self) -> td_api::WebAppOpenParameters {
        td_api::WebAppOpenParameters {
            theme: Some(as_theme_parameters()),
            application_name: "android".to_owned(),
            mode: Some(td_api::WebAppOpenMode::webAppOpenModeFullScreen(
                td_api::webAppOpenModeFullScreen,
            )),
        }
    }

    /// `get_args(args, InputChecklist)` (cli.cpp:1556) — the title word then
    /// the autosplit task spell.
    fn get_input_checklist(&self, args: &str) -> td_api::InputChecklist {
        let (title, tasks) = tokenizer::split(args, ' ');
        let mut count = 0i32;
        let tasks = autosplit(&tasks)
            .iter()
            .map(|task| {
                count += 1;
                td_api::InputChecklistTask {
                    id: count,
                    text: Some(self.as_formatted_text(task)),
                }
            })
            .collect();
        td_api::InputChecklist {
            title: Some(self.as_formatted_text(&title)),
            tasks,
            others_can_add_tasks: random::fast_bool(),
            others_can_mark_tasks_as_done: random::fast_bool(),
        }
    }

    /// The `CliClient::SearchQuery` get_args (cli.cpp:996) — the first word
    /// is the limit when it is a positive integer, else the limit is 10 and
    /// the whole spell is the query. Returns `(query, limit)`.
    fn parse_search_query(spell: &str) -> (String, i32) {
        let trimmed = trim_td(spell);
        let (limit_word, rest) = tokenizer::split(trimmed, ' ');
        match limit_word.parse::<i32>() {
            Ok(parsed) if parsed > 0 => (rest, parsed),
            _ => (trimmed.to_owned(), 10),
        }
    }

    /// `as_search_chat_type_filter` (cli.cpp:1272) — the trailing `b`/`c`.
    fn as_search_chat_type_filter(op: &str) -> Option<td_api::SearchChatTypeFilter> {
        if op.len() <= 2 {
            return None;
        }
        match op.as_bytes()[op.len() - 1] {
            b'b' => Some(td_api::SearchChatTypeFilter::searchChatTypeFilterBot(
                td_api::searchChatTypeFilterBot,
            )),
            b'c' => Some(td_api::SearchChatTypeFilter::searchChatTypeFilterChannel(
                td_api::searchChatTypeFilterChannel,
            )),
            _ => None,
        }
    }

    /// `as_business_recipients` (cli.cpp:643).
    fn as_business_recipients(&self, chat_ids: &str) -> td_api::BusinessRecipients {
        td_api::BusinessRecipients {
            chat_ids: self.as_chat_ids(chat_ids),
            excluded_chat_ids: Vec::new(),
            select_existing_chats: false,
            select_new_chats: false,
            select_contacts: false,
            select_non_contacts: false,
            exclude_selected: true,
        }
    }

    /// `CliClient::as_message_sender` (cli.cpp:609).
    fn as_message_sender(&self, s: &str) -> Option<td_api::MessageSender> {
        let s = trim_td(s);
        let user_id = self.as_user_id_expect_error(s, true);
        if s.is_empty() || user_id > 0 {
            return Some(td_api::MessageSender::messageSenderUser(
                td_api::messageSenderUser { user_id },
            ));
        }
        Some(td_api::MessageSender::messageSenderChat(
            td_api::messageSenderChat {
                chat_id: self.as_chat_id(s),
            },
        ))
    }

    /// `as_user_id` with the silent-lookup variant (`expect_error = true`).
    fn as_user_id_expect_error(&self, s: &str, expect_error: bool) -> i64 {
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
            if !expect_error {
                tdutils::log_error!("Can't find user {}", s);
            }
            return 0;
        }
        tokenizer::to_integer_i64(s)
    }

    /// `CliClient::as_formatted_text` (cli.cpp:2218) — the `\n` spell, the
    /// reserved-character escapes, then the synchronous markdown-V2 parse.
    fn as_formatted_text(&self, text: &str) -> td_api::FormattedText {
        self.as_formatted_text_with_entities(text, Vec::new())
    }

    /// `as_formatted_text` (cli.cpp:2410) with an explicit entity list — a
    /// non-empty list skips the escape/parse path entirely, like the C++
    /// default-argument overload.
    fn as_formatted_text_with_entities(
        &self,
        text: &str,
        entities: Vec<td_api::TextEntity>,
    ) -> td_api::FormattedText {
        if entities.is_empty() && !text.is_empty() {
            const RESERVED: &[u8] = b"#+-={}.";
            let bytes = text.as_bytes();
            let mut new_text = String::with_capacity(text.len());
            let mut i = 0;
            while i < bytes.len() {
                let c = bytes[i];
                if c == b'\\' && bytes.get(i + 1) == Some(&b'n') {
                    new_text.push('\n');
                    i += 2;
                    continue;
                }
                if RESERVED.contains(&c) {
                    new_text.push('\\');
                }
                new_text.push(c as char);
                i += 1;
            }
            let parsed =
                ClientManager::execute(Function::parseTextEntities(td_api::parseTextEntities {
                    text: new_text,
                    parse_mode: Some(td_api::TextParseMode::textParseModeMarkdown(
                        td_api::textParseModeMarkdown { version: 2 },
                    )),
                }));
            if let Object::formattedText(formatted) = parsed {
                return formatted;
            }
        }
        td_api::FormattedText {
            text: text.to_owned(),
            entities,
        }
    }

    /// The `AffiliateType` get_args (cli.cpp:1535) — `my_id` collapses to
    /// the current-user variant.
    fn affiliate_type(&self, args: &str) -> td_api::AffiliateType {
        let mut id = self.as_chat_id(args);
        if id == self.my_id.load(Ordering::SeqCst) {
            id = 0;
        }
        if id == 0 {
            return td_api::AffiliateType::affiliateTypeCurrentUser(
                td_api::affiliateTypeCurrentUser,
            );
        }
        if id > 0 {
            return td_api::AffiliateType::affiliateTypeBot(td_api::affiliateTypeBot {
                user_id: id,
            });
        }
        td_api::AffiliateType::affiliateTypeChannel(td_api::affiliateTypeChannel { chat_id: id })
    }

    /// The `GiveawayParameters` get_args (cli.cpp:1386) —
    /// `chat additional-chats... date country` with the C++ `args.size() < 3`
    /// (string length!) guard kept verbatim; the conversion (cli.cpp:1380)
    /// fills the random flags and the `"prize"` description.
    fn giveaway_parameters(&self, args: &str) -> td_api::GiveawayParameters {
        let mut parsed = (0i64, Vec::<i64>::new(), 0i32, Vec::<String>::new());
        if args.len() >= 3 {
            let parts = autosplit(args);
            if parts.len() >= 3 {
                parsed.0 = self.as_chat_id(&parts[0]);
                parsed.2 = tokenizer::to_integer_i32(&parts[parts.len() - 2]);
                parsed.3.push(parts[parts.len() - 1].clone());
                for part in &parts[1..parts.len() - 2] {
                    parsed.1.push(self.as_chat_id(part));
                }
            }
        }
        td_api::GiveawayParameters {
            boosted_chat_id: parsed.0,
            additional_chat_ids: parsed.1,
            winners_selection_date: parsed.2,
            only_new_members: random::fast_bool(),
            has_public_winners: random::fast_bool(),
            country_codes: parsed.3,
            prize_description: "prize".to_owned(),
        }
    }

    /// `get_saved_messages_topic_id` (cli.cpp:1243) — the `-1` spell is the
    /// general topic (2666000).
    fn get_saved_messages_topic_id(&self) -> i64 {
        let id = self.saved_messages_topic_id.load(Ordering::SeqCst);
        if id == -1 {
            return 2666000;
        }
        id
    }

    /// cli.cpp:3748 — the chats chain (M.14.4a): the chat-list loaders, the
    /// direct-messages/saved-messages topic families, reactions (default,
    /// paid, tags), stories/search dev probes, and the language-pack family.
    /// The `apa` arm alone stays unported until the M.14.5 input-file
    /// machinery lands.
    fn op_block_chats(&self, op: &str, args: &str) -> bool {
        if op == "gc" || op == "gca" || op.starts_with("gc-") {
            self.send_request(Function::getChats(td_api::getChats {
                chat_list: as_chat_list(op),
                limit: as_limit(args, 10000),
            }));
            true
        } else if op == "lc" || op == "lca" || op.starts_with("lc-") {
            self.send_request(Function::loadChats(td_api::loadChats {
                chat_list: as_chat_list(op),
                limit: as_limit(args, 10000),
            }));
            true
        } else if op == "gctest" {
            for limit in [1, 10, 5] {
                self.send_request(Function::getChats(td_api::getChats {
                    chat_list: None,
                    limit,
                }));
            }
            true
        } else if op == "ldmct" {
            let (chat_id, limit) = tokenizer::split(args, ' ');
            self.send_request(Function::loadDirectMessagesChatTopics(
                td_api::loadDirectMessagesChatTopics {
                    chat_id: self.as_chat_id(&chat_id),
                    limit: as_limit(&limit, 10),
                },
            ));
            true
        } else if op == "gdmct" {
            let (chat_id, topic) = tokenizer::split(args, ' ');
            let topic_id = tokenizer::to_integer_i64(&topic);
            let dmct = self.direct_messages_chat_topic_id.load(Ordering::SeqCst);
            self.send_request(Function::getDirectMessagesChatTopic(
                td_api::getDirectMessagesChatTopic {
                    chat_id: self.as_chat_id(&chat_id),
                    topic_id: if topic_id != 0 { topic_id } else { dmct },
                },
            ));
            true
        } else if op == "gdmcth" {
            let (chat_id, limit, from_message_id, offset) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, rest1) = tokenizer::split(&rest0, ' ');
                let (w2, w3) = tokenizer::split(&rest1, ' ');
                (w0, w1, w2, w3)
            };
            self.send_request(Function::getDirectMessagesChatTopicHistory(
                td_api::getDirectMessagesChatTopicHistory {
                    chat_id: self.as_chat_id(&chat_id),
                    topic_id: self.direct_messages_chat_topic_id.load(Ordering::SeqCst),
                    from_message_id: as_message_id(&from_message_id),
                    offset: tokenizer::to_integer_i32(&offset),
                    limit: as_limit(&limit, 10),
                },
            ));
            true
        } else if op == "gdmctmbd" {
            let (chat_id, date) = tokenizer::split(args, ' ');
            self.send_request(Function::getDirectMessagesChatTopicMessageByDate(
                td_api::getDirectMessagesChatTopicMessageByDate {
                    chat_id: self.as_chat_id(&chat_id),
                    topic_id: self.direct_messages_chat_topic_id.load(Ordering::SeqCst),
                    date: tokenizer::to_integer_i32(&date),
                },
            ));
            true
        } else if op == "ddmcth" {
            self.send_request(Function::deleteDirectMessagesChatTopicHistory(
                td_api::deleteDirectMessagesChatTopicHistory {
                    chat_id: self.as_chat_id(args),
                    topic_id: self.direct_messages_chat_topic_id.load(Ordering::SeqCst),
                },
            ));
            true
        } else if op == "ddmctmbd" {
            let (chat_id, min_date, max_date) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            self.send_request(Function::deleteDirectMessagesChatTopicMessagesByDate(
                td_api::deleteDirectMessagesChatTopicMessagesByDate {
                    chat_id: self.as_chat_id(&chat_id),
                    topic_id: self.direct_messages_chat_topic_id.load(Ordering::SeqCst),
                    min_date: tokenizer::to_integer_i32(&min_date),
                    max_date: tokenizer::to_integer_i32(&max_date),
                },
            ));
            true
        } else if op == "sdmctimau" {
            let (chat_id, flag) = tokenizer::split(args, ' ');
            self.send_request(Function::setDirectMessagesChatTopicIsMarkedAsUnread(
                td_api::setDirectMessagesChatTopicIsMarkedAsUnread {
                    chat_id: self.as_chat_id(&chat_id),
                    topic_id: self.direct_messages_chat_topic_id.load(Ordering::SeqCst),
                    is_marked_as_unread: as_bool(&flag),
                },
            ));
            true
        } else if op == "uadmctm" {
            self.send_request(Function::unpinAllDirectMessagesChatTopicMessages(
                td_api::unpinAllDirectMessagesChatTopicMessages {
                    chat_id: self.as_chat_id(args),
                    topic_id: self.direct_messages_chat_topic_id.load(Ordering::SeqCst),
                },
            ));
            true
        } else if op == "radmctr" {
            self.send_request(Function::readAllDirectMessagesChatTopicReactions(
                td_api::readAllDirectMessagesChatTopicReactions {
                    chat_id: self.as_chat_id(args),
                    topic_id: self.direct_messages_chat_topic_id.load(Ordering::SeqCst),
                },
            ));
            true
        } else if op == "gdmctr" {
            self.send_request(Function::getDirectMessagesChatTopicRevenue(
                td_api::getDirectMessagesChatTopicRevenue {
                    chat_id: self.as_chat_id(args),
                    topic_id: self.direct_messages_chat_topic_id.load(Ordering::SeqCst),
                },
            ));
            true
        } else if op == "tdmctcsum" {
            let (chat_id, can_send, refund) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            self.send_request(
                Function::toggleDirectMessagesChatTopicCanSendUnpaidMessages(
                    td_api::toggleDirectMessagesChatTopicCanSendUnpaidMessages {
                        chat_id: self.as_chat_id(&chat_id),
                        topic_id: self.direct_messages_chat_topic_id.load(Ordering::SeqCst),
                        can_send_unpaid_messages: as_bool(&can_send),
                        refund_payments: as_bool(&refund),
                    },
                ),
            );
            true
        } else if op == "lsmt" {
            self.send_request(Function::loadSavedMessagesTopics(
                td_api::loadSavedMessagesTopics {
                    limit: as_limit(args, 10),
                },
            ));
            true
        } else if op == "gsmth" {
            let (limit, from_message_id, offset) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            self.send_request(Function::getSavedMessagesTopicHistory(
                td_api::getSavedMessagesTopicHistory {
                    saved_messages_topic_id: self.get_saved_messages_topic_id(),
                    from_message_id: as_message_id(&from_message_id),
                    offset: tokenizer::to_integer_i32(&offset),
                    limit: as_limit(&limit, 10),
                },
            ));
            true
        } else if op == "gsmtmbd" {
            self.send_request(Function::getSavedMessagesTopicMessageByDate(
                td_api::getSavedMessagesTopicMessageByDate {
                    saved_messages_topic_id: self.get_saved_messages_topic_id(),
                    date: tokenizer::to_integer_i32(args),
                },
            ));
            true
        } else if op == "dsmth" && args.is_empty() {
            self.send_request(Function::deleteSavedMessagesTopicHistory(
                td_api::deleteSavedMessagesTopicHistory {
                    saved_messages_topic_id: self.get_saved_messages_topic_id(),
                },
            ));
            true
        } else if op == "dsmtmbd" {
            let (min_date, max_date) = tokenizer::split(args, ' ');
            self.send_request(Function::deleteSavedMessagesTopicMessagesByDate(
                td_api::deleteSavedMessagesTopicMessagesByDate {
                    saved_messages_topic_id: self.get_saved_messages_topic_id(),
                    min_date: tokenizer::to_integer_i32(&min_date),
                    max_date: tokenizer::to_integer_i32(&max_date),
                },
            ));
            true
        } else if op == "tsmtip" {
            self.send_request(Function::toggleSavedMessagesTopicIsPinned(
                td_api::toggleSavedMessagesTopicIsPinned {
                    saved_messages_topic_id: self.get_saved_messages_topic_id(),
                    is_pinned: as_bool(args),
                },
            ));
            true
        } else if op == "spsmt" {
            self.send_request(Function::setPinnedSavedMessagesTopics(
                td_api::setPinnedSavedMessagesTopics {
                    saved_messages_topic_ids: autosplit(args)
                        .iter()
                        .map(|t| {
                            let id = tokenizer::to_integer_i64(trim_td(t));
                            if id == -1 { 2666000 } else { id }
                        })
                        .collect(),
                },
            ));
            true
        } else if op == "cco" || op == "ccop" {
            let (chat_id, name) = tokenizer::split(args, ' ');
            self.send_request(Function::createCommunity(td_api::createCommunity {
                name,
                chat_id: self.as_chat_id(&chat_id),
                is_chat_hidden: op == "cco",
            }));
            true
        } else if op == "scn" {
            let (community_id, name) = tokenizer::split(args, ' ');
            self.send_request(Function::setCommunityName(td_api::setCommunityName {
                community_id: tokenizer::to_integer_i64(trim_td(&community_id)),
                name,
            }));
            true
        } else if op == "gcc" {
            let (user_id, offset_chat_id, limit) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            self.send_request(Function::getGroupsInCommon(td_api::getGroupsInCommon {
                user_id: self.as_user_id(&user_id),
                offset_chat_id: self.as_chat_id(&offset_chat_id),
                limit: as_limit(&limit, 100),
            }));
            true
        } else if op == "gh" || op == "ghl" || op == "gmth" || op == "gfth" {
            // The C++ `get_args(args, args)` recursion: each option eats its
            // word, the trailing split block takes the remainder.
            let (chat_id, mut tail) = tokenizer::split(args, ' ');
            let mut thread_message_id = String::new();
            let mut forum_topic_id = String::new();
            if op == "gmth" {
                let (word, rest) = tokenizer::split(&tail, ' ');
                thread_message_id = word;
                tail = rest;
            }
            if op == "gfth" {
                let (word, rest) = tokenizer::split(&tail, ' ');
                forum_topic_id = word;
                tail = rest;
            }
            // The remainder after the option words — the C++
            // `get_args(args, limit, from_message_id, offset)` recursion.
            let (limit, from_message_id, offset) = {
                let (w0, rest0) = tokenizer::split(&tail, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            if op == "gmth" {
                self.send_request(Function::getMessageThreadHistory(
                    td_api::getMessageThreadHistory {
                        chat_id: self.as_chat_id(&chat_id),
                        message_id: as_message_id(&thread_message_id),
                        from_message_id: as_message_id(&from_message_id),
                        offset: tokenizer::to_integer_i32(&offset),
                        limit: as_limit(&limit, 10),
                    },
                ));
            } else if op == "gfth" {
                self.send_request(Function::getForumTopicHistory(
                    td_api::getForumTopicHistory {
                        chat_id: self.as_chat_id(&chat_id),
                        forum_topic_id: tokenizer::to_integer_i32(trim_td(&forum_topic_id)),
                        from_message_id: as_message_id(&from_message_id),
                        offset: tokenizer::to_integer_i32(&offset),
                        limit: as_limit(&limit, 10),
                    },
                ));
            } else {
                self.send_request(Function::getChatHistory(td_api::getChatHistory {
                    chat_id: self.as_chat_id(&chat_id),
                    from_message_id: as_message_id(&from_message_id),
                    offset: tokenizer::to_integer_i32(&offset),
                    limit: as_limit(&limit, 10),
                    only_local: op == "ghl",
                }));
            }
            true
        } else if op == "gcsm" {
            self.send_request(Function::getChatScheduledMessages(
                td_api::getChatScheduledMessages {
                    chat_id: self.as_chat_id(args),
                },
            ));
            true
        } else if op == "gpkp" {
            self.send_request(Function::getPasskeyParameters(td_api::getPasskeyParameters));
            true
        } else if op == "alpk" {
            let (client_data, attestation_object) = tokenizer::split(args, ' ');
            self.send_request(Function::addLoginPasskey(td_api::addLoginPasskey {
                client_data,
                attestation_object: base64url_decode(&attestation_object),
            }));
            true
        } else if op == "glpk" {
            self.send_request(Function::getLoginPasskeys(td_api::getLoginPasskeys));
            true
        } else if op == "rlpk" {
            self.send_request(Function::removeLoginPasskey(td_api::removeLoginPasskey {
                passkey_id: args.to_owned(),
            }));
            true
        } else if op == "sdrt" {
            self.send_request(Function::setDefaultReactionType(
                td_api::setDefaultReactionType {
                    reaction_type: as_reaction_type(args),
                },
            ));
            true
        } else if op == "ger" {
            self.send_request(Function::getEmojiReaction(td_api::getEmojiReaction {
                emoji: args.to_owned(),
            }));
            true
        } else if op == "gcera" {
            self.send_request(Function::getCustomEmojiReactionAnimations(
                td_api::getCustomEmojiReactionAnimations,
            ));
            true
        } else if op == "gmar" {
            let (chat_id, message_id) = tokenizer::split(args, ' ');
            self.send_request(Function::getMessageAvailableReactions(
                td_api::getMessageAvailableReactions {
                    chat_id: self.as_chat_id(&chat_id),
                    message_id: as_message_id(&message_id),
                    row_size: 8,
                },
            ));
            true
        } else if op == "crr" {
            self.send_request(Function::clearRecentReactions(td_api::clearRecentReactions));
            true
        } else if op == "amr" || op == "react" {
            let (chat_id, message_id, reaction, is_big, update_recent) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, rest1) = tokenizer::split(&rest0, ' ');
                let (w2, rest2) = tokenizer::split(&rest1, ' ');
                let (w3, w4) = tokenizer::split(&rest2, ' ');
                (w0, w1, w2, w3, w4)
            };
            self.send_request(Function::addMessageReaction(td_api::addMessageReaction {
                chat_id: self.as_chat_id(&chat_id),
                message_id: as_message_id(&message_id),
                reaction_type: as_reaction_type(&reaction),
                is_big: as_bool(&is_big),
                update_recent_reactions: as_bool(&update_recent),
            }));
            true
        } else if op == "rmr" {
            let (chat_id, message_id, reaction) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            self.send_request(Function::removeMessageReaction(
                td_api::removeMessageReaction {
                    chat_id: self.as_chat_id(&chat_id),
                    message_id: as_message_id(&message_id),
                    reaction_type: as_reaction_type(&reaction),
                },
            ));
            true
        } else if op == "darmrfs" {
            let (chat_id, sender_id) = tokenizer::split(args, ' ');
            self.send_request(Function::deleteAllRecentMessageReactionsFromSender(
                td_api::deleteAllRecentMessageReactionsFromSender {
                    chat_id: self.as_chat_id(&chat_id),
                    sender_id: self.as_message_sender(&sender_id),
                },
            ));
            true
        } else if op == "dmrfs" {
            let (chat_id, message_id, sender_id) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            self.send_request(Function::deleteMessageReactionsFromSender(
                td_api::deleteMessageReactionsFromSender {
                    chat_id: self.as_chat_id(&chat_id),
                    message_id: as_message_id(&message_id),
                    sender_id: self.as_message_sender(&sender_id),
                },
            ));
            true
        } else if op == "reactbot" || op == "reactbotbig" {
            let (chat_id, message_id, reactions) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            self.send_request(Function::setMessageReactions(td_api::setMessageReactions {
                chat_id: self.as_chat_id(&chat_id),
                message_id: as_message_id(&message_id),
                reaction_types: autosplit(&reactions)
                    .iter()
                    .filter_map(|t| as_reaction_type(t))
                    .collect(),
                is_big: op == "reactbotbig",
            }));
            true
        } else if op == "gcapmrs" {
            self.send_request(Function::getChatAvailablePaidMessageReactionSenders(
                td_api::getChatAvailablePaidMessageReactionSenders {
                    chat_id: self.as_chat_id(args),
                },
            ));
            true
        } else if op == "appmr" || op == "appmra" || op == "appmrd" {
            let (chat_id, message_id, star_count, reaction_chat_id) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, rest1) = tokenizer::split(&rest0, ' ');
                let (w2, w3) = tokenizer::split(&rest1, ' ');
                (w0, w1, w2, w3)
            };
            let r#type = if op == "appmr" {
                let reaction_chat_id = self.as_chat_id(&reaction_chat_id);
                if reaction_chat_id != 0 {
                    Some(td_api::PaidReactionType::paidReactionTypeChat(
                        td_api::paidReactionTypeChat {
                            chat_id: reaction_chat_id,
                        },
                    ))
                } else {
                    Some(td_api::PaidReactionType::paidReactionTypeRegular(
                        td_api::paidReactionTypeRegular,
                    ))
                }
            } else if op == "appmra" {
                Some(td_api::PaidReactionType::paidReactionTypeAnonymous(
                    td_api::paidReactionTypeAnonymous,
                ))
            } else {
                // `appmrd` leaves the type unset in the C++ too.
                None
            };
            self.send_request(Function::addPendingPaidMessageReaction(
                td_api::addPendingPaidMessageReaction {
                    chat_id: self.as_chat_id(&chat_id),
                    message_id: as_message_id(&message_id),
                    star_count: tokenizer::to_integer_i64(&star_count),
                    r#type,
                },
            ));
            true
        } else if op == "cppmr" {
            let (chat_id, message_id) = tokenizer::split(args, ' ');
            self.send_request(Function::commitPendingPaidMessageReactions(
                td_api::commitPendingPaidMessageReactions {
                    chat_id: self.as_chat_id(&chat_id),
                    message_id: as_message_id(&message_id),
                },
            ));
            true
        } else if op == "rppmr" {
            let (chat_id, message_id) = tokenizer::split(args, ' ');
            self.send_request(Function::removePendingPaidMessageReactions(
                td_api::removePendingPaidMessageReactions {
                    chat_id: self.as_chat_id(&chat_id),
                    message_id: as_message_id(&message_id),
                },
            ));
            true
        } else if op == "spmrt" || op == "spmrta" {
            let (chat_id, message_id, reaction_chat_id) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            let r#type = if op == "spmrt" {
                let reaction_chat_id = self.as_chat_id(&reaction_chat_id);
                if reaction_chat_id != 0 {
                    Some(td_api::PaidReactionType::paidReactionTypeChat(
                        td_api::paidReactionTypeChat {
                            chat_id: reaction_chat_id,
                        },
                    ))
                } else {
                    Some(td_api::PaidReactionType::paidReactionTypeRegular(
                        td_api::paidReactionTypeRegular,
                    ))
                }
            } else {
                Some(td_api::PaidReactionType::paidReactionTypeAnonymous(
                    td_api::paidReactionTypeAnonymous,
                ))
            };
            self.send_request(Function::setPaidMessageReactionType(
                td_api::setPaidMessageReactionType {
                    chat_id: self.as_chat_id(&chat_id),
                    message_id: as_message_id(&message_id),
                    r#type,
                },
            ));
            true
        } else if op == "gmars" {
            let (chat_id, message_id, reaction, offset, limit) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, rest1) = tokenizer::split(&rest0, ' ');
                let (w2, rest2) = tokenizer::split(&rest1, ' ');
                let (w3, w4) = tokenizer::split(&rest2, ' ');
                (w0, w1, w2, w3, w4)
            };
            self.send_request(Function::getMessageAddedReactions(
                td_api::getMessageAddedReactions {
                    chat_id: self.as_chat_id(&chat_id),
                    message_id: as_message_id(&message_id),
                    reaction_type: as_reaction_type(&reaction),
                    offset,
                    limit: as_limit(&limit, 10),
                },
            ));
            true
        } else if op == "gsmts" {
            self.send_request(Function::getSavedMessagesTags(
                td_api::getSavedMessagesTags {
                    saved_messages_topic_id: self.get_saved_messages_topic_id(),
                },
            ));
            true
        } else if op == "ssmtl" {
            let (reaction, label) = tokenizer::split(args, ' ');
            self.send_request(Function::setSavedMessagesTagLabel(
                td_api::setSavedMessagesTagLabel {
                    tag: as_reaction_type(&reaction),
                    label,
                },
            ));
            true
        } else if op == "gme" {
            self.send_request(Function::getMessageEffect(td_api::getMessageEffect {
                effect_id: tokenizer::to_integer_i64(args),
            }));
            true
        } else if op == "gmpf" {
            let (chat_id, message_id, offset, limit) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, rest1) = tokenizer::split(&rest0, ' ');
                let (w2, w3) = tokenizer::split(&rest1, ' ');
                (w0, w1, w2, w3)
            };
            self.send_request(Function::getMessagePublicForwards(
                td_api::getMessagePublicForwards {
                    chat_id: self.as_chat_id(&chat_id),
                    message_id: as_message_id(&message_id),
                    offset,
                    limit: as_limit(&limit, 10),
                },
            ));
            true
        } else if op == "gspf" {
            let (chat_id, story_id, offset, limit) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, rest1) = tokenizer::split(&rest0, ' ');
                let (w2, w3) = tokenizer::split(&rest1, ' ');
                (w0, w1, w2, w3)
            };
            self.send_request(Function::getStoryPublicForwards(
                td_api::getStoryPublicForwards {
                    story_poster_chat_id: self.as_chat_id(&chat_id),
                    story_id: tokenizer::to_integer_i32(trim_td(&story_id)),
                    offset,
                    limit: as_limit(&limit, 10),
                },
            ));
            true
        } else if op == "gcsa" {
            self.send_request(Function::getChatStoryAlbums(td_api::getChatStoryAlbums {
                chat_id: self.as_chat_id(args),
            }));
            true
        } else if op == "gsas" {
            let (chat_id, story_album_id, offset, limit) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, rest1) = tokenizer::split(&rest0, ' ');
                let (w2, w3) = tokenizer::split(&rest1, ' ');
                (w0, w1, w2, w3)
            };
            self.send_request(Function::getStoryAlbumStories(
                td_api::getStoryAlbumStories {
                    chat_id: self.as_chat_id(&chat_id),
                    story_album_id: tokenizer::to_integer_i32(trim_td(&story_album_id)),
                    offset: tokenizer::to_integer_i32(&offset),
                    limit: as_limit(&limit, 10),
                },
            ));
            true
        } else if op == "csa" {
            let (chat_id, name, story_ids) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            self.send_request(Function::createStoryAlbum(td_api::createStoryAlbum {
                story_poster_chat_id: self.as_chat_id(&chat_id),
                name,
                story_ids: autosplit(&story_ids)
                    .iter()
                    .map(|t| tokenizer::to_integer_i32(trim_td(t)))
                    .collect(),
            }));
            true
        } else if op == "rsa" {
            let (chat_id, story_album_ids) = tokenizer::split(args, ' ');
            self.send_request(Function::reorderStoryAlbums(td_api::reorderStoryAlbums {
                chat_id: self.as_chat_id(&chat_id),
                story_album_ids: autosplit(&story_album_ids)
                    .iter()
                    .map(|t| tokenizer::to_integer_i32(trim_td(t)))
                    .collect(),
            }));
            true
        } else if op == "dsa" {
            let (chat_id, story_album_id) = tokenizer::split(args, ' ');
            self.send_request(Function::deleteStoryAlbum(td_api::deleteStoryAlbum {
                chat_id: self.as_chat_id(&chat_id),
                story_album_id: tokenizer::to_integer_i32(trim_td(&story_album_id)),
            }));
            true
        } else if op == "ssan" {
            let (chat_id, story_album_id, name) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            self.send_request(Function::setStoryAlbumName(td_api::setStoryAlbumName {
                chat_id: self.as_chat_id(&chat_id),
                story_album_id: tokenizer::to_integer_i32(trim_td(&story_album_id)),
                name,
            }));
            true
        } else if op == "asas" || op == "rsas" || op == "rosas" {
            let (chat_id, story_album_id, story_ids) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            let story_ids = autosplit(&story_ids)
                .iter()
                .map(|t| tokenizer::to_integer_i32(trim_td(t)))
                .collect();
            match op {
                "asas" => self.send_request(Function::addStoryAlbumStories(
                    td_api::addStoryAlbumStories {
                        chat_id: self.as_chat_id(&chat_id),
                        story_album_id: tokenizer::to_integer_i32(trim_td(&story_album_id)),
                        story_ids,
                    },
                )),
                "rsas" => self.send_request(Function::removeStoryAlbumStories(
                    td_api::removeStoryAlbumStories {
                        chat_id: self.as_chat_id(&chat_id),
                        story_album_id: tokenizer::to_integer_i32(trim_td(&story_album_id)),
                        story_ids,
                    },
                )),
                _ => self.send_request(Function::reorderStoryAlbumStories(
                    td_api::reorderStoryAlbumStories {
                        chat_id: self.as_chat_id(&chat_id),
                        story_album_id: tokenizer::to_integer_i32(trim_td(&story_album_id)),
                        story_ids,
                    },
                )),
            };
            true
        } else if op == "ghf" {
            let chat_id = self.as_chat_id(args);
            self.get_history_chat_id.store(chat_id, Ordering::SeqCst);
            self.send_request(Function::getChatHistory(td_api::getChatHistory {
                chat_id,
                from_message_id: i64::MAX,
                offset: 0,
                limit: 100,
                only_local: false,
            }));
            true
        } else if op == "replies" {
            let (chat_id, filter) = tokenizer::split(args, ' ');
            self.send_request(Function::searchChatMessages(td_api::searchChatMessages {
                chat_id: self.as_chat_id(&chat_id),
                topic_id: self.get_message_topic_id(),
                query: String::new(),
                sender_id: None,
                from_message_id: 0,
                offset: 0,
                limit: 100,
                filter: as_search_messages_filter(&filter),
            }));
            true
        } else if op == "spvf" {
            let chat_id = self.as_chat_id(args);
            self.search_chat_id.store(chat_id, Ordering::SeqCst);
            self.send_request(Function::searchChatMessages(td_api::searchChatMessages {
                chat_id,
                topic_id: self.get_message_topic_id(),
                query: String::new(),
                sender_id: None,
                from_message_id: 0,
                offset: 0,
                limit: 100,
                filter: as_search_messages_filter("pvi"),
            }));
            true
        } else if op == "Search"
            || op == "SearchA"
            || op == "SearchM"
            || op == "SearchP"
            || op == "SearchG"
            || op == "SearchC"
            // 16c6ebcfb — SearchChatPhoto has its own arm below; the
            // prefix match must not shadow it
            || (op.starts_with("SearchC") && op != "SearchChatPhoto")
        {
            let (query, limit, filter, offset) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, rest1) = tokenizer::split(&rest0, ' ');
                let (w2, w3) = tokenizer::split(&rest1, ' ');
                (w0, w1, w2, w3)
            };
            let chat_list = match op {
                "SearchA" => Some(td_api::ChatList::chatListArchive(td_api::chatListArchive)),
                "SearchM" => Some(td_api::ChatList::chatListMain(td_api::chatListMain)),
                _ => None,
            };
            let chat_type_filter = if op == "SearchP" {
                Some(
                    td_api::SearchMessagesChatTypeFilter::searchMessagesChatTypeFilterPrivate(
                        td_api::searchMessagesChatTypeFilterPrivate,
                    ),
                )
            } else if op == "SearchG" {
                Some(
                    td_api::SearchMessagesChatTypeFilter::searchMessagesChatTypeFilterGroup(
                        td_api::searchMessagesChatTypeFilterGroup,
                    ),
                )
            } else if op == "SearchC" {
                Some(
                    td_api::SearchMessagesChatTypeFilter::searchMessagesChatTypeFilterChannel(
                        td_api::searchMessagesChatTypeFilterChannel,
                    ),
                )
            } else if op.starts_with("SearchC") {
                Some(
                    td_api::SearchMessagesChatTypeFilter::searchMessagesChatTypeFilterCommunity(
                        td_api::searchMessagesChatTypeFilterCommunity {
                            community_id: tokenizer::to_integer_i64(
                                op.strip_prefix("SearchC").unwrap_or(""),
                            ),
                        },
                    ),
                )
            } else {
                None
            };
            self.send_request(Function::searchMessages(td_api::searchMessages {
                chat_list,
                query,
                offset,
                limit: as_limit(&limit, 10),
                filter: as_search_messages_filter(&filter),
                chat_type_filter,
                min_date: 1,
                max_date: 2147483647,
            }));
            true
        } else if op == "SCM" {
            let (chat_id, tail) = tokenizer::split(args, ' ');
            let (limit, query) = get_search_query(&tail);
            self.send_request(Function::searchChatMessages(td_api::searchChatMessages {
                chat_id: self.as_chat_id(&chat_id),
                topic_id: self.get_message_topic_id(),
                query,
                sender_id: None,
                from_message_id: 0,
                offset: 0,
                limit,
                filter: None,
            }));
            true
        } else if op == "SMME" {
            let (chat_id, limit) = tokenizer::split(args, ' ');
            self.send_request(Function::searchChatMessages(td_api::searchChatMessages {
                chat_id: self.as_chat_id(&chat_id),
                topic_id: self.get_message_topic_id(),
                query: String::new(),
                sender_id: Some(td_api::MessageSender::messageSenderUser(
                    td_api::messageSenderUser {
                        user_id: self.my_id.load(Ordering::SeqCst),
                    },
                )),
                from_message_id: 0,
                offset: 0,
                limit: as_limit(&limit, 10),
                filter: None,
            }));
            true
        } else if op == "SMU" || op == "SMC" {
            let (chat_id, sender_id, from_message_id, limit) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, rest1) = tokenizer::split(&rest0, ' ');
                let (w2, w3) = tokenizer::split(&rest1, ' ');
                (w0, w1, w2, w3)
            };
            self.send_request(Function::searchChatMessages(td_api::searchChatMessages {
                chat_id: self.as_chat_id(&chat_id),
                topic_id: self.get_message_topic_id(),
                query: String::new(),
                sender_id: self.as_message_sender(&sender_id),
                from_message_id: as_message_id(&from_message_id),
                offset: 0,
                limit: as_limit(&limit, 10),
                filter: None,
            }));
            true
        } else if op == "SM" {
            let (chat_id, filter, limit, offset_message_id, offset) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, rest1) = tokenizer::split(&rest0, ' ');
                let (w2, rest2) = tokenizer::split(&rest1, ' ');
                let (w3, w4) = tokenizer::split(&rest2, ' ');
                (w0, w1, w2, w3, w4)
            };
            self.send_request(Function::searchChatMessages(td_api::searchChatMessages {
                chat_id: self.as_chat_id(&chat_id),
                topic_id: self.get_message_topic_id(),
                query: String::new(),
                sender_id: None,
                from_message_id: as_message_id(&offset_message_id),
                offset: tokenizer::to_integer_i32(&offset),
                limit: as_limit(&limit, 10),
                filter: as_search_messages_filter(&filter),
            }));
            true
        } else if op == "SC" {
            let (limit, offset, only_missed) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            self.send_request(Function::searchCallMessages(td_api::searchCallMessages {
                offset,
                limit: as_limit(&limit, 10),
                only_missed: as_bool(&only_missed),
            }));
            true
        } else if op == "sodm" {
            let (limit, query) = get_search_query(args);
            self.send_request(Function::searchOutgoingDocumentMessages(
                td_api::searchOutgoingDocumentMessages { query, limit },
            ));
            true
        } else if op == "gppsl" {
            self.send_request(Function::getPublicPostSearchLimits(
                td_api::getPublicPostSearchLimits {
                    query: args.to_owned(),
                },
            ));
            true
        } else if op == "spps" {
            let (query, limit, star_count, offset) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, rest1) = tokenizer::split(&rest0, ' ');
                let (w2, w3) = tokenizer::split(&rest1, ' ');
                (w0, w1, w2, w3)
            };
            self.send_request(Function::searchPublicPosts(td_api::searchPublicPosts {
                query,
                offset,
                limit: as_limit(&limit, 10),
                star_count: tokenizer::to_integer_i64(&star_count),
            }));
            true
        } else if op == "spmbt" {
            let (tag, limit, offset) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            self.send_request(Function::searchPublicMessagesByTag(
                td_api::searchPublicMessagesByTag {
                    tag,
                    offset,
                    limit: as_limit(&limit, 10),
                },
            ));
            true
        } else if op == "spsbt" {
            let (chat_id, tag, limit, offset) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, rest1) = tokenizer::split(&rest0, ' ');
                let (w2, w3) = tokenizer::split(&rest1, ' ');
                (w0, w1, w2, w3)
            };
            self.send_request(Function::searchPublicStoriesByTag(
                td_api::searchPublicStoriesByTag {
                    story_poster_chat_id: self.as_chat_id(&chat_id),
                    tag,
                    offset,
                    limit: as_limit(&limit, 10),
                },
            ));
            true
        } else if op == "spsbl" {
            let (country_code, state, city, street, limit, offset) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, rest1) = tokenizer::split(&rest0, ' ');
                let (w2, rest2) = tokenizer::split(&rest1, ' ');
                let (w3, rest3) = tokenizer::split(&rest2, ' ');
                let (w4, w5) = tokenizer::split(&rest3, ' ');
                (w0, w1, w2, w3, w4, w5)
            };
            self.send_request(Function::searchPublicStoriesByLocation(
                td_api::searchPublicStoriesByLocation {
                    address: Some(td_api::LocationAddress {
                        country_code,
                        state,
                        city,
                        street,
                    }),
                    offset,
                    limit: as_limit(&limit, 10),
                },
            ));
            true
        } else if op == "spsbv" {
            let (venue_provider, venue_id, limit, offset) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, rest1) = tokenizer::split(&rest0, ' ');
                let (w2, w3) = tokenizer::split(&rest1, ' ');
                (w0, w1, w2, w3)
            };
            self.send_request(Function::searchPublicStoriesByVenue(
                td_api::searchPublicStoriesByVenue {
                    venue_provider,
                    venue_id,
                    offset,
                    limit: as_limit(&limit, 10),
                },
            ));
            true
        } else if op == "gsfh" {
            let (tag_prefix, limit) = tokenizer::split(args, ' ');
            self.send_request(Function::getSearchedForTags(td_api::getSearchedForTags {
                tag_prefix,
                limit: as_limit(&limit, 10),
            }));
            true
        } else if op == "rsfh" {
            self.send_request(Function::removeSearchedForTag(
                td_api::removeSearchedForTag {
                    tag: args.to_owned(),
                },
            ));
            true
        } else if op == "csfh" || op == "csfc" {
            self.send_request(Function::clearSearchedForTags(
                td_api::clearSearchedForTags {
                    clear_cashtags: op == "csfc",
                },
            ));
            true
        } else if op == "dacm" {
            self.send_request(Function::deleteAllCallMessages(
                td_api::deleteAllCallMessages {
                    revoke: as_bool(args),
                },
            ));
            true
        } else if op == "scrlm" {
            let (chat_id, limit) = tokenizer::split(args, ' ');
            self.send_request(Function::searchChatRecentLocationMessages(
                td_api::searchChatRecentLocationMessages {
                    chat_id: self.as_chat_id(&chat_id),
                    limit: as_limit(&limit, 10),
                },
            ));
            true
        } else if op == "gcmca" {
            let (chat_id, filter, from_message_id) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            self.send_request(Function::getChatMessageCalendar(
                td_api::getChatMessageCalendar {
                    chat_id: self.as_chat_id(&chat_id),
                    topic_id: self.get_message_topic_id(),
                    filter: as_search_messages_filter(&filter),
                    from_message_id: as_message_id(&from_message_id),
                },
            ));
            true
        } else if op == "SearchAudio"
            || op == "SearchDocument"
            || op == "SearchPhoto"
            || op == "SearchChatPhoto"
        {
            let (chat_id, offset_message_id, tail) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            let (limit, query) = get_search_query(&tail);
            self.send_request(Function::searchChatMessages(td_api::searchChatMessages {
                chat_id: self.as_chat_id(&chat_id),
                topic_id: self.get_message_topic_id(),
                query,
                sender_id: None,
                from_message_id: as_message_id(&offset_message_id),
                offset: 0,
                limit,
                filter: as_search_messages_filter(op),
            }));
            true
        } else if op == "ssms" {
            let (tag, from_message_id, offset, tail) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, rest1) = tokenizer::split(&rest0, ' ');
                let (w2, w3) = tokenizer::split(&rest1, ' ');
                (w0, w1, w2, w3)
            };
            let (limit, query) = get_search_query(&tail);
            self.send_request(Function::searchSavedMessages(td_api::searchSavedMessages {
                saved_messages_topic_id: self.get_saved_messages_topic_id(),
                tag: as_reaction_type(&tag),
                query,
                from_message_id: as_message_id(&from_message_id),
                offset: tokenizer::to_integer_i32(&offset),
                limit,
            }));
            true
        } else if op == "gcmbd" {
            let (chat_id, date) = tokenizer::split(args, ' ');
            self.send_request(Function::getChatMessageByDate(
                td_api::getChatMessageByDate {
                    chat_id: self.as_chat_id(&chat_id),
                    date: tokenizer::to_integer_i32(&date),
                },
            ));
            true
        } else if op == "gcsmp" {
            let (chat_id, filter, from_message_id, limit) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, rest1) = tokenizer::split(&rest0, ' ');
                let (w2, w3) = tokenizer::split(&rest1, ' ');
                (w0, w1, w2, w3)
            };
            self.send_request(Function::getChatSparseMessagePositions(
                td_api::getChatSparseMessagePositions {
                    chat_id: self.as_chat_id(&chat_id),
                    filter: as_search_messages_filter(&filter),
                    from_message_id: as_message_id(&from_message_id),
                    limit: as_limit(&limit, 10),
                    saved_messages_topic_id: self.get_saved_messages_topic_id(),
                },
            ));
            true
        } else if op == "gcmc" {
            let (chat_id, filter, return_local) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            self.send_request(Function::getChatMessageCount(td_api::getChatMessageCount {
                chat_id: self.as_chat_id(&chat_id),
                topic_id: self.get_message_topic_id(),
                filter: as_search_messages_filter(&filter),
                return_local: as_bool(&return_local),
            }));
            true
        } else if op == "gcmp" {
            let (chat_id, message_id, filter) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            self.send_request(Function::getChatMessagePosition(
                td_api::getChatMessagePosition {
                    chat_id: self.as_chat_id(&chat_id),
                    topic_id: self.get_message_topic_id(),
                    filter: as_search_messages_filter(&filter),
                    message_id: as_message_id(&message_id),
                },
            ));
            true
        } else if op == "gup" || op == "gupp" || op == "gupa" {
            let (user_id, offset, limit) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            if op == "gupa" {
                self.send_request(Function::getUserProfileAudios(
                    td_api::getUserProfileAudios {
                        user_id: self.as_user_id(&user_id),
                        offset: tokenizer::to_integer_i32(&offset),
                        limit: as_limit(&limit, 10),
                    },
                ));
            } else {
                self.send_request(Function::getUserProfilePhotos(
                    td_api::getUserProfilePhotos {
                        user_id: self.as_user_id(&user_id),
                        offset: tokenizer::to_integer_i32(&offset),
                        limit: as_limit(&limit, 10),
                    },
                ));
            }
            true
        } else if op == "apa" {
            let (audio, duration, title, performer) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, rest1) = tokenizer::split(&rest0, ' ');
                let (w2, w3) = tokenizer::split(&rest1, ' ');
                (w0, w1, w2, w3)
            };
            let mut audio = self.as_input_audio(&audio, tokenizer::to_integer_i32(&duration));
            audio.title = title;
            audio.performer = performer;
            self.send_request(Function::addProfileAudio(td_api::addProfileAudio {
                audio: Some(audio),
            }));
            true
        } else if op == "ipa" {
            self.send_request(Function::isProfileAudio(td_api::isProfileAudio {
                file_id: tokenizer::to_integer_i32(trim_td(args)),
            }));
            true
        } else if op == "spap" {
            let (file_id, after_file_id) = tokenizer::split(args, ' ');
            self.send_request(Function::setProfileAudioPosition(
                td_api::setProfileAudioPosition {
                    file_id: tokenizer::to_integer_i32(trim_td(&file_id)),
                    after_file_id: tokenizer::to_integer_i32(trim_td(&after_file_id)),
                },
            ));
            true
        } else if op == "rpa" {
            self.send_request(Function::removeProfileAudio(td_api::removeProfileAudio {
                file_id: tokenizer::to_integer_i32(trim_td(args)),
            }));
            true
        } else if op == "dcrm" {
            let (chat_id, message_id) = tokenizer::split(args, ' ');
            self.send_request(Function::deleteChatReplyMarkup(
                td_api::deleteChatReplyMarkup {
                    chat_id: self.as_chat_id(&chat_id),
                    message_id: as_message_id(&message_id),
                },
            ));
            true
        } else if op == "glti" {
            self.send_request(Function::getLocalizationTargetInfo(
                td_api::getLocalizationTargetInfo {
                    only_local: as_bool(args),
                },
            ));
            true
        } else if op == "glpi" {
            self.send_request(Function::getLanguagePackInfo(td_api::getLanguagePackInfo {
                language_pack_id: args.to_owned(),
            }));
            true
        } else if op == "glps" {
            let (language_code, keys) = tokenizer::split(args, ' ');
            self.send_request(Function::getLanguagePackStrings(
                td_api::getLanguagePackStrings {
                    language_pack_id: language_code,
                    keys: autosplit(&keys),
                },
            ));
            true
        } else if op == "glpss" {
            let (language_database_path, language_pack, language_code, key) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, rest1) = tokenizer::split(&rest0, ' ');
                let (w2, w3) = tokenizer::split(&rest1, ' ');
                (w0, w1, w2, w3)
            };
            self.send_request(Function::getLanguagePackString(
                td_api::getLanguagePackString {
                    language_pack_database_path: language_database_path,
                    localization_target: language_pack,
                    language_pack_id: language_code,
                    key,
                },
            ));
            true
        } else if op == "synclp" {
            self.send_request(Function::synchronizeLanguagePack(
                td_api::synchronizeLanguagePack {
                    language_pack_id: args.to_owned(),
                },
            ));
            true
        } else if op == "acslp" {
            self.send_request(Function::addCustomServerLanguagePack(
                td_api::addCustomServerLanguagePack {
                    language_pack_id: args.to_owned(),
                },
            ));
            true
        } else if op == "sclp" {
            let (language_code, name, native_name, key) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, rest1) = tokenizer::split(&rest0, ' ');
                let (w2, w3) = tokenizer::split(&rest1, ' ');
                (w0, w1, w2, w3)
            };
            let strings = vec![
                td_api::LanguagePackString {
                    key,
                    value: Some(
                        td_api::LanguagePackStringValue::languagePackStringValueOrdinary(
                            td_api::languagePackStringValueOrdinary {
                                value: "Ordinary value".to_owned(),
                            },
                        ),
                    ),
                },
                td_api::LanguagePackString {
                    key: "Plu".to_owned(),
                    value: Some(
                        td_api::LanguagePackStringValue::languagePackStringValuePluralized(
                            td_api::languagePackStringValuePluralized {
                                zero_value: "Zero".to_owned(),
                                one_value: "One\0One".to_owned(),
                                two_value: "Two".to_owned(),
                                few_value: "Few".to_owned(),
                                many_value: "Many".to_owned(),
                                other_value: "Other".to_owned(),
                            },
                        ),
                    ),
                },
                td_api::LanguagePackString {
                    key: "DELETED".to_owned(),
                    value: Some(
                        td_api::LanguagePackStringValue::languagePackStringValueDeleted(
                            td_api::languagePackStringValueDeleted,
                        ),
                    ),
                },
            ];
            self.send_request(Function::setCustomLanguagePack(
                td_api::setCustomLanguagePack {
                    info: Some(as_language_pack_info(&language_code, &name, &native_name)),
                    strings,
                },
            ));
            true
        } else if op == "eclpi" {
            let (language_code, name, native_name) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            self.send_request(Function::editCustomLanguagePackInfo(
                td_api::editCustomLanguagePackInfo {
                    info: Some(as_language_pack_info(&language_code, &name, &native_name)),
                },
            ));
            true
        } else if op == "sclpsv" || op == "sclpsp" || op == "sclpsd" {
            let (language_code, key, value) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            let str_value = if op == "sclpsv" {
                td_api::LanguagePackStringValue::languagePackStringValueOrdinary(
                    td_api::languagePackStringValueOrdinary { value },
                )
            } else if op == "sclpsp" {
                td_api::LanguagePackStringValue::languagePackStringValuePluralized(
                    td_api::languagePackStringValuePluralized {
                        zero_value: value,
                        one_value: "One\0One".to_owned(),
                        two_value: "Two".to_owned(),
                        few_value: "Few".to_owned(),
                        many_value: "Many".to_owned(),
                        other_value: "Other".to_owned(),
                    },
                )
            } else {
                td_api::LanguagePackStringValue::languagePackStringValueDeleted(
                    td_api::languagePackStringValueDeleted,
                )
            };
            self.send_request(Function::setCustomLanguagePackString(
                td_api::setCustomLanguagePackString {
                    language_pack_id: language_code,
                    new_string: Some(td_api::LanguagePackString {
                        key,
                        value: Some(str_value),
                    }),
                },
            ));
            true
        } else if op == "dlp" {
            self.send_request(Function::deleteLanguagePack(td_api::deleteLanguagePack {
                language_pack_id: args.to_owned(),
            }));
            true
        } else {
            false
        }
    }

    /// cli.cpp:4398 — the options/account/session chain (M.14.4b part 1).
    /// The `gbgu` background-URL probe stays unported until the M.14.6
    /// background coercers land.
    fn op_block_messages(&self, op: &str, args: &str) -> bool {
        if op == "on" || op == "off" {
            self.send_request(Function::setOption(td_api::setOption {
                name: "online".to_owned(),
                value: Some(OptionValue::optionValueBoolean(
                    td_api::optionValueBoolean { value: op == "on" },
                )),
            }));
            true
        } else if op == "go" {
            self.send_request(Function::getOption(td_api::getOption {
                name: args.to_owned(),
            }));
            true
        } else if op == "gos" {
            self.execute(Function::getOption(td_api::getOption {
                name: args.to_owned(),
            }));
            true
        } else if op == "sob" {
            let (name, value) = tokenizer::split(args, ' ');
            self.send_request(Function::setOption(td_api::setOption {
                name,
                value: Some(OptionValue::optionValueBoolean(
                    td_api::optionValueBoolean {
                        value: as_bool(&value),
                    },
                )),
            }));
            true
        } else if op == "soe" {
            self.send_request(Function::setOption(td_api::setOption {
                name: args.to_owned(),
                value: Some(OptionValue::optionValueEmpty(td_api::optionValueEmpty)),
            }));
            true
        } else if op == "soi" {
            let (name, value) = tokenizer::split(args, ' ');
            self.send_request(Function::setOption(td_api::setOption {
                name,
                value: Some(OptionValue::optionValueInteger(
                    td_api::optionValueInteger {
                        value: tokenizer::to_integer_i64(&value),
                    },
                )),
            }));
            true
        } else if op == "sos" {
            let (name, value) = tokenizer::split(args, ' ');
            self.send_request(Function::setOption(td_api::setOption {
                name,
                value: Some(OptionValue::optionValueString(td_api::optionValueString {
                    value,
                })),
            }));
            true
        } else if op == "me" {
            self.send_request(Function::getMe(td_api::getMe));
            true
        } else if op == "sdmadt" {
            self.send_request(Function::setDefaultMessageAutoDeleteTime(
                td_api::setDefaultMessageAutoDeleteTime {
                    message_auto_delete_time: Some(td_api::MessageAutoDeleteTime {
                        time: tokenizer::to_integer_i32(args),
                    }),
                },
            ));
            true
        } else if op == "gdmadt" {
            self.send_request(Function::getDefaultMessageAutoDeleteTime(
                td_api::getDefaultMessageAutoDeleteTime,
            ));
            true
        } else if op == "sattl" {
            self.send_request(Function::setAccountTtl(td_api::setAccountTtl {
                ttl: Some(td_api::AccountTtl {
                    days: tokenizer::to_integer_i32(args),
                }),
            }));
            true
        } else if op == "gattl" {
            self.send_request(Function::getAccountTtl(td_api::getAccountTtl));
            true
        } else if op == "devices" || op == "sessions" {
            self.send_request(Function::getActiveSessions(td_api::getActiveSessions));
            true
        } else if op == "ts" {
            self.send_request(Function::terminateSession(td_api::terminateSession {
                session_id: tokenizer::to_integer_i64(args),
            }));
            true
        } else if op == "taos" {
            self.send_request(Function::terminateAllOtherSessions(
                td_api::terminateAllOtherSessions,
            ));
            true
        } else if op == "cse" {
            self.send_request(Function::confirmSession(td_api::confirmSession {
                session_id: tokenizer::to_integer_i64(args),
            }));
            true
        } else if op == "tscac" {
            let (session_id, flag) = tokenizer::split(args, ' ');
            self.send_request(Function::toggleSessionCanAcceptCalls(
                td_api::toggleSessionCanAcceptCalls {
                    session_id: tokenizer::to_integer_i64(&session_id),
                    can_accept_calls: as_bool(&flag),
                },
            ));
            true
        } else if op == "tscasc" {
            let (session_id, flag) = tokenizer::split(args, ' ');
            self.send_request(Function::toggleSessionCanAcceptSecretChats(
                td_api::toggleSessionCanAcceptSecretChats {
                    session_id: tokenizer::to_integer_i64(&session_id),
                    can_accept_secret_chats: as_bool(&flag),
                },
            ));
            true
        } else if op == "sist" {
            self.send_request(Function::setInactiveSessionTtl(
                td_api::setInactiveSessionTtl {
                    inactive_session_ttl_days: tokenizer::to_integer_i32(args),
                },
            ));
            true
        } else if op == "gcw" {
            self.send_request(Function::getConnectedWebsites(td_api::getConnectedWebsites));
            true
        } else if op == "dw" {
            self.send_request(Function::disconnectWebsite(td_api::disconnectWebsite {
                website_id: tokenizer::to_integer_i64(args),
            }));
            true
        } else if op == "daw" {
            self.send_request(Function::disconnectAllWebsites(
                td_api::disconnectAllWebsites,
            ));
            true
        } else if op == "gib" {
            self.send_request(Function::getInstalledBackgrounds(
                td_api::getInstalledBackgrounds {
                    for_dark_theme: as_bool(args),
                },
            ));
            true
        } else if op == "gbgu" {
            // `send_get_background_url` = getBackgroundUrl("asd", type) over
            // the whole background-type coercer surface (cli.cpp:4475).
            self.send_get_background_url(as_wallpaper_background(false, false));
            self.send_get_background_url(as_wallpaper_background(false, true));
            self.send_get_background_url(as_wallpaper_background(true, false));
            self.send_get_background_url(as_wallpaper_background(true, true));
            self.send_get_background_url(as_solid_pattern_background(-1, 0, false));
            self.send_get_background_url(as_solid_pattern_background(0x1000000, 0, true));
            self.send_get_background_url(as_solid_pattern_background(0, -1, false));
            self.send_get_background_url(as_solid_pattern_background(0, 101, false));
            self.send_get_background_url(as_solid_pattern_background(0, 0, false));
            self.send_get_background_url(as_solid_pattern_background(0xFFFFFF, 100, true));
            self.send_get_background_url(as_solid_pattern_background(0xABCDEF, 49, true));
            self.send_get_background_url(as_gradient_pattern_background(0, 0, 0, false, false));
            self.send_get_background_url(as_gradient_pattern_background(0, 0, 0, true, false));
            self.send_get_background_url(as_gradient_pattern_background(
                0xFFFFFF, 0, 100, false, true,
            ));
            self.send_get_background_url(as_gradient_pattern_background(
                0xFFFFFF, 0, 100, true, true,
            ));
            self.send_get_background_url(as_gradient_pattern_background(
                0xABCDEF, 0xFEDCBA, 49, false, true,
            ));
            self.send_get_background_url(as_gradient_pattern_background(
                0, 0x1000000, 49, false, true,
            ));
            self.send_get_background_url(as_freeform_gradient_pattern_background(
                vec![0xABCDEF, 0xFEDCBA],
                49,
                false,
                true,
            ));
            self.send_get_background_url(as_freeform_gradient_pattern_background(
                vec![0xABCDEF, 0x111111, 0x222222],
                49,
                true,
                true,
            ));
            self.send_get_background_url(as_freeform_gradient_pattern_background(
                vec![0xABCDEF, 0xFEDCBA, 0x111111, 0x222222],
                49,
                false,
                true,
            ));
            self.send_get_background_url(as_solid_background(-1));
            self.send_get_background_url(as_solid_background(0xABCDEF));
            self.send_get_background_url(as_solid_background(0x1000000));
            self.send_get_background_url(as_gradient_background(0xABCDEF, 0xFEDCBA));
            self.send_get_background_url(as_gradient_background(0, 0));
            self.send_get_background_url(as_gradient_background(-1, -1));
            self.send_get_background_url(as_freeform_gradient_background(vec![0xFEDCBA, 0x222222]));
            self.send_get_background_url(as_freeform_gradient_background(vec![
                0xFEDCBA, 0x111111, 0x222222,
            ]));
            self.send_get_background_url(as_freeform_gradient_background(vec![
                0xABCDEF, 0xFEDCBA, 0x111111, 0x222222,
            ]));
            self.send_get_background_url(as_chat_theme_background(args));
            true
        } else {
            false
        }
    }

    /// cli.cpp:4511 — the backgrounds/countries/premium/stars chain
    /// (M.14.4b-part2).
    fn op_block_dialogs(&self, op: &str, args: &str) -> bool {
        if op == "sbg" {
            self.send_request(Function::searchBackground(td_api::searchBackground {
                name: args.to_owned(),
            }));
            true
        } else if op == "sdb" || op == "sdbd" {
            let (input, tail) = tokenizer::split(args, ' ');
            let input_background = input_background(parse_input_background(&input));
            let background_type = background_type(parse_background_type(&tail));
            self.send_request(Function::setDefaultBackground(
                td_api::setDefaultBackground {
                    background: input_background,
                    r#type: background_type,
                    for_dark_theme: op == "sdbd",
                },
            ));
            true
        } else if op == "ddb" || op == "ddbd" {
            self.send_request(Function::deleteDefaultBackground(
                td_api::deleteDefaultBackground {
                    for_dark_theme: op == "ddbd",
                },
            ));
            true
        } else if op == "rib" {
            self.send_request(Function::removeInstalledBackground(
                td_api::removeInstalledBackground {
                    background_id: tokenizer::to_integer_i64(args),
                },
            ));
            true
        } else if op == "ribs" {
            self.send_request(Function::resetInstalledBackgrounds(
                td_api::resetInstalledBackgrounds,
            ));
            true
        } else if op == "scbg" || op == "scbgs" {
            let (chat_id, input, tail) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            let (tail, dark_theme_dimming) = tokenizer::split(&tail, ' ');
            self.send_request(Function::setChatBackground(td_api::setChatBackground {
                chat_id: self.as_chat_id(&chat_id),
                background: input_background(parse_input_background(&input)),
                r#type: background_type(parse_background_type(&tail)),
                dark_theme_dimming: tokenizer::to_integer_i32(&dark_theme_dimming),
                only_for_self: op == "scbgs",
            }));
            true
        } else if op == "dcb" || op == "dcbr" {
            self.send_request(Function::deleteChatBackground(
                td_api::deleteChatBackground {
                    chat_id: self.as_chat_id(args),
                    restore_previous: op == "dcbr",
                },
            ));
            true
        } else if op == "gcos" {
            self.send_request(Function::getCountries(td_api::getCountries));
            true
        } else if op == "gcou" {
            self.send_request(Function::getCountry(td_api::getCountry {
                country_code: args.to_owned(),
            }));
            true
        } else if op == "gcoc" {
            self.send_request(Function::getCountryCode(td_api::getCountryCode));
            true
        } else if op == "gpni" {
            self.send_request(Function::getPhoneNumberInfo(td_api::getPhoneNumberInfo {
                phone_number_prefix: args.to_owned(),
            }));
            true
        } else if op == "gpnis" {
            self.execute(Function::getPhoneNumberInfoSync(
                td_api::getPhoneNumberInfoSync {
                    language_code: if random::fast_bool() { "en" } else { "" }.to_owned(),
                    phone_number_prefix: args.to_owned(),
                },
            ));
            true
        } else if op == "gciu" {
            self.send_request(Function::getCollectibleItemInfo(
                td_api::getCollectibleItemInfo {
                    r#type: Some(td_api::CollectibleItemType::collectibleItemTypeUsername(
                        td_api::collectibleItemTypeUsername {
                            username: args.to_owned(),
                        },
                    )),
                },
            ));
            true
        } else if op == "gcipn" {
            self.send_request(Function::getCollectibleItemInfo(
                td_api::getCollectibleItemInfo {
                    r#type: Some(td_api::CollectibleItemType::collectibleItemTypePhoneNumber(
                        td_api::collectibleItemTypePhoneNumber {
                            phone_number: args.to_owned(),
                        },
                    )),
                },
            ));
            true
        } else if op == "gadl" {
            self.send_request(Function::getApplicationDownloadLink(
                td_api::getApplicationDownloadLink,
            ));
            true
        } else if op == "gprl" {
            self.send_request(Function::getPremiumLimit(td_api::getPremiumLimit {
                limit_type: Some(td_api::PremiumLimitType::premiumLimitTypeChatFolderCount(
                    td_api::premiumLimitTypeChatFolderCount,
                )),
            }));
            true
        } else if op == "gprf" {
            self.send_request(Function::getPremiumFeatures(td_api::getPremiumFeatures {
                source: Some(td_api::PremiumSource::premiumSourceLimitExceeded(
                    td_api::premiumSourceLimitExceeded {
                        limit_type: Some(
                            td_api::PremiumLimitType::premiumLimitTypeChatFolderCount(
                                td_api::premiumLimitTypeChatFolderCount,
                            ),
                        ),
                    },
                )),
            }));
            true
        } else if op == "gprse" {
            self.send_request(Function::getPremiumStickerExamples(
                td_api::getPremiumStickerExamples,
            ));
            true
        } else if op == "gpis" {
            self.send_request(Function::getPremiumInfoSticker(
                td_api::getPremiumInfoSticker {
                    month_count: tokenizer::to_integer_i32(args),
                },
            ));
            true
        } else if op == "vprf" {
            self.send_request(Function::viewPremiumFeature(td_api::viewPremiumFeature {
                feature: Some(td_api::PremiumFeature::premiumFeatureProfileBadge(
                    td_api::premiumFeatureProfileBadge,
                )),
            }));
            true
        } else if op == "cprsb" {
            self.send_request(Function::clickPremiumSubscriptionButton(
                td_api::clickPremiumSubscriptionButton,
            ));
            true
        } else if op == "gprs" {
            self.send_request(Function::getPremiumState(td_api::getPremiumState));
            true
        } else if op == "gpgpo" {
            self.send_request(Function::getPremiumGiftPaymentOptions(
                td_api::getPremiumGiftPaymentOptions,
            ));
            true
        } else if op == "gpgapo" {
            self.send_request(Function::getPremiumGiveawayPaymentOptions(
                td_api::getPremiumGiveawayPaymentOptions {
                    boosted_chat_id: self.as_chat_id(args),
                },
            ));
            true
        } else if op == "cpgc" {
            self.send_request(Function::checkPremiumGiftCode(
                td_api::checkPremiumGiftCode {
                    code: args.to_owned(),
                },
            ));
            true
        } else if op == "apgc" {
            self.send_request(Function::applyPremiumGiftCode(
                td_api::applyPremiumGiftCode {
                    code: args.to_owned(),
                },
            ));
            true
        } else if op == "gpws" {
            let (user_id, star_count, month_count, text) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, rest1) = tokenizer::split(&rest0, ' ');
                let (w2, w3) = tokenizer::split(&rest1, ' ');
                (w0, w1, w2, w3)
            };
            self.send_request(Function::giftPremiumWithStars(
                td_api::giftPremiumWithStars {
                    user_id: self.as_user_id(&user_id),
                    star_count: tokenizer::to_integer_i64(&star_count),
                    month_count: tokenizer::to_integer_i32(&month_count),
                    text: Some(self.as_formatted_text(&text)),
                },
            ));
            true
        } else if op == "lpg" {
            let (giveaway_id, user_count, star_count, tail) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, rest1) = tokenizer::split(&rest0, ' ');
                let (w2, w3) = tokenizer::split(&rest1, ' ');
                (w0, w1, w2, w3)
            };
            let parameters = self.giveaway_parameters(&tail);
            self.send_request(Function::launchPrepaidGiveaway(
                td_api::launchPrepaidGiveaway {
                    giveaway_id: tokenizer::to_integer_i64(&giveaway_id),
                    parameters: Some(parameters),
                    winner_count: tokenizer::to_integer_i32(&user_count),
                    star_count: tokenizer::to_integer_i64(&star_count),
                },
            ));
            true
        } else if op == "ggi" {
            let (chat_id, message_id) = tokenizer::split(args, ' ');
            self.send_request(Function::getGiveawayInfo(td_api::getGiveawayInfo {
                chat_id: self.as_chat_id(&chat_id),
                message_id: as_message_id(&message_id),
            }));
            true
        } else if op == "gspo" {
            self.send_request(Function::getStarPaymentOptions(
                td_api::getStarPaymentOptions,
            ));
            true
        } else if op == "gsgpo" {
            self.send_request(Function::getStarGiftPaymentOptions(
                td_api::getStarGiftPaymentOptions {
                    user_id: self.as_user_id(args),
                },
            ));
            true
        } else if op == "gsgapo" {
            self.send_request(Function::getStarGiveawayPaymentOptions(
                td_api::getStarGiveawayPaymentOptions,
            ));
            true
        } else if op == "gsta" || op == "gsti" || op == "gsto" {
            let (owner_id, limit, offset, subscription_id) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, rest1) = tokenizer::split(&rest0, ' ');
                let (w2, w3) = tokenizer::split(&rest1, ' ');
                (w0, w1, w2, w3)
            };
            let direction = if op == "gsti" {
                Some(td_api::TransactionDirection::transactionDirectionIncoming(
                    td_api::transactionDirectionIncoming,
                ))
            } else if op == "gsto" {
                Some(td_api::TransactionDirection::transactionDirectionOutgoing(
                    td_api::transactionDirectionOutgoing,
                ))
            } else {
                None
            };
            self.send_request(Function::getStarTransactions(td_api::getStarTransactions {
                owner_id: self.as_message_sender(&owner_id),
                subscription_id,
                direction,
                offset,
                limit: as_limit(&limit, 10),
            }));
            true
        } else if op == "gssu" {
            let (only_expiring, offset) = tokenizer::split(args, ' ');
            self.send_request(Function::getStarSubscriptions(
                td_api::getStarSubscriptions {
                    only_expiring: as_bool(&only_expiring),
                    offset,
                },
            ));
            true
        } else if op == "ess" {
            let (subscription_id, is_canceled) = tokenizer::split(args, ' ');
            self.send_request(Function::editStarSubscription(
                td_api::editStarSubscription {
                    subscription_id,
                    is_canceled: as_bool(&is_canceled),
                },
            ));
            true
        } else if op == "rss" {
            self.send_request(Function::reuseStarSubscription(
                td_api::reuseStarSubscription {
                    subscription_id: args.to_owned(),
                },
            ));
            true
        } else if op == "scap" || op == "scapd" {
            let (chat_id, commission, month_count) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            let parameters = if op == "scapd" {
                None
            } else {
                Some(td_api::AffiliateProgramParameters {
                    commission_per_mille: tokenizer::to_integer_i32(&commission),
                    month_count: tokenizer::to_integer_i32(&month_count),
                })
            };
            self.send_request(Function::setChatAffiliateProgram(
                td_api::setChatAffiliateProgram {
                    chat_id: self.as_chat_id(&chat_id),
                    parameters,
                },
            ));
            true
        } else if op == "scapr" {
            let (username, referrer) = tokenizer::split(args, ' ');
            self.send_request(Function::searchChatAffiliateProgram(
                td_api::searchChatAffiliateProgram { username, referrer },
            ));
            true
        } else if op == "sapc" || op == "sapd" || op == "sapr" {
            let (affiliate, limit, offset) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            let sort_order = if op == "sapd" {
                Some(
                    td_api::AffiliateProgramSortOrder::affiliateProgramSortOrderCreationDate(
                        td_api::affiliateProgramSortOrderCreationDate,
                    ),
                )
            } else if op == "sapr" {
                Some(
                    td_api::AffiliateProgramSortOrder::affiliateProgramSortOrderRevenue(
                        td_api::affiliateProgramSortOrderRevenue,
                    ),
                )
            } else {
                None
            };
            self.send_request(Function::searchAffiliatePrograms(
                td_api::searchAffiliatePrograms {
                    affiliate: Some(self.affiliate_type(&affiliate)),
                    sort_order,
                    offset,
                    limit: as_limit(&limit, 10),
                },
            ));
            true
        } else if op == "capr" {
            let (affiliate, bot_user_id) = tokenizer::split(args, ' ');
            self.send_request(Function::connectAffiliateProgram(
                td_api::connectAffiliateProgram {
                    affiliate: Some(self.affiliate_type(&affiliate)),
                    bot_user_id: self.as_user_id(&bot_user_id),
                },
            ));
            true
        } else if op == "dapr" {
            let (affiliate, url) = tokenizer::split(args, ' ');
            self.send_request(Function::disconnectAffiliateProgram(
                td_api::disconnectAffiliateProgram {
                    affiliate: Some(self.affiliate_type(&affiliate)),
                    url,
                },
            ));
            true
        } else if op == "gcapr" {
            let (affiliate, bot_user_id) = tokenizer::split(args, ' ');
            self.send_request(Function::getConnectedAffiliateProgram(
                td_api::getConnectedAffiliateProgram {
                    affiliate: Some(self.affiliate_type(&affiliate)),
                    bot_user_id: self.as_user_id(&bot_user_id),
                },
            ));
            true
        } else if op == "gcaprs" {
            let (affiliate, limit, offset) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            self.send_request(Function::getConnectedAffiliatePrograms(
                td_api::getConnectedAffiliatePrograms {
                    affiliate: Some(self.affiliate_type(&affiliate)),
                    offset,
                    limit: as_limit(&limit, 10),
                },
            ));
            true
        } else if op == "cpfs" || op == "cpfsb" {
            let (user_id, currency, amount, boosted_chat_id) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, rest1) = tokenizer::split(&rest0, ' ');
                let (w2, w3) = tokenizer::split(&rest1, ' ');
                (w0, w1, w2, w3)
            };
            let purpose = if currency.is_empty() {
                td_api::StorePaymentPurpose::storePaymentPurposePremiumSubscription(
                    td_api::storePaymentPurposePremiumSubscription {
                        is_restore: false,
                        is_upgrade: false,
                    },
                )
            } else {
                td_api::StorePaymentPurpose::storePaymentPurposePremiumGiftCodes(
                    td_api::storePaymentPurposePremiumGiftCodes {
                        boosted_chat_id: self.as_chat_id(&boosted_chat_id),
                        currency,
                        amount: tokenizer::to_integer_i64(&amount),
                        user_ids: vec![self.as_user_id(&user_id)],
                        text: None,
                    },
                )
            };
            self.send_request(Function::canPurchaseFromStore(
                td_api::canPurchaseFromStore {
                    purpose: Some(purpose),
                },
            ));
            true
        } else if op == "cpfsg" {
            let (tail, currency, amount) = {
                // The giveaway parameters consume a variable word count;
                // the C++ variadic splits one word off per argument, so the
                // parameters parse from the single first word only if it has
                // no spaces — port the split arithmetic faithfully: two
                // words (currency, amount) come off the END.
                let mut parts = autosplit(args);
                let amount = if !parts.is_empty() {
                    parts.pop().unwrap_or_default()
                } else {
                    String::new()
                };
                let currency = if !parts.is_empty() {
                    parts.pop().unwrap_or_default()
                } else {
                    String::new()
                };
                (parts.join(" "), currency, amount)
            };
            let parameters = self.giveaway_parameters(&tail);
            self.send_request(Function::canPurchaseFromStore(
                td_api::canPurchaseFromStore {
                    purpose: Some(
                        td_api::StorePaymentPurpose::storePaymentPurposePremiumGiveaway(
                            td_api::storePaymentPurposePremiumGiveaway {
                                parameters: Some(parameters),
                                currency,
                                amount: tokenizer::to_integer_i64(&amount),
                            },
                        ),
                    ),
                },
            ));
            true
        } else if op == "cpfssg" {
            let mut parts = autosplit(args);
            let star_count = parts.pop().unwrap_or_default();
            let user_count = parts.pop().unwrap_or_default();
            let amount = parts.pop().unwrap_or_default();
            let currency = parts.pop().unwrap_or_default();
            let tail = parts.join(" ");
            let parameters = self.giveaway_parameters(&tail);
            self.send_request(Function::canPurchaseFromStore(
                td_api::canPurchaseFromStore {
                    purpose: Some(
                        td_api::StorePaymentPurpose::storePaymentPurposeStarGiveaway(
                            td_api::storePaymentPurposeStarGiveaway {
                                parameters: Some(parameters),
                                currency,
                                amount: tokenizer::to_integer_i64(&amount),
                                winner_count: tokenizer::to_integer_i32(&user_count),
                                star_count: tokenizer::to_integer_i64(&star_count),
                            },
                        ),
                    ),
                },
            ));
            true
        } else if op == "cpfss" {
            let (currency, amount, star_count, chat_id) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, rest1) = tokenizer::split(&rest0, ' ');
                let (w2, w3) = tokenizer::split(&rest1, ' ');
                (w0, w1, w2, w3)
            };
            self.send_request(Function::canPurchaseFromStore(
                td_api::canPurchaseFromStore {
                    purpose: Some(td_api::StorePaymentPurpose::storePaymentPurposeStars(
                        td_api::storePaymentPurposeStars {
                            currency,
                            amount: tokenizer::to_integer_i64(&amount),
                            star_count: tokenizer::to_integer_i64(&star_count),
                            chat_id: self.as_chat_id(&chat_id),
                        },
                    )),
                },
            ));
            true
        } else if op == "cpfsgs" {
            let (user_id, currency, amount, star_count) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, rest1) = tokenizer::split(&rest0, ' ');
                let (w2, w3) = tokenizer::split(&rest1, ' ');
                (w0, w1, w2, w3)
            };
            self.send_request(Function::canPurchaseFromStore(
                td_api::canPurchaseFromStore {
                    purpose: Some(td_api::StorePaymentPurpose::storePaymentPurposeGiftedStars(
                        td_api::storePaymentPurposeGiftedStars {
                            user_id: self.as_user_id(&user_id),
                            currency,
                            amount: tokenizer::to_integer_i64(&amount),
                            star_count: tokenizer::to_integer_i64(&star_count),
                        },
                    )),
                },
            ));
            true
        } else if op == "gbf" {
            self.send_request(Function::getBusinessFeatures(td_api::getBusinessFeatures {
                source: None,
            }));
            true
        } else if op == "atos" {
            self.send_request(Function::acceptTermsOfService(
                td_api::acceptTermsOfService {
                    terms_of_service_id: args.to_owned(),
                },
            ));
            true
        } else if op == "gdli" {
            self.send_request(Function::getDeepLinkInfo(td_api::getDeepLinkInfo {
                link: args.to_owned(),
            }));
            true
        } else if op == "tme" {
            self.send_request(Function::getRecentlyVisitedTMeUrls(
                td_api::getRecentlyVisitedTMeUrls {
                    referrer: args.to_owned(),
                },
            ));
            true
        } else if op == "gbms" {
            let (block_list, offset, limit) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            self.send_request(Function::getBlockedMessageSenders(
                td_api::getBlockedMessageSenders {
                    block_list: as_block_list(&block_list),
                    offset: tokenizer::to_integer_i32(&offset),
                    limit: as_limit(&limit, 10),
                },
            ));
            true
        } else if op == "gu" {
            self.send_request(Function::getUser(td_api::getUser {
                user_id: self.as_user_id(args),
            }));
            true
        } else if op == "gsu" {
            self.send_request(Function::getSupportUser(td_api::getSupportUser));
            true
        } else if op == "gso" || op == "gsoa" || op == "gsoc" {
            self.send_request(Function::getStickerOutline(td_api::getStickerOutline {
                sticker_file_id: tokenizer::to_integer_i32(args),
                for_animated_emoji: op == "gsoa",
                for_clicked_animated_emoji_message: op == "gsoc",
            }));
            true
        } else if op == "gsosp" || op == "gsospa" || op == "gsospc" {
            self.send_request(Function::getStickerOutlineSvgPath(
                td_api::getStickerOutlineSvgPath {
                    sticker_file_id: tokenizer::to_integer_i32(args),
                    for_animated_emoji: op == "gsospa",
                    for_clicked_animated_emoji_message: op == "gsospc",
                },
            ));
            true
        } else if op == "gs" || op == "gsmm" || op == "gsee" || op == "gseeme" {
            let (limit, query) = get_search_query(args);
            self.send_request(Function::getStickers(td_api::getStickers {
                sticker_type: as_sticker_type(op),
                query,
                limit,
                chat_id: if op == "gseeme" {
                    self.my_id.load(Ordering::SeqCst)
                } else {
                    0
                },
            }));
            true
        } else if op == "gaser" || op == "gasem" || op == "gase" || op == "gaseme" {
            let (query, return_only_main_emoji) = tokenizer::split(args, ' ');
            self.send_request(Function::getAllStickerEmojis(td_api::getAllStickerEmojis {
                sticker_type: as_sticker_type(op),
                query,
                chat_id: if op == "gaseme" {
                    self.my_id.load(Ordering::SeqCst)
                } else {
                    0
                },
                return_only_main_emoji: as_bool(&return_only_main_emoji),
            }));
            true
        } else if op == "sst" || op == "sstm" || op == "sste" {
            let (limit, emojis, query, input_language_codes, offset) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, rest1) = tokenizer::split(&rest0, ' ');
                let (w2, rest2) = tokenizer::split(&rest1, ' ');
                let (w3, w4) = tokenizer::split(&rest2, ' ');
                (w0, w1, w2, w3, w4)
            };
            self.send_request(Function::searchStickers(td_api::searchStickers {
                sticker_type: as_sticker_type(op),
                emojis,
                query,
                input_language_codes: autosplit(&input_language_codes),
                offset: tokenizer::to_integer_i32(&offset),
                limit: as_limit(&limit, 10),
            }));
            true
        } else if op == "ggs" {
            self.send_request(Function::getGreetingStickers(td_api::getGreetingStickers));
            true
        } else if op == "gprst" {
            self.send_request(Function::getPremiumStickers(td_api::getPremiumStickers {
                limit: as_limit(args, 10),
            }));
            true
        } else if op == "gss" {
            self.send_request(Function::getStickerSet(td_api::getStickerSet {
                set_id: tokenizer::to_integer_i64(args),
            }));
            true
        } else if op == "gssn" {
            self.send_request(Function::getStickerSetName(td_api::getStickerSetName {
                set_id: tokenizer::to_integer_i64(args),
            }));
            true
        } else if op == "giss" || op == "gissm" || op == "gisse" {
            self.send_request(Function::getInstalledStickerSets(
                td_api::getInstalledStickerSets {
                    sticker_type: as_sticker_type(op),
                },
            ));
            true
        } else if op == "gass" || op == "gassm" || op == "gasse" {
            let (offset_sticker_set_id, limit) = tokenizer::split(args, ' ');
            self.send_request(Function::getArchivedStickerSets(
                td_api::getArchivedStickerSets {
                    sticker_type: as_sticker_type(op),
                    offset_sticker_set_id: tokenizer::to_integer_i64(&offset_sticker_set_id),
                    limit: as_limit(&limit, 10),
                },
            ));
            true
        } else if op == "gtss" || op == "gtssm" || op == "gtsse" {
            let (offset, limit) = tokenizer::split(args, ' ');
            self.send_request(Function::getTrendingStickerSets(
                td_api::getTrendingStickerSets {
                    sticker_type: as_sticker_type(op),
                    offset: tokenizer::to_integer_i32(&offset),
                    limit: as_limit(&limit, 1000),
                },
            ));
            true
        } else if op == "gatss" {
            self.send_request(Function::getAttachedStickerSets(
                td_api::getAttachedStickerSets {
                    file_id: tokenizer::to_integer_i32(trim_td(args)),
                },
            ));
            true
        } else {
            false
        }
    }

    /// cli.cpp:4822 — the storage/network/web-browser/top-chats/stickers/
    /// emoji chain (M.14.4b-part3).
    fn op_block_history(&self, op: &str, args: &str) -> bool {
        if op == "storage" {
            self.send_request(Function::getStorageStatistics(
                td_api::getStorageStatistics {
                    chat_limit: tokenizer::to_integer_i32(args),
                },
            ));
            true
        } else if op == "storage_fast" {
            self.send_request(Function::getStorageStatisticsFast(
                td_api::getStorageStatisticsFast,
            ));
            true
        } else if op == "database" {
            self.send_request(Function::getDatabaseStatistics(
                td_api::getDatabaseStatistics,
            ));
            true
        } else if op == "optimize_storage" || op == "optimize_storage_all" {
            let (chat_ids, exclude_chat_ids, chat_ids_limit) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            self.send_request(Function::optimizeStorage(td_api::optimizeStorage {
                size: 10000000,
                ttl: -1,
                count: -1,
                immunity_delay: 0,
                file_types: Vec::new(),
                chat_ids: self.as_chat_ids(&chat_ids),
                exclude_chat_ids: self.as_chat_ids(&exclude_chat_ids),
                return_deleted_file_statistics: op == "optimize_storage",
                chat_limit: tokenizer::to_integer_i32(&chat_ids_limit),
            }));
            true
        } else if op == "clean_storage_default" {
            self.send_request(Function::optimizeStorage(td_api::optimizeStorage::default()));
            true
        } else if op == "clean_photos" {
            self.send_request(Function::optimizeStorage(td_api::optimizeStorage {
                size: 0,
                ttl: 0,
                count: 0,
                immunity_delay: 0,
                file_types: vec![td_api::FileType::fileTypePhoto(td_api::fileTypePhoto)],
                chat_ids: Vec::new(),
                exclude_chat_ids: Vec::new(),
                return_deleted_file_statistics: true,
                chat_limit: 20,
            }));
            true
        } else if op == "clean_storage" {
            let file_type = |ctor: fn() -> td_api::FileType| ctor();
            let _ = file_type;
            self.send_request(Function::optimizeStorage(td_api::optimizeStorage {
                size: 0,
                ttl: -1,
                count: -1,
                immunity_delay: 0,
                file_types: Vec::new(),
                chat_ids: self.as_chat_ids(args),
                exclude_chat_ids: Vec::new(),
                return_deleted_file_statistics: true,
                chat_limit: 20,
            }));
            true
        } else if op == "network" {
            self.send_request(Function::getNetworkStatistics(
                td_api::getNetworkStatistics {
                    only_current: false,
                },
            ));
            true
        } else if op == "current_network" {
            self.send_request(Function::getNetworkStatistics(
                td_api::getNetworkStatistics { only_current: true },
            ));
            true
        } else if op == "reset_network" {
            self.send_request(Function::resetNetworkStatistics(
                td_api::resetNetworkStatistics,
            ));
            true
        } else if op == "snt" {
            self.send_request(Function::setNetworkType(td_api::setNetworkType {
                r#type: Some(as_network_type(args)),
            }));
            true
        } else if op == "gadsp" {
            self.send_request(Function::getAutoDownloadSettingsPresets(
                td_api::getAutoDownloadSettingsPresets,
            ));
            true
        } else if op == "sads" {
            self.send_request(Function::setAutoDownloadSettings(
                td_api::setAutoDownloadSettings {
                    settings: Some(td_api::AutoDownloadSettings::default()),
                    r#type: Some(as_network_type(args)),
                },
            ));
            true
        } else if op == "gaus" {
            self.send_request(Function::getAutosaveSettings(td_api::getAutosaveSettings));
            true
        } else if op == "saus" {
            let (scope_str, autosave_photos, autosave_videos, max_video_file_size) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, rest1) = tokenizer::split(&rest0, ' ');
                let (w2, w3) = tokenizer::split(&rest1, ' ');
                (w0, w1, w2, w3)
            };
            let scope = if scope_str == "users" {
                Some(
                    td_api::AutosaveSettingsScope::autosaveSettingsScopePrivateChats(
                        td_api::autosaveSettingsScopePrivateChats,
                    ),
                )
            } else if scope_str == "groups" {
                Some(
                    td_api::AutosaveSettingsScope::autosaveSettingsScopeGroupChats(
                        td_api::autosaveSettingsScopeGroupChats,
                    ),
                )
            } else if scope_str == "channels" {
                Some(
                    td_api::AutosaveSettingsScope::autosaveSettingsScopeChannelChats(
                        td_api::autosaveSettingsScopeChannelChats,
                    ),
                )
            } else {
                let chat_id = self.as_chat_id(&scope_str);
                if chat_id != 0 {
                    Some(td_api::AutosaveSettingsScope::autosaveSettingsScopeChat(
                        td_api::autosaveSettingsScopeChat { chat_id },
                    ))
                } else {
                    None
                }
            };
            self.send_request(Function::setAutosaveSettings(td_api::setAutosaveSettings {
                scope,
                settings: Some(td_api::ScopeAutosaveSettings {
                    autosave_photos: as_bool(&autosave_photos),
                    autosave_videos: as_bool(&autosave_videos),
                    max_video_file_size: tokenizer::to_integer_i64(&max_video_file_size),
                }),
            }));
            true
        } else if op == "cause" {
            self.send_request(Function::clearAutosaveSettingsExceptions(
                td_api::clearAutosaveSettingsExceptions,
            ));
            true
        } else if op == "cwbs" {
            let (open_external_browser, display_close_button) = tokenizer::split(args, ' ');
            self.send_request(Function::changeWebBrowserSettings(
                td_api::changeWebBrowserSettings {
                    open_external_browser: as_bool(&open_external_browser),
                    display_close_button: as_bool(&display_close_button),
                },
            ));
            true
        } else if op == "awbsee" || op == "awbsei" {
            let (url, _) = tokenizer::split(args, ' ');
            self.send_request(Function::addWebBrowserSettingsException(
                td_api::addWebBrowserSettingsException {
                    open_external_browser: op == "awbsee",
                    url,
                },
            ));
            true
        } else if op == "rwbse" {
            self.send_request(Function::removeWebBrowserSettingsException(
                td_api::removeWebBrowserSettingsException {
                    url: args.to_owned(),
                },
            ));
            true
        } else if op == "rawbse" {
            self.send_request(Function::removeAllWebBrowserSettingsExceptions(
                td_api::removeAllWebBrowserSettingsExceptions,
            ));
            true
        } else if op == "ansc" {
            let (sent_bytes, received_bytes, duration, network_type) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, rest1) = tokenizer::split(&rest0, ' ');
                let (w2, w3) = tokenizer::split(&rest1, ' ');
                (w0, w1, w2, w3)
            };
            self.send_request(Function::addNetworkStatistics(
                td_api::addNetworkStatistics {
                    entry: Some(td_api::NetworkStatisticsEntry::networkStatisticsEntryCall(
                        td_api::networkStatisticsEntryCall {
                            network_type: Some(as_network_type(&network_type)),
                            sent_bytes: tokenizer::to_integer_i64(&sent_bytes),
                            received_bytes: tokenizer::to_integer_i64(&received_bytes),
                            duration: tokenizer::to_integer_i64(&duration) as f64,
                        },
                    )),
                },
            ));
            true
        } else if op == "ans" {
            let (sent_bytes, received_bytes, network_type) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            self.send_request(Function::addNetworkStatistics(
                td_api::addNetworkStatistics {
                    entry: Some(td_api::NetworkStatisticsEntry::networkStatisticsEntryFile(
                        td_api::networkStatisticsEntryFile {
                            file_type: Some(td_api::FileType::fileTypeDocument(
                                td_api::fileTypeDocument,
                            )),
                            network_type: Some(as_network_type(&network_type)),
                            sent_bytes: tokenizer::to_integer_i64(&sent_bytes),
                            received_bytes: tokenizer::to_integer_i64(&received_bytes),
                        },
                    )),
                },
            ));
            true
        } else if op == "gtc" {
            self.send_request(Function::getTopChats(td_api::getTopChats {
                category: Some(as_top_chat_category(args)),
                limit: 50,
            }));
            true
        } else if op == "rtc" {
            let (chat_id, category) = tokenizer::split(args, ' ');
            self.send_request(Function::removeTopChat(td_api::removeTopChat {
                category: Some(as_top_chat_category(&category)),
                chat_id: self.as_chat_id(&chat_id),
            }));
            true
        } else if op == "gsssn" {
            self.send_request(Function::getSuggestedStickerSetName(
                td_api::getSuggestedStickerSetName {
                    title: args.to_owned(),
                },
            ));
            true
        } else if op == "cssn" {
            self.send_request(Function::checkStickerSetName(td_api::checkStickerSetName {
                name: args.to_owned(),
            }));
            true
        } else if op == "usf" || op == "usfa" || op == "usfv" {
            self.send_request(Function::uploadStickerFile(td_api::uploadStickerFile {
                user_id: -1,
                sticker_format: Some(as_sticker_format(op)),
                sticker: as_input_file(args),
            }));
            true
        } else if op == "cnss" || op == "cnssa" || op == "cnssv" || op == "cnssm" || op == "cnsse" {
            let (title, name, stickers) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            let input_stickers = autosplit(&stickers)
                .iter()
                .map(|sticker| td_api::NewSticker {
                    sticker: as_input_file(sticker),
                    format: Some(as_sticker_format(op)),
                    emojis: "\u{1f600}".to_owned(),
                    mask_position: as_mask_position(op),
                    keywords: vec!["keyword".to_owned()],
                })
                .collect();
            self.send_request(Function::createNewStickerSet(td_api::createNewStickerSet {
                user_id: self.my_id.load(Ordering::SeqCst),
                title,
                name,
                sticker_type: as_sticker_type(op),
                needs_repainting: false,
                stickers: input_stickers,
                source: "tg_cli".to_owned(),
            }));
            true
        } else if op == "goss" {
            let (sticker_set_id, limit) = tokenizer::split(args, ' ');
            self.send_request(Function::getOwnedStickerSets(td_api::getOwnedStickerSets {
                offset_sticker_set_id: tokenizer::to_integer_i64(&sticker_set_id),
                limit: as_limit(&limit, 10),
            }));
            true
        } else if op == "sss" || op == "sssf" {
            self.send_request(Function::searchStickerSet(td_api::searchStickerSet {
                name: args.to_owned(),
                ignore_cache: op == "sssf",
            }));
            true
        } else if op == "siss" {
            self.send_request(Function::searchInstalledStickerSets(
                td_api::searchInstalledStickerSets {
                    sticker_type: None,
                    query: args.to_owned(),
                    limit: 2,
                },
            ));
            true
        } else if op == "ssss" || op == "ssssm" || op == "sssse" {
            self.send_request(Function::searchStickerSets(td_api::searchStickerSets {
                sticker_type: as_sticker_type(op),
                query: args.to_owned(),
            }));
            true
        } else if op == "css" {
            let (set_id, is_installed, is_archived) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            self.send_request(Function::changeStickerSet(td_api::changeStickerSet {
                set_id: tokenizer::to_integer_i64(&set_id),
                is_installed: as_bool(&is_installed),
                is_archived: as_bool(&is_archived),
            }));
            true
        } else if op == "vtss" {
            self.send_request(Function::viewTrendingStickerSets(
                td_api::viewTrendingStickerSets {
                    sticker_set_ids: autosplit(args)
                        .iter()
                        .map(|t| tokenizer::to_integer_i64(trim_td(t)))
                        .collect(),
                },
            ));
            true
        } else if op == "riss" || op == "rissm" || op == "risse" {
            self.send_request(Function::reorderInstalledStickerSets(
                td_api::reorderInstalledStickerSets {
                    sticker_type: as_sticker_type(op),
                    sticker_set_ids: autosplit(args)
                        .iter()
                        .map(|t| tokenizer::to_integer_i64(trim_td(t)))
                        .collect(),
                },
            ));
            true
        } else if op == "grs" {
            self.send_request(Function::getRecentStickers(td_api::getRecentStickers {
                is_attached: as_bool(args),
            }));
            true
        } else if op == "ars" || op == "rrs" {
            let (is_attached, sticker_id) = tokenizer::split(args, ' ');
            let file = as_input_file_id(&sticker_id);
            if op == "ars" {
                self.send_request(Function::addRecentSticker(td_api::addRecentSticker {
                    is_attached: as_bool(&is_attached),
                    sticker: file,
                }));
            } else {
                self.send_request(Function::removeRecentSticker(td_api::removeRecentSticker {
                    is_attached: as_bool(&is_attached),
                    sticker: file,
                }));
            }
            true
        } else if op == "gfs" {
            self.send_request(Function::getFavoriteStickers(td_api::getFavoriteStickers));
            true
        } else if op == "afs" {
            self.send_request(Function::addFavoriteSticker(td_api::addFavoriteSticker {
                sticker: as_input_file_id(args),
            }));
            true
        } else if op == "rfs" {
            self.send_request(Function::removeFavoriteSticker(
                td_api::removeFavoriteSticker {
                    sticker: as_input_file_id(args),
                },
            ));
            true
        } else if op == "crs" {
            self.send_request(Function::clearRecentStickers(td_api::clearRecentStickers {
                is_attached: as_bool(args),
            }));
            true
        } else if op == "gse" {
            self.send_request(Function::getStickerEmojis(td_api::getStickerEmojis {
                sticker: as_input_file_id(args),
            }));
            true
        } else if op == "se" {
            self.send_request(Function::searchEmojis(td_api::searchEmojis {
                text: args.to_owned(),
                input_language_codes: Vec::new(),
            }));
            true
        } else if op == "seru" {
            self.send_request(Function::searchEmojis(td_api::searchEmojis {
                text: args.to_owned(),
                input_language_codes: vec!["ru_RU".to_owned()],
            }));
            true
        } else if op == "gke" {
            self.send_request(Function::getKeywordEmojis(td_api::getKeywordEmojis {
                text: args.to_owned(),
                input_language_codes: Vec::new(),
            }));
            true
        } else if op == "gkeru" {
            self.send_request(Function::getKeywordEmojis(td_api::getKeywordEmojis {
                text: args.to_owned(),
                input_language_codes: vec!["ru_RU".to_owned()],
            }));
            true
        } else if op == "gec" || op == "geces" || op == "geccp" || op == "gecrs" {
            let sticker_type = if op == "geces" {
                Some(td_api::EmojiCategoryType::emojiCategoryTypeEmojiStatus(
                    td_api::emojiCategoryTypeEmojiStatus,
                ))
            } else if op == "geccp" {
                Some(td_api::EmojiCategoryType::emojiCategoryTypeChatPhoto(
                    td_api::emojiCategoryTypeChatPhoto,
                ))
            } else if op == "gecrs" {
                Some(td_api::EmojiCategoryType::emojiCategoryTypeRegularStickers(
                    td_api::emojiCategoryTypeRegularStickers,
                ))
            } else {
                Some(td_api::EmojiCategoryType::emojiCategoryTypeDefault(
                    td_api::emojiCategoryTypeDefault,
                ))
            };
            self.send_request(Function::getEmojiCategories(td_api::getEmojiCategories {
                r#type: sticker_type,
            }));
            true
        } else if op == "gae" {
            self.send_request(Function::getAnimatedEmoji(td_api::getAnimatedEmoji {
                emoji: args.to_owned(),
            }));
            true
        } else if op == "gesu" {
            self.send_request(Function::getEmojiSuggestionsUrl(
                td_api::getEmojiSuggestionsUrl {
                    language_code: args.to_owned(),
                },
            ));
            true
        } else if op == "gces" {
            self.send_request(Function::getCustomEmojiStickers(
                td_api::getCustomEmojiStickers {
                    custom_emoji_ids: autosplit(args)
                        .iter()
                        .map(|t| tokenizer::to_integer_i64(trim_td(t)))
                        .collect(),
                },
            ));
            true
        } else if op == "gdcpces" {
            self.send_request(Function::getDefaultChatPhotoCustomEmojiStickers(
                td_api::getDefaultChatPhotoCustomEmojiStickers,
            ));
            true
        } else if op == "gdppces" {
            self.send_request(Function::getDefaultProfilePhotoCustomEmojiStickers(
                td_api::getDefaultProfilePhotoCustomEmojiStickers,
            ));
            true
        } else if op == "gdbces" {
            self.send_request(Function::getDefaultBackgroundCustomEmojiStickers(
                td_api::getDefaultBackgroundCustomEmojiStickers,
            ));
            true
        } else if op == "gsan" {
            self.send_request(Function::getSavedAnimations(td_api::getSavedAnimations));
            true
        } else if op == "asan" {
            self.send_request(Function::addSavedAnimation(td_api::addSavedAnimation {
                animation: as_input_file_id(args),
            }));
            true
        } else if op == "rsan" {
            self.send_request(Function::removeSavedAnimation(
                td_api::removeSavedAnimation {
                    animation: as_input_file_id(args),
                },
            ));
            true
        } else {
            false
        }
    }

    /// cli.cpp:5054 — the info/getters/files/translate chain (census chain 6).
    fn op_block_files(&self, op: &str, args: &str) -> bool {
        if op == "guf" {
            self.send_request(Function::getUserFullInfo(td_api::getUserFullInfo {
                user_id: self.as_user_id(args),
            }));
            true
        } else if op == "gbg" {
            self.send_request(Function::getBasicGroup(td_api::getBasicGroup {
                basic_group_id: self.as_basic_group_id(args),
            }));
            true
        } else if op == "gbgf" {
            self.send_request(Function::getBasicGroupFullInfo(
                td_api::getBasicGroupFullInfo {
                    basic_group_id: self.as_basic_group_id(args),
                },
            ));
            true
        } else if op == "gsg" || op == "gch" {
            self.send_request(Function::getSupergroup(td_api::getSupergroup {
                supergroup_id: self.as_supergroup_id(args),
            }));
            true
        } else if op == "gsgf" || op == "gchf" {
            self.send_request(Function::getSupergroupFullInfo(
                td_api::getSupergroupFullInfo {
                    supergroup_id: self.as_supergroup_id(args),
                },
            ));
            true
        } else if op == "gsc" {
            self.send_request(Function::getSecretChat(td_api::getSecretChat {
                secret_chat_id: self.as_secret_chat_id(args),
            }));
            true
        } else if op == "scm" {
            let (chat_id, filter, tail) = {
                let (w0, r0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&r0, ' ');
                (w0, w1, w2)
            };
            let (limit, query) = get_search_query(&tail);
            self.send_request(Function::searchChatMembers(td_api::searchChatMembers {
                chat_id: self.as_chat_id(&chat_id),
                query,
                limit,
                filter: self.as_chat_members_filter(&filter),
            }));
            true
        } else if op == "gcm" {
            let (chat_id, member_id) = tokenizer::split(args, ' ');
            self.send_request(Function::getChatMember(td_api::getChatMember {
                chat_id: self.as_chat_id(&chat_id),
                member_id: self.as_message_sender(&member_id),
            }));
            true
        } else if op == "GetChatAdministrators" {
            self.send_request(Function::getChatAdministrators(
                td_api::getChatAdministrators {
                    chat_id: self.as_chat_id(args),
                },
            ));
            true
        } else if op == "GetSupergroupAdministrators"
            || op == "GetSupergroupBanned"
            || op == "GetSupergroupBots"
            || op == "GetSupergroupContacts"
            || op == "GetSupergroupMembers"
            || op == "GetSupergroupRestricted"
            || op == "SearchSupergroupMembers"
            || op == "SearchSupergroupMentions"
        {
            let (supergroup_id, offset, tail) = {
                let (w0, r0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&r0, ' ');
                (w0, w1, w2)
            };
            let (limit, query) = get_search_query(&tail);
            self.send_request(Function::getSupergroupMembers(
                td_api::getSupergroupMembers {
                    supergroup_id: self.as_supergroup_id(&supergroup_id),
                    filter: self.as_supergroup_members_filter(op, &query),
                    offset: tokenizer::to_integer_i32(&offset),
                    limit,
                },
            ));
            true
        } else if op == "gd" {
            self.send_request(Function::getChat(td_api::getChat {
                chat_id: self.as_chat_id(args),
            }));
            true
        } else if op == "gcfi" {
            self.send_request(Function::getCommunityFullInfo(
                td_api::getCommunityFullInfo {
                    community_id: tokenizer::to_integer_i64(trim_td(args)),
                },
            ));
            true
        } else if op == "open" {
            let chat_id = self.as_chat_id(args);
            self.send_request(Function::openChat(td_api::openChat { chat_id }));
            self.opened_chat_id.store(chat_id, Ordering::SeqCst);
            true
        } else if op == "close" {
            self.send_request(Function::closeChat(td_api::closeChat {
                chat_id: self.as_chat_id(args),
            }));
            true
        } else if op == "gm" {
            let (chat_id, message_id) = tokenizer::split(args, ' ');
            self.send_request(Function::getMessage(td_api::getMessage {
                chat_id: self.as_chat_id(&chat_id),
                message_id: as_message_id(&message_id),
            }));
            true
        } else if op == "gmf" {
            let (chat_id, from_message_id, to_message_id) = {
                let (w0, r0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&r0, ' ');
                (w0, w1, w2)
            };
            let chat_id = self.as_chat_id(&chat_id);
            let mut message_id = tokenizer::to_integer_i64(&from_message_id);
            let to = tokenizer::to_integer_i64(&to_message_id);
            while message_id <= to {
                self.send_request(Function::getMessage(td_api::getMessage {
                    chat_id,
                    message_id: message_id << 20,
                }));
                message_id += 1;
            }
            true
        } else if op == "gml" {
            let (chat_id, message_id) = tokenizer::split(args, ' ');
            self.send_request(Function::getMessageLocally(td_api::getMessageLocally {
                chat_id: self.as_chat_id(&chat_id),
                message_id: as_message_id(&message_id),
            }));
            true
        } else if op == "grm" {
            let (chat_id, message_id) = tokenizer::split(args, ' ');
            self.send_request(Function::getRepliedMessage(td_api::getRepliedMessage {
                chat_id: self.as_chat_id(&chat_id),
                message_id: as_message_id(&message_id),
            }));
            true
        } else if op == "gmt" {
            let (chat_id, message_id) = tokenizer::split(args, ' ');
            self.send_request(Function::getMessageThread(td_api::getMessageThread {
                chat_id: self.as_chat_id(&chat_id),
                message_id: as_message_id(&message_id),
            }));
            true
        } else if op == "gmrd" {
            let (chat_id, message_id) = tokenizer::split(args, ' ');
            self.send_request(Function::getMessageReadDate(td_api::getMessageReadDate {
                chat_id: self.as_chat_id(&chat_id),
                message_id: as_message_id(&message_id),
            }));
            true
        } else if op == "gmv" {
            let (chat_id, message_id) = tokenizer::split(args, ' ');
            self.send_request(Function::getMessageViewers(td_api::getMessageViewers {
                chat_id: self.as_chat_id(&chat_id),
                message_id: as_message_id(&message_id),
            }));
            true
        } else if op == "gma" {
            let (chat_id, message_id) = tokenizer::split(args, ' ');
            self.send_request(Function::getMessageAuthor(td_api::getMessageAuthor {
                chat_id: self.as_chat_id(&chat_id),
                message_id: as_message_id(&message_id),
            }));
            true
        } else if op == "gcpm" {
            self.send_request(Function::getChatPinnedMessage(
                td_api::getChatPinnedMessage {
                    chat_id: self.as_chat_id(args),
                },
            ));
            true
        } else if op == "gms" {
            let (chat_id, message_ids) = tokenizer::split(args, ' ');
            self.send_request(Function::getMessages(td_api::getMessages {
                chat_id: self.as_chat_id(&chat_id),
                message_ids: autosplit(&message_ids)
                    .iter()
                    .map(|t| as_message_id(trim_td(t)))
                    .collect(),
            }));
            true
        } else if op == "gfrm" {
            let (chat_id, message_id) = tokenizer::split(args, ' ');
            self.send_request(Function::getFullRichMessage(td_api::getFullRichMessage {
                chat_id: self.as_chat_id(&chat_id),
                message_id: as_message_id(&message_id),
            }));
            true
        } else if op == "gmp" {
            let (chat_id, message_id) = tokenizer::split(args, ' ');
            self.send_request(Function::getMessageProperties(
                td_api::getMessageProperties {
                    chat_id: self.as_chat_id(&chat_id),
                    message_id: as_message_id(&message_id),
                },
            ));
            true
        } else if op == "gpop" {
            let (chat_id, message_id, option_id) = {
                let (w0, r0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&r0, ' ');
                (w0, w1, w2)
            };
            self.send_request(Function::getPollOptionProperties(
                td_api::getPollOptionProperties {
                    chat_id: self.as_chat_id(&chat_id),
                    message_id: as_message_id(&message_id),
                    poll_option_id: option_id,
                },
            ));
            true
        } else if op == "gcspm" {
            self.send_request(Function::getChatSponsoredMessages(
                td_api::getChatSponsoredMessages {
                    chat_id: self.as_chat_id(args),
                },
            ));
            true
        } else if op == "ccspm" {
            let (chat_id, message_id) = tokenizer::split(args, ' ');
            self.send_request(Function::clickChatSponsoredMessage(
                td_api::clickChatSponsoredMessage {
                    chat_id: self.as_chat_id(&chat_id),
                    message_id: as_message_id(&message_id),
                    is_media_click: random::fast_bool(),
                    from_fullscreen: random::fast_bool(),
                },
            ));
            true
        } else if op == "rcspm" {
            let (chat_id, message_id, option_id) = {
                let (w0, r0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&r0, ' ');
                (w0, w1, w2)
            };
            self.send_request(Function::reportChatSponsoredMessage(
                td_api::reportChatSponsoredMessage {
                    chat_id: self.as_chat_id(&chat_id),
                    message_id: as_message_id(&message_id),
                    option_id: option_id.into_bytes(),
                },
            ));
            true
        } else if op == "gssc" {
            self.send_request(Function::getSearchSponsoredChats(
                td_api::getSearchSponsoredChats {
                    query: args.to_owned(),
                },
            ));
            true
        } else if op == "vsc" {
            self.send_request(Function::viewSponsoredChat(td_api::viewSponsoredChat {
                sponsored_chat_unique_id: tokenizer::to_integer_i64(args),
            }));
            true
        } else if op == "osc" {
            self.send_request(Function::openSponsoredChat(td_api::openSponsoredChat {
                sponsored_chat_unique_id: tokenizer::to_integer_i64(args),
            }));
            true
        } else if op == "rsc" {
            let (unique_id, option_id) = tokenizer::split(args, ' ');
            self.send_request(Function::reportSponsoredChat(td_api::reportSponsoredChat {
                sponsored_chat_unique_id: tokenizer::to_integer_i64(&unique_id),
                option_id: option_id.into_bytes(),
            }));
            true
        } else if op == "gvma" {
            let (chat_id, message_id) = tokenizer::split(args, ' ');
            self.send_request(Function::getVideoMessageAdvertisements(
                td_api::getVideoMessageAdvertisements {
                    chat_id: self.as_chat_id(&chat_id),
                    message_id: as_message_id(&message_id),
                },
            ));
            true
        } else if op == "vvma" {
            self.send_request(Function::viewVideoMessageAdvertisement(
                td_api::viewVideoMessageAdvertisement {
                    advertisement_unique_id: tokenizer::to_integer_i64(args),
                },
            ));
            true
        } else if op == "cvma" {
            self.send_request(Function::clickVideoMessageAdvertisement(
                td_api::clickVideoMessageAdvertisement {
                    advertisement_unique_id: tokenizer::to_integer_i64(args),
                },
            ));
            true
        } else if op == "rvma" {
            let (unique_id, option_id) = tokenizer::split(args, ' ');
            self.send_request(Function::reportVideoMessageAdvertisement(
                td_api::reportVideoMessageAdvertisement {
                    advertisement_unique_id: tokenizer::to_integer_i64(&unique_id),
                    option_id: option_id.into_bytes(),
                },
            ));
            true
        } else if op == "gmlink" {
            let (
                chat_id,
                message_id,
                media_timestamp,
                checklist_task_id,
                poll_option_id,
                for_album,
                for_comment,
            ) = {
                let (w0, r0) = tokenizer::split(args, ' ');
                let (w1, r1) = tokenizer::split(&r0, ' ');
                let (w2, r2) = tokenizer::split(&r1, ' ');
                let (w3, r3) = tokenizer::split(&r2, ' ');
                let (w4, r4) = tokenizer::split(&r3, ' ');
                let (w5, w6) = tokenizer::split(&r4, ' ');
                (w0, w1, w2, w3, w4, w5, w6)
            };
            self.send_request(Function::getMessageLink(td_api::getMessageLink {
                chat_id: self.as_chat_id(&chat_id),
                message_id: as_message_id(&message_id),
                media_timestamp: tokenizer::to_integer_i32(&media_timestamp),
                checklist_task_id: tokenizer::to_integer_i32(&checklist_task_id),
                poll_option_id,
                for_album: as_bool(&for_album),
                in_message_thread: as_bool(&for_comment),
            }));
            true
        } else if op == "gmec" {
            let (chat_id, message_id, for_album) = {
                let (w0, r0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&r0, ' ');
                (w0, w1, w2)
            };
            self.send_request(Function::getMessageEmbeddingCode(
                td_api::getMessageEmbeddingCode {
                    chat_id: self.as_chat_id(&chat_id),
                    message_id: as_message_id(&message_id),
                    for_album: as_bool(&for_album),
                },
            ));
            true
        } else if op == "gmli" {
            self.send_request(Function::getMessageLinkInfo(td_api::getMessageLinkInfo {
                url: args.to_owned(),
            }));
            true
        } else if op == "ctcs" || op == "ctcsp" {
            let (title, custom_emoji_id, prompt) = {
                let (w0, r0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&r0, ' ');
                (w0, w1, w2)
            };
            self.send_request(Function::createTextCompositionStyle(
                td_api::createTextCompositionStyle {
                    title,
                    custom_emoji_id: tokenizer::to_integer_i64(&custom_emoji_id),
                    prompt,
                    show_creator: op == "ctcsp",
                },
            ));
            true
        } else if op == "etcs" || op == "etcsp" {
            let (name, title, custom_emoji_id, prompt) = {
                let (w0, r0) = tokenizer::split(args, ' ');
                let (w1, r1) = tokenizer::split(&r0, ' ');
                let (w2, w3) = tokenizer::split(&r1, ' ');
                (w0, w1, w2, w3)
            };
            self.send_request(Function::editTextCompositionStyle(
                td_api::editTextCompositionStyle {
                    name,
                    title,
                    custom_emoji_id: tokenizer::to_integer_i64(&custom_emoji_id),
                    prompt,
                    show_creator: op == "etcsp",
                },
            ));
            true
        } else if op == "dtcs" {
            self.send_request(Function::deleteTextCompositionStyle(
                td_api::deleteTextCompositionStyle {
                    name: args.to_owned(),
                },
            ));
            true
        } else if op == "stcs" {
            self.send_request(Function::searchTextCompositionStyle(
                td_api::searchTextCompositionStyle {
                    name: args.to_owned(),
                },
            ));
            true
        } else if op == "gtcse" {
            let (name, example_number) = tokenizer::split(args, ' ');
            self.send_request(Function::getTextCompositionStyleExample(
                td_api::getTextCompositionStyleExample {
                    name,
                    example_number: tokenizer::to_integer_i32(&example_number),
                },
            ));
            true
        } else if op == "atcs" {
            self.send_request(Function::addTextCompositionStyle(
                td_api::addTextCompositionStyle {
                    name: args.to_owned(),
                },
            ));
            true
        } else if op == "rtcs" {
            self.send_request(Function::removeTextCompositionStyle(
                td_api::removeTextCompositionStyle {
                    name: args.to_owned(),
                },
            ));
            true
        } else if op == "tt" || op == "ttf" || op == "ttc" || op == "ttn" {
            let (to_language_code, text) = tokenizer::split(args, ' ');
            self.send_request(Function::translateText(td_api::translateText {
                text: Some(self.as_formatted_text(&text)),
                to_language_code,
                tone: self.as_tone(op),
            }));
            true
        } else if op == "trm" || op == "trmf" || op == "trmc" || op == "trmn" {
            let (to_language_code, message) = tokenizer::split(args, ' ');
            self.send_request(Function::translateRichMessage(
                td_api::translateRichMessage {
                    message: Some(Box::new(self.as_input_rich_message(&message))),
                    to_language_code,
                    tone: self.as_tone(op),
                },
            ));
            true
        } else if op == "tmt" || op == "tmtf" || op == "tmtc" || op == "tmtn" {
            let (chat_id, message_id, to_language_code) = {
                let (w0, r0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&r0, ' ');
                (w0, w1, w2)
            };
            self.send_request(Function::translateMessageText(
                td_api::translateMessageText {
                    chat_id: self.as_chat_id(&chat_id),
                    message_id: as_message_id(&message_id),
                    to_language_code,
                    tone: self.as_tone(op),
                },
            ));
            true
        } else if op == "tmrm" || op == "tmrmf" || op == "tmrmc" || op == "tmrmn" {
            let (chat_id, message_id, to_language_code) = {
                let (w0, r0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&r0, ' ');
                (w0, w1, w2)
            };
            self.send_request(Function::translateMessageRichMessage(
                td_api::translateMessageRichMessage {
                    chat_id: self.as_chat_id(&chat_id),
                    message_id: as_message_id(&message_id),
                    to_language_code,
                    tone: self.as_tone(op),
                },
            ));
            true
        } else if op == "sum" {
            let (chat_id, message_id, to_language_code, tone) = {
                let (w0, r0) = tokenizer::split(args, ' ');
                let (w1, r1) = tokenizer::split(&r0, ' ');
                let (w2, w3) = tokenizer::split(&r1, ' ');
                (w0, w1, w2, w3)
            };
            self.send_request(Function::summarizeMessage(td_api::summarizeMessage {
                chat_id: self.as_chat_id(&chat_id),
                message_id: as_message_id(&message_id),
                translate_to_language_code: to_language_code,
                tone,
            }));
            true
        } else if op == "ctwa" {
            let (to_language_code, style_name, emojify, text) = {
                let (w0, r0) = tokenizer::split(args, ' ');
                let (w1, r1) = tokenizer::split(&r0, ' ');
                let (w2, w3) = tokenizer::split(&r1, ' ');
                (w0, w1, w2, w3)
            };
            self.send_request(Function::composeTextWithAi(td_api::composeTextWithAi {
                text: Some(self.as_formatted_text(&text)),
                translate_to_language_code: to_language_code,
                style_name,
                add_emojis: as_bool(&emojify),
            }));
            true
        } else if op == "crmwa" || op == "crmwac" {
            let (to_language_code, style_name, emojify, message) = {
                let (w0, r0) = tokenizer::split(args, ' ');
                let (w1, r1) = tokenizer::split(&r0, ' ');
                let (w2, w3) = tokenizer::split(&r1, ' ');
                (w0, w1, w2, w3)
            };
            self.send_request(Function::composeRichMessageWithAi(
                td_api::composeRichMessageWithAi {
                    message: Some(Box::new(self.as_input_rich_message(&message))),
                    translate_to_language_code: to_language_code,
                    style_name,
                    custom_prompt: if op == "crmwac" {
                        "Make text uppercase"
                    } else {
                        ""
                    }
                    .to_owned(),
                    add_emojis: as_bool(&emojify),
                },
            ));
            true
        } else if op == "cnrmwa" {
            let (language_code, emojify, prompt) = {
                let (w0, r0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&r0, ' ');
                (w0, w1, w2)
            };
            self.send_request(Function::createRichMessageWithAi(
                td_api::createRichMessageWithAi {
                    prompt,
                    language_code,
                    add_emojis: as_bool(&emojify),
                },
            ));
            true
        } else if op == "ftwa" {
            self.send_request(Function::fixTextWithAi(td_api::fixTextWithAi {
                text: Some(self.as_formatted_text(args)),
            }));
            true
        } else if op == "frmwa" {
            self.send_request(Function::fixRichMessageWithAi(
                td_api::fixRichMessageWithAi {
                    message: Some(Box::new(self.as_input_rich_message(args))),
                },
            ));
            true
        } else if op == "frtwa" {
            // cli.cpp:5473-5476 (f18704166) — the recognized-text fix.
            self.send_request(Function::fixRecognizedTextWithAi(
                td_api::fixRecognizedTextWithAi {
                    text: Some(self.as_formatted_text(args)),
                },
            ));
            true
        } else if op == "frrmwa" {
            // cli.cpp:5477-5480 (f18704166) — the recognized-rich fix.
            self.send_request(Function::fixRecognizedRichMessageWithAi(
                td_api::fixRecognizedRichMessageWithAi {
                    message: Some(Box::new(self.as_input_rich_message(args))),
                },
            ));
            true
        } else if op == "rs" {
            let (chat_id, message_id) = tokenizer::split(args, ' ');
            self.send_request(Function::recognizeSpeech(td_api::recognizeSpeech {
                chat_id: self.as_chat_id(&chat_id),
                message_id: as_message_id(&message_id),
            }));
            true
        } else if op == "rsr" {
            let (chat_id, message_id, is_good) = {
                let (w0, r0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&r0, ' ');
                (w0, w1, w2)
            };
            self.send_request(Function::rateSpeechRecognition(
                td_api::rateSpeechRecognition {
                    chat_id: self.as_chat_id(&chat_id),
                    message_id: as_message_id(&message_id),
                    is_good: as_bool(&is_good),
                },
            ));
            true
        } else if op == "rsif" {
            // cli.cpp:5492-5496 (fb273279e + 3745d89c7) — the file
            // transcription, with the partial-update recognition id.
            let (audio_file, recognition_id) = tokenizer::split(args, ' ');
            self.send_request(Function::recognizeSpeechInFile(
                td_api::recognizeSpeechInFile {
                    audio_file: as_input_file(&audio_file),
                    recognition_id: tokenizer::to_integer_i64(&recognition_id),
                },
            ));
            true
        } else if op == "gsrl" {
            // cli.cpp:5497-5500 (6484a38a3) — the recognition languages.
            self.send_request(Function::getSpeechRecognitionLanguages(
                td_api::getSpeechRecognitionLanguages {
                    chat_id: self.as_chat_id(args),
                },
            ));
            true
        } else if op == "gf" {
            self.send_request(Function::getFile(td_api::getFile {
                file_id: tokenizer::to_integer_i32(trim_td(args)),
            }));
            true
        } else if op == "gfdps" {
            let (file_id, offset) = tokenizer::split(args, ' ');
            self.send_request(Function::getFileDownloadedPrefixSize(
                td_api::getFileDownloadedPrefixSize {
                    file_id: tokenizer::to_integer_i32(trim_td(&file_id)),
                    offset: tokenizer::to_integer_i64(&offset),
                },
            ));
            true
        } else if op == "rfp" {
            let (file_id, offset, count) = {
                let (w0, r0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&r0, ' ');
                (w0, w1, w2)
            };
            self.send_request(Function::readFilePart(td_api::readFilePart {
                file_id: tokenizer::to_integer_i32(trim_td(&file_id)),
                offset: tokenizer::to_integer_i64(&offset),
                count: tokenizer::to_integer_i64(&count),
            }));
            true
        } else if op == "grf" {
            self.send_request(Function::getRemoteFile(td_api::getRemoteFile {
                remote_file_id: args.to_owned(),
                file_type: None,
            }));
            true
        } else if op == "gmtf" {
            let (latitude, longitude, zoom, width, height, scale, chat_id) = {
                let (w0, r0) = tokenizer::split(args, ' ');
                let (w1, r1) = tokenizer::split(&r0, ' ');
                let (w2, r2) = tokenizer::split(&r1, ' ');
                let (w3, r3) = tokenizer::split(&r2, ' ');
                let (w4, r4) = tokenizer::split(&r3, ' ');
                let (w5, w6) = tokenizer::split(&r4, ' ');
                (w0, w1, w2, w3, w4, w5, w6)
            };
            self.send_request(Function::getMapThumbnailFile(td_api::getMapThumbnailFile {
                location: as_location(&latitude, &longitude, ""),
                zoom: tokenizer::to_integer_i32(&zoom),
                width: tokenizer::to_integer_i32(&width),
                height: tokenizer::to_integer_i32(&height),
                scale: tokenizer::to_integer_i32(&scale),
                chat_id: self.as_chat_id(&chat_id),
            }));
            true
        } else if op == "df" || op == "dff" || op == "dfs" {
            let (file_id, offset, limit, priority) = {
                let (w0, r0) = tokenizer::split(args, ' ');
                let (w1, r1) = tokenizer::split(&r0, ' ');
                let (w2, w3) = tokenizer::split(&r1, ' ');
                (w0, w1, w2, w3)
            };
            let mut priority = tokenizer::to_integer_i32(&priority);
            if priority <= 0 {
                priority = 1;
            }
            let max_file_id = tokenizer::to_integer_i32(trim_td(&file_id));
            let min_file_id = if op == "dff" { 1 } else { max_file_id };
            for i in min_file_id..=max_file_id {
                self.send_request(Function::downloadFile(td_api::downloadFile {
                    file_id: i,
                    priority,
                    offset: tokenizer::to_integer_i64(&offset),
                    limit: tokenizer::to_integer_i64(&limit),
                    synchronous: op == "dfs",
                }));
            }
            true
        } else if op == "cdf" {
            self.send_request(Function::cancelDownloadFile(td_api::cancelDownloadFile {
                file_id: tokenizer::to_integer_i32(trim_td(args)),
                only_if_pending: false,
            }));
            true
        } else if op == "gsfn" {
            let (file_id, directory_name) = tokenizer::split(args, ' ');
            self.send_request(Function::getSuggestedFileName(
                td_api::getSuggestedFileName {
                    file_id: tokenizer::to_integer_i32(trim_td(&file_id)),
                    directory: directory_name,
                },
            ));
            true
        } else if op == "uf" || op == "ufs" || op == "ufse" {
            let (file_path, priority) = tokenizer::split(args, ' ');
            let mut priority = tokenizer::to_integer_i32(&priority);
            if priority <= 0 {
                priority = 1;
            }
            let file_type = if op == "ufs" {
                td_api::FileType::fileTypeSecret(td_api::fileTypeSecret)
            } else if op == "ufse" {
                td_api::FileType::fileTypeSecure(td_api::fileTypeSecure)
            } else {
                td_api::FileType::fileTypePhoto(td_api::fileTypePhoto)
            };
            self.send_request(Function::preliminaryUploadFile(
                td_api::preliminaryUploadFile {
                    file: as_input_file(&file_path),
                    file_type: Some(file_type),
                    priority,
                },
            ));
            true
        } else if op == "ufg" {
            let (file_path, conversion) = tokenizer::split(args, ' ');
            self.send_request(Function::preliminaryUploadFile(
                td_api::preliminaryUploadFile {
                    file: Some(td_api::InputFile::inputFileGenerated(
                        td_api::inputFileGenerated {
                            original_path: file_path,
                            conversion,
                            expected_size: 0,
                        },
                    )),
                    file_type: Some(td_api::FileType::fileTypePhoto(td_api::fileTypePhoto)),
                    priority: 1,
                },
            ));
            true
        } else if op == "cuf" {
            self.send_request(Function::cancelPreliminaryUploadFile(
                td_api::cancelPreliminaryUploadFile {
                    file_id: tokenizer::to_integer_i32(trim_td(args)),
                },
            ));
            true
        } else if op == "delf" {
            self.send_request(Function::deleteFile(td_api::deleteFile {
                file_id: tokenizer::to_integer_i32(trim_td(args)),
            }));
            true
        } else if op == "aftd" {
            let (file_id, chat_id, message_id, priority) = {
                let (w0, r0) = tokenizer::split(args, ' ');
                let (w1, r1) = tokenizer::split(&r0, ' ');
                let (w2, w3) = tokenizer::split(&r1, ' ');
                (w0, w1, w2, w3)
            };
            self.send_request(Function::addFileToDownloads(td_api::addFileToDownloads {
                file_id: tokenizer::to_integer_i32(trim_td(&file_id)),
                chat_id: self.as_chat_id(&chat_id),
                message_id: as_message_id(&message_id),
                priority: tokenizer::to_integer_i32(&priority).max(1),
            }));
            true
        } else if op == "tdip" {
            let (file_id, is_paused) = tokenizer::split(args, ' ');
            self.send_request(Function::toggleDownloadIsPaused(
                td_api::toggleDownloadIsPaused {
                    file_id: tokenizer::to_integer_i32(trim_td(&file_id)),
                    is_paused: as_bool(&is_paused),
                },
            ));
            true
        } else if op == "tadap" {
            self.send_request(Function::toggleAllDownloadsArePaused(
                td_api::toggleAllDownloadsArePaused {
                    are_paused: as_bool(args),
                },
            ));
            true
        } else if op == "rffd" {
            let (file_id, delete_from_cache) = tokenizer::split(args, ' ');
            self.send_request(Function::removeFileFromDownloads(
                td_api::removeFileFromDownloads {
                    file_id: tokenizer::to_integer_i32(trim_td(&file_id)),
                    delete_from_cache: as_bool(&delete_from_cache),
                },
            ));
            true
        } else if op == "raffd" || op == "raffda" || op == "raffdc" {
            self.send_request(Function::removeAllFilesFromDownloads(
                td_api::removeAllFilesFromDownloads {
                    only_active: op.ends_with('a'),
                    only_completed: op.ends_with('c'),
                    delete_from_cache: as_bool(args),
                },
            ));
            true
        } else if op == "sfd" || op == "sfda" || op == "sfdc" {
            let (offset, tail) = tokenizer::split(args, ' ');
            let (limit, query) = get_search_query(&tail);
            self.send_request(Function::searchFileDownloads(td_api::searchFileDownloads {
                query,
                only_active: op.ends_with('a'),
                only_completed: op.ends_with('c'),
                offset,
                limit,
            }));
            true
        } else if op == "dm" || op == "dmr" {
            let (chat_id, message_ids) = tokenizer::split(args, ' ');
            self.send_request(Function::deleteMessages(td_api::deleteMessages {
                chat_id: self.as_chat_id(&chat_id),
                message_ids: autosplit(&message_ids)
                    .iter()
                    .map(|t| as_message_id(trim_td(t)))
                    .collect(),
                revoke: op == "dmr",
            }));
            true
        } else if op == "fm" || op == "cm" {
            let (chat_id, from_chat_id, message_ids) = {
                let (w0, r0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&r0, ' ');
                (w0, w1, w2)
            };
            self.send_request(Function::forwardMessages(td_api::forwardMessages {
                chat_id: self.as_chat_id(&chat_id),
                topic_id: self.get_message_topic_id(),
                from_chat_id: self.as_chat_id(&from_chat_id),
                message_ids: autosplit(&message_ids)
                    .iter()
                    .map(|t| as_message_id(trim_td(t)))
                    .collect(),
                options: Some(self.default_message_send_options()),
                send_copy: op.starts_with('c'),
                remove_caption: random::fast_bool(),
            }));
            true
        } else if op == "sqrsm" {
            let (chat_id, shortcut_id) = tokenizer::split(args, ' ');
            self.send_request(Function::sendQuickReplyShortcutMessages(
                td_api::sendQuickReplyShortcutMessages {
                    chat_id: self.as_chat_id(&chat_id),
                    shortcut_id: self.as_shortcut_id(&shortcut_id),
                    sending_id: random::fast(-1000, -1),
                },
            ));
            true
        } else if op == "resend" {
            let (chat_id, message_ids, quote, quote_position) = {
                let (w0, r0) = tokenizer::split(args, ' ');
                let (w1, r1) = tokenizer::split(&r0, ' ');
                let (w2, w3) = tokenizer::split(&r1, ' ');
                (w0, w1, w2, w3)
            };
            let shortcut = self.quick_reply_shortcut_name.lock().unwrap().clone();
            if shortcut.is_empty() {
                self.send_request(Function::resendMessages(td_api::resendMessages {
                    chat_id: self.as_chat_id(&chat_id),
                    message_ids: autosplit(&message_ids)
                        .iter()
                        .map(|t| as_message_id(trim_td(t)))
                        .collect(),
                    quote: Some(td_api::InputTextQuote {
                        text: Some(self.as_formatted_text(&quote)),
                        // 1.8.70 rich-quote plane — the CLI keeps the
                        // plain quote.
                        rich_texts: Vec::new(),
                        position: tokenizer::to_integer_i32(&quote_position),
                    }),
                    paid_message_star_count: self.paid_message_star_count.load(Ordering::SeqCst),
                }));
            } else {
                self.send_request(Function::readdQuickReplyShortcutMessages(
                    td_api::readdQuickReplyShortcutMessages {
                        shortcut_name: shortcut,
                        message_ids: autosplit(&message_ids)
                            .iter()
                            .map(|t| as_message_id(trim_td(t)))
                            .collect(),
                    },
                ));
            }
            true
        } else if op == "csc" {
            self.send_request(Function::createSecretChat(td_api::createSecretChat {
                secret_chat_id: self.as_secret_chat_id(args),
            }));
            true
        } else if op == "cnsc" {
            self.send_request(Function::createNewSecretChat(td_api::createNewSecretChat {
                user_id: self.as_user_id(args),
            }));
            true
        } else if op == "closeSC" || op == "cancelSC" {
            self.send_request(Function::closeSecretChat(td_api::closeSecretChat {
                secret_chat_id: self.as_secret_chat_id(args),
            }));
            true
        } else {
            false
        }
    }

    /// cli.cpp:7327 — the chat-folder/paid-messages/chat-settings chain
    /// (the census chain 14 remainder).
    fn op_block_stickers(&self, op: &str, args: &str) -> bool {
        // cli.cpp:5510 — the call/group-call/live-story chain (the arms below
        // the `gcltac` group are the cli.cpp:8000 chat-folder/dev strays).
        if op == "cc" {
            self.send_request(Function::createCall(td_api::createCall {
                user_id: self.as_user_id(args),
                protocol: Some(td_api::CallProtocol {
                    udp_p2p: true,
                    udp_reflector: true,
                    min_layer: 65,
                    max_layer: 65,
                    library_versions: vec!["2.6".to_owned(), "3.0".to_owned()],
                }),
                is_video: random::fast_bool(),
            }));
            true
        } else if op == "ac" {
            self.send_request(Function::acceptCall(td_api::acceptCall {
                call_id: tokenizer::to_integer_i32(trim_td(args)),
                protocol: Some(td_api::CallProtocol {
                    udp_p2p: true,
                    udp_reflector: true,
                    min_layer: 65,
                    max_layer: 65,
                    library_versions: vec!["2.6".to_owned(), "3.0".to_owned()],
                }),
            }));
            true
        } else if op == "scsd" {
            self.send_request(Function::sendCallSignalingData(
                td_api::sendCallSignalingData {
                    call_id: tokenizer::to_integer_i32(trim_td(args)),
                    data: b"abacaba".to_vec(),
                },
            ));
            true
        } else if op == "dc" {
            let (call_id, rest0) = tokenizer::split(args, ' ');
            let (is_disconnected, invite_link) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::discardCall(td_api::discardCall {
                call_id: tokenizer::to_integer_i32(&call_id),
                is_disconnected: as_bool(&is_disconnected),
                invite_link,
                duration: 0,
                is_video: random::fast_bool(),
                connection_id: 0,
            }));
            true
        } else if op == "scr" {
            let (call_id, rating) = tokenizer::split(args, ' ');
            let problems: Vec<td_api::CallProblem> = vec![
                td_api::CallProblem::callProblemNoise(td_api::callProblemNoise),
                td_api::CallProblem::callProblemNoise(td_api::callProblemNoise),
                td_api::CallProblem::callProblemDistortedVideo(td_api::callProblemDistortedVideo),
                // C++ emplaces a null problem here; the vector model drops it.
                td_api::CallProblem::callProblemNoise(td_api::callProblemNoise),
                td_api::CallProblem::callProblemEcho(td_api::callProblemEcho),
                td_api::CallProblem::callProblemPixelatedVideo(td_api::callProblemPixelatedVideo),
                td_api::CallProblem::callProblemDistortedSpeech(td_api::callProblemDistortedSpeech),
            ];
            self.send_request(Function::sendCallRating(td_api::sendCallRating {
                call_id: self.get_input_call(&call_id),
                rating: tokenizer::to_integer_i32(&rating),
                comment: "Wow, such good call! (TDLib test)".to_owned(),
                problems,
            }));
            true
        } else if op == "scdi" {
            self.send_request(Function::sendCallDebugInformation(
                td_api::sendCallDebugInformation {
                    call_id: self.get_input_call(args),
                    debug_information: "{}".to_owned(),
                },
            ));
            true
        } else if op == "sclog" {
            let (call_id, log_file) = tokenizer::split(args, ' ');
            self.send_request(Function::sendCallLog(td_api::sendCallLog {
                call_id: self.get_input_call(&call_id),
                log_file: as_input_file(&log_file),
            }));
            true
        } else if op == "gvcap" {
            self.send_request(Function::getVideoChatAvailableParticipants(
                td_api::getVideoChatAvailableParticipants {
                    chat_id: self.as_chat_id(args),
                },
            ));
            true
        } else if op == "svcdp" {
            let (chat_id, participant_id) = tokenizer::split(args, ' ');
            self.send_request(Function::setVideoChatDefaultParticipant(
                td_api::setVideoChatDefaultParticipant {
                    chat_id: self.as_chat_id(&chat_id),
                    default_participant_id: self.as_message_sender(&participant_id),
                },
            ));
            true
        } else if op == "cvc" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (title, rest1) = tokenizer::split(&rest0, ' ');
            let (start_date, is_rtmp_stream) = tokenizer::split(&rest1, ' ');
            self.send_request(Function::createVideoChat(td_api::createVideoChat {
                chat_id: self.as_chat_id(&chat_id),
                title,
                start_date: tokenizer::to_integer_i32(&start_date),
                is_rtmp_stream: as_bool(&is_rtmp_stream),
            }));
            true
        } else if op == "cgc" || op == "cgcv" || op == "cgca" {
            let parameters = if op != "cgc" {
                Some(td_api::GroupCallJoinParameters {
                    audio_source_id: self.group_call_source.load(Ordering::SeqCst),
                    payload: self.get_group_call_join_payload(op == "cgcv", false),
                    is_muted: true,
                    is_my_video_enabled: true,
                })
            } else {
                None
            };
            self.send_request(Function::createGroupCall(td_api::createGroupCall {
                join_parameters: parameters,
            }));
            true
        } else if op == "gvcru" {
            self.send_request(Function::getVideoChatRtmpUrl(td_api::getVideoChatRtmpUrl {
                chat_id: self.as_chat_id(args),
            }));
            true
        } else if op == "rvcru" {
            self.send_request(Function::replaceVideoChatRtmpUrl(
                td_api::replaceVideoChatRtmpUrl {
                    chat_id: self.as_chat_id(args),
                },
            ));
            true
        } else if op == "glsru" {
            self.send_request(Function::getLiveStoryRtmpUrl(td_api::getLiveStoryRtmpUrl {
                chat_id: self.as_chat_id(args),
            }));
            true
        } else if op == "rlsru" {
            self.send_request(Function::replaceLiveStoryRtmpUrl(
                td_api::replaceLiveStoryRtmpUrl {
                    chat_id: self.as_chat_id(args),
                },
            ));
            true
        } else if op == "ggc" {
            self.send_request(Function::getGroupCall(td_api::getGroupCall {
                group_call_id: tokenizer::to_integer_i32(trim_td(args)),
            }));
            true
        } else if op == "ggcs" {
            self.send_request(Function::getGroupCallStreams(td_api::getGroupCallStreams {
                group_call_id: tokenizer::to_integer_i32(trim_td(args)),
            }));
            true
        } else if op == "ggcss" {
            let (group_call_id, channel_id) = tokenizer::split(args, ' ');
            self.send_request(Function::getGroupCallStreamSegment(
                td_api::getGroupCallStreamSegment {
                    group_call_id: tokenizer::to_integer_i32(&group_call_id),
                    time_offset: ((time::now() as i64) - 5) * 1000,
                    scale: 0,
                    channel_id: tokenizer::to_integer_i32(&channel_id),
                    video_quality: None,
                },
            ));
            true
        } else if op == "egcd" || op == "egcds" {
            let (group_call_id, rest0) = tokenizer::split(args, ' ');
            let (unecrypted_prefix_size, data) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::encryptGroupCallData(
                td_api::encryptGroupCallData {
                    group_call_id: tokenizer::to_integer_i32(&group_call_id),
                    data_channel: if op == "egcds" {
                        Some(
                            td_api::GroupCallDataChannel::groupCallDataChannelScreenSharing(
                                td_api::groupCallDataChannelScreenSharing,
                            ),
                        )
                    } else {
                        None
                    },
                    data: data.into_bytes(),
                    unencrypted_prefix_size: tokenizer::to_integer_i32(&unecrypted_prefix_size),
                },
            ));
            true
        } else if op == "dgcd" || op == "dgcds" {
            let (group_call_id, rest0) = tokenizer::split(args, ' ');
            let (participant_id, data) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::decryptGroupCallData(
                td_api::decryptGroupCallData {
                    group_call_id: tokenizer::to_integer_i32(&group_call_id),
                    participant_id: self.as_message_sender(&participant_id),
                    data_channel: if op == "dgcds" {
                        Some(
                            td_api::GroupCallDataChannel::groupCallDataChannelScreenSharing(
                                td_api::groupCallDataChannelScreenSharing,
                            ),
                        )
                    } else {
                        None
                    },
                    data: data.into_bytes(),
                },
            ));
            true
        } else if op == "ssvc" {
            self.send_request(Function::startScheduledVideoChat(
                td_api::startScheduledVideoChat {
                    group_call_id: tokenizer::to_integer_i32(trim_td(args)),
                },
            ));
            true
        } else if op == "tvcesn" || op == "tvcesne" {
            self.send_request(Function::toggleVideoChatEnabledStartNotification(
                td_api::toggleVideoChatEnabledStartNotification {
                    group_call_id: tokenizer::to_integer_i32(trim_td(args)),
                    enabled_start_notification: op == "tvcesne",
                },
            ));
            true
        } else if op == "jgc" || op == "jgcv" {
            self.send_request(Function::joinGroupCall(td_api::joinGroupCall {
                input_group_call: Some(self.get_input_group_call(args)),
                join_parameters: Some(td_api::GroupCallJoinParameters {
                    audio_source_id: self.group_call_source.load(Ordering::SeqCst),
                    payload: self.get_group_call_join_payload(op == "jgcv", false),
                    is_muted: true,
                    is_my_video_enabled: true,
                }),
            }));
            true
        } else if op == "jvc" || op == "jvcv" {
            let (group_call_id, rest0) = tokenizer::split(args, ' ');
            let (participant_id, invite_hash) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::joinVideoChat(td_api::joinVideoChat {
                group_call_id: tokenizer::to_integer_i32(&group_call_id),
                participant_id: self.as_message_sender(&participant_id),
                join_parameters: Some(td_api::GroupCallJoinParameters {
                    audio_source_id: self.group_call_source.load(Ordering::SeqCst),
                    payload: self.get_group_call_join_payload(op == "jvcv", false),
                    is_muted: true,
                    is_my_video_enabled: true,
                }),
                invite_hash,
            }));
            true
        } else if op == "jls" || op == "jlsv" {
            self.send_request(Function::joinLiveStory(td_api::joinLiveStory {
                group_call_id: tokenizer::to_integer_i32(trim_td(args)),
                join_parameters: Some(td_api::GroupCallJoinParameters {
                    audio_source_id: self.group_call_source.load(Ordering::SeqCst),
                    payload: self.get_group_call_join_payload(op == "jlsv", false),
                    is_muted: true,
                    is_my_video_enabled: true,
                }),
            }));
            true
        } else if op == "sgcss" {
            self.send_request(Function::startGroupCallScreenSharing(
                td_api::startGroupCallScreenSharing {
                    group_call_id: tokenizer::to_integer_i32(trim_td(args)),
                    audio_source_id: self.group_call_source.load(Ordering::SeqCst) + 1,
                    payload: self.get_group_call_join_payload(true, true),
                },
            ));
            true
        } else if op == "tgcssip" {
            let (group_call_id, is_paused) = tokenizer::split(args, ' ');
            self.send_request(Function::toggleGroupCallScreenSharingIsPaused(
                td_api::toggleGroupCallScreenSharingIsPaused {
                    group_call_id: tokenizer::to_integer_i32(&group_call_id),
                    is_paused: as_bool(&is_paused),
                },
            ));
            true
        } else if op == "egcss" {
            self.send_request(Function::endGroupCallScreenSharing(
                td_api::endGroupCallScreenSharing {
                    group_call_id: tokenizer::to_integer_i32(trim_td(args)),
                },
            ));
            true
        } else if op == "svct" {
            let (group_call_id, title) = tokenizer::split(args, ' ');
            self.send_request(Function::setVideoChatTitle(td_api::setVideoChatTitle {
                group_call_id: tokenizer::to_integer_i32(&group_call_id),
                title,
            }));
            true
        } else if op == "tvcmnp" || op == "tvcmnpe" {
            self.send_request(Function::toggleVideoChatMuteNewParticipants(
                td_api::toggleVideoChatMuteNewParticipants {
                    group_call_id: tokenizer::to_integer_i32(trim_td(args)),
                    mute_new_participants: op == "tvcmnpe",
                },
            ));
            true
        } else if op == "tgcama" {
            let (group_call_id, are_messages_allowed) = tokenizer::split(args, ' ');
            self.send_request(Function::toggleGroupCallAreMessagesAllowed(
                td_api::toggleGroupCallAreMessagesAllowed {
                    group_call_id: tokenizer::to_integer_i32(&group_call_id),
                    are_messages_allowed: as_bool(&are_messages_allowed),
                },
            ));
            true
        } else if op == "sgcpmsc" {
            let (group_call_id, paid_message_star_count) = tokenizer::split(args, ' ');
            self.send_request(Function::setGroupCallPaidMessageStarCount(
                td_api::setGroupCallPaidMessageStarCount {
                    group_call_id: tokenizer::to_integer_i32(&group_call_id),
                    paid_message_star_count: tokenizer::to_integer_i64(&paid_message_star_count),
                },
            ));
            true
        } else if op == "glss" {
            self.send_request(Function::getLiveStoryStreamer(
                td_api::getLiveStoryStreamer {
                    group_call_id: tokenizer::to_integer_i32(trim_td(args)),
                },
            ));
            true
        } else if op == "glsams" {
            self.send_request(Function::getLiveStoryAvailableMessageSenders(
                td_api::getLiveStoryAvailableMessageSenders {
                    group_call_id: tokenizer::to_integer_i32(trim_td(args)),
                },
            ));
            true
        } else if op == "slsms" {
            let (group_call_id, message_sender_id) = tokenizer::split(args, ' ');
            self.send_request(Function::setLiveStoryMessageSender(
                td_api::setLiveStoryMessageSender {
                    group_call_id: tokenizer::to_integer_i32(&group_call_id),
                    message_sender_id: self.as_message_sender(&message_sender_id),
                },
            ));
            true
        } else if op == "sgcm" {
            let (group_call_id, text) = tokenizer::split(args, ' ');
            self.send_request(Function::sendGroupCallMessage(
                td_api::sendGroupCallMessage {
                    group_call_id: tokenizer::to_integer_i32(&group_call_id),
                    text: Some(self.as_formatted_text(&text)),
                    paid_message_star_count: self.paid_message_star_count.load(Ordering::SeqCst),
                },
            ));
            true
        } else if op == "aplsr" {
            let (group_call_id, star_count) = tokenizer::split(args, ' ');
            self.send_request(Function::addPendingLiveStoryReaction(
                td_api::addPendingLiveStoryReaction {
                    group_call_id: tokenizer::to_integer_i32(&group_call_id),
                    star_count: tokenizer::to_integer_i64(&star_count),
                },
            ));
            true
        } else if op == "cplsr" {
            self.send_request(Function::commitPendingLiveStoryReactions(
                td_api::commitPendingLiveStoryReactions {
                    group_call_id: tokenizer::to_integer_i32(trim_td(args)),
                },
            ));
            true
        } else if op == "rplsr" {
            self.send_request(Function::removePendingLiveStoryReactions(
                td_api::removePendingLiveStoryReactions {
                    group_call_id: tokenizer::to_integer_i32(trim_td(args)),
                },
            ));
            true
        } else if op == "dgcm" || op == "dgcms" {
            let (group_call_id, message_ids) = tokenizer::split(args, ' ');
            self.send_request(Function::deleteGroupCallMessages(
                td_api::deleteGroupCallMessages {
                    group_call_id: tokenizer::to_integer_i32(&group_call_id),
                    message_ids: to_integers_i32(&message_ids),
                    report_spam: op == "dgcms",
                },
            ));
            true
        } else if op == "dgcmbs" || op == "dgcmbss" {
            let (group_call_id, sender_id) = tokenizer::split(args, ' ');
            self.send_request(Function::deleteGroupCallMessagesBySender(
                td_api::deleteGroupCallMessagesBySender {
                    group_call_id: tokenizer::to_integer_i32(&group_call_id),
                    sender_id: self.as_message_sender(&sender_id),
                    report_spam: op == "dgcmbss",
                },
            ));
            true
        } else if op == "glstd" {
            self.send_request(Function::getLiveStoryTopDonors(
                td_api::getLiveStoryTopDonors {
                    group_call_id: tokenizer::to_integer_i32(trim_td(args)),
                },
            ));
            true
        } else if op == "rgcil" {
            self.send_request(Function::revokeGroupCallInviteLink(
                td_api::revokeGroupCallInviteLink {
                    group_call_id: tokenizer::to_integer_i32(trim_td(args)),
                },
            ));
            true
        } else if op == "tgcimvp" {
            let (group_call_id, is_my_video_paused) = tokenizer::split(args, ' ');
            self.send_request(Function::toggleGroupCallIsMyVideoPaused(
                td_api::toggleGroupCallIsMyVideoPaused {
                    group_call_id: tokenizer::to_integer_i32(&group_call_id),
                    is_my_video_paused: as_bool(&is_my_video_paused),
                },
            ));
            true
        } else if op == "tgcimve" {
            let (group_call_id, is_my_video_enabled) = tokenizer::split(args, ' ');
            self.send_request(Function::toggleGroupCallIsMyVideoEnabled(
                td_api::toggleGroupCallIsMyVideoEnabled {
                    group_call_id: tokenizer::to_integer_i32(&group_call_id),
                    is_my_video_enabled: as_bool(&is_my_video_enabled),
                },
            ));
            true
        } else if op == "sgcpis" {
            let (group_call_id, rest0) = tokenizer::split(args, ' ');
            let (source_id, is_speaking) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::setGroupCallParticipantIsSpeaking(
                td_api::setGroupCallParticipantIsSpeaking {
                    group_call_id: tokenizer::to_integer_i32(&group_call_id),
                    audio_source: tokenizer::to_integer_i32(&source_id),
                    is_speaking: as_bool(&is_speaking),
                },
            ));
            true
        } else if op == "igcp" || op == "igcpv" {
            let (group_call_id, user_id) = tokenizer::split(args, ' ');
            self.send_request(Function::inviteGroupCallParticipant(
                td_api::inviteGroupCallParticipant {
                    group_call_id: tokenizer::to_integer_i32(&group_call_id),
                    user_id: self.as_user_id(&user_id),
                    is_video: op == "igcpv",
                },
            ));
            true
        } else if op == "dgci" {
            let (chat_id, message_id) = tokenizer::split(args, ' ');
            self.send_request(Function::declineGroupCallInvitation(
                td_api::declineGroupCallInvitation {
                    chat_id: self.as_chat_id(&chat_id),
                    message_id: as_message_id(&message_id),
                },
            ));
            true
        } else if op == "bgcp" {
            let (group_call_id, user_ids) = tokenizer::split(args, ' ');
            self.send_request(Function::banGroupCallParticipants(
                td_api::banGroupCallParticipants {
                    group_call_id: tokenizer::to_integer_i32(&group_call_id),
                    user_ids: self.as_user_ids(&user_ids),
                },
            ));
            true
        } else if op == "ivcp" {
            let (group_call_id, user_ids) = tokenizer::split(args, ' ');
            self.send_request(Function::inviteVideoChatParticipants(
                td_api::inviteVideoChatParticipants {
                    group_call_id: tokenizer::to_integer_i32(&group_call_id),
                    user_ids: self.as_user_ids(&user_ids),
                },
            ));
            true
        } else if op == "gvcil" {
            let (group_call_id, can_self_unmute) = tokenizer::split(args, ' ');
            self.send_request(Function::getVideoChatInviteLink(
                td_api::getVideoChatInviteLink {
                    group_call_id: tokenizer::to_integer_i32(&group_call_id),
                    can_self_unmute: as_bool(&can_self_unmute),
                },
            ));
            true
        } else if op == "sgcr" {
            let (group_call_id, rest0) = tokenizer::split(args, ' ');
            let (title, rest1) = tokenizer::split(&rest0, ' ');
            let (record_video, use_portrait_orientation) = tokenizer::split(&rest1, ' ');
            self.send_request(Function::startGroupCallRecording(
                td_api::startGroupCallRecording {
                    group_call_id: tokenizer::to_integer_i32(&group_call_id),
                    title,
                    record_video: as_bool(&record_video),
                    use_portrait_orientation: as_bool(&use_portrait_orientation),
                },
            ));
            true
        } else if op == "egcr" {
            self.send_request(Function::endGroupCallRecording(
                td_api::endGroupCallRecording {
                    group_call_id: tokenizer::to_integer_i32(trim_td(args)),
                },
            ));
            true
        } else if op == "tgcpim" {
            let (group_call_id, rest0) = tokenizer::split(args, ' ');
            let (participant_id, is_muted) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::toggleGroupCallParticipantIsMuted(
                td_api::toggleGroupCallParticipantIsMuted {
                    group_call_id: tokenizer::to_integer_i32(&group_call_id),
                    participant_id: self.as_message_sender(&participant_id),
                    is_muted: as_bool(&is_muted),
                },
            ));
            true
        } else if op == "sgcpvl" {
            let (group_call_id, rest0) = tokenizer::split(args, ' ');
            let (participant_id, volume_level) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::setGroupCallParticipantVolumeLevel(
                td_api::setGroupCallParticipantVolumeLevel {
                    group_call_id: tokenizer::to_integer_i32(&group_call_id),
                    participant_id: self.as_message_sender(&participant_id),
                    volume_level: tokenizer::to_integer_i32(&volume_level),
                },
            ));
            true
        } else if op == "tgcpihr" {
            let (group_call_id, rest0) = tokenizer::split(args, ' ');
            let (participant_id, is_hand_raised) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::toggleGroupCallParticipantIsHandRaised(
                td_api::toggleGroupCallParticipantIsHandRaised {
                    group_call_id: tokenizer::to_integer_i32(&group_call_id),
                    participant_id: self.as_message_sender(&participant_id),
                    is_hand_raised: as_bool(&is_hand_raised),
                },
            ));
            true
        } else if op == "ggcp" {
            let (input_group_call, limit) = tokenizer::split(args, ' ');
            self.send_request(Function::getGroupCallParticipants(
                td_api::getGroupCallParticipants {
                    input_group_call: Some(self.get_input_group_call(&input_group_call)),
                    limit: as_limit(&limit, 10),
                },
            ));
            true
        } else if op == "lgcp" {
            let (group_call_id, limit) = tokenizer::split(args, ' ');
            self.send_request(Function::loadGroupCallParticipants(
                td_api::loadGroupCallParticipants {
                    group_call_id: tokenizer::to_integer_i32(&group_call_id),
                    limit: as_limit(&limit, 10),
                },
            ));
            true
        } else if op == "lgc" {
            self.send_request(Function::leaveGroupCall(td_api::leaveGroupCall {
                group_call_id: tokenizer::to_integer_i32(trim_td(args)),
            }));
            true
        } else if op == "egc" {
            self.send_request(Function::endGroupCall(td_api::endGroupCall {
                group_call_id: tokenizer::to_integer_i32(trim_td(args)),
            }));
            true
        } else if op == "gcltac" {
            self.send_request(Function::getChatListsToAddChat(
                td_api::getChatListsToAddChat {
                    chat_id: self.as_chat_id(args),
                },
            ));
            true
        } else if op == "actl" || op == "actla" || op.starts_with("actl-") {
            self.send_request(Function::addChatToList(td_api::addChatToList {
                chat_id: self.as_chat_id(args),
                chat_list: as_chat_list(op),
            }));
            true
        } else if op == "gcf" {
            self.send_request(Function::getChatFolder(td_api::getChatFolder {
                chat_folder_id: as_chat_folder_id(args),
            }));
            true
        } else if op == "ccf" {
            self.send_request(Function::createChatFolder(td_api::createChatFolder {
                folder: Some(self.as_chat_folder(args, false)),
            }));
            true
        } else if op == "ccfe" {
            let folder = td_api::ChatFolder {
                name: Some(td_api::ChatFolderName {
                    text: Some(td_api::FormattedText {
                        text: "empty".to_owned(),
                        entities: Vec::new(),
                    }),
                    animate_custom_emoji: true,
                }),
                included_chat_ids: self.as_chat_ids(args),
                ..Default::default()
            };
            self.send_request(Function::createChatFolder(td_api::createChatFolder {
                folder: Some(folder),
            }));
            true
        } else if op == "ecf" || op == "ecfs" {
            let (chat_folder_id, filter) = tokenizer::split(args, ' ');
            self.send_request(Function::editChatFolder(td_api::editChatFolder {
                chat_folder_id: as_chat_folder_id(&chat_folder_id),
                folder: Some(self.as_chat_folder(&filter, op == "ecfs")),
            }));
            true
        } else if op == "dcf" {
            let (chat_folder_id, chat_ids) = tokenizer::split(args, ' ');
            self.send_request(Function::deleteChatFolder(td_api::deleteChatFolder {
                chat_folder_id: as_chat_folder_id(&chat_folder_id),
                leave_chat_ids: self.as_chat_ids(&chat_ids),
            }));
            true
        } else if op == "gcfctl" {
            self.send_request(Function::getChatFolderChatsToLeave(
                td_api::getChatFolderChatsToLeave {
                    chat_folder_id: as_chat_folder_id(args),
                },
            ));
            true
        } else if op == "gcfcc" {
            self.send_request(Function::getChatFolderChatCount(
                td_api::getChatFolderChatCount {
                    folder: Some(self.as_chat_folder(args, false)),
                },
            ));
            true
        } else if op == "rcf" {
            let (main_chat_list_position, chat_folder_ids) = tokenizer::split(args, ' ');
            self.send_request(Function::reorderChatFolders(td_api::reorderChatFolders {
                chat_folder_ids: autosplit(&chat_folder_ids)
                    .iter()
                    .map(|t| tokenizer::to_integer_i32(trim_td(t)))
                    .collect(),
                main_chat_list_position: tokenizer::to_integer_i32(&main_chat_list_position),
            }));
            true
        } else if op == "tcft" {
            self.send_request(Function::toggleChatFolderTags(
                td_api::toggleChatFolderTags {
                    are_tags_enabled: as_bool(args),
                },
            ));
            true
        } else if op == "gcfcfil" {
            self.send_request(Function::getChatsForChatFolderInviteLink(
                td_api::getChatsForChatFolderInviteLink {
                    chat_folder_id: as_chat_folder_id(args),
                },
            ));
            true
        } else if op == "crcfil" {
            let (chat_folder_id, name, chat_ids) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            self.send_request(Function::createChatFolderInviteLink(
                td_api::createChatFolderInviteLink {
                    chat_folder_id: as_chat_folder_id(&chat_folder_id),
                    name,
                    chat_ids: self.as_chat_ids(&chat_ids),
                },
            ));
            true
        } else if op == "gcfil" {
            self.send_request(Function::getChatFolderInviteLinks(
                td_api::getChatFolderInviteLinks {
                    chat_folder_id: as_chat_folder_id(args),
                },
            ));
            true
        } else if op == "ecfil" {
            let (chat_folder_id, invite_link, name, chat_ids) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, rest1) = tokenizer::split(&rest0, ' ');
                let (w2, w3) = tokenizer::split(&rest1, ' ');
                (w0, w1, w2, w3)
            };
            self.send_request(Function::editChatFolderInviteLink(
                td_api::editChatFolderInviteLink {
                    chat_folder_id: as_chat_folder_id(&chat_folder_id),
                    invite_link,
                    name,
                    chat_ids: self.as_chat_ids(&chat_ids),
                },
            ));
            true
        } else if op == "dcfil" {
            let (chat_folder_id, invite_link) = tokenizer::split(args, ' ');
            self.send_request(Function::deleteChatFolderInviteLink(
                td_api::deleteChatFolderInviteLink {
                    chat_folder_id: as_chat_folder_id(&chat_folder_id),
                    invite_link,
                },
            ));
            true
        } else if op == "ccfil" {
            self.send_request(Function::checkChatFolderInviteLink(
                td_api::checkChatFolderInviteLink {
                    invite_link: args.to_owned(),
                },
            ));
            true
        } else if op == "acfbil" {
            let (invite_link, chat_ids) = tokenizer::split(args, ' ');
            self.send_request(Function::addChatFolderByInviteLink(
                td_api::addChatFolderByInviteLink {
                    invite_link,
                    chat_ids: self.as_chat_ids(&chat_ids),
                },
            ));
            true
        } else if op == "gcfnc" {
            self.send_request(Function::getChatFolderNewChats(
                td_api::getChatFolderNewChats {
                    chat_folder_id: as_chat_folder_id(args),
                },
            ));
            true
        } else if op == "pcfnc" {
            let (chat_folder_id, chat_ids) = tokenizer::split(args, ' ');
            self.send_request(Function::processChatFolderNewChats(
                td_api::processChatFolderNewChats {
                    chat_folder_id: as_chat_folder_id(&chat_folder_id),
                    added_chat_ids: self.as_chat_ids(&chat_ids),
                },
            ));
            true
        } else if op == "grcf" {
            self.send_request(Function::getRecommendedChatFolders(
                td_api::getRecommendedChatFolders,
            ));
            true
        } else if op == "gcfdin" {
            self.execute(Function::getChatFolderDefaultIconName(
                td_api::getChatFolderDefaultIconName {
                    folder: Some(self.as_chat_folder(args, false)),
                },
            ));
            true
        } else if op == "gacls" {
            self.send_request(Function::getArchiveChatListSettings(
                td_api::getArchiveChatListSettings,
            ));
            true
        } else if op == "sacls" {
            let (a, b, c) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            self.send_request(Function::setArchiveChatListSettings(
                td_api::setArchiveChatListSettings {
                    settings: Some(td_api::ArchiveChatListSettings {
                        archive_and_mute_new_chats_from_unknown_users: as_bool(&a),
                        keep_unmuted_chats_archived: as_bool(&b),
                        keep_chats_from_folders_archived: as_bool(&c),
                    }),
                },
            ));
            true
        } else if op == "grdps" {
            self.send_request(Function::getReadDatePrivacySettings(
                td_api::getReadDatePrivacySettings,
            ));
            true
        } else if op == "srdps" {
            self.send_request(Function::setReadDatePrivacySettings(
                td_api::setReadDatePrivacySettings {
                    settings: Some(td_api::ReadDatePrivacySettings {
                        show_read_date: as_bool(args),
                    }),
                },
            ));
            true
        } else if op == "gncps" {
            self.send_request(Function::getNewChatPrivacySettings(
                td_api::getNewChatPrivacySettings,
            ));
            true
        } else if op == "sncps" {
            let (allow, paid) = tokenizer::split(args, ' ');
            self.send_request(Function::setNewChatPrivacySettings(
                td_api::setNewChatPrivacySettings {
                    settings: Some(td_api::NewChatPrivacySettings {
                        allow_new_chats_from_unknown_users: as_bool(&allow),
                        incoming_paid_message_star_count: tokenizer::to_integer_i64(&paid),
                    }),
                },
            ));
            true
        } else if op == "gpmr" {
            self.send_request(Function::getPaidMessageRevenue(
                td_api::getPaidMessageRevenue {
                    user_id: self.as_user_id(args),
                },
            ));
            true
        } else if op == "aumfu" {
            let (user_id, refund_payments) = tokenizer::split(args, ' ');
            self.send_request(Function::allowUnpaidMessagesFromUser(
                td_api::allowUnpaidMessagesFromUser {
                    user_id: self.as_user_id(&user_id),
                    refund_payments: as_bool(&refund_payments),
                },
            ));
            true
        } else if op == "scpmsc" {
            let (chat_id, paid_message_star_count) = tokenizer::split(args, ' ');
            self.send_request(Function::setChatPaidMessageStarCount(
                td_api::setChatPaidMessageStarCount {
                    chat_id: self.as_chat_id(&chat_id),
                    paid_message_star_count: tokenizer::to_integer_i64(&paid_message_star_count),
                },
            ));
            true
        } else if op == "csmtu" || op == "csmtul" {
            self.send_request(Function::canSendMessageToUser(
                td_api::canSendMessageToUser {
                    user_id: self.as_user_id(args),
                    only_local: op == "csmtul",
                },
            ));
            true
        } else if op == "sct" {
            let (chat_id, title) = tokenizer::split(args, ' ');
            self.send_request(Function::setChatTitle(td_api::setChatTitle {
                chat_id: self.as_chat_id(&chat_id),
                title,
            }));
            true
        } else if op == "scp" {
            let (chat_id, tail) = tokenizer::split(args, ' ');
            self.send_request(Function::setChatPhoto(td_api::setChatPhoto {
                chat_id: self.as_chat_id(&chat_id),
                photo: self.parse_input_chat_photo(&tail),
            }));
            true
        } else if op == "scac" {
            let (chat_id, accent_color_id, background_custom_emoji_id) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            self.send_request(Function::setChatAccentColor(td_api::setChatAccentColor {
                chat_id: self.as_chat_id(&chat_id),
                accent_color_id: tokenizer::to_integer_i32(&accent_color_id),
                background_custom_emoji_id: self.as_custom_emoji_id(&background_custom_emoji_id),
            }));
            true
        } else if op == "scpac" {
            let (chat_id, profile_accent_color_id, profile_background_custom_emoji_id) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            self.send_request(Function::setChatProfileAccentColor(
                td_api::setChatProfileAccentColor {
                    chat_id: self.as_chat_id(&chat_id),
                    profile_accent_color_id: tokenizer::to_integer_i32(&profile_accent_color_id),
                    profile_background_custom_emoji_id: self
                        .as_custom_emoji_id(&profile_background_custom_emoji_id),
                },
            ));
            true
        } else if op == "scmadt" {
            let (chat_id, auto_delete_time) = tokenizer::split(args, ' ');
            self.send_request(Function::setChatMessageAutoDeleteTime(
                td_api::setChatMessageAutoDeleteTime {
                    chat_id: self.as_chat_id(&chat_id),
                    message_auto_delete_time: tokenizer::to_integer_i32(&auto_delete_time),
                },
            ));
            true
        } else if op == "sces" {
            let (chat_id, custom_emoji_id, expiration_date) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            self.send_request(Function::setChatEmojiStatus(td_api::setChatEmojiStatus {
                chat_id: self.as_chat_id(&chat_id),
                emoji_status: Some(td_api::EmojiStatus {
                    r#type: Some(td_api::EmojiStatusType::emojiStatusTypeCustomEmoji(
                        td_api::emojiStatusTypeCustomEmoji {
                            custom_emoji_id: self.as_custom_emoji_id(&custom_emoji_id),
                        },
                    )),
                    expiration_date: tokenizer::to_integer_i32(&expiration_date),
                }),
            }));
            true
        } else if op == "scese" {
            self.send_request(Function::setChatEmojiStatus(td_api::setChatEmojiStatus {
                chat_id: self.as_chat_id(args),
                emoji_status: None,
            }));
            true
        } else if op == "scperm" {
            let (chat_id, permissions) = tokenizer::split(args, ' ');
            const EXPECTED_SIZE: usize = 16;
            if permissions.len() == EXPECTED_SIZE {
                let bytes = permissions.as_bytes();
                let perms: [bool; 16] = std::array::from_fn(|i| bytes[i] == b'1');
                self.send_request(Function::setChatPermissions(td_api::setChatPermissions {
                    chat_id: self.as_chat_id(&chat_id),
                    permissions: Some(chat_permissions(perms)),
                }));
            } else {
                tdutils::log_error!("Wrong permissions size, expected {}", EXPECTED_SIZE);
            }
            true
        } else if op == "ggct" {
            let (limit, offset) = tokenizer::split(args, ' ');
            self.send_request(Function::getGiftChatThemes(td_api::getGiftChatThemes {
                offset,
                limit: as_limit(&limit, 10),
            }));
            true
        } else if op == "scth" {
            let (chat_id, name) = tokenizer::split(args, ' ');
            let theme = if name
                .as_bytes()
                .first()
                .is_some_and(|b| b.is_ascii_alphabetic())
            {
                Some(td_api::InputChatTheme::inputChatThemeGift(
                    td_api::inputChatThemeGift { name: name.clone() },
                ))
            } else if !name.is_empty() {
                Some(td_api::InputChatTheme::inputChatThemeEmoji(
                    td_api::inputChatThemeEmoji { name: name.clone() },
                ))
            } else {
                None
            };
            self.send_request(Function::setChatTheme(td_api::setChatTheme {
                chat_id: self.as_chat_id(&chat_id),
                theme,
            }));
            true
        } else if op == "sccd" {
            let (chat_id, client_data) = tokenizer::split(args, ' ');
            self.send_request(Function::setChatClientData(td_api::setChatClientData {
                chat_id: self.as_chat_id(&chat_id),
                client_data,
            }));
            true
        } else if op == "acm" {
            let (chat_id, user_id, forward_limit) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            self.send_request(Function::addChatMember(td_api::addChatMember {
                chat_id: self.as_chat_id(&chat_id),
                user_id: self.as_user_id(&user_id),
                forward_limit: tokenizer::to_integer_i32(&forward_limit),
            }));
            true
        } else if op == "acms" {
            let (chat_id, user_ids) = tokenizer::split(args, ' ');
            self.send_request(Function::addChatMembers(td_api::addChatMembers {
                chat_id: self.as_chat_id(&chat_id),
                user_ids: self.as_user_ids(&user_ids),
            }));
            true
        } else {
            false
        }
    }

    /// cli.cpp:5840 — the invite-link chain plus the text/JSON/price probes.
    fn op_block_web_pages(&self, op: &str, args: &str) -> bool {
        if op == "rpcil" {
            self.send_request(Function::replacePrimaryChatInviteLink(
                td_api::replacePrimaryChatInviteLink {
                    chat_id: self.as_chat_id(args),
                },
            ));
            true
        } else if op == "ccilt" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (name, rest1) = tokenizer::split(&rest0, ' ');
            let (expiration_date, rest2) = tokenizer::split(&rest1, ' ');
            let (member_limit, creates_join_request) = tokenizer::split(&rest2, ' ');
            self.send_request(Function::createChatInviteLink(
                td_api::createChatInviteLink {
                    chat_id: self.as_chat_id(&chat_id),
                    name,
                    expiration_date: tokenizer::to_integer_i32(&expiration_date),
                    member_limit: tokenizer::to_integer_i32(&member_limit),
                    creates_join_request: as_bool(&creates_join_request),
                },
            ));
            true
        } else if op == "ccsil" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (name, rest1) = tokenizer::split(&rest0, ' ');
            let (period, star_count) = tokenizer::split(&rest1, ' ');
            self.send_request(Function::createChatSubscriptionInviteLink(
                td_api::createChatSubscriptionInviteLink {
                    chat_id: self.as_chat_id(&chat_id),
                    name,
                    subscription_pricing: Some(td_api::StarSubscriptionPricing {
                        period: tokenizer::to_integer_i32(&period),
                        star_count: tokenizer::to_integer_i64(&star_count),
                    }),
                },
            ));
            true
        } else if op == "ecil" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (invite_link, rest1) = tokenizer::split(&rest0, ' ');
            let (name, rest2) = tokenizer::split(&rest1, ' ');
            let (expiration_date, rest3) = tokenizer::split(&rest2, ' ');
            let (member_limit, rest4) = tokenizer::split(&rest3, ' ');
            let creates_join_request = rest4;
            self.send_request(Function::editChatInviteLink(td_api::editChatInviteLink {
                chat_id: self.as_chat_id(&chat_id),
                invite_link,
                name,
                expiration_date: tokenizer::to_integer_i32(&expiration_date),
                member_limit: tokenizer::to_integer_i32(&member_limit),
                creates_join_request: as_bool(&creates_join_request),
            }));
            true
        } else if op == "ecsil" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (invite_link, name) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::editChatSubscriptionInviteLink(
                td_api::editChatSubscriptionInviteLink {
                    chat_id: self.as_chat_id(&chat_id),
                    invite_link,
                    name,
                },
            ));
            true
        } else if op == "rcil" {
            let (chat_id, invite_link) = tokenizer::split(args, ' ');
            self.send_request(Function::revokeChatInviteLink(
                td_api::revokeChatInviteLink {
                    chat_id: self.as_chat_id(&chat_id),
                    invite_link,
                },
            ));
            true
        } else if op == "gcilc" {
            self.send_request(Function::getChatInviteLinkCounts(
                td_api::getChatInviteLinkCounts {
                    chat_id: self.as_chat_id(args),
                },
            ));
            true
        } else if op == "gcil" {
            let (chat_id, invite_link) = tokenizer::split(args, ' ');
            self.send_request(Function::getChatInviteLink(td_api::getChatInviteLink {
                chat_id: self.as_chat_id(&chat_id),
                invite_link,
            }));
            true
        } else if op == "gcils" || op == "gcilr" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (creator_user_id, rest1) = tokenizer::split(&rest0, ' ');
            let (offset_date, rest2) = tokenizer::split(&rest1, ' ');
            let (offset_invite_link, limit) = tokenizer::split(&rest2, ' ');
            self.send_request(Function::getChatInviteLinks(td_api::getChatInviteLinks {
                chat_id: self.as_chat_id(&chat_id),
                creator_user_id: self.as_user_id(&creator_user_id),
                is_revoked: op == "gcilr",
                offset_date: tokenizer::to_integer_i32(&offset_date),
                offset_invite_link,
                limit: as_limit(&limit, 10),
            }));
            true
        } else if op == "gcilm" || op == "gcilme" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (invite_link, rest1) = tokenizer::split(&rest0, ' ');
            let (offset_user_id, rest2) = tokenizer::split(&rest1, ' ');
            let (offset_date, limit) = tokenizer::split(&rest2, ' ');
            self.send_request(Function::getChatInviteLinkMembers(
                td_api::getChatInviteLinkMembers {
                    chat_id: self.as_chat_id(&chat_id),
                    invite_link,
                    only_with_expired_subscription: op == "gcilme",
                    offset_member: Some(td_api::ChatInviteLinkMember {
                        user_id: self.as_user_id(&offset_user_id),
                        joined_chat_date: tokenizer::to_integer_i32(&offset_date),
                        via_chat_folder_invite_link: false,
                        approver_user_id: 0,
                    }),
                    limit: as_limit(&limit, 10),
                },
            ));
            true
        } else if op == "gcjr" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (invite_link, rest1) = tokenizer::split(&rest0, ' ');
            let (query, rest2) = tokenizer::split(&rest1, ' ');
            let (offset_user_id, rest3) = tokenizer::split(&rest2, ' ');
            let (offset_date, limit) = tokenizer::split(&rest3, ' ');
            self.send_request(Function::getChatJoinRequests(td_api::getChatJoinRequests {
                chat_id: self.as_chat_id(&chat_id),
                invite_link,
                query,
                offset_request: Some(td_api::ChatJoinRequest {
                    user_id: self.as_user_id(&offset_user_id),
                    date: tokenizer::to_integer_i32(&offset_date),
                    bio: String::new(),
                }),
                limit: as_limit(&limit, 10),
            }));
            true
        } else if op == "pcjr" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (user_id, approve) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::processChatJoinRequest(
                td_api::processChatJoinRequest {
                    chat_id: self.as_chat_id(&chat_id),
                    user_id: self.as_user_id(&user_id),
                    approve: as_bool(&approve),
                },
            ));
            true
        } else if op == "pcjrs" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (invite_link, approve) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::processChatJoinRequests(
                td_api::processChatJoinRequests {
                    chat_id: self.as_chat_id(&chat_id),
                    invite_link,
                    approve: as_bool(&approve),
                },
            ));
            true
        } else if op == "drcil" {
            let (chat_id, invite_link) = tokenizer::split(args, ' ');
            self.send_request(Function::deleteRevokedChatInviteLink(
                td_api::deleteRevokedChatInviteLink {
                    chat_id: self.as_chat_id(&chat_id),
                    invite_link,
                },
            ));
            true
        } else if op == "darcil" {
            let (chat_id, creator_user_id) = tokenizer::split(args, ' ');
            self.send_request(Function::deleteAllRevokedChatInviteLinks(
                td_api::deleteAllRevokedChatInviteLinks {
                    chat_id: self.as_chat_id(&chat_id),
                    creator_user_id: self.as_user_id(&creator_user_id),
                },
            ));
            true
        } else if op == "ccil" {
            self.send_request(Function::checkChatInviteLink(td_api::checkChatInviteLink {
                invite_link: args.to_owned(),
            }));
            true
        } else if op == "jcbil" {
            self.send_request(Function::joinChatByInviteLink(
                td_api::joinChatByInviteLink {
                    invite_link: args.to_owned(),
                },
            ));
            true
        } else if op == "asp" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (message_id, send_date) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::approveSuggestedPost(
                td_api::approveSuggestedPost {
                    chat_id: self.as_chat_id(&chat_id),
                    message_id: as_message_id(&message_id),
                    send_date: tokenizer::to_integer_i32(&send_date),
                },
            ));
            true
        } else if op == "dsp" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (message_id, comment) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::declineSuggestedPost(
                td_api::declineSuggestedPost {
                    chat_id: self.as_chat_id(&chat_id),
                    message_id: as_message_id(&message_id),
                    comment,
                },
            ));
            true
        } else if op == "ao" {
            let (chat_id, message_id) = tokenizer::split(args, ' ');
            self.send_request(Function::addOffer(td_api::addOffer {
                chat_id: self.as_chat_id(&chat_id),
                message_id: as_message_id(&message_id),
                options: Some(self.default_message_send_options()),
            }));
            true
        } else if op == "sq" {
            let (text, rest0) = tokenizer::split(args, ' ');
            let (quote, quote_position) = tokenizer::split(&rest0, ' ');
            self.execute(Function::searchQuote(td_api::searchQuote {
                text: Some(self.as_formatted_text(&text)),
                quote: Some(self.as_formatted_text(&quote)),
                quote_position: tokenizer::to_integer_i32(&quote_position),
            }));
            true
        } else if op == "gte" {
            self.send_request(Function::getTextEntities(td_api::getTextEntities {
                text: args.to_owned(),
            }));
            true
        } else if op == "gtee" {
            self.execute(Function::getTextEntities(td_api::getTextEntities {
                text: args.to_owned(),
            }));
            true
        } else if op == "pm" {
            self.send_request(Function::parseMarkdown(td_api::parseMarkdown {
                text: Some(td_api::FormattedText {
                    text: args.to_owned(),
                    entities: Vec::new(),
                }),
            }));
            true
        } else if op == "pte" || op == "ptes" {
            let request = Function::parseTextEntities(td_api::parseTextEntities {
                text: args.to_owned(),
                parse_mode: Some(td_api::TextParseMode::textParseModeMarkdown(
                    td_api::textParseModeMarkdown { version: 2 },
                )),
            });
            if op == "pte" {
                self.send_request(request);
            } else {
                self.execute(request);
            }
            true
        } else if op == "pteh" || op == "ptehs" {
            let request = Function::parseTextEntities(td_api::parseTextEntities {
                text: args.to_owned(),
                parse_mode: Some(td_api::TextParseMode::textParseModeHTML(
                    td_api::textParseModeHTML,
                )),
            });
            if op == "pteh" {
                self.send_request(request);
            } else {
                self.execute(request);
            }
            true
        } else if op == "ssbp" {
            let (strings, rest0) = tokenizer::split(args, ' ');
            let (query, rest1) = tokenizer::split(&rest0, ' ');
            let (limit, return_none_for_empty_query) = tokenizer::split(&rest1, ' ');
            self.execute(Function::searchStringsByPrefix(
                td_api::searchStringsByPrefix {
                    strings: autosplit(&strings),
                    query,
                    limit: as_limit(&limit, 10),
                    return_none_for_empty_query: as_bool(&return_none_for_empty_query),
                },
            ));
            true
        } else if op == "gcfe" {
            self.execute(Function::getCountryFlagEmoji(td_api::getCountryFlagEmoji {
                country_code: trim_td(args).to_owned(),
            }));
            true
        } else if op == "gfmt" {
            self.execute(Function::getFileMimeType(td_api::getFileMimeType {
                file_name: trim_td(args).to_owned(),
            }));
            true
        } else if op == "gfe" {
            self.execute(Function::getFileExtension(td_api::getFileExtension {
                mime_type: trim_td(args).to_owned(),
            }));
            true
        } else if op == "cfn" {
            self.execute(Function::cleanFileName(td_api::cleanFileName {
                file_name: args.to_owned(),
            }));
            true
        } else if op == "gjv" {
            self.execute(Function::getJsonValue(td_api::getJsonValue {
                json: args.to_owned(),
            }));
            true
        } else if op == "gjvtest" {
            // The C++ literals carry a raw 0x80 byte (`\200`); the Rust
            // String model holds U+0080 instead (the lossy-encoding decision).
            for json in ["\"aba\u{80}caba\"", "\"\\u0080\"", "\"\\uD800\""] {
                self.execute(Function::getJsonValue(td_api::getJsonValue {
                    json: json.to_owned(),
                }));
            }
            true
        } else if op == "gjs" {
            let test_get_json_string = |client: &Self, json_value: Option<td_api::JsonValue>| {
                client.execute(Function::getJsonString(td_api::getJsonString {
                    json_value: json_value.map(Box::new),
                }));
            };
            test_get_json_string(self, None);
            test_get_json_string(
                self,
                Some(td_api::JsonValue::jsonValueNull(td_api::jsonValueNull)),
            );
            test_get_json_string(
                self,
                Some(td_api::JsonValue::jsonValueBoolean(
                    td_api::jsonValueBoolean { value: true },
                )),
            );
            test_get_json_string(
                self,
                Some(td_api::JsonValue::jsonValueNumber(
                    td_api::jsonValueNumber {
                        value: 123456789123.0,
                    },
                )),
            );
            test_get_json_string(
                self,
                Some(td_api::JsonValue::jsonValueString(
                    td_api::jsonValueString {
                        value: "aba\0caba".to_owned(),
                    },
                )),
            );
            // C++ `string("aba\200caba")` — the raw 0x80 byte again.
            test_get_json_string(
                self,
                Some(td_api::JsonValue::jsonValueString(
                    td_api::jsonValueString {
                        value: "aba\u{80}caba".to_owned(),
                    },
                )),
            );
            let inner_array = td_api::JsonValue::jsonValueArray(td_api::jsonValueArray {
                values: vec![Box::new(td_api::JsonValue::jsonValueBoolean(
                    td_api::jsonValueBoolean { value: false },
                ))],
            });
            let array = td_api::JsonValue::jsonValueArray(td_api::jsonValueArray {
                values: vec![
                    // C++ emplaces a null element first; the vector model drops it.
                    Box::new(inner_array),
                    Box::new(td_api::JsonValue::jsonValueNull(td_api::jsonValueNull)),
                    Box::new(td_api::JsonValue::jsonValueNumber(
                        td_api::jsonValueNumber { value: -1.0 },
                    )),
                ],
            });
            test_get_json_string(self, Some(array));
            // C++ emplaces a null member; the vector model drops it.
            let members = vec![
                Box::new(td_api::JsonObjectMember {
                    key: String::new(),
                    value: Some(Box::new(td_api::JsonValue::jsonValueString(
                        td_api::jsonValueString {
                            value: "test".to_owned(),
                        },
                    ))),
                }),
                Box::new(td_api::JsonObjectMember {
                    key: "a".to_owned(),
                    value: None,
                }),
                Box::new(td_api::JsonObjectMember {
                    key: "\u{80}".to_owned(),
                    value: None,
                }),
                Box::new(td_api::JsonObjectMember {
                    key: "a".to_owned(),
                    value: Some(Box::new(td_api::JsonValue::jsonValueNull(
                        td_api::jsonValueNull,
                    ))),
                }),
            ];
            let object = td_api::JsonValue::jsonValueObject(td_api::jsonValueObject { members });
            test_get_json_string(self, Some(object));
            true
        } else if op == "gtpjs" {
            self.execute(Function::getThemeParametersJsonString(
                td_api::getThemeParametersJsonString {
                    theme: Some(as_theme_parameters()),
                },
            ));
            true
        } else if op == "gac" {
            self.send_request(Function::getApplicationConfig(td_api::getApplicationConfig));
            true
        } else if op == "sale" {
            let (r#type, rest0) = tokenizer::split(args, ' ');
            let (chat_id, json) = tokenizer::split(&rest0, ' ');
            let result = self.execute(Function::getJsonValue(td_api::getJsonValue { json }));
            let json_value = match result {
                Object::error(_) => {
                    // C++ logs `to_string(result)` (JSON); the Rust port logs
                    // the debug-string rendering (no serde_json dep).
                    tdutils::log_error!("{}", result.to_debug_string());
                    None
                }
                Object::jsonValueNull(_) => {
                    Some(td_api::JsonValue::jsonValueNull(td_api::jsonValueNull))
                }
                Object::jsonValueBoolean(x) => Some(td_api::JsonValue::jsonValueBoolean(x)),
                Object::jsonValueNumber(x) => Some(td_api::JsonValue::jsonValueNumber(x)),
                Object::jsonValueString(x) => Some(td_api::JsonValue::jsonValueString(x)),
                Object::jsonValueArray(x) => Some(td_api::JsonValue::jsonValueArray(x)),
                Object::jsonValueObject(x) => Some(td_api::JsonValue::jsonValueObject(x)),
                _ => None,
            };
            if let Some(json_value) = json_value {
                self.send_request(Function::saveApplicationLogEvent(
                    td_api::saveApplicationLogEvent {
                        r#type,
                        chat_id: self.as_chat_id(&chat_id),
                        data: Some(Box::new(json_value)),
                    },
                ));
            }
            true
        } else {
            false
        }
    }

    /// cli.cpp:6061 — the misc chain: drafts setters, chat toggles, the
    /// story surface, and the chat-boost probes.
    fn op_block_misc(&self, op: &str, args: &str) -> bool {
        if op == "scdm" || op == "scdmrm" {
            let (chat_id, message) = tokenizer::split(args, ' ');
            let reply_to = self.get_input_message_reply_to();
            let content = if reply_to.is_some() || !message.is_empty() {
                if op == "scdm" {
                    let mut entities = Vec::new();
                    if !message.is_empty() {
                        entities.push(td_api::TextEntity {
                            offset: 0,
                            length: 1,
                            r#type: Some(td_api::TextEntityType::textEntityTypePre(
                                td_api::textEntityTypePre,
                            )),
                        });
                    }
                    Some(td_api::DraftMessageContent::draftMessageContentText(
                        td_api::draftMessageContentText {
                            text: Some(self.as_formatted_text_with_entities(&message, entities)),
                            link_preview_options: self.get_link_preview_options(),
                        },
                    ))
                } else {
                    Some(
                        td_api::DraftMessageContent::draftMessageContentInputRichMessage(
                            td_api::draftMessageContentInputRichMessage {
                                message: Some(Box::new(self.as_input_rich_message(&message))),
                            },
                        ),
                    )
                }
            } else {
                None
            };
            self.set_draft_message(self.as_chat_id(&chat_id), content);
            true
        } else if op == "scdmvn" {
            let (chat_id, video) = tokenizer::split(args, ' ');
            self.set_draft_message(
                self.as_chat_id(&chat_id),
                Some(td_api::DraftMessageContent::draftMessageContentVideoNote(
                    td_api::draftMessageContentVideoNote {
                        file_path: video,
                        duration: 10,
                        length: 5,
                        self_destruct_type: self.get_message_self_destruct_type(),
                    },
                )),
            );
            true
        } else if op == "scdmvoice" {
            let (chat_id, voice) = tokenizer::split(args, ' ');
            self.set_draft_message(
                self.as_chat_id(&chat_id),
                Some(td_api::DraftMessageContent::draftMessageContentVoiceNote(
                    td_api::draftMessageContentVoiceNote {
                        file_path: voice,
                        duration: 0,
                        waveform: b"abacaba".to_vec(),
                        self_destruct_type: self.get_message_self_destruct_type(),
                    },
                )),
            );
            true
        } else if op == "cadm" {
            self.send_request(Function::clearAllDraftMessages(
                td_api::clearAllDraftMessages {
                    exclude_secret_chats: false,
                },
            ));
            true
        } else if op == "gsds" {
            self.send_request(Function::getStakeDiceState(td_api::getStakeDiceState));
            true
        } else if op == "tchpc" {
            let (chat_id, has_protected_content) = tokenizer::split(args, ' ');
            self.send_request(Function::toggleChatHasProtectedContent(
                td_api::toggleChatHasProtectedContent {
                    chat_id: self.as_chat_id(&chat_id),
                    has_protected_content: as_bool(&has_protected_content),
                },
            ));
            true
        } else if op == "pchpcdr" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (message_id, approve) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::processChatHasProtectedContentDisableRequest(
                td_api::processChatHasProtectedContentDisableRequest {
                    chat_id: self.as_chat_id(&chat_id),
                    request_message_id: as_message_id(&message_id),
                    approve: as_bool(&approve),
                },
            ));
            true
        } else if op == "tcip" || op == "tcipa" || op.starts_with("tcip-") {
            let (chat_id, is_pinned) = tokenizer::split(args, ' ');
            self.send_request(Function::toggleChatIsPinned(td_api::toggleChatIsPinned {
                chat_list: as_chat_list(op),
                chat_id: self.as_chat_id(&chat_id),
                is_pinned: as_bool(&is_pinned),
            }));
            true
        } else if op == "tcimau" {
            let (chat_id, is_marked_as_unread) = tokenizer::split(args, ' ');
            self.send_request(Function::toggleChatIsMarkedAsUnread(
                td_api::toggleChatIsMarkedAsUnread {
                    chat_id: self.as_chat_id(&chat_id),
                    is_marked_as_unread: as_bool(&is_marked_as_unread),
                },
            ));
            true
        } else if op == "tcvat" {
            let (chat_id, view_as_topics) = tokenizer::split(args, ' ');
            self.send_request(Function::toggleChatViewAsTopics(
                td_api::toggleChatViewAsTopics {
                    chat_id: self.as_chat_id(&chat_id),
                    view_as_topics: as_bool(&view_as_topics),
                },
            ));
            true
        } else if op == "tcit" {
            let (chat_id, is_translatable) = tokenizer::split(args, ' ');
            self.send_request(Function::toggleChatIsTranslatable(
                td_api::toggleChatIsTranslatable {
                    chat_id: self.as_chat_id(&chat_id),
                    is_translatable: as_bool(&is_translatable),
                },
            ));
            true
        } else if op == "smsbl" {
            let (sender_id, block_list) = tokenizer::split(args, ' ');
            self.send_request(Function::setMessageSenderBlockList(
                td_api::setMessageSenderBlockList {
                    sender_id: self.as_message_sender(&sender_id),
                    block_list: as_block_list(&block_list),
                },
            ));
            true
        } else if op == "bmsfr" {
            let (message_id, rest0) = tokenizer::split(args, ' ');
            let (delete_message, rest1) = tokenizer::split(&rest0, ' ');
            let (delete_all_messages, report_spam) = tokenizer::split(&rest1, ' ');
            self.send_request(Function::blockMessageSenderFromReplies(
                td_api::blockMessageSenderFromReplies {
                    message_id: as_message_id(&message_id),
                    delete_message: as_bool(&delete_message),
                    delete_all_messages: as_bool(&delete_all_messages),
                    report_spam: as_bool(&report_spam),
                },
            ));
            true
        } else if op == "tcddn" {
            let (chat_id, default_disable_notification) = tokenizer::split(args, ' ');
            self.send_request(Function::toggleChatDefaultDisableNotification(
                td_api::toggleChatDefaultDisableNotification {
                    chat_id: self.as_chat_id(&chat_id),
                    default_disable_notification: as_bool(&default_disable_notification),
                },
            ));
            true
        } else if op == "spchats" || op == "spchatsa" || op.starts_with("spchats-") {
            self.send_request(Function::setPinnedChats(td_api::setPinnedChats {
                chat_list: as_chat_list(op),
                chat_ids: self.as_chat_ids(args),
            }));
            true
        } else if op == "rcl" || op == "rcla" || op.starts_with("rcl-") {
            self.send_request(Function::readChatList(td_api::readChatList {
                chat_list: as_chat_list(op),
            }));
            true
        } else if op == "gcwe" {
            let (latitude, longitude) = tokenizer::split(args, ' ');
            self.send_request(Function::getCurrentWeather(td_api::getCurrentWeather {
                location: as_location(&latitude, &longitude, "0.0"),
            }));
            true
        } else if op == "gst" || op == "gstl" {
            let (story_poster_chat_id, story_id) = tokenizer::split(args, ' ');
            self.send_request(Function::getStory(td_api::getStory {
                story_poster_chat_id: self.as_chat_id(&story_poster_chat_id),
                story_id: as_story_id(&story_id),
                only_local: op == "gstl",
            }));
            true
        } else if op == "gctps" {
            self.send_request(Function::getChatsToPostStories(
                td_api::getChatsToPostStories,
            ));
            true
        } else if op == "cps" {
            self.send_request(Function::canPostStory(td_api::canPostStory {
                chat_id: self.as_chat_id(args),
            }));
            true
        } else if op == "srsfi" {
            let (reposted_story_chat_id, reposted_story_id) = tokenizer::split(args, ' ');
            self.reposted_story_chat_id
                .store(self.as_chat_id(&reposted_story_chat_id), Ordering::SeqCst);
            self.reposted_story_id
                .store(as_story_id(&reposted_story_id), Ordering::SeqCst);
            true
        } else if op == "ssmu" {
            // cpp:6324 — the story-music state for postStory's 1.8.70
            // music leg.
            *self.story_music.lock().unwrap() = args.to_owned();
            true
        } else if op == "psp" || op == "pspp" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (photo, rest1) = tokenizer::split(&rest0, ' ');
            let (rules, rest2) = tokenizer::split(&rest1, ' ');
            let (areas, rest3) = tokenizer::split(&rest2, ' ');
            let (active_period, rest4) = tokenizer::split(&rest3, ' ');
            let (protect_content, album_ids) = tokenizer::split(&rest4, ' ');
            self.send_request(Function::postStory(td_api::postStory {
                chat_id: self.as_chat_id(&chat_id),
                content: Some(td_api::InputStoryContent::inputStoryContentPhoto(
                    td_api::inputStoryContentPhoto {
                        photo: as_input_file(&photo),
                        added_sticker_file_ids: self.get_added_sticker_file_ids(),
                    },
                )),
                areas: get_input_story_areas(&areas),
                caption: self.get_caption(),
                music: self.get_story_music(),
                privacy_settings: Some(self.get_story_privacy_settings(&rules)),
                album_ids: to_integers_i32(&album_ids),
                active_period: if tokenizer::to_integer_i32(&active_period) != 0 {
                    tokenizer::to_integer_i32(&active_period)
                } else {
                    86400
                },
                from_story_full_id: self.get_reposted_story_full_id(),
                is_posted_to_chat_page: op == "pspp",
                protect_content: as_bool(&protect_content),
            }));
            true
        } else if op == "psv" || op == "psvp" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (video, rest1) = tokenizer::split(&rest0, ' ');
            let (rules, rest2) = tokenizer::split(&rest1, ' ');
            let (areas, rest3) = tokenizer::split(&rest2, ' ');
            let (active_period, rest4) = tokenizer::split(&rest3, ' ');
            let (duration, rest5) = tokenizer::split(&rest4, ' ');
            let (protect_content, album_ids) = tokenizer::split(&rest5, ' ');
            self.send_request(Function::postStory(td_api::postStory {
                chat_id: self.as_chat_id(&chat_id),
                content: Some(td_api::InputStoryContent::inputStoryContentVideo(
                    td_api::inputStoryContentVideo {
                        video: as_input_file(&video),
                        added_sticker_file_ids: self.get_added_sticker_file_ids(),
                        duration: tdutils::json::to_double(&duration),
                        cover_frame_timestamp: 0.5,
                        is_animation: true,
                    },
                )),
                areas: get_input_story_areas(&areas),
                caption: self.get_caption(),
                music: self.get_story_music(),
                privacy_settings: Some(self.get_story_privacy_settings(&rules)),
                album_ids: to_integers_i32(&album_ids),
                active_period: if tokenizer::to_integer_i32(&active_period) != 0 {
                    tokenizer::to_integer_i32(&active_period)
                } else {
                    86400
                },
                from_story_full_id: self.get_reposted_story_full_id(),
                is_posted_to_chat_page: op == "psvp",
                protect_content: as_bool(&protect_content),
            }));
            true
        } else if op == "sls" || op == "slsr" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (rules, rest1) = tokenizer::split(&rest0, ' ');
            let (protect_content, _album_ids) = tokenizer::split(&rest1, ' ');
            self.send_request(Function::startLiveStory(td_api::startLiveStory {
                chat_id: self.as_chat_id(&chat_id),
                privacy_settings: Some(self.get_story_privacy_settings(&rules)),
                protect_content: as_bool(&protect_content),
                is_rtmp_stream: op == "slsr",
                enable_messages: true,
                paid_message_star_count: 1,
            }));
            true
        } else if op == "esc" {
            let (story_poster_chat_id, rest0) = tokenizer::split(args, ' ');
            let (story_id, areas) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::editStory(td_api::editStory {
                story_poster_chat_id: self.as_chat_id(&story_poster_chat_id),
                story_id: as_story_id(&story_id),
                content: None,
                areas: get_input_story_areas(&areas),
                caption: self.get_caption(),
            }));
            true
        } else if op == "esp" {
            let (story_poster_chat_id, rest0) = tokenizer::split(args, ' ');
            let (story_id, rest1) = tokenizer::split(&rest0, ' ');
            let (photo, areas) = tokenizer::split(&rest1, ' ');
            self.send_request(Function::editStory(td_api::editStory {
                story_poster_chat_id: self.as_chat_id(&story_poster_chat_id),
                story_id: as_story_id(&story_id),
                content: Some(td_api::InputStoryContent::inputStoryContentPhoto(
                    td_api::inputStoryContentPhoto {
                        photo: as_input_file(&photo),
                        added_sticker_file_ids: self.get_added_sticker_file_ids(),
                    },
                )),
                areas: get_input_story_areas(&areas),
                caption: self.get_caption(),
            }));
            true
        } else if op == "esv" {
            let (story_poster_chat_id, rest0) = tokenizer::split(args, ' ');
            let (story_id, rest1) = tokenizer::split(&rest0, ' ');
            let (video, duration) = tokenizer::split(&rest1, ' ');
            self.send_request(Function::editStory(td_api::editStory {
                story_poster_chat_id: self.as_chat_id(&story_poster_chat_id),
                story_id: as_story_id(&story_id),
                content: Some(td_api::InputStoryContent::inputStoryContentVideo(
                    td_api::inputStoryContentVideo {
                        video: as_input_file(&video),
                        added_sticker_file_ids: self.get_added_sticker_file_ids(),
                        duration: tokenizer::to_integer_i32(&duration).into(),
                        cover_frame_timestamp: 0.0,
                        is_animation: false,
                    },
                )),
                areas: None,
                caption: self.get_caption(),
            }));
            true
        } else if op == "esco" {
            let (story_poster_chat_id, rest0) = tokenizer::split(args, ' ');
            let (story_id, cover_frame_timetamp) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::editStoryCover(td_api::editStoryCover {
                story_poster_chat_id: self.as_chat_id(&story_poster_chat_id),
                story_id: as_story_id(&story_id),
                cover_frame_timestamp: tdutils::json::to_double(&cover_frame_timetamp),
            }));
            true
        } else if op == "ssps" {
            let (story_id, rules) = tokenizer::split(args, ' ');
            self.send_request(Function::setStoryPrivacySettings(
                td_api::setStoryPrivacySettings {
                    story_id: as_story_id(&story_id),
                    privacy_settings: Some(self.get_story_privacy_settings(&rules)),
                },
            ));
            true
        } else if op == "tsiptcp" {
            let (story_poster_chat_id, rest0) = tokenizer::split(args, ' ');
            let (story_id, is_posted_to_chat_page) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::toggleStoryIsPostedToChatPage(
                td_api::toggleStoryIsPostedToChatPage {
                    story_poster_chat_id: self.as_chat_id(&story_poster_chat_id),
                    story_id: as_story_id(&story_id),
                    is_posted_to_chat_page: as_bool(&is_posted_to_chat_page),
                },
            ));
            true
        } else if op == "ds" {
            let (story_poster_chat_id, story_id) = tokenizer::split(args, ' ');
            self.send_request(Function::deleteStory(td_api::deleteStory {
                story_poster_chat_id: self.as_chat_id(&story_poster_chat_id),
                story_id: as_story_id(&story_id),
            }));
            true
        } else if op == "las" || op == "lasa" || op == "lase" {
            self.send_request(Function::loadActiveStories(td_api::loadActiveStories {
                story_list: as_story_list(op),
            }));
            true
        } else if op == "scasl" || op == "scasla" || op == "scasle" {
            self.send_request(Function::setChatActiveStoriesList(
                td_api::setChatActiveStoriesList {
                    chat_id: self.as_chat_id(args),
                    story_list: as_story_list(op),
                },
            ));
            true
        } else if op == "gcptcps" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (from_story_id, limit) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::getChatPostedToChatPageStories(
                td_api::getChatPostedToChatPageStories {
                    chat_id: self.as_chat_id(&chat_id),
                    from_story_id: as_story_id(&from_story_id),
                    limit: as_limit(&limit, 10),
                },
            ));
            true
        } else if op == "gcast" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (from_story_id, limit) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::getChatArchivedStories(
                td_api::getChatArchivedStories {
                    chat_id: self.as_chat_id(&chat_id),
                    from_story_id: as_story_id(&from_story_id),
                    limit: as_limit(&limit, 10),
                },
            ));
            true
        } else if op == "scps" {
            let (chat_id, mut rest) = tokenizer::split(args, ' ');
            let mut story_ids = Vec::new();
            loop {
                let (word, remainder) = tokenizer::split(&rest, ' ');
                let story_id = as_story_id(&word);
                if story_id <= 0 {
                    break;
                }
                story_ids.push(story_id);
                rest = remainder;
            }
            self.send_request(Function::setChatPinnedStories(
                td_api::setChatPinnedStories {
                    chat_id: self.as_chat_id(&chat_id),
                    story_ids,
                },
            ));
            true
        } else if op == "gsnse" {
            self.send_request(Function::getStoryNotificationSettingsExceptions(
                td_api::getStoryNotificationSettingsExceptions,
            ));
            true
        } else if op == "gcas" {
            self.send_request(Function::getChatActiveStories(
                td_api::getChatActiveStories {
                    chat_id: self.as_chat_id(args),
                },
            ));
            true
        } else if op == "os" {
            let (story_poster_chat_id, story_id) = tokenizer::split(args, ' ');
            self.send_request(Function::openStory(td_api::openStory {
                story_poster_chat_id: self.as_chat_id(&story_poster_chat_id),
                story_id: as_story_id(&story_id),
            }));
            true
        } else if op == "cs" {
            let (story_poster_chat_id, story_id) = tokenizer::split(args, ' ');
            self.send_request(Function::closeStory(td_api::closeStory {
                story_poster_chat_id: self.as_chat_id(&story_poster_chat_id),
                story_id: as_story_id(&story_id),
            }));
            true
        } else if op == "gsar" {
            self.send_request(Function::getStoryAvailableReactions(
                td_api::getStoryAvailableReactions {
                    row_size: tokenizer::to_integer_i32(trim_td(args)),
                },
            ));
            true
        } else if op == "ssr" {
            let (story_poster_chat_id, rest0) = tokenizer::split(args, ' ');
            let (story_id, rest1) = tokenizer::split(&rest0, ' ');
            let (reaction, update_recent_reactions) = tokenizer::split(&rest1, ' ');
            self.send_request(Function::setStoryReaction(td_api::setStoryReaction {
                story_poster_chat_id: self.as_chat_id(&story_poster_chat_id),
                story_id: as_story_id(&story_id),
                reaction_type: as_reaction_type(&reaction),
                update_recent_reactions: as_bool(&update_recent_reactions),
            }));
            true
        } else if op == "gsi" {
            let (story_id, rest0) = tokenizer::split(args, ' ');
            let (limit, rest1) = tokenizer::split(&rest0, ' ');
            let (offset, rest2) = tokenizer::split(&rest1, ' ');
            let (query, rest3) = tokenizer::split(&rest2, ' ');
            let (only_contacts, rest4) = tokenizer::split(&rest3, ' ');
            let (prefer_forwards, prefer_with_reaction) = tokenizer::split(&rest4, ' ');
            self.send_request(Function::getStoryInteractions(
                td_api::getStoryInteractions {
                    story_id: as_story_id(&story_id),
                    query,
                    only_contacts: as_bool(&only_contacts),
                    prefer_forwards: as_bool(&prefer_forwards),
                    prefer_with_reaction: as_bool(&prefer_with_reaction),
                    offset,
                    limit: as_limit(&limit, 10),
                },
            ));
            true
        } else if op == "gcsi" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (story_id, rest1) = tokenizer::split(&rest0, ' ');
            let (limit, rest2) = tokenizer::split(&rest1, ' ');
            let (offset, rest3) = tokenizer::split(&rest2, ' ');
            let (reaction_type, prefer_forwards) = tokenizer::split(&rest3, ' ');
            self.send_request(Function::getChatStoryInteractions(
                td_api::getChatStoryInteractions {
                    story_poster_chat_id: self.as_chat_id(&chat_id),
                    story_id: as_story_id(&story_id),
                    reaction_type: as_reaction_type(&reaction_type),
                    prefer_forwards: as_bool(&prefer_forwards),
                    offset,
                    limit: as_limit(&limit, 10),
                },
            ));
            true
        } else if op == "rst" {
            let (story_poster_chat_id, rest0) = tokenizer::split(args, ' ');
            let (story_id, rest1) = tokenizer::split(&rest0, ' ');
            let (option_id, text) = tokenizer::split(&rest1, ' ');
            self.send_request(Function::reportStory(td_api::reportStory {
                story_poster_chat_id: self.as_chat_id(&story_poster_chat_id),
                story_id: as_story_id(&story_id),
                option_id: option_id.into_bytes(),
                text,
            }));
            true
        } else if op == "assm" {
            self.send_request(Function::activateStoryStealthMode(
                td_api::activateStoryStealthMode,
            ));
            true
        } else if op == "gcblf" {
            let (is_channel, level) = tokenizer::split(args, ' ');
            self.send_request(Function::getChatBoostLevelFeatures(
                td_api::getChatBoostLevelFeatures {
                    is_channel: as_bool(&is_channel),
                    level: tokenizer::to_integer_i32(&level),
                },
            ));
            true
        } else if op == "gcbf" {
            self.send_request(Function::getChatBoostFeatures(
                td_api::getChatBoostFeatures {
                    is_channel: as_bool(args),
                },
            ));
            true
        } else if op == "gacbs" {
            self.send_request(Function::getAvailableChatBoostSlots(
                td_api::getAvailableChatBoostSlots,
            ));
            true
        } else if op == "gcbs" {
            self.send_request(Function::getChatBoostStatus(td_api::getChatBoostStatus {
                chat_id: self.as_chat_id(args),
            }));
            true
        } else if op == "bc" {
            let (chat_id, slot_ids) = tokenizer::split(args, ' ');
            self.send_request(Function::boostChat(td_api::boostChat {
                chat_id: self.as_chat_id(&chat_id),
                slot_ids: to_integers_i32(&slot_ids),
            }));
            true
        } else if op == "gcbl" {
            self.send_request(Function::getChatBoostLink(td_api::getChatBoostLink {
                chat_id: self.as_chat_id(args),
            }));
            true
        } else if op == "gcbli" {
            self.send_request(Function::getChatBoostLinkInfo(
                td_api::getChatBoostLinkInfo {
                    url: args.to_owned(),
                },
            ));
            true
        } else if op == "gcb" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (only_gift_codes, rest1) = tokenizer::split(&rest0, ' ');
            let (offset, limit) = tokenizer::split(&rest1, ' ');
            self.send_request(Function::getChatBoosts(td_api::getChatBoosts {
                chat_id: self.as_chat_id(&chat_id),
                only_gift_codes: as_bool(&only_gift_codes),
                offset,
                limit: as_limit(&limit, 10),
            }));
            true
        } else if op == "gucb" {
            let (chat_id, user_id) = tokenizer::split(args, ' ');
            self.send_request(Function::getUserChatBoosts(td_api::getUserChatBoosts {
                chat_id: self.as_chat_id(&chat_id),
                user_id: self.as_user_id(&user_id),
            }));
            true
        } else {
            false
        }
    }

    /// cli.cpp:6392 — the web-app/chat-action/state-setter/message-edit
    /// chain.
    fn op_block_network(&self, op: &str, args: &str) -> bool {
        if op == "gamb" {
            self.send_request(Function::getAttachmentMenuBot(
                td_api::getAttachmentMenuBot {
                    bot_user_id: self.as_user_id(args),
                },
            ));
            true
        } else if op == "tbiatam" {
            let (user_id, rest0) = tokenizer::split(args, ' ');
            let (is_added, allow_write_access) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::toggleBotIsAddedToAttachmentMenu(
                td_api::toggleBotIsAddedToAttachmentMenu {
                    bot_user_id: self.as_user_id(&user_id),
                    is_added: as_bool(&is_added),
                    allow_write_access: as_bool(&allow_write_access),
                },
            ));
            true
        } else if op == "ggwab" {
            let (offset, limit) = tokenizer::split(args, ' ');
            self.send_request(Function::getGrossingWebAppBots(
                td_api::getGrossingWebAppBots {
                    offset,
                    limit: as_limit(&limit, 10),
                },
            ));
            true
        } else if op == "swa" {
            let (bot_user_id, short_name) = tokenizer::split(args, ' ');
            self.send_request(Function::searchWebApp(td_api::searchWebApp {
                bot_user_id: self.as_user_id(&bot_user_id),
                web_app_short_name: short_name,
            }));
            true
        } else if op == "gwap" {
            self.send_request(Function::getWebAppPlaceholder(
                td_api::getWebAppPlaceholder {
                    bot_user_id: self.as_user_id(args),
                },
            ));
            true
        } else if op == "gwalu" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (bot_user_id, rest1) = tokenizer::split(&rest0, ' ');
            let (short_name, start_parameter) = tokenizer::split(&rest1, ' ');
            self.send_request(Function::getWebAppLinkUrl(td_api::getWebAppLinkUrl {
                chat_id: self.as_chat_id(&chat_id),
                bot_user_id: self.as_user_id(&bot_user_id),
                web_app_short_name: short_name,
                start_parameter,
                allow_write_access: true,
                parameters: Some(self.as_web_app_open_parameters()),
            }));
            true
        } else if op == "gmwa" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (bot_user_id, start_parameter) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::getMainWebApp(td_api::getMainWebApp {
                chat_id: self.as_chat_id(&chat_id),
                bot_user_id: self.as_user_id(&bot_user_id),
                start_parameter,
                parameters: Some(self.as_web_app_open_parameters()),
            }));
            true
        } else if op == "gwau" {
            let (bot_user_id, url) = tokenizer::split(args, ' ');
            self.send_request(Function::getWebAppUrl(td_api::getWebAppUrl {
                bot_user_id: self.as_user_id(&bot_user_id),
                url,
                parameters: Some(self.as_web_app_open_parameters()),
            }));
            true
        } else if op == "ggbwau" {
            self.send_request(Function::getGuardBotWebAppUrl(
                td_api::getGuardBotWebAppUrl {
                    query_id: tokenizer::to_integer_i64(trim_td(args)),
                    parameters: Some(self.as_web_app_open_parameters()),
                },
            ));
            true
        } else if op == "swad" {
            let (bot_user_id, rest0) = tokenizer::split(args, ' ');
            let (button_text, data) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::sendWebAppData(td_api::sendWebAppData {
                bot_user_id: self.as_user_id(&bot_user_id),
                button_text,
                data,
            }));
            true
        } else if op == "owa" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (bot_user_id, url) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::openWebApp(td_api::openWebApp {
                chat_id: self.as_chat_id(&chat_id),
                bot_user_id: self.as_user_id(&bot_user_id),
                url,
                topic_id: self.get_message_topic_id(),
                reply_to: self.get_input_message_reply_to(),
                parameters: Some(self.as_web_app_open_parameters()),
            }));
            true
        } else if op == "cwa" {
            self.send_request(Function::closeWebApp(td_api::closeWebApp {
                web_app_launch_id: tokenizer::to_integer_i64(trim_td(args)),
            }));
            true
        } else if op == "cwafd" {
            let (bot_user_id, rest0) = tokenizer::split(args, ' ');
            let (file_name, url) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::checkWebAppFileDownload(
                td_api::checkWebAppFileDownload {
                    bot_user_id: self.as_user_id(&bot_user_id),
                    file_name,
                    url,
                },
            ));
            true
        } else if op == "sca" {
            let (chat_id, action) = tokenizer::split(args, ' ');
            self.send_request(Function::sendChatAction(td_api::sendChatAction {
                chat_id: self.as_chat_id(&chat_id),
                topic_id: self.get_message_topic_id(),
                business_connection_id: self.business_connection_id.lock().unwrap().clone(),
                action: as_chat_action(&action),
            }));
            true
        } else if op == "spm" {
            let (chat_id, draft_id) = tokenizer::split(args, ' ');
            self.send_request(Function::stopPendingMessage(td_api::stopPendingMessage {
                chat_id: self.as_chat_id(&chat_id),
                topic_id: self.get_message_topic_id(),
                draft_id: tokenizer::to_integer_i64(&draft_id),
            }));
            true
        } else if op == "smt" || op == "smtp" || op == "smtf" || op == "smtpf" {
            let chat_id = self.as_chat_id(args);
            for i in 1..=200 {
                let mut message = if random::fast(0, 3) == 0 && i > 90 {
                    "sleep ".to_owned()
                } else {
                    String::new()
                };
                message.push('#');
                message.push_str(&i.to_string());
                if i == 6 || (op.ends_with('f') && i % 2 == 0) {
                    message = "a".repeat(4097);
                }
                if op.as_bytes().get(3).copied() == Some(b'p') {
                    self.send_message(
                        chat_id,
                        td_api::InputMessageContent::inputMessagePhoto(td_api::inputMessagePhoto {
                            photo: Some(self.as_input_photo("rgb.jpg")),
                            caption: Some(self.as_formatted_text(&message)),
                            show_caption_above_media: self
                                .show_caption_above_media
                                .load(Ordering::SeqCst),
                            self_destruct_type: self.get_message_self_destruct_type(),
                            has_spoiler: self.has_spoiler.load(Ordering::SeqCst),
                        }),
                    );
                } else {
                    self.send_message(chat_id, self.as_input_message(&message, false));
                }
            }
            true
        } else if op == "ssm" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (filter, rest1) = tokenizer::split(&rest0, ' ');
            let (offset, query_spell) = tokenizer::split(&rest1, ' ');
            // The `SearchQuery` get_args (cli.cpp:996) — the first word is
            // the limit when it is a positive integer, else the limit is 10
            // and the whole spell is the query.
            let trimmed = trim_td(&query_spell);
            let (limit, query) = {
                let (limit_word, rest) = tokenizer::split(trimmed, ' ');
                match limit_word.parse::<i32>() {
                    Ok(parsed) if parsed > 0 => (parsed, rest),
                    _ => (10, trimmed.to_owned()),
                }
            };
            self.send_request(Function::searchSecretMessages(
                td_api::searchSecretMessages {
                    chat_id: self.as_chat_id(&chat_id),
                    query,
                    offset,
                    limit,
                    filter: as_search_messages_filter(&filter),
                },
            ));
            true
        } else if op == "ssd" {
            *self.schedule_date.lock().unwrap() = args.to_owned();
            true
        } else if op == "ssrp" {
            self.schedule_repeat_period
                .store(tokenizer::to_integer_i32(trim_td(args)), Ordering::SeqCst);
            true
        } else if op == "smei" {
            self.message_effect_id
                .store(tokenizer::to_integer_i64(trim_td(args)), Ordering::SeqCst);
            true
        } else if op == "smpmsc" {
            self.paid_message_star_count
                .store(tokenizer::to_integer_i64(trim_td(args)), Ordering::SeqCst);
            true
        } else if op == "sop" {
            self.only_preview.store(as_bool(args), Ordering::SeqCst);
            true
        } else if op == "sfti" {
            self.forum_topic_id
                .store(as_forum_topic_id(args), Ordering::SeqCst);
            true
        } else if op == "smti" {
            self.message_thread_id
                .store(tokenizer::to_integer_i64(trim_td(args)), Ordering::SeqCst);
            true
        } else if op == "sdmcti" {
            self.direct_messages_chat_topic_id
                .store(tokenizer::to_integer_i64(trim_td(args)), Ordering::SeqCst);
            true
        } else if op == "sbci" {
            *self.business_connection_id.lock().unwrap() = args.to_owned();
            true
        } else if op == "shs" {
            self.has_spoiler.store(as_bool(args), Ordering::SeqCst);
            true
        } else if op == "smsdt" {
            self.message_self_destruct_time
                .store(tokenizer::to_integer_i32(trim_td(args)), Ordering::SeqCst);
            true
        } else if op == "gcams" {
            self.send_request(Function::getChatAvailableMessageSenders(
                td_api::getChatAvailableMessageSenders {
                    chat_id: self.as_chat_id(args),
                },
            ));
            true
        } else if op == "scmsr" {
            let (chat_id, sender_id) = tokenizer::split(args, ' ');
            self.send_request(Function::setChatMessageSender(
                td_api::setChatMessageSender {
                    chat_id: self.as_chat_id(&chat_id),
                    message_sender_id: self.as_message_sender(&sender_id),
                },
            ));
            true
        } else if op == "smr" {
            let (reply_message_id, reply_chat_id) = tokenizer::split(args, ' ');
            self.reply_message_id
                .store(as_message_id(&reply_message_id), Ordering::SeqCst);
            self.reply_chat_id
                .store(self.as_chat_id(&reply_chat_id), Ordering::SeqCst);
            true
        } else if op == "smrq" {
            *self.reply_quote.lock().unwrap() = args.to_owned();
            true
        } else if op == "smrqp" {
            self.reply_quote_position
                .store(tokenizer::to_integer_i32(trim_td(args)), Ordering::SeqCst);
            true
        } else if op == "smrcti" {
            self.reply_checklist_task_id
                .store(tokenizer::to_integer_i32(trim_td(args)), Ordering::SeqCst);
            true
        } else if op == "smrpoi" {
            *self.reply_poll_option_id.lock().unwrap() = args.to_owned();
            true
        } else if op == "smrs" {
            let (reply_story_chat_id, reply_story_id) = tokenizer::split(args, ' ');
            self.reply_story_chat_id
                .store(self.as_chat_id(&reply_story_chat_id), Ordering::SeqCst);
            self.reply_story_id
                .store(as_story_id(&reply_story_id), Ordering::SeqCst);
            true
        } else if op == "slpo" {
            let (is_disabled, rest0) = tokenizer::split(args, ' ');
            let (url, rest1) = tokenizer::split(&rest0, ' ');
            let (small, rest2) = tokenizer::split(&rest1, ' ');
            let (large, above) = tokenizer::split(&rest2, ' ');
            self.link_preview_is_disabled
                .store(as_bool(&is_disabled), Ordering::SeqCst);
            *self.link_preview_url.lock().unwrap() = url;
            self.link_preview_force_small_media
                .store(as_bool(&small), Ordering::SeqCst);
            self.link_preview_force_large_media
                .store(as_bool(&large), Ordering::SeqCst);
            self.link_preview_show_above_text
                .store(as_bool(&above), Ordering::SeqCst);
            true
        } else if op == "sscam" {
            self.show_caption_above_media
                .store(as_bool(args), Ordering::SeqCst);
            true
        } else if op == "sgci" {
            self.gift_collection_id
                .store(tokenizer::to_integer_i32(trim_td(args)), Ordering::SeqCst);
            true
        } else if op == "ssmt" {
            self.saved_messages_topic_id
                .store(self.as_chat_id(args), Ordering::SeqCst);
            true
        } else if op == "sqrs" {
            *self.quick_reply_shortcut_name.lock().unwrap() = args.to_owned();
            true
        } else if op == "smer" {
            self.ephemeral_message_receiver_id
                .store(tokenizer::to_integer_i64(trim_td(args)), Ordering::SeqCst);
            true
        } else if op == "swm" {
            self.send_welcome_message
                .store(as_bool(args), Ordering::SeqCst);
            true
        } else if op == "ewm" {
            self.edit_welcome_message_id
                .store(tokenizer::to_integer_i32(trim_td(args)), Ordering::SeqCst);
            true
        } else if op == "smas" {
            *self.added_sticker_file_ids.lock().unwrap() = as_file_ids(args);
            true
        } else if op == "smc" {
            *self.caption.lock().unwrap() = args.to_owned();
            true
        } else if op == "srmm" {
            *self.rich_message_media.lock().unwrap() = args.to_owned();
            true
        } else if op == "smco" {
            *self.cover.lock().unwrap() = args.to_owned();
            true
        } else if op == "smth" {
            *self.thumbnail.lock().unwrap() = args.to_owned();
            true
        } else if op == "smst" {
            self.start_timestamp
                .store(tokenizer::to_integer_i32(trim_td(args)), Ordering::SeqCst);
            true
        } else if op == "sugai" {
            *self.upgraded_gift_attribute_ids.lock().unwrap() = tokenizer::full_split(args, ' ');
            true
        } else if op == "sspp" {
            *self.suggested_post_price.lock().unwrap() = args.to_owned();
            true
        } else if op == "sspsd" {
            self.suggested_post_send_date
                .store(tokenizer::to_integer_i32(trim_td(args)), Ordering::SeqCst);
            true
        } else if op == "sm" || op == "sms" || op == "smf" || op == "srm" {
            let (chat_id, message) = tokenizer::split(args, ' ');
            let message = if op == "smf" {
                "a".repeat(5097)
            } else {
                message
            };
            self.send_message_opts(
                self.as_chat_id(&chat_id),
                self.as_input_message(&message, op == "srm"),
                op == "sms",
                false,
            );
            true
        } else if op == "smce" {
            let chat_id = self.as_chat_id(args);
            let entities = vec![
                td_api::TextEntity {
                    offset: 0,
                    length: 2,
                    r#type: Some(td_api::TextEntityType::textEntityTypeCustomEmoji(
                        td_api::textEntityTypeCustomEmoji {
                            custom_emoji_id: 5368324170671202286,
                        },
                    )),
                },
                td_api::TextEntity {
                    offset: 3,
                    length: 2,
                    r#type: Some(td_api::TextEntityType::textEntityTypeCustomEmoji(
                        td_api::textEntityTypeCustomEmoji {
                            custom_emoji_id: 5377637695583426942,
                        },
                    )),
                },
                td_api::TextEntity {
                    offset: 6,
                    length: 5,
                    r#type: Some(td_api::TextEntityType::textEntityTypeCustomEmoji(
                        td_api::textEntityTypeCustomEmoji {
                            custom_emoji_id: 5368324170671202286,
                        },
                    )),
                },
            ];
            let text = self.as_formatted_text_with_entities(
                "\u{1F44D} \u{1F609} \u{1F9D1}\u{200D}\u{1F9D2}",
                entities,
            );
            self.send_message(
                chat_id,
                td_api::InputMessageContent::inputMessageText(td_api::inputMessageText {
                    text: Some(text),
                    link_preview_options: self.get_link_preview_options(),
                    clear_draft: true,
                }),
            );
            true
        } else if op == "alm" || op == "alrm" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (sender_id, message) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::addLocalMessage(td_api::addLocalMessage {
                chat_id: self.as_chat_id(&chat_id),
                sender_id: self.as_message_sender(&sender_id),
                reply_to: self.get_input_message_reply_to(),
                disable_notification: false,
                input_message_content: Some(Box::new(
                    self.as_input_message(&message, op == "alrm"),
                )),
            }));
            true
        } else if op == "spmp" || op == "spmv" {
            let (chat_id, files) = tokenizer::split(args, ' ');
            let is_photo = op == "spmp";
            let star_count = if is_photo { 11 } else { 12 };
            let paid_media = tokenizer::full_split(&files, ' ')
                .iter()
                .map(|file| td_api::InputPaidMedia {
                    r#type: if is_photo {
                        Some(td_api::InputPaidMediaType::inputPaidMediaTypePhoto(
                            td_api::inputPaidMediaTypePhoto {
                                video: self.get_input_cover(),
                            },
                        ))
                    } else {
                        Some(td_api::InputPaidMediaType::inputPaidMediaTypeVideo(
                            td_api::inputPaidMediaTypeVideo {
                                cover: self.get_input_cover(),
                                start_timestamp: self.start_timestamp.load(Ordering::SeqCst),
                                duration: 10,
                                supports_streaming: true,
                            },
                        ))
                    },
                    media: as_input_file(file),
                    thumbnail: self.get_input_thumbnail(),
                    added_sticker_file_ids: self.get_added_sticker_file_ids(),
                    width: 0,
                    height: 0,
                })
                .collect();
            self.send_message(
                self.as_chat_id(&chat_id),
                td_api::InputMessageContent::inputMessagePaidMedia(td_api::inputMessagePaidMedia {
                    star_count,
                    paid_media,
                    caption: self.get_caption(),
                    show_caption_above_media: random::fast_bool(),
                    payload: if is_photo { "photo" } else { "video" }.to_owned(),
                }),
            );
            true
        } else if op == "smap" || op == "smad" || op == "smav" {
            let (chat_id, files) = tokenizer::split(args, ' ');
            let input_message_contents = tokenizer::full_split(&files, ' ')
                .iter()
                .map(|file| -> Box<td_api::InputMessageContent> {
                    if op == "smap" {
                        Box::new(td_api::InputMessageContent::inputMessagePhoto(
                            td_api::inputMessagePhoto {
                                photo: Some(self.as_input_photo(file)),
                                caption: self.get_caption(),
                                show_caption_above_media: self
                                    .show_caption_above_media
                                    .load(Ordering::SeqCst),
                                self_destruct_type: if random::fast_bool() {
                                    self.get_message_self_destruct_type()
                                } else {
                                    None
                                },
                                has_spoiler: self.has_spoiler.load(Ordering::SeqCst)
                                    && random::fast_bool(),
                            },
                        ))
                    } else if op == "smad" {
                        Box::new(td_api::InputMessageContent::inputMessageDocument(
                            td_api::inputMessageDocument {
                                document: Some(self.as_input_document(file, true)),
                                caption: self.get_caption(),
                            },
                        ))
                    } else {
                        Box::new(td_api::InputMessageContent::inputMessageVideo(
                            td_api::inputMessageVideo {
                                video: Some(self.as_input_video(file)),
                                caption: self.get_caption(),
                                show_caption_above_media: self
                                    .show_caption_above_media
                                    .load(Ordering::SeqCst),
                                self_destruct_type: self.get_message_self_destruct_type(),
                                has_spoiler: self.has_spoiler.load(Ordering::SeqCst),
                            },
                        ))
                    }
                })
                .collect();
            let business = self.business_connection_id.lock().unwrap().clone();
            if !business.is_empty() {
                self.send_request(Function::sendBusinessMessageAlbum(
                    td_api::sendBusinessMessageAlbum {
                        business_connection_id: business,
                        chat_id: self.as_chat_id(&chat_id),
                        reply_to: self.get_input_message_reply_to(),
                        protect_content: random::fast_bool(),
                        disable_notification: random::fast_bool(),
                        effect_id: self.message_effect_id.load(Ordering::SeqCst),
                        input_message_contents,
                    },
                ));
            } else {
                let shortcut = self.quick_reply_shortcut_name.lock().unwrap().clone();
                if !shortcut.is_empty() {
                    self.send_request(Function::addQuickReplyShortcutMessageAlbum(
                        td_api::addQuickReplyShortcutMessageAlbum {
                            shortcut_name: shortcut,
                            reply_to_message_id: self.reply_message_id.load(Ordering::SeqCst),
                            input_message_contents,
                        },
                    ));
                } else {
                    self.send_request(Function::sendMessageAlbum(td_api::sendMessageAlbum {
                        chat_id: self.as_chat_id(&chat_id),
                        topic_id: self.get_message_topic_id(),
                        reply_to: self.get_input_message_reply_to(),
                        options: Some(self.default_message_send_options()),
                        input_message_contents,
                    }));
                }
            }
            true
        } else if op == "savt" {
            let (verification_id, token) = tokenizer::split(args, ' ');
            self.send_request(Function::setApplicationVerificationToken(
                td_api::setApplicationVerificationToken {
                    verification_id: tokenizer::to_integer_i64(&verification_id),
                    token,
                },
            ));
            true
        } else if op == "gmft" {
            use std::io::Read;
            let opened = std::fs::File::open(args).and_then(|mut file| {
                let mut buf = vec![0u8; 2 << 10];
                let mut read = 0usize;
                while read < buf.len() {
                    match file.read(&mut buf[read..]) {
                        Ok(0) => break,
                        Ok(n) => read += n,
                        Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(e) => return Err(e),
                    }
                }
                buf.truncate(read);
                Ok(buf)
            });
            match opened {
                Ok(mut head) => {
                    // C++ pops bytes until `check_utf8` passes; the longest
                    // valid prefix is the same string.
                    if let Err(e) = std::str::from_utf8(&head) {
                        head.truncate(e.valid_up_to());
                    }
                    self.send_request(Function::getMessageFileType(td_api::getMessageFileType {
                        message_file_head: String::from_utf8_lossy(&head).into_owned(),
                    }));
                }
                Err(err) => {
                    tdutils::log_error!("{}", err);
                }
            }
            true
        } else if op == "gmict" {
            self.send_request(Function::getMessageImportConfirmationText(
                td_api::getMessageImportConfirmationText {
                    chat_id: self.as_chat_id(args),
                },
            ));
            true
        } else if op == "im" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (message_file, attached) = tokenizer::split(&rest0, ' ');
            let attached_files = tokenizer::full_split(&attached, ' ')
                .iter()
                .filter_map(|f| as_input_file(f))
                .collect();
            self.send_request(Function::importMessages(td_api::importMessages {
                chat_id: self.as_chat_id(&chat_id),
                message_file: as_input_file(&message_file),
                attached_files,
            }));
            true
        } else if op == "em" || op == "emrm" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (message_id, message) = tokenizer::split(&rest0, ' ');
            let input_text = self.as_input_message(&message, op == "emrm");
            let business = self.business_connection_id.lock().unwrap().clone();
            if !business.is_empty() {
                self.send_request(Function::editBusinessMessageText(
                    td_api::editBusinessMessageText {
                        business_connection_id: business,
                        chat_id: self.as_chat_id(&chat_id),
                        message_id: as_message_id(&message_id),
                        reply_markup: None,
                        input_message_content: Some(Box::new(input_text)),
                    },
                ));
            } else {
                self.send_request(Function::editMessageText(td_api::editMessageText {
                    chat_id: self.as_chat_id(&chat_id),
                    message_id: as_message_id(&message_id),
                    reply_markup: None,
                    input_message_content: Some(Box::new(input_text)),
                }));
            }
            true
        } else if op == "eqrm" || op == "eqrrm" {
            let (shortcut_id, rest0) = tokenizer::split(args, ' ');
            let (message_id, message) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::editQuickReplyMessage(
                td_api::editQuickReplyMessage {
                    shortcut_id: self.as_shortcut_id(&shortcut_id),
                    message_id: as_message_id(&message_id),
                    input_message_content: Some(Box::new(
                        self.as_input_message(&message, op == "eqrrm"),
                    )),
                },
            ));
            true
        } else if op == "eman" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (message_id, animation) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::editMessageMedia(td_api::editMessageMedia {
                chat_id: self.as_chat_id(&chat_id),
                message_id: as_message_id(&message_id),
                reply_markup: None,
                input_message_content: Some(Box::new(
                    td_api::InputMessageContent::inputMessageAnimation(
                        td_api::inputMessageAnimation {
                            animation: Some(self.as_input_animation(&animation)),
                            caption: self.get_caption(),
                            show_caption_above_media: self
                                .show_caption_above_media
                                .load(Ordering::SeqCst),
                            has_spoiler: self.has_spoiler.load(Ordering::SeqCst),
                        },
                    ),
                )),
            }));
            true
        } else if op == "emc" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (message_id, caption) = tokenizer::split(&rest0, ' ');
            let caption_text = if caption.is_empty() {
                let stored = self.caption.lock().unwrap().clone();
                self.as_formatted_text(&stored)
            } else {
                self.as_formatted_text(&caption)
            };
            self.send_request(Function::editMessageCaption(td_api::editMessageCaption {
                chat_id: self.as_chat_id(&chat_id),
                message_id: as_message_id(&message_id),
                reply_markup: None,
                caption: Some(caption_text),
                show_caption_above_media: self.show_caption_above_media.load(Ordering::SeqCst),
            }));
            true
        } else if op == "emd" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (message_id, document) = tokenizer::split(&rest0, ' ');
            let input_document =
                td_api::InputMessageContent::inputMessageDocument(td_api::inputMessageDocument {
                    document: Some(self.as_input_document(&document, false)),
                    caption: self.get_caption(),
                });
            let business = self.business_connection_id.lock().unwrap().clone();
            if !business.is_empty() {
                self.send_request(Function::editBusinessMessageMedia(
                    td_api::editBusinessMessageMedia {
                        business_connection_id: business,
                        chat_id: self.as_chat_id(&chat_id),
                        message_id: as_message_id(&message_id),
                        reply_markup: None,
                        input_message_content: Some(Box::new(input_document)),
                    },
                ));
            } else {
                self.send_request(Function::editMessageMedia(td_api::editMessageMedia {
                    chat_id: self.as_chat_id(&chat_id),
                    message_id: as_message_id(&message_id),
                    reply_markup: None,
                    input_message_content: Some(Box::new(input_document)),
                }));
            }
            true
        } else if op == "eqrmd" {
            let (shortcut_id, rest0) = tokenizer::split(args, ' ');
            let (message_id, document) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::editQuickReplyMessage(
                td_api::editQuickReplyMessage {
                    shortcut_id: self.as_shortcut_id(&shortcut_id),
                    message_id: as_message_id(&message_id),
                    input_message_content: Some(Box::new(
                        td_api::InputMessageContent::inputMessageDocument(
                            td_api::inputMessageDocument {
                                document: Some(self.as_input_document(&document, false)),
                                caption: self.get_caption(),
                            },
                        ),
                    )),
                },
            ));
            true
        } else if op == "emp" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (message_id, photo) = tokenizer::split(&rest0, ' ');
            let input_photo =
                td_api::InputMessageContent::inputMessagePhoto(td_api::inputMessagePhoto {
                    photo: Some(self.as_input_photo(&photo)),
                    caption: self.get_caption(),
                    show_caption_above_media: self.show_caption_above_media.load(Ordering::SeqCst),
                    self_destruct_type: self.get_message_self_destruct_type(),
                    has_spoiler: self.has_spoiler.load(Ordering::SeqCst),
                });
            let business = self.business_connection_id.lock().unwrap().clone();
            if !business.is_empty() {
                self.send_request(Function::editBusinessMessageMedia(
                    td_api::editBusinessMessageMedia {
                        business_connection_id: business,
                        chat_id: self.as_chat_id(&chat_id),
                        message_id: as_message_id(&message_id),
                        reply_markup: None,
                        input_message_content: Some(Box::new(input_photo)),
                    },
                ));
            } else {
                self.send_request(Function::editMessageMedia(td_api::editMessageMedia {
                    chat_id: self.as_chat_id(&chat_id),
                    message_id: as_message_id(&message_id),
                    reply_markup: None,
                    input_message_content: Some(Box::new(input_photo)),
                }));
            }
            true
        } else if op == "eqrmp" {
            let (shortcut_id, rest0) = tokenizer::split(args, ' ');
            let (message_id, photo) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::editQuickReplyMessage(
                td_api::editQuickReplyMessage {
                    shortcut_id: self.as_shortcut_id(&shortcut_id),
                    message_id: as_message_id(&message_id),
                    input_message_content: Some(Box::new(
                        td_api::InputMessageContent::inputMessagePhoto(td_api::inputMessagePhoto {
                            photo: Some(self.as_input_photo(&photo)),
                            caption: self.get_caption(),
                            show_caption_above_media: self
                                .show_caption_above_media
                                .load(Ordering::SeqCst),
                            self_destruct_type: None,
                            has_spoiler: self.has_spoiler.load(Ordering::SeqCst),
                        }),
                    )),
                },
            ));
            true
        } else if op == "eqrmv" {
            let (shortcut_id, rest0) = tokenizer::split(args, ' ');
            let (message_id, video) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::editQuickReplyMessage(
                td_api::editQuickReplyMessage {
                    shortcut_id: self.as_shortcut_id(&shortcut_id),
                    message_id: as_message_id(&message_id),
                    input_message_content: Some(Box::new(
                        td_api::InputMessageContent::inputMessageVideo(td_api::inputMessageVideo {
                            video: Some(self.as_input_video(&video)),
                            caption: self.get_caption(),
                            show_caption_above_media: self
                                .show_caption_above_media
                                .load(Ordering::SeqCst),
                            self_destruct_type: self.get_message_self_destruct_type(),
                            has_spoiler: self.has_spoiler.load(Ordering::SeqCst),
                        }),
                    )),
                },
            ));
            true
        } else if op == "eqrman" {
            let (shortcut_id, rest0) = tokenizer::split(args, ' ');
            let (message_id, animation) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::editQuickReplyMessage(
                td_api::editQuickReplyMessage {
                    shortcut_id: self.as_shortcut_id(&shortcut_id),
                    message_id: as_message_id(&message_id),
                    input_message_content: Some(Box::new(
                        td_api::InputMessageContent::inputMessageAnimation(
                            td_api::inputMessageAnimation {
                                animation: Some(self.as_input_animation(&animation)),
                                caption: self.get_caption(),
                                show_caption_above_media: self
                                    .show_caption_above_media
                                    .load(Ordering::SeqCst),
                                has_spoiler: self.has_spoiler.load(Ordering::SeqCst),
                            },
                        ),
                    )),
                },
            ));
            true
        } else if op == "eqrmchl" {
            let (shortcut_id, rest0) = tokenizer::split(args, ' ');
            let (message_id, checklist) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::editQuickReplyMessage(
                td_api::editQuickReplyMessage {
                    shortcut_id: self.as_shortcut_id(&shortcut_id),
                    message_id: as_message_id(&message_id),
                    input_message_content: Some(Box::new(
                        td_api::InputMessageContent::inputMessageChecklist(
                            td_api::inputMessageChecklist {
                                checklist: Some(self.get_input_checklist(&checklist)),
                            },
                        ),
                    )),
                },
            ));
            true
        } else if op == "emv" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (message_id, video) = tokenizer::split(&rest0, ' ');
            let input_video =
                td_api::InputMessageContent::inputMessageVideo(td_api::inputMessageVideo {
                    video: Some(self.as_input_video(&video)),
                    caption: self.get_caption(),
                    show_caption_above_media: self.show_caption_above_media.load(Ordering::SeqCst),
                    self_destruct_type: self.get_message_self_destruct_type(),
                    has_spoiler: self.has_spoiler.load(Ordering::SeqCst),
                });
            let business = self.business_connection_id.lock().unwrap().clone();
            if !business.is_empty() {
                self.send_request(Function::editBusinessMessageMedia(
                    td_api::editBusinessMessageMedia {
                        business_connection_id: business,
                        chat_id: self.as_chat_id(&chat_id),
                        message_id: as_message_id(&message_id),
                        reply_markup: None,
                        input_message_content: Some(Box::new(input_video)),
                    },
                ));
            } else {
                self.send_request(Function::editMessageMedia(td_api::editMessageMedia {
                    chat_id: self.as_chat_id(&chat_id),
                    message_id: as_message_id(&message_id),
                    reply_markup: None,
                    input_message_content: Some(Box::new(input_video)),
                }));
            }
            true
        } else if op == "emll" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (message_id, rest1) = tokenizer::split(&rest0, ' ');
            let (latitude, rest2) = tokenizer::split(&rest1, ' ');
            let (longitude, rest3) = tokenizer::split(&rest2, ' ');
            let (live_period, rest4) = tokenizer::split(&rest3, ' ');
            let (accuracy, rest5) = tokenizer::split(&rest4, ' ');
            let (heading, proximity_alert_radius) = tokenizer::split(&rest5, ' ');
            self.send_request(Function::editMessageLiveLocation(
                td_api::editMessageLiveLocation {
                    chat_id: self.as_chat_id(&chat_id),
                    message_id: as_message_id(&message_id),
                    reply_markup: None,
                    location: Some(self.as_live_location(
                        &latitude,
                        &longitude,
                        &accuracy,
                        tokenizer::to_integer_i32(&live_period),
                        tokenizer::to_integer_i32(&heading),
                        tokenizer::to_integer_i32(&proximity_alert_radius),
                    )),
                },
            ));
            true
        } else if op == "emchl" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (message_id, checklist) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::editMessageChecklist(
                td_api::editMessageChecklist {
                    chat_id: self.as_chat_id(&chat_id),
                    message_id: as_message_id(&message_id),
                    reply_markup: None,
                    checklist: Some(self.get_input_checklist(&checklist)),
                },
            ));
            true
        } else if op == "emss" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (message_id, rest1) = tokenizer::split(&rest0, ' ');
            let (date, repeat_period) = tokenizer::split(&rest1, ' ');
            let scheduling_state = {
                let date = trim_td(&date);
                if date.is_empty() {
                    None
                } else {
                    let send_date = tokenizer::to_integer_i32(date);
                    if send_date == -1 {
                        Some(
                            td_api::MessageSchedulingState::messageSchedulingStateSendWhenOnline(
                                td_api::messageSchedulingStateSendWhenOnline,
                            ),
                        )
                    } else {
                        Some(
                            td_api::MessageSchedulingState::messageSchedulingStateSendAtDate(
                                td_api::messageSchedulingStateSendAtDate {
                                    send_date,
                                    repeat_period: tokenizer::to_integer_i32(&repeat_period),
                                },
                            ),
                        )
                    }
                }
            };
            self.send_request(Function::editMessageSchedulingState(
                td_api::editMessageSchedulingState {
                    chat_id: self.as_chat_id(&chat_id),
                    message_id: as_message_id(&message_id),
                    scheduling_state,
                },
            ));
            true
        } else if op == "dmec" {
            let (chat_id, message_id) = tokenizer::split(args, ' ');
            self.send_request(Function::deleteMessageEphemeralContent(
                td_api::deleteMessageEphemeralContent {
                    chat_id: self.as_chat_id(&chat_id),
                    message_id: as_message_id(&message_id),
                },
            ));
            true
        } else if op == "lcwm" {
            self.send_request(Function::loadChatWelcomeMessages(
                td_api::loadChatWelcomeMessages {
                    chat_id: self.as_chat_id(args),
                },
            ));
            true
        } else if op == "dcwm" {
            let (chat_id, welcome_message_id) = tokenizer::split(args, ' ');
            self.send_request(Function::deleteChatWelcomeMessage(
                td_api::deleteChatWelcomeMessage {
                    chat_id: self.as_chat_id(&chat_id),
                    welcome_message_id: tokenizer::to_integer_i32(&welcome_message_id),
                },
            ));
            true
        } else if op == "dacwm" {
            self.send_request(Function::deleteAllChatWelcomeMessages(
                td_api::deleteAllChatWelcomeMessages {
                    chat_id: self.as_chat_id(args),
                },
            ));
            true
        } else if op == "smfc" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (message_id, message) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::setMessageFactCheck(td_api::setMessageFactCheck {
                chat_id: self.as_chat_id(&chat_id),
                message_id: as_message_id(&message_id),
                text: Some(self.as_formatted_text(&message)),
            }));
            true
        } else {
            false
        }
    }

    /// cli.cpp:6861 — the quick-reply/inline-query/callback-query/forum-topic
    /// chain.
    fn op_block_logging(&self, op: &str, args: &str) -> bool {
        if op == "cqrsn" {
            self.execute(Function::checkQuickReplyShortcutName(
                td_api::checkQuickReplyShortcutName {
                    name: args.to_owned(),
                },
            ));
            true
        } else if op == "lqrs" {
            self.send_request(Function::loadQuickReplyShortcuts(
                td_api::loadQuickReplyShortcuts,
            ));
            true
        } else if op == "dqrs" {
            self.send_request(Function::deleteQuickReplyShortcut(
                td_api::deleteQuickReplyShortcut {
                    shortcut_id: self.as_shortcut_id(args),
                },
            ));
            true
        } else if op == "sqrsn" {
            let (shortcut_id, name) = tokenizer::split(args, ' ');
            self.send_request(Function::setQuickReplyShortcutName(
                td_api::setQuickReplyShortcutName {
                    shortcut_id: self.as_shortcut_id(&shortcut_id),
                    name,
                },
            ));
            true
        } else if op == "rqrs" {
            self.send_request(Function::reorderQuickReplyShortcuts(
                td_api::reorderQuickReplyShortcuts {
                    shortcut_ids: self.as_shortcut_ids(args),
                },
            ));
            true
        } else if op == "lqrsm" {
            self.send_request(Function::loadQuickReplyShortcutMessages(
                td_api::loadQuickReplyShortcutMessages {
                    shortcut_id: self.as_shortcut_id(args),
                },
            ));
            true
        } else if op == "dqrsm" {
            let (shortcut_id, message_ids) = tokenizer::split(args, ' ');
            self.send_request(Function::deleteQuickReplyShortcutMessages(
                td_api::deleteQuickReplyShortcutMessages {
                    shortcut_id: self.as_shortcut_id(&shortcut_id),
                    message_ids: as_message_ids(&message_ids),
                },
            ));
            true
        } else if op == "gftdi" {
            self.send_request(Function::getForumTopicDefaultIcons(
                td_api::getForumTopicDefaultIcons,
            ));
            true
        } else if op == "cft" || op == "cfti" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (name, icon_color) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::createForumTopic(td_api::createForumTopic {
                chat_id: self.as_chat_id(&chat_id),
                name,
                is_name_implicit: op == "cfti",
                icon: Some(td_api::ForumTopicIcon {
                    color: tokenizer::to_integer_i32(&icon_color),
                    custom_emoji_id: 0,
                }),
            }));
            true
        } else if op == "eft" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (forum_topic_id, rest1) = tokenizer::split(&rest0, ' ');
            let (name, rest2) = tokenizer::split(&rest1, ' ');
            let (edit_icon_custom_emoji, icon_custom_emoji_id) = tokenizer::split(&rest2, ' ');
            self.send_request(Function::editForumTopic(td_api::editForumTopic {
                chat_id: self.as_chat_id(&chat_id),
                forum_topic_id: as_forum_topic_id(&forum_topic_id),
                name,
                edit_icon_custom_emoji: as_bool(&edit_icon_custom_emoji),
                icon_custom_emoji_id: self.as_custom_emoji_id(&icon_custom_emoji_id),
            }));
            true
        } else if op == "gft" {
            let (chat_id, forum_topic_id) = tokenizer::split(args, ' ');
            self.send_request(Function::getForumTopic(td_api::getForumTopic {
                chat_id: self.as_chat_id(&chat_id),
                forum_topic_id: as_forum_topic_id(&forum_topic_id),
            }));
            true
        } else if op == "gftl" {
            let (chat_id, forum_topic_id) = tokenizer::split(args, ' ');
            self.send_request(Function::getForumTopicLink(td_api::getForumTopicLink {
                chat_id: self.as_chat_id(&chat_id),
                forum_topic_id: as_forum_topic_id(&forum_topic_id),
            }));
            true
        } else if op == "gfts" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (query, rest1) = tokenizer::split(&rest0, ' ');
            let (offset_date, rest2) = tokenizer::split(&rest1, ' ');
            let (offset_message_id, rest3) = tokenizer::split(&rest2, ' ');
            let (offset_forum_topic_id, limit) = tokenizer::split(&rest3, ' ');
            self.send_request(Function::getForumTopics(td_api::getForumTopics {
                chat_id: self.as_chat_id(&chat_id),
                query,
                offset_date: tokenizer::to_integer_i32(&offset_date),
                offset_message_id: as_message_id(&offset_message_id),
                offset_forum_topic_id: as_forum_topic_id(&offset_forum_topic_id),
                limit: as_limit(&limit, 10),
            }));
            true
        } else if op == "tftic" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (forum_topic_id, is_closed) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::toggleForumTopicIsClosed(
                td_api::toggleForumTopicIsClosed {
                    chat_id: self.as_chat_id(&chat_id),
                    forum_topic_id: as_forum_topic_id(&forum_topic_id),
                    is_closed: as_bool(&is_closed),
                },
            ));
            true
        } else if op == "tgftih" {
            let (chat_id, is_hidden) = tokenizer::split(args, ' ');
            self.send_request(Function::toggleGeneralForumTopicIsHidden(
                td_api::toggleGeneralForumTopicIsHidden {
                    chat_id: self.as_chat_id(&chat_id),
                    is_hidden: as_bool(&is_hidden),
                },
            ));
            true
        } else if op == "tftip" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (forum_topic_id, is_pinned) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::toggleForumTopicIsPinned(
                td_api::toggleForumTopicIsPinned {
                    chat_id: self.as_chat_id(&chat_id),
                    forum_topic_id: as_forum_topic_id(&forum_topic_id),
                    is_pinned: as_bool(&is_pinned),
                },
            ));
            true
        } else if op == "spft" {
            let (chat_id, forum_topic_ids) = tokenizer::split(args, ' ');
            self.send_request(Function::setPinnedForumTopics(
                td_api::setPinnedForumTopics {
                    chat_id: self.as_chat_id(&chat_id),
                    forum_topic_ids: to_integers_i32(&forum_topic_ids),
                },
            ));
            true
        } else if op == "dft" {
            let (chat_id, forum_topic_id) = tokenizer::split(args, ' ');
            self.send_request(Function::deleteForumTopic(td_api::deleteForumTopic {
                chat_id: self.as_chat_id(&chat_id),
                forum_topic_id: as_forum_topic_id(&forum_topic_id),
            }));
            true
        } else if op == "sbsm" {
            let (bot_user_id, rest0) = tokenizer::split(args, ' ');
            let (chat_id, parameter) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::sendBotStartMessage(td_api::sendBotStartMessage {
                bot_user_id: self.as_user_id(&bot_user_id),
                chat_id: self.as_chat_id(&chat_id),
                parameter,
            }));
            true
        } else if op == "giqr" {
            let (bot_id, query) = tokenizer::split(args, ' ');
            self.send_request(Function::getInlineQueryResults(
                td_api::getInlineQueryResults {
                    bot_user_id: self.as_user_id(&bot_id),
                    chat_id: self.as_chat_id(&bot_id),
                    user_location: None,
                    query,
                    offset: String::new(),
                },
            ));
            true
        } else if op == "giqro" {
            let (bot_user_id, rest0) = tokenizer::split(args, ' ');
            let (offset, query) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::getInlineQueryResults(
                td_api::getInlineQueryResults {
                    bot_user_id: self.as_user_id(&bot_user_id),
                    chat_id: 0,
                    user_location: None,
                    query,
                    offset,
                },
            ));
            true
        } else if op == "giqrl" {
            let (bot_user_id, query) = tokenizer::split(args, ' ');
            self.send_request(Function::getInlineQueryResults(
                td_api::getInlineQueryResults {
                    bot_user_id: self.as_user_id(&bot_user_id),
                    chat_id: 0,
                    user_location: as_location("1.1", "2.2", ""),
                    query,
                    offset: String::new(),
                },
            ));
            true
        } else if op == "gpim" {
            let (bot_user_id, prepared_message_id) = tokenizer::split(args, ' ');
            self.send_request(Function::getPreparedInlineMessage(
                td_api::getPreparedInlineMessage {
                    bot_user_id: self.as_user_id(&bot_user_id),
                    prepared_message_id,
                },
            ));
            true
        } else if op == "siqr" || op == "siqrh" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (query_id, result_id) = tokenizer::split(&rest0, ' ');
            if self.quick_reply_shortcut_name.lock().unwrap().is_empty() {
                self.send_request(Function::sendInlineQueryResultMessage(
                    td_api::sendInlineQueryResultMessage {
                        chat_id: self.as_chat_id(&chat_id),
                        topic_id: self.get_message_topic_id(),
                        reply_to: self.get_input_message_reply_to(),
                        options: Some(self.default_message_send_options()),
                        query_id: tokenizer::to_integer_i64(&query_id),
                        result_id,
                        hide_via_bot: op == "siqrh",
                    },
                ));
            } else {
                self.send_request(Function::addQuickReplyShortcutInlineQueryResultMessage(
                    td_api::addQuickReplyShortcutInlineQueryResultMessage {
                        shortcut_name: self.quick_reply_shortcut_name.lock().unwrap().clone(),
                        reply_to_message_id: self.reply_message_id.load(Ordering::SeqCst),
                        query_id: tokenizer::to_integer_i64(&query_id),
                        result_id,
                        hide_via_bot: op == "siqrh",
                    },
                ));
            }
            true
        } else if op == "gpkb" {
            let (bot_user_id, prepared_button_id) = tokenizer::split(args, ' ');
            self.send_request(Function::getPreparedKeyboardButton(
                td_api::getPreparedKeyboardButton {
                    bot_user_id: self.as_user_id(&bot_user_id),
                    prepared_button_id,
                },
            ));
            true
        } else if op == "gcqa" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (message_id, data) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::getCallbackQueryAnswer(
                td_api::getCallbackQueryAnswer {
                    chat_id: self.as_chat_id(&chat_id),
                    message_id: as_message_id(&message_id),
                    payload: Some(td_api::CallbackQueryPayload::callbackQueryPayloadData(
                        td_api::callbackQueryPayloadData {
                            data: data.into_bytes(),
                        },
                    )),
                },
            ));
            true
        } else if op == "gcpqa" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (message_id, rest1) = tokenizer::split(&rest0, ' ');
            let (password, data) = tokenizer::split(&rest1, ' ');
            self.send_request(Function::getCallbackQueryAnswer(
                td_api::getCallbackQueryAnswer {
                    chat_id: self.as_chat_id(&chat_id),
                    message_id: as_message_id(&message_id),
                    payload: Some(
                        td_api::CallbackQueryPayload::callbackQueryPayloadDataWithPassword(
                            td_api::callbackQueryPayloadDataWithPassword {
                                password,
                                data: data.into_bytes(),
                            },
                        ),
                    ),
                },
            ));
            true
        } else if op == "gcgqa" {
            let (chat_id, message_id) = tokenizer::split(args, ' ');
            self.send_request(Function::getCallbackQueryAnswer(
                td_api::getCallbackQueryAnswer {
                    chat_id: self.as_chat_id(&chat_id),
                    message_id: as_message_id(&message_id),
                    payload: Some(td_api::CallbackQueryPayload::callbackQueryPayloadGame(
                        td_api::callbackQueryPayloadGame {
                            game_short_name: String::new(),
                        },
                    )),
                },
            ));
            true
        } else if op == "acq" || op == "acqa" {
            let (callback_query_id, text) = tokenizer::split(args, ' ');
            self.send_request(Function::answerCallbackQuery(td_api::answerCallbackQuery {
                callback_query_id: tokenizer::to_integer_i64(&callback_query_id),
                text,
                show_alert: op == "acqa",
                url: String::new(),
                cache_time: 0,
            }));
            true
        } else {
            false
        }
    }

    /// cli.cpp:7554 — the polls/checklists/ban chain (M.14.4c).
    fn op_block_drafts(&self, op: &str, args: &str) -> bool {
        if op == "bcm" {
            let (chat_id, member_id, banned_until_date, revoke_messages) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, rest1) = tokenizer::split(&rest0, ' ');
                let (w2, w3) = tokenizer::split(&rest1, ' ');
                (w0, w1, w2, w3)
            };
            self.send_request(Function::banChatMember(td_api::banChatMember {
                chat_id: self.as_chat_id(&chat_id),
                member_id: self.as_message_sender(&member_id),
                banned_until_date: tokenizer::to_integer_i32(&banned_until_date),
                revoke_messages: as_bool(&revoke_messages),
            }));
            true
        } else if op == "apo"
            || op == "apol"
            || op == "apolo"
            || op == "apop"
            || op == "apov"
            || op == "apoan"
        {
            let (chat_id, message_id, text, tail) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, rest1) = tokenizer::split(&rest0, ' ');
                let (w2, w3) = tokenizer::split(&rest1, ' ');
                (w0, w1, w2, w3)
            };
            let media = if op == "apol" {
                let (link, _) = tokenizer::split(&tail, ' ');
                Some(td_api::InputPollMedia::inputPollMediaLink(
                    td_api::inputPollMediaLink { url: link },
                ))
            } else if op == "apolo" {
                let (latitude, longitude) = tokenizer::split(&tail, ' ');
                Some(td_api::InputPollMedia::inputPollMediaLocation(
                    td_api::inputPollMediaLocation {
                        location: as_location(&latitude, &longitude, ""),
                    },
                ))
            } else if op == "apop" {
                let (photo, _) = tokenizer::split(&tail, ' ');
                Some(td_api::InputPollMedia::inputPollMediaPhoto(
                    td_api::inputPollMediaPhoto {
                        photo: Some(self.as_input_photo(&photo)),
                    },
                ))
            } else if op == "apov" {
                let (video, _) = tokenizer::split(&tail, ' ');
                Some(td_api::InputPollMedia::inputPollMediaVideo(
                    td_api::inputPollMediaVideo {
                        video: Some(self.as_input_video(&video)),
                    },
                ))
            } else if op == "apoan" {
                let (animation, _) = tokenizer::split(&tail, ' ');
                Some(td_api::InputPollMedia::inputPollMediaAnimation(
                    td_api::inputPollMediaAnimation {
                        animation: Some(self.as_input_animation(&animation)),
                    },
                ))
            } else {
                None
            };
            self.send_request(Function::addPollOption(td_api::addPollOption {
                chat_id: self.as_chat_id(&chat_id),
                message_id: as_message_id(&message_id),
                option: Some(td_api::InputPollOption {
                    text: Some(self.as_formatted_text(&text)),
                    media,
                }),
            }));
            true
        } else if op == "dpo" {
            let (chat_id, message_id, option_id) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            self.send_request(Function::deletePollOption(td_api::deletePollOption {
                chat_id: self.as_chat_id(&chat_id),
                message_id: as_message_id(&message_id),
                option_id,
            }));
            true
        } else if op == "spolla" {
            let (chat_id, message_id, option_ids) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            self.send_request(Function::setPollAnswer(td_api::setPollAnswer {
                chat_id: self.as_chat_id(&chat_id),
                message_id: as_message_id(&message_id),
                option_ids: autosplit(&option_ids)
                    .iter()
                    .map(|t| tokenizer::to_integer_i32(trim_td(t)))
                    .collect(),
            }));
            true
        } else if op == "gpollv" {
            let (chat_id, message_id, option_id, offset, limit) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, rest1) = tokenizer::split(&rest0, ' ');
                let (w2, rest2) = tokenizer::split(&rest1, ' ');
                let (w3, w4) = tokenizer::split(&rest2, ' ');
                (w0, w1, w2, w3, w4)
            };
            self.send_request(Function::getPollVoters(td_api::getPollVoters {
                chat_id: self.as_chat_id(&chat_id),
                message_id: as_message_id(&message_id),
                option_id: tokenizer::to_integer_i32(&option_id),
                offset: tokenizer::to_integer_i32(&offset),
                limit: as_limit(&limit, 10),
            }));
            true
        } else if op == "gpvst" {
            let (chat_id, message_id, is_dark) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            self.send_request(Function::getPollVoteStatistics(
                td_api::getPollVoteStatistics {
                    chat_id: self.as_chat_id(&chat_id),
                    message_id: as_message_id(&message_id),
                    is_dark: as_bool(&is_dark),
                },
            ));
            true
        } else if op == "stoppoll" {
            let (chat_id, message_id) = tokenizer::split(args, ' ');
            let business = self.business_connection_id.lock().unwrap().clone();
            if !business.is_empty() {
                self.send_request(Function::stopBusinessPoll(td_api::stopBusinessPoll {
                    business_connection_id: business,
                    chat_id: self.as_chat_id(&chat_id),
                    message_id: as_message_id(&message_id),
                    reply_markup: None,
                }));
            } else {
                self.send_request(Function::stopPoll(td_api::stopPoll {
                    chat_id: self.as_chat_id(&chat_id),
                    message_id: as_message_id(&message_id),
                    reply_markup: None,
                }));
            }
            true
        } else if op == "achlt" {
            let (chat_id, message_id, task_id, tail) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, rest1) = tokenizer::split(&rest0, ' ');
                let (w2, w3) = tokenizer::split(&rest1, ' ');
                (w0, w1, w2, w3)
            };
            let mut task_id = tokenizer::to_integer_i32(&task_id);
            let tasks = autosplit(&tail)
                .iter()
                .map(|task| {
                    task_id += 1;
                    td_api::InputChecklistTask {
                        id: task_id,
                        text: Some(self.as_formatted_text(task)),
                    }
                })
                .collect();
            self.send_request(Function::addChecklistTasks(td_api::addChecklistTasks {
                chat_id: self.as_chat_id(&chat_id),
                message_id: as_message_id(&message_id),
                tasks,
            }));
            true
        } else if op == "mchltad" {
            let (chat_id, message_id, done_task_ids, not_done_task_ids) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, rest1) = tokenizer::split(&rest0, ' ');
                let (w2, w3) = tokenizer::split(&rest1, ' ');
                (w0, w1, w2, w3)
            };
            self.send_request(Function::markChecklistTasksAsDone(
                td_api::markChecklistTasksAsDone {
                    chat_id: self.as_chat_id(&chat_id),
                    message_id: as_message_id(&message_id),
                    marked_as_done_task_ids: autosplit(&done_task_ids)
                        .iter()
                        .map(|t| tokenizer::to_integer_i32(trim_td(t)))
                        .collect(),
                    marked_as_not_done_task_ids: autosplit(&not_done_task_ids)
                        .iter()
                        .map(|t| tokenizer::to_integer_i32(trim_td(t)))
                        .collect(),
                },
            ));
            true
        } else {
            false
        }
    }

    /// cli.cpp:7650 — the member-status/profile/supergroup-toggle chain
    /// (census chain 15). The `scms` status spell carries the full
    /// administrator-rights/permissions tables.
    fn op_block_media(&self, op: &str, args: &str) -> bool {
        if op == "scms" {
            let (chat_id, member_id, status_str) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            let admin_rights = |anonymous: bool, mask: [bool; 18]| {
                as_chat_administrator_rights(
                    mask[0], mask[1], mask[2], mask[3], mask[4], mask[5], mask[6], mask[7],
                    mask[8], mask[9], mask[10], mask[11], mask[12], mask[13], mask[14], mask[15],
                    mask[16], anonymous,
                )
            };
            let status = match status_str.as_str() {
                "member" => Some(td_api::ChatMemberStatus::chatMemberStatusMember(
                    td_api::chatMemberStatusMember {
                        member_until_date: 0,
                    },
                )),
                "left" => Some(td_api::ChatMemberStatus::chatMemberStatusLeft(
                    td_api::chatMemberStatusLeft,
                )),
                "banned" => Some(td_api::ChatMemberStatus::chatMemberStatusBanned(
                    td_api::chatMemberStatusBanned {
                        banned_until_date: i32::MAX,
                    },
                )),
                "creator" => Some(td_api::ChatMemberStatus::chatMemberStatusCreator(
                    td_api::chatMemberStatusCreator {
                        is_anonymous: false,
                        is_member: true,
                    },
                )),
                "creatoranon" => Some(td_api::ChatMemberStatus::chatMemberStatusCreator(
                    td_api::chatMemberStatusCreator {
                        is_anonymous: true,
                        is_member: true,
                    },
                )),
                "uncreator" => Some(td_api::ChatMemberStatus::chatMemberStatusCreator(
                    td_api::chatMemberStatusCreator {
                        is_anonymous: false,
                        is_member: false,
                    },
                )),
                "anonadmin" => Some(td_api::ChatMemberStatus::chatMemberStatusAdministrator(
                    td_api::chatMemberStatusAdministrator {
                        can_be_edited: true,
                        rights: Some(admin_rights(true, [true; 18])),
                    },
                )),
                "anon" => {
                    let mut mask = [false; 18];
                    mask[17] = true;
                    Some(td_api::ChatMemberStatus::chatMemberStatusAdministrator(
                        td_api::chatMemberStatusAdministrator {
                            can_be_edited: false,
                            rights: Some(admin_rights(false, mask)),
                        },
                    ))
                }
                "addadmin" => {
                    let mut mask = [false; 18];
                    mask[9] = true;
                    Some(td_api::ChatMemberStatus::chatMemberStatusAdministrator(
                        td_api::chatMemberStatusAdministrator {
                            can_be_edited: false,
                            rights: Some(admin_rights(false, mask)),
                        },
                    ))
                }
                "calladmin" => {
                    let mut mask = [false; 18];
                    mask[10] = true;
                    Some(td_api::ChatMemberStatus::chatMemberStatusAdministrator(
                        td_api::chatMemberStatusAdministrator {
                            can_be_edited: false,
                            rights: Some(admin_rights(false, mask)),
                        },
                    ))
                }
                "admin" | "adminq" => {
                    let mut mask = [false; 18];
                    mask[1] = true;
                    mask[2] = true;
                    mask[3] = true;
                    mask[4] = true;
                    mask[5] = true;
                    mask[6] = true;
                    mask[7] = true;
                    mask[8] = true;
                    mask[9] = true;
                    mask[10] = true;
                    mask[11] = true;
                    mask[12] = true;
                    mask[13] = true;
                    mask[14] = true;
                    mask[15] = true;
                    mask[16] = true;
                    Some(td_api::ChatMemberStatus::chatMemberStatusAdministrator(
                        td_api::chatMemberStatusAdministrator {
                            can_be_edited: true,
                            rights: Some(admin_rights(false, mask)),
                        },
                    ))
                }
                "minadmin" => {
                    let mut mask = [false; 18];
                    mask[0] = true;
                    Some(td_api::ChatMemberStatus::chatMemberStatusAdministrator(
                        td_api::chatMemberStatusAdministrator {
                            can_be_edited: true,
                            rights: Some(admin_rights(false, mask)),
                        },
                    ))
                }
                "unadmin" => Some(td_api::ChatMemberStatus::chatMemberStatusAdministrator(
                    td_api::chatMemberStatusAdministrator {
                        can_be_edited: true,
                        rights: None,
                    },
                )),
                "rest" => {
                    let until = 120 + time::now() as i32;
                    Some(td_api::ChatMemberStatus::chatMemberStatusRestricted(
                        td_api::chatMemberStatusRestricted {
                            is_member: true,
                            restricted_until_date: until,
                            permissions: Some(chat_permissions([false; 16])),
                        },
                    ))
                }
                "restkick" => {
                    let until = 120 + time::now() as i32;
                    let mut perm = [false; 16];
                    perm[0] = true;
                    Some(td_api::ChatMemberStatus::chatMemberStatusRestricted(
                        td_api::chatMemberStatusRestricted {
                            is_member: false,
                            restricted_until_date: until,
                            permissions: Some(chat_permissions(perm)),
                        },
                    ))
                }
                "restunkick" => {
                    let until = 120 + time::now() as i32;
                    let mut perm = [false; 16];
                    perm[0] = true;
                    Some(td_api::ChatMemberStatus::chatMemberStatusRestricted(
                        td_api::chatMemberStatusRestricted {
                            is_member: true,
                            restricted_until_date: until,
                            permissions: Some(chat_permissions(perm)),
                        },
                    ))
                }
                "unrest" => Some(td_api::ChatMemberStatus::chatMemberStatusRestricted(
                    td_api::chatMemberStatusRestricted {
                        is_member: true,
                        restricted_until_date: 0,
                        permissions: Some(chat_permissions([true; 16])),
                    },
                )),
                _ => None,
            };
            if let Some(status) = status {
                self.send_request(Function::setChatMemberStatus(td_api::setChatMemberStatus {
                    chat_id: self.as_chat_id(&chat_id),
                    member_id: self.as_message_sender(&member_id),
                    status: Some(status),
                }));
            } else {
                tdutils::log_error!("Unknown status \"{}\"", status_str);
            }
            true
        } else if op == "scmt" {
            let (chat_id, user_id, tag) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            self.send_request(Function::setChatMemberTag(td_api::setChatMemberTag {
                chat_id: self.as_chat_id(&chat_id),
                user_id: self.as_user_id(&user_id),
                tag,
            }));
            true
        } else if op == "cto" {
            self.send_request(Function::canTransferOwnership(td_api::canTransferOwnership));
            true
        } else if op == "transferChatOwnership" {
            let (chat_id, user_id, password) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            self.send_request(Function::transferChatOwnership(
                td_api::transferChatOwnership {
                    chat_id: self.as_chat_id(&chat_id),
                    user_id: self.as_user_id(&user_id),
                    password,
                },
            ));
            true
        } else if op == "gcoal" {
            self.send_request(Function::getChatOwnerAfterLeaving(
                td_api::getChatOwnerAfterLeaving {
                    chat_id: self.as_chat_id(args),
                },
            ));
            true
        } else if op == "log" {
            let (chat_id, limit, user_ids) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            self.send_request(Function::getChatEventLog(td_api::getChatEventLog {
                chat_id: self.as_chat_id(&chat_id),
                query: String::new(),
                from_event_id: 0,
                limit: as_limit(&limit, 10),
                filters: None,
                user_ids: self.as_user_ids(&user_ids),
            }));
            true
        } else if op == "logf" {
            let chat_id = self.as_chat_id(args);
            self.get_log_chat_id.store(chat_id, Ordering::SeqCst);
            self.send_request(Function::getChatEventLog(td_api::getChatEventLog {
                chat_id,
                query: String::new(),
                from_event_id: 0,
                limit: 100,
                filters: None,
                user_ids: Vec::new(),
            }));
            true
        } else if op == "gtz" {
            self.send_request(Function::getTimeZones(td_api::getTimeZones));
            true
        } else if op == "join" {
            self.send_request(Function::joinChat(td_api::joinChat {
                chat_id: self.as_chat_id(args),
            }));
            true
        } else if op == "leave" {
            self.send_request(Function::leaveChat(td_api::leaveChat {
                chat_id: self.as_chat_id(args),
            }));
            true
        } else if op == "dcm" {
            let (chat_id, member_id) = tokenizer::split(args, ' ');
            let status = if self.as_user_id(&member_id) == self.my_id.load(Ordering::SeqCst) {
                td_api::ChatMemberStatus::chatMemberStatusLeft(td_api::chatMemberStatusLeft)
            } else {
                td_api::ChatMemberStatus::chatMemberStatusBanned(td_api::chatMemberStatusBanned {
                    banned_until_date: 0,
                })
            };
            self.send_request(Function::setChatMemberStatus(td_api::setChatMemberStatus {
                chat_id: self.as_chat_id(&chat_id),
                member_id: self.as_message_sender(&member_id),
                status: Some(status),
            }));
            true
        } else if op == "sn" {
            let (first_name, last_name) = tokenizer::split(args, ' ');
            self.send_request(Function::setName(td_api::setName {
                first_name,
                last_name,
            }));
            true
        } else if op == "sb" {
            self.send_request(Function::setBio(td_api::setBio {
                bio: format!("\\n{}\\n{}\\n", args, args),
            }));
            true
        } else if op == "sun" {
            self.send_request(Function::setUsername(td_api::setUsername {
                username: args.to_owned(),
            }));
            true
        } else if op == "tunia" {
            let (username, is_active) = tokenizer::split(args, ' ');
            self.send_request(Function::toggleUsernameIsActive(
                td_api::toggleUsernameIsActive {
                    username,
                    is_active: as_bool(&is_active),
                },
            ));
            true
        } else if op == "raun" {
            self.send_request(Function::reorderActiveUsernames(
                td_api::reorderActiveUsernames {
                    usernames: autosplit(args),
                },
            ));
            true
        } else if op == "sbd" {
            let (day, month, year) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            let birthdate = if tokenizer::to_integer_i32(&day) == 0 {
                None
            } else {
                Some(td_api::Birthdate {
                    day: tokenizer::to_integer_i32(&day),
                    month: tokenizer::to_integer_i32(&month),
                    year: tokenizer::to_integer_i32(&year),
                })
            };
            self.send_request(Function::setBirthdate(td_api::setBirthdate { birthdate }));
            true
        } else if op == "smpt" || op == "smptg" {
            let main_profile_tab = if op == "smptg" {
                td_api::ProfileTab::profileTabGifts(td_api::profileTabGifts)
            } else {
                td_api::ProfileTab::profileTabPosts(td_api::profileTabPosts)
            };
            self.send_request(Function::setMainProfileTab(td_api::setMainProfileTab {
                main_profile_tab: Some(main_profile_tab),
            }));
            true
        } else if op == "spec" {
            self.send_request(Function::setPersonalChat(td_api::setPersonalChat {
                chat_id: self.as_chat_id(args),
            }));
            true
        } else if op == "sese" {
            self.send_request(Function::setEmojiStatus(td_api::setEmojiStatus {
                emoji_status: None,
            }));
            true
        } else if op == "ses" {
            let (custom_emoji_id, expiration_date) = tokenizer::split(args, ' ');
            self.send_request(Function::setEmojiStatus(td_api::setEmojiStatus {
                emoji_status: Some(td_api::EmojiStatus {
                    r#type: Some(td_api::EmojiStatusType::emojiStatusTypeCustomEmoji(
                        td_api::emojiStatusTypeCustomEmoji {
                            custom_emoji_id: self.as_custom_emoji_id(&custom_emoji_id),
                        },
                    )),
                    expiration_date: tokenizer::to_integer_i32(&expiration_date),
                }),
            }));
            true
        } else if op == "sesg" {
            let (
                upgraded_gift_id,
                title,
                name,
                model_custom_emoji_id,
                symbol_custom_emoji_id,
                center_color,
                edge_color,
                symbol_color,
                text_color,
                expiration_date,
            ) = {
                let (a, t1) = tokenizer::split(args, ' ');
                let (b, t2) = tokenizer::split(&t1, ' ');
                let (c, t3) = tokenizer::split(&t2, ' ');
                let (d, t4) = tokenizer::split(&t3, ' ');
                let (e, t5) = tokenizer::split(&t4, ' ');
                let (f, t6) = tokenizer::split(&t5, ' ');
                let (g, t7) = tokenizer::split(&t6, ' ');
                let (h, t8) = tokenizer::split(&t7, ' ');
                let (i, j) = tokenizer::split(&t8, ' ');
                (a, b, c, d, e, f, g, h, i, j)
            };
            self.send_request(Function::setEmojiStatus(td_api::setEmojiStatus {
                emoji_status: Some(td_api::EmojiStatus {
                    r#type: Some(td_api::EmojiStatusType::emojiStatusTypeUpgradedGift(
                        td_api::emojiStatusTypeUpgradedGift {
                            upgraded_gift_id: tokenizer::to_integer_i64(&upgraded_gift_id),
                            gift_title: title,
                            gift_name: name,
                            model_custom_emoji_id: self.as_custom_emoji_id(&model_custom_emoji_id),
                            symbol_custom_emoji_id: self
                                .as_custom_emoji_id(&symbol_custom_emoji_id),
                            backdrop_colors: Some(td_api::UpgradedGiftBackdropColors {
                                center_color: tokenizer::to_integer_i32(&center_color),
                                edge_color: tokenizer::to_integer_i32(&edge_color),
                                symbol_color: tokenizer::to_integer_i32(&symbol_color),
                                text_color: tokenizer::to_integer_i32(&text_color),
                            }),
                        },
                    )),
                    expiration_date: tokenizer::to_integer_i32(&expiration_date),
                }),
            }));
            true
        } else if op == "thsme" {
            self.send_request(Function::toggleHasSponsoredMessagesEnabled(
                td_api::toggleHasSponsoredMessagesEnabled {
                    has_sponsored_messages_enabled: as_bool(args),
                },
            ));
            true
        } else if op == "gtes" {
            self.send_request(Function::getThemedEmojiStatuses(
                td_api::getThemedEmojiStatuses,
            ));
            true
        } else if op == "gdes" {
            self.send_request(Function::getDefaultEmojiStatuses(
                td_api::getDefaultEmojiStatuses,
            ));
            true
        } else if op == "gres" {
            self.send_request(Function::getRecentEmojiStatuses(
                td_api::getRecentEmojiStatuses,
            ));
            true
        } else if op == "guges" {
            self.send_request(Function::getUpgradedGiftEmojiStatuses(
                td_api::getUpgradedGiftEmojiStatuses,
            ));
            true
        } else if op == "cres" {
            self.send_request(Function::clearRecentEmojiStatuses(
                td_api::clearRecentEmojiStatuses,
            ));
            true
        } else if op == "gtces" {
            self.send_request(Function::getThemedChatEmojiStatuses(
                td_api::getThemedChatEmojiStatuses,
            ));
            true
        } else if op == "gdces" {
            self.send_request(Function::getDefaultChatEmojiStatuses(
                td_api::getDefaultChatEmojiStatuses,
            ));
            true
        } else if op == "gdices" {
            self.send_request(Function::getDisallowedChatEmojiStatuses(
                td_api::getDisallowedChatEmojiStatuses,
            ));
            true
        } else if op == "ccun" {
            let (chat_id, username) = tokenizer::split(args, ' ');
            self.send_request(Function::checkChatUsername(td_api::checkChatUsername {
                chat_id: self.as_chat_id(&chat_id),
                username,
            }));
            true
        } else if op == "ssgun" || op == "schun" {
            let (supergroup_id, username) = tokenizer::split(args, ' ');
            self.send_request(Function::setSupergroupUsername(
                td_api::setSupergroupUsername {
                    supergroup_id: self.as_supergroup_id(&supergroup_id),
                    username,
                },
            ));
            true
        } else if op == "tsgunia" || op == "tchunia" {
            let (supergroup_id, username, is_active) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            self.send_request(Function::toggleSupergroupUsernameIsActive(
                td_api::toggleSupergroupUsernameIsActive {
                    supergroup_id: self.as_supergroup_id(&supergroup_id),
                    username,
                    is_active: as_bool(&is_active),
                },
            ));
            true
        } else if op == "dasgun" || op == "dachun" {
            self.send_request(Function::disableAllSupergroupUsernames(
                td_api::disableAllSupergroupUsernames {
                    supergroup_id: self.as_supergroup_id(args),
                },
            ));
            true
        } else if op == "rsgaun" || op == "rchaun" {
            let (supergroup_id, _) = tokenizer::split(args, ' ');
            self.send_request(Function::reorderSupergroupActiveUsernames(
                td_api::reorderSupergroupActiveUsernames {
                    supergroup_id: self.as_supergroup_id(&supergroup_id),
                    usernames: autosplit(&tokenizer::split(args, ' ').1),
                },
            ));
            true
        } else if op == "ssgss" {
            let (supergroup_id, sticker_set_id) = tokenizer::split(args, ' ');
            self.send_request(Function::setSupergroupStickerSet(
                td_api::setSupergroupStickerSet {
                    supergroup_id: self.as_supergroup_id(&supergroup_id),
                    sticker_set_id: tokenizer::to_integer_i64(&sticker_set_id),
                },
            ));
            true
        } else if op == "ssgcess" {
            let (supergroup_id, sticker_set_id) = tokenizer::split(args, ' ');
            self.send_request(Function::setSupergroupCustomEmojiStickerSet(
                td_api::setSupergroupCustomEmojiStickerSet {
                    supergroup_id: self.as_supergroup_id(&supergroup_id),
                    custom_emoji_sticker_set_id: tokenizer::to_integer_i64(&sticker_set_id),
                },
            ));
            true
        } else if op == "ssgubc" {
            let (supergroup_id, unrestrict_boost_count) = tokenizer::split(args, ' ');
            self.send_request(Function::setSupergroupUnrestrictBoostCount(
                td_api::setSupergroupUnrestrictBoostCount {
                    supergroup_id: self.as_supergroup_id(&supergroup_id),
                    unrestrict_boost_count: tokenizer::to_integer_i32(&unrestrict_boost_count),
                },
            ));
            true
        } else if op == "tsgp" {
            let (supergroup_id, flag) = tokenizer::split(args, ' ');
            self.send_request(Function::toggleSupergroupIsAllHistoryAvailable(
                td_api::toggleSupergroupIsAllHistoryAvailable {
                    supergroup_id: self.as_supergroup_id(&supergroup_id),
                    is_all_history_available: as_bool(&flag),
                },
            ));
            true
        } else if op == "tsgchsm" {
            let (supergroup_id, flag) = tokenizer::split(args, ' ');
            self.send_request(Function::toggleSupergroupCanHaveSponsoredMessages(
                td_api::toggleSupergroupCanHaveSponsoredMessages {
                    supergroup_id: self.as_supergroup_id(&supergroup_id),
                    can_have_sponsored_messages: as_bool(&flag),
                },
            ));
            true
        } else if op == "tsghat" {
            let (supergroup_id, flag) = tokenizer::split(args, ' ');
            self.send_request(Function::toggleSupergroupHasAutomaticTranslation(
                td_api::toggleSupergroupHasAutomaticTranslation {
                    supergroup_id: self.as_supergroup_id(&supergroup_id),
                    has_automatic_translation: as_bool(&flag),
                },
            ));
            true
        } else if op == "tsghhm" {
            let (supergroup_id, flag) = tokenizer::split(args, ' ');
            self.send_request(Function::toggleSupergroupHasHiddenMembers(
                td_api::toggleSupergroupHasHiddenMembers {
                    supergroup_id: self.as_supergroup_id(&supergroup_id),
                    has_hidden_members: as_bool(&flag),
                },
            ));
            true
        } else if op == "tsgas" {
            let (supergroup_id, flag) = tokenizer::split(args, ' ');
            self.send_request(Function::toggleSupergroupHasAggressiveAntiSpamEnabled(
                td_api::toggleSupergroupHasAggressiveAntiSpamEnabled {
                    supergroup_id: self.as_supergroup_id(&supergroup_id),
                    has_aggressive_anti_spam_enabled: as_bool(&flag),
                },
            ));
            true
        } else if op == "tsgif" {
            // cpp:8173-8176 — the 1.8.70 four-arg form.
            let (supergroup_id, is_forum, has_forum_tabs, has_forum_sidebar) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, rest1) = tokenizer::split(&rest0, ' ');
                let (w2, w3) = tokenizer::split(&rest1, ' ');
                (w0, w1, w2, w3)
            };
            self.send_request(Function::toggleSupergroupIsForum(
                td_api::toggleSupergroupIsForum {
                    supergroup_id: self.as_supergroup_id(&supergroup_id),
                    is_forum: as_bool(&is_forum),
                    has_forum_tabs: as_bool(&has_forum_tabs),
                    has_forum_sidebar: as_bool(&has_forum_sidebar),
                },
            ));
            true
        } else if op == "tsibg" {
            self.send_request(Function::toggleSupergroupIsBroadcastGroup(
                td_api::toggleSupergroupIsBroadcastGroup {
                    supergroup_id: self.as_supergroup_id(args),
                },
            ));
            true
        } else if op == "ssmpt" || op == "ssmptg" {
            let main_profile_tab = if op == "ssmptg" {
                td_api::ProfileTab::profileTabGifs(td_api::profileTabGifs)
            } else {
                td_api::ProfileTab::profileTabPosts(td_api::profileTabPosts)
            };
            self.send_request(Function::setSupergroupMainProfileTab(
                td_api::setSupergroupMainProfileTab {
                    supergroup_id: self.as_supergroup_id(args),
                    main_profile_tab: Some(main_profile_tab),
                },
            ));
            true
        } else if op == "tsgsm" {
            let (supergroup_id, sign_messages, show_message_sender) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            self.send_request(Function::toggleSupergroupSignMessages(
                td_api::toggleSupergroupSignMessages {
                    supergroup_id: self.as_supergroup_id(&supergroup_id),
                    sign_messages: as_bool(&sign_messages),
                    show_message_sender: as_bool(&show_message_sender),
                },
            ));
            true
        } else if op == "tsgjtsm" {
            let (supergroup_id, join_to_send_message) = tokenizer::split(args, ' ');
            self.send_request(Function::toggleSupergroupJoinToSendMessages(
                td_api::toggleSupergroupJoinToSendMessages {
                    supergroup_id: self.as_supergroup_id(&supergroup_id),
                    join_to_send_messages: as_bool(&join_to_send_message),
                },
            ));
            true
        } else if op == "tsgjbr" || op == "tsgjbra" {
            let (supergroup_id, join_by_request, guard_bot_user_id) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            self.send_request(Function::toggleSupergroupJoinByRequest(
                td_api::toggleSupergroupJoinByRequest {
                    supergroup_id: self.as_supergroup_id(&supergroup_id),
                    join_by_request: as_bool(&join_by_request),
                    guard_bot_user_id: self.as_user_id(&guard_bot_user_id),
                    apply_to_invite_links: op == "tsgjbra",
                },
            ));
            true
        } else {
            false
        }
    }

    /// cli.cpp:7209 — the dev/deletion/chat-creation chain (census chain 13).
    fn op_block_dev_a(&self, op: &str, args: &str) -> bool {
        if op == "test" {
            self.send_request(Function::testNetwork(td_api::testNetwork));
            true
        } else if op == "alarm" {
            self.send_request(Function::setAlarm(td_api::setAlarm {
                seconds: args.trim().parse().unwrap_or(0.0),
            }));
            true
        } else if op == "delete" {
            let (chat_id, remove_from_the_chat_list, revoke) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            self.send_request(Function::deleteChatHistory(td_api::deleteChatHistory {
                chat_id: self.as_chat_id(&chat_id),
                remove_from_chat_list: as_bool(&remove_from_the_chat_list),
                revoke: as_bool(&revoke),
            }));
            true
        } else if op == "dcmbd" {
            let (chat_id, min_date, max_date, revoke) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, rest1) = tokenizer::split(&rest0, ' ');
                let (w2, w3) = tokenizer::split(&rest1, ' ');
                (w0, w1, w2, w3)
            };
            self.send_request(Function::deleteChatMessagesByDate(
                td_api::deleteChatMessagesByDate {
                    chat_id: self.as_chat_id(&chat_id),
                    min_date: tokenizer::to_integer_i32(&min_date),
                    max_date: tokenizer::to_integer_i32(&max_date),
                    revoke: as_bool(&revoke),
                },
            ));
            true
        } else if op == "dcmbs" {
            let (chat_id, sender_id) = tokenizer::split(args, ' ');
            self.send_request(Function::deleteChatMessagesBySender(
                td_api::deleteChatMessagesBySender {
                    chat_id: self.as_chat_id(&chat_id),
                    sender_id: self.as_message_sender(&sender_id),
                },
            ));
            true
        } else if op == "cnbgc" {
            let (user_ids_string, title, message_auto_delete_time) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            self.send_request(Function::createNewBasicGroupChat(
                td_api::createNewBasicGroupChat {
                    user_ids: self.as_user_ids(&user_ids_string),
                    title,
                    message_auto_delete_time: tokenizer::to_integer_i32(&message_auto_delete_time),
                },
            ));
            true
        } else if op == "cnchc"
            || op == "cnchcadt"
            || op == "cnsgc"
            || op == "cnsgcadt"
            || op == "cnfc"
            || op == "cnfcadt"
        {
            self.send_request(Function::createNewSupergroupChat(
                td_api::createNewSupergroupChat {
                    title: args.to_owned(),
                    is_forum: op == "cnfc" || op == "cnfcadt",
                    is_channel: op == "cnchc" || op == "cnchcadt",
                    description: "Description".to_owned(),
                    location: None,
                    message_auto_delete_time: if op.ends_with("adt") { 86400 } else { 0 },
                    for_import: false,
                },
            ));
            true
        } else if op == "cnsgcloc" {
            self.send_request(Function::createNewSupergroupChat(
                td_api::createNewSupergroupChat {
                    title: args.to_owned(),
                    is_forum: false,
                    is_channel: false,
                    description: "Description".to_owned(),
                    location: Some(td_api::ChatLocation {
                        location: as_location("40.0", "60.0", ""),
                        address: "address".to_owned(),
                    }),
                    message_auto_delete_time: 0,
                    for_import: false,
                },
            ));
            true
        } else if op == "cnsgcimport" {
            self.send_request(Function::createNewSupergroupChat(
                td_api::createNewSupergroupChat {
                    title: args.to_owned(),
                    is_forum: false,
                    is_channel: false,
                    description: "Description".to_owned(),
                    location: None,
                    message_auto_delete_time: 0,
                    for_import: true,
                },
            ));
            true
        } else if op == "ubgctsc" {
            self.send_request(Function::upgradeBasicGroupChatToSupergroupChat(
                td_api::upgradeBasicGroupChatToSupergroupChat {
                    chat_id: self.as_chat_id(args),
                },
            ));
            true
        } else if op == "DeleteChat" {
            self.send_request(Function::deleteChat(td_api::deleteChat {
                chat_id: self.as_chat_id(args),
            }));
            true
        } else if op == "grc" {
            self.send_request(Function::getRecommendedChats(td_api::getRecommendedChats));
            true
        } else if op == "gcsc" {
            self.send_request(Function::getChatSimilarChats(td_api::getChatSimilarChats {
                chat_id: self.as_chat_id(args),
            }));
            true
        } else if op == "gcscc" {
            let (chat_id, return_local) = tokenizer::split(args, ' ');
            self.send_request(Function::getChatSimilarChatCount(
                td_api::getChatSimilarChatCount {
                    chat_id: self.as_chat_id(&chat_id),
                    return_local: as_bool(&return_local),
                },
            ));
            true
        } else if op == "ocsc" {
            let (chat_id, similar_chat_id) = tokenizer::split(args, ' ');
            self.send_request(Function::openChatSimilarChat(td_api::openChatSimilarChat {
                chat_id: self.as_chat_id(&chat_id),
                opened_chat_id: self.as_chat_id(&similar_chat_id),
            }));
            true
        } else if op == "gbsb" {
            self.send_request(Function::getBotSimilarBots(td_api::getBotSimilarBots {
                bot_user_id: self.as_user_id(args),
            }));
            true
        } else if op == "gbsbc" {
            let (bot_user_id, return_local) = tokenizer::split(args, ' ');
            self.send_request(Function::getBotSimilarBotCount(
                td_api::getBotSimilarBotCount {
                    bot_user_id: self.as_user_id(&bot_user_id),
                    return_local: as_bool(&return_local),
                },
            ));
            true
        } else if op == "obsb" {
            let (bot_user_id, similar_bot_user_id) = tokenizer::split(args, ' ');
            self.send_request(Function::openBotSimilarBot(td_api::openBotSimilarBot {
                bot_user_id: self.as_user_id(&bot_user_id),
                opened_bot_user_id: self.as_user_id(&similar_bot_user_id),
            }));
            true
        } else if op == "gcpc" {
            self.send_request(Function::getCreatedPublicChats(
                td_api::getCreatedPublicChats { r#type: None },
            ));
            true
        } else if op == "gcpcl" {
            self.send_request(Function::getCreatedPublicChats(
                td_api::getCreatedPublicChats {
                    r#type: Some(td_api::PublicChatType::publicChatTypeIsLocationBased(
                        td_api::publicChatTypeIsLocationBased,
                    )),
                },
            ));
            true
        } else if op == "ccpcl" {
            self.send_request(Function::checkCreatedPublicChatsLimit(
                td_api::checkCreatedPublicChatsLimit { r#type: None },
            ));
            true
        } else if op == "ccpcll" {
            self.send_request(Function::checkCreatedPublicChatsLimit(
                td_api::checkCreatedPublicChatsLimit {
                    r#type: Some(td_api::PublicChatType::publicChatTypeIsLocationBased(
                        td_api::publicChatTypeIsLocationBased,
                    )),
                },
            ));
            true
        } else if op == "gsdc" {
            self.send_request(Function::getSuitableDiscussionChats(
                td_api::getSuitableDiscussionChats,
            ));
            true
        } else if op == "gisc" {
            self.send_request(Function::getInactiveSupergroupChats(
                td_api::getInactiveSupergroupChats,
            ));
            true
        } else if op == "gspc" {
            self.send_request(Function::getSuitablePersonalChats(
                td_api::getSuitablePersonalChats,
            ));
            true
        } else if op == "gscoc" {
            self.send_request(Function::getSuitableCommunityChats(
                td_api::getSuitableCommunityChats,
            ));
            true
        } else if op == "cpc" {
            let (user_id, force) = tokenizer::split(args, ' ');
            self.send_request(Function::createPrivateChat(td_api::createPrivateChat {
                user_id: self.as_user_id(&user_id),
                force: as_bool(&force),
            }));
            true
        } else if op == "cbgc" {
            let (basic_group_id, force) = tokenizer::split(args, ' ');
            self.send_request(Function::createBasicGroupChat(
                td_api::createBasicGroupChat {
                    basic_group_id: self.as_basic_group_id(&basic_group_id),
                    force: as_bool(&force),
                },
            ));
            true
        } else if op == "csgc" || op == "cchc" {
            let (supergroup_id, force) = tokenizer::split(args, ' ');
            self.send_request(Function::createSupergroupChat(
                td_api::createSupergroupChat {
                    supergroup_id: self.as_supergroup_id(&supergroup_id),
                    force: as_bool(&force),
                },
            ));
            true
        } else {
            false
        }
    }

    /// cli.cpp:7032 — the send family (census chain 12): every `s*` message
    /// command over [`CliClient::send_message`] and the input-media builders.
    fn op_block_dev_b(&self, op: &str, args: &str) -> bool {
        if op == "san" {
            let (chat_id, animation, width, height) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, rest1) = tokenizer::split(&rest0, ' ');
                let (w2, w3) = tokenizer::split(&rest1, ' ');
                (w0, w1, w2, w3)
            };
            let animation = td_api::InputAnimation {
                duration: 60,
                width: tokenizer::to_integer_i32(&width),
                height: tokenizer::to_integer_i32(&height),
                ..self.as_input_animation(&animation)
            };
            self.send_message(
                self.as_chat_id(&chat_id),
                td_api::InputMessageContent::inputMessageAnimation(td_api::inputMessageAnimation {
                    animation: Some(animation),
                    caption: self.get_caption(),
                    show_caption_above_media: self.show_caption_above_media.load(Ordering::SeqCst),
                    has_spoiler: self.has_spoiler.load(Ordering::SeqCst),
                }),
            );
            true
        } else if op == "sau" {
            let (chat_id, audio, duration, title, performer) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, rest1) = tokenizer::split(&rest0, ' ');
                let (w2, rest2) = tokenizer::split(&rest1, ' ');
                let (w3, w4) = tokenizer::split(&rest2, ' ');
                (w0, w1, w2, w3, w4)
            };
            let mut audio = self.as_input_audio(&audio, tokenizer::to_integer_i32(&duration));
            audio.title = title;
            audio.performer = performer;
            self.send_message(
                self.as_chat_id(&chat_id),
                td_api::InputMessageContent::inputMessageAudio(td_api::inputMessageAudio {
                    audio: Some(audio),
                    caption: self.get_caption(),
                }),
            );
            true
        } else if op == "svoice" {
            let (chat_id, voice_note) = tokenizer::split(args, ' ');
            self.send_message(
                self.as_chat_id(&chat_id),
                td_api::InputMessageContent::inputMessageVoiceNote(td_api::inputMessageVoiceNote {
                    voice_note: Some(self.as_input_voice_note(&voice_note)),
                    caption: self.get_caption(),
                    self_destruct_type: self.get_message_self_destruct_type(),
                }),
            );
            true
        } else if op == "scontact" {
            let (chat_id, phone_number, first_name, last_name, user_id) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, rest1) = tokenizer::split(&rest0, ' ');
                let (w2, rest2) = tokenizer::split(&rest1, ' ');
                let (w3, w4) = tokenizer::split(&rest2, ' ');
                (w0, w1, w2, w3, w4)
            };
            self.send_message(
                self.as_chat_id(&chat_id),
                td_api::InputMessageContent::inputMessageContact(td_api::inputMessageContact {
                    contact: Some(td_api::Contact {
                        phone_number,
                        first_name,
                        last_name,
                        vcard: String::new(),
                        user_id: self.as_user_id(&user_id),
                    }),
                }),
            );
            true
        } else if op == "sf" || op == "scopy" {
            let (chat_id, from_chat_id, from_message_id, replace_video_start_timestamp) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, rest1) = tokenizer::split(&rest0, ' ');
                let (w2, w3) = tokenizer::split(&rest1, ' ');
                (w0, w1, w2, w3)
            };
            let copy_options = if op == "scopy" {
                Some(td_api::MessageCopyOptions {
                    send_copy: true,
                    replace_caption: random::fast_bool(),
                    new_caption: self.get_caption(),
                    new_show_caption_above_media: self
                        .show_caption_above_media
                        .load(Ordering::SeqCst),
                })
            } else {
                None
            };
            self.send_message(
                self.as_chat_id(&chat_id),
                td_api::InputMessageContent::inputMessageForwarded(td_api::inputMessageForwarded {
                    from_chat_id: self.as_chat_id(&from_chat_id),
                    message_id: as_message_id(&from_message_id),
                    in_game_share: true,
                    replace_video_start_timestamp: as_bool(&replace_video_start_timestamp),
                    new_video_start_timestamp: self.start_timestamp.load(Ordering::SeqCst),
                    copy_options,
                }),
            );
            true
        } else if op == "sdice" || op == "sdicecd" {
            let (chat_id, emoji) = tokenizer::split(args, ' ');
            self.send_message(
                self.as_chat_id(&chat_id),
                td_api::InputMessageContent::inputMessageDice(td_api::inputMessageDice {
                    emoji,
                    clear_draft: op == "sdicecd",
                }),
            );
            true
        } else if op == "stake" {
            let (chat_id, state_hash, stake_gram_amount, clear_draft) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, rest1) = tokenizer::split(&rest0, ' ');
                let (w2, w3) = tokenizer::split(&rest1, ' ');
                (w0, w1, w2, w3)
            };
            self.send_message(
                self.as_chat_id(&chat_id),
                td_api::InputMessageContent::inputMessageStakeDice(td_api::inputMessageStakeDice {
                    state_hash,
                    stake_gram_amount: tokenizer::to_integer_i64(&stake_gram_amount),
                    clear_draft: as_bool(&clear_draft),
                }),
            );
            true
        } else if op == "sd" || op == "sdf" {
            let (chat_id, document) = tokenizer::split(args, ' ');
            self.send_message(
                self.as_chat_id(&chat_id),
                td_api::InputMessageContent::inputMessageDocument(td_api::inputMessageDocument {
                    document: Some(self.as_input_document(&document, op == "sdf")),
                    caption: self.get_caption(),
                }),
            );
            true
        } else if op == "sg" {
            let (chat_id, bot_user_id, game_short_name) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            self.send_message(
                self.as_chat_id(&chat_id),
                td_api::InputMessageContent::inputMessageGame(td_api::inputMessageGame {
                    bot_user_id: self.as_user_id(&bot_user_id),
                    game_short_name,
                }),
            );
            true
        } else if op == "sl" {
            let (chat_id, latitude, longitude, accuracy) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, rest1) = tokenizer::split(&rest0, ' ');
                let (w2, w3) = tokenizer::split(&rest1, ' ');
                (w0, w1, w2, w3)
            };
            self.send_message(
                self.as_chat_id(&chat_id),
                td_api::InputMessageContent::inputMessageLocation(td_api::inputMessageLocation {
                    location: as_location(&latitude, &longitude, &accuracy),
                }),
            );
            true
        } else if op == "sll" {
            let (chat_id, period, latitude, longitude, accuracy, heading, proximity_alert_radius) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, rest1) = tokenizer::split(&rest0, ' ');
                let (w2, rest2) = tokenizer::split(&rest1, ' ');
                let (w3, rest3) = tokenizer::split(&rest2, ' ');
                let (w4, rest4) = tokenizer::split(&rest3, ' ');
                let (w5, w6) = tokenizer::split(&rest4, ' ');
                (w0, w1, w2, w3, w4, w5, w6)
            };
            self.send_message(
                self.as_chat_id(&chat_id),
                td_api::InputMessageContent::inputMessageLiveLocation(
                    td_api::inputMessageLiveLocation {
                        location: Some(self.as_live_location(
                            &latitude,
                            &longitude,
                            &accuracy,
                            tokenizer::to_integer_i32(&period),
                            tokenizer::to_integer_i32(&heading),
                            tokenizer::to_integer_i32(&proximity_alert_radius),
                        )),
                    },
                ),
            );
            true
        } else if op == "spoll" || op == "spollp" || op == "squiz" {
            let (chat_id, question, tail) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            let options = autosplit(&tail)
                .iter()
                .map(|option| {
                    let media = if option.starts_with("https://") {
                        Some(td_api::InputPollMedia::inputPollMediaLink(
                            td_api::inputPollMediaLink {
                                url: option.clone(),
                            },
                        ))
                    } else {
                        None
                    };
                    td_api::InputPollOption {
                        text: Some(self.as_formatted_text(option)),
                        media,
                    }
                })
                .collect::<Vec<_>>();
            let poll_type = if op == "squiz" {
                let mut correct_option_ids = vec![0i32];
                if options.len() > 1 {
                    correct_option_ids.push(options.len() as i32 - 1);
                }
                td_api::InputPollType::inputPollTypeQuiz(td_api::inputPollTypeQuiz {
                    correct_option_ids,
                    explanation: Some(self.as_formatted_text("_te*st*_")),
                    explanation_media: None,
                })
            } else {
                td_api::InputPollType::inputPollTypeRegular(td_api::inputPollTypeRegular {
                    allow_adding_options: random::fast_bool() && op == "spollp",
                })
            };
            let can_restrict = self
                .channel_ids
                .lock()
                .unwrap()
                .contains(&self.as_chat_id(&chat_id));
            let country_codes = if random::fast_bool() && can_restrict {
                vec!["US".to_owned()]
            } else {
                Vec::new()
            };
            self.send_message(
                self.as_chat_id(&chat_id),
                td_api::InputMessageContent::inputMessagePoll(td_api::inputMessagePoll {
                    question: Some(self.as_formatted_text(&question)),
                    options,
                    description: self.get_caption(),
                    media: None,
                    is_anonymous: op != "spollp",
                    allows_multiple_answers: random::fast_bool(),
                    allows_revoting: random::fast_bool(),
                    members_only: random::fast_bool() && can_restrict,
                    country_codes,
                    shuffle_options: random::fast_bool(),
                    hide_results_until_closes: random::fast_bool(),
                    r#type: Some(poll_type),
                    open_period: 0,
                    close_date: 0,
                    is_closed: false,
                }),
            );
            true
        } else if op == "schl" {
            let (chat_id, tail) = tokenizer::split(args, ' ');
            let (title, tasks) = tokenizer::split(&tail, ' ');
            let mut count = 0i32;
            let tasks = autosplit(&tasks)
                .iter()
                .map(|task| {
                    count += 1;
                    td_api::InputChecklistTask {
                        id: count,
                        text: Some(self.as_formatted_text(task)),
                    }
                })
                .collect();
            self.send_message(
                self.as_chat_id(&chat_id),
                td_api::InputMessageContent::inputMessageChecklist(td_api::inputMessageChecklist {
                    checklist: Some(td_api::InputChecklist {
                        title: Some(self.as_formatted_text(&title)),
                        tasks,
                        others_can_add_tasks: random::fast_bool(),
                        others_can_mark_tasks_as_done: random::fast_bool(),
                    }),
                }),
            );
            true
        } else if op == "sp" {
            let (chat_id, photo) = tokenizer::split(args, ' ');
            self.send_message(
                self.as_chat_id(&chat_id),
                td_api::InputMessageContent::inputMessagePhoto(td_api::inputMessagePhoto {
                    photo: Some(self.as_input_photo(&photo)),
                    caption: self.get_caption(),
                    show_caption_above_media: self.show_caption_above_media.load(Ordering::SeqCst),
                    self_destruct_type: self.get_message_self_destruct_type(),
                    has_spoiler: self.has_spoiler.load(Ordering::SeqCst),
                }),
            );
            true
        } else if op == "ss" {
            let (chat_id, sticker, emoji) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            self.send_message(
                self.as_chat_id(&chat_id),
                td_api::InputMessageContent::inputMessageSticker(td_api::inputMessageSticker {
                    sticker: Some(self.as_input_sticker(&sticker)),
                    emoji,
                }),
            );
            true
        } else if op == "sstory" {
            let (chat_id, story_poster_chat_id, story_id) = {
                let (w0, rest0) = tokenizer::split(args, ' ');
                let (w1, w2) = tokenizer::split(&rest0, ' ');
                (w0, w1, w2)
            };
            self.send_message(
                self.as_chat_id(&chat_id),
                td_api::InputMessageContent::inputMessageStory(td_api::inputMessageStory {
                    story_poster_chat_id: self.as_chat_id(&story_poster_chat_id),
                    story_id: tokenizer::to_integer_i32(trim_td(&story_id)),
                }),
            );
            true
        } else if op == "sv" {
            let (chat_id, video) = tokenizer::split(args, ' ');
            self.send_message(
                self.as_chat_id(&chat_id),
                td_api::InputMessageContent::inputMessageVideo(td_api::inputMessageVideo {
                    video: Some(self.as_input_video(&video)),
                    caption: self.get_caption(),
                    show_caption_above_media: self.show_caption_above_media.load(Ordering::SeqCst),
                    self_destruct_type: self.get_message_self_destruct_type(),
                    has_spoiler: self.has_spoiler.load(Ordering::SeqCst),
                }),
            );
            true
        } else if op == "svn" {
            let (chat_id, video_note) = tokenizer::split(args, ' ');
            self.send_message(
                self.as_chat_id(&chat_id),
                td_api::InputMessageContent::inputMessageVideoNote(td_api::inputMessageVideoNote {
                    video_note: Some(self.as_input_video_note(&video_note)),
                    self_destruct_type: self.get_message_self_destruct_type(),
                }),
            );
            true
        } else if op == "svenue" {
            let (
                chat_id,
                latitude,
                longitude,
                accuracy,
                title,
                address,
                provider,
                venue_id,
                venue_type,
            ) = {
                let (a, t1) = tokenizer::split(args, ' ');
                let (b, t2) = tokenizer::split(&t1, ' ');
                let (c, t3) = tokenizer::split(&t2, ' ');
                let (d, t4) = tokenizer::split(&t3, ' ');
                let (e, t5) = tokenizer::split(&t4, ' ');
                let (f, t6) = tokenizer::split(&t5, ' ');
                let (g, t7) = tokenizer::split(&t6, ' ');
                let (h, i) = tokenizer::split(&t7, ' ');
                (a, b, c, d, e, f, g, h, i)
            };
            self.send_message(
                self.as_chat_id(&chat_id),
                td_api::InputMessageContent::inputMessageVenue(td_api::inputMessageVenue {
                    venue: Some(td_api::Venue {
                        location: as_location(&latitude, &longitude, &accuracy),
                        title,
                        address,
                        provider,
                        id: venue_id,
                        r#type: venue_type,
                    }),
                }),
            );
            true
        } else {
            false
        }
    }

    /// cli.cpp:7966 — the chat-settings/business/profile chain.
    fn op_block_dev_c(&self, op: &str, args: &str) -> bool {
        if op == "scar" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (max_reaction_count, available_reactions) = tokenizer::split(&rest0, ' ');
            let chat_available_reactions = if available_reactions == "all" {
                Some(td_api::ChatAvailableReactions::chatAvailableReactionsAll(
                    td_api::chatAvailableReactionsAll {
                        max_reaction_count: tokenizer::to_integer_i32(&max_reaction_count),
                    },
                ))
            } else if !available_reactions.is_empty() {
                let reactions = autosplit(&available_reactions)
                    .iter()
                    .filter_map(|r| as_reaction_type(r))
                    .collect();
                Some(td_api::ChatAvailableReactions::chatAvailableReactionsSome(
                    td_api::chatAvailableReactionsSome {
                        reactions,
                        max_reaction_count: tokenizer::to_integer_i32(&max_reaction_count),
                    },
                ))
            } else {
                None
            };
            self.send_request(Function::setChatAvailableReactions(
                td_api::setChatAvailableReactions {
                    chat_id: self.as_chat_id(&chat_id),
                    available_reactions: chat_available_reactions,
                },
            ));
            true
        } else if op == "scd" {
            let (chat_id, description) = tokenizer::split(args, ' ');
            self.send_request(Function::setChatDescription(td_api::setChatDescription {
                chat_id: self.as_chat_id(&chat_id),
                description,
            }));
            true
        } else if op == "scdg" {
            let (chat_id, group_chat_id) = tokenizer::split(args, ' ');
            self.send_request(Function::setChatDiscussionGroup(
                td_api::setChatDiscussionGroup {
                    chat_id: self.as_chat_id(&chat_id),
                    discussion_chat_id: self.as_chat_id(&group_chat_id),
                },
            ));
            true
        } else if op == "scdmg" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (is_enabled, paid_message_star_count) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::setChatDirectMessagesGroup(
                td_api::setChatDirectMessagesGroup {
                    chat_id: self.as_chat_id(&chat_id),
                    is_enabled: as_bool(&is_enabled),
                    paid_message_star_count: tokenizer::to_integer_i64(&paid_message_star_count),
                },
            ));
            true
        } else if op == "scl" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (latitude, longitude) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::setChatLocation(td_api::setChatLocation {
                chat_id: self.as_chat_id(&chat_id),
                location: Some(td_api::ChatLocation {
                    location: as_location(&latitude, &longitude, ""),
                    address: "address".to_owned(),
                }),
            }));
            true
        } else if op == "scsmd" {
            let (chat_id, slow_mode_delay) = tokenizer::split(args, ' ');
            self.send_request(Function::setChatSlowModeDelay(
                td_api::setChatSlowModeDelay {
                    chat_id: self.as_chat_id(&chat_id),
                    slow_mode_delay: tokenizer::to_integer_i32(&slow_mode_delay),
                },
            ));
            true
        } else if op == "pcm" || op == "pcms" || op == "pcmo" {
            let (chat_id, message_id) = tokenizer::split(args, ' ');
            let chat_id = self.as_chat_id(&chat_id);
            let message_id = as_message_id(&message_id);
            let business = self.business_connection_id.lock().unwrap().clone();
            if !business.is_empty() {
                self.send_request(Function::setBusinessMessageIsPinned(
                    td_api::setBusinessMessageIsPinned {
                        business_connection_id: business,
                        chat_id,
                        message_id,
                        is_pinned: true,
                    },
                ));
            } else {
                self.send_request(Function::pinChatMessage(td_api::pinChatMessage {
                    chat_id,
                    message_id,
                    disable_notification: op == "pcms",
                    only_for_self: op == "pcmo",
                }));
            }
            true
        } else if op == "upcm" {
            let (chat_id, message_id) = tokenizer::split(args, ' ');
            let chat_id = self.as_chat_id(&chat_id);
            let message_id = as_message_id(&message_id);
            let business = self.business_connection_id.lock().unwrap().clone();
            if !business.is_empty() {
                self.send_request(Function::setBusinessMessageIsPinned(
                    td_api::setBusinessMessageIsPinned {
                        business_connection_id: business,
                        chat_id,
                        message_id,
                        is_pinned: false,
                    },
                ));
            } else {
                self.send_request(Function::unpinChatMessage(td_api::unpinChatMessage {
                    chat_id,
                    message_id,
                }));
            }
            true
        } else if op == "uacm" {
            self.send_request(Function::unpinAllChatMessages(
                td_api::unpinAllChatMessages {
                    chat_id: self.as_chat_id(args),
                },
            ));
            true
        } else if op == "uaftm" {
            let (chat_id, forum_topic_id) = tokenizer::split(args, ' ');
            self.send_request(Function::unpinAllForumTopicMessages(
                td_api::unpinAllForumTopicMessages {
                    chat_id: self.as_chat_id(&chat_id),
                    forum_topic_id: as_forum_topic_id(&forum_topic_id),
                },
            ));
            true
        } else if op == "rbm" {
            let (chat_id, message_id) = tokenizer::split(args, ' ');
            self.send_request(Function::readBusinessMessage(td_api::readBusinessMessage {
                business_connection_id: self.business_connection_id.lock().unwrap().clone(),
                chat_id: self.as_chat_id(&chat_id),
                message_id: as_message_id(&message_id),
            }));
            true
        } else if op == "dbm" {
            self.send_request(Function::deleteBusinessMessages(
                td_api::deleteBusinessMessages {
                    business_connection_id: self.business_connection_id.lock().unwrap().clone(),
                    message_ids: as_message_ids(args),
                },
            ));
            true
        } else if op == "ebsp" || op == "ebsv" {
            let (story_poster_chat_id, rest0) = tokenizer::split(args, ' ');
            let (story_id, rest1) = tokenizer::split(&rest0, ' ');
            let (story, rest2) = tokenizer::split(&rest1, ' ');
            let (rules, areas) = tokenizer::split(&rest2, ' ');
            let content = if op == "ebsp" {
                td_api::InputStoryContent::inputStoryContentPhoto(td_api::inputStoryContentPhoto {
                    photo: as_input_file(&story),
                    added_sticker_file_ids: self.get_added_sticker_file_ids(),
                })
            } else {
                td_api::InputStoryContent::inputStoryContentVideo(td_api::inputStoryContentVideo {
                    video: as_input_file(&story),
                    added_sticker_file_ids: self.get_added_sticker_file_ids(),
                    duration: 10.0,
                    cover_frame_timestamp: 0.5,
                    is_animation: true,
                })
            };
            self.send_request(Function::editBusinessStory(td_api::editBusinessStory {
                story_poster_chat_id: self.as_chat_id(&story_poster_chat_id),
                story_id: as_story_id(&story_id),
                content: Some(content),
                areas: get_input_story_areas(&areas),
                caption: self.get_caption(),
                privacy_settings: Some(self.get_story_privacy_settings(&rules)),
            }));
            true
        } else if op == "dbs" {
            self.send_request(Function::deleteBusinessStory(td_api::deleteBusinessStory {
                business_connection_id: self.business_connection_id.lock().unwrap().clone(),
                story_id: as_story_id(args),
            }));
            true
        } else if op == "sban" {
            let (first_name, last_name) = tokenizer::split(args, ' ');
            self.send_request(Function::setBusinessAccountName(
                td_api::setBusinessAccountName {
                    business_connection_id: self.business_connection_id.lock().unwrap().clone(),
                    first_name,
                    last_name,
                },
            ));
            true
        } else if op == "sbab" {
            self.send_request(Function::setBusinessAccountBio(
                td_api::setBusinessAccountBio {
                    business_connection_id: self.business_connection_id.lock().unwrap().clone(),
                    bio: args.to_owned(),
                },
            ));
            true
        } else if op == "sbau" {
            self.send_request(Function::setBusinessAccountUsername(
                td_api::setBusinessAccountUsername {
                    business_connection_id: self.business_connection_id.lock().unwrap().clone(),
                    username: args.to_owned(),
                },
            ));
            true
        } else if op == "sbapp" || op == "sbappf" {
            self.send_request(Function::setBusinessAccountProfilePhoto(
                td_api::setBusinessAccountProfilePhoto {
                    business_connection_id: self.business_connection_id.lock().unwrap().clone(),
                    photo: self.parse_input_chat_photo(args),
                    is_public: op == "sbappf",
                },
            ));
            true
        } else if op == "sbags" {
            let (show_button, rest0) = tokenizer::split(args, ' ');
            let (unlimited_gifts, rest1) = tokenizer::split(&rest0, ' ');
            let (limited_gifts, rest2) = tokenizer::split(&rest1, ' ');
            let (upgraded_gifts, rest3) = tokenizer::split(&rest2, ' ');
            let (gifts_from_channels, premium_subscription) = tokenizer::split(&rest3, ' ');
            self.send_request(Function::setBusinessAccountGiftSettings(
                td_api::setBusinessAccountGiftSettings {
                    business_connection_id: self.business_connection_id.lock().unwrap().clone(),
                    settings: Some(td_api::GiftSettings {
                        show_gift_button: as_bool(&show_button),
                        accepted_gift_types: Some(td_api::AcceptedGiftTypes {
                            unlimited_gifts: as_bool(&unlimited_gifts),
                            limited_gifts: as_bool(&limited_gifts),
                            upgraded_gifts: as_bool(&upgraded_gifts),
                            gifts_from_channels: as_bool(&gifts_from_channels),
                            premium_subscription: as_bool(&premium_subscription),
                        }),
                    }),
                },
            ));
            true
        } else if op == "gbasa" {
            self.send_request(Function::getBusinessAccountStarAmount(
                td_api::getBusinessAccountStarAmount {
                    business_connection_id: self.business_connection_id.lock().unwrap().clone(),
                },
            ));
            true
        } else if op == "tbas" {
            self.send_request(Function::transferBusinessAccountStars(
                td_api::transferBusinessAccountStars {
                    business_connection_id: self.business_connection_id.lock().unwrap().clone(),
                    star_count: tokenizer::to_integer_i64(trim_td(args)),
                },
            ));
            true
        } else if op == "grib" {
            self.send_request(Function::getRecentInlineBots(td_api::getRecentInlineBots));
            true
        } else if op == "gob" {
            self.send_request(Function::getOwnedBots(td_api::getOwnedBots));
            true
        } else if op == "spc" || op == "su" {
            self.send_request(Function::searchPublicChat(td_api::searchPublicChat {
                username: args.to_owned(),
            }));
            true
        } else if op == "spcs" || op == "spcsb" || op == "spcsc" {
            self.send_request(Function::searchPublicChats(td_api::searchPublicChats {
                query: args.to_owned(),
                type_filter: Self::as_search_chat_type_filter(op),
            }));
            true
        } else if op == "sc" || op == "scb" || op == "scc" {
            let (query, limit) = Self::parse_search_query(args);
            self.send_request(Function::searchChats(td_api::searchChats {
                query,
                type_filter: Self::as_search_chat_type_filter(op),
                limit,
            }));
            true
        } else if op == "scos" || op == "scosb" || op == "scosc" {
            let (query, limit) = Self::parse_search_query(args);
            self.send_request(Function::searchChatsOnServer(td_api::searchChatsOnServer {
                query,
                type_filter: Self::as_search_chat_type_filter(op),
                limit,
            }));
            true
        } else if op == "sbl" {
            let (latitude, longitude) = tokenizer::split(args, ' ');
            let location = if latitude.is_empty() {
                None
            } else {
                Some(td_api::BusinessLocation {
                    location: if longitude.is_empty() {
                        None
                    } else {
                        as_location(&latitude, &longitude, "")
                    },
                    address: "business address".to_owned(),
                })
            };
            self.send_request(Function::setBusinessLocation(td_api::setBusinessLocation {
                location,
            }));
            true
        } else if op == "sboh" {
            let (time_zone_id, opening_hours) = tokenizer::split(args, ' ');
            let opening_hours = if time_zone_id.is_empty() {
                None
            } else {
                let mut minutes = to_integers_i32(&opening_hours);
                if minutes.len() % 2 == 1 {
                    minutes.push(8 * 24 * 60);
                }
                let intervals = minutes
                    .chunks(2)
                    .map(|pair| td_api::BusinessOpeningHoursInterval {
                        start_minute: pair[0],
                        end_minute: pair[1],
                    })
                    .collect();
                Some(td_api::BusinessOpeningHours {
                    time_zone_id,
                    opening_hours: intervals,
                })
            };
            self.send_request(Function::setBusinessOpeningHours(
                td_api::setBusinessOpeningHours { opening_hours },
            ));
            true
        } else if op == "sbgms" {
            let (shortcut_id, rest0) = tokenizer::split(args, ' ');
            let (chat_ids, inactivity_days) = tokenizer::split(&rest0, ' ');
            let shortcut_id = self.as_shortcut_id(&shortcut_id);
            let settings = if shortcut_id == 0 {
                None
            } else {
                Some(td_api::BusinessGreetingMessageSettings {
                    shortcut_id,
                    recipients: Some(self.as_business_recipients(&chat_ids)),
                    inactivity_days: tokenizer::to_integer_i32(&inactivity_days),
                })
            };
            self.send_request(Function::setBusinessGreetingMessageSettings(
                td_api::setBusinessGreetingMessageSettings {
                    greeting_message_settings: settings,
                },
            ));
            true
        } else if op == "sbams" || op == "sbamso" {
            let (shortcut_id, rest0) = tokenizer::split(args, ' ');
            let (chat_ids, schedule) = tokenizer::split(&rest0, ' ');
            let shortcut_id = self.as_shortcut_id(&shortcut_id);
            let settings = if shortcut_id == 0 {
                None
            } else {
                let first = schedule.as_bytes().first().copied();
                let schedule_object = if first == Some(b'a') {
                    Some(
                        td_api::BusinessAwayMessageSchedule::businessAwayMessageScheduleAlways(
                            td_api::businessAwayMessageScheduleAlways,
                        ),
                    )
                } else if first == Some(b'o') {
                    Some(td_api::BusinessAwayMessageSchedule::businessAwayMessageScheduleOutsideOfOpeningHours(
                        td_api::businessAwayMessageScheduleOutsideOfOpeningHours,
                    ))
                } else {
                    let start_date = tokenizer::to_integer_i32(&schedule);
                    Some(
                        td_api::BusinessAwayMessageSchedule::businessAwayMessageScheduleCustom(
                            td_api::businessAwayMessageScheduleCustom {
                                start_date,
                                end_date: start_date + random::fast(1000, 100000),
                            },
                        ),
                    )
                };
                Some(td_api::BusinessAwayMessageSettings {
                    shortcut_id,
                    recipients: Some(self.as_business_recipients(&chat_ids)),
                    schedule: schedule_object,
                    offline_only: op == "sbamso",
                })
            };
            self.send_request(Function::setBusinessAwayMessageSettings(
                td_api::setBusinessAwayMessageSettings {
                    away_message_settings: settings,
                },
            ));
            true
        } else if op == "sbsp" {
            let (title, rest0) = tokenizer::split(args, ' ');
            let (message, sticker) = tokenizer::split(&rest0, ' ');
            let start_page = if title.is_empty() {
                None
            } else {
                Some(td_api::InputBusinessStartPage {
                    title,
                    message,
                    sticker: as_input_file(&sticker),
                })
            };
            self.send_request(Function::setBusinessStartPage(
                td_api::setBusinessStartPage { start_page },
            ));
            true
        } else if op == "gbcb" {
            self.send_request(Function::getBusinessConnectedBot(
                td_api::getBusinessConnectedBot,
            ));
            true
        } else if op == "sbcb" {
            let (bot_user_id, rest0) = tokenizer::split(args, ' ');
            let (chat_ids, rest1) = tokenizer::split(&rest0, ' ');
            let (can_reply, rest2) = tokenizer::split(&rest1, ' ');
            let (can_read_messages, rest3) = tokenizer::split(&rest2, ' ');
            let (can_delete_outgoing_messages, rest4) = tokenizer::split(&rest3, ' ');
            let (can_delete_incoming_messages, rest5) = tokenizer::split(&rest4, ' ');
            let (can_edit_name, rest6) = tokenizer::split(&rest5, ' ');
            let (can_edit_bio, rest7) = tokenizer::split(&rest6, ' ');
            let (can_edit_profile_photo, rest8) = tokenizer::split(&rest7, ' ');
            let (can_edit_username, rest9) = tokenizer::split(&rest8, ' ');
            let (can_view_gifts, rest10) = tokenizer::split(&rest9, ' ');
            let (can_sell_gifts, rest11) = tokenizer::split(&rest10, ' ');
            let (can_change_gift_settings, rest12) = tokenizer::split(&rest11, ' ');
            let (can_transfer_and_upgrade_gifts, rest13) = tokenizer::split(&rest12, ' ');
            let (can_transfer_stars, can_manage_stories) = tokenizer::split(&rest13, ' ');
            self.send_request(Function::setBusinessConnectedBot(
                td_api::setBusinessConnectedBot {
                    bot: Some(td_api::BusinessConnectedBot {
                        bot_user_id: self.as_user_id(&bot_user_id),
                        recipients: Some(self.as_business_recipients(&chat_ids)),
                        rights: Some(td_api::BusinessBotRights {
                            can_reply: as_bool(&can_reply),
                            can_read_messages: as_bool(&can_read_messages),
                            can_delete_sent_messages: as_bool(&can_delete_outgoing_messages),
                            can_delete_all_messages: as_bool(&can_delete_incoming_messages),
                            can_edit_name: as_bool(&can_edit_name),
                            can_edit_bio: as_bool(&can_edit_bio),
                            can_edit_profile_photo: as_bool(&can_edit_profile_photo),
                            can_edit_username: as_bool(&can_edit_username),
                            can_view_gifts_and_stars: as_bool(&can_view_gifts),
                            can_sell_gifts: as_bool(&can_sell_gifts),
                            can_change_gift_settings: as_bool(&can_change_gift_settings),
                            can_transfer_and_upgrade_gifts: as_bool(
                                &can_transfer_and_upgrade_gifts,
                            ),
                            can_transfer_stars: as_bool(&can_transfer_stars),
                            can_manage_stories: as_bool(&can_manage_stories),
                        }),
                    }),
                },
            ));
            true
        } else if op == "tbcbcip" {
            let (chat_id, is_paused) = tokenizer::split(args, ' ');
            self.send_request(Function::toggleBusinessConnectedBotChatIsPaused(
                td_api::toggleBusinessConnectedBotChatIsPaused {
                    chat_id: self.as_chat_id(&chat_id),
                    is_paused: as_bool(&is_paused),
                },
            ));
            true
        } else if op == "rbcbfc" {
            self.send_request(Function::removeBusinessConnectedBotFromChat(
                td_api::removeBusinessConnectedBotFromChat {
                    chat_id: self.as_chat_id(args),
                },
            ));
            true
        } else if op == "cbcb" {
            self.send_request(Function::confirmBusinessConnectedBot(
                td_api::confirmBusinessConnectedBot {
                    bot_user_id: self.as_user_id(args),
                },
            ));
            true
        } else if op == "dbcb" {
            self.send_request(Function::deleteBusinessConnectedBot(
                td_api::deleteBusinessConnectedBot {
                    bot_user_id: self.as_user_id(args),
                },
            ));
            true
        } else if op == "gbcl" {
            self.send_request(Function::getBusinessChatLinks(td_api::getBusinessChatLinks));
            true
        } else if op == "cbcl" {
            let (text, title) = tokenizer::split(args, ' ');
            self.send_request(Function::createBusinessChatLink(
                td_api::createBusinessChatLink {
                    link_info: Some(td_api::InputBusinessChatLink {
                        text: Some(self.as_formatted_text(&text)),
                        title,
                    }),
                },
            ));
            true
        } else if op == "ebcl" {
            let (link, rest0) = tokenizer::split(args, ' ');
            let (text, title) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::editBusinessChatLink(
                td_api::editBusinessChatLink {
                    link,
                    link_info: Some(td_api::InputBusinessChatLink {
                        text: Some(self.as_formatted_text(&text)),
                        title,
                    }),
                },
            ));
            true
        } else if op == "dbcl" {
            self.send_request(Function::deleteBusinessChatLink(
                td_api::deleteBusinessChatLink {
                    link: args.to_owned(),
                },
            ));
            true
        } else if op == "gbcli" {
            self.send_request(Function::getBusinessChatLinkInfo(
                td_api::getBusinessChatLinkInfo {
                    link_name: args.to_owned(),
                },
            ));
            true
        } else if op == "gbc" {
            let connection_id = if args.is_empty() {
                self.business_connection_id.lock().unwrap().clone()
            } else {
                args.to_owned()
            };
            self.send_request(Function::getBusinessConnection(
                td_api::getBusinessConnection { connection_id },
            ));
            true
        } else if op == "sco" {
            let (query, limit) = Self::parse_search_query(args);
            self.send_request(Function::searchContacts(td_api::searchContacts {
                query,
                limit,
            }));
            true
        } else if op == "srfcs" || op == "srfcsb" || op == "srfcsc" {
            let (query, limit) = Self::parse_search_query(args);
            self.send_request(Function::searchRecentlyFoundChats(
                td_api::searchRecentlyFoundChats {
                    query,
                    type_filter: Self::as_search_chat_type_filter(op),
                    limit,
                },
            ));
            true
        } else if op == "arfc" {
            self.send_request(Function::addRecentlyFoundChat(
                td_api::addRecentlyFoundChat {
                    chat_id: self.as_chat_id(args),
                },
            ));
            true
        } else if op == "rrfc" {
            self.send_request(Function::removeRecentlyFoundChat(
                td_api::removeRecentlyFoundChat {
                    chat_id: self.as_chat_id(args),
                },
            ));
            true
        } else if op == "crfcs" {
            self.send_request(Function::clearRecentlyFoundChats(
                td_api::clearRecentlyFoundChats,
            ));
            true
        } else if op == "groc" {
            self.send_request(Function::getRecentlyOpenedChats(
                td_api::getRecentlyOpenedChats {
                    limit: as_limit(args, 10),
                },
            ));
            true
        } else if op == "glp" {
            self.send_request(Function::getLinkPreview(td_api::getLinkPreview {
                text: Some(self.as_formatted_text(args)),
                link_preview_options: self.get_link_preview_options(),
            }));
            true
        } else if op == "gwpiv" {
            let (url, only_local) = tokenizer::split(args, ' ');
            self.send_request(Function::getWebPageInstantView(
                td_api::getWebPageInstantView {
                    url,
                    only_local: as_bool(&only_local),
                },
            ));
            true
        } else if op == "spp" || op == "spppf" {
            self.send_request(Function::setProfilePhoto(td_api::setProfilePhoto {
                photo: self.parse_input_chat_photo(args),
                is_public: op == "spppf",
            }));
            true
        } else if op == "suppp" {
            let (user_id, photo) = tokenizer::split(args, ' ');
            self.send_request(Function::setUserPersonalProfilePhoto(
                td_api::setUserPersonalProfilePhoto {
                    user_id: self.as_user_id(&user_id),
                    photo: self.parse_input_chat_photo(&photo),
                },
            ));
            true
        } else if op == "sunote" {
            let (user_id, note) = tokenizer::split(args, ' ');
            let note = if note.is_empty() {
                self.caption.lock().unwrap().clone()
            } else {
                note
            };
            self.send_request(Function::setUserNote(td_api::setUserNote {
                user_id: self.as_user_id(&user_id),
                note: Some(self.as_formatted_text(&note)),
            }));
            true
        } else if op == "supp" {
            let (user_id, photo) = tokenizer::split(args, ' ');
            self.send_request(Function::suggestUserProfilePhoto(
                td_api::suggestUserProfilePhoto {
                    user_id: self.as_user_id(&user_id),
                    photo: self.parse_input_chat_photo(&photo),
                },
            ));
            true
        } else if op == "sub" {
            let (user_id, rest0) = tokenizer::split(args, ' ');
            let (day, rest1) = tokenizer::split(&rest0, ' ');
            let (month, year) = tokenizer::split(&rest1, ' ');
            let day = tokenizer::to_integer_i32(&day);
            self.send_request(Function::suggestUserBirthdate(
                td_api::suggestUserBirthdate {
                    user_id: self.as_user_id(&user_id),
                    birthdate: if day == 0 {
                        None
                    } else {
                        Some(td_api::Birthdate {
                            day,
                            month: tokenizer::to_integer_i32(&month),
                            year: tokenizer::to_integer_i32(&year),
                        })
                    },
                },
            ));
            true
        } else if op == "tbcmes" {
            let (user_id, can_manage_emoji_status) = tokenizer::split(args, ' ');
            self.send_request(Function::toggleBotCanManageEmojiStatus(
                td_api::toggleBotCanManageEmojiStatus {
                    bot_user_id: self.as_user_id(&user_id),
                    can_manage_emoji_status: as_bool(&can_manage_emoji_status),
                },
            ));
            true
        } else if op == "cbsm" {
            self.send_request(Function::canBotSendMessages(td_api::canBotSendMessages {
                bot_user_id: self.as_user_id(args),
            }));
            true
        } else if op == "abtsm" {
            self.send_request(Function::allowBotToSendMessages(
                td_api::allowBotToSendMessages {
                    bot_user_id: self.as_user_id(args),
                },
            ));
            true
        } else {
            false
        }
    }

    /// cli.cpp:8346 — the bot-management/notification/statistics chain.
    fn op_block_stats(&self, op: &str, args: &str) -> bool {
        if op == "swacr" {
            let (bot_user_id, rest0) = tokenizer::split(args, ' ');
            let (method, parameters) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::sendWebAppCustomRequest(
                td_api::sendWebAppCustomRequest {
                    bot_user_id: self.as_user_id(&bot_user_id),
                    method,
                    parameters,
                },
            ));
            true
        } else if op == "gbmp" {
            self.send_request(Function::getBotMediaPreviews(td_api::getBotMediaPreviews {
                bot_user_id: self.as_user_id(args),
            }));
            true
        } else if op == "gbmpi" {
            let (bot_user_id, language_code) = tokenizer::split(args, ' ');
            self.send_request(Function::getBotMediaPreviewInfo(
                td_api::getBotMediaPreviewInfo {
                    bot_user_id: self.as_user_id(&bot_user_id),
                    language_code,
                },
            ));
            true
        } else if op == "abmpp" || op == "abmpv" {
            let (bot_user_id, rest0) = tokenizer::split(args, ' ');
            let (language_code, media) = tokenizer::split(&rest0, ' ');
            let content = if op == "abmpp" {
                td_api::InputStoryContent::inputStoryContentPhoto(td_api::inputStoryContentPhoto {
                    photo: as_input_file(&media),
                    added_sticker_file_ids: self.get_added_sticker_file_ids(),
                })
            } else {
                td_api::InputStoryContent::inputStoryContentVideo(td_api::inputStoryContentVideo {
                    video: as_input_file(&media),
                    added_sticker_file_ids: self.get_added_sticker_file_ids(),
                    duration: 0.0,
                    cover_frame_timestamp: 1.5,
                    is_animation: true,
                })
            };
            self.send_request(Function::addBotMediaPreview(td_api::addBotMediaPreview {
                bot_user_id: self.as_user_id(&bot_user_id),
                language_code,
                content: Some(content),
            }));
            true
        } else if op == "ebmpp" || op == "ebmpv" {
            let (bot_user_id, rest0) = tokenizer::split(args, ' ');
            let (language_code, rest1) = tokenizer::split(&rest0, ' ');
            let (file_id, media) = tokenizer::split(&rest1, ' ');
            let content = if op == "ebmpp" {
                td_api::InputStoryContent::inputStoryContentPhoto(td_api::inputStoryContentPhoto {
                    photo: as_input_file(&media),
                    added_sticker_file_ids: self.get_added_sticker_file_ids(),
                })
            } else {
                td_api::InputStoryContent::inputStoryContentVideo(td_api::inputStoryContentVideo {
                    video: as_input_file(&media),
                    added_sticker_file_ids: self.get_added_sticker_file_ids(),
                    duration: 0.0,
                    cover_frame_timestamp: 1.5,
                    is_animation: true,
                })
            };
            self.send_request(Function::editBotMediaPreview(td_api::editBotMediaPreview {
                bot_user_id: self.as_user_id(&bot_user_id),
                language_code,
                file_id: tokenizer::to_integer_i32(&file_id),
                content: Some(content),
            }));
            true
        } else if op == "rbmp" || op == "dbmp" {
            let (bot_user_id, rest0) = tokenizer::split(args, ' ');
            let (language_code, file_ids) = tokenizer::split(&rest0, ' ');
            let file_ids = as_file_ids(&file_ids);
            if op == "rbmp" {
                self.send_request(Function::reorderBotMediaPreviews(
                    td_api::reorderBotMediaPreviews {
                        bot_user_id: self.as_user_id(&bot_user_id),
                        language_code,
                        file_ids,
                    },
                ));
            } else {
                self.send_request(Function::deleteBotMediaPreviews(
                    td_api::deleteBotMediaPreviews {
                        bot_user_id: self.as_user_id(&bot_user_id),
                        language_code,
                        file_ids,
                    },
                ));
            }
            true
        } else if op == "cbun" || op == "cbun2" {
            self.send_request(Function::checkBotUsername(td_api::checkBotUsername {
                username: args.to_owned(),
                is_secondary: op == "cbun2",
            }));
            true
        } else if op == "absun" {
            let (bot_user_id, username) = tokenizer::split(args, ' ');
            self.send_request(Function::addBotSecondaryUsername(
                td_api::addBotSecondaryUsername {
                    bot_user_id: self.as_user_id(&bot_user_id),
                    username: username.to_owned(),
                },
            ));
            true
        } else if op == "dbsun" {
            self.send_request(Function::deleteBotSecondaryUsername(
                td_api::deleteBotSecondaryUsername {
                    bot_user_id: self.as_user_id(args),
                },
            ));
            true
        } else if op == "cb" || op == "cbl" {
            let (manager_bot_user_id, rest0) = tokenizer::split(args, ' ');
            let (name, username) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::createBot(td_api::createBot {
                manager_bot_user_id: self.as_user_id(&manager_bot_user_id),
                name,
                username,
                via_link: op == "cbl",
            }));
            true
        } else if op == "gbi" {
            let (bot_user_id, language_code) = tokenizer::split(args, ' ');
            let bot_user_id = self.as_user_id(&bot_user_id);
            self.send_request(Function::getBotName(td_api::getBotName {
                bot_user_id,
                language_code: language_code.clone(),
            }));
            self.send_request(Function::getBotInfoDescription(
                td_api::getBotInfoDescription {
                    bot_user_id,
                    language_code: language_code.clone(),
                },
            ));
            self.send_request(Function::getBotInfoShortDescription(
                td_api::getBotInfoShortDescription {
                    bot_user_id,
                    language_code,
                },
            ));
            true
        } else if op == "sbit" {
            let (bot_user_id, rest0) = tokenizer::split(args, ' ');
            let (language_code, rest1) = tokenizer::split(&rest0, ' ');
            let (name, rest2) = tokenizer::split(&rest1, ' ');
            let (description, short_description) = tokenizer::split(&rest2, ' ');
            let bot_user_id = self.as_user_id(&bot_user_id);
            self.send_request(Function::setBotName(td_api::setBotName {
                bot_user_id,
                language_code: language_code.clone(),
                name,
            }));
            self.send_request(Function::setBotInfoDescription(
                td_api::setBotInfoDescription {
                    bot_user_id,
                    language_code: language_code.clone(),
                    description,
                },
            ));
            self.send_request(Function::setBotInfoShortDescription(
                td_api::setBotInfoShortDescription {
                    bot_user_id,
                    language_code,
                    short_description,
                },
            ));
            true
        } else if op == "sbn" {
            let (bot_user_id, rest0) = tokenizer::split(args, ' ');
            let (language_code, name) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::setBotName(td_api::setBotName {
                bot_user_id: self.as_user_id(&bot_user_id),
                language_code,
                name,
            }));
            true
        } else if op == "gbn" {
            let (bot_user_id, language_code) = tokenizer::split(args, ' ');
            self.send_request(Function::getBotName(td_api::getBotName {
                bot_user_id: self.as_user_id(&bot_user_id),
                language_code,
            }));
            true
        } else if op == "sbpp" {
            let (bot_user_id, photo) = tokenizer::split(args, ' ');
            self.send_request(Function::setBotProfilePhoto(td_api::setBotProfilePhoto {
                bot_user_id: self.as_user_id(&bot_user_id),
                photo: self.parse_input_chat_photo(&photo),
            }));
            true
        } else if op == "tbunia" {
            let (bot_user_id, rest0) = tokenizer::split(args, ' ');
            let (username, is_active) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::toggleBotUsernameIsActive(
                td_api::toggleBotUsernameIsActive {
                    bot_user_id: self.as_user_id(&bot_user_id),
                    username,
                    is_active: as_bool(&is_active),
                },
            ));
            true
        } else if op == "rbaun" {
            let (bot_user_id, usernames) = tokenizer::split(args, ' ');
            self.send_request(Function::reorderBotActiveUsernames(
                td_api::reorderBotActiveUsernames {
                    bot_user_id: self.as_user_id(&bot_user_id),
                    usernames: autosplit(&usernames),
                },
            ));
            true
        } else if op == "sbid" {
            let (bot_user_id, rest0) = tokenizer::split(args, ' ');
            let (language_code, description) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::setBotInfoDescription(
                td_api::setBotInfoDescription {
                    bot_user_id: self.as_user_id(&bot_user_id),
                    language_code,
                    description,
                },
            ));
            true
        } else if op == "gbid" {
            let (bot_user_id, language_code) = tokenizer::split(args, ' ');
            self.send_request(Function::getBotInfoDescription(
                td_api::getBotInfoDescription {
                    bot_user_id: self.as_user_id(&bot_user_id),
                    language_code,
                },
            ));
            true
        } else if op == "sbisd" {
            let (bot_user_id, rest0) = tokenizer::split(args, ' ');
            let (language_code, short_description) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::setBotInfoShortDescription(
                td_api::setBotInfoShortDescription {
                    bot_user_id: self.as_user_id(&bot_user_id),
                    language_code,
                    short_description,
                },
            ));
            true
        } else if op == "gbisd" {
            let (bot_user_id, language_code) = tokenizer::split(args, ' ');
            self.send_request(Function::getBotInfoShortDescription(
                td_api::getBotInfoShortDescription {
                    bot_user_id: self.as_user_id(&bot_user_id),
                    language_code,
                },
            ));
            true
        } else if op == "smsbv" {
            let (bot_user_id, rest0) = tokenizer::split(args, ' ');
            let (sender_id, custom_description) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::setMessageSenderBotVerification(
                td_api::setMessageSenderBotVerification {
                    bot_user_id: self.as_user_id(&bot_user_id),
                    verified_id: self.as_message_sender(&sender_id),
                    custom_description: Some(self.as_formatted_text(&custom_description)),
                },
            ));
            true
        } else if op == "rmsbv" {
            let (bot_user_id, sender_id) = tokenizer::split(args, ' ');
            self.send_request(Function::removeMessageSenderBotVerification(
                td_api::removeMessageSenderBotVerification {
                    bot_user_id: self.as_user_id(&bot_user_id),
                    verified_id: self.as_message_sender(&sender_id),
                },
            ));
            true
        } else if op == "sh" {
            self.send_request(Function::searchHashtags(td_api::searchHashtags {
                prefix: args.to_owned(),
                limit: 10,
            }));
            true
        } else if op == "rrh" {
            self.send_request(Function::removeRecentHashtag(td_api::removeRecentHashtag {
                hashtag: args.to_owned(),
            }));
            true
        } else if op == "view"
            || op == "viewh"
            || op == "viewft"
            || op == "viewt"
            || op == "views"
            || op == "viewf"
        {
            let (chat_id, message_ids) = tokenizer::split(args, ' ');
            let source = match op {
                "viewh" => Some(td_api::MessageSource::messageSourceChatHistory(
                    td_api::messageSourceChatHistory,
                )),
                "viewft" => Some(td_api::MessageSource::messageSourceForumTopicHistory(
                    td_api::messageSourceForumTopicHistory,
                )),
                "viewt" => Some(td_api::MessageSource::messageSourceMessageThreadHistory(
                    td_api::messageSourceMessageThreadHistory,
                )),
                "viewf" => Some(
                    td_api::MessageSource::messageSourceDirectMessagesChatTopicHistory(
                        td_api::messageSourceDirectMessagesChatTopicHistory,
                    ),
                ),
                "views" => Some(td_api::MessageSource::messageSourceScreenshot(
                    td_api::messageSourceScreenshot,
                )),
                _ => None,
            };
            self.send_request(Function::viewMessages(td_api::viewMessages {
                chat_id: self.as_chat_id(&chat_id),
                message_ids: as_message_ids(&message_ids),
                source,
                force_read: true,
            }));
            true
        } else if op == "omc" {
            let (chat_id, message_id) = tokenizer::split(args, ' ');
            self.send_request(Function::openMessageContent(td_api::openMessageContent {
                chat_id: self.as_chat_id(&chat_id),
                message_id: as_message_id(&message_id),
            }));
            true
        } else if op == "caem" {
            let (chat_id, message_id) = tokenizer::split(args, ' ');
            self.send_request(Function::clickAnimatedEmojiMessage(
                td_api::clickAnimatedEmojiMessage {
                    chat_id: self.as_chat_id(&chat_id),
                    message_id: as_message_id(&message_id),
                },
            ));
            true
        } else if op == "lta" {
            let (file_id, duration) = tokenizer::split(args, ' ');
            self.send_request(Function::listenToAudio(td_api::listenToAudio {
                audio_file_id: tokenizer::to_integer_i32(&file_id),
                duration: tokenizer::to_integer_i32(&duration),
            }));
            true
        } else if op == "smvm" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (message_id, rest1) = tokenizer::split(&rest0, ' ');
            let (time_in_view_ms, rest2) = tokenizer::split(&rest1, ' ');
            let (active_time_in_view_ms, rest3) = tokenizer::split(&rest2, ' ');
            let (height_ratio, seen_range_ratio) = tokenizer::split(&rest3, ' ');
            self.send_request(Function::sendMessageViewMetrics(
                td_api::sendMessageViewMetrics {
                    chat_id: self.as_chat_id(&chat_id),
                    message_id: as_message_id(&message_id),
                    time_in_view_ms: tokenizer::to_integer_i32(&time_in_view_ms),
                    active_time_in_view_ms: tokenizer::to_integer_i32(&active_time_in_view_ms),
                    height_to_viewport_ratio_per_mille: tokenizer::to_integer_i32(&height_ratio),
                    seen_range_ratio_per_mille: tokenizer::to_integer_i32(&seen_range_ratio),
                },
            ));
            true
        } else if op == "gilt" {
            self.send_request(Function::getInternalLinkType(td_api::getInternalLinkType {
                link: args.to_owned(),
            }));
            true
        } else if op == "geli" {
            self.send_request(Function::getExternalLinkInfo(td_api::getExternalLinkInfo {
                link: args.to_owned(),
            }));
            true
        } else if op == "gel" || op == "gelw" {
            self.send_request(Function::getExternalLink(td_api::getExternalLink {
                link: args.to_owned(),
                allow_write_access: op == "gelw",
            }));
            true
        } else if op == "glwbt" {
            self.send_request(Function::getLinkWebBrowserType(
                td_api::getLinkWebBrowserType {
                    link: args.to_owned(),
                },
            ));
            true
        } else if op == "goli" {
            let (link, in_app_origin) = tokenizer::split(args, ' ');
            self.send_request(Function::getOauthLinkInfo(td_api::getOauthLinkInfo {
                url: link,
                in_app_origin,
            }));
            true
        } else if op == "cormc" {
            let (link, match_code) = tokenizer::split(args, ' ');
            self.send_request(Function::checkOauthRequestMatchCode(
                td_api::checkOauthRequestMatchCode {
                    url: link,
                    match_code,
                },
            ));
            true
        } else if op == "aor" || op == "aorw" || op == "aorp" {
            let (link, match_code) = tokenizer::split(args, ' ');
            self.send_request(Function::acceptOauthRequest(td_api::acceptOauthRequest {
                url: link,
                match_code,
                allow_write_access: op == "aorw",
                allow_phone_number_access: op == "aorp",
                // cpp:8837 — the CLI passes no TON Connect data.
                ton_connect_connect_data: None,
            }));
            true
        } else if op == "dor" {
            self.send_request(Function::declineOauthRequest(td_api::declineOauthRequest {
                url: args.to_owned(),
            }));
            true
        } else if op == "racm" {
            self.send_request(Function::readAllChatMentions(td_api::readAllChatMentions {
                chat_id: self.as_chat_id(args),
            }));
            true
        } else if op == "raftm" {
            let (chat_id, forum_topic_id) = tokenizer::split(args, ' ');
            self.send_request(Function::readAllForumTopicMentions(
                td_api::readAllForumTopicMentions {
                    chat_id: self.as_chat_id(&chat_id),
                    forum_topic_id: as_forum_topic_id(&forum_topic_id),
                },
            ));
            true
        } else if op == "racr" {
            self.send_request(Function::readAllChatReactions(
                td_api::readAllChatReactions {
                    chat_id: self.as_chat_id(args),
                },
            ));
            true
        } else if op == "racpv" {
            self.send_request(Function::readAllChatPollVotes(
                td_api::readAllChatPollVotes {
                    chat_id: self.as_chat_id(args),
                },
            ));
            true
        } else if op == "raftr" {
            let (chat_id, forum_topic_id) = tokenizer::split(args, ' ');
            self.send_request(Function::readAllForumTopicReactions(
                td_api::readAllForumTopicReactions {
                    chat_id: self.as_chat_id(&chat_id),
                    forum_topic_id: as_forum_topic_id(&forum_topic_id),
                },
            ));
            true
        } else if op == "raftpv" {
            let (chat_id, forum_topic_id) = tokenizer::split(args, ' ');
            self.send_request(Function::readAllForumTopicPollVotes(
                td_api::readAllForumTopicPollVotes {
                    chat_id: self.as_chat_id(&chat_id),
                    forum_topic_id: as_forum_topic_id(&forum_topic_id),
                },
            ));
            true
        } else if op == "tre" {
            self.send_request(Function::testReturnError(td_api::testReturnError {
                error: if args.is_empty() {
                    None
                } else {
                    Some(td_api::Error {
                        code: -1,
                        message: args.to_owned(),
                    })
                },
            }));
            true
        } else if op == "dpp" {
            self.send_request(Function::deleteProfilePhoto(td_api::deleteProfilePhoto {
                profile_photo_id: tokenizer::to_integer_i64(trim_td(args)),
            }));
            true
        } else if op == "sac" {
            let (accent_color_id, background_custom_emoji_id) = tokenizer::split(args, ' ');
            self.send_request(Function::setAccentColor(td_api::setAccentColor {
                accent_color_id: tokenizer::to_integer_i32(&accent_color_id),
                background_custom_emoji_id: self.as_custom_emoji_id(&background_custom_emoji_id),
            }));
            true
        } else if op == "sugc" {
            self.send_request(Function::setUpgradedGiftColors(
                td_api::setUpgradedGiftColors {
                    upgraded_gift_colors_id: tokenizer::to_integer_i64(trim_td(args)),
                },
            ));
            true
        } else if op == "spac" {
            let (profile_accent_color_id, profile_background_custom_emoji_id) =
                tokenizer::split(args, ' ');
            self.send_request(Function::setProfileAccentColor(
                td_api::setProfileAccentColor {
                    profile_accent_color_id: tokenizer::to_integer_i32(&profile_accent_color_id),
                    profile_background_custom_emoji_id: self
                        .as_custom_emoji_id(&profile_background_custom_emoji_id),
                },
            ));
            true
        } else if op == "gns" {
            self.send_request(Function::getSavedNotificationSound(
                td_api::getSavedNotificationSound {
                    notification_sound_id: tokenizer::to_integer_i64(trim_td(args)),
                },
            ));
            true
        } else if op == "gnss" {
            self.send_request(Function::getSavedNotificationSounds(
                td_api::getSavedNotificationSounds,
            ));
            true
        } else if op == "asns" {
            self.send_request(Function::addSavedNotificationSound(
                td_api::addSavedNotificationSound {
                    sound: as_input_file(args),
                },
            ));
            true
        } else if op == "rns" {
            self.send_request(Function::removeSavedNotificationSound(
                td_api::removeSavedNotificationSound {
                    notification_sound_id: tokenizer::to_integer_i64(trim_td(args)),
                },
            ));
            true
        } else if op == "gcnse" || op == "gcnses" {
            self.send_request(Function::getChatNotificationSettingsExceptions(
                td_api::getChatNotificationSettingsExceptions {
                    scope: as_notification_settings_scope(args),
                    compare_sound: op == "gcnses",
                },
            ));
            true
        } else if op == "gsns" {
            self.send_request(Function::getScopeNotificationSettings(
                td_api::getScopeNotificationSettings {
                    scope: as_notification_settings_scope(args),
                },
            ));
            true
        } else if op == "scns" || op == "ssns" || op == "sftns" || op == "scons" {
            let (scope, rest0) = tokenizer::split(args, ' ');
            let (mute_for, rest1) = tokenizer::split(&rest0, ' ');
            let (sound_id, rest2) = tokenizer::split(&rest1, ' ');
            let (show_preview, rest3) = tokenizer::split(&rest2, ' ');
            let (mute_stories, rest4) = tokenizer::split(&rest3, ' ');
            let (story_sound_id, rest5) = tokenizer::split(&rest4, ' ');
            let (hide_story_poster, rest6) = tokenizer::split(&rest5, ' ');
            let (disable_pinned_message_notifications, disable_mention_notifications) =
                tokenizer::split(&rest6, ' ');
            let sound_id = tokenizer::to_integer_i64(&sound_id);
            let story_sound_id = tokenizer::to_integer_i64(&story_sound_id);
            if op == "ssns" {
                self.send_request(Function::setScopeNotificationSettings(
                    td_api::setScopeNotificationSettings {
                        scope: as_notification_settings_scope(&scope),
                        notification_settings: Some(td_api::ScopeNotificationSettings {
                            mute_for: tokenizer::to_integer_i32(&mute_for),
                            sound_id,
                            show_preview: as_bool(&show_preview),
                            use_default_mute_stories: mute_stories.is_empty(),
                            mute_stories: as_bool(&mute_stories),
                            story_sound_id,
                            // C++ passes `as_bool(hide_story_poster)` here
                            // (the ctor's 7th argument) — keep the quirk.
                            show_story_poster: as_bool(&hide_story_poster),
                            disable_pinned_message_notifications: as_bool(
                                &disable_pinned_message_notifications,
                            ),
                            disable_mention_notifications: as_bool(&disable_mention_notifications),
                        }),
                    },
                ));
            } else {
                let settings = td_api::ChatNotificationSettings {
                    use_default_mute_for: mute_for.is_empty(),
                    mute_for: tokenizer::to_integer_i32(&mute_for),
                    use_default_sound: sound_id == -1,
                    sound_id,
                    use_default_show_preview: show_preview.is_empty(),
                    show_preview: as_bool(&show_preview),
                    use_default_mute_stories: mute_stories.is_empty(),
                    mute_stories: as_bool(&mute_stories),
                    use_default_story_sound: story_sound_id == -1,
                    story_sound_id,
                    use_default_show_story_poster: hide_story_poster.is_empty(),
                    show_story_poster: as_bool(&hide_story_poster),
                    use_default_disable_pinned_message_notifications:
                        disable_pinned_message_notifications.is_empty(),
                    disable_pinned_message_notifications: as_bool(
                        &disable_pinned_message_notifications,
                    ),
                    use_default_disable_mention_notifications: disable_mention_notifications
                        .is_empty(),
                    disable_mention_notifications: as_bool(&disable_mention_notifications),
                };
                if op == "scns" {
                    self.send_request(Function::setChatNotificationSettings(
                        td_api::setChatNotificationSettings {
                            chat_id: self.as_chat_id(&scope),
                            notification_settings: Some(settings),
                        },
                    ));
                } else if op == "scons" {
                    // cli.cpp:8938-8940 (99302f0e2) — the community form
                    // over the raw id (as_community_id).
                    self.send_request(Function::setCommunityNotificationSettings(
                        td_api::setCommunityNotificationSettings {
                            community_id: tokenizer::to_integer_i64(trim_td(&scope)),
                            notification_settings: Some(settings),
                        },
                    ));
                } else {
                    let (chat_id, forum_topic_id) = tokenizer::split(&scope, ',');
                    self.send_request(Function::setForumTopicNotificationSettings(
                        td_api::setForumTopicNotificationSettings {
                            chat_id: self.as_chat_id(&chat_id),
                            forum_topic_id: as_forum_topic_id(&forum_topic_id),
                            notification_settings: Some(settings),
                        },
                    ));
                }
            }
            true
        } else if op == "srns" {
            let (message_reactions, rest0) = tokenizer::split(args, ' ');
            let (story_reactions, rest1) = tokenizer::split(&rest0, ' ');
            let (poll_votes, rest2) = tokenizer::split(&rest1, ' ');
            let (sound_id, show_preview) = tokenizer::split(&rest2, ' ');
            self.send_request(Function::setReactionNotificationSettings(
                td_api::setReactionNotificationSettings {
                    notification_settings: Some(td_api::ReactionNotificationSettings {
                        message_reaction_source: reaction_notification_source(trim_td(
                            &message_reactions,
                        )),
                        story_reaction_source: reaction_notification_source(trim_td(
                            &story_reactions,
                        )),
                        poll_vote_source: reaction_notification_source(trim_td(&poll_votes)),
                        sound_id: tokenizer::to_integer_i64(&sound_id),
                        show_preview: as_bool(&show_preview),
                    }),
                },
            ));
            true
        } else if op == "rans" {
            self.send_request(Function::resetAllNotificationSettings(
                td_api::resetAllNotificationSettings,
            ));
            true
        } else if op == "rn" {
            let (group_id, notification_ids) = tokenizer::split(args, ' ');
            let group_id = tokenizer::to_integer_i32(&group_id);
            for notification_id in to_integers_i32(&notification_ids) {
                self.send_request(Function::removeNotification(td_api::removeNotification {
                    notification_group_id: group_id,
                    notification_id,
                }));
            }
            true
        } else if op == "rng" {
            let (group_id, max_notification_id) = tokenizer::split(args, ' ');
            self.send_request(Function::removeNotificationGroup(
                td_api::removeNotificationGroup {
                    notification_group_id: tokenizer::to_integer_i32(&group_id),
                    max_notification_id: tokenizer::to_integer_i32(&max_notification_id),
                },
            ));
            true
        } else if op == "rcab" {
            self.send_request(Function::removeChatActionBar(td_api::removeChatActionBar {
                chat_id: self.as_chat_id(args),
            }));
            true
        } else if op == "rc" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (option_id, rest1) = tokenizer::split(&rest0, ' ');
            let (message_ids, text) = tokenizer::split(&rest1, ' ');
            self.send_request(Function::reportChat(td_api::reportChat {
                chat_id: self.as_chat_id(&chat_id),
                option_id: option_id.into_bytes(),
                message_ids: as_message_ids(&message_ids),
                text,
            }));
            true
        } else if op == "rcp" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (file_id, rest1) = tokenizer::split(&rest0, ' ');
            let (reason, text) = tokenizer::split(&rest1, ' ');
            self.send_request(Function::reportChatPhoto(td_api::reportChatPhoto {
                chat_id: self.as_chat_id(&chat_id),
                file_id: tokenizer::to_integer_i32(&file_id),
                reason: as_report_reason(&reason),
                text,
            }));
            true
        } else if op == "reportmr" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (message_id, sender_id) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::reportMessageReactions(
                td_api::reportMessageReactions {
                    chat_id: self.as_chat_id(&chat_id),
                    message_id: as_message_id(&message_id),
                    sender_id: self.as_message_sender(&sender_id),
                },
            ));
            true
        } else if op == "gcst" {
            let (chat_id, is_dark) = tokenizer::split(args, ' ');
            self.send_request(Function::getChatStatistics(td_api::getChatStatistics {
                chat_id: self.as_chat_id(&chat_id),
                is_dark: as_bool(&is_dark),
            }));
            true
        } else if op == "gcrst" {
            let (chat_id, is_dark) = tokenizer::split(args, ' ');
            self.send_request(Function::getChatRevenueStatistics(
                td_api::getChatRevenueStatistics {
                    chat_id: self.as_chat_id(&chat_id),
                    is_dark: as_bool(&is_dark),
                },
            ));
            true
        } else if op == "gcrwu" {
            let (chat_id, password) = tokenizer::split(args, ' ');
            self.send_request(Function::getChatRevenueWithdrawalUrl(
                td_api::getChatRevenueWithdrawalUrl {
                    chat_id: self.as_chat_id(&chat_id),
                    password,
                },
            ));
            true
        } else if op == "gcrt" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (offset, limit) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::getChatRevenueTransactions(
                td_api::getChatRevenueTransactions {
                    chat_id: self.as_chat_id(&chat_id),
                    offset,
                    limit: as_limit(&limit, 10),
                },
            ));
            true
        } else if op == "gtta" || op == "gtti" || op == "gtto" {
            let (limit, offset) = tokenizer::split(args, ' ');
            let direction = if op == "gtti" {
                Some(td_api::TransactionDirection::transactionDirectionIncoming(
                    td_api::transactionDirectionIncoming,
                ))
            } else if op == "gtto" {
                Some(td_api::TransactionDirection::transactionDirectionOutgoing(
                    td_api::transactionDirectionOutgoing,
                ))
            } else {
                // C++ `gtta` leaves the direction null.
                None
            };
            self.send_request(Function::getTonTransactions(td_api::getTonTransactions {
                direction,
                offset,
                limit: as_limit(&limit, 10),
            }));
            true
        } else if op == "gsrs" {
            let (owner_id, is_dark) = tokenizer::split(args, ' ');
            self.send_request(Function::getStarRevenueStatistics(
                td_api::getStarRevenueStatistics {
                    owner_id: self.as_message_sender(&owner_id),
                    is_dark: as_bool(&is_dark),
                },
            ));
            true
        } else if op == "gswu" {
            let (owner_id, rest0) = tokenizer::split(args, ' ');
            let (star_count, password) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::getStarWithdrawalUrl(
                td_api::getStarWithdrawalUrl {
                    owner_id: self.as_message_sender(&owner_id),
                    star_count: tokenizer::to_integer_i64(&star_count),
                    password,
                },
            ));
            true
        } else if op == "gsaau" {
            self.send_request(Function::getStarAdAccountUrl(td_api::getStarAdAccountUrl {
                owner_id: self.as_message_sender(args),
            }));
            true
        } else if op == "ggrs" {
            self.send_request(Function::getGramRevenueStatistics(
                td_api::getGramRevenueStatistics {
                    is_dark: as_bool(args),
                },
            ));
            true
        } else if op == "ggwu" {
            self.send_request(Function::getGramWithdrawalUrl(
                td_api::getGramWithdrawalUrl {
                    password: args.to_owned(),
                },
            ));
            true
        } else {
            false
        }
    }

    /// cli.cpp:8985 — `q` (the rest of the block — `dnq`, `alog`/`aloge`,
    /// `fatal`, `unreachable` — lands in M.14.6).
    /// cli.cpp:8797 — the game scores/statistics/proxy/logging/dev chain and
    /// the quit block.
    fn op_block_quit(&self, op: &str, args: &str) -> bool {
        if op == "sgs" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (message_id, rest1) = tokenizer::split(&rest0, ' ');
            let (user_id, score) = tokenizer::split(&rest1, ' ');
            self.send_request(Function::setGameScore(td_api::setGameScore {
                chat_id: self.as_chat_id(&chat_id),
                message_id: as_message_id(&message_id),
                edit_message: true,
                user_id: self.as_user_id(&user_id),
                score: tokenizer::to_integer_i32(&score),
                force: true,
            }));
            true
        } else if op == "gghs" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (message_id, user_id) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::getGameHighScores(td_api::getGameHighScores {
                chat_id: self.as_chat_id(&chat_id),
                message_id: as_message_id(&message_id),
                user_id: self.as_user_id(&user_id),
            }));
            true
        } else if op == "gmst" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (message_id, is_dark) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::getMessageStatistics(
                td_api::getMessageStatistics {
                    chat_id: self.as_chat_id(&chat_id),
                    message_id: as_message_id(&message_id),
                    is_dark: as_bool(&is_dark),
                },
            ));
            true
        } else if op == "gsst" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (story_id, is_dark) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::getStoryStatistics(td_api::getStoryStatistics {
                chat_id: self.as_chat_id(&chat_id),
                story_id: as_story_id(&story_id),
                is_dark: as_bool(&is_dark),
            }));
            true
        } else if op == "gstg" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (token, x) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::getStatisticalGraph(td_api::getStatisticalGraph {
                chat_id: self.as_chat_id(&chat_id),
                token,
                x: tokenizer::to_integer_i64(&x),
            }));
            true
        } else if op == "hsa" {
            self.send_request(Function::hideSuggestedAction(td_api::hideSuggestedAction {
                action: self.as_suggested_action(args),
            }));
            true
        } else if op == "hccb" {
            self.send_request(Function::hideContactCloseBirthdays(
                td_api::hideContactCloseBirthdays,
            ));
            true
        } else if op == "glui" || op == "glu" || op == "glua" {
            let (chat_id, rest0) = tokenizer::split(args, ' ');
            let (message_id, button_id) = tokenizer::split(&rest0, ' ');
            let chat_id = self.as_chat_id(&chat_id);
            let message_id = as_message_id(&message_id);
            let button_id = tokenizer::to_integer_i64(&button_id);
            if op == "glui" {
                self.send_request(Function::getLoginUrlInfo(td_api::getLoginUrlInfo {
                    chat_id,
                    message_id,
                    button_id,
                }));
            } else {
                self.send_request(Function::getLoginUrl(td_api::getLoginUrl {
                    chat_id,
                    message_id,
                    button_id,
                    allow_write_access: op == "glua",
                }));
            }
            true
        } else if op == "suwb" || op == "suwbc" {
            let (source, rest0) = tokenizer::split(args, ' ');
            let (button_id, shared_user_ids) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::shareUsersWithBot(td_api::shareUsersWithBot {
                source: Some(self.get_keyboard_button_source(&source)),
                button_id: tokenizer::to_integer_i32(&button_id),
                shared_user_ids: self.as_user_ids(&shared_user_ids),
                only_check: op == "suwbc",
            }));
            true
        } else if op == "scwb" || op == "scwbc" {
            let (source, rest0) = tokenizer::split(args, ' ');
            let (button_id, shared_chat_id) = tokenizer::split(&rest0, ' ');
            self.send_request(Function::shareChatWithBot(td_api::shareChatWithBot {
                source: Some(self.get_keyboard_button_source(&source)),
                button_id: tokenizer::to_integer_i32(&button_id),
                shared_chat_id: self.as_chat_id(&shared_chat_id),
                only_check: op == "scwbc",
            }));
            true
        } else if op == "rsgs" {
            let (supergroup_id, message_ids) = tokenizer::split(args, ' ');
            self.send_request(Function::reportSupergroupSpam(
                td_api::reportSupergroupSpam {
                    supergroup_id: self.as_supergroup_id(&supergroup_id),
                    message_ids: as_message_ids(&message_ids),
                },
            ));
            true
        } else if op == "rsgasfp" {
            let (supergroup_id, message_id) = tokenizer::split(args, ' ');
            self.send_request(Function::reportSupergroupAntiSpamFalsePositive(
                td_api::reportSupergroupAntiSpamFalsePositive {
                    supergroup_id: self.as_supergroup_id(&supergroup_id),
                    message_id: as_message_id(&message_id),
                },
            ));
            true
        } else if op == "gdiff" {
            self.send_request(Function::testGetDifference(td_api::testGetDifference));
            true
        } else if op == "dproxy" {
            self.send_request(Function::disableProxy(td_api::disableProxy));
            true
        } else if op == "eproxy" {
            self.send_request(Function::enableProxy(td_api::enableProxy {
                proxy_id: as_proxy_id(args),
            }));
            true
        } else if op == "rproxy" {
            self.send_request(Function::removeProxy(td_api::removeProxy {
                proxy_id: as_proxy_id(args),
            }));
            true
        } else if op == "aproxy"
            || op == "aeproxy"
            || op == "aeproxytcp"
            || op == "editproxy"
            || op == "editeproxy"
            || op == "editeproxytcp"
            || op == "tproxy"
            || op == "pproxy"
            || op == "pproxyp"
        {
            let mut spell = args.to_owned();
            let mut proxy_id = String::new();
            if op.starts_with('e') {
                let (id, rest) = tokenizer::split(&spell, ' ');
                proxy_id = id;
                spell = rest;
            }
            let (server, rest0) = tokenizer::split(&spell, ' ');
            let (port, rest1) = tokenizer::split(&rest0, ' ');
            let (user, password) = tokenizer::split(&rest1, ' ');
            let port = tokenizer::to_integer_i32(&port);
            let enable = op != "aproxy" && op != "editproxy";
            let type_ = if !user.is_empty() && password.is_empty() {
                td_api::ProxyType::proxyTypeMtproto(td_api::proxyTypeMtproto { secret: user })
            } else if port == 80 || port == 8080 {
                td_api::ProxyType::proxyTypeHttp(td_api::proxyTypeHttp {
                    username: user,
                    password,
                    // C++: `op.back() != 'p'`.
                    http_only: !op.ends_with('p'),
                })
            } else {
                td_api::ProxyType::proxyTypeSocks5(td_api::proxyTypeSocks5 {
                    username: user,
                    password,
                })
            };
            let proxy = td_api::Proxy {
                server,
                port,
                r#type: Some(type_),
            };
            let comment = if random::fast_bool() {
                random::fast(0, 10).to_string()
            } else {
                String::new()
            };
            if op.starts_with('e') {
                self.send_request(Function::editProxy(td_api::editProxy {
                    proxy_id: as_proxy_id(&proxy_id),
                    proxy: Some(proxy),
                    enable,
                    comment,
                }));
            } else if op == "pproxy" || op == "pproxyp" {
                self.send_request(Function::pingProxy(td_api::pingProxy {
                    proxy: Some(proxy),
                }));
            } else if op == "tproxy" {
                self.send_request(Function::testProxy(td_api::testProxy {
                    proxy: Some(proxy),
                    dc_id: 2,
                    timeout: 10.0,
                }));
            } else {
                self.send_request(Function::addProxy(td_api::addProxy {
                    proxy: Some(proxy),
                    enable,
                    comment,
                }));
            }
            true
        } else if op == "ping" {
            self.send_request(Function::pingProxy(td_api::pingProxy { proxy: None }));
            true
        } else if op == "gproxy" || op == "gproxies" {
            self.send_request(Function::getProxies(td_api::getProxies));
            true
        } else if op == "gusi" {
            self.send_request(Function::getUserSupportInfo(td_api::getUserSupportInfo {
                user_id: self.as_user_id(args),
            }));
            true
        } else if op == "susi" {
            let (user_id, text) = tokenizer::split(args, ' ');
            self.send_request(Function::setUserSupportInfo(td_api::setUserSupportInfo {
                user_id: self.as_user_id(&user_id),
                message: Some(self.as_formatted_text(&text)),
            }));
            true
        } else if op == "gsn" {
            self.send_request(Function::getSupportName(td_api::getSupportName));
            true
        } else if op == "touch" {
            use std::io::{Seek, SeekFrom, Write};
            let opened = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(args)
                .and_then(|mut file| {
                    let size = file.metadata()?.len();
                    file.seek(SeekFrom::Start(size))?;
                    file.write_all(b"a")?;
                    file.set_len(size)?;
                    Ok(())
                });
            if let Err(error) = opened {
                tdutils::log_error!("{}", error);
            }
            true
        } else if op == "mem" {
            // The `mem_stat` probe over /proc/self/status (the Linux path of
            // td::mem_stat): RSS/peak RSS/VSZ/peak VSZ, in bytes.
            match self.read_proc_status_metrics() {
                Ok(metrics) => tdutils::log_error!(
                    "RSS = {}, peak RSS = {}, VSZ {}, peak VSZ = {}",
                    metrics.0,
                    metrics.1,
                    metrics.2,
                    metrics.3
                ),
                Err(error) => tdutils::log_error!("{}", error),
            }
            true
        } else if op == "cpu" {
            // C++ `to_integer<uint32>(args)`.
            let mut inc_count = tokenizer::to_integer_i64(trim_td(args)) as u32;
            while inc_count > 0 {
                self.cpu_counter.fetch_add(1, Ordering::Relaxed);
                inc_count -= 1;
            }
            // The `cpu_stat` probe over /proc: total ticks across cores plus
            // the process's user/system ticks.
            match self.read_cpu_stat_metrics() {
                Ok((total, user, system)) => tdutils::log_error!(
                    "{}, total ticks = {}, user ticks = {}, system ticks = {}",
                    self.cpu_counter.load(Ordering::Relaxed),
                    total,
                    user,
                    system
                ),
                Err(error) => tdutils::log_error!("{}", error),
            }
            true
        } else if op.starts_with('v')
            && op.len() > 1
            && (op.as_bytes()[1] == b'v' || op.as_bytes()[1].is_ascii_digit())
        {
            let new_verbosity_level = if op.as_bytes()[1] == b'v' {
                op.len() as i32
            } else {
                tokenizer::to_integer_i32(&op[1..])
            };
            tdutils::logging::set_verbosity_level(new_verbosity_level.max(tdutils::logging::DEBUG));
            crate::log::COMBINED_LOG.set_first_verbosity_level(new_verbosity_level);
            true
        } else if op == "slse" {
            self.execute(Function::setLogStream(td_api::setLogStream {
                log_stream: Some(td_api::LogStream::logStreamEmpty(td_api::logStreamEmpty)),
            }));
            true
        } else if op == "slsd" {
            self.execute(Function::setLogStream(td_api::setLogStream {
                log_stream: Some(td_api::LogStream::logStreamDefault(
                    td_api::logStreamDefault,
                )),
            }));
            true
        } else if op == "gls" {
            self.execute(Function::getLogStream(td_api::getLogStream));
            true
        } else if op == "slvl" {
            self.execute(Function::setLogVerbosityLevel(
                td_api::setLogVerbosityLevel {
                    new_verbosity_level: tokenizer::to_integer_i32(trim_td(args)),
                },
            ));
            true
        } else if op == "glvl" {
            self.execute(Function::getLogVerbosityLevel(td_api::getLogVerbosityLevel));
            true
        } else if op == "gtags" || op == "glt" {
            self.execute(Function::getLogTags(td_api::getLogTags));
            true
        } else if op == "sltvl" || op == "sltvle" || op == "tag" {
            let (tag, level) = tokenizer::split(args, ' ');
            let request = Function::setLogTagVerbosityLevel(td_api::setLogTagVerbosityLevel {
                tag,
                new_verbosity_level: tokenizer::to_integer_i32(&level),
            });
            if op == "sltvl" {
                self.send_request(request);
            } else {
                self.execute(request);
            }
            true
        } else if op == "gltvl" || op == "gltvle" || op == "gtag" {
            let request = Function::getLogTagVerbosityLevel(td_api::getLogTagVerbosityLevel {
                tag: args.to_owned(),
            });
            if op == "gltvl" {
                self.send_request(request);
            } else {
                self.execute(request);
            }
            true
        } else if op == "alog" || op == "aloge" {
            let (level, text) = tokenizer::split(args, ' ');
            let request = Function::addLogMessage(td_api::addLogMessage {
                verbosity_level: tokenizer::to_integer_i32(&level),
                text,
            });
            if op == "alog" {
                self.send_request(request);
            } else {
                self.execute(request);
            }
            true
        } else if op == "q" {
            self.quit();
            true
        } else if op == "dnq" {
            // C++ dumps `*net_query_stats_`; the Rust CLI has no NetQuery
            // stats machinery ([not ported: tdactor NetQueryStats]).
            tdutils::log_error!("No pending network queries");
            true
        } else if op == "fatal" {
            // C++ LOG(FATAL) aborts the CLI; the census harness only survives
            // catchable deaths, so mirror the sibling `unreachable` arm.
            panic!("Fatal!")
        } else if op == "unreachable" {
            unreachable!("UNREACHABLE()");
        } else {
            false
        }
    }

    /// Reads the four memory metrics the `mem` arm reports (kB fields of
    /// /proc/self/status, scaled to bytes like td::mem_stat).
    fn read_proc_status_metrics(&self) -> std::io::Result<(u64, u64, u64, u64)> {
        use std::collections::HashMap;
        let status = std::fs::read_to_string("/proc/self/status")?;
        let mut fields: HashMap<&str, u64> = HashMap::new();
        for line in status.lines() {
            if let Some((key, value)) = line.split_once(':') {
                let kb = value.split_whitespace().next().unwrap_or("0");
                fields.insert(key.trim(), kb.parse::<u64>().unwrap_or(0) * 1024);
            }
        }
        Ok((
            fields.get("VmRSS").copied().unwrap_or(0),
            fields.get("VmHWM").copied().unwrap_or(0),
            fields.get("VmSize").copied().unwrap_or(0),
            fields.get("VmPeak").copied().unwrap_or(0),
        ))
    }

    /// Reads (total ticks, process user ticks, process system ticks) for the
    /// `cpu` arm from /proc/stat and /proc/self/stat.
    fn read_cpu_stat_metrics(&self) -> std::io::Result<(u64, u64, u64)> {
        let stat = std::fs::read_to_string("/proc/self/stat")?;
        // Fields per `man proc`: utime = 14, stime = 15 (1-based, after the
        // parenthesized comm which may contain spaces).
        let open = stat.rfind(')').unwrap_or(0);
        let fields: Vec<&str> = stat[open + 1..].split_whitespace().collect();
        let user = fields
            .get(11)
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        let system = fields
            .get(12)
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        let mut total = 0u64;
        if let Ok(cores) = std::fs::read_to_string("/proc/stat") {
            for line in cores.lines() {
                if let Some(rest) = line.strip_prefix("cpu")
                    && rest.starts_with(|c: char| c.is_ascii_digit())
                {
                    total += rest
                        .split_whitespace()
                        .filter_map(|v| v.parse::<u64>().ok())
                        .sum::<u64>();
                }
            }
        }
        Ok((total, user, system))
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

    /// The M.14.2 census gate: the fixture pins all 1382 command arms of the
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
            1382,
            "the C++ tree changed — regenerate the census with \
             conformance/cli_gate/extract_cli_census.sh"
        );
        // (op, chain) pairs whose arms are ported so far: M.14.1 (q) +
        // M.14.3 (the whole auth/account chain and the recovery head) +
        // M.14.4a (the chats chain, minus `apa` — M.14.5 input files).
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
            ("aco", 1),
            ("acon", 1),
            ("agicg", 1),
            ("bgu", 1),
            ("cg", 1),
            ("cga", 1),
            ("cgic", 1),
            ("cic", 1),
            ("cico", 1),
            ("cpnc", 1),
            ("cpr", 1),
            ("csg", 1),
            ("ctp", 1),
            ("dgic", 1),
            ("dgod", 1),
            ("dpe", 1),
            ("dsc", 1),
            ("dsoi", 1),
            ("gag", 1),
            ("gape", 1),
            ("gbci", 1),
            ("gcfr", 1),
            ("gco", 1),
            ("ggaag", 1),
            ("ggas", 1),
            ("ggfc", 1),
            ("ggic", 1),
            ("ggup", 1),
            ("gicc", 1),
            ("gpe", 1),
            ("gpf", 1),
            ("gpr", 1),
            ("gpre", 1),
            ("gpri", 1),
            ("grg", 1),
            ("grgs", 1),
            ("grgsp", 1),
            ("gsoi", 1),
            ("gtp", 1),
            ("gug", 1),
            ("gugpa", 1),
            ("gugv", 1),
            ("gugvc", 1),
            ("gugvi", 1),
            ("gugvu", 1),
            ("gugwu", 1),
            ("gul", 1),
            ("gulf", 1),
            ("gulfp", 1),
            ("gulfr", 1),
            ("gulfpr", 1),
            ("ic", 1),
            ("igab", 1),
            ("oga", 1),
            ("pgab", 1),
            ("pgabp", 1),
            ("pgpoa", 1),
            ("pgpod", 1),
            ("ppn", 1),
            ("rco", 1),
            ("rda", 1),
            ("rdb", 1),
            ("rdf", 1),
            ("rdt", 1),
            ("rdu", 1),
            ("rdw", 1),
            ("resetp", 1),
            ("rgicg", 1),
            ("rogic", 1),
            ("rogicg", 1),
            ("rpnc", 1),
            ("rpncm", 1),
            ("rsp", 1),
            ("scfr", 1),
            ("sellg", 1),
            ("sendg", 1),
            ("sendgp", 1),
            ("sgfr", 1),
            ("sgfrd", 1),
            ("sgfrn", 1),
            ("sgicn", 1),
            ("sgift", 1),
            ("sgpo", 1),
            ("sgrp", 1),
            ("sgss", 1),
            ("spe", 1),
            ("spes", 1),
            ("spfn", 1),
            ("spfrc", 1),
            ("spfs", 1),
            ("spfstar", 1),
            ("spg", 1),
            ("spn", 1),
            ("spncc", 1),
            ("spncco", 1),
            ("spncv", 1),
            ("spnfs", 1),
            ("spr", 1),
            ("srg", 1),
            ("srgp", 1),
            ("subpn", 1),
            ("subpnl", 1),
            ("subt", 1),
            ("tcgn", 1),
            ("tg", 1),
            ("tgis", 1),
            ("ug", 1),
            ("voi", 1),
            ("q", 18),
            ("acslp", 2),
            ("alpk", 2),
            ("amr", 2),
            ("appmr", 2),
            ("appmra", 2),
            ("appmrd", 2),
            ("asas", 2),
            ("cco", 2),
            ("ccop", 2),
            ("cppmr", 2),
            ("crr", 2),
            ("csa", 2),
            ("csfc", 2),
            ("csfh", 2),
            ("dacm", 2),
            ("darmrfs", 2),
            ("dcrm", 2),
            ("ddmcth", 2),
            ("ddmctmbd", 2),
            ("dlp", 2),
            ("dmrfs", 2),
            ("dsa", 2),
            ("dsmth", 2),
            ("dsmtmbd", 2),
            ("eclpi", 2),
            ("gc", 2),
            ("gca", 2),
            ("gcapmrs", 2),
            ("gcc", 2),
            ("gcera", 2),
            ("gcmbd", 2),
            ("gcmc", 2),
            ("gcmca", 2),
            ("gcmp", 2),
            ("gcsa", 2),
            ("gcsm", 2),
            ("gcsmp", 2),
            ("gctest", 2),
            ("gdmct", 2),
            ("gdmcth", 2),
            ("gdmctmbd", 2),
            ("gdmctr", 2),
            ("ger", 2),
            ("gfth", 2),
            ("gh", 2),
            ("ghf", 2),
            ("ghl", 2),
            ("glpi", 2),
            ("glpk", 2),
            ("glps", 2),
            ("glpss", 2),
            ("glti", 2),
            ("gmar", 2),
            ("gmars", 2),
            ("gme", 2),
            ("gmpf", 2),
            ("gmth", 2),
            ("gpkp", 2),
            ("gppsl", 2),
            ("gsas", 2),
            ("gsfh", 2),
            ("gsmth", 2),
            ("gsmtmbd", 2),
            ("gsmts", 2),
            ("gspf", 2),
            ("gup", 2),
            ("gupa", 2),
            ("gupp", 2),
            ("ipa", 2),
            ("apa", 2),
            ("lc", 2),
            ("lca", 2),
            ("ldmct", 2),
            ("lsmt", 2),
            ("radmctr", 2),
            ("react", 2),
            ("reactbot", 2),
            ("reactbotbig", 2),
            ("replies", 2),
            ("rlpk", 2),
            ("rmr", 2),
            ("rosas", 2),
            ("rpa", 2),
            ("rppmr", 2),
            ("rsa", 2),
            ("rsas", 2),
            ("rsfh", 2),
            ("SC", 2),
            ("sclp", 2),
            ("sclpsd", 2),
            ("sclpsp", 2),
            ("sclpsv", 2),
            ("SCM", 2),
            ("scn", 2),
            ("scrlm", 2),
            ("sdmctimau", 2),
            ("sdrt", 2),
            ("Search", 2),
            ("SearchA", 2),
            ("SearchAudio", 2),
            ("SearchChatPhoto", 2),
            ("SearchDocument", 2),
            ("SearchG", 2),
            ("SearchM", 2),
            ("SearchP", 2),
            ("SearchPhoto", 2),
            ("SM", 2),
            ("SMC", 2),
            ("SMME", 2),
            ("SMU", 2),
            ("sodm", 2),
            ("spap", 2),
            ("spmbt", 2),
            ("spmrt", 2),
            ("spmrta", 2),
            ("spps", 2),
            ("spsbl", 2),
            ("spsbt", 2),
            ("spsbv", 2),
            ("spsmt", 2),
            ("spvf", 2),
            ("ssan", 2),
            ("ssms", 2),
            ("ssmtl", 2),
            ("synclp", 2),
            ("tdmctcsum", 2),
            ("tsmtip", 2),
            ("uadmctm", 2),
            ("GetChatAdministrators", 6),
            ("GetSupergroupAdministrators", 6),
            ("GetSupergroupBanned", 6),
            ("GetSupergroupBots", 6),
            ("aftd", 6),
            ("atcs", 6),
            ("cancelSC", 6),
            ("ccspm", 6),
            ("cdf", 6),
            ("close", 6),
            ("closeSC", 6),
            ("cm", 6),
            ("cnrmwa", 6),
            ("cnsc", 6),
            ("crmwa", 6),
            ("crmwac", 6),
            ("csc", 6),
            ("ctcs", 6),
            ("ctcsp", 6),
            ("ctwa", 6),
            ("cuf", 6),
            ("cvma", 6),
            ("delf", 6),
            ("df", 6),
            ("dff", 6),
            ("dfs", 6),
            ("dm", 6),
            ("dmr", 6),
            ("dtcs", 6),
            ("etcs", 6),
            ("etcsp", 6),
            ("fm", 6),
            ("frmwa", 6),
            ("frtwa", 6),
            ("frrmwa", 6),
            ("ftwa", 6),
            ("gbg", 6),
            ("gbgf", 6),
            ("gch", 6),
            ("gchf", 6),
            ("gcm", 6),
            ("gcpm", 6),
            ("gcspm", 6),
            ("gd", 6),
            ("gf", 6),
            ("gfdps", 6),
            ("gfrm", 6),
            ("gm", 6),
            ("gma", 6),
            ("gmec", 6),
            ("gmf", 6),
            ("gml", 6),
            ("gmli", 6),
            ("gmlink", 6),
            ("gmp", 6),
            ("gmrd", 6),
            ("gms", 6),
            ("gmt", 6),
            ("gmtf", 6),
            ("gmv", 6),
            ("gpop", 6),
            ("grf", 6),
            ("grm", 6),
            ("gsc", 6),
            ("gsfn", 6),
            ("gsg", 6),
            ("gsgf", 6),
            ("gssc", 6),
            ("gtcse", 6),
            ("guf", 6),
            ("gvma", 6),
            ("gcfi", 6),
            ("open", 6),
            ("osc", 6),
            ("raffd", 6),
            ("raffda", 6),
            ("raffdc", 6),
            ("rcspm", 6),
            ("resend", 6),
            ("rffd", 6),
            ("rfp", 6),
            ("rs", 6),
            ("rsc", 6),
            ("rsif", 6),
            ("gsrl", 6),
            ("rsr", 6),
            ("rtcs", 6),
            ("rvma", 6),
            ("scm", 6),
            ("sfd", 6),
            ("sfda", 6),
            ("sfdc", 6),
            ("sqrsm", 6),
            ("stcs", 6),
            ("sum", 6),
            ("tadap", 6),
            ("tdip", 6),
            ("tmrm", 6),
            ("tmrmc", 6),
            ("tmrmf", 6),
            ("tmrmn", 6),
            ("tmt", 6),
            ("tmtc", 6),
            ("tmtf", 6),
            ("tmtn", 6),
            ("trm", 6),
            ("trmc", 6),
            ("trmf", 6),
            ("trmn", 6),
            ("tt", 6),
            ("ttc", 6),
            ("ttf", 6),
            ("ttn", 6),
            ("uf", 6),
            ("ufg", 6),
            ("ufs", 6),
            ("ufse", 6),
            ("vsc", 6),
            ("vvma", 6),
            ("acfbil", 14),
            ("acm", 14),
            ("acms", 14),
            ("actl", 14),
            ("actla", 14),
            ("aumfu", 14),
            ("ccf", 14),
            ("ccfe", 14),
            ("ccfil", 14),
            ("crcfil", 14),
            ("csmtu", 14),
            ("csmtul", 14),
            ("dcf", 14),
            ("dcfil", 14),
            ("ecf", 14),
            ("ecfil", 14),
            ("ecfs", 14),
            ("gacls", 14),
            ("gcf", 14),
            ("gcfcc", 14),
            ("gcfcfil", 14),
            ("gcfctl", 14),
            ("gcfdin", 14),
            ("gcfil", 14),
            ("gcfnc", 14),
            ("gcltac", 14),
            ("ggct", 14),
            ("gncps", 14),
            ("gpmr", 14),
            ("grcf", 14),
            ("grdps", 14),
            ("pcfnc", 14),
            ("rcf", 14),
            ("sacls", 14),
            ("scac", 14),
            ("sccd", 14),
            ("sces", 14),
            ("scese", 14),
            ("scmadt", 14),
            ("scp", 14),
            ("scpac", 14),
            ("scperm", 14),
            ("scpmsc", 14),
            ("sct", 14),
            ("scth", 14),
            ("sncps", 14),
            ("srdps", 14),
            ("tcft", 14),
            ("san", 12),
            ("sau", 12),
            ("schl", 12),
            ("scontact", 12),
            ("scopy", 12),
            ("sd", 12),
            ("sdf", 12),
            ("sdice", 12),
            ("sdicecd", 12),
            ("sf", 12),
            ("sg", 12),
            ("sl", 12),
            ("sll", 12),
            ("sp", 12),
            ("spoll", 12),
            ("spollp", 12),
            ("squiz", 12),
            ("ss", 12),
            ("sstory", 12),
            ("stake", 12),
            ("sv", 12),
            ("svenue", 12),
            ("svn", 12),
            ("svoice", 12),
            ("DeleteChat", 13),
            ("alarm", 13),
            ("cbgc", 13),
            ("cchc", 13),
            ("ccpcl", 13),
            ("ccpcll", 13),
            ("cnbgc", 13),
            ("cnchc", 13),
            ("cnchcadt", 13),
            ("cnfc", 13),
            ("cnfcadt", 13),
            ("cnsgc", 13),
            ("cnsgcadt", 13),
            ("cnsgcimport", 13),
            ("cnsgcloc", 13),
            ("cpc", 13),
            ("csgc", 13),
            ("dcmbd", 13),
            ("dcmbs", 13),
            ("delete", 13),
            ("gbsb", 13),
            ("gbsbc", 13),
            ("gcpc", 13),
            ("gcpcl", 13),
            ("gcsc", 13),
            ("gcscc", 13),
            ("gisc", 13),
            ("grc", 13),
            ("gscoc", 13),
            ("gsdc", 13),
            ("gspc", 13),
            ("obsb", 13),
            ("ocsc", 13),
            ("test", 13),
            ("ubgctsc", 13),
            ("ccun", 15),
            ("cres", 15),
            ("cto", 15),
            ("dachun", 15),
            ("dasgun", 15),
            ("dcm", 15),
            ("gcoal", 15),
            ("gdces", 15),
            ("gdes", 15),
            ("gdices", 15),
            ("gres", 15),
            ("gtces", 15),
            ("gtes", 15),
            ("gtz", 15),
            ("guges", 15),
            ("join", 15),
            ("leave", 15),
            ("log", 15),
            ("logf", 15),
            ("raun", 15),
            ("rchaun", 15),
            ("rsgaun", 15),
            ("sb", 15),
            ("sbd", 15),
            ("schun", 15),
            ("scms", 15),
            ("scmt", 15),
            ("ses", 15),
            ("sese", 15),
            ("sesg", 15),
            ("smpt", 15),
            ("smptg", 15),
            ("sn", 15),
            ("spec", 15),
            ("ssgcess", 15),
            ("ssgss", 15),
            ("ssgubc", 15),
            ("ssgun", 15),
            ("ssmpt", 15),
            ("ssmptg", 15),
            ("sun", 15),
            ("tchunia", 15),
            ("thsme", 15),
            ("transferChatOwnership", 15),
            ("tsgas", 15),
            ("tsgchsm", 15),
            ("tsghat", 15),
            ("tsghhm", 15),
            ("tsgif", 15),
            ("tsgjbr", 15),
            ("tsgjbra", 15),
            ("tsgjtsm", 15),
            ("tsgp", 15),
            ("tsgsm", 15),
            ("tsgunia", 15),
            ("tsibg", 15),
            ("tunia", 15),
            ("bcm", 14),
            ("apolo", 14),
            ("apo", 14),
            ("apol", 14),
            ("apop", 14),
            ("apov", 14),
            ("apoan", 14),
            ("dpo", 14),
            ("spolla", 14),
            ("gpollv", 14),
            ("gpvst", 14),
            ("stoppoll", 14),
            ("achlt", 14),
            ("mchltad", 14),
            ("afs", 5),
            ("ans", 5),
            ("ansc", 5),
            ("ars", 5),
            ("asan", 5),
            ("awbsee", 5),
            ("awbsei", 5),
            ("cause", 5),
            ("clean_photos", 5),
            ("clean_storage", 5),
            ("clean_storage_default", 5),
            ("cnss", 5),
            ("cnssa", 5),
            ("cnsse", 5),
            ("cnssm", 5),
            ("cnssv", 5),
            ("crs", 5),
            ("css", 5),
            ("cssn", 5),
            ("current_network", 5),
            ("cwbs", 5),
            ("database", 5),
            ("gadsp", 5),
            ("gae", 5),
            ("gaus", 5),
            ("gces", 5),
            ("gdbces", 5),
            ("gdcpces", 5),
            ("gdppces", 5),
            ("gec", 5),
            ("geccp", 5),
            ("geces", 5),
            ("gecrs", 5),
            ("gesu", 5),
            ("gfs", 5),
            ("gke", 5),
            ("gkeru", 5),
            ("goss", 5),
            ("grs", 5),
            ("gsan", 5),
            ("gse", 5),
            ("gsssn", 5),
            ("gtc", 5),
            ("network", 5),
            ("optimize_storage", 5),
            ("optimize_storage_all", 5),
            ("rawbse", 5),
            ("reset_network", 5),
            ("rfs", 5),
            ("riss", 5),
            ("risse", 5),
            ("rissm", 5),
            ("rrs", 5),
            ("rsan", 5),
            ("rtc", 5),
            ("rwbse", 5),
            ("sads", 5),
            ("saus", 5),
            ("se", 5),
            ("seru", 5),
            ("siss", 5),
            ("snt", 5),
            ("sss", 5),
            ("sssf", 5),
            ("ssss", 5),
            ("sssse", 5),
            ("ssssm", 5),
            ("storage", 5),
            ("storage_fast", 5),
            ("usf", 5),
            ("usfa", 5),
            ("usfv", 5),
            ("vtss", 5),
            ("apgc", 4),
            ("atos", 4),
            ("capr", 4),
            ("cpfs", 4),
            ("cpfsb", 4),
            ("cpfsg", 4),
            ("cpfsgs", 4),
            ("cpfss", 4),
            ("cpfssg", 4),
            ("cpgc", 4),
            ("cprsb", 4),
            ("dapr", 4),
            ("dcb", 4),
            ("dcbr", 4),
            ("ddb", 4),
            ("ddbd", 4),
            ("ess", 4),
            ("gadl", 4),
            ("gase", 4),
            ("gasem", 4),
            ("gaseme", 4),
            ("gaser", 4),
            ("gass", 4),
            ("gasse", 4),
            ("gassm", 4),
            ("gatss", 4),
            ("gbf", 4),
            ("gbms", 4),
            ("gcapr", 4),
            ("gcaprs", 4),
            ("gcipn", 4),
            ("gciu", 4),
            ("gcoc", 4),
            ("gcos", 4),
            ("gcou", 4),
            ("gdli", 4),
            ("ggi", 4),
            ("ggs", 4),
            ("giss", 4),
            ("gisse", 4),
            ("gissm", 4),
            ("gpgapo", 4),
            ("gpgpo", 4),
            ("gpis", 4),
            ("gpni", 4),
            ("gpnis", 4),
            ("gprf", 4),
            ("gprl", 4),
            ("gprs", 4),
            ("gprse", 4),
            ("gprst", 4),
            ("gpws", 4),
            ("gs", 4),
            ("gsee", 4),
            ("gseeme", 4),
            ("gsgapo", 4),
            ("gsgpo", 4),
            ("gsmm", 4),
            ("gso", 4),
            ("gsoa", 4),
            ("gsoc", 4),
            ("gsosp", 4),
            ("gsospa", 4),
            ("gsospc", 4),
            ("gspo", 4),
            ("gss", 4),
            ("gssn", 4),
            ("gssu", 4),
            ("gsta", 4),
            ("gsti", 4),
            ("gsto", 4),
            ("gsu", 4),
            ("gtss", 4),
            ("gtsse", 4),
            ("gtssm", 4),
            ("gu", 4),
            ("lpg", 4),
            ("rib", 4),
            ("ribs", 4),
            ("rss", 4),
            ("sapc", 4),
            ("sapd", 4),
            ("sapr", 4),
            ("sbg", 4),
            ("scap", 4),
            ("scapd", 4),
            ("scapr", 4),
            ("scbg", 4),
            ("scbgs", 4),
            ("sdb", 4),
            ("sdbd", 4),
            ("sst", 4),
            ("sste", 4),
            ("sstm", 4),
            ("tme", 4),
            ("vprf", 4),
            ("on", 3),
            ("off", 3),
            ("go", 3),
            ("gos", 3),
            ("sob", 3),
            ("soe", 3),
            ("soi", 3),
            ("sos", 3),
            ("me", 3),
            ("sdmadt", 3),
            ("gdmadt", 3),
            ("sattl", 3),
            ("gattl", 3),
            ("devices", 3),
            ("sessions", 3),
            ("ts", 3),
            ("taos", 3),
            ("cse", 3),
            ("tscac", 3),
            ("tscasc", 3),
            ("sist", 3),
            ("gcw", 3),
            ("dw", 3),
            ("daw", 3),
            ("gib", 3),
            ("gbgu", 3),
            ("scdm", 9),
            ("scdmrm", 9),
            ("scdmvn", 9),
            ("scdmvoice", 9),
            ("cadm", 9),
            ("gsds", 9),
            ("tchpc", 9),
            ("pchpcdr", 9),
            ("tcip", 9),
            ("tcipa", 9),
            ("tcimau", 9),
            ("tcvat", 9),
            ("tcit", 9),
            ("smsbl", 9),
            ("bmsfr", 9),
            ("tcddn", 9),
            ("spchats", 9),
            ("spchatsa", 9),
            ("rcl", 9),
            ("rcla", 9),
            ("gcwe", 9),
            ("gst", 9),
            ("gstl", 9),
            ("gctps", 9),
            ("cps", 9),
            ("srsfi", 9),
            ("ssmu", 9),
            ("psp", 9),
            ("pspp", 9),
            ("psv", 9),
            ("psvp", 9),
            ("sls", 9),
            ("slsr", 9),
            ("esc", 9),
            ("esp", 9),
            ("esv", 9),
            ("esco", 9),
            ("ssps", 9),
            ("tsiptcp", 9),
            ("ds", 9),
            ("las", 9),
            ("lasa", 9),
            ("lase", 9),
            ("scasl", 9),
            ("scasla", 9),
            ("scasle", 9),
            ("gcptcps", 9),
            ("gcast", 9),
            ("scps", 9),
            ("gsnse", 9),
            ("gcas", 9),
            ("os", 9),
            ("cs", 9),
            ("gsar", 9),
            ("ssr", 9),
            ("gsi", 9),
            ("gcsi", 9),
            ("rst", 9),
            ("assm", 9),
            ("gcblf", 9),
            ("gcbf", 9),
            ("gacbs", 9),
            ("gcbs", 9),
            ("bc", 9),
            ("gcbl", 9),
            ("gcbli", 9),
            ("gcb", 9),
            ("gucb", 9),
            ("cqrsn", 11),
            ("lqrs", 11),
            ("dqrs", 11),
            ("sqrsn", 11),
            ("rqrs", 11),
            ("lqrsm", 11),
            ("dqrsm", 11),
            ("gftdi", 11),
            ("cft", 11),
            ("cfti", 11),
            ("eft", 11),
            ("gft", 11),
            ("gftl", 11),
            ("gfts", 11),
            ("tftic", 11),
            ("tgftih", 11),
            ("tftip", 11),
            ("spft", 11),
            ("dft", 11),
            ("sbsm", 11),
            ("giqr", 11),
            ("giqro", 11),
            ("giqrl", 11),
            ("gpim", 11),
            ("siqr", 11),
            ("siqrh", 11),
            ("gpkb", 11),
            ("gcqa", 11),
            ("gcpqa", 11),
            ("gcgqa", 11),
            ("acq", 11),
            ("acqa", 11),
            ("cc", 7),
            ("ac", 7),
            ("scsd", 7),
            ("dc", 7),
            ("scr", 7),
            ("scdi", 7),
            ("sclog", 7),
            ("gvcap", 7),
            ("svcdp", 7),
            ("cvc", 7),
            ("cgc", 7),
            ("cgcv", 7),
            ("cgca", 7),
            ("gvcru", 7),
            ("rvcru", 7),
            ("glsru", 7),
            ("rlsru", 7),
            ("ggc", 7),
            ("ggcs", 7),
            ("ggcss", 7),
            ("egcd", 7),
            ("egcds", 7),
            ("dgcd", 7),
            ("dgcds", 7),
            ("ssvc", 7),
            ("tvcesn", 7),
            ("tvcesne", 7),
            ("jgc", 7),
            ("jgcv", 7),
            ("jvc", 7),
            ("jvcv", 7),
            ("jls", 7),
            ("jlsv", 7),
            ("sgcss", 7),
            ("tgcssip", 7),
            ("egcss", 7),
            ("svct", 7),
            ("tvcmnp", 7),
            ("tvcmnpe", 7),
            ("tgcama", 7),
            ("sgcpmsc", 7),
            ("glss", 7),
            ("glsams", 7),
            ("slsms", 7),
            ("sgcm", 7),
            ("aplsr", 7),
            ("cplsr", 7),
            ("rplsr", 7),
            ("dgcm", 7),
            ("dgcms", 7),
            ("dgcmbs", 7),
            ("dgcmbss", 7),
            ("glstd", 7),
            ("rgcil", 7),
            ("tgcimvp", 7),
            ("tgcimve", 7),
            ("sgcpis", 7),
            ("igcp", 7),
            ("igcpv", 7),
            ("dgci", 7),
            ("bgcp", 7),
            ("ivcp", 7),
            ("gvcil", 7),
            ("sgcr", 7),
            ("egcr", 7),
            ("tgcpim", 7),
            ("sgcpvl", 7),
            ("tgcpihr", 7),
            ("ggcp", 7),
            ("lgcp", 7),
            ("lgc", 7),
            ("egc", 7),
            ("gamb", 10),
            ("tbiatam", 10),
            ("ggwab", 10),
            ("swa", 10),
            ("gwap", 10),
            ("gwalu", 10),
            ("gmwa", 10),
            ("gwau", 10),
            ("ggbwau", 10),
            ("swad", 10),
            ("owa", 10),
            ("cwa", 10),
            ("cwafd", 10),
            ("sca", 10),
            ("spm", 10),
            ("smt", 10),
            ("smtp", 10),
            ("smtf", 10),
            ("smtpf", 10),
            ("ssm", 10),
            ("ssd", 10),
            ("ssrp", 10),
            ("smei", 10),
            ("smpmsc", 10),
            ("sop", 10),
            ("sfti", 10),
            ("smti", 10),
            ("sdmcti", 10),
            ("sbci", 10),
            ("shs", 10),
            ("smsdt", 10),
            ("gcams", 10),
            ("scmsr", 10),
            ("smr", 10),
            ("smrq", 10),
            ("smrqp", 10),
            ("smrcti", 10),
            ("smrpoi", 10),
            ("smrs", 10),
            ("slpo", 10),
            ("sscam", 10),
            ("sgci", 10),
            ("ssmt", 10),
            ("sqrs", 10),
            ("smer", 10),
            ("swm", 10),
            ("ewm", 10),
            ("smas", 10),
            ("smc", 10),
            ("srmm", 10),
            ("smco", 10),
            ("smth", 10),
            ("smst", 10),
            ("sugai", 10),
            ("sspp", 10),
            ("sspsd", 10),
            ("sm", 10),
            ("sms", 10),
            ("smf", 10),
            ("srm", 10),
            ("smce", 10),
            ("alm", 10),
            ("alrm", 10),
            ("spmp", 10),
            ("spmv", 10),
            ("smap", 10),
            ("smad", 10),
            ("smav", 10),
            ("savt", 10),
            ("gmft", 10),
            ("gmict", 10),
            ("im", 10),
            ("em", 10),
            ("emrm", 10),
            ("eqrm", 10),
            ("eqrrm", 10),
            ("eman", 10),
            ("emc", 10),
            ("emd", 10),
            ("eqrmd", 10),
            ("emp", 10),
            ("eqrmp", 10),
            ("eqrmv", 10),
            ("eqrman", 10),
            ("eqrmchl", 10),
            ("emv", 10),
            ("emll", 10),
            ("emchl", 10),
            ("emss", 10),
            ("dmec", 10),
            ("lcwm", 10),
            ("dcwm", 10),
            ("dacwm", 10),
            ("smfc", 10),
            ("scar", 16),
            ("scd", 16),
            ("scdg", 16),
            ("scdmg", 16),
            ("scl", 16),
            ("scsmd", 16),
            ("pcm", 16),
            ("pcms", 16),
            ("pcmo", 16),
            ("upcm", 16),
            ("uacm", 16),
            ("uaftm", 16),
            ("rbm", 16),
            ("dbm", 16),
            ("ebsp", 16),
            ("ebsv", 16),
            ("dbs", 16),
            ("sban", 16),
            ("sbab", 16),
            ("sbau", 16),
            ("sbapp", 16),
            ("sbappf", 16),
            ("sbags", 16),
            ("gbasa", 16),
            ("tbas", 16),
            ("grib", 16),
            ("gob", 16),
            ("spc", 16),
            ("su", 16),
            ("spcs", 16),
            ("spcsb", 16),
            ("spcsc", 16),
            ("sc", 16),
            ("scb", 16),
            ("scc", 16),
            ("scos", 16),
            ("scosb", 16),
            ("scosc", 16),
            ("sbl", 16),
            ("sboh", 16),
            ("sbgms", 16),
            ("sbams", 16),
            ("sbamso", 16),
            ("sbsp", 16),
            ("gbcb", 16),
            ("sbcb", 16),
            ("tbcbcip", 16),
            ("rbcbfc", 16),
            ("cbcb", 16),
            ("dbcb", 16),
            ("gbcl", 16),
            ("cbcl", 16),
            ("ebcl", 16),
            ("dbcl", 16),
            ("gbcli", 16),
            ("gbc", 16),
            ("sco", 16),
            ("srfcs", 16),
            ("srfcsb", 16),
            ("srfcsc", 16),
            ("arfc", 16),
            ("rrfc", 16),
            ("crfcs", 16),
            ("groc", 16),
            ("glp", 16),
            ("gwpiv", 16),
            ("spp", 16),
            ("spppf", 16),
            ("suppp", 16),
            ("sunote", 16),
            ("supp", 16),
            ("sub", 16),
            ("tbcmes", 16),
            ("cbsm", 16),
            ("abtsm", 16),
            ("swacr", 17),
            ("gbmp", 17),
            ("gbmpi", 17),
            ("abmpp", 17),
            ("abmpv", 17),
            ("ebmpp", 17),
            ("ebmpv", 17),
            ("rbmp", 17),
            ("dbmp", 17),
            ("cbun", 17),
            ("cbun2", 17),
            ("absun", 17),
            ("dbsun", 17),
            ("cb", 17),
            ("cbl", 17),
            ("gbi", 17),
            ("sbit", 17),
            ("sbn", 17),
            ("gbn", 17),
            ("sbpp", 17),
            ("tbunia", 17),
            ("rbaun", 17),
            ("sbid", 17),
            ("gbid", 17),
            ("sbisd", 17),
            ("gbisd", 17),
            ("smsbv", 17),
            ("rmsbv", 17),
            ("sh", 17),
            ("rrh", 17),
            ("view", 17),
            ("viewh", 17),
            ("viewft", 17),
            ("viewt", 17),
            ("views", 17),
            ("viewf", 17),
            ("omc", 17),
            ("caem", 17),
            ("lta", 17),
            ("smvm", 17),
            ("gilt", 17),
            ("geli", 17),
            ("gel", 17),
            ("gelw", 17),
            ("glwbt", 17),
            ("goli", 17),
            ("cormc", 17),
            ("aor", 17),
            ("aorw", 17),
            ("aorp", 17),
            ("dor", 17),
            ("racm", 17),
            ("raftm", 17),
            ("racr", 17),
            ("racpv", 17),
            ("raftr", 17),
            ("raftpv", 17),
            ("tre", 17),
            ("dpp", 17),
            ("sac", 17),
            ("sugc", 17),
            ("spac", 17),
            ("gns", 17),
            ("gnss", 17),
            ("asns", 17),
            ("rns", 17),
            ("gcnse", 17),
            ("gcnses", 17),
            ("gsns", 17),
            ("scns", 17),
            ("ssns", 17),
            ("scons", 17),
            ("sftns", 17),
            ("srns", 17),
            ("rans", 17),
            ("rn", 17),
            ("rng", 17),
            ("rcab", 17),
            ("rc", 17),
            ("rcp", 17),
            ("reportmr", 17),
            ("gcst", 17),
            ("gcrst", 17),
            ("gcrwu", 17),
            ("gcrt", 17),
            ("gtta", 17),
            ("gtti", 17),
            ("gtto", 17),
            ("gsrs", 17),
            ("gswu", 17),
            ("gsaau", 17),
            ("ggrs", 17),
            ("ggwu", 17),
            ("sgs", 18),
            ("gghs", 18),
            ("gmst", 18),
            ("gsst", 18),
            ("gstg", 18),
            ("hsa", 18),
            ("hccb", 18),
            ("glui", 18),
            ("glu", 18),
            ("glua", 18),
            ("suwb", 18),
            ("suwbc", 18),
            ("scwb", 18),
            ("scwbc", 18),
            ("rsgs", 18),
            ("rsgasfp", 18),
            ("gdiff", 18),
            ("dproxy", 18),
            ("eproxy", 18),
            ("rproxy", 18),
            ("aproxy", 18),
            ("aeproxy", 18),
            ("aeproxytcp", 18),
            ("editproxy", 18),
            ("editeproxy", 18),
            ("ping", 18),
            ("gproxy", 18),
            ("gproxies", 18),
            ("gusi", 18),
            ("susi", 18),
            ("gsn", 18),
            ("touch", 18),
            ("mem", 18),
            ("cpu", 18),
            ("slse", 18),
            ("slsd", 18),
            ("gls", 18),
            ("slvl", 18),
            ("glvl", 18),
            ("gtags", 18),
            ("glt", 18),
            ("sltvl", 18),
            ("sltvle", 18),
            ("tag", 18),
            ("gltvl", 18),
            ("gltvle", 18),
            ("gtag", 18),
            ("alog", 18),
            ("aloge", 18),
            ("dnq", 18),
            ("fatal", 18),
            ("unreachable", 18),
            ("rpcil", 8),
            ("ccilt", 8),
            ("ccsil", 8),
            ("ecil", 8),
            ("ecsil", 8),
            ("rcil", 8),
            ("gcilc", 8),
            ("gcil", 8),
            ("gcils", 8),
            ("gcilr", 8),
            ("gcilm", 8),
            ("gcilme", 8),
            ("gcjr", 8),
            ("pcjr", 8),
            ("pcjrs", 8),
            ("drcil", 8),
            ("darcil", 8),
            ("ccil", 8),
            ("jcbil", 8),
            ("asp", 8),
            ("dsp", 8),
            ("ao", 8),
            ("sq", 8),
            ("gte", 8),
            ("gtee", 8),
            ("pm", 8),
            ("pte", 8),
            ("pteh", 8),
            ("ptes", 8),
            ("ptehs", 8),
            ("ssbp", 8),
            ("gcfe", 8),
            ("gfmt", 8),
            ("gfe", 8),
            ("cfn", 8),
            ("gjv", 8),
            ("gjvtest", 8),
            ("gjs", 8),
            ("gtpjs", 8),
            ("gac", 8),
            ("sale", 8),
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
            // An arm may legitimately CHECK-fail on empty arguments (the
            // C++ CLI would abort the same way) — the census gate only
            // classifies ROUTING, so a panic counts as routed. `aloge` is
            // the one arm whose C++ behavior is unconditional process death
            // (Logging::add_message → VLOG(level=0) → LOG(FATAL) abort), so
            // it is routed with a benign level instead; routing is pinned.
            let cmd = if op == "aloge" {
                "aloge 2 test".to_owned()
            } else {
                op.to_string()
            };
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                client.on_cmd(cmd);
            }));
            // `slvl`-class ops mutate process-wide logging state (an empty-args
            // `slvl` drops the verbosity to FATAL, silencing later ERROR/INFO
            // lines); restore the process default so every op is classified
            // independently and later tests start fresh.
            logging::set_verbosity_level(logging::DEBUG + 1);
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
            query_id_to_send_message_info: Mutex::new(HashMap::new()),
            message_id_to_send_message_info: Mutex::new(HashMap::new()),
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
            business_connection_id: Mutex::new(String::new()),
            added_sticker_file_ids: Mutex::new(Vec::new()),
            start_timestamp: AtomicI32::new(0),
            thumbnail: Mutex::new(String::new()),
            cover: Mutex::new(String::new()),
            rich_message_media: Mutex::new(String::new()),
            schedule_date: Mutex::new(String::new()),
            schedule_repeat_period: AtomicI32::new(0),
            paid_message_star_count: AtomicI64::new(0),
            message_effect_id: AtomicI64::new(0),
            only_preview: AtomicBool::new(false),
            has_spoiler: AtomicBool::new(false),
            message_self_destruct_time: AtomicI32::new(0),
            reply_chat_id: AtomicI64::new(0),
            reply_message_id: AtomicI64::new(0),
            reply_quote: Mutex::new(String::new()),
            reply_quote_position: AtomicI32::new(0),
            reply_checklist_task_id: AtomicI32::new(0),
            reply_poll_option_id: Mutex::new(String::new()),
            reply_story_chat_id: AtomicI64::new(0),
            reply_story_id: AtomicI32::new(0),
            link_preview_url: Mutex::new(String::new()),
            link_preview_is_disabled: AtomicBool::new(false),
            link_preview_force_small_media: AtomicBool::new(false),
            link_preview_force_large_media: AtomicBool::new(false),
            link_preview_show_above_text: AtomicBool::new(false),
            show_caption_above_media: AtomicBool::new(false),
            send_welcome_message: AtomicBool::new(false),
            edit_welcome_message_id: AtomicI32::new(0),
            quick_reply_shortcut_name: Mutex::new(String::new()),
            ephemeral_message_receiver_id: AtomicI64::new(0),
            caption: Mutex::new(String::new()),
            gift_collection_id: AtomicI32::new(0),
            upgraded_gift_attribute_ids: Mutex::new(Vec::new()),
            reposted_story_chat_id: AtomicI64::new(0),
            reposted_story_id: AtomicI32::new(0),
            story_music: Mutex::new(String::new()),
            group_call_source: AtomicI32::new(random::fast(1, 1000000000)),
            suggested_post_price: Mutex::new(String::new()),
            suggested_post_send_date: AtomicI32::new(-1),
            cpu_counter: AtomicU64::new(0),
        }
    }
}
