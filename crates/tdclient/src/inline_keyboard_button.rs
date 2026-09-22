//! Port of the reply-markup value slices the WebPageBlock rich-text
//! machinery consumes (M.17.1): `KeyboardButtonStyle.{h,cpp,hpp}`,
//! `RichButtonStyle.{h,cpp,hpp}`, `TargetDialogTypes.{h,cpp}` and
//! `InlineKeyboardButton.{h,cpp,hpp}`.
//!
//! File ownership note: `ReplyMarkup.{h,cpp}` stays M.22; these four
//! siblings land here as the WebPageBlock dependency plane, the same way
//! M.16 landed the misc hash helpers.
//!
//! The `[blocked: net]`-free surface: every conversion is local. The
//! `is_test_dc` probe of the two `check_link(url, true, ...)` calls
//! rides the [`InlineKeyboardButtonEnv`] seam.

use crate::emoji_status::CustomEmojiId;
use crate::link_manager::{check_link, get_link_user_id};
use crate::store_parse::{Parser, store_flags, store_i32, store_i64, store_string};
use crate::td_api;
use crate::user_manager::UserId;
use tdmtproto::telegram_api;
use tdutils::status::Status;

/// The `G()->is_test_dc()` probe plus the LinkManager entry points the
/// button intakes need (the `check_link` free function is local; the
/// env exists to keep the module testable offline).
pub trait InlineKeyboardButtonEnv {
    /// `G()->is_test_dc()`.
    fn is_test_dc(&self) -> bool;
}

/// `KeyboardButtonStyle::Type` (KeyboardButtonStyle.h:24).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum KeyboardButtonStyleType {
    #[default]
    Default,
    Primary,
    Danger,
    Success,
}

/// `td::KeyboardButtonStyle` (KeyboardButtonStyle.h:26).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KeyboardButtonStyle {
    pub(crate) type_: KeyboardButtonStyleType,
    pub(crate) icon_custom_emoji_id: CustomEmojiId,
}

impl KeyboardButtonStyle {
    /// `KeyboardButtonStyle(td_api::ButtonStyle, int64)` (KeyboardButtonStyle.cpp:11-34).
    pub fn from_td_api(
        style: Option<td_api::ButtonStyle>,
        icon_custom_emoji_id: i64,
    ) -> KeyboardButtonStyle {
        let mut result = KeyboardButtonStyle {
            type_: KeyboardButtonStyleType::Default,
            icon_custom_emoji_id: CustomEmojiId::new(icon_custom_emoji_id),
        };
        let Some(style) = style else {
            return result;
        };
        match style {
            td_api::ButtonStyle::buttonStyleDefault(_) => {}
            td_api::ButtonStyle::buttonStylePrimary(_) => {
                result.type_ = KeyboardButtonStyleType::Primary
            }
            td_api::ButtonStyle::buttonStyleDanger(_) => {
                result.type_ = KeyboardButtonStyleType::Danger
            }
            td_api::ButtonStyle::buttonStyleSuccess(_) => {
                result.type_ = KeyboardButtonStyleType::Success
            }
            // the never-allowed link style keeps the default type
            td_api::ButtonStyle::buttonStyleLink(_) => {}
        }
        result
    }

    /// `KeyboardButtonStyle(telegram_api::keyboardButtonStyle)`
    /// (KeyboardButtonStyle.cpp:36-49).
    pub fn from_telegram(style: Option<telegram_api::KeyboardButtonStyle>) -> KeyboardButtonStyle {
        let Some(style) = style else {
            return KeyboardButtonStyle::default();
        };
        let mut result = KeyboardButtonStyle {
            type_: KeyboardButtonStyleType::Default,
            icon_custom_emoji_id: CustomEmojiId::new(style.icon),
        };
        if style.bg_primary {
            result.type_ = KeyboardButtonStyleType::Primary;
        } else if style.bg_danger {
            result.type_ = KeyboardButtonStyleType::Danger;
        } else if style.bg_success {
            result.type_ = KeyboardButtonStyleType::Success;
        }
        result
    }

    /// `is_default` (KeyboardButtonStyle.h:42-44).
    pub fn is_default(&self) -> bool {
        self.type_ == KeyboardButtonStyleType::Default && !self.icon_custom_emoji_id.is_valid()
    }

    /// `get_button_style_object` (KeyboardButtonStyle.cpp:51-66).
    pub fn get_button_style_object(&self) -> Option<td_api::ButtonStyle> {
        match self.type_ {
            KeyboardButtonStyleType::Default => Some(td_api::ButtonStyle::buttonStyleDefault(
                td_api::buttonStyleDefault {},
            )),
            KeyboardButtonStyleType::Primary => Some(td_api::ButtonStyle::buttonStylePrimary(
                td_api::buttonStylePrimary {},
            )),
            KeyboardButtonStyleType::Danger => Some(td_api::ButtonStyle::buttonStyleDanger(
                td_api::buttonStyleDanger {},
            )),
            KeyboardButtonStyleType::Success => Some(td_api::ButtonStyle::buttonStyleSuccess(
                td_api::buttonStyleSuccess {},
            )),
        }
    }

    /// `get_icon_custom_emoji_id` (KeyboardButtonStyle.h:48-50).
    pub fn get_icon_custom_emoji_id(&self) -> CustomEmojiId {
        self.icon_custom_emoji_id
    }

    /// `get_input_keyboard_button_style` (KeyboardButtonStyle.cpp:68-81).
    pub fn get_input_keyboard_button_style(&self) -> Option<telegram_api::KeyboardButtonStyle> {
        if self.is_default() {
            return None;
        }
        let mut flags = 0;
        if self.icon_custom_emoji_id.is_valid() {
            // keyboardButtonStyle::ICON_MASK
            flags |= 1 << 0;
        }
        Some(telegram_api::KeyboardButtonStyle {
            flags,
            bg_primary: self.type_ == KeyboardButtonStyleType::Primary,
            bg_danger: self.type_ == KeyboardButtonStyleType::Danger,
            bg_success: self.type_ == KeyboardButtonStyleType::Success,
            icon: self.icon_custom_emoji_id.get(),
        })
    }

    /// `KeyboardButtonStyle::store` (KeyboardButtonStyle.hpp:14-30).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_type = self.type_ != KeyboardButtonStyleType::Default;
        let has_icon_custom_emoji_id = self.icon_custom_emoji_id.is_valid();
        store_flags(out, [has_type, has_icon_custom_emoji_id]);
        if has_type {
            store_i32(out, self.type_ as i32);
        }
        if has_icon_custom_emoji_id {
            store_i64(out, self.icon_custom_emoji_id.get());
        }
    }

    /// `KeyboardButtonStyle::parse` (KeyboardButtonStyle.hpp:32-48).
    pub fn parse(parser: &mut Parser<'_>) -> Result<KeyboardButtonStyle, &'static str> {
        let [has_type, has_icon_custom_emoji_id] = parser.parse_flags()?;
        let mut result = KeyboardButtonStyle::default();
        if has_type {
            let type_ = parser.i32_()?;
            result.type_ = match type_ {
                0 => KeyboardButtonStyleType::Default,
                1 => KeyboardButtonStyleType::Primary,
                2 => KeyboardButtonStyleType::Danger,
                3 => KeyboardButtonStyleType::Success,
                _ => return Err("Unknown KeyboardButtonStyle type"),
            };
        }
        if has_icon_custom_emoji_id {
            result.icon_custom_emoji_id = CustomEmojiId::new(parser.i64_()?);
        }
        Ok(result)
    }
}

/// `RichButtonStyle::Type` (RichButtonStyle.h:24).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RichButtonStyleType {
    #[default]
    Default,
    Primary,
    Danger,
    Success,
    Link,
}

/// `td::RichButtonStyle` (RichButtonStyle.h:26).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RichButtonStyle {
    pub(crate) type_: RichButtonStyleType,
}

impl RichButtonStyle {
    /// `RichButtonStyle(td_api::ButtonStyle)` (RichButtonStyle.cpp:11-34).
    pub fn from_td_api(style: Option<td_api::ButtonStyle>) -> RichButtonStyle {
        let Some(style) = style else {
            return RichButtonStyle::default();
        };
        let type_ = match style {
            td_api::ButtonStyle::buttonStyleDefault(_) => RichButtonStyleType::Default,
            td_api::ButtonStyle::buttonStylePrimary(_) => RichButtonStyleType::Primary,
            td_api::ButtonStyle::buttonStyleDanger(_) => RichButtonStyleType::Danger,
            td_api::ButtonStyle::buttonStyleSuccess(_) => RichButtonStyleType::Success,
            td_api::ButtonStyle::buttonStyleLink(_) => RichButtonStyleType::Link,
        };
        RichButtonStyle { type_ }
    }

    /// `RichButtonStyle(telegram_api::richButtonStyle)` (RichButtonStyle.cpp:36-49).
    pub fn from_telegram(style: Option<telegram_api::RichButtonStyle>) -> RichButtonStyle {
        let Some(style) = style else {
            return RichButtonStyle::default();
        };
        let mut result = RichButtonStyle::default();
        if style.bg_primary {
            result.type_ = RichButtonStyleType::Primary;
        } else if style.bg_danger {
            result.type_ = RichButtonStyleType::Danger;
        } else if style.bg_success {
            result.type_ = RichButtonStyleType::Success;
        } else if style.link {
            result.type_ = RichButtonStyleType::Link;
        }
        result
    }

    /// `is_default` (RichButtonStyle.h:44-46).
    pub fn is_default(&self) -> bool {
        self.type_ == RichButtonStyleType::Default
    }

    /// `get_button_style_object` (RichButtonStyle.cpp:51-67).
    pub fn get_button_style_object(&self) -> Option<td_api::ButtonStyle> {
        match self.type_ {
            RichButtonStyleType::Default => Some(td_api::ButtonStyle::buttonStyleDefault(
                td_api::buttonStyleDefault {},
            )),
            RichButtonStyleType::Primary => Some(td_api::ButtonStyle::buttonStylePrimary(
                td_api::buttonStylePrimary {},
            )),
            RichButtonStyleType::Danger => Some(td_api::ButtonStyle::buttonStyleDanger(
                td_api::buttonStyleDanger {},
            )),
            RichButtonStyleType::Success => Some(td_api::ButtonStyle::buttonStyleSuccess(
                td_api::buttonStyleSuccess {},
            )),
            RichButtonStyleType::Link => Some(td_api::ButtonStyle::buttonStyleLink(
                td_api::buttonStyleLink {},
            )),
        }
    }

    /// `get_input_rich_button_style` (RichButtonStyle.cpp:69-75).
    pub fn get_input_rich_button_style(&self) -> Option<telegram_api::RichButtonStyle> {
        if self.is_default() {
            return None;
        }
        Some(telegram_api::RichButtonStyle {
            flags: 0,
            bg_primary: self.type_ == RichButtonStyleType::Primary,
            bg_danger: self.type_ == RichButtonStyleType::Danger,
            bg_success: self.type_ == RichButtonStyleType::Success,
            link: self.type_ == RichButtonStyleType::Link,
        })
    }

    /// `RichButtonStyle::store` (RichButtonStyle.hpp:14-27).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_type = self.type_ != RichButtonStyleType::Default;
        store_flags(out, [has_type]);
        if has_type {
            store_i32(out, self.type_ as i32);
        }
    }

    /// `RichButtonStyle::parse` (RichButtonStyle.hpp:29-42).
    pub fn parse(parser: &mut Parser<'_>) -> Result<RichButtonStyle, &'static str> {
        let [has_type] = parser.parse_flags()?;
        let mut result = RichButtonStyle::default();
        if has_type {
            let type_ = parser.i32_()?;
            result.type_ = match type_ {
                0 => RichButtonStyleType::Default,
                1 => RichButtonStyleType::Primary,
                2 => RichButtonStyleType::Danger,
                3 => RichButtonStyleType::Success,
                4 => RichButtonStyleType::Link,
                _ => return Err("Unknown RichButtonStyle type"),
            };
        }
        Ok(result)
    }
}

/// `td::TargetDialogTypes` (TargetDialogTypes.h:18) — the switch-inline
/// target mask.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TargetDialogTypes {
    mask: i64,
}

impl TargetDialogTypes {
    const USERS_MASK: i64 = 1;
    const BOTS_MASK: i64 = 2;
    const CHATS_MASK: i64 = 4;
    const BROADCASTS_MASK: i64 = 8;
    const FULL_MASK: i64 =
        Self::USERS_MASK | Self::BOTS_MASK | Self::CHATS_MASK | Self::BROADCASTS_MASK;

    /// `explicit TargetDialogTypes(int64 mask)`.
    pub fn from_mask(mask: i64) -> TargetDialogTypes {
        TargetDialogTypes { mask }
    }

    /// `TargetDialogTypes(vector<InlineQueryPeerType>)` (TargetDialogTypes.cpp:12-33).
    pub fn from_telegram(types: &[telegram_api::InlineQueryPeerType]) -> TargetDialogTypes {
        let mut mask = 0;
        for peer_type in types {
            match peer_type {
                telegram_api::InlineQueryPeerType::inlineQueryPeerTypePM(_) => {
                    mask |= Self::USERS_MASK
                }
                telegram_api::InlineQueryPeerType::inlineQueryPeerTypeBotPM(_) => {
                    mask |= Self::BOTS_MASK
                }
                telegram_api::InlineQueryPeerType::inlineQueryPeerTypeChat(_) => {
                    mask |= Self::CHATS_MASK
                }
                telegram_api::InlineQueryPeerType::inlineQueryPeerTypeMegagroup(_) => {
                    mask |= Self::CHATS_MASK
                }
                telegram_api::InlineQueryPeerType::inlineQueryPeerTypeBroadcast(_) => {
                    mask |= Self::BROADCASTS_MASK
                }
                peer_type => {
                    tdutils::log_error!("Receive {peer_type:?}");
                }
            }
        }
        TargetDialogTypes { mask }
    }

    /// `get_target_dialog_types` (TargetDialogTypes.cpp:35-52).
    pub fn get_target_dialog_types(
        types: Option<&td_api::TargetChatTypes>,
    ) -> std::result::Result<TargetDialogTypes, Status> {
        let mut mask = 0;
        if let Some(types) = types {
            if types.allow_user_chats {
                mask |= Self::USERS_MASK;
            }
            if types.allow_bot_chats {
                mask |= Self::BOTS_MASK;
            }
            if types.allow_group_chats {
                mask |= Self::CHATS_MASK;
            }
            if types.allow_channel_chats {
                mask |= Self::BROADCASTS_MASK;
            }
        }
        if mask == 0 {
            return Err(Status::error(400, "At least one chat type must be allowed"));
        }
        Ok(TargetDialogTypes { mask })
    }

    /// `get_mask` (TargetDialogTypes.h:39-41).
    pub fn get_mask(&self) -> i64 {
        self.mask
    }

    /// `get_full_mask` (TargetDialogTypes.h:43-45).
    pub fn get_full_mask(&self) -> i64 {
        if self.mask == 0 {
            Self::FULL_MASK
        } else {
            self.mask
        }
    }

    /// `get_input_peer_types` (TargetDialogTypes.cpp:54-73).
    pub fn get_input_peer_types(&self) -> Vec<telegram_api::InlineQueryPeerType> {
        let mut peer_types = Vec::new();
        if self.mask == Self::FULL_MASK {
            return peer_types;
        }
        if self.mask & Self::USERS_MASK != 0 {
            peer_types.push(telegram_api::InlineQueryPeerType::inlineQueryPeerTypePM(
                telegram_api::inlineQueryPeerTypePM {},
            ));
        }
        if self.mask & Self::BOTS_MASK != 0 {
            peer_types.push(telegram_api::InlineQueryPeerType::inlineQueryPeerTypeBotPM(
                telegram_api::inlineQueryPeerTypeBotPM {},
            ));
        }
        if self.mask & Self::CHATS_MASK != 0 {
            peer_types.push(telegram_api::InlineQueryPeerType::inlineQueryPeerTypeChat(
                telegram_api::inlineQueryPeerTypeChat {},
            ));
            peer_types.push(
                telegram_api::InlineQueryPeerType::inlineQueryPeerTypeMegagroup(
                    telegram_api::inlineQueryPeerTypeMegagroup {},
                ),
            );
        }
        if self.mask & Self::BROADCASTS_MASK != 0 {
            peer_types.push(
                telegram_api::InlineQueryPeerType::inlineQueryPeerTypeBroadcast(
                    telegram_api::inlineQueryPeerTypeBroadcast {},
                ),
            );
        }
        peer_types
    }

    /// `get_target_chat_types_object` (TargetDialogTypes.cpp:75-82).
    pub fn get_target_chat_types_object(&self) -> td_api::TargetChatTypes {
        let mask = self.get_full_mask();
        td_api::TargetChatTypes {
            allow_user_chats: mask & Self::USERS_MASK != 0,
            allow_bot_chats: mask & Self::BOTS_MASK != 0,
            allow_group_chats: mask & Self::CHATS_MASK != 0,
            allow_channel_chats: mask & Self::BROADCASTS_MASK != 0,
        }
    }
}

/// `InlineKeyboardButton::Type` (InlineKeyboardButton.h:27) — append only.
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

/// `td::InlineKeyboardButton` (InlineKeyboardButton.h:25).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InlineKeyboardButton {
    pub(crate) type_: InlineKeyboardButtonType,
    pub(crate) id: i64,
    pub(crate) user_id: UserId,
    pub(crate) style: KeyboardButtonStyle,
    pub(crate) text: String,
    pub(crate) forward_text: String,
    pub(crate) data: String,
}

impl InlineKeyboardButton {
    /// `InlineKeyboardButton::copy` (InlineKeyboardButton.cpp:21-32).
    fn copy(&self) -> InlineKeyboardButton {
        self.clone()
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

    /// The `user_id` field read (the C++ field is public).
    pub fn user_id(&self) -> UserId {
        self.user_id
    }

    /// `get_login_url` (InlineKeyboardButton.h:77-82).
    pub fn get_login_url(&self, button_id: i64) -> Option<&str> {
        if self.type_ == InlineKeyboardButtonType::UrlAuth && self.id == button_id {
            return Some(&self.data);
        }
        None
    }

    /// `InlineKeyboardButton::clone` (InlineKeyboardButton.cpp:39-78) —
    /// the `MessageContentDupType` shape arrives with the rich-message
    /// consumers; the local form takes the four read fields.
    #[allow(clippy::too_many_arguments)]
    pub fn clone_for_dup(
        &self,
        dialog_type_is_secret_chat: bool,
        is_send: bool,
        is_forward: bool,
        is_rich_message: bool,
        is_via_bot: bool,
    ) -> InlineKeyboardButton {
        if dialog_type_is_secret_chat {
            // secret chats have no reply markup support
            return InlineKeyboardButton::default();
        }
        if is_send {
            // any button can be sent
            return self.copy();
        }
        if !is_forward && !is_rich_message {
            // keyboard buttons can't be copied
            return InlineKeyboardButton::default();
        }
        if matches!(
            self.type_,
            InlineKeyboardButtonType::Url
                | InlineKeyboardButtonType::User
                | InlineKeyboardButtonType::Copy
        ) {
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
            && matches!(
                self.type_,
                InlineKeyboardButtonType::SwitchInline
                    | InlineKeyboardButtonType::SwitchInlineCurrentDialog
            )
        {
            let mut result = self.copy();
            result.type_ = InlineKeyboardButtonType::SwitchInline;
            return result;
        }
        InlineKeyboardButton::default()
    }

    /// `InlineKeyboardButton::store` (InlineKeyboardButton.hpp:18-48) —
    /// the `AddKeyboardButtonFlags` layout.
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_id = self.id != 0;
        let has_user_id = self.user_id.is_valid();
        let has_forward_text = !self.forward_text.is_empty();
        let has_data = !self.data.is_empty();
        let has_style = !self.style.is_default();
        store_flags(
            out,
            [has_id, has_user_id, has_forward_text, has_data, has_style],
        );
        store_i32(out, self.type_ as i32);
        if has_id {
            store_i64(out, self.id);
        }
        if has_user_id {
            store_i64(out, self.user_id.get());
        }
        store_string(out, &self.text);
        if has_forward_text {
            store_string(out, &self.forward_text);
        }
        if has_data {
            store_string(out, &self.data);
        }
        if has_style {
            self.style.store(out);
        }
    }

    /// `InlineKeyboardButton::parse` (InlineKeyboardButton.hpp:50-96) —
    /// the pre-`AddKeyboardButtonFlags` legacy branch is not reachable
    /// (the parser version is `>= 35` for every live writer), mirrored
    /// with an error like the C++ parse failure.
    pub fn parse(parser: &mut Parser<'_>) -> Result<InlineKeyboardButton, &'static str> {
        let mut result = InlineKeyboardButton::default();
        if parser.version() < crate::store_parse::version::ADD_KEYBOARD_BUTTON_FLAGS {
            return Err("Inline keyboard button stored before AddKeyboardButtonFlags");
        }
        let [has_id, has_user_id, has_forward_text, has_data, has_style] = parser.parse_flags()?;
        let type_ = parser.i32_()?;
        result.type_ = match type_ {
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
            11 => InlineKeyboardButtonType::Disabled,
            _ => return Err("Unknown InlineKeyboardButton type"),
        };
        if has_id {
            result.id = parser.i64_()?;
        }
        if has_user_id {
            result.user_id = UserId::new(parser.i64_()?);
        }
        result.text = parser.string()?;
        if has_forward_text {
            result.forward_text = parser.string()?;
        }
        if has_data {
            result.data = parser.string()?;
        }
        if has_style {
            result.style = KeyboardButtonStyle::parse(parser)?;
        }
        Ok(result)
    }
}

/// `get_inline_keyboard_button(keyboardInlineButton)` (InlineKeyboardButton.cpp:135-225).
pub fn get_inline_keyboard_button_from_telegram(
    keyboard_button: telegram_api::KeyboardInlineButton,
) -> InlineKeyboardButton {
    let mut button = InlineKeyboardButton::default();
    if let Some(button_type) = keyboard_button.r#type {
        match &button_type {
            telegram_api::InlineButtonType::inlineButtonTypeUrl(type_) => {
                match check_link(&type_.url, false, false) {
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
                button.data = String::from_utf8_lossy(&type_.data).into_owned();
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
                button.data = type_.query.clone();
                if !type_.same_peer {
                    button.id = TargetDialogTypes::from_telegram(&type_.peer_types).get_mask();
                }
            }
            telegram_api::InlineButtonType::inlineButtonTypeBuy(_) => {
                button.type_ = InlineKeyboardButtonType::Buy;
            }
            telegram_api::InlineButtonType::inlineButtonTypeUrlAuth(type_) => {
                match check_link(&type_.url, false, false) {
                    Ok(url) => {
                        button.type_ = InlineKeyboardButtonType::UrlAuth;
                        button.id = type_.button_id as i64;
                        button.forward_text = type_.fwd_text.clone();
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
                    tdutils::log_error!(
                        "Receive invalid user {} in inline keyboard",
                        type_.user_id
                    );
                } else {
                    button.type_ = InlineKeyboardButtonType::User;
                    button.user_id = user_id;
                }
            }
            telegram_api::InlineButtonType::inlineButtonTypeWebView(type_) => {
                match check_link(&type_.url, false, false) {
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
                button.data = type_.copy_text.clone();
            }
            telegram_api::InlineButtonType::inlineButtonTypeDisabled(_) => {
                button.type_ = InlineKeyboardButtonType::Disabled;
            }
            button_type => {
                tdutils::log_error!("Unsupported inline keyboard button: {button_type:?}");
            }
        }
    }
    button.style = KeyboardButtonStyle::from_telegram(keyboard_button.style);
    button.text = keyboard_button.text;
    button
}

/// `get_inline_keyboard_button(td_api::inlineKeyboardButton, switch_inline_buttons_allowed)`
/// (InlineKeyboardButton.cpp:227-387).
pub fn get_inline_keyboard_button_from_td_api(
    button: td_api::InlineKeyboardButton,
    switch_inline_buttons_allowed: bool,
    env: &impl InlineKeyboardButtonEnv,
) -> std::result::Result<InlineKeyboardButton, Status> {
    let mut current_button = InlineKeyboardButton {
        text: crate::misc::clean_input_string(&button.text).ok_or_else(|| {
            Status::error(400, "Inline keyboard button text must be encoded in UTF-8")
        })?,
        ..InlineKeyboardButton::default()
    };
    if current_button.text.is_empty() {
        return Err(Status::error(
            400,
            "Inline keyboard button text must be non-empty",
        ));
    }
    let Some(button_type) = button.r#type else {
        return Err(Status::error(
            400,
            "Inline keyboard button type must be non-empty",
        ));
    };
    current_button.style =
        KeyboardButtonStyle::from_td_api(button.style, button.icon_custom_emoji_id);

    match &button_type {
        td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeUrl(button_type) => {
            let user_id = get_link_user_id(&button_type.url);
            if user_id.is_valid() {
                current_button.type_ = InlineKeyboardButtonType::User;
                current_button.user_id = user_id;
            } else {
                let url = check_link(&button_type.url, false, false).map_err(|error| {
                    Status::error(400, &format!("Inline keyboard button {}", error.message()))
                })?;
                current_button.type_ = InlineKeyboardButtonType::Url;
                current_button.data = crate::misc::clean_input_string(&url).ok_or_else(|| {
                    Status::error(400, "Inline keyboard button URL must be encoded in UTF-8")
                })?;
            }
        }
        td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeCallback(button_type) => {
            current_button.type_ = InlineKeyboardButtonType::Callback;
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
            let Some(target_chat) = button_type.target_chat.as_deref() else {
                return Err(Status::error(400, "Target chat must be non-empty"));
            };
            match target_chat {
                td_api::TargetChat::targetChatChosen(target_chat) => {
                    let types = target_chat
                        .types
                        .as_ref()
                        .map(|types| TargetDialogTypes::get_target_dialog_types(Some(types)))
                        .transpose()?
                        .unwrap_or_default();
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
                let button_name = if current_button.type_ == InlineKeyboardButtonType::SwitchInline
                {
                    "switch_inline_query"
                } else {
                    "switch_inline_query_current_chat"
                };
                return Err(Status::error(
                    400,
                    &format!(
                        "Can't use {button_name} button in a channel chat, because users will not be able to use \
                         the button without knowing bot's username"
                    ),
                ));
            }

            current_button.data =
                crate::misc::clean_input_string(&button_type.query).ok_or_else(|| {
                    Status::error(
                        400,
                        "Inline keyboard button switch inline query must be encoded in UTF-8",
                    )
                })?;
        }
        td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeBuy(_) => {
            current_button.type_ = InlineKeyboardButtonType::Buy;
        }
        td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeLoginUrl(button_type) => {
            let user_id = get_link_user_id(&button_type.url);
            if user_id.is_valid() {
                return Err(Status::error(
                    400,
                    "Link to a user can't be used in login URL buttons",
                ));
            }
            let url = check_link(&button_type.url, true, !env.is_test_dc()).map_err(|error| {
                Status::error(
                    400,
                    &format!("Inline keyboard button login {}", error.message()),
                )
            })?;
            current_button.type_ = InlineKeyboardButtonType::UrlAuth;
            current_button.data = crate::misc::clean_input_string(&url).ok_or_else(|| {
                Status::error(
                    400,
                    "Inline keyboard button login URL must be encoded in UTF-8",
                )
            })?;
            current_button.forward_text =
                crate::misc::clean_input_string(&button_type.forward_text).ok_or_else(|| {
                    Status::error(
                        400,
                        "Inline keyboard button forward text must be encoded in UTF-8",
                    )
                })?;
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
            let user_id = get_link_user_id(&button_type.url);
            if user_id.is_valid() {
                return Err(Status::error(
                    400,
                    "Link to a user can't be used in Web App URL buttons",
                ));
            }
            let url = check_link(&button_type.url, true, !env.is_test_dc()).map_err(|error| {
                Status::error(
                    400,
                    &format!("Inline keyboard button Web App {}", error.message()),
                )
            })?;
            current_button.type_ = InlineKeyboardButtonType::WebView;
            current_button.data = crate::misc::clean_input_string(&url).ok_or_else(|| {
                Status::error(
                    400,
                    "Inline keyboard button Web App URL must be encoded in UTF-8",
                )
            })?;
        }
        td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeCopyText(button_type) => {
            current_button.type_ = InlineKeyboardButtonType::Copy;
            current_button.data =
                crate::misc::clean_input_string(&button_type.text).ok_or_else(|| {
                    Status::error(
                        400,
                        "Inline keyboard button copied text must be encoded in UTF-8",
                    )
                })?;
        }
        td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeDisabled(_) => {
            current_button.type_ = InlineKeyboardButtonType::Disabled;
        }
    }

    Ok(current_button)
}

/// `get_input_keyboard_inline_button` (InlineKeyboardButton.cpp:389-468) —
/// the InputUser construction rides the [`UserInputEnv`] seam.
pub trait UserInputEnv {
    /// `UserManager::get_input_user` — the error band collapses to the
    /// C++ `inputUserEmpty` fallback at the call sites.
    fn get_input_user(
        &mut self,
        user_id: UserId,
    ) -> std::result::Result<telegram_api::InputUser, Status>;
    /// `UserManager::is_user_bot(get_my_id())` — the object-builder probe.
    fn is_my_user_bot(&self) -> bool;
    /// `UserManager::get_user_id_object`.
    fn get_user_id_object(&mut self, user_id: UserId, source: &str) -> i64;
}

pub fn get_input_keyboard_inline_button<U: UserInputEnv + ?Sized>(
    user_manager: &mut U,
    keyboard_button: &InlineKeyboardButton,
) -> telegram_api::KeyboardInlineButton {
    let type_: telegram_api::InlineButtonType = match keyboard_button.type_ {
        InlineKeyboardButtonType::Url => {
            telegram_api::InlineButtonType::inlineButtonTypeUrl(telegram_api::inlineButtonTypeUrl {
                url: keyboard_button.data.clone(),
            })
        }
        InlineKeyboardButtonType::Callback => {
            telegram_api::InlineButtonType::inlineButtonTypeCallback(
                telegram_api::inlineButtonTypeCallback {
                    flags: 0,
                    requires_password: false,
                    data: keyboard_button.data.as_bytes().to_vec(),
                },
            )
        }
        InlineKeyboardButtonType::CallbackGame => {
            telegram_api::InlineButtonType::inlineButtonTypeGame(
                telegram_api::inlineButtonTypeGame {},
            )
        }
        InlineKeyboardButtonType::SwitchInline => {
            let peer_types =
                TargetDialogTypes::from_mask(keyboard_button.id).get_input_peer_types();
            let mut flags = 0;
            if !peer_types.is_empty() {
                // inlineButtonTypeSwitchInline::PEER_TYPES_MASK
                flags |= 1 << 0;
            }
            telegram_api::InlineButtonType::inlineButtonTypeSwitchInline(
                telegram_api::inlineButtonTypeSwitchInline {
                    flags,
                    same_peer: false,
                    query: keyboard_button.data.clone(),
                    peer_types,
                },
            )
        }
        InlineKeyboardButtonType::SwitchInlineCurrentDialog => {
            telegram_api::InlineButtonType::inlineButtonTypeSwitchInline(
                telegram_api::inlineButtonTypeSwitchInline {
                    flags: 0,
                    same_peer: true,
                    query: keyboard_button.data.clone(),
                    peer_types: Vec::new(),
                },
            )
        }
        InlineKeyboardButtonType::Buy => telegram_api::InlineButtonType::inlineButtonTypeBuy(
            telegram_api::inlineButtonTypeBuy {},
        ),
        InlineKeyboardButtonType::UrlAuth => {
            let mut flags = 0;
            let mut request_write_access = false;
            let mut bot_user_id = keyboard_button.id;
            if bot_user_id >= 0 {
                request_write_access = true;
            } else {
                bot_user_id = -bot_user_id - 1;
            }
            if !keyboard_button.forward_text.is_empty() {
                // inputInlineButtonTypeUrlAuth::FWD_TEXT_MASK
                flags |= 1 << 0;
            }
            let mut input_user: Option<telegram_api::InputUser> = None;
            if bot_user_id != 0 {
                match user_manager.get_input_user(UserId::new(bot_user_id)) {
                    Ok(input) => {
                        // inputInlineButtonTypeUrlAuth::BOT_MASK
                        flags |= 1 << 1;
                        input_user = Some(input);
                    }
                    Err(error) => {
                        tdutils::log_error!("Failed to get InputUser for {bot_user_id}: {error}");
                        return telegram_api::KeyboardInlineButton {
                            flags: 0,
                            style: keyboard_button.style.get_input_keyboard_button_style(),
                            text: keyboard_button.text.clone(),
                            r#type: Some(telegram_api::InlineButtonType::inlineButtonTypeUrl(
                                telegram_api::inlineButtonTypeUrl {
                                    url: keyboard_button.data.clone(),
                                },
                            )),
                        };
                    }
                }
            }
            telegram_api::InlineButtonType::inputInlineButtonTypeUrlAuth(
                telegram_api::inputInlineButtonTypeUrlAuth {
                    flags,
                    request_write_access,
                    fwd_text: keyboard_button.forward_text.clone(),
                    url: keyboard_button.data.clone(),
                    bot: input_user,
                },
            )
        }
        InlineKeyboardButtonType::CallbackWithPassword => {
            // the C++ UNREACHABLEs; the caller-invariant collapse answers
            // the plain callback
            telegram_api::InlineButtonType::inlineButtonTypeCallback(
                telegram_api::inlineButtonTypeCallback {
                    flags: 0,
                    requires_password: false,
                    data: keyboard_button.data.as_bytes().to_vec(),
                },
            )
        }
        InlineKeyboardButtonType::User => {
            let input_user = match user_manager.get_input_user(keyboard_button.user_id) {
                Ok(input) => input,
                Err(error) => {
                    tdutils::log_error!(
                        "Failed to get InputUser for {}: {error}",
                        keyboard_button.user_id.get()
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
                    url: keyboard_button.data.clone(),
                },
            )
        }
        InlineKeyboardButtonType::Copy => telegram_api::InlineButtonType::inlineButtonTypeCopy(
            telegram_api::inlineButtonTypeCopy {
                copy_text: keyboard_button.data.clone(),
            },
        ),
        InlineKeyboardButtonType::Disabled => {
            telegram_api::InlineButtonType::inlineButtonTypeDisabled(
                telegram_api::inlineButtonTypeDisabled {},
            )
        }
    };
    let style = keyboard_button.style.get_input_keyboard_button_style();
    let mut flags = 0;
    if style.is_some() {
        flags |= 1 << 10;
    }
    telegram_api::KeyboardInlineButton {
        flags,
        style,
        text: keyboard_button.text.clone(),
        r#type: Some(type_),
    }
}

/// `get_inline_keyboard_button_object` (InlineKeyboardButton.cpp:470-527).
pub fn get_inline_keyboard_button_object<U: UserInputEnv + ?Sized>(
    user_manager: &mut U,
    keyboard_button: &InlineKeyboardButton,
) -> td_api::InlineKeyboardButtonType {
    let type_: td_api::InlineKeyboardButtonType = match keyboard_button.type_ {
        InlineKeyboardButtonType::Url => {
            td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeUrl(
                td_api::inlineKeyboardButtonTypeUrl {
                    url: keyboard_button.data.clone(),
                },
            )
        }
        InlineKeyboardButtonType::Callback => {
            td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeCallback(
                td_api::inlineKeyboardButtonTypeCallback {
                    data: keyboard_button.data.as_bytes().to_vec(),
                },
            )
        }
        InlineKeyboardButtonType::CallbackGame => {
            td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeCallbackGame(
                td_api::inlineKeyboardButtonTypeCallbackGame {},
            )
        }
        InlineKeyboardButtonType::SwitchInline => {
            td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeSwitchInline(
                td_api::inlineKeyboardButtonTypeSwitchInline {
                    query: keyboard_button.data.clone(),
                    target_chat: Some(Box::new(td_api::TargetChat::targetChatChosen(
                        td_api::targetChatChosen {
                            types: Some(
                                TargetDialogTypes::from_mask(keyboard_button.id)
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
                    query: keyboard_button.data.clone(),
                    target_chat: Some(Box::new(td_api::TargetChat::targetChatCurrent(
                        td_api::targetChatCurrent {},
                    ))),
                },
            )
        }
        InlineKeyboardButtonType::Buy => {
            td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeBuy(
                td_api::inlineKeyboardButtonTypeBuy {},
            )
        }
        InlineKeyboardButtonType::UrlAuth => {
            td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeLoginUrl(
                td_api::inlineKeyboardButtonTypeLoginUrl {
                    url: keyboard_button.data.clone(),
                    id: keyboard_button.id,
                    forward_text: keyboard_button.forward_text.clone(),
                },
            )
        }
        InlineKeyboardButtonType::CallbackWithPassword => {
            td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeCallbackWithPassword(
                td_api::inlineKeyboardButtonTypeCallbackWithPassword {
                    data: keyboard_button.data.as_bytes().to_vec(),
                },
            )
        }
        InlineKeyboardButtonType::User => {
            let need_user = !user_manager.is_my_user_bot();
            let user_id = if need_user {
                user_manager.get_user_id_object(
                    keyboard_button.user_id,
                    "get_inline_keyboard_button_object",
                )
            } else {
                keyboard_button.user_id.get()
            };
            td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeUser(
                td_api::inlineKeyboardButtonTypeUser { user_id },
            )
        }
        InlineKeyboardButtonType::WebView => {
            td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeWebApp(
                td_api::inlineKeyboardButtonTypeWebApp {
                    url: keyboard_button.data.clone(),
                },
            )
        }
        InlineKeyboardButtonType::Copy => {
            td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeCopyText(
                td_api::inlineKeyboardButtonTypeCopyText {
                    text: keyboard_button.data.clone(),
                },
            )
        }
        InlineKeyboardButtonType::Disabled => {
            td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeDisabled(
                td_api::inlineKeyboardButtonTypeDisabled {},
            )
        }
    };
    type_
}
