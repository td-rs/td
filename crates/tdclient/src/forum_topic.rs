//! `td::ForumTopic` + the forum value family (ForumTopicId/
//! ForumTopicFullId/ForumTopicIcon/ForumTopicEditedData/ForumTopicInfo)
//! (ForumTopic.h/.cpp/.hpp + the sibling headers, M.49).
//!
//! The notification-settings and draft-message halves ride the env seams
//! ([blocked: the notification-settings / draft planes]); the message-id
//! and icon planes are byte-faithful.

use crate::dialog_id::DialogId;
use crate::emoji_status::CustomEmojiId;
use crate::store_parse::{self, Parser};
use crate::td_api;
use tdmtproto::telegram_api;

/// `td::ForumTopicId` (ForumTopicId.h).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ForumTopicId(i32);

impl ForumTopicId {
    pub const fn new(id: i32) -> ForumTopicId {
        ForumTopicId(id)
    }

    pub const fn get(self) -> i32 {
        self.0
    }

    /// `ForumTopicId::is_valid` (ForumTopicId.h) — positive.
    pub const fn is_valid(self) -> bool {
        self.0 > 0
    }

    /// `ForumTopicId::general` (ForumTopicId.h:38-40) — the general
    /// topic's fixed id.
    pub const fn general() -> ForumTopicId {
        ForumTopicId(1)
    }

    /// `ForumTopicId::get_forum_topic_ids` (ForumTopicId.h).
    pub fn get_forum_topic_ids(input_ids: &[i32]) -> Vec<ForumTopicId> {
        input_ids.iter().copied().map(ForumTopicId::new).collect()
    }

    /// `ForumTopicId::store` (ForumTopicId.h).
    pub fn store(self, out: &mut Vec<u8>) {
        store_parse::store_i32(out, self.0);
    }

    /// `ForumTopicId::parse` (ForumTopicId.h).
    pub fn parse(parser: &mut Parser) -> std::result::Result<ForumTopicId, &'static str> {
        Ok(ForumTopicId::new(parser.i32_()?))
    }
}

/// `td::ForumTopicFullId` (ForumTopicFullId.h).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ForumTopicFullId {
    dialog_id: DialogId,
    forum_topic_id: ForumTopicId,
}

impl ForumTopicFullId {
    pub const fn new(dialog_id: DialogId, forum_topic_id: ForumTopicId) -> ForumTopicFullId {
        ForumTopicFullId {
            dialog_id,
            forum_topic_id,
        }
    }

    pub fn get_dialog_id(self) -> DialogId {
        self.dialog_id
    }

    pub fn get_forum_topic_id(self) -> ForumTopicId {
        self.forum_topic_id
    }
}

/// `td::ForumTopicIcon` (ForumTopicIcon.h/.cpp/.hpp).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ForumTopicIcon {
    color: i32,
    custom_emoji_id: CustomEmojiId,
}

impl ForumTopicIcon {
    /// The C++ default color (ForumTopicIcon.h).
    const DEFAULT_COLOR: i32 = 0x6FB9F0;

    /// `ForumTopicIcon(int32, int64)` (ForumTopicIcon.cpp:12-14) — the
    /// color keeps the low 24 bits.
    pub fn new(color: i32, custom_emoji_id: i64) -> ForumTopicIcon {
        ForumTopicIcon {
            color: color & 0xFFFFFF,
            custom_emoji_id: CustomEmojiId::new(custom_emoji_id),
        }
    }

    /// The default icon (the C++ `ForumTopicIcon()`).
    pub fn default_icon() -> ForumTopicIcon {
        ForumTopicIcon {
            color: Self::DEFAULT_COLOR,
            custom_emoji_id: CustomEmojiId::default(),
        }
    }

    /// `ForumTopicIcon::edit_custom_emoji_id` (ForumTopicIcon.cpp:16-22).
    pub fn edit_custom_emoji_id(&mut self, custom_emoji_id: CustomEmojiId) -> bool {
        if self.custom_emoji_id != custom_emoji_id {
            self.custom_emoji_id = custom_emoji_id;
            return true;
        }
        false
    }

    /// `get_forum_topic_icon_object` (ForumTopicIcon.cpp:24-26).
    pub fn get_forum_topic_icon_object(self) -> td_api::ForumTopicIcon {
        td_api::ForumTopicIcon {
            color: self.color,
            custom_emoji_id: self.custom_emoji_id.get(),
        }
    }

    /// `store` (ForumTopicIcon.hpp:13-21).
    pub fn store(self, out: &mut Vec<u8>) {
        let has_custom_emoji_id = self.custom_emoji_id.is_valid();
        store_parse::store_flags(out, [has_custom_emoji_id]);
        store_parse::store_i32(out, self.color);
        if has_custom_emoji_id {
            store_parse::store_i64(out, self.custom_emoji_id.get());
        }
    }

    /// `parse` (ForumTopicIcon.hpp:22-31).
    pub fn parse(parser: &mut Parser) -> std::result::Result<ForumTopicIcon, &'static str> {
        let [has_custom_emoji_id] = parser.parse_flags()?;
        let mut result = ForumTopicIcon {
            color: parser.i32_()?,
            custom_emoji_id: CustomEmojiId::default(),
        };
        if has_custom_emoji_id {
            result.custom_emoji_id = CustomEmojiId::new(parser.i64_()?);
        }
        Ok(result)
    }
}

/// `td::ForumTopicEditedData` (ForumTopicEditedData.h/.cpp/.hpp).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ForumTopicEditedData {
    title: String,
    icon_custom_emoji_id: CustomEmojiId,
    edit_icon_custom_emoji_id: bool,
    edit_is_closed: bool,
    is_closed: bool,
    is_hidden: bool,
}

impl ForumTopicEditedData {
    /// `get_edited_data` (ForumTopicEditedData.cpp:13-31).
    pub fn get_edited_data(
        is_closed: bool,
        is_hidden: bool,
        icon_custom_emoji_id: CustomEmojiId,
        edit_icon_custom_emoji_id: bool,
        title: Option<&str>,
    ) -> ForumTopicEditedData {
        ForumTopicEditedData {
            title: title.unwrap_or_default().to_owned(),
            icon_custom_emoji_id,
            edit_icon_custom_emoji_id,
            edit_is_closed: true,
            is_closed,
            is_hidden,
        }
    }

    /// `get_icon_custom_emoji_id` (ForumTopicEditedData.h).
    pub fn get_icon_custom_emoji_id(&self) -> CustomEmojiId {
        self.icon_custom_emoji_id
    }

    /// `get_is_closed` (ForumTopicEditedData.h).
    pub fn get_is_closed(&self) -> bool {
        self.is_closed
    }

    /// `get_is_hidden` (ForumTopicEditedData.h).
    pub fn get_is_hidden(&self) -> bool {
        self.is_hidden
    }

    /// `store` (ForumTopicEditedData.hpp:13-28).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_title = !self.title.is_empty();
        let has_icon_custom_emoji_id = self.icon_custom_emoji_id.is_valid();
        store_parse::store_flags(
            out,
            [
                self.edit_icon_custom_emoji_id,
                self.edit_is_closed,
                self.is_closed,
                has_title,
                has_icon_custom_emoji_id,
                self.is_hidden,
            ],
        );
        if has_title {
            store_parse::store_string(out, &self.title);
        }
        if has_icon_custom_emoji_id {
            store_parse::store_i64(out, self.icon_custom_emoji_id.get());
        }
    }

    /// `parse` (ForumTopicEditedData.hpp:29-47).
    pub fn parse(parser: &mut Parser) -> std::result::Result<ForumTopicEditedData, &'static str> {
        let [
            edit_icon_custom_emoji_id,
            edit_is_closed,
            is_closed,
            has_title,
            has_icon_custom_emoji_id,
            is_hidden,
        ] = parser.parse_flags()?;
        let mut result = ForumTopicEditedData {
            edit_icon_custom_emoji_id,
            edit_is_closed,
            is_closed,
            is_hidden,
            ..ForumTopicEditedData::default()
        };
        if has_title {
            result.title = parser.string()?;
        }
        if has_icon_custom_emoji_id {
            result.icon_custom_emoji_id = CustomEmojiId::new(parser.i64_()?);
        }
        Ok(result)
    }
}

/// `td::ForumTopicInfo` (ForumTopicInfo.h/.cpp/.hpp).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ForumTopicInfo {
    dialog_id: DialogId,
    forum_topic_id: ForumTopicId,
    title: String,
    icon: ForumTopicIcon,
    creation_date: i32,
    creator_dialog_id: DialogId,
    is_outgoing: bool,
    is_closed: bool,
    is_hidden: bool,
    is_title_missing: bool,
}

impl ForumTopicInfo {
    /// `ForumTopicInfo(ForumTopic)` (ForumTopicInfo.cpp:13-40) — the
    /// dialog/creator ids resolve off the wire peers (with the
    /// force-create rides the dialog-inventory plane), the validity and
    /// the expected-dialog bands reset to the empty info like the C++.
    pub fn from_telegram(
        forum_topic: &telegram_api::ForumTopic,
        expected_dialog_id: DialogId,
    ) -> ForumTopicInfo {
        let telegram_api::ForumTopic::forumTopic(forum_topic) = forum_topic else {
            tdutils::log_error!("Receive {}", forum_topic.to_debug_string());
            return ForumTopicInfo::default();
        };
        let mut result = ForumTopicInfo {
            dialog_id: forum_topic
                .peer
                .as_ref()
                .map(DialogId::from_peer)
                .unwrap_or_default(),
            forum_topic_id: ForumTopicId::new(forum_topic.id),
            title: forum_topic.title.clone(),
            icon: ForumTopicIcon::new(forum_topic.icon_color, forum_topic.icon_emoji_id),
            creation_date: forum_topic.date,
            creator_dialog_id: forum_topic
                .from_id
                .as_ref()
                .map(DialogId::from_peer)
                .unwrap_or_default(),
            is_outgoing: forum_topic.my,
            is_closed: forum_topic.closed,
            is_hidden: forum_topic.hidden,
            is_title_missing: forum_topic.title_missing,
        };
        if result.creation_date <= 0
            || !result.forum_topic_id.is_valid()
            || !result.dialog_id.is_valid()
            || !result.creator_dialog_id.is_valid()
        {
            tdutils::log_error!("Receive {}", forum_topic.to_debug_string());
            result = ForumTopicInfo::default();
        }
        if expected_dialog_id != DialogId::default() && result.dialog_id != expected_dialog_id {
            tdutils::log_error!(
                "Receive forum topic in {:?} instead of {expected_dialog_id:?}",
                result.dialog_id
            );
            result = ForumTopicInfo::default();
        }
        result
    }

    /// The all-fields constructor (ForumTopicInfo.h:35-56).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        dialog_id: DialogId,
        forum_topic_id: ForumTopicId,
        title: String,
        icon: ForumTopicIcon,
        creation_date: i32,
        creator_dialog_id: DialogId,
        is_outgoing: bool,
        is_closed: bool,
        is_hidden: bool,
        is_title_missing: bool,
    ) -> ForumTopicInfo {
        ForumTopicInfo {
            dialog_id,
            forum_topic_id,
            title,
            icon,
            creation_date,
            creator_dialog_id,
            is_outgoing,
            is_closed,
            is_hidden,
            is_title_missing,
        }
    }

    /// `ForumTopicInfo::is_empty` (ForumTopicInfo.h:58-60).
    pub fn is_empty(&self) -> bool {
        !self.forum_topic_id.is_valid()
    }

    pub fn get_dialog_id(&self) -> DialogId {
        self.dialog_id
    }

    pub fn get_forum_topic_id(&self) -> ForumTopicId {
        self.forum_topic_id
    }

    pub fn get_creator_dialog_id(&self) -> DialogId {
        self.creator_dialog_id
    }

    pub fn is_outgoing(&self) -> bool {
        self.is_outgoing
    }

    pub fn is_closed(&self) -> bool {
        self.is_closed
    }

    pub fn is_hidden(&self) -> bool {
        self.is_hidden
    }

    /// `ForumTopicInfo::apply_edited_data` (ForumTopicInfo.cpp:63-92).
    pub fn apply_edited_data(&mut self, edited_data: &ForumTopicEditedData) -> bool {
        let mut is_changed = false;
        if edited_data.edit_icon_custom_emoji_id
            && self
                .icon
                .edit_custom_emoji_id(edited_data.icon_custom_emoji_id)
        {
            is_changed = true;
        }
        if edited_data.edit_is_closed && self.is_closed != edited_data.is_closed {
            self.is_closed = edited_data.is_closed;
            is_changed = true;
        }
        if self.is_hidden != edited_data.is_hidden {
            self.is_hidden = edited_data.is_hidden;
            is_changed = true;
        }
        if !edited_data.title.is_empty() && self.title != edited_data.title {
            self.title = edited_data.title.clone();
            is_changed = true;
        }
        is_changed
    }

    /// `get_forum_topic_info_object` (ForumTopicInfo.cpp:94-101) — the
    /// local scheme carries the chat_id and the forum_topic_id pair.
    pub fn get_forum_topic_info_object(&self) -> td_api::ForumTopicInfo {
        td_api::ForumTopicInfo {
            chat_id: self.dialog_id.get(),
            forum_topic_id: self.forum_topic_id.get(),
            name: self.title.clone(),
            icon: Some(self.icon.get_forum_topic_icon_object()),
            creation_date: self.creation_date,
            creator_id: Some(td_api::MessageSender::messageSenderUser(
                td_api::messageSenderUser {
                    user_id: self.creator_dialog_id.get_user_id().get(),
                },
            )),
            is_general: self.forum_topic_id.get() == 1,
            is_outgoing: self.is_outgoing,
            is_closed: self.is_closed,
            is_hidden: self.is_hidden,
            is_name_implicit: self.is_title_missing,
        }
    }

    /// `store` (ForumTopicInfo.hpp:13-28).
    pub fn store(&self, out: &mut Vec<u8>) {
        store_parse::store_flags(
            out,
            [
                self.is_outgoing,
                self.is_closed,
                self.is_hidden,
                self.is_title_missing,
            ],
        );
        self.dialog_id.store(out);
        self.forum_topic_id.store(out);
        store_parse::store_string(out, &self.title);
        self.icon.store(out);
        store_parse::store_i32(out, self.creation_date);
        self.creator_dialog_id.store(out);
    }

    /// `parse` (ForumTopicInfo.hpp:29-44).
    pub fn parse(parser: &mut Parser) -> std::result::Result<ForumTopicInfo, &'static str> {
        let [is_outgoing, is_closed, is_hidden, is_title_missing] = parser.parse_flags()?;
        Ok(ForumTopicInfo {
            dialog_id: DialogId::parse_from(parser)?,
            forum_topic_id: ForumTopicId::parse(parser)?,
            title: parser.string()?,
            icon: ForumTopicIcon::parse(parser)?,
            creation_date: parser.i32_()?,
            creator_dialog_id: DialogId::parse_from(parser)?,
            is_outgoing,
            is_closed,
            is_hidden,
            is_title_missing,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forum_topic_id_store_parse_round_trip() {
        let value = ForumTopicId::new(42);
        let mut out = Vec::new();
        value.store(&mut out);
        let mut parser = Parser::new(&out);
        assert_eq!(ForumTopicId::parse(&mut parser).unwrap(), value);
        assert!(value.is_valid());
        assert!(!ForumTopicId::default().is_valid());
    }

    #[test]
    fn icon_masks_the_color_and_round_trips() {
        // ForumTopicIcon.cpp:12-14 — the low 24 bits.
        let value = ForumTopicIcon::new(0x1234567, 9);
        assert_eq!(value.get_forum_topic_icon_object().color, 0x234567);
        let mut out = Vec::new();
        value.store(&mut out);
        let mut parser = Parser::new(&out);
        assert_eq!(ForumTopicIcon::parse(&mut parser).unwrap(), value);
    }

    #[test]
    fn edited_data_round_trip_covers_the_flag_shapes() {
        let value = ForumTopicEditedData::get_edited_data(
            true,
            false,
            CustomEmojiId::new(5),
            true,
            Some("New title"),
        );
        let mut out = Vec::new();
        value.store(&mut out);
        let mut parser = Parser::new(&out);
        assert_eq!(ForumTopicEditedData::parse(&mut parser).unwrap(), value);
    }

    #[test]
    fn info_applies_edited_data() {
        let mut info = ForumTopicInfo::new(
            DialogId::new(10),
            ForumTopicId::new(2),
            "Old".to_owned(),
            ForumTopicIcon::default_icon(),
            100,
            DialogId::new(11),
            false,
            false,
            false,
            false,
        );
        let edited = ForumTopicEditedData::get_edited_data(
            true,
            false,
            CustomEmojiId::default(),
            false,
            Some("New"),
        );
        assert!(info.apply_edited_data(&edited));
        assert!(info.is_closed());
        assert_eq!(info.title, "New");
    }

    #[test]
    fn info_store_parse_round_trip() {
        let value = ForumTopicInfo::new(
            DialogId::new(10),
            ForumTopicId::new(2),
            "Topic".to_owned(),
            ForumTopicIcon::new(0xABCDEF, 7),
            1234,
            DialogId::new(11),
            true,
            true,
            false,
            false,
        );
        let mut out = Vec::new();
        value.store(&mut out);
        let mut parser = Parser::new(&out);
        assert_eq!(ForumTopicInfo::parse(&mut parser).unwrap(), value);
    }
}
