//! `td::StoryForwardInfo` (StoryForwardInfo.h/.cpp/.hpp, M.47).

use crate::dialog_id::DialogId;
use crate::store_parse::{self, Parser};
use crate::story_ids::{StoryFullId, StoryId, dialog_id_from_peer};
use crate::story_manager::StoryManagerEnv;
use crate::td_api;
use tdmtproto::telegram_api;

/// The `storyFwdHeader::FROM_NAME_MASK` bit (telegram_api.tl:1807).
const STORY_FWD_HEADER_FROM_NAME_MASK: i32 = 1 << 1;

/// `td::StoryForwardInfo` (StoryForwardInfo.h:23-58).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StoryForwardInfo {
    dialog_id: DialogId,
    story_id: StoryId,
    sender_name: String,
    is_modified: bool,
}

impl StoryForwardInfo {
    /// `StoryForwardInfo(StoryFullId, bool)` (StoryForwardInfo.h:37-40).
    pub fn from_story_full_id(story_full_id: StoryFullId, is_modified: bool) -> StoryForwardInfo {
        StoryForwardInfo {
            dialog_id: story_full_id.get_dialog_id(),
            story_id: story_full_id.get_story_id(),
            sender_name: String::new(),
            is_modified,
        }
    }

    /// `StoryForwardInfo(Td *, storyFwdHeader)` (StoryForwardInfo.cpp:13-37).
    pub fn from_fwd_header(
        env: &mut impl StoryManagerEnv,
        fwd_header: telegram_api::StoryFwdHeader,
    ) -> StoryForwardInfo {
        let mut result = StoryForwardInfo {
            is_modified: fwd_header.modified,
            ..StoryForwardInfo::default()
        };
        if let Some(from) = &fwd_header.from {
            result.dialog_id = dialog_id_from_peer(Some(from));
            result.story_id = StoryId::new(fwd_header.story_id);
            if !result.dialog_id.is_valid() || !result.story_id.is_server() {
                tdutils::log_error!("Receive storyFwdHeader");
                result.dialog_id = DialogId::default();
                result.story_id = StoryId::default();
            } else {
                env.force_create_dialog(result.dialog_id, "StoryForwardInfo", true);
            }
        } else if fwd_header.flags & STORY_FWD_HEADER_FROM_NAME_MASK != 0 {
            if fwd_header.story_id != 0 {
                tdutils::log_error!("Receive storyFwdHeader");
            }
            result.sender_name = fwd_header.from_name;
        } else {
            tdutils::log_error!("Receive storyFwdHeader");
        }
        result
    }

    /// `StoryForwardInfo::hide_sender_if_needed` (StoryForwardInfo.cpp:39-41)
    /// — currently a no-op.
    pub fn hide_sender_if_needed(&mut self, _env: &mut impl StoryManagerEnv) {}

    /// `StoryForwardInfo::add_dependencies` (StoryForwardInfo.cpp:43-45) —
    /// the Dependencies collapse is the manager's force-create seam; the
    /// original story is deliberately not loaded.
    pub fn add_dependencies(&self, env: &mut impl StoryManagerEnv) {
        if self.dialog_id.is_valid() {
            env.force_create_dialog(self.dialog_id, "StoryForwardInfo", true);
        }
    }

    /// `StoryForwardInfo::get_story_repost_info_object`
    /// (StoryForwardInfo.cpp:47-56).
    pub fn get_story_repost_info_object(
        &self,
        env: &mut impl StoryManagerEnv,
    ) -> td_api::StoryRepostInfo {
        let origin = if self.dialog_id.is_valid() && self.story_id.is_valid() {
            td_api::StoryOrigin::storyOriginPublicStory(td_api::storyOriginPublicStory {
                chat_id: env.get_chat_id_object(self.dialog_id, "storyOriginPublicStory"),
                story_id: self.story_id.get(),
            })
        } else {
            td_api::StoryOrigin::storyOriginHiddenUser(td_api::storyOriginHiddenUser {
                poster_name: self.sender_name.clone(),
            })
        };
        td_api::StoryRepostInfo {
            origin: Some(origin),
            is_content_modified: self.is_modified,
        }
    }

    /// `StoryForwardInfo::store` (StoryForwardInfo.hpp:14-38).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_dialog_id = self.dialog_id.is_valid();
        let has_story_id = self.story_id.is_valid();
        let has_sender_name = !self.sender_name.is_empty();
        store_parse::store_flags(
            out,
            [
                has_dialog_id,
                has_story_id,
                has_sender_name,
                self.is_modified,
            ],
        );
        if has_dialog_id {
            self.dialog_id.store(out);
        }
        if has_story_id {
            self.story_id.store(out);
        }
        if has_sender_name {
            store_parse::store_string(out, &self.sender_name);
        }
    }

    /// `StoryForwardInfo::parse` (StoryForwardInfo.hpp:40-62).
    pub fn parse(parser: &mut Parser) -> std::result::Result<StoryForwardInfo, &'static str> {
        let [has_dialog_id, has_story_id, has_sender_name, is_modified] = parser.parse_flags()?;
        let mut result = StoryForwardInfo {
            is_modified,
            ..StoryForwardInfo::default()
        };
        if has_dialog_id {
            result.dialog_id = DialogId::parse_from(parser)?;
        }
        if has_story_id {
            result.story_id = StoryId::parse(parser)?;
        }
        if has_sender_name {
            result.sender_name = parser.string()?;
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_story_full_id_sets_dialog_and_story() {
        let full_id = StoryFullId::new(
            DialogId::from_user_id(crate::user_manager::UserId::new(7)),
            StoryId::new(42),
        );
        let info = StoryForwardInfo::from_story_full_id(full_id, true);
        assert_eq!(info.dialog_id, full_id.get_dialog_id());
        assert_eq!(info.story_id, full_id.get_story_id());
        assert!(info.is_modified);
        assert!(info.sender_name.is_empty());
    }

    #[test]
    fn store_parse_round_trip_covers_every_flag_shape() {
        // StoryForwardInfo.hpp:14-62 — dialog/story/sender-name flags.
        let value = StoryForwardInfo {
            dialog_id: DialogId::from_user_id(crate::user_manager::UserId::new(11)),
            story_id: StoryId::new(123),
            sender_name: "A forwarded sender".to_owned(),
            is_modified: true,
        };
        let mut out = Vec::new();
        value.store(&mut out);
        let mut parser = Parser::new(&out);
        let parsed = StoryForwardInfo::parse(&mut parser).expect("parse");
        assert_eq!(parsed, value);

        let value = StoryForwardInfo {
            dialog_id: DialogId::default(),
            story_id: StoryId::default(),
            sender_name: String::new(),
            is_modified: false,
        };
        let mut out = Vec::new();
        value.store(&mut out);
        let mut parser = Parser::new(&out);
        let parsed = StoryForwardInfo::parse(&mut parser).expect("parse");
        assert_eq!(parsed, value);
    }
}
