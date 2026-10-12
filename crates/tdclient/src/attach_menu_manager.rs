//! `AttachMenuManager` (td/telegram/AttachMenuManager.{h,cpp}) — the M.40
//! Wave-3 slice: the attachment-menu bots, their icons and the
//! add-to-attachment-menu toggle. The icon `FileId`s are real (the M.12
//! file plane): the wire document registers through
//! [`crate::files::FileManager::register_remote`] inside the
//! [`AttachMenuManagerEnv::on_get_icon_document`] seam — the narrow
//! `DocumentsManager::on_get_document` collapse (the General-vs-Sticker
//! type derivation reads the document attributes; the rest of the
//! DocumentsManager stays its own track). The `FileId` store/parse ride
//! [`crate::files::FileManager::store_file`]/[`crate::files::FileManager::parse_file`]
//! through the env, exactly like the photo stores. The three `Td::ResultHandler`
//! query classes (`GetAttachMenuBotsQuery`, `GetAttachMenuBotQuery`,
//! `ToggleBotInAttachMenuQuery`) collapse into the
//! [`AttachMenuManagerEnv`] query seams — `[blocked: net]` with the
//! promises parked (the M.31 convention); the two answer handlers are
//! real and unit-tested. The C++ actor timeout (the 1–1.33h successful
//! reload / 1–2min error retry) rides the shared Td timeout slot through
//! [`AttachMenuManager::tick_timeouts`]/[`AttachMenuManager::next_deadline`]
//! (the M.13 convention).

use crate::files::{FileId, FileSourceId};
use crate::store_parse::{self, Parser};
use crate::td_api;
use crate::user_manager::{BotData, UserId};
use std::collections::HashMap;
use tdmtproto::telegram_api;
use tdutils::promise::Promise;
use tdutils::status::Status;

/// The `G()`/`td_` surface of the manager. The net queries are the
/// `[blocked: net]` query seams; the `send_update` outbox is drained by
/// `crate::td::Td::flush_user_updates`.
pub trait AttachMenuManagerEnv {
    /// `G()->close_flag()`.
    fn close_flag(&self) -> bool;

    /// `td_->auth_manager_->is_authorized()`.
    fn is_authorized(&self) -> bool;

    /// `td_->auth_manager_->is_bot()`.
    fn is_bot(&self) -> bool;

    /// `G()->use_chat_info_database()`.
    fn use_chat_info_database(&self) -> bool;

    /// `G()->td_db()->get_binlog_pmc()->get`.
    fn binlog_pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8>;

    /// `G()->td_db()->get_binlog_pmc()->set`.
    fn binlog_pmc_set_raw(&mut self, key: &[u8], value: &[u8]);

    /// `G()->td_db()->get_binlog_pmc()->erase`.
    fn binlog_pmc_erase(&mut self, key: &str);

    /// `send_closure(G()->td(), &Td::send_update, ...)`.
    fn send_update(&mut self, update: td_api::Update);

    /// `td_->user_manager_->get_input_user(user_id)`.
    fn get_input_user(&mut self, user_id: UserId) -> Result<telegram_api::InputUser, Status>;

    /// `td_->user_manager_->get_bot_data(user_id)`.
    fn get_bot_data(&mut self, user_id: UserId) -> Result<BotData, Status>;

    /// `td_->user_manager_->have_accessible_user(user_id)`.
    fn have_accessible_user(&self, user_id: UserId) -> bool;

    /// `td_->user_manager_->get_user_id_object(user_id, source)`.
    fn get_user_id_object(&mut self, user_id: UserId) -> i64;

    /// `td_->user_manager_->on_get_users(users, source)`.
    fn on_get_users(&mut self, users: Vec<telegram_api::User>, source: &str);

    /// `td_->file_reference_manager_->create_attach_menu_bot_file_source`
    /// — the interned `FileSource::AttachMenuBot(user_id)`.
    fn get_attach_menu_bot_file_source_id(&mut self, user_id: UserId) -> FileSourceId;

    /// `DocumentsManager::on_get_document` for the icon documents
    /// (AttachMenuManager.cpp:472-478): derives the document type from the
    /// attributes (Sticker when a sticker attribute is present, General
    /// otherwise), registers the remote location and returns the `FileId`;
    /// an invalid `FileId` models the C++ type-mismatch `continue` (the
    /// `expected_sticker` half of the C++ `expected_document_type` check).
    fn on_get_icon_document(
        &mut self,
        document: &telegram_api::document,
        expected_sticker: bool,
    ) -> FileId;

    /// `td_->file_manager_->add_file_source(file_id, source_id, ...)`.
    fn add_file_source(&mut self, file_id: FileId, source_id: FileSourceId);

    /// `td_->file_manager_->get_file_object(file_id)`.
    fn get_file_object(&mut self, file_id: FileId) -> td_api::File;

    /// `td::store(FileId, storer)` — the FileManager-mediated
    /// serialization (`FileManager::store_file`, ttl 5).
    fn store_file_id(&mut self, file_id: FileId, out: &mut Vec<u8>);

    /// `parser.context()->td()...->file_manager_->parse_file(parser)`.
    fn parse_file_id(&mut self, parser: &mut Parser) -> Result<FileId, &'static str>;

    /// `GetAttachMenuBotsQuery::send` (cpp:43-45) — `[blocked: net]`; the
    /// answer arrives through
    /// [`AttachMenuManager::on_reload_attach_menu_bots`].
    fn send_get_attach_menu_bots_query(&mut self, hash: i64) {
        tdutils::log_info!("Send GetAttachMenuBots(hash = {hash}) — [blocked: net]");
    }

    /// `GetAttachMenuBotQuery::send` (cpp:71-73) — `[blocked: net]`; the
    /// answer arrives through
    /// [`AttachMenuManager::on_get_attach_menu_bot`].
    fn send_get_attach_menu_bot_query(&mut self, input_user: &telegram_api::InputUser) {
        tdutils::log_info!("Send GetAttachMenuBot — [blocked: net]");
        let _ = input_user;
    }

    /// `ToggleBotInAttachMenuQuery::send` (cpp:98-101) — `[blocked: net]`;
    /// the C++ success closure chains into `reload_attach_menu_bots` and
    /// the C++ `on_error` reloads + fails the promise (cpp:116-119) — both
    /// obligations ride the net-step integration.
    fn send_toggle_bot_in_attach_menu_query(
        &mut self,
        input_user: &telegram_api::InputUser,
        write_allowed: bool,
        is_added: bool,
    ) {
        tdutils::log_info!(
            "Send ToggleBotInAttachMenu(write_allowed = {write_allowed}, is_added = {is_added}) — [blocked: net]"
        );
        let _ = input_user;
    }
}

/// `AttachMenuManager::is_active` (cpp:440-442).
fn is_active(env: &impl AttachMenuManagerEnv) -> bool {
    !env.close_flag() && env.is_authorized() && !env.is_bot()
}

/// `td::fail_promises` (Promise.h) — fail the queue with a cloned error.
fn fail_promises<T: Send>(promises: &mut Vec<Promise<T>>, error: &Status) {
    for mut promise in std::mem::take(promises) {
        promise.set_error(error.clone());
    }
}

/// `AttachMenuManager::AttachMenuBotColor` (AttachMenuManager.h:54-63) —
/// the -1 defaults are the C++ member initializers.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct AttachMenuBotColor {
    light_color: i32,
    dark_color: i32,
}

impl Default for AttachMenuBotColor {
    fn default() -> AttachMenuBotColor {
        AttachMenuBotColor {
            light_color: -1,
            dark_color: -1,
        }
    }
}

impl AttachMenuBotColor {
    /// `AttachMenuBotColor::store` (cpp:130-134).
    fn store(&self, out: &mut Vec<u8>) {
        store_parse::store_i32(out, self.light_color);
        store_parse::store_i32(out, self.dark_color);
    }

    /// `AttachMenuBotColor::parse` (cpp:136-140).
    fn parse(parser: &mut Parser) -> Result<AttachMenuBotColor, &'static str> {
        let light_color = parser.i32_()?;
        let dark_color = parser.i32_()?;
        Ok(AttachMenuBotColor {
            light_color,
            dark_color,
        })
    }
}

/// `AttachMenuManager::AttachMenuBot` (AttachMenuManager.h:68-101).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AttachMenuBot {
    is_added: bool,
    user_id: UserId,
    supports_self_dialogs: bool,
    supports_user_dialogs: bool,
    supports_bot_dialogs: bool,
    supports_group_dialogs: bool,
    supports_broadcast_dialogs: bool,
    request_write_access: bool,
    show_in_attach_menu: bool,
    show_in_side_menu: bool,
    side_menu_disclaimer_needed: bool,
    name: String,
    name_color: AttachMenuBotColor,
    default_icon_file_id: FileId,
    ios_static_icon_file_id: FileId,
    ios_animated_icon_file_id: FileId,
    android_icon_file_id: FileId,
    macos_icon_file_id: FileId,
    android_side_menu_icon_file_id: FileId,
    ios_side_menu_icon_file_id: FileId,
    macos_side_menu_icon_file_id: FileId,
    icon_color: AttachMenuBotColor,
    placeholder_file_id: FileId,
    cache_version: u32,
}

impl Default for AttachMenuBot {
    /// the C++ member initializers; `FileId` defaults to the invalid id
    fn default() -> AttachMenuBot {
        AttachMenuBot {
            is_added: false,
            user_id: UserId::new(0),
            supports_self_dialogs: false,
            supports_user_dialogs: false,
            supports_bot_dialogs: false,
            supports_group_dialogs: false,
            supports_broadcast_dialogs: false,
            request_write_access: false,
            show_in_attach_menu: false,
            show_in_side_menu: false,
            side_menu_disclaimer_needed: false,
            name: String::new(),
            name_color: AttachMenuBotColor::default(),
            default_icon_file_id: FileId::new(0, 0),
            ios_static_icon_file_id: FileId::new(0, 0),
            ios_animated_icon_file_id: FileId::new(0, 0),
            android_icon_file_id: FileId::new(0, 0),
            macos_icon_file_id: FileId::new(0, 0),
            android_side_menu_icon_file_id: FileId::new(0, 0),
            ios_side_menu_icon_file_id: FileId::new(0, 0),
            macos_side_menu_icon_file_id: FileId::new(0, 0),
            icon_color: AttachMenuBotColor::default(),
            placeholder_file_id: FileId::new(0, 0),
            cache_version: 0,
        }
    }
}

impl AttachMenuBot {
    /// `AttachMenuBot::CACHE_VERSION` (AttachMenuManager.h:93).
    pub(crate) const CACHE_VERSION: u32 = 3;

    /// `AttachMenuBot::store` (cpp:166-241) — the FileId fields ride the
    /// FileManager-mediated serialization through the env.
    fn store(&self, env: &mut impl AttachMenuManagerEnv, out: &mut Vec<u8>) {
        let has_ios_static_icon_file_id = self.ios_static_icon_file_id.is_valid();
        let has_ios_animated_icon_file_id = self.ios_animated_icon_file_id.is_valid();
        let has_android_icon_file_id = self.android_icon_file_id.is_valid();
        let has_macos_icon_file_id = self.macos_icon_file_id.is_valid();
        let has_name_color = self.name_color != AttachMenuBotColor::default();
        let has_icon_color = self.icon_color != AttachMenuBotColor::default();
        let has_support_flags = true;
        let has_placeholder_file_id = self.placeholder_file_id.is_valid();
        let has_cache_version = self.cache_version != 0;
        let has_android_side_menu_icon_file_id = self.android_side_menu_icon_file_id.is_valid();
        let has_ios_side_menu_icon_file_id = self.ios_side_menu_icon_file_id.is_valid();
        let has_macos_side_menu_icon_file_id = self.macos_side_menu_icon_file_id.is_valid();
        store_parse::store_flags(
            out,
            [
                has_ios_static_icon_file_id,
                has_ios_animated_icon_file_id,
                has_android_icon_file_id,
                has_macos_icon_file_id,
                self.is_added,
                has_name_color,
                has_icon_color,
                has_support_flags,
                self.supports_self_dialogs,
                self.supports_user_dialogs,
                self.supports_bot_dialogs,
                self.supports_group_dialogs,
                self.supports_broadcast_dialogs,
                false,
                has_placeholder_file_id,
                has_cache_version,
                self.request_write_access,
                self.show_in_attach_menu,
                self.show_in_side_menu,
                self.side_menu_disclaimer_needed,
                has_android_side_menu_icon_file_id,
                has_ios_side_menu_icon_file_id,
                has_macos_side_menu_icon_file_id,
            ],
        );
        store_parse::store_i64(out, self.user_id.get());
        store_parse::store_string(out, &self.name);
        env.store_file_id(self.default_icon_file_id, out);
        if has_ios_static_icon_file_id {
            env.store_file_id(self.ios_static_icon_file_id, out);
        }
        if has_ios_animated_icon_file_id {
            env.store_file_id(self.ios_animated_icon_file_id, out);
        }
        if has_android_icon_file_id {
            env.store_file_id(self.android_icon_file_id, out);
        }
        if has_macos_icon_file_id {
            env.store_file_id(self.macos_icon_file_id, out);
        }
        if has_name_color {
            self.name_color.store(out);
        }
        if has_icon_color {
            self.icon_color.store(out);
        }
        if has_placeholder_file_id {
            env.store_file_id(self.placeholder_file_id, out);
        }
        if has_cache_version {
            store_parse::store_u32(out, self.cache_version);
        }
        if has_android_side_menu_icon_file_id {
            env.store_file_id(self.android_side_menu_icon_file_id, out);
        }
        if has_ios_side_menu_icon_file_id {
            env.store_file_id(self.ios_side_menu_icon_file_id, out);
        }
        if has_macos_side_menu_icon_file_id {
            env.store_file_id(self.macos_side_menu_icon_file_id, out);
        }
    }

    /// `AttachMenuBot::parse` (cpp:243-329) with the two parse fixups: the
    /// legacy support flags default and the is-added-implies-attach-menu
    /// flag.
    fn parse(
        env: &mut impl AttachMenuManagerEnv,
        parser: &mut Parser,
    ) -> Result<AttachMenuBot, &'static str> {
        let [
            has_ios_static_icon_file_id,
            has_ios_animated_icon_file_id,
            has_android_icon_file_id,
            has_macos_icon_file_id,
            is_added,
            has_name_color,
            has_icon_color,
            has_support_flags,
            supports_self_dialogs,
            supports_user_dialogs,
            supports_bot_dialogs,
            supports_group_dialogs,
            supports_broadcast_dialogs,
            _legacy_supports_settings,
            has_placeholder_file_id,
            has_cache_version,
            request_write_access,
            show_in_attach_menu,
            show_in_side_menu,
            side_menu_disclaimer_needed,
            has_android_side_menu_icon_file_id,
            has_ios_side_menu_icon_file_id,
            has_macos_side_menu_icon_file_id,
        ] = parser.parse_flags()?;
        let user_id = UserId::new(parser.i64_()?);
        let name = parser.string()?;
        let default_icon_file_id = env.parse_file_id(parser)?;
        let ios_static_icon_file_id = if has_ios_static_icon_file_id {
            env.parse_file_id(parser)?
        } else {
            FileId::new(0, 0)
        };
        let ios_animated_icon_file_id = if has_ios_animated_icon_file_id {
            env.parse_file_id(parser)?
        } else {
            FileId::new(0, 0)
        };
        let android_icon_file_id = if has_android_icon_file_id {
            env.parse_file_id(parser)?
        } else {
            FileId::new(0, 0)
        };
        let macos_icon_file_id = if has_macos_icon_file_id {
            env.parse_file_id(parser)?
        } else {
            FileId::new(0, 0)
        };
        let name_color = if has_name_color {
            AttachMenuBotColor::parse(parser)?
        } else {
            AttachMenuBotColor::default()
        };
        let icon_color = if has_icon_color {
            AttachMenuBotColor::parse(parser)?
        } else {
            AttachMenuBotColor::default()
        };
        let placeholder_file_id = if has_placeholder_file_id {
            env.parse_file_id(parser)?
        } else {
            FileId::new(0, 0)
        };
        let cache_version = if has_cache_version { parser.u32_()? } else { 0 };
        let android_side_menu_icon_file_id = if has_android_side_menu_icon_file_id {
            env.parse_file_id(parser)?
        } else {
            FileId::new(0, 0)
        };
        let ios_side_menu_icon_file_id = if has_ios_side_menu_icon_file_id {
            env.parse_file_id(parser)?
        } else {
            FileId::new(0, 0)
        };
        let macos_side_menu_icon_file_id = if has_macos_side_menu_icon_file_id {
            env.parse_file_id(parser)?
        } else {
            FileId::new(0, 0)
        };

        // the legacy no-support-flags fixup (cpp:320-324)
        let (supports_self_dialogs, supports_user_dialogs, supports_bot_dialogs) =
            if !has_support_flags {
                (true, true, true)
            } else {
                (
                    supports_self_dialogs,
                    supports_user_dialogs,
                    supports_bot_dialogs,
                )
            };
        // the is-added-implies-show-in-attach-menu fixup (cpp:325-328)
        let show_in_attach_menu = if is_added
            && !show_in_attach_menu
            && !show_in_side_menu
            && !has_android_side_menu_icon_file_id
            && !has_ios_side_menu_icon_file_id
            && !has_macos_side_menu_icon_file_id
        {
            true
        } else {
            show_in_attach_menu
        };

        Ok(AttachMenuBot {
            is_added,
            user_id,
            supports_self_dialogs,
            supports_user_dialogs,
            supports_bot_dialogs,
            supports_group_dialogs,
            supports_broadcast_dialogs,
            request_write_access,
            show_in_attach_menu,
            show_in_side_menu,
            side_menu_disclaimer_needed,
            name,
            name_color,
            default_icon_file_id,
            ios_static_icon_file_id,
            ios_animated_icon_file_id,
            android_icon_file_id,
            macos_icon_file_id,
            android_side_menu_icon_file_id,
            ios_side_menu_icon_file_id,
            macos_side_menu_icon_file_id,
            icon_color,
            placeholder_file_id,
            cache_version,
        })
    }
}

/// `AttachMenuManager::AttachMenuBotsLogEvent` (cpp:331-353).
#[derive(Default)]
struct AttachMenuBotsLogEvent {
    hash: i64,
    attach_menu_bots: Vec<AttachMenuBot>,
}

impl AttachMenuBotsLogEvent {
    fn store(&self, env: &mut impl AttachMenuManagerEnv, out: &mut Vec<u8>) {
        store_parse::store_i64(out, self.hash);
        store_parse::store_i32(out, self.attach_menu_bots.len() as i32);
        for bot in &self.attach_menu_bots {
            bot.store(env, out);
        }
    }

    fn parse(
        env: &mut impl AttachMenuManagerEnv,
        parser: &mut Parser,
    ) -> Result<AttachMenuBotsLogEvent, &'static str> {
        let hash = parser.i64_()?;
        let size = parser.i32_()?;
        if size < 0 {
            return Err("invalid attach menu bots count");
        }
        let mut attach_menu_bots = Vec::with_capacity(size as usize);
        for _ in 0..size {
            attach_menu_bots.push(AttachMenuBot::parse(env, parser)?);
        }
        Ok(AttachMenuBotsLogEvent {
            hash,
            attach_menu_bots,
        })
    }
}

/// The `AttachMenuBots` wire answer split by constructor id — the C++
/// `on_reload_attach_menu_bots` checks the id before the typed parse (the
/// `config_result` precedent).
#[derive(Debug)]
pub enum AttachMenuBotsAnswer {
    NotModified,
    Bots(telegram_api::attachMenuBots),
}

/// The name table of the C++ `name[5]` switch (cpp:479-512): the icon
/// name → the slot it fills.
enum IconSlot {
    Default,
    IosStatic,
    IosAnimated,
    AndroidIcon,
    MacosIcon,
    Placeholder,
    AndroidSideMenu,
    IosSideMenu,
    MacosSideMenu,
}

/// The C++ `switch (name[5])` collapse; the caller checked the name
/// against the nine-name allow list, so the byte-index routing is total.
fn icon_slot(name: &str) -> Option<IconSlot> {
    let bytes = name.as_bytes();
    match bytes.get(5)? {
        b'l' => Some(IconSlot::Default),     // default_static
        b't' => Some(IconSlot::IosStatic),   // ios_static
        b'n' => Some(IconSlot::IosAnimated), // ios_animated
        b'i' => match bytes.get(8)? {
            b's' => Some(IconSlot::AndroidSideMenu), // android_side_menu_static
            b'_' => Some(IconSlot::IosSideMenu),     // ios_side_menu_static
            _ => Some(IconSlot::AndroidIcon),        // android_animated
        },
        b'_' => match bytes.get(6)? {
            b's' => Some(IconSlot::MacosSideMenu), // macos_side_menu_static
            _ => Some(IconSlot::MacosIcon),        // macos_animated
        },
        b'h' => Some(IconSlot::Placeholder), // placeholder_static
        _ => None,
    }
}

/// The C++ icon-name allow list (cpp:465-467).
fn is_known_icon_name(name: &str) -> bool {
    matches!(
        name,
        "default_static"
            | "ios_static"
            | "ios_animated"
            | "android_animated"
            | "macos_animated"
            | "placeholder_static"
            | "ios_side_menu_static"
            | "android_side_menu_static"
            | "macos_side_menu_static"
    )
}

/// The C++ `expect_colors` predicate — the "android_animated" slot only.
fn icon_expects_colors(name: &str) -> bool {
    matches!(icon_slot(name), Some(IconSlot::AndroidIcon))
}

/// The parked `getAttachmentMenuBot` request — the C++ promise rides the
/// query actor until the answer; the collapse parks it here until the net
/// query actors land. Both fields read at the net-step integration.
#[allow(dead_code)]
struct PendingBotQuery {
    user_id: UserId,
    promise: Promise<td_api::AttachmentMenuBot>,
}

/// The C++ `AttachMenuManager` actor; the Rust manager is a plain struct
/// held by `Td` (the actor envelope collapses, the M.13 convention).
#[derive(Default)]
pub struct AttachMenuManager {
    /// `is_inited_` (AttachMenuManager.h:131).
    is_inited: bool,
    /// `hash_`.
    hash: i64,
    /// `attach_menu_bots_`.
    attach_menu_bots: Vec<AttachMenuBot>,
    /// `attach_menu_bot_file_source_ids_` — the interning cache; the
    /// source ids themselves live in the FileReferenceManager.
    attach_menu_bot_file_source_ids: HashMap<UserId, FileSourceId>,
    /// `reload_attach_menu_bots_queries_`.
    reload_attach_menu_bots_queries: Vec<Promise<()>>,
    /// The parked single-bot queries (the `[blocked: net]` convention).
    pending_bot_queries: Vec<PendingBotQuery>,
    /// The parked toggle promises (the same convention; the C++
    /// success-chains them into `reload_attach_menu_bots`).
    pending_toggle_promises: Vec<Promise<()>>,
    /// The C++ actor timeout deadline (`set_timeout_in(Random::fast(...))`,
    /// process-monotonic — the M.3.7.2 convention).
    next_reload_at: f64,
}

impl AttachMenuManager {
    /// `AttachMenuManager::AttachMenuManager` (cpp:355-356).
    pub fn new() -> AttachMenuManager {
        AttachMenuManager::default()
    }

    /// `get_attach_menu_bots_database_key` (cpp:845-847).
    fn get_attach_menu_bots_database_key() -> &'static str {
        "attach_bots"
    }

    /// `AttachMenuManager::init` (cpp:366-430) — also `start_up`
    /// (cpp:362-364); the C++ `AuthManager` on_get_authorization call
    /// (AuthManager.cpp:1542) re-runs it after the sign-in.
    pub fn init(&mut self, env: &mut impl AttachMenuManagerEnv) {
        if !is_active(env) {
            return;
        }
        if self.is_inited {
            return;
        }
        self.is_inited = true;

        if !env.use_chat_info_database() {
            env.binlog_pmc_erase(Self::get_attach_menu_bots_database_key());
        } else {
            let attach_menu_bots_string =
                env.binlog_pmc_get_raw(Self::get_attach_menu_bots_database_key().as_bytes());

            if !attach_menu_bots_string.is_empty() {
                let mut parse = || -> Result<AttachMenuBotsLogEvent, &'static str> {
                    let mut parser = Parser::new_log_event(&attach_menu_bots_string)?;
                    AttachMenuBotsLogEvent::parse(env, &mut parser)
                };
                let mut is_valid = true;
                let attach_menu_bots_log_event = match parse() {
                    Ok(log_event) => log_event,
                    Err(_) => {
                        is_valid = false;
                        AttachMenuBotsLogEvent::default()
                    }
                };

                for attach_menu_bot in &attach_menu_bots_log_event.attach_menu_bots {
                    if !attach_menu_bot.user_id.is_valid()
                        || !attach_menu_bot.default_icon_file_id.is_valid()
                    {
                        is_valid = false;
                        break;
                    }
                }
                // `dependencies.resolve_force` (cpp:395) — the user-cache
                // replay is the M.4 binlog-replay seam; always resolvable
                // in the collapse.
                if is_valid {
                    let is_cache_outdated = attach_menu_bots_log_event
                        .attach_menu_bots
                        .iter()
                        .any(|bot| bot.cache_version != AttachMenuBot::CACHE_VERSION);
                    self.hash = if is_cache_outdated {
                        0
                    } else {
                        attach_menu_bots_log_event.hash
                    };
                    self.attach_menu_bots = attach_menu_bots_log_event.attach_menu_bots;

                    // the file-source registrations (cpp:405-421)
                    for attach_menu_bot in self.attach_menu_bots.clone() {
                        let file_source_id =
                            self.get_attach_menu_bot_file_source_id(env, attach_menu_bot.user_id);
                        let icon_file_ids = [
                            attach_menu_bot.default_icon_file_id,
                            attach_menu_bot.ios_static_icon_file_id,
                            attach_menu_bot.ios_animated_icon_file_id,
                            attach_menu_bot.android_icon_file_id,
                            attach_menu_bot.macos_icon_file_id,
                            attach_menu_bot.placeholder_file_id,
                            attach_menu_bot.android_side_menu_icon_file_id,
                            attach_menu_bot.ios_side_menu_icon_file_id,
                            attach_menu_bot.macos_side_menu_icon_file_id,
                        ];
                        for file_id in icon_file_ids {
                            if file_id.is_valid() {
                                env.add_file_source(file_id, file_source_id);
                            }
                        }
                    }
                } else {
                    tdutils::log_error!("Ignore invalid attachment menu bots log event");
                }
            }
        }

        self.send_update_attach_menu_bots(env);
        self.reload_attach_menu_bots(env, Promise::default());
    }

    /// `timeout_expired` (cpp:432-438) — the shared-slot tick (the M.13
    /// convention).
    pub fn tick_timeouts(&mut self, env: &mut impl AttachMenuManagerEnv) {
        if !is_active(env) {
            return;
        }
        if self.next_reload_at == 0.0 || self.next_reload_at > tdutils::time::now() {
            return;
        }
        self.next_reload_at = 0.0;
        self.reload_attach_menu_bots(env, Promise::default());
    }

    /// The shared-slot arm (the `set_timeout_in` collapse); `None` while
    /// no timeout is pending.
    pub fn next_deadline(&self) -> Option<f64> {
        if self.next_reload_at > 0.0 {
            Some(self.next_reload_at)
        } else {
            None
        }
    }

    /// `get_attach_menu_bot_file_source_id` (cpp:763-774) — the interning
    /// cache over the env's FileReferenceManager seam.
    fn get_attach_menu_bot_file_source_id(
        &mut self,
        env: &mut impl AttachMenuManagerEnv,
        user_id: UserId,
    ) -> FileSourceId {
        if !user_id.is_valid() || !is_active(env) {
            return FileSourceId::new(0);
        }

        if let Some(source_id) = self.attach_menu_bot_file_source_ids.get(&user_id) {
            return *source_id;
        }
        let source_id = env.get_attach_menu_bot_file_source_id(user_id);
        self.attach_menu_bot_file_source_ids
            .insert(user_id, source_id);
        source_id
    }

    /// `AttachMenuManager::get_attach_menu_bot` (cpp:444-593) — the wire
    /// conversion. The `on_get_document` hop rides
    /// [`AttachMenuManagerEnv::on_get_icon_document`].
    fn get_attach_menu_bot(
        &mut self,
        env: &mut impl AttachMenuManagerEnv,
        bot: telegram_api::AttachMenuBot,
    ) -> Result<AttachMenuBot, Status> {
        let user_id = UserId::new(bot.bot_id);
        if !env.have_accessible_user(user_id) {
            return Err(Status::error(
                400,
                &format!("Have no information about {}", user_id.get()),
            ));
        }

        let file_source_id = self.get_attach_menu_bot_file_source_id(env, user_id);

        let mut attach_menu_bot = AttachMenuBot {
            is_added: !bot.inactive,
            user_id,
            name: bot.short_name,
            ..AttachMenuBot::default()
        };
        for icon in bot.icons {
            let name = icon.name.clone();
            let Some(telegram_api::Document::document(document)) = icon.icon else {
                // the C++ documentEmpty case
                return Err(Status::error(
                    400,
                    &format!("Have no icon for {} with name {name}", user_id.get()),
                ));
            };

            if !is_known_icon_name(&name) {
                tdutils::log_error!("Have icon for {} with name {name}", user_id.get());
                continue;
            }

            // the C++ `expected_document_type` (cpp:472): the `_static`
            // names are General documents, the animated ones are Stickers
            let expected_sticker = !name.ends_with("_static");
            let file_id = env.on_get_icon_document(&document, expected_sticker);
            if file_id.is_valid() {
                env.add_file_source(file_id, file_source_id);
            }
            // the C++ type-mismatch `continue` (cpp:475-478)
            if !file_id.is_valid() {
                tdutils::log_error!(
                    "Receive wrong attachment menu bot icon \"{name}\" for {}",
                    user_id.get()
                );
                continue;
            }
            match icon_slot(&name) {
                Some(IconSlot::Default) => attach_menu_bot.default_icon_file_id = file_id,
                Some(IconSlot::IosStatic) => attach_menu_bot.ios_static_icon_file_id = file_id,
                Some(IconSlot::IosAnimated) => attach_menu_bot.ios_animated_icon_file_id = file_id,
                Some(IconSlot::AndroidIcon) => attach_menu_bot.android_icon_file_id = file_id,
                Some(IconSlot::MacosIcon) => attach_menu_bot.macos_icon_file_id = file_id,
                Some(IconSlot::Placeholder) => attach_menu_bot.placeholder_file_id = file_id,
                Some(IconSlot::AndroidSideMenu) => {
                    attach_menu_bot.android_side_menu_icon_file_id = file_id
                }
                Some(IconSlot::IosSideMenu) => attach_menu_bot.ios_side_menu_icon_file_id = file_id,
                Some(IconSlot::MacosSideMenu) => {
                    attach_menu_bot.macos_side_menu_icon_file_id = file_id
                }
                None => unreachable!("a known icon name always resolves a slot"),
            }
            if icon_expects_colors(&name) {
                if icon.colors.is_empty() {
                    tdutils::log_error!(
                        "Have no colors for attachment menu bot icon for {}",
                        user_id.get()
                    );
                } else {
                    for color in icon.colors {
                        if color.name != "light_icon"
                            && color.name != "light_text"
                            && color.name != "dark_icon"
                            && color.name != "dark_text"
                        {
                            tdutils::log_error!(
                                "Receive unexpected attachment menu color {} for {}",
                                color.name,
                                user_id.get()
                            );
                            continue;
                        }
                        let alpha = (color.color >> 24) & 0xFF;
                        if alpha != 0 && alpha != 0xFF {
                            tdutils::log_error!(
                                "Receive alpha in attachment menu color {} for {}",
                                color.name,
                                user_id.get()
                            );
                        }
                        let c = color.color & 0xFFFFFF;
                        // the C++ `switch (color->name_[6])`
                        match color.name.as_bytes().get(6) {
                            Some(b'i') => attach_menu_bot.icon_color.light_color = c,
                            Some(b't') => attach_menu_bot.name_color.light_color = c,
                            Some(b'c') => attach_menu_bot.icon_color.dark_color = c,
                            Some(b'e') => attach_menu_bot.name_color.dark_color = c,
                            _ => unreachable!("the allow list makes name[6] total"),
                        }
                    }
                    if attach_menu_bot.icon_color.light_color == -1
                        || attach_menu_bot.icon_color.dark_color == -1
                    {
                        tdutils::log_error!("Receive wrong icon_color for {}", user_id.get());
                        attach_menu_bot.icon_color = AttachMenuBotColor::default();
                    }
                    if attach_menu_bot.name_color.light_color == -1
                        || attach_menu_bot.name_color.dark_color == -1
                    {
                        tdutils::log_error!("Receive wrong name_color for {}", user_id.get());
                        attach_menu_bot.name_color = AttachMenuBotColor::default();
                    }
                }
            } else if !icon.colors.is_empty() {
                tdutils::log_error!(
                    "Have unexpected colors for attachment menu bot icon for {} with name {name}",
                    user_id.get()
                );
            }
        }
        for peer_type in bot.peer_types {
            match peer_type {
                telegram_api::AttachMenuPeerType::attachMenuPeerTypeSameBotPM(_) => {
                    attach_menu_bot.supports_self_dialogs = true
                }
                telegram_api::AttachMenuPeerType::attachMenuPeerTypeBotPM(_) => {
                    attach_menu_bot.supports_bot_dialogs = true
                }
                telegram_api::AttachMenuPeerType::attachMenuPeerTypePM(_) => {
                    attach_menu_bot.supports_user_dialogs = true
                }
                telegram_api::AttachMenuPeerType::attachMenuPeerTypeChat(_) => {
                    attach_menu_bot.supports_group_dialogs = true
                }
                telegram_api::AttachMenuPeerType::attachMenuPeerTypeBroadcast(_) => {
                    attach_menu_bot.supports_broadcast_dialogs = true
                }
            }
        }
        attach_menu_bot.request_write_access = bot.request_write_access;
        attach_menu_bot.show_in_attach_menu = bot.show_in_attach_menu;
        attach_menu_bot.show_in_side_menu = bot.show_in_side_menu;
        attach_menu_bot.side_menu_disclaimer_needed = bot.side_menu_disclaimer_needed;
        if !attach_menu_bot.default_icon_file_id.is_valid() {
            return Err(Status::error(
                400,
                &format!("Have no default icon for {}", user_id.get()),
            ));
        }
        attach_menu_bot.cache_version = AttachMenuBot::CACHE_VERSION;

        Ok(attach_menu_bot)
    }

    /// `reload_attach_menu_bots` (cpp:595-608) — also the C++
    /// `UpdatesManager::before_get_difference` hook (cpp:4124); the query
    /// is the `[blocked: net]` seam.
    pub fn reload_attach_menu_bots(
        &mut self,
        env: &mut impl AttachMenuManagerEnv,
        mut promise: Promise<()>,
    ) {
        if !is_active(env) {
            return promise.set_error(Status::error(400, "Can't reload attachment menu bots"));
        }

        self.reload_attach_menu_bots_queries.push(promise);
        if self.reload_attach_menu_bots_queries.len() == 1 {
            env.send_get_attach_menu_bots_query(self.hash);
        }
    }

    /// `on_reload_attach_menu_bots` (cpp:610-660) — the shared answer
    /// handler of the get-bots query.
    pub fn on_reload_attach_menu_bots(
        &mut self,
        env: &mut impl AttachMenuManagerEnv,
        result: Result<AttachMenuBotsAnswer, Status>,
    ) {
        if !is_active(env) {
            for mut promise in std::mem::take(&mut self.reload_attach_menu_bots_queries) {
                promise.set_value(());
            }
            return;
        }
        if let Err(error) = result {
            // `set_timeout_in(Random::fast(60, 120))` (cpp:616)
            self.next_reload_at = tdutils::time::now() + f64::from(tdutils::random::fast(60, 120));
            fail_promises(&mut self.reload_attach_menu_bots_queries, &error);
            return;
        }

        self.is_inited = true;

        // `set_timeout_in(Random::fast(3600, 4800))` (cpp:622)
        self.next_reload_at = tdutils::time::now() + f64::from(tdutils::random::fast(3600, 4800));

        let answer = result.unwrap();
        let answer = match answer {
            AttachMenuBotsAnswer::NotModified => {
                for mut promise in std::mem::take(&mut self.reload_attach_menu_bots_queries) {
                    promise.set_value(());
                }
                return;
            }
            AttachMenuBotsAnswer::Bots(bots) => bots,
        };

        env.on_get_users(answer.users, "on_reload_attach_menu_bots");

        let mut new_hash = answer.hash;
        let mut new_attach_menu_bots = Vec::new();

        for bot in answer.bots {
            match self.get_attach_menu_bot(env, bot) {
                Err(error) => {
                    tdutils::log_error!("{}", error.message());
                    new_hash = 0;
                }
                Ok(attach_menu_bot) => new_attach_menu_bots.push(attach_menu_bot),
            }
        }

        let need_update = new_attach_menu_bots != self.attach_menu_bots;
        if need_update || self.hash != new_hash {
            self.hash = new_hash;
            self.attach_menu_bots = new_attach_menu_bots;

            if need_update {
                self.send_update_attach_menu_bots(env);
            }

            self.save_attach_menu_bots(env);
        }
        for mut promise in std::mem::take(&mut self.reload_attach_menu_bots_queries) {
            promise.set_value(());
        }
    }

    /// `remove_bot_from_attach_menu` (cpp:662-673).
    fn remove_bot_from_attach_menu(
        &mut self,
        env: &mut impl AttachMenuManagerEnv,
        user_id: UserId,
    ) {
        if let Some(index) = self
            .attach_menu_bots
            .iter()
            .position(|bot| bot.user_id == user_id)
        {
            self.hash = 0;
            self.attach_menu_bots.remove(index);

            self.send_update_attach_menu_bots(env);
            self.save_attach_menu_bots(env);
        }
    }

    /// `get_attach_menu_bot` (cpp:675-691) — the `getAttachmentMenuBot`
    /// request flow; the query is the `[blocked: net]` seam and the
    /// promise parks until the answer.
    pub fn get_attach_menu_bot_request(
        &mut self,
        env: &mut impl AttachMenuManagerEnv,
        user_id: UserId,
        mut promise: Promise<td_api::AttachmentMenuBot>,
    ) {
        let input_user = match env.get_input_user(user_id) {
            Ok(input_user) => input_user,
            Err(status) => return promise.set_error(status),
        };

        let bot_data = match env.get_bot_data(user_id) {
            Ok(bot_data) => bot_data,
            Err(status) => return promise.set_error(status),
        };
        if !bot_data.can_be_added_to_attach_menu {
            return promise.set_error(Status::error(
                400,
                "The bot can't be added to attachment menu",
            ));
        }

        env.send_get_attach_menu_bot_query(&input_user);
        self.pending_bot_queries
            .push(PendingBotQuery { user_id, promise });
    }

    /// `reload_attach_menu_bot` (cpp:693-715) — no td_api arm reaches it
    /// in this revision; kept for the net-step integration.
    #[allow(dead_code)]
    pub fn reload_attach_menu_bot(
        &mut self,
        env: &mut impl AttachMenuManagerEnv,
        user_id: UserId,
        mut promise: Promise<()>,
    ) {
        if !is_active(env) {
            return promise.set_error(Status::error(400, "Can't reload attachment menu bot"));
        }

        let input_user = match env.get_input_user(user_id) {
            Ok(input_user) => input_user,
            Err(status) => return promise.set_error(status),
        };

        env.send_get_attach_menu_bot_query(&input_user);
        self.pending_bot_queries.push(PendingBotQuery {
            user_id,
            promise: Promise::default(),
        });
        // the C++ wraps the Unit promise around the object answer (the
        // object drops); the park keeps the ordering
        self.pending_toggle_promises.push(promise);
    }

    /// `on_get_attach_menu_bot` (cpp:717-761) — the shared answer handler
    /// of the single-bot queries.
    pub fn on_get_attach_menu_bot(
        &mut self,
        env: &mut impl AttachMenuManagerEnv,
        user_id: UserId,
        result: Result<telegram_api::AttachMenuBotsBot, Status>,
        mut promise: Promise<td_api::AttachmentMenuBot>,
    ) {
        if env.close_flag() {
            return promise.set_error(Status::error(500, "Request aborted"));
        }
        let bot = match result {
            Ok(bot) => bot,
            Err(error) => return promise.set_error(error),
        };

        env.on_get_users(bot.users, "on_get_attach_menu_bot");

        let Some(bot) = bot.bot else {
            return promise.set_error(Status::error(500, "Receive invalid response"));
        };
        let attach_menu_bot = match self.get_attach_menu_bot(env, bot) {
            Ok(attach_menu_bot) => attach_menu_bot,
            Err(error) => {
                tdutils::log_error!("{}", error.message());
                return promise.set_error(Status::error(500, "Receive invalid response"));
            }
        };
        if attach_menu_bot.user_id != user_id {
            return promise.set_error(Status::error(500, "Receive wrong bot"));
        }
        if attach_menu_bot.is_added {
            let mut is_found = false;
            for old_bot in &mut self.attach_menu_bots {
                if old_bot.user_id == user_id {
                    is_found = true;
                    if *old_bot != attach_menu_bot {
                        tdutils::log_info!("Update attachment menu bot {}", user_id.get());

                        *old_bot = attach_menu_bot.clone();

                        self.send_update_attach_menu_bots(env);
                        self.save_attach_menu_bots(env);
                    }
                    break;
                }
            }
            if !is_found {
                tdutils::log_info!("Add missing attachment menu bot {}", user_id.get());

                self.hash = 0;
                self.attach_menu_bots.insert(0, attach_menu_bot.clone());

                self.send_update_attach_menu_bots(env);
                self.save_attach_menu_bots(env);
            }
        }
        promise.set_value(Self::get_attachment_menu_bot_object(env, &attach_menu_bot));
    }

    /// `toggle_bot_is_added_to_attach_menu` (cpp:776-802) — the
    /// `toggleBotIsAddedToAttachmentMenu` request flow; the C++
    /// success-chains the promise into `reload_attach_menu_bots` and the
    /// C++ `ToggleBotInAttachMenuQuery::on_error` reloads + fails — both
    /// obligations ride the net-step integration.
    pub fn toggle_bot_is_added_to_attach_menu(
        &mut self,
        env: &mut impl AttachMenuManagerEnv,
        user_id: UserId,
        is_added: bool,
        allow_write_access: bool,
        mut promise: Promise<()>,
    ) {
        assert!(is_active(env), "the toggle CHECK(is_active) fired");

        let input_user = match env.get_input_user(user_id) {
            Ok(input_user) => input_user,
            Err(status) => return promise.set_error(status),
        };

        if is_added {
            let bot_data = match env.get_bot_data(user_id) {
                Ok(bot_data) => bot_data,
                Err(status) => return promise.set_error(status),
            };
            if !bot_data.can_be_added_to_attach_menu {
                return promise.set_error(Status::error(
                    400,
                    "The bot can't be added to attachment menu",
                ));
            }
        } else {
            self.remove_bot_from_attach_menu(env, user_id);
        }

        let write_allowed = is_added && allow_write_access;
        env.send_toggle_bot_in_attach_menu_query(&input_user, write_allowed, is_added);
        self.pending_toggle_promises.push(promise);
    }

    /// `get_attachment_menu_bot_object` (cpp:804-831) — invalid icon
    /// file ids answer `None` (the C++ nullptr `get_file`).
    fn get_attachment_menu_bot_object(
        env: &mut impl AttachMenuManagerEnv,
        bot: &AttachMenuBot,
    ) -> td_api::AttachmentMenuBot {
        let get_color = |color: &AttachMenuBotColor| {
            if *color == AttachMenuBotColor::default() {
                None
            } else {
                Some(td_api::AttachmentMenuBotColor {
                    light_color: color.light_color,
                    dark_color: color.dark_color,
                })
            }
        };
        td_api::AttachmentMenuBot {
            bot_user_id: env.get_user_id_object(bot.user_id),
            supports_self_chat: bot.supports_self_dialogs,
            supports_user_chats: bot.supports_user_dialogs,
            supports_bot_chats: bot.supports_bot_dialogs,
            supports_group_chats: bot.supports_group_dialogs,
            supports_channel_chats: bot.supports_broadcast_dialogs,
            request_write_access: bot.request_write_access,
            is_added: bot.is_added,
            show_in_attachment_menu: bot.show_in_attach_menu,
            show_in_side_menu: bot.show_in_side_menu,
            show_disclaimer_in_side_menu: bot.side_menu_disclaimer_needed,
            name: bot.name.clone(),
            name_color: get_color(&bot.name_color),
            default_icon: get_file_object_opt(env, bot.default_icon_file_id),
            ios_static_icon: get_file_object_opt(env, bot.ios_static_icon_file_id),
            ios_animated_icon: get_file_object_opt(env, bot.ios_animated_icon_file_id),
            ios_side_menu_icon: get_file_object_opt(env, bot.ios_side_menu_icon_file_id),
            android_icon: get_file_object_opt(env, bot.android_icon_file_id),
            android_side_menu_icon: get_file_object_opt(env, bot.android_side_menu_icon_file_id),
            macos_icon: get_file_object_opt(env, bot.macos_icon_file_id),
            macos_side_menu_icon: get_file_object_opt(env, bot.macos_side_menu_icon_file_id),
            icon_color: get_color(&bot.icon_color),
            web_app_placeholder: get_file_object_opt(env, bot.placeholder_file_id),
        }
    }

    /// `get_update_attachment_menu_bots_object` (cpp:833-839).
    fn get_update_attachment_menu_bots_object(
        env: &mut impl AttachMenuManagerEnv,
        bots: &[AttachMenuBot],
    ) -> td_api::Update {
        td_api::Update::updateAttachmentMenuBots(td_api::updateAttachmentMenuBots {
            bots: bots
                .iter()
                .map(|bot| Self::get_attachment_menu_bot_object(env, bot))
                .collect(),
        })
    }

    /// `send_update_attach_menu_bots` (cpp:841-843) — the collapsed
    /// `send_closure(G()->td(), &Td::send_update, ...)` hop goes through
    /// the env outbox.
    fn send_update_attach_menu_bots(&self, env: &mut impl AttachMenuManagerEnv) {
        let update = Self::get_update_attachment_menu_bots_object(env, &self.attach_menu_bots);
        env.send_update(update);
    }

    /// `save_attach_menu_bots` (cpp:849-861).
    fn save_attach_menu_bots(&self, env: &mut impl AttachMenuManagerEnv) {
        if !env.use_chat_info_database() {
            return;
        }

        if self.attach_menu_bots.is_empty() {
            env.binlog_pmc_erase(Self::get_attach_menu_bots_database_key());
        } else {
            let mut data = Vec::new();
            store_parse::store_log_event_version(&mut data);
            AttachMenuBotsLogEvent {
                hash: self.hash,
                attach_menu_bots: self.attach_menu_bots.clone(),
            }
            .store(env, &mut data);
            env.binlog_pmc_set_raw(Self::get_attach_menu_bots_database_key().as_bytes(), &data);
        }
    }

    /// `get_current_state` (cpp:863-869; Requests.cpp:2215).
    pub fn get_current_state(
        &self,
        env: &mut impl AttachMenuManagerEnv,
        updates: &mut Vec<td_api::Update>,
    ) {
        if !is_active(env) {
            return;
        }

        updates.push(Self::get_update_attachment_menu_bots_object(
            env,
            &self.attach_menu_bots,
        ));
    }
}

/// The `get_file` lambda of the C++ `get_attachment_menu_bot_object`
/// (cpp:806-811): invalid ids answer `None`.
fn get_file_object_opt(
    env: &mut impl AttachMenuManagerEnv,
    file_id: FileId,
) -> Option<td_api::File> {
    if !file_id.is_valid() {
        return None;
    }
    Some(env.get_file_object(file_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tdutils::promise::PromiseInterface;

    struct TestEnv {
        close_flag: bool,
        is_authorized: bool,
        is_bot: bool,
        use_chat_info_database: bool,
        pmc: std::collections::BTreeMap<Vec<u8>, Vec<u8>>,
        updates: Vec<td_api::Update>,
        query_log: Vec<String>,
        next_file_id: i32,
        file_ids: HashMap<String, FileId>,
        have_users: Vec<UserId>,
        bot_is_addable: bool,
    }

    impl TestEnv {
        fn new() -> TestEnv {
            TestEnv {
                close_flag: false,
                is_authorized: true,
                is_bot: false,
                use_chat_info_database: true,
                pmc: std::collections::BTreeMap::new(),
                updates: Vec::new(),
                query_log: Vec::new(),
                next_file_id: 1,
                file_ids: HashMap::new(),
                have_users: Vec::new(),
                bot_is_addable: true,
            }
        }

        fn icon_file_id(&mut self, name: &str) -> FileId {
            if let Some(&file_id) = self.file_ids.get(name) {
                return file_id;
            }
            let file_id = FileId::new(self.next_file_id, 0);
            self.next_file_id += 1;
            self.file_ids.insert(name.to_string(), file_id);
            file_id
        }
    }

    impl AttachMenuManagerEnv for TestEnv {
        fn get_file_object(&mut self, _file_id: FileId) -> td_api::File {
            td_api::File::default()
        }

        fn close_flag(&self) -> bool {
            self.close_flag
        }

        fn is_authorized(&self) -> bool {
            self.is_authorized
        }

        fn is_bot(&self) -> bool {
            self.is_bot
        }

        fn use_chat_info_database(&self) -> bool {
            self.use_chat_info_database
        }

        fn binlog_pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8> {
            self.pmc.get(key).cloned().unwrap_or_default()
        }

        fn binlog_pmc_set_raw(&mut self, key: &[u8], value: &[u8]) {
            self.pmc.insert(key.to_vec(), value.to_vec());
        }

        fn binlog_pmc_erase(&mut self, key: &str) {
            self.query_log.push(format!("pmc erase {key}"));
            self.pmc.remove(key.as_bytes());
        }

        fn send_update(&mut self, update: td_api::Update) {
            self.updates.push(update);
        }

        fn get_input_user(&mut self, user_id: UserId) -> Result<telegram_api::InputUser, Status> {
            Ok(telegram_api::InputUser::inputUser(
                telegram_api::inputUser {
                    user_id: user_id.get(),
                    access_hash: 0,
                },
            ))
        }

        fn get_bot_data(&mut self, _user_id: UserId) -> Result<BotData, Status> {
            if !self.bot_is_addable {
                return Err(Status::error(400, "Bot not found"));
            }
            Ok(BotData {
                can_be_added_to_attach_menu: true,
                ..BotData::default()
            })
        }

        fn have_accessible_user(&self, user_id: UserId) -> bool {
            self.have_users.contains(&user_id)
        }

        fn get_user_id_object(&mut self, user_id: UserId) -> i64 {
            user_id.get()
        }

        fn on_get_users(&mut self, _users: Vec<telegram_api::User>, _source: &str) {}

        fn get_attach_menu_bot_file_source_id(&mut self, user_id: UserId) -> FileSourceId {
            FileSourceId::new(user_id.get() as i32 + 1000)
        }

        fn on_get_icon_document(
            &mut self,
            _document: &telegram_api::document,
            _expected_sticker: bool,
        ) -> FileId {
            self.icon_file_id("icon")
        }

        fn add_file_source(&mut self, file_id: FileId, source_id: FileSourceId) {
            self.query_log
                .push(format!("add source {} {}", file_id.get(), source_id.get()));
        }

        fn store_file_id(&mut self, file_id: FileId, out: &mut Vec<u8>) {
            store_parse::store_i32(out, file_id.get());
        }

        fn parse_file_id(&mut self, parser: &mut Parser) -> Result<FileId, &'static str> {
            Ok(FileId::new(parser.i32_()?, 0))
        }

        fn send_get_attach_menu_bots_query(&mut self, hash: i64) {
            self.query_log.push(format!("get bots {hash}"));
        }

        fn send_get_attach_menu_bot_query(&mut self, _input_user: &telegram_api::InputUser) {
            self.query_log.push("get bot".to_string());
        }

        fn send_toggle_bot_in_attach_menu_query(
            &mut self,
            _input_user: &telegram_api::InputUser,
            write_allowed: bool,
            is_added: bool,
        ) {
            self.query_log
                .push(format!("toggle {write_allowed} {is_added}"));
        }
    }

    #[derive(Default)]
    struct Recorded<T>(std::sync::Arc<std::sync::Mutex<Option<T>>>);
    struct RecordingPromise<T: Send + 'static> {
        recorded: Recorded<T>,
    }

    impl<T: Send + 'static> PromiseInterface<T> for RecordingPromise<T> {
        fn set_value(&mut self, value: T) {
            *self.recorded.0.lock().unwrap() = Some(value);
        }

        fn set_error(&mut self, _error: Status) {}
    }

    fn recording_promise<T: Send + 'static>() -> (Promise<T>, Recorded<T>) {
        let recorded = Recorded(std::sync::Arc::new(std::sync::Mutex::new(None)));
        (
            Promise::from_interface(RecordingPromise {
                recorded: Recorded(recorded.0.clone()),
            }),
            recorded,
        )
    }

    fn wire_document() -> telegram_api::document {
        telegram_api::document {
            flags: 0,
            id: 100,
            access_hash: 7,
            file_reference: b"ref".to_vec(),
            date: 0,
            mime_type: "image/png".to_string(),
            size: 123,
            thumbs: Vec::new(),
            video_thumbs: Vec::new(),
            dc_id: 2,
            attributes: Vec::new(),
        }
    }

    fn wire_icon(name: &str) -> telegram_api::AttachMenuBotIcon {
        telegram_api::AttachMenuBotIcon {
            flags: 0,
            name: name.to_string(),
            icon: Some(telegram_api::Document::document(wire_document())),
            colors: Vec::new(),
        }
    }

    fn wire_bot(user_id: i64, short_name: &str) -> telegram_api::AttachMenuBot {
        telegram_api::AttachMenuBot {
            flags: 0,
            inactive: false,
            has_settings: false,
            request_write_access: true,
            show_in_attach_menu: true,
            show_in_side_menu: false,
            side_menu_disclaimer_needed: false,
            bot_id: user_id,
            short_name: short_name.to_string(),
            peer_types: vec![
                telegram_api::AttachMenuPeerType::attachMenuPeerTypePM(
                    telegram_api::attachMenuPeerTypePM,
                ),
                telegram_api::AttachMenuPeerType::attachMenuPeerTypeChat(
                    telegram_api::attachMenuPeerTypeChat,
                ),
            ],
            icons: vec![wire_icon("default_static")],
        }
    }

    fn bots_answer(bots: Vec<telegram_api::AttachMenuBot>) -> AttachMenuBotsAnswer {
        AttachMenuBotsAnswer::Bots(telegram_api::attachMenuBots {
            hash: 42,
            bots,
            users: Vec::new(),
        })
    }

    fn user_id(id: i64) -> UserId {
        UserId::new(id)
    }

    #[test]
    fn icon_slot_matches_the_cpp_switch() {
        assert!(matches!(
            icon_slot("default_static"),
            Some(IconSlot::Default)
        ));
        assert!(matches!(icon_slot("ios_static"), Some(IconSlot::IosStatic)));
        assert!(matches!(
            icon_slot("ios_animated"),
            Some(IconSlot::IosAnimated)
        ));
        assert!(matches!(
            icon_slot("android_animated"),
            Some(IconSlot::AndroidIcon)
        ));
        assert!(matches!(
            icon_slot("android_side_menu_static"),
            Some(IconSlot::AndroidSideMenu)
        ));
        assert!(matches!(
            icon_slot("macos_animated"),
            Some(IconSlot::MacosIcon)
        ));
        assert!(matches!(
            icon_slot("macos_side_menu_static"),
            Some(IconSlot::MacosSideMenu)
        ));
        assert!(matches!(
            icon_slot("ios_side_menu_static"),
            Some(IconSlot::IosSideMenu)
        ));
        assert!(matches!(
            icon_slot("placeholder_static"),
            Some(IconSlot::Placeholder)
        ));
        assert!(icon_slot("unknown_x").is_none());
    }

    #[test]
    fn init_is_gated_and_erases_without_the_chat_database() {
        let mut env = TestEnv::new();
        env.is_authorized = false;
        let mut manager = AttachMenuManager::new();
        manager.init(&mut env);
        assert_eq!(env.query_log.len(), 0, "not active: nothing happens");

        env.is_authorized = true;
        env.use_chat_info_database = false;
        manager.init(&mut env);
        assert!(
            env.query_log.contains(&"pmc erase attach_bots".to_string()),
            "the no-chat-info-db erase"
        );
        assert_eq!(
            env.query_log.len(),
            2,
            "the pmc erase + the init-tail reload"
        );
        assert!(
            env.query_log.contains(&"get bots 0".to_string()),
            "the init tail fires the reload query"
        );
        assert_eq!(env.updates.len(), 1, "the empty updateAttachmentMenuBots");
    }

    #[test]
    fn init_loads_and_replays_the_saved_bots() {
        // seed the pmc through a first manager's save
        let mut env = TestEnv::new();
        env.have_users.push(user_id(300));
        let mut manager = AttachMenuManager::new();
        manager.on_reload_attach_menu_bots(&mut env, Ok(bots_answer(vec![wire_bot(300, "bot")])));
        assert!(!env.pmc.is_empty(), "saved to the binlog pmc");

        // a fresh manager loads them: the update replays, the hash survives
        let mut fresh_env = TestEnv::new();
        fresh_env.pmc = env.pmc.clone();
        fresh_env.have_users.push(user_id(300));
        let mut fresh = AttachMenuManager::new();
        fresh.init(&mut fresh_env);
        assert_eq!(fresh_env.updates.len(), 1);
        assert_eq!(fresh.hash, 42);
        assert_eq!(fresh.attach_menu_bots.len(), 1);
        assert!(
            fresh_env
                .query_log
                .iter()
                .any(|q| q.starts_with("add source ")),
            "the icon file sources re-register"
        );
    }

    #[test]
    fn reload_applies_changes_and_saves() {
        let mut env = TestEnv::new();
        env.have_users.push(user_id(300));
        let mut manager = AttachMenuManager::new();
        let (promise, recorded) = recording_promise();
        manager.reload_attach_menu_bots(&mut env, promise);
        assert!(recorded.0.lock().unwrap().is_none(), "rides the query");
        assert_eq!(env.query_log, vec!["get bots 0".to_string()]);

        // the second request queues behind the first
        let (promise, _) = recording_promise();
        manager.reload_attach_menu_bots(&mut env, promise);
        assert_eq!(env.query_log.len(), 1);

        manager.on_reload_attach_menu_bots(&mut env, Ok(bots_answer(vec![wire_bot(300, "bot")])));
        assert!(manager.attach_menu_bots.len() == 1);
        assert_eq!(manager.hash, 42);
        assert!(env.updates.len() == 1, "the need_update broadcast");
        assert!(
            manager.next_deadline().is_some(),
            "the 1-1.33h timeout arms"
        );
        assert!(!env.pmc.is_empty(), "saved");

        // not modified: promises settle without changes
        let (promise, recorded) = recording_promise();
        manager.reload_attach_menu_bots(&mut env, promise);
        manager.on_reload_attach_menu_bots(&mut env, Ok(AttachMenuBotsAnswer::NotModified));
        assert!(recorded.0.lock().unwrap().is_some());
        assert_eq!(env.updates.len(), 1);
    }

    #[test]
    fn reload_error_sets_the_short_retry() {
        let mut env = TestEnv::new();
        let mut manager = AttachMenuManager::new();
        let (promise, _) = recording_promise();
        manager.reload_attach_menu_bots(&mut env, promise);
        manager.on_reload_attach_menu_bots(&mut env, Err(Status::error(429, "FLOOD")));
        assert!(manager.reload_attach_menu_bots_queries.is_empty());
        let deadline = manager.next_deadline().unwrap();
        assert!(
            deadline - tdutils::time::now() >= 59.0 && deadline - tdutils::time::now() <= 120.0,
            "the Random::fast(60, 120) retry window"
        );
    }

    #[test]
    fn tick_timeouts_fires_only_when_due_and_active() {
        let mut env = TestEnv::new();
        let mut manager = AttachMenuManager::new();
        manager.next_reload_at = tdutils::time::now() - 1.0;
        manager.tick_timeouts(&mut env);
        assert_eq!(
            env.query_log.last().unwrap(),
            "get bots 0",
            "the due timeout reloads"
        );

        // not yet due: no query
        let queries = env.query_log.len();
        manager.next_reload_at = tdutils::time::now() + 3600.0;
        manager.tick_timeouts(&mut env);
        assert_eq!(env.query_log.len(), queries);

        // inactive: no query
        env.is_authorized = false;
        manager.next_reload_at = tdutils::time::now() - 1.0;
        manager.tick_timeouts(&mut env);
        assert_eq!(env.query_log.len(), queries);
    }

    #[test]
    fn get_attach_menu_bot_request_checks_the_bot_data() {
        let mut env = TestEnv::new();
        env.bot_is_addable = false;
        let mut manager = AttachMenuManager::new();
        let (promise, _) = recording_promise();
        manager.get_attach_menu_bot_request(&mut env, user_id(1), promise);
        assert!(env.query_log.is_empty(), "rejected before the query");

        env.bot_is_addable = true;
        let (promise, _) = recording_promise();
        manager.get_attach_menu_bot_request(&mut env, user_id(1), promise);
        assert_eq!(env.query_log, vec!["get bot".to_string()]);
        assert_eq!(manager.pending_bot_queries.len(), 1);
    }

    #[test]
    fn on_get_attach_menu_bot_updates_the_cache() {
        let mut env = TestEnv::new();
        env.have_users.push(user_id(300));
        let mut manager = AttachMenuManager::new();
        let (promise, recorded) = recording_promise();

        let bot = wire_bot(300, "bot");
        manager.on_get_attach_menu_bot(
            &mut env,
            user_id(300),
            Ok(telegram_api::AttachMenuBotsBot {
                bot: Some(bot),
                users: Vec::new(),
            }),
            promise,
        );
        let object = recorded.0.lock().unwrap().take().unwrap();
        assert_eq!(object.bot_user_id, 300);
        assert!(object.is_added);
        assert!(object.supports_user_chats && object.supports_group_chats);
        assert!(object.default_icon.is_some(), "the registered icon file");
        assert_eq!(object.name, "bot");
        assert!(object.request_write_access);
        assert!(object.show_in_attachment_menu);
        // the missing bot was inserted + broadcast + saved
        assert_eq!(manager.attach_menu_bots.len(), 1);
        assert_eq!(manager.hash, 0, "the inserted bot resets the hash");
        assert_eq!(env.updates.len(), 1);
        assert!(!env.pmc.is_empty());

        // the same bot again: no update, the object still answers
        let (promise, recorded) = recording_promise();
        manager.on_get_attach_menu_bot(
            &mut env,
            user_id(300),
            Ok(telegram_api::AttachMenuBotsBot {
                bot: Some(wire_bot(300, "bot")),
                users: Vec::new(),
            }),
            promise,
        );
        assert!(recorded.0.lock().unwrap().is_some());
        assert_eq!(env.updates.len(), 1);

        // the wrong user id rejects
        let (promise, _) = recording_promise();
        manager.on_get_attach_menu_bot(
            &mut env,
            user_id(999),
            Ok(telegram_api::AttachMenuBotsBot {
                bot: Some(wire_bot(300, "bot")),
                users: Vec::new(),
            }),
            promise,
        );
        assert!(env.updates.len() == 1, "no new update for the wrong bot");
    }

    #[test]
    fn toggle_removes_and_parks_the_promise() {
        let mut env = TestEnv::new();
        env.have_users.push(user_id(300));
        let mut manager = AttachMenuManager::new();
        manager.on_reload_attach_menu_bots(&mut env, Ok(bots_answer(vec![wire_bot(300, "bot")])));
        let updates_after_load = env.updates.len();

        // the removal rewrites the cache before the query goes out
        let (promise, _) = recording_promise();
        manager.toggle_bot_is_added_to_attach_menu(&mut env, user_id(300), false, false, promise);
        assert!(manager.attach_menu_bots.is_empty());
        assert_eq!(
            env.updates.len(),
            updates_after_load + 1,
            "the removal broadcast"
        );
        assert!(
            env.query_log.iter().any(|q| q == "toggle false false"),
            "write_allowed = is_added && allow_write_access"
        );
        assert_eq!(manager.pending_toggle_promises.len(), 1);

        // the add path checks the bot data and sends write_allowed
        let (promise, _) = recording_promise();
        manager.toggle_bot_is_added_to_attach_menu(&mut env, user_id(300), true, true, promise);
        assert!(env.query_log.iter().any(|q| q == "toggle true true"));

        // the not-addable bot rejects
        env.bot_is_addable = false;
        let (promise, _) = recording_promise::<()>();
        manager.toggle_bot_is_added_to_attach_menu(&mut env, user_id(300), true, true, promise);
        assert_eq!(manager.pending_toggle_promises.len(), 2);
    }

    #[test]
    fn get_current_state_guards_active_and_emits_the_bots() {
        let mut env = TestEnv::new();
        env.have_users.push(user_id(300));
        let mut manager = AttachMenuManager::new();
        manager.on_reload_attach_menu_bots(&mut env, Ok(bots_answer(vec![wire_bot(300, "bot")])));
        let updates = env.updates.len();

        let mut state_updates = Vec::new();
        manager.get_current_state(&mut env, &mut state_updates);
        assert_eq!(state_updates.len(), 1);
        assert!(matches!(
            state_updates[0],
            td_api::Update::updateAttachmentMenuBots(_)
        ));

        // inactive: nothing
        env.is_bot = true;
        let mut state_updates = Vec::new();
        manager.get_current_state(&mut env, &mut state_updates);
        assert!(state_updates.is_empty());
        assert_eq!(env.updates.len(), updates, "no new broadcasts");
    }

    #[test]
    fn store_parse_round_trips_the_log_event() {
        let mut env = TestEnv::new();
        env.have_users.push(user_id(300));
        let mut manager = AttachMenuManager::new();
        manager.on_reload_attach_menu_bots(&mut env, Ok(bots_answer(vec![wire_bot(300, "bot")])));

        // re-parse the saved blob through a fresh env
        let saved = env.pmc.get(b"attach_bots".as_slice()).cloned().unwrap();
        let mut fresh_env = TestEnv::new();
        let mut parser = Parser::new_log_event(&saved).unwrap();
        let parsed = AttachMenuBotsLogEvent::parse(&mut fresh_env, &mut parser).unwrap();
        assert_eq!(parsed.hash, 42);
        assert_eq!(parsed.attach_menu_bots.len(), 1);
        let bot = &parsed.attach_menu_bots[0];
        assert_eq!(bot.user_id, user_id(300));
        assert_eq!(bot.cache_version, AttachMenuBot::CACHE_VERSION);
        assert!(bot.default_icon_file_id.is_valid());
        assert!(bot.supports_user_dialogs && bot.supports_group_dialogs);
        assert!(bot.request_write_access && bot.show_in_attach_menu);
    }

    #[test]
    fn the_legacy_parse_fixups_apply() {
        let mut env = TestEnv::new();
        // a pre-support-flags, pre-side-menu blob: is_added without the
        // show flags gains show_in_attach_menu + the true support trio
        let mut data = Vec::new();
        store_parse::store_log_event_version(&mut data);
        store_parse::store_i64(&mut data, 0);
        store_parse::store_i32(&mut data, 1);
        let mut flags: Vec<bool> = Vec::new();
        // [has_ios_static, has_ios_animated, has_android, has_macos] = no
        flags.extend([false, false, false, false]);
        flags.push(true); // is_added
        flags.push(false); // has_name_color
        flags.push(false); // has_icon_color
        flags.push(false); // has_support_flags = LEGACY
        flags.extend([false, false, false, false, false]); // support flags
        flags.push(false); // the always-false 14th slot
        flags.push(false); // legacy_supports_settings
        flags.push(false); // has_placeholder
        flags.push(false); // has_cache_version
        flags.extend([false, false, false]); // write/show/disclaimer
        flags.extend([false, false, false]); // side-menu icons
        let flags: [bool; 23] = flags.try_into().unwrap();
        store_parse::store_flags(&mut data, flags);
        store_parse::store_i64(&mut data, 300);
        store_parse::store_string(&mut data, "bot");
        store_parse::store_i32(&mut data, 77); // the default icon FileId

        let mut parser = Parser::new_log_event(&data).unwrap();
        let parsed = AttachMenuBotsLogEvent::parse(&mut env, &mut parser).unwrap();
        let bot = &parsed.attach_menu_bots[0];
        assert!(bot.supports_self_dialogs && bot.supports_user_dialogs && bot.supports_bot_dialogs);
        assert!(bot.show_in_attach_menu, "the is-added fixup");
    }
}
