//! Port of `td/telegram/EmojiStatus.{h,cpp}` — the data-plane subset: the
//! constructed-from-telegram object, emptiness, the premium/expiry-aware
//! effective status, and the byte-exact database serialization (the
//! `store`/`parse` pair lives in the C++ header). The td_api object
//! conversion and the recent/default emoji status queries arrive with the
//! StickersManager/ premium steps.

use crate::store_parse::{self, Parser};
use crate::td_api;
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

    /// `EmojiStatus::EmojiStatus(td_api::object_ptr<td_api::emojiStatus>)`
    /// and `get_emoji_status(td_api::...)` (EmojiStatus.cpp:26-64): the
    /// null and structurally-empty statuses collapse into `None`; an
    /// already expired `expiration_date` empties the status (the C++ ctor
    /// reads `G()->unix_time()` — the port threads the clock in).
    pub fn get_emoji_status_from_td_api(
        emoji_status: Option<&td_api::EmojiStatus>,
        unix_time: i32,
    ) -> Option<EmojiStatus> {
        let emoji_status = emoji_status?;
        let mut result = EmojiStatus::default();
        if emoji_status.expiration_date != 0 {
            if emoji_status.expiration_date <= unix_time {
                return None;
            }
            result.until_date = emoji_status.expiration_date;
        }
        match emoji_status.r#type.as_ref() {
            Some(td_api::EmojiStatusType::emojiStatusTypeCustomEmoji(type_)) => {
                result.custom_emoji_id = CustomEmojiId::new(type_.custom_emoji_id);
            }
            Some(td_api::EmojiStatusType::emojiStatusTypeUpgradedGift(type_)) => {
                result.collectible_id = type_.upgraded_gift_id;
                result.title = type_.gift_title.clone();
                result.slug = type_.gift_name.clone();
                result.model_custom_emoji_id = CustomEmojiId::new(type_.model_custom_emoji_id);
                result.pattern_custom_emoji_id = CustomEmojiId::new(type_.symbol_custom_emoji_id);
                if let Some(colors) = type_.backdrop_colors.as_ref() {
                    result.center_color = colors.center_color;
                    result.edge_color = colors.edge_color;
                    result.pattern_color = colors.symbol_color;
                    result.text_color = colors.text_color;
                }
            }
            None => {}
        }
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

    /// `EmojiStatus::get_emoji_status_object` (EmojiStatus.cpp:488-503) —
    /// the empty status is the C++ `nullptr` (the callers that move the
    /// object into a vector drop the empty entry, mirroring the C++
    /// null-element shape the flattened model cannot carry).
    pub fn get_emoji_status_object(&self) -> Option<td_api::EmojiStatus> {
        if self.is_empty() {
            return None;
        }
        let r#type = if self.custom_emoji_id.is_valid() {
            td_api::EmojiStatusType::emojiStatusTypeCustomEmoji(
                td_api::emojiStatusTypeCustomEmoji {
                    custom_emoji_id: self.custom_emoji_id.get(),
                },
            )
        } else {
            td_api::EmojiStatusType::emojiStatusTypeUpgradedGift(
                td_api::emojiStatusTypeUpgradedGift {
                    upgraded_gift_id: self.collectible_id,
                    gift_title: self.title.clone(),
                    gift_name: self.slug.clone(),
                    model_custom_emoji_id: self.model_custom_emoji_id.get(),
                    symbol_custom_emoji_id: self.pattern_custom_emoji_id.get(),
                    backdrop_colors: Some(td_api::UpgradedGiftBackdropColors {
                        center_color: self.center_color,
                        edge_color: self.edge_color,
                        symbol_color: self.pattern_color,
                        text_color: self.text_color,
                    }),
                },
            )
        };
        Some(td_api::EmojiStatus {
            r#type: Some(r#type),
            expiration_date: self.until_date,
        })
    }
}

/// `EmojiStatusCustomEmojis` (EmojiStatus.cpp:24-55) — the binlog-pmc
/// cache behind the default-emoji-status lists: the server hash plus the
/// collected custom emoji ids. The server-answer constructor (the C++
/// `telegram_api::account_emojiStatuses` intake) lands with the net
/// queries.
#[derive(Clone, Debug, Default)]
pub struct EmojiStatusCustomEmojisCache {
    pub hash: i64,
    pub custom_emoji_ids: Vec<CustomEmojiId>,
}

impl EmojiStatusCustomEmojisCache {
    /// `get_emoji_status_custom_emojis_object`
    /// (EmojiStatus.cpp:565-569).
    pub fn get_emoji_status_custom_emojis_object(&self) -> td_api::EmojiStatusCustomEmojis {
        td_api::EmojiStatusCustomEmojis {
            custom_emoji_ids: self.custom_emoji_ids.iter().map(|id| id.get()).collect(),
        }
    }

    /// `EmojiStatusCustomEmojis::store` (EmojiStatus.cpp:44-48).
    pub fn store(&self, out: &mut Vec<u8>) {
        use store_parse::*;
        store_i64(out, self.hash);
        store_i64_vec(
            out,
            &self
                .custom_emoji_ids
                .iter()
                .map(|id| id.get())
                .collect::<Vec<_>>(),
        );
    }

    /// `EmojiStatusCustomEmojis::parse` (EmojiStatus.cpp:50-54).
    pub fn parse(input: &[u8]) -> Result<EmojiStatusCustomEmojisCache, &'static str> {
        use store_parse::*;
        let mut parser = Parser::new(input);
        let hash = parser.i64_()?;
        let count = parser.i32_()?;
        let mut custom_emoji_ids = Vec::with_capacity(count.max(0) as usize);
        for _ in 0..count {
            custom_emoji_ids.push(CustomEmojiId::new(parser.i64_()?));
        }
        parser.fetch_end()?;
        Ok(EmojiStatusCustomEmojisCache {
            hash,
            custom_emoji_ids,
        })
    }
}

/// `EmojiStatuses` (EmojiStatus.cpp:68-106) — the binlog-pmc cache behind
/// the recent/collectible emoji-status lists.
#[derive(Clone, Debug, Default)]
pub struct EmojiStatusesCache {
    pub hash: i64,
    pub emoji_statuses: Vec<EmojiStatus>,
}

impl EmojiStatusesCache {
    /// `get_emoji_statuses_object` (EmojiStatus.cpp:72-75); empty
    /// statuses are skipped (see
    /// [`EmojiStatus::get_emoji_status_object`](EmojiStatus::get_emoji_status_object)).
    pub fn get_emoji_statuses_object(&self) -> td_api::EmojiStatuses {
        td_api::EmojiStatuses {
            emoji_statuses: self
                .emoji_statuses
                .iter()
                .filter_map(EmojiStatus::get_emoji_status_object)
                .collect(),
        }
    }

    /// `EmojiStatuses::store` (EmojiStatus.cpp:95-99).
    pub fn store(&self, out: &mut Vec<u8>) {
        use store_parse::*;
        store_i64(out, self.hash);
        store_i32(out, self.emoji_statuses.len() as i32);
        for emoji_status in &self.emoji_statuses {
            emoji_status.store(out);
        }
    }

    /// `EmojiStatuses::parse` (EmojiStatus.cpp:101-105).
    pub fn parse(input: &[u8]) -> Result<EmojiStatusesCache, &'static str> {
        use store_parse::*;
        let mut parser = Parser::new(input);
        let hash = parser.i64_()?;
        let count = parser.i32_()?;
        let mut emoji_statuses = Vec::with_capacity(count.max(0) as usize);
        for _ in 0..count {
            emoji_statuses.push(EmojiStatus::parse_from(&mut parser)?);
        }
        parser.fetch_end()?;
        Ok(EmojiStatusesCache {
            hash,
            emoji_statuses,
        })
    }
}

/// `get_default_emoji_statuses_database_key` (EmojiStatus.cpp:109-112).
pub const DEFAULT_EMOJI_STATUSES_DATABASE_KEY: &[u8] = b"def_emoji_statuses";

/// `get_default_channel_emoji_statuses_database_key`
/// (EmojiStatus.cpp:114-117).
pub const DEFAULT_CHANNEL_EMOJI_STATUSES_DATABASE_KEY: &[u8] = b"def_ch_emoji_statuses";

/// `get_recent_emoji_statuses_database_key` (EmojiStatus.cpp:119-122).
pub const RECENT_EMOJI_STATUSES_DATABASE_KEY: &[u8] = b"rec_emoji_statuses";

/// `get_upgraded_gift_emoji_statuses_database_key`
/// (EmojiStatus.cpp:124-127).
pub const UPGRADED_GIFT_EMOJI_STATUSES_DATABASE_KEY: &[u8] = b"nft_emoji_statuses";

/// `load_emoji_status_custom_emojis` (EmojiStatus.cpp:143-156): an empty
/// or unparsable value is the `hash = -1` "no cache" state.
pub(crate) fn load_emoji_status_custom_emojis(
    binlog_pmc: &mut tddb::binlog_key_value::BinlogKeyValue,
    key: &[u8],
) -> EmojiStatusCustomEmojisCache {
    let value = binlog_pmc.get_raw(key);
    if value.is_empty() {
        return EmojiStatusCustomEmojisCache {
            hash: -1,
            custom_emoji_ids: Vec::new(),
        };
    }
    EmojiStatusCustomEmojisCache::parse(&value).unwrap_or(EmojiStatusCustomEmojisCache {
        hash: -1,
        custom_emoji_ids: Vec::new(),
    })
}

/// `load_emoji_statuses` (EmojiStatus.cpp:129-142).
pub(crate) fn load_emoji_statuses(
    binlog_pmc: &mut tddb::binlog_key_value::BinlogKeyValue,
    key: &[u8],
) -> EmojiStatusesCache {
    let value = binlog_pmc.get_raw(key);
    if value.is_empty() {
        return EmojiStatusesCache {
            hash: -1,
            emoji_statuses: Vec::new(),
        };
    }
    EmojiStatusesCache::parse(&value).unwrap_or(EmojiStatusesCache {
        hash: -1,
        emoji_statuses: Vec::new(),
    })
}

/// `save_emoji_statuses`/`save_emoji_status_custom_emojis`
/// (EmojiStatus.cpp:157-164).
pub(crate) fn save_emoji_statuses_cache(
    binlog_pmc: &mut tddb::binlog_key_value::BinlogKeyValue,
    key: &[u8],
    value: &[u8],
) {
    binlog_pmc.set_raw(key, value);
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

    #[test]
    fn emoji_status_object_builder_matches_the_td_api_shape() {
        // the plain status
        let status = EmojiStatus {
            custom_emoji_id: CustomEmojiId::new(42),
            until_date: 1000,
            ..EmojiStatus::default()
        };
        match status.get_emoji_status_object() {
            Some(td_api::EmojiStatus {
                r#type: Some(td_api::EmojiStatusType::emojiStatusTypeCustomEmoji(type_)),
                expiration_date,
            }) => {
                assert_eq!(type_.custom_emoji_id, 42);
                assert_eq!(expiration_date, 1000);
            }
            other => panic!("wrong plain object shape: {other:?}"),
        }

        // the collectible status
        let collectible_status = EmojiStatus::from_telegram(collectible(1));
        match collectible_status.get_emoji_status_object() {
            Some(td_api::EmojiStatus {
                r#type: Some(td_api::EmojiStatusType::emojiStatusTypeUpgradedGift(type_)),
                expiration_date,
            }) => {
                assert_eq!(type_.upgraded_gift_id, 1);
                assert_eq!(type_.gift_title, "Model");
                assert_eq!(type_.model_custom_emoji_id, 10);
                assert_eq!(type_.symbol_custom_emoji_id, 20);
                let backdrop = type_
                    .backdrop_colors
                    .expect("the collectible carries its colors");
                assert_eq!(backdrop.center_color, 1);
                assert_eq!(expiration_date, 0);
            }
            other => panic!("wrong collectible object shape: {other:?}"),
        }

        // the empty status is the C++ nullptr
        assert!(EmojiStatus::default().get_emoji_status_object().is_none());
    }

    #[test]
    fn emoji_status_cache_round_trips_and_preserves_the_hash() {
        let custom = EmojiStatusCustomEmojisCache {
            hash: -77,
            custom_emoji_ids: vec![
                CustomEmojiId::new(1),
                CustomEmojiId::new(2),
                CustomEmojiId::new(3),
            ],
        };
        let mut bytes = Vec::new();
        custom.store(&mut bytes);
        let parsed = EmojiStatusCustomEmojisCache::parse(&bytes).unwrap();
        assert_eq!(parsed.hash, -77);
        assert_eq!(
            parsed
                .get_emoji_status_custom_emojis_object()
                .custom_emoji_ids,
            vec![1, 2, 3]
        );

        let statuses = EmojiStatusesCache {
            hash: 19,
            emoji_statuses: vec![
                EmojiStatus {
                    custom_emoji_id: CustomEmojiId::new(9),
                    until_date: 55,
                    ..EmojiStatus::default()
                },
                EmojiStatus::from_telegram(collectible(2)),
            ],
        };
        let mut bytes = Vec::new();
        statuses.store(&mut bytes);
        let parsed = EmojiStatusesCache::parse(&bytes).unwrap();
        assert_eq!(parsed.hash, 19);
        assert_eq!(parsed.emoji_statuses, statuses.emoji_statuses);
        assert_eq!(parsed.get_emoji_statuses_object().emoji_statuses.len(), 2);

        // the unparsable framing is the "no cache" state at the load layer
        let empty = EmojiStatusCustomEmojisCache::parse(&[0u8; 3]);
        assert!(empty.is_err());
    }
}
