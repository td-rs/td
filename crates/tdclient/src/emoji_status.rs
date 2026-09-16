//! Port of `td/telegram/EmojiStatus.{h,cpp}` — the data-plane subset: the
//! constructed-from-telegram object, emptiness, the premium/expiry-aware
//! effective status, and the byte-exact database serialization (the
//! `store`/`parse` pair lives in the C++ header). The td_api object
//! conversion and the recent/default emoji status queries arrive with the
//! StickersManager/ premium steps.

use crate::store_parse::{self, Parser};
use tdmtproto::telegram_api;

/// `td::CustomEmojiId` — the emoji/document identifier wrapper (the port
/// keeps it local until the file-manager step promotes it).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct CustomEmojiId(i64);

impl CustomEmojiId {
    pub fn new(id: i64) -> CustomEmojiId {
        CustomEmojiId(id)
    }

    pub fn is_valid(self) -> bool {
        self.0 != 0
    }

    pub fn get(self) -> i64 {
        self.0
    }
}

/// `td::EmojiStatus`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EmojiStatus {
    custom_emoji_id: CustomEmojiId,

    collectible_id: i64,
    title: String,
    slug: String,
    model_custom_emoji_id: CustomEmojiId,
    pattern_custom_emoji_id: CustomEmojiId,
    center_color: i32,
    edge_color: i32,
    pattern_color: i32,
    text_color: i32,

    until_date: i32,
}

impl EmojiStatus {
    /// `EmojiStatus::EmojiStatus(telegram_api::EmojiStatus)`.
    pub fn from_telegram(emoji_status: telegram_api::EmojiStatus) -> EmojiStatus {
        match emoji_status {
            telegram_api::EmojiStatus::emojiStatusEmpty(_) => EmojiStatus::default(),
            telegram_api::EmojiStatus::emojiStatus(status) => EmojiStatus {
                custom_emoji_id: CustomEmojiId::new(status.document_id),
                until_date: status.until,
                ..EmojiStatus::default()
            },
            telegram_api::EmojiStatus::emojiStatusCollectible(status) => EmojiStatus {
                collectible_id: status.collectible_id,
                title: status.title,
                slug: status.slug,
                model_custom_emoji_id: CustomEmojiId::new(status.document_id),
                pattern_custom_emoji_id: CustomEmojiId::new(status.pattern_document_id),
                center_color: status.center_color,
                edge_color: status.edge_color,
                pattern_color: status.pattern_color,
                text_color: status.text_color,
                ..EmojiStatus::default()
            },
            // inputEmojiStatusCollectible is a request-side constructor
            telegram_api::EmojiStatus::inputEmojiStatusCollectible(_) => EmojiStatus::default(),
        }
    }

    /// `EmojiStatus::get_emoji_status(telegram_api::...)` — the empty
    /// constructor and structurally-empty statuses collapse into `None`.
    pub fn get_emoji_status(emoji_status: telegram_api::EmojiStatus) -> Option<EmojiStatus> {
        let result = EmojiStatus::from_telegram(emoji_status);
        if result.is_empty() {
            return None;
        }
        Some(result)
    }

    pub fn is_empty(&self) -> bool {
        !self.custom_emoji_id.is_valid()
            && (self.collectible_id == 0
                || self.title.is_empty()
                || !self.model_custom_emoji_id.is_valid()
                || !self.pattern_custom_emoji_id.is_valid())
    }

    pub fn get_custom_emoji_id(&self) -> CustomEmojiId {
        self.custom_emoji_id
    }

    pub fn get_until_date(&self) -> i32 {
        self.until_date
    }

    pub fn clear_until_date(&mut self) {
        self.until_date = 0;
    }

    /// `EmojiStatus::get_effective_emoji_status(bool, int32)`.
    pub fn effective(self, is_premium: bool, unix_time: i32) -> EmojiStatus {
        if !is_premium {
            return EmojiStatus::default();
        }
        if self.until_date != 0 && self.until_date <= unix_time {
            return EmojiStatus::default();
        }
        self
    }

    /// `EmojiStatus::get_effective_emoji_status(unique_ptr, bool, int32)`.
    pub fn get_effective_emoji_status(
        emoji_status: &Option<EmojiStatus>,
        is_premium: bool,
        unix_time: i32,
    ) -> Option<EmojiStatus> {
        let emoji_status = emoji_status.as_ref()?;
        Some(emoji_status.clone().effective(is_premium, unix_time))
    }

    /// `EmojiStatus::store` (EmojiStatus.h).
    pub fn store(&self, out: &mut Vec<u8>) {
        use store_parse::*;
        let has_custom_emoji_id = self.custom_emoji_id.is_valid();
        let has_until_date = self.until_date != 0;
        let has_collectible_id = self.collectible_id != 0;
        let has_title = !self.title.is_empty();
        let has_slug = !self.slug.is_empty();
        let has_gift = self.model_custom_emoji_id.is_valid()
            || self.pattern_custom_emoji_id.is_valid()
            || self.center_color != 0
            || self.edge_color != 0
            || self.pattern_color != 0
            || self.text_color != 0;
        store_flags(
            out,
            [
                has_custom_emoji_id,
                has_until_date,
                has_collectible_id,
                has_title,
                has_slug,
                has_gift,
            ],
        );
        if has_custom_emoji_id {
            store_i64(out, self.custom_emoji_id.get());
        }
        if has_until_date {
            store_i32(out, self.until_date);
        }
        if has_collectible_id {
            store_i64(out, self.collectible_id);
        }
        if has_title {
            store_string(out, &self.title);
        }
        if has_slug {
            store_string(out, &self.slug);
        }
        if has_gift {
            store_i64(out, self.model_custom_emoji_id.get());
            store_i64(out, self.pattern_custom_emoji_id.get());
            store_i32(out, self.center_color);
            store_i32(out, self.edge_color);
            store_i32(out, self.pattern_color);
            store_i32(out, self.text_color);
        }
    }

    /// `EmojiStatus::parse` (EmojiStatus.h).
    pub fn parse(input: &[u8]) -> Result<EmojiStatus, &'static str> {
        let mut parser = Parser::new(input);
        let result = EmojiStatus::parse_from(&mut parser)?;
        parser.fetch_end()?;
        Ok(result)
    }

    /// The parse over a shared stream (an EmojiStatus stored inline inside
    /// the `User` blob).
    pub fn parse_from(parser: &mut Parser) -> Result<EmojiStatus, &'static str> {
        let [
            has_custom_emoji_id,
            has_until_date,
            has_collectible_id,
            has_title,
            has_slug,
            has_gift,
        ] = parser.parse_flags()?;
        let mut result = EmojiStatus::default();
        if has_custom_emoji_id {
            result.custom_emoji_id = CustomEmojiId::new(parser.i64_()?);
        }
        if has_until_date {
            result.until_date = parser.i32_()?;
        }
        if has_collectible_id {
            result.collectible_id = parser.i64_()?;
        }
        if has_title {
            result.title = parser.string()?;
        }
        if has_slug {
            result.slug = parser.string()?;
        }
        if has_gift {
            result.model_custom_emoji_id = CustomEmojiId::new(parser.i64_()?);
            result.pattern_custom_emoji_id = CustomEmojiId::new(parser.i64_()?);
            result.center_color = parser.i32_()?;
            result.edge_color = parser.i32_()?;
            result.pattern_color = parser.i32_()?;
            result.text_color = parser.i32_()?;
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(document_id: i64) -> telegram_api::EmojiStatus {
        telegram_api::EmojiStatus::emojiStatus(telegram_api::emojiStatus {
            flags: 0,
            document_id,
            until: 0,
        })
    }

    fn collectible(collectible_id: i64) -> telegram_api::EmojiStatus {
        telegram_api::EmojiStatus::emojiStatusCollectible(telegram_api::emojiStatusCollectible {
            flags: 0,
            collectible_id,
            title: "Model".to_owned(),
            slug: "model".to_owned(),
            document_id: 10,
            pattern_document_id: 20,
            center_color: 1,
            edge_color: 2,
            pattern_color: 3,
            text_color: 4,
            until: 0,
        })
    }

    #[test]
    fn empty_statuses_collapse_to_none() {
        assert_eq!(
            EmojiStatus::get_emoji_status(telegram_api::EmojiStatus::emojiStatusEmpty(
                telegram_api::emojiStatusEmpty
            )),
            None
        );
        assert_eq!(EmojiStatus::get_emoji_status(plain(0)), None);
    }

    #[test]
    fn plain_status_is_parsed_and_effective_gates_expiry() {
        let status = EmojiStatus::get_emoji_status(plain(1234567)).unwrap();
        assert_eq!(status.get_custom_emoji_id().get(), 1234567);
        assert_eq!(status.get_until_date(), 0);
        assert_eq!(
            status
                .clone()
                .effective(false, 100)
                .get_custom_emoji_id()
                .get(),
            0
        );

        let expiring = EmojiStatus {
            custom_emoji_id: CustomEmojiId::new(5),
            until_date: 50,
            ..EmojiStatus::default()
        };
        assert_eq!(
            EmojiStatus::get_effective_emoji_status(&Some(expiring.clone()), true, 40)
                .unwrap()
                .get_custom_emoji_id()
                .get(),
            5
        );
        // Expired for a premium user returns the EMPTY status value (the
        // C++ returns a pointer to a default-constructed EmojiStatus, not
        // nullptr — only a null input gives null).
        let expired = EmojiStatus::get_effective_emoji_status(&Some(expiring), true, 60).unwrap();
        assert_eq!(expired.get_custom_emoji_id().get(), 0);
        assert!(expired.is_empty());
    }

    #[test]
    fn collectible_fields_are_kept() {
        let status = EmojiStatus::from_telegram(collectible(77));
        assert!(!status.is_empty());
        assert_eq!(status.title, "Model");
        assert_eq!(status.model_custom_emoji_id.get(), 10);
        assert_eq!(status.pattern_custom_emoji_id.get(), 20);
        assert_eq!(status.center_color, 1);
        assert_eq!(status.text_color, 4);
    }

    #[test]
    fn serialization_round_trips() {
        for status in [
            EmojiStatus::get_emoji_status(plain(42)).unwrap(),
            EmojiStatus {
                custom_emoji_id: CustomEmojiId::new(7),
                until_date: 999,
                ..EmojiStatus::default()
            },
            EmojiStatus::from_telegram(collectible(1)),
        ] {
            let mut bytes = Vec::new();
            status.store(&mut bytes);
            assert_eq!(EmojiStatus::parse(&bytes).unwrap(), status);
        }
    }
}
