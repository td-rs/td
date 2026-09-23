//! Port of `td/telegram/ReplyMarkup.{h,cpp,hpp}` — the shared reply-markup
//! model (the internal `Message::reply_markup` plane), its three conversion
//! directions (wire intake, td_api intake/output), the db store/parse and
//! the duplication/dependency walks.

use crate::dialog_id::{DialogId, DialogType};
use crate::inline_keyboard_button::InlineKeyboardButton;
use crate::keyboard_button::KeyboardButton;
use crate::message_content_dup_type::MessageContentDupType;
use crate::store_parse::{Parser, store_flags, store_i32, store_string};
use crate::td_api;
use tdmtproto::telegram_api;
use tdutils::status::Status;

/// `ReplyMarkup::Type` (ReplyMarkup.h:27) — append only; the int32
/// discriminants mirror the C++ enum values (the store/parse framing
/// writes them like `store(reply_markup.type, storer)` does).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ReplyMarkupType {
    #[default]
    InlineKeyboard,
    ShowKeyboard,
    RemoveKeyboard,
    ForceReply,
}

impl ReplyMarkupType {
    fn from_i32(value: i32) -> ReplyMarkupType {
        match value {
            1 => ReplyMarkupType::ShowKeyboard,
            2 => ReplyMarkupType::RemoveKeyboard,
            3 => ReplyMarkupType::ForceReply,
            _ => ReplyMarkupType::InlineKeyboard,
        }
    }

    fn as_i32(self) -> i32 {
        match self {
            ReplyMarkupType::InlineKeyboard => 0,
            ReplyMarkupType::ShowKeyboard => 1,
            ReplyMarkupType::RemoveKeyboard => 2,
            ReplyMarkupType::ForceReply => 3,
        }
    }
}

/// `td::ReplyMarkup` (ReplyMarkup.h:26-56).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReplyMarkup {
    pub r#type: ReplyMarkupType,
    /// for ShowKeyboard, RemoveKeyboard, ForceReply
    pub is_personal: bool,
    /// for ShowKeyboard, InlineKeyboard
    pub force_reply: bool,
    /// for ShowKeyboard
    pub is_persistent: bool,
    /// for ShowKeyboard
    pub need_resize_keyboard: bool,
    /// for ShowKeyboard
    pub is_one_time_keyboard: bool,
    /// for ShowKeyboard
    pub keyboard: Vec<Vec<KeyboardButton>>,
    /// for ShowKeyboard, ForceReply
    pub placeholder: String,
    /// for InlineKeyboard
    pub inline_keyboard: Vec<Vec<InlineKeyboardButton>>,
}

/// `store(const ReplyMarkup &, StorerT &)` (ReplyMarkup.hpp:25-54).
pub fn store_reply_markup(reply_markup: &ReplyMarkup, out: &mut Vec<u8>) {
    let has_keyboard = !reply_markup.keyboard.is_empty();
    let has_inline_keyboard = !reply_markup.inline_keyboard.is_empty();
    let has_placeholder = !reply_markup.placeholder.is_empty();
    store_flags(
        out,
        [
            reply_markup.is_personal,
            reply_markup.need_resize_keyboard,
            reply_markup.is_one_time_keyboard,
            has_keyboard,
            has_inline_keyboard,
            has_placeholder,
            reply_markup.is_persistent,
            reply_markup.force_reply,
        ],
    );
    store_i32(out, reply_markup.r#type.as_i32());
    if has_keyboard {
        store_i32(out, reply_markup.keyboard.len() as i32);
        for row in &reply_markup.keyboard {
            store_i32(out, row.len() as i32);
            for button in row {
                button.store(out);
            }
        }
    }
    if has_inline_keyboard {
        store_i32(out, reply_markup.inline_keyboard.len() as i32);
        for row in &reply_markup.inline_keyboard {
            store_i32(out, row.len() as i32);
            for button in row {
                button.store(out);
            }
        }
    }
    if has_placeholder {
        store_string(out, &reply_markup.placeholder);
    }
}

/// `parse(ReplyMarkup &, ParserT &)` (ReplyMarkup.hpp:57-86).
pub fn parse_reply_markup(parser: &mut Parser<'_>) -> Result<ReplyMarkup, &'static str> {
    let [
        is_personal,
        need_resize_keyboard,
        is_one_time_keyboard,
        has_keyboard,
        has_inline_keyboard,
        has_placeholder,
        is_persistent,
        force_reply,
    ] = parser.parse_flags()?;
    let type_ = ReplyMarkupType::from_i32(parser.i32_()?);
    let mut keyboard = Vec::new();
    if has_keyboard {
        let rows = parser.i32_()?;
        if rows < 0 {
            return Err("Wrong vector length");
        }
        for _ in 0..rows {
            let buttons_len = parser.i32_()?;
            if buttons_len < 0 {
                return Err("Wrong vector length");
            }
            let mut row = Vec::with_capacity(buttons_len as usize);
            for _ in 0..buttons_len {
                row.push(KeyboardButton::parse(parser)?);
            }
            keyboard.push(row);
        }
    }
    let mut inline_keyboard = Vec::new();
    if has_inline_keyboard {
        let rows = parser.i32_()?;
        if rows < 0 {
            return Err("Wrong vector length");
        }
        for _ in 0..rows {
            let buttons_len = parser.i32_()?;
            if buttons_len < 0 {
                return Err("Wrong vector length");
            }
            let mut row = Vec::with_capacity(buttons_len as usize);
            for _ in 0..buttons_len {
                row.push(InlineKeyboardButton::parse(parser)?);
            }
            inline_keyboard.push(row);
        }
    }
    let placeholder = if has_placeholder {
        parser.string()?
    } else {
        String::new()
    };
    Ok(ReplyMarkup {
        r#type: type_,
        is_personal,
        force_reply,
        is_persistent,
        need_resize_keyboard,
        is_one_time_keyboard,
        keyboard,
        placeholder,
        inline_keyboard,
    })
}

impl ReplyMarkup {
    /// `ReplyMarkup::get_requested_dialog_type` (ReplyMarkup.cpp:467-477).
    pub fn get_requested_dialog_type(
        &self,
        button_id: i32,
    ) -> Option<&crate::requested_dialog_type::RequestedDialogType> {
        for row in &self.keyboard {
            for button in row {
                let requested_dialog_type = button.get_requested_dialog_type();
                if let Some(requested_dialog_type) = requested_dialog_type
                    && requested_dialog_type.get_button_id() == button_id
                {
                    return Some(requested_dialog_type);
                }
            }
        }
        None
    }

    /// `ReplyMarkup::get_login_button_url` (ReplyMarkup.cpp:479-489).
    pub fn get_login_button_url(&self, button_id: i64) -> Option<&str> {
        for row in &self.inline_keyboard {
            for button in row {
                if let Some(login_url) = button.get_login_url(button_id) {
                    return Some(login_url);
                }
            }
        }
        None
    }

    /// `ReplyMarkup::has_buy_button` (ReplyMarkup.cpp:491-493).
    pub fn has_buy_button(&self) -> bool {
        !self.inline_keyboard.is_empty()
            && !self.inline_keyboard[0].is_empty()
            && self.inline_keyboard[0][0].is_buy()
    }
}

/// `get_reply_markup(telegram_api::ReplyMarkup &&, is_bot,
/// only_inline_keyboard, message_contains_mention)`
/// (ReplyMarkup.cpp:104-199) — the wire intake.
pub fn get_reply_markup_from_telegram(
    reply_markup_ptr: Option<telegram_api::ReplyMarkup>,
    is_bot: bool,
    only_inline_keyboard: bool,
    message_contains_mention: bool,
) -> Option<ReplyMarkup> {
    let reply_markup_ptr = reply_markup_ptr?;
    if only_inline_keyboard
        && !matches!(
            reply_markup_ptr,
            telegram_api::ReplyMarkup::replyInlineMarkup(_)
        )
    {
        return None;
    }
    let mut reply_markup = ReplyMarkup::default();
    match reply_markup_ptr {
        telegram_api::ReplyMarkup::replyInlineMarkup(inline_markup) => {
            reply_markup.r#type = ReplyMarkupType::InlineKeyboard;
            reply_markup
                .inline_keyboard
                .reserve(inline_markup.rows.len());
            for row in inline_markup.rows {
                let mut buttons = Vec::with_capacity(row.buttons.len());
                for button in row.buttons {
                    buttons.push(InlineKeyboardButton::from_telegram(button));
                    if buttons.last().is_some_and(|button| {
                        button.text.is_empty()
                            && !button.style.get_icon_custom_emoji_id().is_valid()
                    }) {
                        buttons.pop();
                    }
                }
                if !buttons.is_empty() {
                    reply_markup.inline_keyboard.push(buttons);
                }
            }
            if reply_markup.inline_keyboard.is_empty() {
                return None;
            }
            reply_markup.force_reply = inline_markup.force_reply;
        }
        telegram_api::ReplyMarkup::replyKeyboardMarkup(keyboard_markup) => {
            reply_markup.r#type = ReplyMarkupType::ShowKeyboard;
            reply_markup.is_persistent = keyboard_markup.persistent;
            reply_markup.need_resize_keyboard = keyboard_markup.resize;
            reply_markup.is_one_time_keyboard = keyboard_markup.single_use;
            reply_markup.is_personal = keyboard_markup.selective;
            reply_markup.force_reply = keyboard_markup.force_reply;
            reply_markup.placeholder = keyboard_markup.placeholder;
            reply_markup.keyboard.reserve(keyboard_markup.rows.len());
            for row in keyboard_markup.rows {
                let mut buttons = Vec::with_capacity(row.buttons.len());
                for button in row.buttons {
                    buttons.push(KeyboardButton::from_telegram(button));
                    if buttons.last().is_some_and(KeyboardButton::is_empty) {
                        buttons.pop();
                    }
                }
                if !buttons.is_empty() {
                    reply_markup.keyboard.push(buttons);
                }
            }
            if reply_markup.keyboard.is_empty() {
                return None;
            }
        }
        telegram_api::ReplyMarkup::replyKeyboardHide(hide_keyboard_markup) => {
            reply_markup.r#type = ReplyMarkupType::RemoveKeyboard;
            reply_markup.is_personal = hide_keyboard_markup.selective;
        }
        telegram_api::ReplyMarkup::replyKeyboardForceReply(force_reply_markup) => {
            reply_markup.r#type = ReplyMarkupType::ForceReply;
            reply_markup.is_personal = force_reply_markup.selective;
            reply_markup.placeholder = force_reply_markup.placeholder;
        }
    }

    if !is_bot && reply_markup.r#type != ReplyMarkupType::InlineKeyboard {
        // incoming keyboard
        if reply_markup.is_personal {
            if !message_contains_mention {
                reply_markup.is_personal = false;
                reply_markup.force_reply = false;
            }
        } else {
            reply_markup.is_personal = true;
        }
    }

    Some(reply_markup)
}

/// `get_reply_markup(td_api::ReplyMarkup &&, is_bot, only_inline_keyboard,
/// request_buttons_allowed, switch_inline_buttons_allowed, allow_personal)`
/// (ReplyMarkup.cpp:201-317) — the td_api intake.
#[allow(clippy::too_many_arguments)]
fn get_reply_markup_from_td_api(
    reply_markup_ptr: Option<td_api::ReplyMarkup>,
    is_bot: bool,
    only_inline_keyboard: bool,
    request_buttons_allowed: bool,
    switch_inline_buttons_allowed: bool,
    allow_personal: bool,
    is_test_dc: bool,
) -> Result<Option<ReplyMarkup>, Status> {
    let reply_markup_ptr = match reply_markup_ptr {
        Some(reply_markup_ptr) if is_bot => reply_markup_ptr,
        _ => return Ok(None),
    };

    let mut reply_markup = ReplyMarkup::default();
    match reply_markup_ptr {
        td_api::ReplyMarkup::replyMarkupShowKeyboard(show_keyboard_markup) => {
            if only_inline_keyboard {
                return Err(Status::error(400, "Inline keyboard expected"));
            }
            reply_markup.r#type = ReplyMarkupType::ShowKeyboard;
            reply_markup.is_persistent = show_keyboard_markup.is_persistent;
            reply_markup.need_resize_keyboard = show_keyboard_markup.resize_keyboard;
            reply_markup.is_one_time_keyboard = show_keyboard_markup.one_time;
            reply_markup.is_personal = show_keyboard_markup.is_personal && allow_personal;
            reply_markup.force_reply = show_keyboard_markup.force_reply;
            reply_markup.placeholder = show_keyboard_markup.input_field_placeholder;

            let mut total_button_count = 0;
            for row in show_keyboard_markup.rows {
                let mut row_buttons = Vec::with_capacity(row.len());
                let mut row_button_count = 0;
                for button in row {
                    if button.text.is_empty() && button.icon_custom_emoji_id == 0 {
                        continue;
                    }
                    row_buttons.push(KeyboardButton::from_td_api(
                        button,
                        request_buttons_allowed,
                        is_test_dc,
                    )?);
                    row_button_count += 1;
                    total_button_count += 1;
                    if row_button_count >= 12 || total_button_count >= 300 {
                        break;
                    }
                }
                if !row_buttons.is_empty() {
                    reply_markup.keyboard.push(row_buttons);
                }
                if total_button_count >= 300 {
                    break;
                }
            }
            if reply_markup.keyboard.is_empty() {
                return Ok(None);
            }
        }
        td_api::ReplyMarkup::replyMarkupInlineKeyboard(inline_keyboard_markup) => {
            if only_inline_keyboard {
                // no-op: this IS the inline keyboard
            }
            reply_markup.r#type = ReplyMarkupType::InlineKeyboard;

            let mut total_button_count = 0;
            for row in inline_keyboard_markup.rows {
                let mut row_buttons = Vec::with_capacity(row.len());
                let mut row_button_count = 0;
                for button in row {
                    if button.text.is_empty() && button.icon_custom_emoji_id == 0 {
                        continue;
                    }
                    row_buttons.push(InlineKeyboardButton::from_td_api(
                        button,
                        switch_inline_buttons_allowed,
                        is_test_dc,
                    )?);
                    row_button_count += 1;
                    total_button_count += 1;
                    if row_button_count >= 12 || total_button_count >= 300 {
                        break;
                    }
                }
                if !row_buttons.is_empty() {
                    reply_markup.inline_keyboard.push(row_buttons);
                }
                if total_button_count >= 300 {
                    break;
                }
            }
            if reply_markup.inline_keyboard.is_empty() {
                return Ok(None);
            }
            reply_markup.force_reply = inline_keyboard_markup.force_reply;
        }
        td_api::ReplyMarkup::replyMarkupRemoveKeyboard(remove_keyboard_markup) => {
            if only_inline_keyboard {
                return Err(Status::error(400, "Inline keyboard expected"));
            }
            reply_markup.r#type = ReplyMarkupType::RemoveKeyboard;
            reply_markup.is_personal = remove_keyboard_markup.is_personal && allow_personal;
        }
        td_api::ReplyMarkup::replyMarkupForceReply(force_reply_markup) => {
            if only_inline_keyboard {
                return Err(Status::error(400, "Inline keyboard expected"));
            }
            reply_markup.r#type = ReplyMarkupType::ForceReply;
            reply_markup.is_personal = force_reply_markup.is_personal && allow_personal;
            reply_markup.placeholder = force_reply_markup.input_field_placeholder;
        }
    }
    Ok(Some(reply_markup))
}

/// `get_inline_reply_markup` (ReplyMarkup.cpp:319-322).
pub fn get_inline_reply_markup(
    reply_markup_ptr: Option<td_api::ReplyMarkup>,
    is_bot: bool,
    switch_inline_buttons_allowed: bool,
    is_test_dc: bool,
) -> Result<Option<ReplyMarkup>, Status> {
    get_reply_markup_from_td_api(
        reply_markup_ptr,
        is_bot,
        true,
        false,
        switch_inline_buttons_allowed,
        false,
        is_test_dc,
    )
}

/// `get_reply_markup(td_api::ReplyMarkup &&, dialog_type,
/// is_admined_monoforum, is_bot, is_anonymous)` (ReplyMarkup.cpp:324-333).
pub fn get_reply_markup(
    reply_markup_ptr: Option<td_api::ReplyMarkup>,
    dialog_type: DialogType,
    is_admined_monoforum: bool,
    is_bot: bool,
    is_anonymous: bool,
    is_test_dc: bool,
) -> Result<Option<ReplyMarkup>, Status> {
    let only_inline_keyboard = is_anonymous && !is_admined_monoforum;
    let request_buttons_allowed = dialog_type == DialogType::User;
    let switch_inline_buttons_allowed = !is_anonymous;
    let allow_personal = dialog_type != DialogType::User;
    get_reply_markup_from_td_api(
        reply_markup_ptr,
        is_bot,
        only_inline_keyboard,
        request_buttons_allowed,
        switch_inline_buttons_allowed,
        allow_personal,
        is_test_dc,
    )
}

/// `dup_reply_markup` (ReplyMarkup.cpp:335-372).
pub fn dup_reply_markup(
    reply_markup: &Option<ReplyMarkup>,
    dialog_id: DialogId,
    dup_type: MessageContentDupType,
    is_via_bot: bool,
) -> Option<ReplyMarkup> {
    let reply_markup = reply_markup.as_ref()?;
    if dialog_id.get_type() == DialogType::SecretChat
        || dup_type == MessageContentDupType::Copy
        || dup_type == MessageContentDupType::ServerCopy
    {
        return None;
    }
    let is_send = dup_type.is_send();
    if !is_send && reply_markup.r#type != ReplyMarkupType::InlineKeyboard {
        return None;
    }
    let mut result = ReplyMarkup {
        r#type: reply_markup.r#type,
        is_personal: reply_markup.is_personal,
        ..ReplyMarkup::default()
    };
    if is_send {
        result.force_reply = reply_markup.force_reply;
    }
    result.is_persistent = reply_markup.is_persistent;
    result.need_resize_keyboard = reply_markup.need_resize_keyboard;
    result.keyboard = reply_markup
        .keyboard
        .iter()
        .map(|row| row.iter().map(|button| button.clone_button()).collect())
        .collect();
    result.placeholder = reply_markup.placeholder.clone();
    let mut need_drop = false;
    result.inline_keyboard = reply_markup
        .inline_keyboard
        .iter()
        .map(|row| {
            row.iter()
                .map(|button| {
                    let new_button = button.clone_button(dialog_id, dup_type, is_via_bot, false);
                    if new_button.is_disabled() && !button.is_disabled() {
                        need_drop = true;
                    }
                    new_button
                })
                .collect()
        })
        .collect();
    if need_drop {
        return None;
    }
    Some(result)
}

/// `ReplyMarkup::get_input_reply_markup` (ReplyMarkup.cpp:374-423).
pub fn get_input_reply_markup(reply_markup: &ReplyMarkup) -> Option<telegram_api::ReplyMarkup> {
    match reply_markup.r#type {
        ReplyMarkupType::InlineKeyboard => {
            let mut rows = Vec::with_capacity(reply_markup.inline_keyboard.len());
            for row in &reply_markup.inline_keyboard {
                let buttons = row
                    .iter()
                    .map(|button| button.get_input_keyboard_inline_button(&()))
                    .collect();
                rows.push(telegram_api::KeyboardInlineButtonRow { buttons });
            }
            Some(telegram_api::ReplyMarkup::replyInlineMarkup(
                telegram_api::replyInlineMarkup {
                    flags: 0,
                    force_reply: reply_markup.force_reply,
                    rows,
                },
            ))
        }
        ReplyMarkupType::ShowKeyboard => {
            let mut rows = Vec::with_capacity(reply_markup.keyboard.len());
            for row in &reply_markup.keyboard {
                let buttons = row
                    .iter()
                    .map(|button| button.get_input_keyboard_button())
                    .collect();
                rows.push(telegram_api::KeyboardButtonRow { buttons });
            }
            // replyKeyboardMarkup::PLACEHOLDER_MASK (flag bit 3)
            let mut flags = 0;
            if !reply_markup.placeholder.is_empty() {
                flags |= 1 << 3;
            }
            Some(telegram_api::ReplyMarkup::replyKeyboardMarkup(
                telegram_api::replyKeyboardMarkup {
                    flags,
                    resize: reply_markup.need_resize_keyboard,
                    single_use: reply_markup.is_one_time_keyboard,
                    selective: reply_markup.is_personal,
                    persistent: reply_markup.is_persistent,
                    force_reply: reply_markup.force_reply,
                    rows,
                    placeholder: reply_markup.placeholder.clone(),
                },
            ))
        }
        ReplyMarkupType::ForceReply => {
            // replyKeyboardForceReply::PLACEHOLDER_MASK (flag bit 3)
            let mut flags = 0;
            if !reply_markup.placeholder.is_empty() {
                flags |= 1 << 3;
            }
            Some(telegram_api::ReplyMarkup::replyKeyboardForceReply(
                telegram_api::replyKeyboardForceReply {
                    flags,
                    single_use: reply_markup.is_one_time_keyboard,
                    selective: reply_markup.is_personal,
                    placeholder: reply_markup.placeholder.clone(),
                },
            ))
        }
        ReplyMarkupType::RemoveKeyboard => Some(telegram_api::ReplyMarkup::replyKeyboardHide(
            telegram_api::replyKeyboardHide {
                flags: 0,
                selective: reply_markup.is_personal,
            },
        )),
    }
}

/// `get_reply_markup_object(UserManager *, const unique_ptr<ReplyMarkup> &)`
/// (ReplyMarkup.cpp:425-465) — the `UserManager *` rides the env; `Some(&())`
/// is the null-manager band.
pub fn get_reply_markup_object(
    env: Option<&dyn crate::inline_keyboard_button::InlineKeyboardButtonEnv>,
    reply_markup: &ReplyMarkup,
) -> td_api::ReplyMarkup {
    match reply_markup.r#type {
        ReplyMarkupType::InlineKeyboard => {
            let rows: Vec<Vec<td_api::InlineKeyboardButton>> = reply_markup
                .inline_keyboard
                .iter()
                .map(|row| {
                    row.iter()
                        .map(|button| button.get_inline_keyboard_button_object(env))
                        .collect()
                })
                .collect();
            td_api::ReplyMarkup::replyMarkupInlineKeyboard(td_api::replyMarkupInlineKeyboard {
                rows,
                force_reply: reply_markup.force_reply,
            })
        }
        ReplyMarkupType::ShowKeyboard => {
            let rows: Vec<Vec<td_api::KeyboardButton>> = reply_markup
                .keyboard
                .iter()
                .map(|row| {
                    row.iter()
                        .map(|button| button.get_keyboard_button_object())
                        .collect()
                })
                .collect();
            td_api::ReplyMarkup::replyMarkupShowKeyboard(td_api::replyMarkupShowKeyboard {
                rows,
                is_persistent: reply_markup.is_persistent,
                resize_keyboard: reply_markup.need_resize_keyboard,
                one_time: reply_markup.is_one_time_keyboard,
                is_personal: reply_markup.is_personal,
                force_reply: reply_markup.force_reply,
                input_field_placeholder: reply_markup.placeholder.clone(),
            })
        }
        ReplyMarkupType::RemoveKeyboard => {
            td_api::ReplyMarkup::replyMarkupRemoveKeyboard(td_api::replyMarkupRemoveKeyboard {
                is_personal: reply_markup.is_personal,
            })
        }
        ReplyMarkupType::ForceReply => {
            td_api::ReplyMarkup::replyMarkupForceReply(td_api::replyMarkupForceReply {
                is_personal: reply_markup.is_personal,
                input_field_placeholder: reply_markup.placeholder.clone(),
            })
        }
    }
}

/// `add_reply_markup_dependencies(Dependencies &, const
/// unique_ptr<ReplyMarkup> &)` (ReplyMarkup.cpp:513-522) — the
/// inline-button halves only (the C++ inline-button `add_dependencies`);
/// the nullable `unique_ptr` maps to `Option<&ReplyMarkup>`.
pub fn add_reply_markup_dependencies(
    dependencies: &mut dyn crate::inline_keyboard_button::DependenciesSink,
    reply_markup: Option<&ReplyMarkup>,
) {
    let Some(reply_markup) = reply_markup else {
        return;
    };
    for row in &reply_markup.inline_keyboard {
        for button in row {
            button.add_dependencies(dependencies);
        }
    }
}

impl ReplyMarkup {
    /// `ReplyMarkup::print` (ReplyMarkup.cpp:46-98) — the debug render.
    pub fn print(&self, out: &mut String) {
        out.push_str("ReplyMarkup[");
        match self.r#type {
            ReplyMarkupType::InlineKeyboard => out.push_str("InlineKeyboard"),
            ReplyMarkupType::ShowKeyboard => out.push_str("ShowKeyboard"),
            ReplyMarkupType::RemoveKeyboard => out.push_str("RemoveKeyboard"),
            ReplyMarkupType::ForceReply => out.push_str("ForceReply"),
        }
        if self.force_reply {
            out.push_str(", force reply");
        }
        if self.is_personal {
            out.push_str(", personal");
        }
        if !self.placeholder.is_empty() {
            out.push_str(&format!(", placeholder \"{}\"", self.placeholder));
        }
        if self.r#type == ReplyMarkupType::ShowKeyboard {
            if self.is_persistent {
                out.push_str(", persistent");
            }
            if self.need_resize_keyboard {
                out.push_str(", need resize");
            }
            if self.is_one_time_keyboard {
                out.push_str(", one time");
            }
        }
        if self.r#type == ReplyMarkupType::InlineKeyboard {
            for row in &self.inline_keyboard {
                out.push_str(", ");
                for button in row {
                    button.print(out);
                }
            }
        }
        if self.r#type == ReplyMarkupType::ShowKeyboard {
            for row in &self.keyboard {
                out.push_str(", ");
                for button in row {
                    button.print(out);
                }
            }
        }
        out.push(']');
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialog_id::DialogType;
    use crate::keyboard_button_style::KeyboardButtonStyle;
    use crate::message_content_dup_type::MessageContentDupType;

    fn inline_button(text: &str, data: &str) -> InlineKeyboardButton {
        InlineKeyboardButton {
            text: text.to_owned(),
            data: data.to_owned(),
            ..InlineKeyboardButton::default()
        }
    }

    fn keyboard_button(text: &str) -> KeyboardButton {
        KeyboardButton {
            text: text.to_owned(),
            ..KeyboardButton::default()
        }
    }

    #[test]
    fn store_parse_round_trips() {
        let markup = ReplyMarkup {
            r#type: ReplyMarkupType::InlineKeyboard,
            force_reply: true,
            inline_keyboard: vec![
                vec![inline_button("one", "c1")],
                vec![inline_button("two", "c2")],
            ],
            ..ReplyMarkup::default()
        };
        let mut out = Vec::new();
        store_reply_markup(&markup, &mut out);
        let mut parser = Parser::new(&out);
        assert_eq!(parse_reply_markup(&mut parser).unwrap(), markup);

        let markup = ReplyMarkup {
            r#type: ReplyMarkupType::ShowKeyboard,
            is_personal: true,
            is_persistent: true,
            need_resize_keyboard: true,
            placeholder: "placeholder".to_owned(),
            keyboard: vec![vec![keyboard_button("a"), keyboard_button("b")]],
            ..ReplyMarkup::default()
        };
        let mut out = Vec::new();
        store_reply_markup(&markup, &mut out);
        let mut parser = Parser::new(&out);
        assert_eq!(parse_reply_markup(&mut parser).unwrap(), markup);

        for type_ in [
            ReplyMarkupType::InlineKeyboard,
            ReplyMarkupType::ShowKeyboard,
            ReplyMarkupType::RemoveKeyboard,
            ReplyMarkupType::ForceReply,
        ] {
            let markup = ReplyMarkup {
                r#type: type_,
                is_personal: true,
                force_reply: true,
                is_persistent: true,
                need_resize_keyboard: true,
                is_one_time_keyboard: true,
                placeholder: "p".to_owned(),
                ..ReplyMarkup::default()
            };
            let mut out = Vec::new();
            store_reply_markup(&markup, &mut out);
            let mut parser = Parser::new(&out);
            assert_eq!(parse_reply_markup(&mut parser).unwrap(), markup);
        }
    }

    #[test]
    fn empty_markup_round_trips() {
        let markup = ReplyMarkup::default();
        let mut out = Vec::new();
        store_reply_markup(&markup, &mut out);
        let mut parser = Parser::new(&out);
        assert_eq!(parse_reply_markup(&mut parser).unwrap(), markup);
    }

    #[test]
    fn wire_intake_builds_the_rows() {
        let wire = telegram_api::ReplyMarkup::replyInlineMarkup(telegram_api::replyInlineMarkup {
            flags: 0,
            force_reply: true,
            rows: vec![telegram_api::KeyboardInlineButtonRow {
                buttons: vec![telegram_api::KeyboardInlineButton {
                    flags: 0,
                    style: None,
                    text: "click".to_owned(),
                    r#type: Some(telegram_api::InlineButtonType::inlineButtonTypeCallback(
                        telegram_api::inlineButtonTypeCallback {
                            flags: 0,
                            requires_password: false,
                            data: "cb".as_bytes().to_vec(),
                        },
                    )),
                }],
            }],
        });
        let markup = get_reply_markup_from_telegram(Some(wire), false, false, false).unwrap();
        assert_eq!(markup.r#type, ReplyMarkupType::InlineKeyboard);
        assert!(markup.force_reply);
        assert_eq!(markup.inline_keyboard.len(), 1);
        assert_eq!(markup.inline_keyboard[0][0].text, "click");
    }

    #[test]
    fn wire_intake_drops_empty_inline_rows() {
        // a row of only text-less buttons is dropped, and an empty keyboard
        // answers None
        let wire = telegram_api::ReplyMarkup::replyInlineMarkup(telegram_api::replyInlineMarkup {
            flags: 0,
            force_reply: false,
            rows: vec![telegram_api::KeyboardInlineButtonRow {
                buttons: vec![telegram_api::KeyboardInlineButton {
                    flags: 0,
                    style: None,
                    text: String::new(),
                    r#type: None,
                }],
            }],
        });
        assert!(get_reply_markup_from_telegram(Some(wire), false, false, false).is_none());
    }

    #[test]
    fn wire_intake_personal_flip() {
        // non-bot incoming show keyboard without a mention flips personal on
        let wire = telegram_api::ReplyMarkup::replyKeyboardHide(telegram_api::replyKeyboardHide {
            flags: 0,
            selective: false,
        });
        let markup = get_reply_markup_from_telegram(Some(wire), false, false, false).unwrap();
        assert!(markup.is_personal);
        // with a mention the personal flag is preserved
        let wire = telegram_api::ReplyMarkup::replyKeyboardHide(telegram_api::replyKeyboardHide {
            flags: 0,
            selective: true,
        });
        let markup = get_reply_markup_from_telegram(Some(wire), false, false, true).unwrap();
        assert!(markup.is_personal);
    }

    #[test]
    fn td_api_intake_show_keyboard_rounds() {
        let td = td_api::ReplyMarkup::replyMarkupShowKeyboard(td_api::replyMarkupShowKeyboard {
            rows: vec![vec![td_api::KeyboardButton {
                text: "yes".to_owned(),
                icon_custom_emoji_id: 0,
                style: None,
                r#type: Some(td_api::KeyboardButtonType::keyboardButtonTypeText(
                    td_api::keyboardButtonTypeText,
                )),
            }]],
            is_persistent: true,
            resize_keyboard: false,
            one_time: true,
            is_personal: false,
            force_reply: false,
            input_field_placeholder: String::new(),
        });
        let markup = get_reply_markup(Some(td), DialogType::User, false, true, false, false)
            .unwrap()
            .unwrap();
        assert_eq!(markup.r#type, ReplyMarkupType::ShowKeyboard);
        assert!(markup.is_persistent);
        assert!(markup.is_one_time_keyboard);
        assert_eq!(markup.keyboard[0][0].text, "yes");
    }

    #[test]
    fn td_api_intake_requires_bot() {
        let td = td_api::ReplyMarkup::replyMarkupForceReply(td_api::replyMarkupForceReply {
            is_personal: false,
            input_field_placeholder: String::new(),
        });
        assert!(
            get_reply_markup(Some(td), DialogType::User, false, false, false, false)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn inline_intake_rejects_show_keyboard() {
        let td = td_api::ReplyMarkup::replyMarkupShowKeyboard(td_api::replyMarkupShowKeyboard {
            rows: Vec::new(),
            is_persistent: false,
            resize_keyboard: false,
            one_time: false,
            is_personal: false,
            force_reply: false,
            input_field_placeholder: String::new(),
        });
        assert!(get_inline_reply_markup(Some(td), true, true, false).is_err());
    }

    #[test]
    fn object_builder_matches_the_intake() {
        let markup = ReplyMarkup {
            r#type: ReplyMarkupType::InlineKeyboard,
            force_reply: true,
            inline_keyboard: vec![vec![inline_button("btn", "c1")]],
            ..ReplyMarkup::default()
        };
        let object = get_reply_markup_object(Some(&()), &markup);
        match object {
            td_api::ReplyMarkup::replyMarkupInlineKeyboard(inline) => {
                assert!(inline.force_reply);
                assert_eq!(inline.rows.len(), 1);
                assert_eq!(inline.rows[0][0].text, "btn");
            }
            other => panic!("wrong markup object: {other:?}"),
        }

        let markup = ReplyMarkup {
            r#type: ReplyMarkupType::RemoveKeyboard,
            is_personal: true,
            ..ReplyMarkup::default()
        };
        match get_reply_markup_object(Some(&()), &markup) {
            td_api::ReplyMarkup::replyMarkupRemoveKeyboard(remove) => {
                assert!(remove.is_personal);
            }
            other => panic!("wrong markup object: {other:?}"),
        }
    }

    #[test]
    fn dup_drops_for_copy_and_secret_chats() {
        let markup = Some(ReplyMarkup {
            r#type: ReplyMarkupType::InlineKeyboard,
            inline_keyboard: vec![vec![inline_button("a", "c1")]],
            ..ReplyMarkup::default()
        });
        assert!(
            dup_reply_markup(
                &markup,
                DialogId::default(),
                MessageContentDupType::Copy,
                false
            )
            .is_none()
        );
        let secret = DialogId::from_secret_chat_id(crate::user_manager::SecretChatId::new(1));
        assert!(dup_reply_markup(&markup, secret, MessageContentDupType::Send, false).is_none());
        let dup = dup_reply_markup(
            &markup,
            DialogId::default(),
            MessageContentDupType::Send,
            false,
        )
        .unwrap();
        assert_eq!(dup.inline_keyboard[0][0].text, "a");
    }

    #[test]
    fn buy_button_probe() {
        let mut markup = ReplyMarkup::default();
        assert!(!markup.has_buy_button());
        markup.inline_keyboard = vec![vec![inline_button("pay", "c1")]];
        assert!(!markup.has_buy_button());
    }

    #[test]
    fn print_renders_the_type() {
        let mut out = String::new();
        ReplyMarkup {
            r#type: ReplyMarkupType::ForceReply,
            is_personal: true,
            placeholder: "ph".to_owned(),
            ..ReplyMarkup::default()
        }
        .print(&mut out);
        assert!(out.contains("ForceReply"));
        assert!(out.contains("personal"));
        assert!(out.contains("ph"));
    }

    #[test]
    fn default_button_style_has_no_icon() {
        assert!(
            !KeyboardButtonStyle::default()
                .get_icon_custom_emoji_id()
                .is_valid()
        );
    }
}
