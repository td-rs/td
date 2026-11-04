//! `td::BusinessManager` (BusinessManager.h/.cpp, M.48) + the
//! [`BusinessEnv`] seam every business value class reads through.
//!
//! The C++ manager is a thin router over thirteen `Td::ResultHandler`
//! query classes; the net-drain 26 slice dispatches them over the
//! BusinessNetKind plane (the `UserManager::net` convention — the
//! manager holds the net handle, the query construction lives at the
//! manager send sites, the answers drain through
//! `Td::on_business_net_answer`), while the local validation (the
//! connected-bot input checks, the dialog-access checks) is real. The
//! `get_business_features` static helper is ported too.

use crate::business_away_message::BusinessAwayMessage;

use crate::business_connected_bot::BusinessConnectedBot;
use crate::business_greeting_message::BusinessGreetingMessage;
use crate::business_intro::BusinessIntro;
use crate::business_work_hours::BusinessWorkHours;
use crate::dialog_id::DialogId;
use crate::files::FileId;
use crate::store_parse::Parser;
use crate::td_api;
use crate::user_manager::UserId;
use tdutils::promise::Promise;
use tdutils::status::Status;

/// The `G()`/`td_` surface the business value classes and the manager
/// read through (the C++ reads the same surfaces off its `Td *td_`
/// directly). The defaulted halves are the seams: the
/// [blocked: M.10 message plane] query tails park their promises, the
/// sticker/file-view halves answer their defaults until their tracks
/// land.
pub trait BusinessEnv {
    fn is_bot(&self) -> bool;
    fn get_time_zone_offset(&mut self, time_zone_id: &str) -> i32;
    fn get_option_integer(&mut self, name: &str, default_value: i64) -> i64;

    // -- the user/dialog surfaces (the test envs override what they need;
    //    the defaults keep the C++-required calls observable) --
    fn force_create_dialog(&mut self, dialog_id: DialogId, source: &str, force: bool) {
        tdutils::log_info!(
            "force_create_dialog({}, {source}, {force}) — seam default",
            dialog_id.get()
        );
    }
    fn get_chat_id_object(&mut self, dialog_id: DialogId, source: &str) -> i64 {
        let _ = source;
        dialog_id.get()
    }
    fn get_input_users_force(
        &mut self,
        user_ids: &[UserId],
    ) -> Vec<tdmtproto::telegram_api::InputUser> {
        tdutils::log_info!(
            "get_input_users_force for {} users — seam default",
            user_ids.len()
        );
        Vec::new()
    }
    fn get_user_id_object(&mut self, user_id: UserId, source: &str) -> i64 {
        let _ = source;
        user_id.get()
    }
    fn get_input_user(
        &mut self,
        user_id: UserId,
    ) -> std::result::Result<tdmtproto::telegram_api::InputUser, Status> {
        tdutils::log_info!("get_input_user({}) — seam default", user_id.get());
        Err(Status::error(400, "User not found"))
    }

    /// `td_->option_manager_->get_option_boolean(name, default)` — the
    /// always-parse-markdown input of the chat-link text parse.
    fn get_option_boolean(&mut self, name: &str, default_value: bool) -> bool {
        let _ = name;
        default_value
    }

    /// `td_->dialog_manager_->get_input_peer(dialog_id, access_rights)`
    /// (DialogManager.cpp:1377-1395) — the Write/Read send legs of the
    /// connected-bot families and the star-status query.
    fn get_input_peer(
        &mut self,
        _dialog_id: DialogId,
        _access_rights: crate::user_manager::AccessRights,
    ) -> Option<tdmtproto::telegram_api::InputPeer> {
        tdutils::log_info!("get_input_peer — seam default");
        None
    }

    /// `td_->dialog_manager_->get_my_dialog_id()` — the input-form
    /// formatted-text parse context.
    fn get_my_dialog_id(&mut self) -> DialogId {
        DialogId::from_user_id(crate::user_manager::UserId::default())
    }

    /// `get_formatted_text(td_, my_dialog_id, text, false, true, true,
    /// false)` (MessageEntity.h:260) — the inputBusinessChatLink
    /// draft-text parse; the default skips the parse (the seam form).
    fn get_formatted_text_input(
        &mut self,
        _text: Option<td_api::FormattedText>,
    ) -> std::result::Result<td_api::FormattedText, Status> {
        Ok(td_api::FormattedText::default())
    }

    /// `get_input_message_entities(user_manager, &text,
    /// "get_input_business_chat_link")` (MessageEntity.cpp) — the
    /// wire entities of the chat-link text; the default carries none.
    fn get_input_message_entities(
        &mut self,
        _text: &td_api::FormattedText,
    ) -> Vec<tdmtproto::telegram_api::MessageEntity> {
        Vec::new()
    }

    // -- the message-entity plane (BusinessChatLink) --
    fn get_message_text(
        &mut self,
        message: String,
        entities: Option<Vec<tdmtproto::telegram_api::MessageEntity>>,
        source: &str,
    ) -> Option<td_api::FormattedText> {
        let _ = (message, entities, source);
        None
    }
    fn get_formatted_text_object(
        &mut self,
        text: &Option<td_api::FormattedText>,
        skip_bot_commands: bool,
        max_length: i32,
    ) -> Option<td_api::FormattedText> {
        let _ = (skip_bot_commands, max_length);
        text.clone()
    }

    // -- the sticker plane (BusinessIntro) —
    //    [blocked: the sticker/file-view plane] --
    fn on_get_sticker_document(
        &mut self,
        _document: Option<tdmtproto::telegram_api::Document>,
        _source: &str,
    ) -> FileId {
        tdutils::log_info!("on_get_sticker_document — [blocked: the sticker plane]");
        FileId::default()
    }
    fn get_sticker_object(&mut self, _file_id: FileId) -> Option<td_api::Sticker> {
        tdutils::log_info!("get_sticker_object — [blocked: the sticker plane]");
        None
    }
    fn intro_get_input_file_id(&mut self, _file: Option<td_api::InputFile>) -> FileId {
        tdutils::log_info!("intro_get_input_file_id — [blocked: Files]");
        FileId::default()
    }
    fn intro_get_input_document(
        &mut self,
        _file_id: FileId,
    ) -> Option<tdmtproto::telegram_api::InputDocument> {
        tdutils::log_info!("intro_get_input_document — [blocked: Files]");
        None
    }
    fn intro_get_sticker_file_ids(&mut self, _file_id: FileId) -> Vec<FileId> {
        tdutils::log_info!("intro_get_sticker_file_ids — [blocked: Files]");
        Vec::new()
    }
    fn intro_store_sticker(&mut self, _file_id: FileId, _out: &mut Vec<u8>) {
        tdutils::log_info!("intro_store_sticker — [blocked: the sticker plane]");
    }
    fn intro_parse_sticker(
        &mut self,
        _parser: &mut Parser,
    ) -> std::result::Result<FileId, &'static str> {
        tdutils::log_info!("intro_parse_sticker — [blocked: the sticker plane]");
        Ok(FileId::default())
    }

    // -- the update outbox --
    fn send_update(&mut self, update: td_api::Update) {
        tdutils::log_info!("send_update({update:?}) — seam default");
    }

    // -- the BusinessConnectionManager query seams —
    //    [blocked: M.10 message plane] --
    fn send_business_message_query(
        &mut self,
        _connection_id: crate::business_connection_manager::BusinessConnectionId,
        _dialog_id: DialogId,
        _promise: Promise<td_api::BusinessMessage>,
    ) {
        tdutils::log_info!("Send business message — [blocked: M.10 message plane]");
    }
    fn send_business_message_album_query(
        &mut self,
        _connection_id: crate::business_connection_manager::BusinessConnectionId,
        _dialog_id: DialogId,
        _promise: Promise<td_api::BusinessMessages>,
    ) {
        tdutils::log_info!("Send business message album — [blocked: M.10 message plane]");
    }
    fn edit_business_message_query(
        &mut self,
        _connection_id: crate::business_connection_manager::BusinessConnectionId,
        _dialog_id: DialogId,
        _message_id: crate::message_id::MessageId,
        _promise: Promise<td_api::BusinessMessage>,
    ) {
        tdutils::log_info!("Edit business message — [blocked: M.10 message plane]");
    }
    fn stop_business_poll_query(
        &mut self,
        _connection_id: crate::business_connection_manager::BusinessConnectionId,
        _dialog_id: DialogId,
        _message_id: crate::message_id::MessageId,
        _promise: Promise<td_api::BusinessMessage>,
    ) {
        tdutils::log_info!("Stop business poll — [blocked: M.10 message plane]");
    }
    fn read_business_message_query(
        &mut self,
        _connection_id: crate::business_connection_manager::BusinessConnectionId,
        _dialog_id: DialogId,
        _message_id: crate::message_id::MessageId,
        _promise: Promise<()>,
    ) {
        tdutils::log_info!("Read business message — [blocked: M.10 message plane]");
    }
    fn delete_business_messages_query(
        &mut self,
        _connection_id: crate::business_connection_manager::BusinessConnectionId,
        _message_ids: Vec<crate::message_id::MessageId>,
        _promise: Promise<()>,
    ) {
        tdutils::log_info!("Delete business messages — [blocked: M.10 message plane]");
    }
    fn delete_business_story_query(
        &mut self,
        _connection_id: crate::business_connection_manager::BusinessConnectionId,
        _promise: Promise<()>,
    ) {
        tdutils::log_info!("Delete business story — [blocked: the story plane]");
    }

    // -- the account/messages-manager seams (the query answer halves) --
    fn on_confirm_authorization(&mut self, _is_confirm: bool, _hash: i64, _bot_user_id: UserId) {}
    fn on_update_dialog_business_bot_is_paused(&mut self, _dialog_id: DialogId, _is_paused: bool) {}
    fn on_update_dialog_business_bot_removed(&mut self, _dialog_id: DialogId) {}
    fn hide_all_business_bot_manager_bars(&mut self) {}
    fn on_update_my_user_location(&mut self, _location: crate::chat_manager::DialogLocation) {}
    fn on_update_my_user_work_hours(&mut self, _work_hours: &BusinessWorkHours) {}
    fn on_update_my_user_greeting_message(&mut self, _greeting_message: &BusinessGreetingMessage) {}
    fn on_update_my_user_away_message(&mut self, _away_message: &BusinessAwayMessage) {}
    fn on_update_my_user_intro(&mut self, _intro: &BusinessIntro) {}
    fn on_get_users(&mut self, _users: Vec<tdmtproto::telegram_api::User>, _source: &str) {}
    fn on_get_chats(&mut self, _chats: Vec<tdmtproto::telegram_api::Chat>, _source: &str) {}
}

/// `td::InputBusinessChatLink` (InputBusinessChatLink.h:12-27) — the
/// create/edit wire input of the chat-link families (the draft text
/// parses through the input-form formatted-text helper; a parse error
/// keeps the default empty text with the C++ "Ignore draft text" log).
pub struct InputBusinessChatLink {
    text: td_api::FormattedText,
    title: String,
}

impl InputBusinessChatLink {
    /// `InputBusinessChatLink(td, inputBusinessChatLink)`
    /// (InputBusinessChatLink.cpp:13-26).
    pub fn from_td_api(
        env: &mut impl BusinessEnv,
        link: Option<td_api::InputBusinessChatLink>,
    ) -> InputBusinessChatLink {
        let mut result = InputBusinessChatLink {
            text: td_api::FormattedText::default(),
            title: String::new(),
        };
        let Some(link) = link else {
            return result;
        };
        match env.get_formatted_text_input(link.text) {
            Ok(text) => result.text = text,
            Err(status) => {
                tdutils::log_info!("Ignore draft text: {}", status.message());
            }
        }
        if crate::misc::clean_input_string(&link.title).is_some() {
            result.title = link.title;
        }
        result
    }

    /// `get_input_business_chat_link` (InputBusinessChatLink.cpp:28-38) —
    /// the ENTITIES/TITLE flag positions over the cleaned text.
    pub fn get_input_business_chat_link(
        &self,
        env: &mut impl BusinessEnv,
    ) -> tdmtproto::telegram_api::InputBusinessChatLink {
        let mut flags = 0;
        let entities = env.get_input_message_entities(&self.text);
        if !entities.is_empty() {
            flags |= 1; // ENTITIES_MASK
        }
        if !self.title.is_empty() {
            flags |= 1 << 1; // TITLE_MASK
        }
        tdmtproto::telegram_api::InputBusinessChatLink {
            flags,
            message: self.text.text.clone(),
            entities,
            title: self.title.clone(),
        }
    }
}

/// The business-plane in-flight query kinds — the `Td::ResultHandler`
/// classes of BusinessManager.cpp/BusinessConnectionManager.cpp (the
/// net-drain 26 families). The completions park by kind behind the
/// manager-held net handle; `Td::on_business_net_answer` routes them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum BusinessNetKind {
    /// `GetConnectedBotsQuery` (BusinessManager.cpp:49).
    GetConnectedBots,
    /// `UpdateConnectedBotQuery` (BusinessManager.cpp:87) — both the set
    /// form (the RIGHTS_MASK flag) and the delete form (the `deleted`
    /// flag over the empty recipients); the ok half is Updates-typed.
    UpdateConnectedBot,
    /// `ConfirmBotConnectionQuery` (BusinessManager.cpp:71).
    ConfirmBotConnection,
    /// `ToggleConnectedBotPausedQuery` (BusinessManager.cpp:133) — the
    /// handler's `dialog_id_` member (the `on_get_dialog_error` head).
    ToggleConnectedBotPaused { dialog_id: DialogId },
    /// `DisablePeerConnectedBotQuery` (BusinessManager.cpp:160) — the
    /// handler's `dialog_id_` member (the removed-bot intake + the
    /// error head).
    DisablePeerConnectedBot { dialog_id: DialogId },
    /// `GetBusinessChatLinksQuery` (BusinessManager.cpp:225).
    GetBusinessChatLinks,
    /// `CreateBusinessChatLinkQuery` (BusinessManager.cpp:272).
    CreateBusinessChatLink,
    /// `EditBusinessChatLinkQuery` (BusinessManager.cpp:299).
    EditBusinessChatLink,
    /// `DeleteBusinessChatLinkQuery` (BusinessManager.cpp:326).
    DeleteBusinessChatLink,
    /// `ResolveBusinessChatLinkQuery` (BusinessManager.cpp:352) — the
    /// text/dialog conversions run on the answer half.
    ResolveBusinessChatLink,
    /// `UpdateBusinessLocationQuery` (BusinessManager.cpp:398) — the
    /// handler's `location_` member (the my-user location tail).
    UpdateBusinessLocation,
    /// `UpdateBusinessWorkHoursQuery` (BusinessManager.cpp:434) — the
    /// handler's `work_hours_` member (the my-user work-hours tail).
    UpdateBusinessWorkHours,
    /// `UpdateBusinessGreetingMessageQuery` (BusinessManager.cpp:466) —
    /// the handler's `greeting_message_` member (the my-user tail).
    UpdateBusinessGreetingMessage,
    /// `UpdateBusinessAwayMessageQuery` (BusinessManager.cpp:498) — the
    /// handler's `away_message_` member (the my-user tail).
    UpdateBusinessAwayMessage,
    /// `UpdateBusinessIntroQuery` (BusinessManager.cpp:530) — the
    /// handler's `intro_` member (the my-user tail).
    UpdateBusinessIntro,
    /// `UpdateBusinessProfileQuery` (BusinessConnectionManager.cpp:635)
    /// — the name/about setter pair behind one wire query (the flags
    /// pick the halves); the `user_id_`/`set_name_`/`set_about_`/
    /// `first_name_`/`last_name_` members park beside the promise.
    UpdateBusinessProfile,
    /// `UpdateBusinessUsernameQuery` (BusinessConnectionManager.cpp:691)
    /// — the handler's `user_id_` member (the `on_get_user` intake).
    UpdateBusinessUsername,
    /// `UpdateBusinessGiftSettingsQuery`
    /// (BusinessConnectionManager.cpp:723) — the handler's `user_id_`
    /// member (the `invalidate_user_full` tail).
    UpdateBusinessGiftSettings,
    /// `GetBusinessStarsStatusQuery` (BusinessConnectionManager.cpp:764)
    /// — the balance must be the plain starsAmount (the 500 band).
    GetBusinessStarsStatus,
    /// `GetBusinessStarTransferPaymentFormQuery`
    /// (BusinessConnectionManager.cpp:826) — the transfer head; the
    /// price-check band and the `payments_sendStarsForm` tail ride the
    /// answer half.
    GetBusinessStarTransferPaymentForm { star_count: i64 },
    /// `TransferBusinessStarsQuery` (BusinessConnectionManager.cpp:791)
    /// — the send tail of the transfer chain; the
    /// FORM_SUBMIT_DUPLICATE log band on the error half.
    TransferBusinessStars,
}

/// The per-family parked promises (and the handler members the answer
/// halves re-read) — one dedicated vec per kind so the answers never
/// cross-settle (the net-drain 22 convention).
#[derive(Default)]
pub(crate) struct BusinessPendingQueries {
    pub(crate) get_connected_bots: Vec<Promise<Option<td_api::BusinessConnectedBotInfo>>>,
    pub(crate) update_connected_bot: Vec<Promise<()>>,
    pub(crate) confirm_bot_connection: Vec<Promise<()>>,
    pub(crate) toggle_connected_bot_paused: Vec<Promise<()>>,
    pub(crate) disable_peer_connected_bot: Vec<Promise<()>>,
    pub(crate) get_business_chat_links: Vec<Promise<td_api::BusinessChatLinks>>,
    pub(crate) create_business_chat_link: Vec<Promise<td_api::BusinessChatLink>>,
    pub(crate) edit_business_chat_link: Vec<Promise<td_api::BusinessChatLink>>,
    pub(crate) delete_business_chat_link: Vec<Promise<()>>,
    pub(crate) resolve_business_chat_link: Vec<Promise<td_api::BusinessChatLinkInfo>>,
    pub(crate) update_business_location: Vec<(crate::chat_manager::DialogLocation, Promise<()>)>,
    pub(crate) update_business_work_hours: Vec<(BusinessWorkHours, Promise<()>)>,
    pub(crate) update_business_greeting_message: Vec<(BusinessGreetingMessage, Promise<()>)>,
    pub(crate) update_business_away_message: Vec<(BusinessAwayMessage, Promise<()>)>,
    pub(crate) update_business_intro: Vec<(BusinessIntro, Promise<()>)>,
    pub(crate) update_business_profile: Vec<(BusinessProfileMembers, Promise<()>)>,
    pub(crate) update_business_username: Vec<(UserId, Promise<()>)>,
    pub(crate) update_business_gift_settings: Vec<(UserId, Promise<()>)>,
    pub(crate) get_business_stars_status: Vec<Promise<td_api::StarAmount>>,
    pub(crate) get_business_star_transfer_payment_form: Vec<(BusinessTransferChain, Promise<()>)>,
    pub(crate) transfer_business_stars: Vec<Promise<()>>,
}

/// The `UpdateBusinessProfileQuery` handler members
/// (BusinessConnectionManager.cpp:637-646) — which halves the flags
/// picked and the names they carry (the answer-half user tails).
pub(crate) struct BusinessProfileMembers {
    pub(crate) user_id: UserId,
    pub(crate) set_name: bool,
    pub(crate) set_about: bool,
    pub(crate) first_name: String,
    pub(crate) last_name: String,
}

/// The `GetBusinessStarTransferPaymentFormQuery` handler members —
/// the connection the chain tail re-dispatches over (the
/// `business_connection_id_`/`star_count_` pair of
/// BusinessConnectionManager.cpp:828-833).
pub(crate) struct BusinessTransferChain {
    pub(crate) connection_id: crate::business_connection_manager::BusinessConnectionId,
    pub(crate) dc_id: i32,
    pub(crate) star_count: i64,
}

/// `td::BusinessManager` (BusinessManager.h:27-71).
#[derive(Default)]
pub struct BusinessManager {
    /// The parked query promises.
    pub(crate) pending: BusinessPendingQueries,
    /// The net dispatch surface — the `td_` back-pointer the C++ manager
    /// uses for its query sends, narrowed to the net plane (`None` in
    /// the net-inert tests; set at the dispatcher install).
    pub(crate) net: Option<crate::td::BusinessNetHandle>,
}

impl BusinessManager {
    pub fn new() -> BusinessManager {
        BusinessManager::default()
    }

    /// The `td_->create_handler<…>()->send(...)` collapse — the wire
    /// query construction lives at the manager send sites; `None` keeps
    /// the net-inert posture (the tests drive the answer halves).
    fn send_business_net_query(
        &self,
        kind: BusinessNetKind,
        function: tdmtproto::telegram_api::Function,
        what: &str,
    ) {
        match &self.net {
            Some(net) => net.dispatch(kind, function),
            None => tdutils::log_info!("Send {what} — [net-inert]"),
        }
    }

    /// The `create_with_prefix` form (NetQueryCreator.cpp:36-39) — the
    /// business-connection invoke prefix rides the serialized head and
    /// the query dispatches on the connection's DC; `None` keeps the
    /// net-inert posture.
    fn send_business_net_query_with_prefix(
        &self,
        kind: BusinessNetKind,
        connection_id: &crate::business_connection_manager::BusinessConnectionId,
        function: tdmtproto::telegram_api::Function,
        dc_id: i32,
        what: &str,
    ) {
        match &self.net {
            Some(net) => net.dispatch_with_prefix(
                kind,
                tdmtproto::telegram_api::Function::invokeWithBusinessConnectionPrefix(
                    tdmtproto::telegram_api::invokeWithBusinessConnectionPrefix {
                        connection_id: connection_id.get().to_owned(),
                    },
                ),
                function,
                tdnet::net_query::DcId::from_value(dc_id),
            ),
            None => tdutils::log_info!("Send {what} (business) — [net-inert]"),
        }
    }

    /// The send-leg failure band — the C++ `ResultHandler::send` that
    /// calls `on_error(Status)` inline when it can't resolve the inputs
    /// (the "Can't access the chat" 400); the net-inert form logs.
    fn complete_business_net_error(&mut self, kind: BusinessNetKind, code: i32, message: &str) {
        match &self.net {
            Some(net) => net.complete_error(kind, code, message),
            None => tdutils::log_info!("Send error {message} — [net-inert]"),
        }
    }

    // -- the connected-bot families (BusinessManager.cpp:564-611) --

    /// `get_business_connected_bot` (BusinessManager.cpp:564-567).
    pub fn get_business_connected_bot(
        &mut self,
        _env: &mut impl BusinessEnv,
        promise: Promise<Option<td_api::BusinessConnectedBotInfo>>,
    ) {
        // `GetConnectedBotsQuery::send` (BusinessManager.cpp:54-56)
        self.send_business_net_query(
            BusinessNetKind::GetConnectedBots,
            tdmtproto::telegram_api::Function::account_getConnectedBots(
                tdmtproto::telegram_api::account_getConnectedBots {},
            ),
            "account.getConnectedBots",
        );
        self.pending.get_connected_bots.push(promise);
    }

    /// `set_business_connected_bot` (BusinessManager.cpp:569-577).
    pub fn set_business_connected_bot(
        &mut self,
        env: &mut impl BusinessEnv,
        bot: Option<td_api::BusinessConnectedBot>,
        mut promise: Promise<()>,
    ) {
        let Some(bot) = bot else {
            promise.set_error(Status::error(400, "Bot must be non-empty"));
            return;
        };
        let connected_bot = BusinessConnectedBot::from_td_api(Some(bot));
        match env.get_input_user(connected_bot.get_user_id()) {
            Ok(input_user) => {
                // `UpdateConnectedBotQuery::send(bot, input_user)`
                // (BusinessManager.cpp:96-103) — the RIGHTS_MASK flag
                // over the bot's rights/recipients.
                self.send_business_net_query(
                    BusinessNetKind::UpdateConnectedBot,
                    tdmtproto::telegram_api::Function::account_updateConnectedBot(
                        tdmtproto::telegram_api::account_updateConnectedBot {
                            flags: 1, // RIGHTS_MASK
                            deleted: false,
                            rights: Some(
                                connected_bot.get_rights().get_input_business_bot_rights(),
                            ),
                            bot: Some(input_user),
                            recipients: Some(
                                connected_bot
                                    .get_recipients()
                                    .get_input_business_bot_recipients(env),
                            ),
                        },
                    ),
                    "account.updateConnectedBot",
                );
                self.pending.update_connected_bot.push(promise);
            }
            Err(status) => promise.set_error(status),
        }
    }

    /// `confirm_business_connected_bot` (BusinessManager.cpp:579-583).
    pub fn confirm_business_connected_bot(
        &mut self,
        env: &mut impl BusinessEnv,
        bot_user_id: UserId,
        mut promise: Promise<()>,
    ) {
        match env.get_input_user(bot_user_id) {
            Ok(input_user) => {
                env.on_confirm_authorization(true, 0, bot_user_id);
                // `ConfirmBotConnectionQuery::send` (BusinessManager.cpp:76-79)
                self.send_business_net_query(
                    BusinessNetKind::ConfirmBotConnection,
                    tdmtproto::telegram_api::Function::account_confirmBotConnection(
                        tdmtproto::telegram_api::account_confirmBotConnection {
                            bot_id: Some(input_user),
                        },
                    ),
                    "account.confirmBotConnection",
                );
                self.pending.confirm_bot_connection.push(promise);
            }
            Err(status) => promise.set_error(status),
        }
    }

    /// `delete_business_connected_bot` (BusinessManager.cpp:585-589).
    pub fn delete_business_connected_bot(
        &mut self,
        env: &mut impl BusinessEnv,
        bot_user_id: UserId,
        mut promise: Promise<()>,
    ) {
        match env.get_input_user(bot_user_id) {
            Ok(input_user) => {
                env.on_confirm_authorization(true, 0, bot_user_id);
                // `UpdateConnectedBotQuery::send(input_user)` — the
                // delete form (BusinessManager.cpp:105-111): no flags,
                // `deleted`, the empty recipients.
                self.send_business_net_query(
                    BusinessNetKind::UpdateConnectedBot,
                    tdmtproto::telegram_api::Function::account_updateConnectedBot(
                        tdmtproto::telegram_api::account_updateConnectedBot {
                            flags: 0,
                            deleted: true,
                            rights: None,
                            bot: Some(input_user),
                            recipients: Some(
                                crate::business_recipients::BusinessRecipients::default()
                                    .get_input_business_bot_recipients(env),
                            ),
                        },
                    ),
                    "account.updateConnectedBot",
                );
                self.pending.update_connected_bot.push(promise);
            }
            Err(status) => promise.set_error(status),
        }
    }

    /// `toggle_business_connected_bot_dialog_is_paused`
    /// (BusinessManager.cpp:591-601). The dialog-access check runs in the
    /// request arm on `Td` (the master dialog plane has no instance
    /// method yet); the status is passed in.
    pub fn toggle_business_connected_bot_dialog_is_paused(
        &mut self,
        env: &mut impl BusinessEnv,
        dialog_id: DialogId,
        is_paused: bool,
        dialog_status: Status,
        mut promise: Promise<()>,
    ) {
        if dialog_status.is_error() {
            promise.set_error(dialog_status);
            return;
        }
        if dialog_id.get_type() != crate::dialog_id::DialogType::User {
            promise.set_error(Status::error(400, "The chat has no connected bot"));
            return;
        }
        env.on_update_dialog_business_bot_is_paused(dialog_id, is_paused);
        let kind = BusinessNetKind::ToggleConnectedBotPaused { dialog_id };
        // `ToggleConnectedBotPausedQuery::send`
        // (BusinessManager.cpp:141-148) — the Write-access input peer at
        // the send leg (the C++ CHECK resolves through the same surface;
        // the None band files the 400 through the answer half).
        let Some(input_peer) =
            env.get_input_peer(dialog_id, crate::user_manager::AccessRights::Write)
        else {
            promise.set_error(Status::error(400, "Can't access the chat"));
            return;
        };
        self.send_business_net_query(
            kind,
            tdmtproto::telegram_api::Function::account_toggleConnectedBotPaused(
                tdmtproto::telegram_api::account_toggleConnectedBotPaused {
                    peer: Some(Box::new(input_peer)),
                    paused: is_paused,
                },
            ),
            "account.toggleConnectedBotPaused",
        );
        self.pending.toggle_connected_bot_paused.push(promise);
    }

    /// `remove_business_connected_bot_from_dialog`
    /// (BusinessManager.cpp:603-611). The dialog-access check runs in the
    /// request arm on `Td`; the status is passed in.
    pub fn remove_business_connected_bot_from_dialog(
        &mut self,
        env: &mut impl BusinessEnv,
        dialog_id: DialogId,
        dialog_status: Status,
        mut promise: Promise<()>,
    ) {
        if dialog_status.is_error() {
            promise.set_error(dialog_status);
            return;
        }
        if dialog_id.get_type() != crate::dialog_id::DialogType::User {
            promise.set_error(Status::error(400, "The chat has no connected bot"));
            return;
        }
        env.on_update_dialog_business_bot_removed(dialog_id);
        let kind = BusinessNetKind::DisablePeerConnectedBot { dialog_id };
        // `DisablePeerConnectedBotQuery::send`
        // (BusinessManager.cpp:168-175) — the Write-access input peer at
        // the send leg.
        let Some(input_peer) =
            env.get_input_peer(dialog_id, crate::user_manager::AccessRights::Write)
        else {
            promise.set_error(Status::error(400, "Can't access the chat"));
            return;
        };
        self.send_business_net_query(
            kind,
            tdmtproto::telegram_api::Function::account_disablePeerConnectedBot(
                tdmtproto::telegram_api::account_disablePeerConnectedBot {
                    peer: Some(Box::new(input_peer)),
                },
            ),
            "account.disablePeerConnectedBot",
        );
        self.pending.disable_peer_connected_bot.push(promise);
    }

    // -- the chat-link families (BusinessManager.cpp:613-637) --

    /// `get_business_chat_links` (BusinessManager.cpp:613-615).
    pub fn get_business_chat_links(
        &mut self,
        _env: &mut impl BusinessEnv,
        promise: Promise<td_api::BusinessChatLinks>,
    ) {
        // `GetBusinessChatLinksQuery::send` (BusinessManager.cpp:233-235)
        self.send_business_net_query(
            BusinessNetKind::GetBusinessChatLinks,
            tdmtproto::telegram_api::Function::account_getBusinessChatLinks(
                tdmtproto::telegram_api::account_getBusinessChatLinks {},
            ),
            "account.getBusinessChatLinks",
        );
        self.pending.get_business_chat_links.push(promise);
    }

    /// `create_business_chat_link` (BusinessManager.cpp:617-621).
    pub fn create_business_chat_link(
        &mut self,
        env: &mut impl BusinessEnv,
        link_info: Option<td_api::InputBusinessChatLink>,
        promise: Promise<td_api::BusinessChatLink>,
    ) {
        // `CreateBusinessChatLinkQuery::send` (BusinessManager.cpp:280-285)
        let link = InputBusinessChatLink::from_td_api(env, link_info);
        self.send_business_net_query(
            BusinessNetKind::CreateBusinessChatLink,
            tdmtproto::telegram_api::Function::account_createBusinessChatLink(
                tdmtproto::telegram_api::account_createBusinessChatLink {
                    link: Some(link.get_input_business_chat_link(env)),
                },
            ),
            "account.createBusinessChatLink",
        );
        self.pending.create_business_chat_link.push(promise);
    }

    /// `edit_business_chat_link` (BusinessManager.cpp:623-628).
    pub fn edit_business_chat_link(
        &mut self,
        env: &mut impl BusinessEnv,
        link: &str,
        link_info: Option<td_api::InputBusinessChatLink>,
        promise: Promise<td_api::BusinessChatLink>,
    ) {
        // `EditBusinessChatLinkQuery::send` (BusinessManager.cpp:307-312)
        let input_link = InputBusinessChatLink::from_td_api(env, link_info);
        self.send_business_net_query(
            BusinessNetKind::EditBusinessChatLink,
            tdmtproto::telegram_api::Function::account_editBusinessChatLink(
                tdmtproto::telegram_api::account_editBusinessChatLink {
                    slug: link.to_owned(),
                    link: Some(input_link.get_input_business_chat_link(env)),
                },
            ),
            "account.editBusinessChatLink",
        );
        self.pending.edit_business_chat_link.push(promise);
    }

    /// `delete_business_chat_link` (BusinessManager.cpp:630-632).
    pub fn delete_business_chat_link(
        &mut self,
        _env: &mut impl BusinessEnv,
        link: &str,
        promise: Promise<()>,
    ) {
        // `DeleteBusinessChatLinkQuery::send` (BusinessManager.cpp:334-337)
        self.send_business_net_query(
            BusinessNetKind::DeleteBusinessChatLink,
            tdmtproto::telegram_api::Function::account_deleteBusinessChatLink(
                tdmtproto::telegram_api::account_deleteBusinessChatLink {
                    slug: link.to_owned(),
                },
            ),
            "account.deleteBusinessChatLink",
        );
        self.pending.delete_business_chat_link.push(promise);
    }

    /// `get_business_chat_link_info` (BusinessManager.cpp:634-637).
    pub fn get_business_chat_link_info(
        &mut self,
        _env: &mut impl BusinessEnv,
        link: &str,
        promise: Promise<td_api::BusinessChatLinkInfo>,
    ) {
        // `ResolveBusinessChatLinkQuery::send` (BusinessManager.cpp:360-363)
        self.send_business_net_query(
            BusinessNetKind::ResolveBusinessChatLink,
            tdmtproto::telegram_api::Function::account_resolveBusinessChatLink(
                tdmtproto::telegram_api::account_resolveBusinessChatLink {
                    slug: link.to_owned(),
                },
            ),
            "account.resolveBusinessChatLink",
        );
        self.pending.resolve_business_chat_link.push(promise);
    }

    // -- the five update setters (BusinessManager.cpp:639-658) --

    /// `set_business_location` (BusinessManager.cpp:639-641).
    pub fn set_business_location(
        &mut self,
        _env: &mut impl BusinessEnv,
        location: crate::chat_manager::DialogLocation,
        promise: Promise<()>,
    ) {
        // `UpdateBusinessLocationQuery::send` (BusinessManager.cpp:405-417)
        // — the GEO_POINT(1<<1)/ADDRESS(1) flag positions over the moved
        // member. The C++ `!location_.empty()` is exactly the geo-point
        // presence (DialogLocation.cpp:48-50 checks `location_` only;
        // the Rust `empty()` deliberately folds the address in — the
        // `dialog_location_round_trips` pin), so the flag reads the wire
        // form directly.
        let geo_point = location.get_input_geo_point();
        let has_geo_point = !matches!(
            geo_point,
            tdmtproto::telegram_api::InputGeoPoint::inputGeoPointEmpty(_)
        );
        let address = location.get_address().to_owned();
        let flags = (has_geo_point as i32) << 1 | (!address.is_empty() as i32);
        self.send_business_net_query(
            BusinessNetKind::UpdateBusinessLocation,
            tdmtproto::telegram_api::Function::account_updateBusinessLocation(
                tdmtproto::telegram_api::account_updateBusinessLocation {
                    flags,
                    geo_point: has_geo_point.then_some(geo_point),
                    address,
                },
            ),
            "account.updateBusinessLocation",
        );
        self.pending
            .update_business_location
            .push((location, promise));
    }

    /// `set_business_work_hours` (BusinessManager.cpp:643-645).
    pub fn set_business_work_hours(
        &mut self,
        _env: &mut impl BusinessEnv,
        work_hours: BusinessWorkHours,
        promise: Promise<()>,
    ) {
        // `UpdateBusinessWorkHoursQuery::send` (BusinessManager.cpp:441-451)
        // — the BUSINESS_WORK_HOURS flag over the moved member.
        let flags = if work_hours.is_empty() { 0 } else { 1 };
        let business_work_hours = (flags != 0)
            .then(|| work_hours.get_input_business_work_hours())
            .flatten();
        self.send_business_net_query(
            BusinessNetKind::UpdateBusinessWorkHours,
            tdmtproto::telegram_api::Function::account_updateBusinessWorkHours(
                tdmtproto::telegram_api::account_updateBusinessWorkHours {
                    flags,
                    business_work_hours,
                },
            ),
            "account.updateBusinessWorkHours",
        );
        self.pending
            .update_business_work_hours
            .push((work_hours, promise));
    }

    /// `set_business_greeting_message` (BusinessManager.cpp:647-650).
    pub fn set_business_greeting_message(
        &mut self,
        env: &mut impl BusinessEnv,
        greeting_message: BusinessGreetingMessage,
        promise: Promise<()>,
    ) {
        // `UpdateBusinessGreetingMessageQuery::send`
        // (BusinessManager.cpp:473-483) — the MESSAGE flag over the moved
        // member.
        let flags = if greeting_message.is_empty() { 0 } else { 1 };
        let message =
            (flags != 0).then(|| greeting_message.get_input_business_greeting_message(env));
        self.send_business_net_query(
            BusinessNetKind::UpdateBusinessGreetingMessage,
            tdmtproto::telegram_api::Function::account_updateBusinessGreetingMessage(
                tdmtproto::telegram_api::account_updateBusinessGreetingMessage { flags, message },
            ),
            "account.updateBusinessGreetingMessage",
        );
        self.pending
            .update_business_greeting_message
            .push((greeting_message, promise));
    }

    /// `set_business_away_message` (BusinessManager.cpp:652-654).
    pub fn set_business_away_message(
        &mut self,
        env: &mut impl BusinessEnv,
        away_message: BusinessAwayMessage,
        promise: Promise<()>,
    ) {
        // `UpdateBusinessAwayMessageQuery::send`
        // (BusinessManager.cpp:505-515) — the MESSAGE flag over the moved
        // member.
        let flags = if away_message.is_empty() { 0 } else { 1 };
        let message = (flags != 0).then(|| away_message.get_input_business_away_message(env));
        self.send_business_net_query(
            BusinessNetKind::UpdateBusinessAwayMessage,
            tdmtproto::telegram_api::Function::account_updateBusinessAwayMessage(
                tdmtproto::telegram_api::account_updateBusinessAwayMessage { flags, message },
            ),
            "account.updateBusinessAwayMessage",
        );
        self.pending
            .update_business_away_message
            .push((away_message, promise));
    }

    /// `set_business_intro` (BusinessManager.cpp:656-658).
    pub fn set_business_intro(
        &mut self,
        env: &mut impl BusinessEnv,
        intro: BusinessIntro,
        promise: Promise<()>,
    ) {
        // `UpdateBusinessIntroQuery::send` (BusinessManager.cpp:537-547)
        // — the INTRO flag over the moved member.
        let flags = if intro.is_empty() { 0 } else { 1 };
        let intro_value = (flags != 0).then(|| intro.get_input_business_intro(env));
        self.send_business_net_query(
            BusinessNetKind::UpdateBusinessIntro,
            tdmtproto::telegram_api::Function::account_updateBusinessIntro(
                tdmtproto::telegram_api::account_updateBusinessIntro {
                    flags,
                    intro: intro_value,
                },
            ),
            "account.updateBusinessIntro",
        );
        self.pending.update_business_intro.push((intro, promise));
    }

    // -- the business-connection families (BusinessConnectionManager.cpp:635-905) --

    /// `set_business_name` send tail — `UpdateBusinessProfileQuery::send`
    /// with the name halves (BusinessConnectionManager.cpp:647-667).
    pub(crate) fn send_update_business_name_query(
        &mut self,
        connection_id: &crate::business_connection_manager::BusinessConnectionId,
        dc_id: i32,
        user_id: UserId,
        first_name: &str,
        last_name: &str,
        promise: Promise<()>,
    ) {
        let first_name = crate::misc::clean_name(first_name, 64);
        let last_name = crate::misc::clean_name(last_name, 64);
        let members = BusinessProfileMembers {
            user_id,
            set_name: true,
            set_about: false,
            first_name: first_name.clone(),
            last_name: last_name.clone(),
        };
        // FIRST_NAME|LAST_NAME flag positions (the C++ sets both).
        self.send_business_net_query_with_prefix(
            BusinessNetKind::UpdateBusinessProfile,
            connection_id,
            tdmtproto::telegram_api::Function::account_updateProfile(
                tdmtproto::telegram_api::account_updateProfile {
                    flags: 1 | (1 << 1), // FIRST_NAME_MASK | LAST_NAME_MASK
                    first_name,
                    last_name,
                    about: String::new(),
                },
            ),
            dc_id,
            "account.updateProfile",
        );
        self.pending
            .update_business_profile
            .push((members, promise));
    }

    /// `set_business_about` send tail — `UpdateBusinessProfileQuery::send`
    /// with the about half (BusinessConnectionManager.cpp:647-667).
    pub(crate) fn send_update_business_about_query(
        &mut self,
        connection_id: &crate::business_connection_manager::BusinessConnectionId,
        dc_id: i32,
        user_id: UserId,
        about: &str,
        promise: Promise<()>,
    ) {
        let members = BusinessProfileMembers {
            user_id,
            set_name: false,
            set_about: true,
            first_name: String::new(),
            last_name: String::new(),
        };
        self.send_business_net_query_with_prefix(
            BusinessNetKind::UpdateBusinessProfile,
            connection_id,
            tdmtproto::telegram_api::Function::account_updateProfile(
                tdmtproto::telegram_api::account_updateProfile {
                    flags: 1 << 2, // ABOUT_MASK
                    first_name: String::new(),
                    last_name: String::new(),
                    about: about.to_owned(),
                },
            ),
            dc_id,
            "account.updateProfile",
        );
        self.pending
            .update_business_profile
            .push((members, promise));
    }

    /// `set_business_username` send tail —
    /// `UpdateBusinessUsernameQuery::send`
    /// (BusinessConnectionManager.cpp:699-705).
    pub(crate) fn send_update_business_username_query(
        &mut self,
        connection_id: &crate::business_connection_manager::BusinessConnectionId,
        dc_id: i32,
        user_id: UserId,
        username: &str,
        promise: Promise<()>,
    ) {
        self.send_business_net_query_with_prefix(
            BusinessNetKind::UpdateBusinessUsername,
            connection_id,
            tdmtproto::telegram_api::Function::account_updateUsername(
                tdmtproto::telegram_api::account_updateUsername {
                    username: username.to_owned(),
                },
            ),
            dc_id,
            "account.updateUsername",
        );
        self.pending
            .update_business_username
            .push((user_id, promise));
    }

    /// `set_business_gift_settings` send tail —
    /// `UpdateBusinessGiftSettingsQuery::send`
    /// (BusinessConnectionManager.cpp:731-746) — the
    /// account.setGlobalPrivacySettings form over the disallowed-gifts
    /// settings.
    pub(crate) fn send_update_business_gift_settings_query(
        &mut self,
        connection_id: &crate::business_connection_manager::BusinessConnectionId,
        dc_id: i32,
        user_id: UserId,
        settings: Option<td_api::GiftSettings>,
        promise: Promise<()>,
    ) {
        let settings =
            crate::star_gift_settings::StarGiftSettings::from_gift_settings(settings.as_ref());
        let mut flags = 0;
        let disallowed_gifts = settings
            .get_disallowed_gifts()
            .get_input_disallowed_gifts_settings();
        if disallowed_gifts.is_some() {
            flags |= 1 << 6; // DISALLOWED_GIFTS_MASK
        }
        self.send_business_net_query_with_prefix(
            BusinessNetKind::UpdateBusinessGiftSettings,
            connection_id,
            tdmtproto::telegram_api::Function::account_setGlobalPrivacySettings(
                tdmtproto::telegram_api::account_setGlobalPrivacySettings {
                    settings: Some(tdmtproto::telegram_api::GlobalPrivacySettings {
                        flags,
                        display_gifts_button: settings.get_display_gifts_button(),
                        disallowed_gifts,
                        ..tdmtproto::telegram_api::GlobalPrivacySettings::default()
                    }),
                },
            ),
            dc_id,
            "account.setGlobalPrivacySettings",
        );
        self.pending
            .update_business_gift_settings
            .push((user_id, promise));
    }

    /// `get_business_star_status` send tail —
    /// `GetBusinessStarsStatusQuery::send`
    /// (BusinessConnectionManager.cpp:772-784) — the Read-access input
    /// peer of the connection's user at the send leg (the null band is
    /// the synchronous "Can't access the chat" 400).
    pub(crate) fn send_get_business_stars_status_query(
        &mut self,
        env: &mut impl BusinessEnv,
        connection_id: &crate::business_connection_manager::BusinessConnectionId,
        dc_id: i32,
        user_id: UserId,
        promise: Promise<td_api::StarAmount>,
    ) {
        let kind = BusinessNetKind::GetBusinessStarsStatus;
        let Some(input_peer) = env.get_input_peer(
            DialogId::from_user_id(user_id),
            crate::user_manager::AccessRights::Read,
        ) else {
            self.complete_business_net_error(kind, 400, "Can't access the chat");
            return;
        };
        self.send_business_net_query_with_prefix(
            kind,
            connection_id,
            tdmtproto::telegram_api::Function::payments_getStarsStatus(
                tdmtproto::telegram_api::payments_getStarsStatus {
                    flags: 0,
                    ton: false,
                    peer: Some(Box::new(input_peer)),
                },
            ),
            dc_id,
            "payments.getStarsStatus",
        );
        self.pending.get_business_stars_status.push(promise);
    }

    /// `transfer_business_stars` send tail —
    /// `GetBusinessStarTransferPaymentFormQuery::send`
    /// (BusinessConnectionManager.cpp:861-873) — the transfer chain head.
    pub(crate) fn send_transfer_business_stars_query(
        &mut self,
        connection_id: &crate::business_connection_manager::BusinessConnectionId,
        dc_id: i32,
        star_count: i64,
        promise: Promise<()>,
    ) {
        self.send_business_net_query_with_prefix(
            BusinessNetKind::GetBusinessStarTransferPaymentForm { star_count },
            connection_id,
            tdmtproto::telegram_api::Function::payments_getPaymentForm(
                tdmtproto::telegram_api::payments_getPaymentForm {
                    flags: 0,
                    invoice: Some(
                        tdmtproto::telegram_api::InputInvoice::inputInvoiceBusinessBotTransferStars(
                            input_invoice_business_bot_transfer_stars_self(star_count),
                        ),
                    ),
                    theme_params: None,
                },
            ),
            dc_id,
            "payments.getPaymentForm",
        );
        self.pending.get_business_star_transfer_payment_form.push((
            BusinessTransferChain {
                connection_id: connection_id.clone(),
                dc_id,
                star_count,
            },
            promise,
        ));
    }

    /// `TransferBusinessStarsQuery::send`
    /// (BusinessConnectionManager.cpp:807-819) — the chain tail the
    /// payment-form answer half dispatches (the form id + star count of
    /// the form).
    pub(crate) fn send_transfer_business_stars_form_query(
        &mut self,
        connection_id: &crate::business_connection_manager::BusinessConnectionId,
        dc_id: i32,
        payment_form_id: i64,
        star_count: i64,
        promise: Promise<()>,
    ) {
        self.send_business_net_query_with_prefix(
            BusinessNetKind::TransferBusinessStars,
            connection_id,
            tdmtproto::telegram_api::Function::payments_sendStarsForm(
                tdmtproto::telegram_api::payments_sendStarsForm {
                    form_id: payment_form_id,
                    invoice: Some(
                        tdmtproto::telegram_api::InputInvoice::inputInvoiceBusinessBotTransferStars(
                            input_invoice_business_bot_transfer_stars_self(star_count),
                        ),
                    ),
                },
            ),
            dc_id,
            "payments.sendStarsForm",
        );
        self.pending.transfer_business_stars.push(promise);
    }
}

/// The `inputInvoiceBusinessBotTransferStars(inputUserSelf, star_count)`
/// form (BusinessConnectionManager.cpp:816-819/869-872).
fn input_invoice_business_bot_transfer_stars_self(
    star_count: i64,
) -> tdmtproto::telegram_api::inputInvoiceBusinessBotTransferStars {
    tdmtproto::telegram_api::inputInvoiceBusinessBotTransferStars {
        bot: Some(tdmtproto::telegram_api::InputUser::inputUserSelf(
            tdmtproto::telegram_api::inputUserSelf {},
        )),
        stars: star_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The seam-default env — the reads fall to the defaults (the
    /// get_input_user error band, the null input peer), which is exactly
    /// what the local-validation bands need.
    struct BandEnv;

    impl BusinessEnv for BandEnv {
        fn is_bot(&self) -> bool {
            false
        }
        fn get_time_zone_offset(&mut self, _time_zone_id: &str) -> i32 {
            0
        }
        fn get_option_integer(&mut self, _name: &str, default_value: i64) -> i64 {
            default_value
        }
    }

    fn recorded_unit() -> (
        Promise<()>,
        std::sync::Arc<std::sync::Mutex<Option<Status>>>,
    ) {
        let recorded = std::sync::Arc::new(std::sync::Mutex::new(None));
        let slot = recorded.clone();
        (
            tdutils::promise::PromiseCreator::lambda_result(move |result| {
                *slot.lock().unwrap() = Some(match result {
                    tdutils::status::Result::Ok(()) => Status::ok(),
                    tdutils::status::Result::Err(status) => status,
                });
            }),
            recorded,
        )
    }

    fn error_of(recorded: &std::sync::Arc<std::sync::Mutex<Option<Status>>>) -> (i32, String) {
        let status = recorded
            .lock()
            .unwrap()
            .clone()
            .expect("the settled promise");
        (status.code(), status.message().to_owned())
    }

    /// The `set_business_connected_bot` local-validation bands
    /// (BusinessManager.cpp:569-577).
    #[test]
    fn set_business_connected_bot_bands() {
        let mut manager = BusinessManager::new();
        let mut env = BandEnv;

        // the null bot
        let (promise, recorded) = recorded_unit();
        manager.set_business_connected_bot(&mut env, None, promise);
        assert_eq!(
            error_of(&recorded),
            (400, "Bot must be non-empty".to_owned())
        );

        // the unresolvable bot user (the seam-default input-user error)
        let (promise, recorded) = recorded_unit();
        manager.set_business_connected_bot(
            &mut env,
            Some(td_api::BusinessConnectedBot {
                bot_user_id: 1,
                recipients: None,
                rights: None,
            }),
            promise,
        );
        assert_eq!(error_of(&recorded), (400, "User not found".to_owned()));
    }

    /// The non-user-dialog bands of the toggle/remove pair
    /// (BusinessManager.cpp:591-611) — the dialog-access status is the
    /// request arm's check; the type check answers before the send.
    #[test]
    fn toggle_remove_reject_non_user_dialogs() {
        let mut manager = BusinessManager::new();
        let mut env = BandEnv;
        let chat_dialog = DialogId::new(-1002147483648); // a channel-shaped id

        let (promise, recorded) = recorded_unit();
        manager.toggle_business_connected_bot_dialog_is_paused(
            &mut env,
            chat_dialog,
            true,
            Status::ok(),
            promise,
        );
        assert_eq!(
            error_of(&recorded),
            (400, "The chat has no connected bot".to_owned())
        );

        let (promise, recorded) = recorded_unit();
        manager.remove_business_connected_bot_from_dialog(
            &mut env,
            chat_dialog,
            Status::ok(),
            promise,
        );
        assert_eq!(
            error_of(&recorded),
            (400, "The chat has no connected bot".to_owned())
        );
    }
}
