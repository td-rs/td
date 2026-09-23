//! `td::BusinessIntro` (BusinessIntro.h/.cpp/.hpp, M.48).
//!
//! The start-page record: title/description + the sticker FileId. The
//! sticker half rides the env seams — `intro_get_input_file_id` and the
//! store/parse sticker round-trip are [blocked: the sticker plane]
//! (the StickersManager halves land with the sticker track), while the
//! C++ clean_input_string title/description pass is byte-faithful.

use crate::business_manager::BusinessEnv;
use crate::files::{FileId, FileType};
use crate::store_parse::{self, Parser};
use crate::td_api;
use tdmtproto::telegram_api;

/// The `inputBusinessIntro.sticker` bit (telegram_api.tl:1613).
const INPUT_BUSINESS_INTRO_STICKER_MASK: i32 = 1;

/// `td::BusinessIntro` (BusinessIntro.h:12-48).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BusinessIntro {
    title: String,
    description: String,
    sticker_file_id: FileId,
}

impl BusinessIntro {
    /// `BusinessIntro(businessIntro)` (BusinessIntro.cpp:13-26) — the
    /// sticker registration rides the env seam.
    pub fn from_telegram(
        env: &mut impl BusinessEnv,
        intro: Option<telegram_api::BusinessIntro>,
    ) -> BusinessIntro {
        let mut result = BusinessIntro::default();
        let Some(intro) = intro else {
            return result;
        };
        let mut title = intro.title;
        if crate::misc::clean_input_string(&title).is_none() {
            title.clear();
        }
        let mut description = intro.description;
        if crate::misc::clean_input_string(&description).is_none() {
            description.clear();
        }
        result.title = title;
        result.description = description;
        result.sticker_file_id = env.on_get_sticker_document(intro.sticker, "BusinessIntro");
        result
    }

    /// `BusinessIntro(inputBusinessStartPage)` (BusinessIntro.cpp:28-44) —
    /// the file-id validation rides the env seams (the file-view checks
    /// are [blocked: Files] and the custom-emoji check is [blocked: the
    /// sticker plane]).
    pub fn from_td_api(
        env: &mut impl BusinessEnv,
        intro: Option<td_api::InputBusinessStartPage>,
    ) -> BusinessIntro {
        let mut result = BusinessIntro::default();
        let Some(intro) = intro else {
            return result;
        };
        result.title = intro.title;
        result.description = intro.message;
        result.sticker_file_id = env.intro_get_input_file_id(intro.sticker);
        result
    }

    /// `get_business_start_page_object` (BusinessIntro.cpp:46-50); `None`
    /// for the C++ nullptr.
    pub fn get_business_start_page_object(
        &self,
        env: &mut impl BusinessEnv,
    ) -> Option<td_api::BusinessStartPage> {
        if self.is_empty() {
            return None;
        }
        Some(td_api::BusinessStartPage {
            title: self.title.clone(),
            message: self.description.clone(),
            sticker: env.get_sticker_object(self.sticker_file_id),
        })
    }

    /// `get_input_business_intro` (BusinessIntro.cpp:52-62) — the sticker
    /// input-document rides the env seam.
    pub fn get_input_business_intro(
        &self,
        env: &mut impl BusinessEnv,
    ) -> telegram_api::InputBusinessIntro {
        let (flags, input_document) = if self.sticker_file_id.is_valid() {
            (
                INPUT_BUSINESS_INTRO_STICKER_MASK,
                env.intro_get_input_document(self.sticker_file_id),
            )
        } else {
            (0, None)
        };
        telegram_api::InputBusinessIntro {
            flags,
            title: self.title.clone(),
            description: self.description.clone(),
            sticker: input_document,
        }
    }

    /// `BusinessIntro::is_empty` (BusinessIntro.h:23-25).
    pub fn is_empty(&self) -> bool {
        self.title.is_empty() && self.description.is_empty() && !self.sticker_file_id.is_valid()
    }

    /// `get_file_ids` (BusinessIntro.cpp:64-70) — the sticker document's
    /// file ids; [blocked: Files] answers an empty set.
    pub fn get_file_ids(&self, env: &mut impl BusinessEnv) -> Vec<FileId> {
        if !self.sticker_file_id.is_valid() {
            return Vec::new();
        }
        env.intro_get_sticker_file_ids(self.sticker_file_id)
    }

    /// `store` (BusinessIntro.hpp:13-33) — the sticker round-trip rides
    /// the env seam.
    pub fn store(&self, env: &mut impl BusinessEnv, out: &mut Vec<u8>) {
        let has_title = !self.title.is_empty();
        let has_description = !self.description.is_empty();
        let has_sticker_file_id = self.sticker_file_id.is_valid();
        store_parse::store_flags(out, [has_title, has_description, has_sticker_file_id]);
        if has_title {
            store_parse::store_string(out, &self.title);
        }
        if has_description {
            store_parse::store_string(out, &self.description);
        }
        if has_sticker_file_id {
            env.intro_store_sticker(self.sticker_file_id, out);
        }
    }

    /// `parse` (BusinessIntro.hpp:34-54).
    pub fn parse(
        env: &mut impl BusinessEnv,
        parser: &mut Parser,
    ) -> std::result::Result<BusinessIntro, &'static str> {
        let [has_title, has_description, has_sticker_file_id] = parser.parse_flags()?;
        let mut result = BusinessIntro::default();
        if has_title {
            result.title = parser.string()?;
        }
        if has_description {
            result.description = parser.string()?;
        }
        if has_sticker_file_id {
            result.sticker_file_id = env.intro_parse_sticker(parser)?;
        }
        Ok(result)
    }

    /// The default FileType for the intro sticker input.
    pub const STICKER_FILE_TYPE: FileType = FileType::Sticker;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_intro() {
        let intro = BusinessIntro::default();
        assert!(intro.is_empty());
        assert_eq!(
            intro.title, "",
            "the C++ default title/description are empty"
        );
    }

    #[test]
    fn from_td_api_takes_title_and_message() {
        struct NoStickerEnv;
        impl BusinessEnv for NoStickerEnv {
            fn is_bot(&self) -> bool {
                false
            }
            fn get_time_zone_offset(&mut self, _time_zone_id: &str) -> i32 {
                0
            }
            fn get_option_integer(&mut self, _name: &str, default_value: i64) -> i64 {
                default_value
            }
            fn on_get_sticker_document(
                &mut self,
                _document: Option<telegram_api::Document>,
                _source: &str,
            ) -> FileId {
                FileId::default()
            }
            fn intro_get_input_file_id(&mut self, _file: Option<td_api::InputFile>) -> FileId {
                FileId::default()
            }
        }

        let mut env = NoStickerEnv;
        let intro = BusinessIntro::from_td_api(
            &mut env,
            Some(td_api::InputBusinessStartPage {
                title: "Shop".to_owned(),
                message: "Welcome".to_owned(),
                sticker: None,
            }),
        );
        assert_eq!(intro.title, "Shop");
        assert_eq!(intro.description, "Welcome");
        assert!(!intro.is_empty());
    }
}
