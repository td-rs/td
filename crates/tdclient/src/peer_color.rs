//! Port of `td/telegram/PeerColor.{h,cpp}` and
//! `td/telegram/PeerColorCollectible.{h,cpp,hpp}`: the peer accent color
//! pair parsed out of the telegram `peerColor`/`peerColorCollectible`
//! constructors, with the byte-exact database serialization of the
//! collectible. The td_api `upgradedGiftColors` conversion is kept; the
//! `BackgroundInfo`-based parts arrive with the Backgrounds step.

use crate::emoji_status::CustomEmojiId;
use crate::store_parse::{self, Parser};
use crate::td_api;
use tdmtproto::telegram_api;

/// `td::AccentColorId` — the profile/chat accent color wrapper (the port
/// keeps it local until the AccentColors step promotes it).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AccentColorId(i32);

impl Default for AccentColorId {
    /// `int32 id = -1` (AccentColorId.h) — the invalid marker, so
    /// `is_valid()` on a default id is false like the C++.
    fn default() -> Self {
        AccentColorId(-1)
    }
}

impl AccentColorId {
    pub fn new(id: i32) -> AccentColorId {
        AccentColorId(id)
    }

    /// `AccentColorId(UserId)` / `(ChannelId)` — the id modulo the 7
    /// built-in colors.
    pub fn from_peer_id(peer_id: i64) -> AccentColorId {
        AccentColorId((peer_id % 7) as i32)
    }

    pub fn is_valid(self) -> bool {
        self.0 >= 0
    }

    /// `AccentColorId::is_built_in` (AccentColorId.h) — the first 7 ids
    /// are the client-known built-in palette.
    pub fn is_built_in(self) -> bool {
        0 <= self.0 && self.0 < 7
    }

    pub fn get(self) -> i32 {
        self.0
    }
}

/// `td::PeerColor` (PeerColor.h) — the plain (non-collectible) color.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PeerColor {
    accent_color_id: AccentColorId,
    background_custom_emoji_id: CustomEmojiId,
}

impl PeerColor {
    /// `PeerColor::PeerColor(telegram_api::PeerColor)` (PeerColor.cpp).
    pub fn from_telegram(peer_color: &telegram_api::PeerColor) -> PeerColor {
        let telegram_api::PeerColor::peerColor(peer_color) = peer_color else {
            tdutils::log_error!("Receive non-plain peer color");
            return PeerColor::default();
        };
        let mut result = PeerColor::default();
        // peerColor::COLOR_MASK
        if peer_color.flags & 1 != 0 {
            result.accent_color_id = AccentColorId::new(peer_color.color);
            if !result.accent_color_id.is_valid() {
                tdutils::log_error!("Receive invalid peer color");
                result.accent_color_id = AccentColorId::default();
            }
        }
        if peer_color.background_emoji_id != 0 {
            result.background_custom_emoji_id = CustomEmojiId::new(peer_color.background_emoji_id);
            if !result.background_custom_emoji_id.is_valid() {
                tdutils::log_error!("Receive invalid background emoji");
                result.background_custom_emoji_id = CustomEmojiId::default();
            }
        }
        result
    }

    pub fn accent_color_id(&self) -> AccentColorId {
        self.accent_color_id
    }

    pub fn background_custom_emoji_id(&self) -> CustomEmojiId {
        self.background_custom_emoji_id
    }
}

/// `td::PeerColorCollectible`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PeerColorCollectible {
    unique_gift_id: i64,
    gift_custom_emoji_id: CustomEmojiId,
    background_custom_emoji_id: CustomEmojiId,
    light_accent_color: i32,
    light_colors: Vec<i32>,
    dark_accent_color: i32,
    dark_colors: Vec<i32>,
}

impl PeerColorCollectible {
    /// `PeerColorCollectible::PeerColorCollectible(telegram_api::...)`
    /// (PeerColorCollectible.cpp:9-36).
    pub fn from_telegram(peer_color: telegram_api::peerColorCollectible) -> PeerColorCollectible {
        // peerColorCollectible::DARK_ACCENT_COLOR_MASK / DARK_COLORS_MASK
        const DARK_ACCENT_COLOR_MASK: i32 = 1 << 0;
        const DARK_COLORS_MASK: i32 = 1 << 1;

        let mut result = PeerColorCollectible {
            unique_gift_id: peer_color.collectible_id,
            gift_custom_emoji_id: CustomEmojiId::new(peer_color.gift_emoji_id),
            background_custom_emoji_id: CustomEmojiId::new(peer_color.background_emoji_id),
            light_accent_color: peer_color.accent_color,
            light_colors: peer_color.colors,
            dark_accent_color: 0,
            dark_colors: Vec::new(),
        };
        if !result.gift_custom_emoji_id.is_valid() {
            tdutils::log_error!("Receive collectible without a gift emoji");
            result.gift_custom_emoji_id = CustomEmojiId::default();
        }
        if !result.background_custom_emoji_id.is_valid() {
            tdutils::log_error!("Receive collectible without a background emoji");
            result.background_custom_emoji_id = CustomEmojiId::default();
        }
        if result.light_colors.len() > 3 {
            tdutils::log_error!("Receive collectible with too many colors");
            result.light_colors = Vec::new();
        }
        if peer_color.flags & DARK_ACCENT_COLOR_MASK != 0 {
            result.dark_accent_color = peer_color.dark_accent_color;
        } else {
            result.dark_accent_color = result.light_accent_color;
        }
        if peer_color.flags & DARK_COLORS_MASK != 0 {
            result.dark_colors = peer_color.dark_colors;
            if result.dark_colors.len() > 3 {
                tdutils::log_error!("Receive collectible with too many dark colors");
                result.dark_colors = Vec::new();
            }
        } else {
            result.dark_colors = result.light_colors.clone();
        }
        result
    }

    /// `PeerColorCollectible::get_peer_color_collectible`.
    pub fn get_peer_color_collectible(
        peer_color: telegram_api::peerColorCollectible,
    ) -> Option<PeerColorCollectible> {
        let result = PeerColorCollectible::from_telegram(peer_color);
        if !result.is_valid() {
            return None;
        }
        Some(result)
    }

    /// `PeerColorCollectible::is_valid`.
    pub fn is_valid(&self) -> bool {
        !self.light_colors.is_empty() && !self.dark_colors.is_empty()
    }

    /// `PeerColorCollectible::get_upgraded_gift_colors_object`.
    pub fn get_upgraded_gift_colors_object(&self) -> td_api::UpgradedGiftColors {
        td_api::UpgradedGiftColors {
            id: self.unique_gift_id,
            model_custom_emoji_id: self.gift_custom_emoji_id.get(),
            symbol_custom_emoji_id: self.background_custom_emoji_id.get(),
            light_theme_accent_color: self.light_accent_color,
            light_theme_colors: self.light_colors.clone(),
            dark_theme_accent_color: self.dark_accent_color,
            dark_theme_colors: self.dark_colors.clone(),
        }
    }

    /// `PeerColorCollectible::store` (PeerColorCollectible.hpp).
    pub fn store(&self, out: &mut Vec<u8>) {
        use store_parse::*;
        let has_dark_accent_color = self.dark_accent_color != self.light_accent_color;
        let has_dark_colors = self.dark_colors != self.light_colors;
        store_flags(out, [has_dark_accent_color, has_dark_colors]);
        store_i64(out, self.unique_gift_id);
        store_i64(out, self.gift_custom_emoji_id.get());
        store_i64(out, self.background_custom_emoji_id.get());
        store_i32(out, self.light_accent_color);
        store_i32_vec(out, &self.light_colors);
        if has_dark_accent_color {
            store_i32(out, self.dark_accent_color);
        }
        if has_dark_colors {
            store_i32_vec(out, &self.dark_colors);
        }
    }

    /// `PeerColorCollectible::parse` (PeerColorCollectible.hpp).
    pub fn parse(input: &[u8]) -> Result<PeerColorCollectible, &'static str> {
        let mut parser = Parser::new(input);
        let result = PeerColorCollectible::parse_from(&mut parser)?;
        parser.fetch_end()?;
        Ok(result)
    }

    /// The parse over a shared stream (a collectible stored inline inside
    /// the `User` blob).
    pub fn parse_from(parser: &mut Parser) -> Result<PeerColorCollectible, &'static str> {
        let [has_dark_accent_color, has_dark_colors] = parser.parse_flags()?;
        let mut result = PeerColorCollectible {
            unique_gift_id: parser.i64_()?,
            gift_custom_emoji_id: CustomEmojiId::new(parser.i64_()?),
            background_custom_emoji_id: CustomEmojiId::new(parser.i64_()?),
            light_accent_color: parser.i32_()?,
            light_colors: parser.i32_vec()?,
            dark_accent_color: 0,
            dark_colors: Vec::new(),
        };
        if has_dark_accent_color {
            result.dark_accent_color = parser.i32_()?;
        } else {
            result.dark_accent_color = result.light_accent_color;
        }
        if has_dark_colors {
            result.dark_colors = parser.i32_vec()?;
        } else {
            result.dark_colors = result.light_colors.clone();
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collectible() -> telegram_api::peerColorCollectible {
        telegram_api::peerColorCollectible {
            flags: 0,
            collectible_id: 99,
            gift_emoji_id: 111,
            background_emoji_id: 222,
            accent_color: 0xff00ff,
            colors: vec![1, 2, 3],
            dark_accent_color: 0,
            dark_colors: Vec::new(),
        }
    }

    #[test]
    fn plain_peer_color_reads_color_and_emoji() {
        let color = telegram_api::PeerColor::peerColor(telegram_api::peerColor {
            flags: 1,
            color: 3,
            background_emoji_id: 42,
        });
        let parsed = PeerColor::from_telegram(&color);
        assert_eq!(parsed.accent_color_id().get(), 3);
        assert_eq!(parsed.background_custom_emoji_id().get(), 42);
    }

    #[test]
    fn plain_peer_color_defaults_without_flags() {
        let parsed = PeerColor::from_telegram(&telegram_api::PeerColor::peerColor(
            telegram_api::peerColor::default(),
        ));
        assert_eq!(parsed.accent_color_id().get(), -1);
        assert!(!parsed.accent_color_id().is_valid());
    }

    #[test]
    fn collectible_derives_dark_colors_from_light() {
        let result = PeerColorCollectible::from_telegram(collectible());
        assert!(result.is_valid());
        assert_eq!(result.dark_accent_color, 0xff00ff);
        assert_eq!(result.dark_colors, [1, 2, 3]);
    }

    #[test]
    fn collectible_keeps_explicit_dark_colors() {
        let mut raw = collectible();
        raw.flags = 3;
        raw.dark_accent_color = 9;
        raw.dark_colors = vec![7, 8];
        let result = PeerColorCollectible::from_telegram(raw);
        assert_eq!(result.dark_accent_color, 9);
        assert_eq!(result.dark_colors, [7, 8]);
    }

    #[test]
    fn invalid_collectibles_are_rejected() {
        let mut raw = collectible();
        raw.colors = Vec::new();
        assert_eq!(PeerColorCollectible::get_peer_color_collectible(raw), None);
    }

    #[test]
    fn serialization_round_trips_both_shapes() {
        for status in [
            PeerColorCollectible::from_telegram(collectible()),
            PeerColorCollectible::from_telegram(telegram_api::peerColorCollectible {
                flags: 3,
                ..collectible()
            }),
        ] {
            let mut bytes = Vec::new();
            status.store(&mut bytes);
            assert_eq!(PeerColorCollectible::parse(&bytes).unwrap(), status);
        }
    }
}
