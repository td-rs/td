//! Port of `td/telegram/InlineKeyboardButton.{h,cpp,hpp}` — the inline
//! keyboard button model and its three conversion directions (wire
//! intake, td_api intake, and the two output builders).
//!
//! URL validation goes through the landed
//! [`link_manager`] surface (`check_link`,
//! `get_link_user_id`), so no link plane is ported here. The
//! `user_manager->` reads (the UrlAuth/User `InputUser` builders and the
//! td_api user-id rewrite) go through the
//! [`InlineKeyboardButtonEnv`] trait; `add_dependencies` walks through
//! the [`DependenciesSink`] seam until the real M.10 `Dependencies`
//! lands.

use tdutils::status::Status;

use crate::dialog_id::{DialogId, DialogType};
use crate::keyboard_button_style::KeyboardButtonStyle;
use crate::link_manager;
use crate::message_content_dup_type::MessageContentDupType;
use crate::misc::clean_input_string;
use crate::store_parse::{self, Parser};
use crate::target_dialog_types::TargetDialogTypes;
use crate::td_api;
use crate::user_manager::UserId;
use tdmtproto::telegram_api;

/// The `(G(), UserManager *)` slice the C++ conversion functions take
/// (InlineKeyboardButton.cpp:389-527) — the `G()->is_test_dc()` probe of
/// the two `check_link(url, true, ...)` calls rides the trait, so every
/// conversion stays offline-testable.
pub trait InlineKeyboardButtonEnv {
    /// `G()->is_test_dc()`.
    fn is_test_dc(&self) -> bool;
    /// `user_manager->get_input_user(user_id)`.
    fn get_input_user(&self, user_id: UserId) -> Result<telegram_api::InputUser, Status>;
    /// `user_manager->is_user_bot(user_manager->get_my_id())`.
    fn is_my_user_bot(&self) -> bool;
    /// `user_manager->get_user_id_object(user_id, source)`.
    fn get_user_id_object(&self, user_id: UserId, source: &str) -> i64;
}

/// The C++ `user_manager == nullptr` band of the button builders: raw
/// ids, no bot knowledge, the production test-dc answer (the null-manager
/// form; callers with a live global thread the probe themselves).
impl InlineKeyboardButtonEnv for () {
    fn is_test_dc(&self) -> bool {
        false
    }

    fn get_input_user(&self, _user_id: UserId) -> Result<telegram_api::InputUser, Status> {
        Err(Status::error(400, "User not found"))
    }

    fn is_my_user_bot(&self) -> bool {
        false
    }

    fn get_user_id_object(&self, user_id: UserId, _source: &str) -> i64 {
        user_id.get()
    }
}

/// The `td::Dependencies` walk seam (Dependencies.h) — the real struct
/// lands with the M.10 dependency machinery; the walk itself is landed.
/// [blocked: M.10 integration]
pub trait DependenciesSink {
    /// `dependencies.add(user_id)`.
    fn add_user(&mut self, user_id: UserId);
}

/// `td::InlineKeyboardButton::Type` (InlineKeyboardButton.h:27) —
/// append only; the discriminants ride the store/parse format.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InlineKeyboardButtonType {
    Url,
    Callback,
    CallbackGame,
    SwitchInline,
    SwitchInlineCurrentDialog,
    Buy,
    UrlAuth,
    CallbackWithPassword,
    User,
    WebView,
    Copy,
    #[default]
    Disabled,
}

impl InlineKeyboardButtonType {
    fn as_i32(self) -> i32 {
        match self {
            InlineKeyboardButtonType::Url => 0,
            InlineKeyboardButtonType::Callback => 1,
            InlineKeyboardButtonType::CallbackGame => 2,
            InlineKeyboardButtonType::SwitchInline => 3,
            InlineKeyboardButtonType::SwitchInlineCurrentDialog => 4,
            InlineKeyboardButtonType::Buy => 5,
            InlineKeyboardButtonType::UrlAuth => 6,
            InlineKeyboardButtonType::CallbackWithPassword => 7,
            InlineKeyboardButtonType::User => 8,
            InlineKeyboardButtonType::WebView => 9,
            InlineKeyboardButtonType::Copy => 10,
            InlineKeyboardButtonType::Disabled => 11,
        }
    }

    fn from_i32(value: i32) -> InlineKeyboardButtonType {
        match value {
            0 => InlineKeyboardButtonType::Url,
            1 => InlineKeyboardButtonType::Callback,
            2 => InlineKeyboardButtonType::CallbackGame,
            3 => InlineKeyboardButtonType::SwitchInline,
            4 => InlineKeyboardButtonType::SwitchInlineCurrentDialog,
            5 => InlineKeyboardButtonType::Buy,
            6 => InlineKeyboardButtonType::UrlAuth,
            7 => InlineKeyboardButtonType::CallbackWithPassword,
            8 => InlineKeyboardButtonType::User,
            9 => InlineKeyboardButtonType::WebView,
            10 => InlineKeyboardButtonType::Copy,
            _ => InlineKeyboardButtonType::Disabled,
        }
    }
}

/// `td::InlineKeyboardButton` (InlineKeyboardButton.h:25).
#[derive(Clone, Debug, Default)]
pub struct InlineKeyboardButton {
    pub(crate) type_: InlineKeyboardButtonType,
    /// UrlAuth: the button id or the `(2 * request_write_access - 1) *
    /// bot_user_id + request_write_access - 1` encoding; SwitchInline:
    /// the [`TargetDialogTypes`] mask (0 if any).
    pub(crate) id: i64,
    pub(crate) user_id: UserId, // User only
    pub(crate) style: KeyboardButtonStyle,
    pub(crate) text: String,
    pub(crate) forward_text: String, // UrlAuth only
    pub(crate) data: String,
}

impl InlineKeyboardButton {
    /// `InlineKeyboardButton::copy` (InlineKeyboardButton.cpp:26-36).
    pub fn copy(&self) -> InlineKeyboardButton {
        self.clone()
    }

    /// `InlineKeyboardButton::clone` (InlineKeyboardButton.cpp:42-82) —
    /// the dup decision table. Secret chats and non-forward non-rich
    /// copies degrade to the Disabled button; the caller drops rows with
    /// a newly-disabled button ([`InlineKeyboardButton::is_disabled`]).
    pub fn clone_button(
        &self,
        dialog_id: DialogId,
        dup_type: MessageContentDupType,
        is_via_bot: bool,
        is_rich_message: bool,
    ) -> InlineKeyboardButton {
        if dialog_id.get_type() == DialogType::SecretChat {
            // secret chats have no reply markup support
            return InlineKeyboardButton::default();
        }
        if dup_type.is_send() {
            // any button can be sent
            return self.copy();
        }
        let is_forward = dup_type == MessageContentDupType::Forward;
        if !is_forward && !is_rich_message {
            // keyboard buttons can't be copied
            return InlineKeyboardButton::default();
        }
        if self.type_ == InlineKeyboardButtonType::Url
            || self.type_ == InlineKeyboardButtonType::User
            || self.type_ == InlineKeyboardButtonType::Copy
        {
            return self.copy();
        }
        if self.type_ == InlineKeyboardButtonType::UrlAuth {
            let mut result = self.copy();
            if is_forward {
                if !result.forward_text.is_empty() {
                    result.text = std::mem::take(&mut result.forward_text);
                }
            } else {
                debug_assert!(is_rich_message);
                result.type_ = InlineKeyboardButtonType::Url;
                result.forward_text.clear();
                result.id = 0;
            }
            return result;
        }
        if is_forward
            && is_via_bot
            && (self.type_ == InlineKeyboardButtonType::SwitchInline
                || self.type_ == InlineKeyboardButtonType::SwitchInlineCurrentDialog)
        {
            let mut result = self.copy();
            result.type_ = InlineKeyboardButtonType::SwitchInline;
            return result;
        }
        InlineKeyboardButton::default()
    }

    /// `add_dependencies` (InlineKeyboardButton.cpp:38-40).
    pub fn add_dependencies(&self, dependencies: &mut dyn DependenciesSink) {
        dependencies.add_user(self.user_id);
    }

    /// `is_buy` (InlineKeyboardButton.h:65-67).
    pub fn is_buy(&self) -> bool {
        self.type_ == InlineKeyboardButtonType::Buy
    }

    /// `is_disabled` (InlineKeyboardButton.h:69-71).
    pub fn is_disabled(&self) -> bool {
        self.type_ == InlineKeyboardButtonType::Disabled
    }

    /// `get_forward_text` (InlineKeyboardButton.h:73-75).
    pub fn get_forward_text(&self) -> &str {
        &self.forward_text
    }

    /// `get_login_url` (InlineKeyboardButton.h:77-82).
    pub fn get_login_url(&self, button_id: i64) -> Option<&str> {
        if self.type_ == InlineKeyboardButtonType::UrlAuth && self.id == button_id {
            return Some(&self.data);
        }
        None
    }

    /// `get_inline_keyboard_button` (InlineKeyboardButton.cpp:135-225) —
    /// the wire intake; failed URL checks log and fall back to the
    /// Disabled button, exactly like the C++ break-out.
    pub fn from_telegram(
        keyboard_button: telegram_api::KeyboardInlineButton,
    ) -> InlineKeyboardButton {
        let mut button = InlineKeyboardButton {
            style: KeyboardButtonStyle::from_telegram(keyboard_button.style),
            text: keyboard_button.text,
            ..InlineKeyboardButton::default()
        };
        if let Some(type_) = keyboard_button.r#type {
            match type_ {
                telegram_api::InlineButtonType::inlineButtonTypeUrl(type_) => {
                    match link_manager::check_link(&type_.url, false, false) {
                        Ok(url) => {
                            button.type_ = InlineKeyboardButtonType::Url;
                            button.data = url;
                        }
                        Err(error) => {
                            tdutils::log_error!("Inline keyboard {}", error.message());
                        }
                    }
                }
                telegram_api::InlineButtonType::inlineButtonTypeCallback(type_) => {
                    button.type_ = if type_.requires_password {
                        InlineKeyboardButtonType::CallbackWithPassword
                    } else {
                        InlineKeyboardButtonType::Callback
                    };
                    button.data = String::from_utf8_lossy(&type_.data).to_string();
                }
                telegram_api::InlineButtonType::inlineButtonTypeGame(_) => {
                    button.type_ = InlineKeyboardButtonType::CallbackGame;
                }
                telegram_api::InlineButtonType::inlineButtonTypeSwitchInline(type_) => {
                    button.type_ = if type_.same_peer {
                        InlineKeyboardButtonType::SwitchInlineCurrentDialog
                    } else {
                        InlineKeyboardButtonType::SwitchInline
                    };
                    button.data = type_.query;
                    if !type_.same_peer {
                        button.id = TargetDialogTypes::from_telegram(&type_.peer_types).get_mask();
                    }
                }
                telegram_api::InlineButtonType::inlineButtonTypeBuy(_) => {
                    button.type_ = InlineKeyboardButtonType::Buy;
                }
                telegram_api::InlineButtonType::inlineButtonTypeUrlAuth(type_) => {
                    match link_manager::check_link(&type_.url, false, false) {
                        Ok(url) => {
                            button.type_ = InlineKeyboardButtonType::UrlAuth;
                            button.id = type_.button_id as i64;
                            button.forward_text = type_.fwd_text;
                            button.data = url;
                        }
                        Err(error) => {
                            tdutils::log_error!("Inline keyboard Login {}", error.message());
                        }
                    }
                }
                telegram_api::InlineButtonType::inlineButtonTypeUserProfile(type_) => {
                    let user_id = UserId::new(type_.user_id);
                    if !user_id.is_valid() {
                        tdutils::log_error!("Receive {} in inline keyboard", user_id.get());
                    } else {
                        button.type_ = InlineKeyboardButtonType::User;
                        button.user_id = user_id;
                    }
                }
                telegram_api::InlineButtonType::inlineButtonTypeWebView(type_) => {
                    match link_manager::check_link(&type_.url, false, false) {
                        Ok(url) => {
                            button.type_ = InlineKeyboardButtonType::WebView;
                            button.data = url;
                        }
                        Err(error) => {
                            tdutils::log_error!("Inline keyboard Web App {}", error.message());
                        }
                    }
                }
                telegram_api::InlineButtonType::inlineButtonTypeCopy(type_) => {
                    button.type_ = InlineKeyboardButtonType::Copy;
                    button.data = type_.copy_text;
                }
                telegram_api::InlineButtonType::inlineButtonTypeDisabled(_) => {
                    button.type_ = InlineKeyboardButtonType::Disabled;
                }
                type_ => {
                    // inputInlineButtonTypeUrlAuth/inputInlineButtonTypeUserProfile
                    // are the request-side constructors; the C++
                    // UNREACHABLE() default covers them.
                    tdutils::log_error!("Unsupported inline keyboard button {type_:?}");
                }
            }
        }
        button
    }

    /// `get_inline_keyboard_button` (InlineKeyboardButton.cpp:227-387) —
    /// the td_api intake with the full 400 table.
    pub fn from_td_api(
        mut button: td_api::InlineKeyboardButton,
        switch_inline_buttons_allowed: bool,
        is_test_dc: bool,
    ) -> Result<InlineKeyboardButton, Status> {
        let Some(text) = clean_input_string(&button.text) else {
            return Err(Status::error(
                400,
                "Inline keyboard button text must be encoded in UTF-8",
            ));
        };
        if text.is_empty() {
            return Err(Status::error(
                400,
                "Inline keyboard button text must be non-empty",
            ));
        }
        button.text = text;
        let Some(button_type) = button.r#type.take() else {
            return Err(Status::error(
                400,
                "Inline keyboard button type must be non-empty",
            ));
        };

        let mut current_button = InlineKeyboardButton {
            text: button.text,
            style: KeyboardButtonStyle::from_td_api(
                button.style.take(),
                button.icon_custom_emoji_id,
            ),
            ..InlineKeyboardButton::default()
        };

        match button_type {
            td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeUrl(button_type) => {
                let user_id = link_manager::get_link_user_id(&button_type.url);
                if user_id.is_valid() {
                    current_button.type_ = InlineKeyboardButtonType::User;
                    current_button.user_id = user_id;
                } else {
                    let url = link_manager::check_link(&button_type.url, false, false).map_err(
                        |error| {
                            Status::error(
                                400,
                                &format!("Inline keyboard button {}", error.message()),
                            )
                        },
                    )?;
                    current_button.type_ = InlineKeyboardButtonType::Url;
                    current_button.data = url;
                    if clean_input_string(&current_button.data).is_none() {
                        return Err(Status::error(
                            400,
                            "Inline keyboard button URL must be encoded in UTF-8",
                        ));
                    }
                }
            }
            td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeCallback(button_type) => {
                current_button.type_ = InlineKeyboardButtonType::Callback;
                // td_api bytes are Vec<u8> in the Rust codegen; the C++
                // std::string keeps raw bytes, the port documents the
                // lossy UTF-8 view (the same convention as the wire arm).
                current_button.data = String::from_utf8_lossy(&button_type.data).into_owned();
            }
            td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeCallbackGame(_) => {
                current_button.type_ = InlineKeyboardButtonType::CallbackGame;
            }
            td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeCallbackWithPassword(_) => {
                return Err(Status::error(
                    400,
                    "Can't use CallbackWithPassword inline button",
                ));
            }
            td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeSwitchInline(button_type) => {
                let Some(target_chat) = button_type.target_chat else {
                    return Err(Status::error(400, "Target chat must be non-empty"));
                };
                match *target_chat {
                    td_api::TargetChat::targetChatChosen(chosen) => {
                        let types =
                            TargetDialogTypes::get_target_dialog_types(chosen.types.as_ref())?;
                        current_button.id = types.get_mask();
                        current_button.type_ = InlineKeyboardButtonType::SwitchInline;
                    }
                    td_api::TargetChat::targetChatCurrent(_) => {
                        current_button.type_ = InlineKeyboardButtonType::SwitchInlineCurrentDialog;
                    }
                    td_api::TargetChat::targetChatInternalLink(_) => {
                        return Err(Status::error(400, "Unsupported target chat specified"));
                    }
                }
                if !switch_inline_buttons_allowed {
                    let button_name =
                        if current_button.type_ == InlineKeyboardButtonType::SwitchInline {
                            "switch_inline_query"
                        } else {
                            "switch_inline_query_current_chat"
                        };
                    return Err(Status::error(
                        400,
                        &format!(
                            "Can't use {button_name} button in a channel chat, because users \
                             will not be able to use the button without knowing bot's username"
                        ),
                    ));
                }
                current_button.data = button_type.query;
                if clean_input_string(&current_button.data).is_none() {
                    return Err(Status::error(
                        400,
                        "Inline keyboard button switch inline query must be encoded in UTF-8",
                    ));
                }
            }
            td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeBuy(_) => {
                current_button.type_ = InlineKeyboardButtonType::Buy;
            }
            td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeLoginUrl(button_type) => {
                if link_manager::get_link_user_id(&button_type.url).is_valid() {
                    return Err(Status::error(
                        400,
                        "Link to a user can't be used in login URL buttons",
                    ));
                }
                let url = link_manager::check_link(&button_type.url, true, !is_test_dc).map_err(
                    |error| {
                        Status::error(
                            400,
                            &format!("Inline keyboard button login {}", error.message()),
                        )
                    },
                )?;
                current_button.type_ = InlineKeyboardButtonType::UrlAuth;
                current_button.data = url;
                current_button.forward_text = button_type.forward_text;
                if clean_input_string(&current_button.data).is_none() {
                    return Err(Status::error(
                        400,
                        "Inline keyboard button login URL must be encoded in UTF-8",
                    ));
                }
                if clean_input_string(&current_button.forward_text).is_none() {
                    return Err(Status::error(
                        400,
                        "Inline keyboard button forward text must be encoded in UTF-8",
                    ));
                }
                current_button.id = button_type.id;
                if current_button.id == i64::MIN {
                    return Err(Status::error(400, "Invalid bot_user_id specified"));
                }
                let bot_user_id = UserId::new(if current_button.id >= 0 {
                    current_button.id
                } else {
                    -current_button.id - 1
                });
                if !bot_user_id.is_valid() && bot_user_id != UserId::default() {
                    return Err(Status::error(400, "Invalid bot_user_id specified"));
                }
            }
            td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeUser(button_type) => {
                current_button.type_ = InlineKeyboardButtonType::User;
                current_button.user_id = UserId::new(button_type.user_id);
                if !current_button.user_id.is_valid() {
                    return Err(Status::error(400, "Invalid user_id specified"));
                }
            }
            td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeWebApp(button_type) => {
                if link_manager::get_link_user_id(&button_type.url).is_valid() {
                    return Err(Status::error(
                        400,
                        "Link to a user can't be used in Web App URL buttons",
                    ));
                }
                let url = link_manager::check_link(&button_type.url, true, !is_test_dc).map_err(
                    |error| {
                        Status::error(
                            400,
                            &format!("Inline keyboard button Web App {}", error.message()),
                        )
                    },
                )?;
                current_button.type_ = InlineKeyboardButtonType::WebView;
                current_button.data = url;
                if clean_input_string(&current_button.data).is_none() {
                    return Err(Status::error(
                        400,
                        "Inline keyboard button Web App URL must be encoded in UTF-8",
                    ));
                }
            }
            td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeCopyText(button_type) => {
                current_button.type_ = InlineKeyboardButtonType::Copy;
                current_button.data = button_type.text;
                if clean_input_string(&current_button.data).is_none() {
                    return Err(Status::error(
                        400,
                        "Inline keyboard button copied text must be encoded in UTF-8",
                    ));
                }
            }
            td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeDisabled(_) => {
                current_button.type_ = InlineKeyboardButtonType::Disabled;
            }
        }
        Ok(current_button)
    }

    /// `get_input_keyboard_inline_button` (InlineKeyboardButton.cpp:389-468).
    pub fn get_input_keyboard_inline_button(
        &self,
        env: &dyn InlineKeyboardButtonEnv,
    ) -> telegram_api::KeyboardInlineButton {
        let type_ = match self.type_ {
            InlineKeyboardButtonType::Url => telegram_api::InlineButtonType::inlineButtonTypeUrl(
                telegram_api::inlineButtonTypeUrl {
                    url: self.data.clone(),
                },
            ),
            InlineKeyboardButtonType::Callback => {
                telegram_api::InlineButtonType::inlineButtonTypeCallback(
                    telegram_api::inlineButtonTypeCallback {
                        flags: 0,
                        requires_password: false,
                        data: self.data.clone().into_bytes(),
                    },
                )
            }
            InlineKeyboardButtonType::CallbackGame => {
                telegram_api::InlineButtonType::inlineButtonTypeGame(
                    telegram_api::inlineButtonTypeGame {},
                )
            }
            InlineKeyboardButtonType::SwitchInline => {
                let mut flags = 0;
                let peer_types = TargetDialogTypes::from_mask(self.id).get_input_peer_types();
                if !peer_types.is_empty() {
                    flags |= 1 << 1; // PEER_TYPES_MASK
                }
                telegram_api::InlineButtonType::inlineButtonTypeSwitchInline(
                    telegram_api::inlineButtonTypeSwitchInline {
                        flags,
                        same_peer: false,
                        query: self.data.clone(),
                        peer_types,
                    },
                )
            }
            InlineKeyboardButtonType::SwitchInlineCurrentDialog => {
                telegram_api::InlineButtonType::inlineButtonTypeSwitchInline(
                    telegram_api::inlineButtonTypeSwitchInline {
                        flags: 0,
                        same_peer: true,
                        query: self.data.clone(),
                        peer_types: Vec::new(),
                    },
                )
            }
            InlineKeyboardButtonType::Buy => telegram_api::InlineButtonType::inlineButtonTypeBuy(
                telegram_api::inlineButtonTypeBuy {},
            ),
            InlineKeyboardButtonType::UrlAuth => {
                let mut flags = 0;
                let mut bot_user_id = self.id;
                let request_write_access = if bot_user_id >= 0 {
                    true
                } else {
                    bot_user_id = -bot_user_id - 1;
                    false
                };
                if !self.forward_text.is_empty() {
                    flags |= 1 << 1; // FWD_TEXT_MASK
                }
                let mut input_user = None;
                if bot_user_id != 0 {
                    match env.get_input_user(UserId::new(bot_user_id)) {
                        Ok(user) => {
                            flags |= 1 << 2; // BOT_MASK
                            input_user = Some(user);
                        }
                        Err(error) => {
                            tdutils::log_error!(
                                "Failed to get InputUser for {bot_user_id}: {error}"
                            );
                            // The C++ degrades the whole button to a URL one.
                            return self.style_and_wrap(
                                telegram_api::InlineButtonType::inlineButtonTypeUrl(
                                    telegram_api::inlineButtonTypeUrl {
                                        url: self.data.clone(),
                                    },
                                ),
                            );
                        }
                    }
                }
                telegram_api::InlineButtonType::inputInlineButtonTypeUrlAuth(
                    telegram_api::inputInlineButtonTypeUrlAuth {
                        flags,
                        request_write_access,
                        fwd_text: self.forward_text.clone(),
                        url: self.data.clone(),
                        bot: input_user,
                    },
                )
            }
            InlineKeyboardButtonType::CallbackWithPassword => {
                unreachable!("the C++ UNREACHABLE() fired")
            }
            InlineKeyboardButtonType::User => {
                let input_user = match env.get_input_user(self.user_id) {
                    Ok(user) => user,
                    Err(error) => {
                        tdutils::log_error!(
                            "Failed to get InputUser for {}: {}",
                            self.user_id.get(),
                            error
                        );
                        telegram_api::InputUser::inputUserEmpty(telegram_api::inputUserEmpty {})
                    }
                };
                telegram_api::InlineButtonType::inputInlineButtonTypeUserProfile(
                    telegram_api::inputInlineButtonTypeUserProfile {
                        user_id: Some(input_user),
                    },
                )
            }
            InlineKeyboardButtonType::WebView => {
                telegram_api::InlineButtonType::inlineButtonTypeWebView(
                    telegram_api::inlineButtonTypeWebView {
                        url: self.data.clone(),
                    },
                )
            }
            InlineKeyboardButtonType::Copy => telegram_api::InlineButtonType::inlineButtonTypeCopy(
                telegram_api::inlineButtonTypeCopy {
                    copy_text: self.data.clone(),
                },
            ),
            InlineKeyboardButtonType::Disabled => {
                telegram_api::InlineButtonType::inlineButtonTypeDisabled(
                    telegram_api::inlineButtonTypeDisabled {},
                )
            }
        };
        self.style_and_wrap(type_)
    }

    /// The shared tail of the wire builder: the style flag bit plus the
    /// wrapping `keyboardInlineButton` object.
    fn style_and_wrap(
        &self,
        type_: telegram_api::InlineButtonType,
    ) -> telegram_api::KeyboardInlineButton {
        let mut flags = 0;
        let style = self.style.get_input_keyboard_button_style();
        if style.is_some() {
            flags |= 1 << 10;
        }
        telegram_api::KeyboardInlineButton {
            flags,
            style,
            text: self.text.clone(),
            r#type: Some(type_),
        }
    }

    /// `get_inline_keyboard_button_object` (InlineKeyboardButton.cpp:470-527).
    /// The user-manager may be absent (`UserManager *user_manager` can be
    /// null in the C++ callers).
    pub fn get_inline_keyboard_button_object(
        &self,
        env: Option<&dyn InlineKeyboardButtonEnv>,
    ) -> td_api::InlineKeyboardButton {
        let type_ = match self.type_ {
            InlineKeyboardButtonType::Url => {
                td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeUrl(
                    td_api::inlineKeyboardButtonTypeUrl {
                        url: self.data.clone(),
                    },
                )
            }
            InlineKeyboardButtonType::Callback => {
                td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeCallback(
                    td_api::inlineKeyboardButtonTypeCallback {
                        data: self.data.clone().into_bytes(),
                    },
                )
            }
            InlineKeyboardButtonType::CallbackGame => {
                td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeCallbackGame(
                    td_api::inlineKeyboardButtonTypeCallbackGame,
                )
            }
            InlineKeyboardButtonType::SwitchInline => {
                td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeSwitchInline(
                    td_api::inlineKeyboardButtonTypeSwitchInline {
                        query: self.data.clone(),
                        target_chat: Some(Box::new(td_api::TargetChat::targetChatChosen(
                            td_api::targetChatChosen {
                                types: Some(
                                    TargetDialogTypes::from_mask(self.id)
                                        .get_target_chat_types_object(),
                                ),
                            },
                        ))),
                    },
                )
            }
            InlineKeyboardButtonType::SwitchInlineCurrentDialog => {
                td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeSwitchInline(
                    td_api::inlineKeyboardButtonTypeSwitchInline {
                        query: self.data.clone(),
                        target_chat: Some(Box::new(td_api::TargetChat::targetChatCurrent(
                            td_api::targetChatCurrent,
                        ))),
                    },
                )
            }
            InlineKeyboardButtonType::Buy => {
                td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeBuy(
                    td_api::inlineKeyboardButtonTypeBuy,
                )
            }
            InlineKeyboardButtonType::UrlAuth => {
                td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeLoginUrl(
                    td_api::inlineKeyboardButtonTypeLoginUrl {
                        url: self.data.clone(),
                        id: self.id,
                        forward_text: self.forward_text.clone(),
                    },
                )
            }
            InlineKeyboardButtonType::CallbackWithPassword => {
                td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeCallbackWithPassword(
                    td_api::inlineKeyboardButtonTypeCallbackWithPassword {
                        data: self.data.clone().into_bytes(),
                    },
                )
            }
            InlineKeyboardButtonType::User => {
                let need_user = env.is_some_and(|env| !env.is_my_user_bot());
                let user_id = if need_user {
                    env.map(|env| {
                        env.get_user_id_object(self.user_id, "get_inline_keyboard_button_object")
                    })
                    .unwrap_or_default()
                } else {
                    self.user_id.get()
                };
                td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeUser(
                    td_api::inlineKeyboardButtonTypeUser { user_id },
                )
            }
            InlineKeyboardButtonType::WebView => {
                td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeWebApp(
                    td_api::inlineKeyboardButtonTypeWebApp {
                        url: self.data.clone(),
                    },
                )
            }
            InlineKeyboardButtonType::Copy => {
                td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeCopyText(
                    td_api::inlineKeyboardButtonTypeCopyText {
                        text: self.data.clone(),
                    },
                )
            }
            InlineKeyboardButtonType::Disabled => {
                td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeDisabled(
                    td_api::inlineKeyboardButtonTypeDisabled,
                )
            }
        };
        td_api::InlineKeyboardButton {
            text: self.text.clone(),
            icon_custom_emoji_id: self.style.get_icon_custom_emoji_id().get(),
            style: if self.style.is_default() {
                None
            } else {
                Some(self.style.get_button_style_object())
            },
            r#type: Some(type_),
        }
    }

    /// `InlineKeyboardButton::store` (InlineKeyboardButton.hpp:17-48).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_id = self.id != 0;
        let has_user_id = self.user_id.is_valid();
        let has_forward_text = !self.forward_text.is_empty();
        let has_data = !self.data.is_empty();
        let has_style = !self.style.is_default();
        store_parse::store_flags(
            out,
            [has_id, has_user_id, has_forward_text, has_data, has_style],
        );
        store_parse::store_i32(out, self.type_.as_i32());
        if has_id {
            store_parse::store_i64(out, self.id);
        }
        if has_user_id {
            store_parse::store_i64(out, self.user_id.get());
        }
        store_parse::store_string(out, &self.text);
        if has_forward_text {
            store_parse::store_string(out, &self.forward_text);
        }
        if has_data {
            store_parse::store_string(out, &self.data);
        }
        if has_style {
            self.style.store(out);
        }
    }

    /// `InlineKeyboardButton::parse` (InlineKeyboardButton.hpp:50-96) —
    /// the pre-35 legacy ladder plus the `Support64BitIds` id width
    /// split.
    pub fn parse(parser: &mut Parser<'_>) -> Result<InlineKeyboardButton, &'static str> {
        if parser.version() >= store_parse::version::ADD_KEYBOARD_BUTTON_FLAGS {
            let [has_id, has_user_id, has_forward_text, has_data, has_style] =
                parser.parse_flags::<5>()?;
            let type_ = InlineKeyboardButtonType::from_i32(parser.i32_()?);
            let id = if has_id { parser.i64_()? } else { 0 };
            let user_id = if has_user_id {
                UserId::new(parser.i64_()?)
            } else {
                UserId::default()
            };
            let text = parser.string()?;
            let forward_text = if has_forward_text {
                parser.string()?
            } else {
                String::new()
            };
            let data = if has_data {
                parser.string()?
            } else {
                String::new()
            };
            let style = if has_style {
                KeyboardButtonStyle::parse(parser)?
            } else {
                KeyboardButtonStyle::default()
            };
            Ok(InlineKeyboardButton {
                type_,
                id,
                user_id,
                style,
                text,
                forward_text,
                data,
            })
        } else {
            let type_ = InlineKeyboardButtonType::from_i32(parser.i32_()?);
            let id = if type_ == InlineKeyboardButtonType::UrlAuth {
                if parser.version() >= store_parse::version::SUPPORT_64_BIT_IDS {
                    parser.i64_()?
                } else {
                    parser.i32_()? as i64
                }
            } else {
                0
            };
            let text = parser.string()?;
            let data = parser.string()?;
            Ok(InlineKeyboardButton {
                type_,
                id,
                user_id: UserId::default(),
                style: KeyboardButtonStyle::default(),
                text,
                forward_text: String::new(),
                data,
            })
        }
    }

    /// `operator<<` (InlineKeyboardButton.cpp:89-133).
    pub fn print(&self, out: &mut String) {
        out.push_str("Button[");
        match self.type_ {
            InlineKeyboardButtonType::Url => out.push_str("Url"),
            InlineKeyboardButtonType::Callback => out.push_str("Callback"),
            InlineKeyboardButtonType::CallbackGame => out.push_str("CallbackGame"),
            InlineKeyboardButtonType::SwitchInline => {
                out.push_str("SwitchInline, target chats = ");
                out.push_str(&TargetDialogTypes::from_mask(self.id).to_string());
            }
            InlineKeyboardButtonType::SwitchInlineCurrentDialog => {
                out.push_str("SwitchInlineCurrentChat");
            }
            InlineKeyboardButtonType::Buy => out.push_str("Buy"),
            InlineKeyboardButtonType::UrlAuth => {
                out.push_str("UrlAuth, ID = ");
                out.push_str(&self.id.to_string());
            }
            InlineKeyboardButtonType::CallbackWithPassword => out.push_str("CallbackWithPassword"),
            InlineKeyboardButtonType::User => {
                out.push_str("User ");
                out.push_str(&self.user_id.get().to_string());
            }
            InlineKeyboardButtonType::WebView => out.push_str("WebView"),
            InlineKeyboardButtonType::Copy => out.push_str("Copy"),
            InlineKeyboardButtonType::Disabled => out.push_str("Disabled"),
        }
        out.push_str(", text = ");
        out.push_str(&self.text);
        self.style.print(out);
        out.push_str(", ");
        out.push_str(&self.data);
        out.push(']');
    }
}

impl std::fmt::Display for InlineKeyboardButton {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut out = String::new();
        self.print(&mut out);
        f.write_str(&out)
    }
}

/// `operator==` (InlineKeyboardButton.cpp:84-87).
impl PartialEq for InlineKeyboardButton {
    fn eq(&self, other: &Self) -> bool {
        self.type_ == other.type_
            && self.id == other.id
            && self.user_id == other.user_id
            && self.style == other.style
            && self.text == other.text
            && self.forward_text == other.forward_text
            && self.data == other.data
    }
}

// Note for the M.10 integration: `UserManager` implements this trait in
// the td.rs wiring (the `get_input_user`/`is_user_bot_of`/
// `get_user_id_object` overloads need the `&mut` manager + env pair,
// so the blanket impl lives where both are borrowed).

#[cfg(test)]
mod tests {
    use super::*;

    /// The offline [`InlineKeyboardButtonEnv`] stub: bots are answered
    /// with inputUser, everyone else 400s — the manager's bot branch.
    struct TestEnv {
        my_user_is_bot: bool,
    }

    impl InlineKeyboardButtonEnv for TestEnv {
        fn is_test_dc(&self) -> bool {
            false
        }
        fn get_input_user(&self, user_id: UserId) -> Result<telegram_api::InputUser, Status> {
            if !user_id.is_valid() {
                return Err(Status::error(400, "User not found"));
            }
            Ok(telegram_api::InputUser::inputUser(
                telegram_api::inputUser {
                    user_id: user_id.get(),
                    access_hash: 0,
                },
            ))
        }
        fn is_my_user_bot(&self) -> bool {
            self.my_user_is_bot
        }
        fn get_user_id_object(&self, user_id: UserId, _source: &str) -> i64 {
            user_id.get()
        }
    }

    fn td_url_button(url: &str) -> td_api::InlineKeyboardButton {
        td_api::InlineKeyboardButton {
            text: "Open".to_owned(),
            icon_custom_emoji_id: 0,
            style: None,
            r#type: Some(
                td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeUrl(
                    td_api::inlineKeyboardButtonTypeUrl {
                        url: url.to_owned(),
                    },
                ),
            ),
        }
    }

    #[test]
    fn td_api_url_intake() {
        let button =
            InlineKeyboardButton::from_td_api(td_url_button("https://example.com/x"), true, true)
                .expect("valid url");
        assert_eq!(button.type_, InlineKeyboardButtonType::Url);
        assert_eq!(button.data, "https://example.com/x");
        assert!(button.get_login_url(0).is_none());
        assert!(!button.is_buy());
        assert!(!button.is_disabled());

        // A user link rewrites the button into the User type.
        let user_button =
            InlineKeyboardButton::from_td_api(td_url_button("tg://user?id=123"), true, true)
                .expect("user link");
        assert_eq!(user_button.type_, InlineKeyboardButtonType::User);
        assert_eq!(user_button.user_id, UserId::new(123));

        // A bad host is a 400 with the link message inlined.
        let error =
            InlineKeyboardButton::from_td_api(td_url_button("http://localhost:1/a"), true, true)
                .expect_err("bad host");
        assert_eq!(error.code(), 400);
        assert!(error.message().starts_with("Inline keyboard button "));
    }

    #[test]
    fn td_api_intake_400_table() {
        let mk = |r#type| td_api::InlineKeyboardButton {
            text: "X".to_owned(),
            icon_custom_emoji_id: 0,
            style: None,
            r#type: Some(r#type),
        };
        // CallbackWithPassword is never constructible.
        let error = InlineKeyboardButton::from_td_api(
            mk(
                td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeCallbackWithPassword(
                    td_api::inlineKeyboardButtonTypeCallbackWithPassword {
                        data: b"d".to_vec(),
                    },
                ),
            ),
            true,
            true,
        )
        .expect_err("callback with password");
        assert_eq!(
            error.message(),
            "Can't use CallbackWithPassword inline button"
        );

        // A null target chat.
        let error = InlineKeyboardButton::from_td_api(
            mk(
                td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeSwitchInline(
                    td_api::inlineKeyboardButtonTypeSwitchInline {
                        query: "q".to_owned(),
                        target_chat: None,
                    },
                ),
            ),
            true,
            true,
        )
        .expect_err("no target chat");
        assert_eq!(error.message(), "Target chat must be non-empty");

        // Switch-inline in a channel.
        let error = InlineKeyboardButton::from_td_api(
            mk(
                td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeSwitchInline(
                    td_api::inlineKeyboardButtonTypeSwitchInline {
                        query: "q".to_owned(),
                        target_chat: Some(Box::new(td_api::TargetChat::targetChatCurrent(
                            td_api::targetChatCurrent,
                        ))),
                    },
                ),
            ),
            false,
            true,
        )
        .expect_err("channel");
        assert!(
            error
                .message()
                .starts_with("Can't use switch_inline_query_current_chat button")
        );

        // LoginUrl with a negative-id encoding: id = -b - 1 -> b = 0.
        let login = mk(
            td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeLoginUrl(
                td_api::inlineKeyboardButtonTypeLoginUrl {
                    url: "https://example.com/login".to_owned(),
                    id: -1,
                    forward_text: "Fwd".to_owned(),
                },
            ),
        );
        let button = InlineKeyboardButton::from_td_api(login, true, true).expect("login url");
        assert_eq!(button.type_, InlineKeyboardButtonType::UrlAuth);
        assert_eq!(button.id, -1);
        assert_eq!(button.get_forward_text(), "Fwd");
        assert_eq!(button.get_login_url(-1), Some("https://example.com/login"));

        // LoginUrl with the int64-min id is the invalid bot_user_id form.
        let login = mk(
            td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeLoginUrl(
                td_api::inlineKeyboardButtonTypeLoginUrl {
                    url: "https://example.com/login".to_owned(),
                    id: i64::MIN,
                    forward_text: String::new(),
                },
            ),
        );
        let error = InlineKeyboardButton::from_td_api(login, true, true).expect_err("min id");
        assert_eq!(error.message(), "Invalid bot_user_id specified");

        // A User button with an invalid id.
        let error = InlineKeyboardButton::from_td_api(
            mk(
                td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeUser(
                    td_api::inlineKeyboardButtonTypeUser { user_id: -5 },
                ),
            ),
            true,
            true,
        )
        .expect_err("bad user");
        assert_eq!(error.message(), "Invalid user_id specified");

        // CopyText round trip.
        let copy = mk(
            td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeCopyText(
                td_api::inlineKeyboardButtonTypeCopyText {
                    text: "copied".to_owned(),
                },
            ),
        );
        let button = InlineKeyboardButton::from_td_api(copy, true, true).expect("copy");
        assert_eq!(button.type_, InlineKeyboardButtonType::Copy);
        assert_eq!(button.data, "copied");
    }

    #[test]
    fn switch_inline_mask_round_trip() {
        let types = td_api::TargetChatTypes {
            allow_user_chats: true,
            allow_bot_chats: false,
            allow_group_chats: false,
            allow_channel_chats: true,
        };
        let button = InlineKeyboardButton::from_td_api(
            td_api::InlineKeyboardButton {
                text: "Switch".to_owned(),
                icon_custom_emoji_id: 0,
                style: None,
                r#type: Some(
                    td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeSwitchInline(
                        td_api::inlineKeyboardButtonTypeSwitchInline {
                            query: "game".to_owned(),
                            target_chat: Some(Box::new(td_api::TargetChat::targetChatChosen(
                                td_api::targetChatChosen {
                                    types: Some(types.clone()),
                                },
                            ))),
                        },
                    ),
                ),
            },
            true,
            true,
        )
        .expect("valid");
        assert_eq!(button.type_, InlineKeyboardButtonType::SwitchInline);
        assert_eq!(
            button.id,
            TargetDialogTypes::get_target_dialog_types(Some(&types))
                .expect("valid")
                .get_mask()
        );

        // The wire output sets PEER_TYPES_MASK and the two peer types.
        let wire = button.get_input_keyboard_inline_button(&TestEnv {
            my_user_is_bot: true,
        });
        assert_eq!(wire.flags, 0);
        match wire.r#type {
            Some(telegram_api::InlineButtonType::inlineButtonTypeSwitchInline(switch_inline)) => {
                assert_eq!(switch_inline.flags, 1 << 1);
                assert!(!switch_inline.same_peer);
                assert_eq!(switch_inline.query, "game");
                assert_eq!(switch_inline.peer_types.len(), 2);
            }
            other => panic!("wrong wire type: {other:?}"),
        }

        // The td_api object rebuilds the chosen target off the stored
        // mask (TargetDialogTypes.cpp:81-85: users + channels here).
        match button
            .get_inline_keyboard_button_object(Some(&TestEnv {
                my_user_is_bot: true,
            }))
            .r#type
        {
            Some(td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeSwitchInline(
                switch_inline,
            )) => match *switch_inline.target_chat.expect("target chat") {
                td_api::TargetChat::targetChatChosen(chosen) => {
                    let types = chosen.types.expect("types");
                    assert!(types.allow_user_chats);
                    assert!(!types.allow_bot_chats);
                    assert!(!types.allow_group_chats);
                    assert!(types.allow_channel_chats);
                }
                other => panic!("wrong target chat: {other:?}"),
            },
            other => panic!("wrong object type: {other:?}"),
        }

        // The current-dialog form serializes without peer types.
        let current = InlineKeyboardButton {
            type_: InlineKeyboardButtonType::SwitchInlineCurrentDialog,
            data: "here".to_owned(),
            ..InlineKeyboardButton::default()
        };
        match current
            .get_input_keyboard_inline_button(&TestEnv {
                my_user_is_bot: true,
            })
            .r#type
        {
            Some(telegram_api::InlineButtonType::inlineButtonTypeSwitchInline(switch_inline)) => {
                assert_eq!(switch_inline.flags, 0);
                assert!(switch_inline.same_peer);
                assert!(switch_inline.peer_types.is_empty());
            }
            other => panic!("wrong wire type: {other:?}"),
        }
    }

    #[test]
    fn wire_intake_fallbacks() {
        // A failed URL check keeps the Disabled default (the C++ break).
        let wire = telegram_api::KeyboardInlineButton {
            flags: 0,
            style: None,
            text: "Bad".to_owned(),
            r#type: Some(telegram_api::InlineButtonType::inlineButtonTypeUrl(
                telegram_api::inlineButtonTypeUrl {
                    url: "http://localhost/".to_owned(),
                },
            )),
        };
        let button = InlineKeyboardButton::from_telegram(wire);
        assert!(button.is_disabled());
        assert_eq!(button.text, "Bad");

        // The callback data bytes decode through; requires_password picks
        // the CallbackWithPassword type.
        let wire = telegram_api::KeyboardInlineButton {
            flags: 0,
            style: None,
            text: "Pay".to_owned(),
            r#type: Some(telegram_api::InlineButtonType::inlineButtonTypeCallback(
                telegram_api::inlineButtonTypeCallback {
                    flags: 1 << 0,
                    requires_password: true,
                    data: b"\x01\x02\x03".to_vec(),
                },
            )),
        };
        let button = InlineKeyboardButton::from_telegram(wire);
        assert_eq!(button.type_, InlineKeyboardButtonType::CallbackWithPassword);
        assert_eq!(button.data.as_bytes(), b"\x01\x02\x03");

        // A switch-inline with peer types folds the mask into id.
        let wire = telegram_api::KeyboardInlineButton {
            flags: 0,
            style: None,
            text: "S".to_owned(),
            r#type: Some(
                telegram_api::InlineButtonType::inlineButtonTypeSwitchInline(
                    telegram_api::inlineButtonTypeSwitchInline {
                        flags: 1 << 1,
                        same_peer: false,
                        query: "q".to_owned(),
                        peer_types: vec![telegram_api::InlineQueryPeerType::inlineQueryPeerTypePM(
                            telegram_api::inlineQueryPeerTypePM {},
                        )],
                    },
                ),
            ),
        };
        let button = InlineKeyboardButton::from_telegram(wire);
        assert_eq!(button.type_, InlineKeyboardButtonType::SwitchInline);
        assert_eq!(button.id, TargetDialogTypes::USERS_MASK);

        // An invalid user id stays Disabled.
        let wire = telegram_api::KeyboardInlineButton {
            flags: 0,
            style: None,
            text: "U".to_owned(),
            r#type: Some(telegram_api::InlineButtonType::inlineButtonTypeUserProfile(
                telegram_api::inlineButtonTypeUserProfile { user_id: 0 },
            )),
        };
        assert!(InlineKeyboardButton::from_telegram(wire).is_disabled());
    }

    #[test]
    fn url_auth_wire_encoding() {
        // id >= 0 means request_write_access + bot; the env resolves the
        // InputUser.
        let button = InlineKeyboardButton {
            type_: InlineKeyboardButtonType::UrlAuth,
            id: 42,
            forward_text: "Login".to_owned(),
            data: "https://example.com".to_owned(),
            ..InlineKeyboardButton::default()
        };
        let wire = button.get_input_keyboard_inline_button(&TestEnv {
            my_user_is_bot: true,
        });
        match wire.r#type {
            Some(telegram_api::InlineButtonType::inputInlineButtonTypeUrlAuth(url_auth)) => {
                assert_eq!(url_auth.flags, (1 << 1) | (1 << 2)); // FWD_TEXT | BOT
                assert!(url_auth.request_write_access);
                assert_eq!(url_auth.fwd_text, "Login");
                assert!(url_auth.bot.is_some());
            }
            other => panic!("wrong wire type: {other:?}"),
        }

        // id 0 = no bot attached: the write access defaults to requested
        // (InlineKeyboardButton.cpp:415: `bot_user_id >= 0`), the button
        // stays UrlAuth, and the InputUser stays empty.
        let button = InlineKeyboardButton {
            type_: InlineKeyboardButtonType::UrlAuth,
            id: 0, // bot_user_id 0 skips the input user, no degradation
            data: "https://example.com".to_owned(),
            ..InlineKeyboardButton::default()
        };
        let wire = button.get_input_keyboard_inline_button(&TestEnv {
            my_user_is_bot: true,
        });
        match wire.r#type {
            Some(telegram_api::InlineButtonType::inputInlineButtonTypeUrlAuth(url_auth)) => {
                assert_eq!(url_auth.flags, 0);
                assert!(url_auth.request_write_access);
                assert!(url_auth.bot.is_none());
            }
            other => panic!("wrong wire type: {other:?}"),
        }

        // The User type: invalid ids fall back to inputUserEmpty.
        let button = InlineKeyboardButton {
            type_: InlineKeyboardButtonType::User,
            user_id: UserId::new(0),
            text: "U".to_owned(),
            ..InlineKeyboardButton::default()
        };
        let wire = button.get_input_keyboard_inline_button(&TestEnv {
            my_user_is_bot: false,
        });
        match wire.r#type {
            Some(telegram_api::InlineButtonType::inputInlineButtonTypeUserProfile(user)) => {
                match user.user_id {
                    Some(telegram_api::InputUser::inputUserEmpty(_)) => {}
                    other => panic!("expected inputUserEmpty: {other:?}"),
                }
            }
            other => panic!("wrong wire type: {other:?}"),
        }
    }

    #[test]
    fn td_api_user_object_respects_bot_flag() {
        let button = InlineKeyboardButton {
            type_: InlineKeyboardButtonType::User,
            user_id: UserId::new(7),
            text: "U".to_owned(),
            ..InlineKeyboardButton::default()
        };
        // A bot client sends the raw user id.
        let bot_env = TestEnv {
            my_user_is_bot: true,
        };
        match button
            .get_inline_keyboard_button_object(Some(&bot_env))
            .r#type
        {
            Some(td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeUser(user)) => {
                assert_eq!(user.user_id, 7);
            }
            other => panic!("wrong object: {other:?}"),
        }
        // A human client gets the manager's id object.
        let human_env = TestEnv {
            my_user_is_bot: false,
        };
        match button
            .get_inline_keyboard_button_object(Some(&human_env))
            .r#type
        {
            Some(td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeUser(user)) => {
                assert_eq!(user.user_id, 7);
            }
            other => panic!("wrong object: {other:?}"),
        }
        // A null manager sends the raw id.
        match button.get_inline_keyboard_button_object(None).r#type {
            Some(td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeUser(user)) => {
                assert_eq!(user.user_id, 7);
            }
            other => panic!("wrong object: {other:?}"),
        }
    }

    #[test]
    fn clone_decision_table() {
        let dialog_id = DialogId::new(-42);
        let secret_dialog_id = DialogId::new(-2_000_000_000_500);
        let url = InlineKeyboardButton {
            type_: InlineKeyboardButtonType::Url,
            data: "https://example.com".to_owned(),
            text: "U".to_owned(),
            ..InlineKeyboardButton::default()
        };
        let url_auth = InlineKeyboardButton {
            type_: InlineKeyboardButtonType::UrlAuth,
            id: 5,
            forward_text: "F".to_owned(),
            text: "T".to_owned(),
            data: "https://example.com".to_owned(),
            ..InlineKeyboardButton::default()
        };
        let callback = InlineKeyboardButton {
            type_: InlineKeyboardButtonType::Callback,
            data: "d".to_owned(),
            text: "C".to_owned(),
            ..InlineKeyboardButton::default()
        };
        let switch_current = InlineKeyboardButton {
            type_: InlineKeyboardButtonType::SwitchInlineCurrentDialog,
            data: "q".to_owned(),
            text: "S".to_owned(),
            ..InlineKeyboardButton::default()
        };

        // Secret chats degrade everything to Disabled.
        assert!(
            url_auth
                .clone_button(secret_dialog_id, MessageContentDupType::Send, false, false)
                .is_disabled()
        );

        // Sending copies anything.
        let cloned = callback.clone_button(dialog_id, MessageContentDupType::Send, false, false);
        assert_eq!(cloned, callback);
        let cloned =
            callback.clone_button(dialog_id, MessageContentDupType::SendViaBot, false, false);
        assert_eq!(cloned, callback);

        // Local/server copies carry no buttons at all
        // (InlineKeyboardButton.cpp:53-55: neither forward nor rich).
        assert!(
            callback
                .clone_button(dialog_id, MessageContentDupType::Copy, false, false)
                .is_disabled()
        );
        assert!(
            callback
                .clone_button(dialog_id, MessageContentDupType::ServerCopy, false, false)
                .is_disabled()
        );
        assert!(
            url.clone_button(dialog_id, MessageContentDupType::Copy, false, false)
                .is_disabled()
        );

        // Forwards keep Url/User/Copy, rewrite UrlAuth through the
        // forward text, and demote switch-inline-current via bots.
        assert_eq!(
            url.clone_button(dialog_id, MessageContentDupType::Forward, false, false),
            url
        );
        let forwarded =
            url_auth.clone_button(dialog_id, MessageContentDupType::Forward, false, false);
        assert_eq!(forwarded.text, "F");
        assert!(forwarded.forward_text.is_empty());
        assert_eq!(forwarded.type_, InlineKeyboardButtonType::UrlAuth);
        assert!(
            switch_current
                .clone_button(dialog_id, MessageContentDupType::Forward, false, false)
                .is_disabled()
        );
        let via_bot =
            switch_current.clone_button(dialog_id, MessageContentDupType::Forward, true, false);
        assert_eq!(via_bot.type_, InlineKeyboardButtonType::SwitchInline);

        // The rich-message rewrite of UrlAuth becomes a plain Url.
        let rich = url_auth.clone_button(dialog_id, MessageContentDupType::Copy, false, true);
        assert_eq!(rich.type_, InlineKeyboardButtonType::Url);
        assert_eq!(rich.id, 0);
        assert!(rich.forward_text.is_empty());
    }

    #[test]
    fn dependencies_walk() {
        struct Collector {
            user_ids: Vec<UserId>,
        }
        impl DependenciesSink for Collector {
            fn add_user(&mut self, user_id: UserId) {
                self.user_ids.push(user_id);
            }
        }
        let button = InlineKeyboardButton {
            type_: InlineKeyboardButtonType::User,
            user_id: UserId::new(9),
            ..InlineKeyboardButton::default()
        };
        let mut collector = Collector {
            user_ids: Vec::new(),
        };
        button.add_dependencies(&mut collector);
        assert_eq!(collector.user_ids, vec![UserId::new(9)]);
    }

    #[test]
    fn store_parse_round_trip_and_legacy_ladders() {
        let button = InlineKeyboardButton {
            type_: InlineKeyboardButtonType::UrlAuth,
            id: -7,
            user_id: UserId::new(0),
            style: KeyboardButtonStyle::from_td_api(None, 3),
            text: "L".to_owned(),
            forward_text: "Fwd".to_owned(),
            data: "https://e.com".to_owned(),
        };
        let mut out = Vec::new();
        button.store(&mut out);
        let parser = &mut Parser::new(&out);
        parser.set_version(store_parse::version::LOG_EVENT_STORE_VERSION);
        let parsed = InlineKeyboardButton::parse(parser).expect("parse");
        assert_eq!(parsed, button);
        assert!(parser.fetch_end().is_ok());

        // The pre-35 legacy ladder: type, then the UrlAuth id (int32
        // below Support64BitIds), then text and data.
        let mut legacy = Vec::new();
        store_parse::store_i32(&mut legacy, InlineKeyboardButtonType::UrlAuth.as_i32());
        store_parse::store_i32(&mut legacy, 11);
        store_parse::store_string(&mut legacy, "Old");
        store_parse::store_string(&mut legacy, "data");
        let parser = &mut Parser::new(&legacy);
        // below Support64BitIds: the UrlAuth id reads as int32
        parser.set_version(store_parse::version::SUPPORT_64_BIT_IDS - 1);
        let parsed = InlineKeyboardButton::parse(parser).expect("legacy parse");
        assert_eq!(parsed.type_, InlineKeyboardButtonType::UrlAuth);
        assert_eq!(parsed.id, 11);
        assert_eq!(parsed.text, "Old");
        assert_eq!(parsed.data, "data");
        assert!(parser.fetch_end().is_ok());

        // At Support64BitIds the id widens.
        let mut wide = Vec::new();
        store_parse::store_i32(&mut wide, InlineKeyboardButtonType::UrlAuth.as_i32());
        store_parse::store_i64(&mut wide, -11);
        store_parse::store_string(&mut wide, "Old");
        store_parse::store_string(&mut wide, "data");
        let parser = &mut Parser::new(&wide);
        parser.set_version(store_parse::version::SUPPORT_64_BIT_IDS);
        let parsed = InlineKeyboardButton::parse(parser).expect("wide parse");
        assert_eq!(parsed.id, -11);
    }

    #[test]
    fn display_matches_the_cpp_stream_output() {
        let button = InlineKeyboardButton {
            type_: InlineKeyboardButtonType::SwitchInline,
            id: TargetDialogTypes::BOTS_MASK,
            text: "S".to_owned(),
            data: "q".to_owned(),
            ..InlineKeyboardButton::default()
        };
        assert_eq!(
            button.to_string(),
            "Button[SwitchInline, target chats = (bots), text = S, q]"
        );
        let url_auth = InlineKeyboardButton {
            type_: InlineKeyboardButtonType::UrlAuth,
            id: 5,
            text: "T".to_owned(),
            data: "d".to_owned(),
            ..InlineKeyboardButton::default()
        };
        assert_eq!(url_auth.to_string(), "Button[UrlAuth, ID = 5, text = T, d]");
    }
}
