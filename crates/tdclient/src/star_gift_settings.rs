//! Port of `td/telegram/StarGiftSettings.{h,cpp}` and
//! `td/telegram/DisallowedGiftsSettings.{h,cpp}` — the `setGiftSettings`
//! slice. The value surface is complete: the td_api intakes, the
//! `allow_nothing` factory, the td_api/telegram builders, the byte-exact
//! database serialization (the two `*Settings.hpp` flag blocks) and the
//! `StringBuilder` debug forms.

use crate::store_parse::{self, Parser};
use crate::td_api;
use tdmtproto::telegram_api;

/// `td::DisallowedGiftsSettings` (DisallowedGiftsSettings.h).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DisallowedGiftsSettings {
    disallow_unlimited_stargifts: bool,
    disallow_limited_stargifts: bool,
    disallow_unique_stargifts: bool,
    disallow_premium_gifts: bool,
    disallow_gifts_from_channels: bool,
}

impl DisallowedGiftsSettings {
    /// `DisallowedGiftsSettings(telegram_api::disallowedGiftsSettings *)`
    /// (DisallowedGiftsSettings.cpp:11-20).
    fn from_telegram(settings: Option<telegram_api::DisallowedGiftsSettings>) -> Self {
        let mut result = DisallowedGiftsSettings::default();
        if let Some(settings) = settings {
            result.disallow_unlimited_stargifts = settings.disallow_unlimited_stargifts;
            result.disallow_limited_stargifts = settings.disallow_limited_stargifts;
            result.disallow_unique_stargifts = settings.disallow_unique_stargifts;
            result.disallow_premium_gifts = settings.disallow_premium_gifts;
            result.disallow_gifts_from_channels = settings.disallow_stargifts_from_channels;
        }
        result
    }

    /// `DisallowedGiftsSettings(td_api::acceptedGiftTypes *)`
    /// (DisallowedGiftsSettings.cpp:22-30) — the accepted types invert
    /// into the disallowed flags.
    pub fn from_accepted_gift_types(types: Option<&td_api::AcceptedGiftTypes>) -> Self {
        let mut result = DisallowedGiftsSettings::default();
        if let Some(types) = types {
            result.disallow_unlimited_stargifts = !types.unlimited_gifts;
            result.disallow_limited_stargifts = !types.limited_gifts;
            result.disallow_unique_stargifts = !types.upgraded_gifts;
            result.disallow_premium_gifts = !types.premium_subscription;
            result.disallow_gifts_from_channels = !types.gifts_from_channels;
        }
        result
    }

    /// `DisallowedGiftsSettings::allow_nothing`
    /// (DisallowedGiftsSettings.cpp:32-40).
    pub fn allow_nothing() -> Self {
        DisallowedGiftsSettings {
            disallow_unlimited_stargifts: true,
            disallow_limited_stargifts: true,
            disallow_unique_stargifts: true,
            disallow_premium_gifts: true,
            disallow_gifts_from_channels: true,
        }
    }

    /// `DisallowedGiftsSettings::is_default` (DisallowedGiftsSettings.h:41).
    pub fn is_default(&self) -> bool {
        !self.disallow_unlimited_stargifts
            && !self.disallow_limited_stargifts
            && !self.disallow_unique_stargifts
            && !self.disallow_premium_gifts
            && !self.disallow_gifts_from_channels
    }

    /// `DisallowedGiftsSettings::get_accepted_gift_types_object`
    /// (DisallowedGiftsSettings.cpp:42-46).
    pub fn get_accepted_gift_types_object(&self) -> td_api::AcceptedGiftTypes {
        td_api::AcceptedGiftTypes {
            unlimited_gifts: !self.disallow_unlimited_stargifts,
            limited_gifts: !self.disallow_limited_stargifts,
            upgraded_gifts: !self.disallow_unique_stargifts,
            gifts_from_channels: !self.disallow_gifts_from_channels,
            premium_subscription: !self.disallow_premium_gifts,
        }
    }

    /// `DisallowedGiftsSettings::get_input_disallowed_gifts_settings`
    /// (DisallowedGiftsSettings.cpp:48-56) — `None` is the C++ `nullptr`
    /// for the default settings.
    pub fn get_input_disallowed_gifts_settings(
        &self,
    ) -> Option<telegram_api::DisallowedGiftsSettings> {
        if self.is_default() {
            return None;
        }
        Some(telegram_api::DisallowedGiftsSettings {
            flags: 0,
            disallow_unlimited_stargifts: self.disallow_unlimited_stargifts,
            disallow_limited_stargifts: self.disallow_limited_stargifts,
            disallow_unique_stargifts: self.disallow_unique_stargifts,
            disallow_premium_gifts: self.disallow_premium_gifts,
            disallow_stargifts_from_channels: self.disallow_gifts_from_channels,
        })
    }

    /// `DisallowedGiftsSettings::store` (DisallowedGiftsSettings.hpp) —
    /// the single 5-flag block.
    pub fn store(&self, out: &mut Vec<u8>) {
        store_parse::store_flags(
            out,
            [
                self.disallow_unlimited_stargifts,
                self.disallow_limited_stargifts,
                self.disallow_unique_stargifts,
                self.disallow_premium_gifts,
                self.disallow_gifts_from_channels,
            ],
        );
    }

    /// `DisallowedGiftsSettings::parse`.
    pub fn parse(input: &[u8]) -> Result<DisallowedGiftsSettings, &'static str> {
        let mut parser = Parser::new(input);
        let result = DisallowedGiftsSettings::parse_from(&mut parser)?;
        parser.fetch_end()?;
        Ok(result)
    }

    /// The parse over a shared stream (a `DisallowedGiftsSettings` stored
    /// inline inside the `StarGiftSettings` blob).
    pub fn parse_from(parser: &mut Parser) -> Result<DisallowedGiftsSettings, &'static str> {
        let [
            disallow_unlimited_stargifts,
            disallow_limited_stargifts,
            disallow_unique_stargifts,
            disallow_premium_gifts,
            disallow_gifts_from_channels,
        ] = parser.parse_flags()?;
        Ok(DisallowedGiftsSettings {
            disallow_unlimited_stargifts,
            disallow_limited_stargifts,
            disallow_unique_stargifts,
            disallow_premium_gifts,
            disallow_gifts_from_channels,
        })
    }
}

/// `operator<<(StringBuilder, DisallowedGiftsSettings)`
/// (DisallowedGiftsSettings.cpp:64-79).
impl std::fmt::Display for DisallowedGiftsSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if !self.disallow_unlimited_stargifts {
            f.write_str("(unlimited)")?;
        }
        if !self.disallow_limited_stargifts {
            f.write_str("(limited)")?;
        }
        if !self.disallow_unique_stargifts {
            f.write_str("(unique)")?;
        }
        if !self.disallow_premium_gifts {
            f.write_str("(premium)")?;
        }
        if !self.disallow_gifts_from_channels {
            f.write_str("(from channels)")?;
        }
        Ok(())
    }
}

/// `td::StarGiftSettings` (StarGiftSettings.h).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StarGiftSettings {
    display_gifts_button: bool,
    disallowed_gifts: DisallowedGiftsSettings,
}

impl StarGiftSettings {
    /// `StarGiftSettings(bool, telegram_api::disallowedGiftsSettings *)`
    /// (StarGiftSettings.cpp:11-14).
    pub fn new(
        display_gifts_button: bool,
        disallowed_gifts: Option<telegram_api::DisallowedGiftsSettings>,
    ) -> Self {
        StarGiftSettings {
            display_gifts_button,
            disallowed_gifts: DisallowedGiftsSettings::from_telegram(disallowed_gifts),
        }
    }

    /// `StarGiftSettings(td_api::giftSettings *)`
    /// (StarGiftSettings.cpp:16-21).
    pub fn from_gift_settings(settings: Option<&td_api::GiftSettings>) -> Self {
        let mut result = StarGiftSettings::default();
        if let Some(settings) = settings {
            result.display_gifts_button = settings.show_gift_button;
            result.disallowed_gifts = DisallowedGiftsSettings::from_accepted_gift_types(
                settings.accepted_gift_types.as_ref(),
            );
        }
        result
    }

    /// `StarGiftSettings::allow_nothing` (StarGiftSettings.cpp:23-29).
    pub fn allow_nothing() -> Self {
        StarGiftSettings {
            display_gifts_button: false,
            disallowed_gifts: DisallowedGiftsSettings::allow_nothing(),
        }
    }

    /// `StarGiftSettings::get_gift_settings_object`
    /// (StarGiftSettings.cpp:31-35).
    pub fn get_gift_settings_object(&self) -> td_api::GiftSettings {
        td_api::GiftSettings {
            show_gift_button: self.display_gifts_button,
            accepted_gift_types: Some(self.disallowed_gifts.get_accepted_gift_types_object()),
        }
    }

    /// `StarGiftSettings::get_display_gifts_button` (StarGiftSettings.h).
    pub fn get_display_gifts_button(&self) -> bool {
        self.display_gifts_button
    }

    /// `StarGiftSettings::get_disallowed_gifts` (StarGiftSettings.h).
    pub fn get_disallowed_gifts(&self) -> &DisallowedGiftsSettings {
        &self.disallowed_gifts
    }

    /// `StarGiftSettings::is_default` (StarGiftSettings.h).
    pub fn is_default(&self) -> bool {
        !self.display_gifts_button && self.disallowed_gifts.is_default()
    }

    /// `StarGiftSettings::store` (StarGiftSettings.hpp) — the 2-flag
    /// block with the flag-gated nested settings.
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_default_disallowed_gifts = self.disallowed_gifts.is_default();
        store_parse::store_flags(
            out,
            [self.display_gifts_button, has_default_disallowed_gifts],
        );
        if !has_default_disallowed_gifts {
            self.disallowed_gifts.store(out);
        }
    }

    /// `StarGiftSettings::parse`.
    pub fn parse(input: &[u8]) -> Result<StarGiftSettings, &'static str> {
        let mut parser = Parser::new(input);
        let result = StarGiftSettings::parse_from(&mut parser)?;
        parser.fetch_end()?;
        Ok(result)
    }

    /// The parse over a shared stream (a `StarGiftSettings` stored inline
    /// inside the `UserFull` blob).
    pub fn parse_from(parser: &mut Parser) -> Result<StarGiftSettings, &'static str> {
        let [display_gifts_button, has_default_disallowed_gifts] = parser.parse_flags()?;
        let mut result = StarGiftSettings {
            display_gifts_button,
            disallowed_gifts: DisallowedGiftsSettings::default(),
        };
        if !has_default_disallowed_gifts {
            result.disallowed_gifts = DisallowedGiftsSettings::parse_from(parser)?;
        }
        Ok(result)
    }
}

/// `operator<<(StringBuilder, StarGiftSettings)`
/// (StarGiftSettings.cpp:55-63).
impl std::fmt::Display for StarGiftSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.display_gifts_button {
            f.write_str("(show button)")?;
        }
        write!(f, "{}", self.disallowed_gifts)
    }
}
// `operator==(StarGiftSettings, StarGiftSettings)`
// (StarGiftSettings.cpp:49-52) — the derived `PartialEq` on the struct.
