//! `StarGiftManager` (td/telegram/StarGiftManager.{h,cpp}) — the M.42
//! Wave-3 slice: the star gift value plane the M.9 message-content
//! deferral pointed at (`StarGift`, the attributes, the background, the
//! ids, `UserStarGift`, `StarGiftCollection`, the auction state plane and
//! `StarGiftResalePrice`) plus the manager itself with its gift-price
//! cache, auction bookkeeping and gift-message polling registry.
//!
//! The td_api conversion plane lands here (`get_gift_object`,
//! `get_upgraded_gift_object`, `get_gift_for_resale_object`,
//! `get_sent_gift_object`, `get_received_gift_object`); the message-content
//! integration flip stays with its own track.
//!
//! Seams ([`StarGiftEnv`] defaults, the unmerged-manager surfaces):
//! - StickersManager (`on_get_sticker_document`, `get_sticker_object`,
//!   `get_sticker_id`, `store_sticker`/`parse_sticker`) — the M.11
//!   sticker/file plane; the default registers nothing and answers no
//!   sticker object, so a plain `starGift` without the seam stays invalid
//!   (the C++ bails the same way on an unparsable sticker).
//! - DialogManager (`force_create_dialog`, `check_dialog_access`,
//!   `get_input_peer`, `is_broadcast_channel`, the channel status) — the
//!   chat/channel halves are the M.6.15a stub inventory; the td wiring
//!   delegates the user halves to the real read surface.
//! - MessageSender (`get_message_sender_object`) and the gift-count
//!   updates (`on_update_my_gift_count`,
//!   `on_update_user_gift_count`, `on_update_channel_gift_count`) — the
//!   MessagesManager/ChatManager/user-full plane.
//! - StarManager (`has_owned_star_count`, `add_pending_owned_star_count`,
//!   `has_owned_amount`, `add_pending_owned_amount`,
//!   `reload_owned_star_count`) — the M.46 star balance plane.
//! - PasswordManager (`get_input_check_password_srp`) — the M.25
//!   password plane.
//! - BusinessConnectionManager (`check_business_connection`) — the M.38
//!   business plane; the default rejects every connection id like an
//!   unknown one.
//! - Every `td_->create_handler<...>->send(...)` query is the
//!   `[blocked: net]` query seam: the send logs the query name and parks
//!   the promise in `StarGiftManager::pending`. The answer halves that
//!   only need the value plane and the user/chat intakes are real
//!   (`on_get_available_gifts`, `on_get_saved_star_gifts`, the auction
//!   plane, the collections, ...) and unit-tested; the payment-form query
//!   chains (SendGiftQuery and friends) answer through
//!   `updates_manager_->on_get_updates` and the StarManager pending
//!   balance, so their on_result halves stay parked with the net step
//!   (see the module divergences).
//!
//! Recorded divergences:
//! - The three `get_*_promise` statics (StarGiftManager.cpp:777-822,
//!   1086-1121, 1412-1436) process `telegram_api::Updates` through
//!   `UpdatesManager::get_new_messages` and finish through
//!   `MessagesManager::finish_gift_upgrade/finish_gift_craft`; they stay
//!   with the updates integration, parked behind the same net seam as the
//!   query chains they serve.
//! - `MultiTimeout update_gift_message_timeout_` /
//!   `reload_gift_auction_timeout_` / `Timeout
//!   active_gift_auctions_reload_timeout_`: the collapsed port keeps the
//!   registry maps and the scheduled flags; the timer firing itself is
//!   driven by the scheduler integration ([`StarGiftManager::poll_gift`]
//!   and [`StarGiftManager::reload_active_gift_auctions`] are the public
//!   fire points).
//! - `StarGiftSettings`/`DisallowedGiftsSettings` are not part of the
//!   manager header (ChatManager/UserManager consumers) — they stay with
//!   their own tracks, as do the `StarGiftAuction*` bid-flow value files
//!   beyond the state/user-state pair the manager header requires.

use crate::chat_manager::ChannelId;
use crate::dialog_id::{DialogId, DialogType, ServerMessageId};
use crate::files::FileId;
use crate::message_entity::{self, FormattedText};
use crate::peer_color::PeerColorCollectible;
use crate::store_parse::{self, Parser};
use crate::td_api;
use crate::user_manager::UserId;
use std::collections::HashMap;
use tdmtproto::telegram_api;
use tdutils::promise::Promise;
use tdutils::status::Status;

/// The `dialog_id::DialogId` → `message_entity::DialogId` bridge (the
/// entity rules carry their own minimal id enum).
fn to_entity_dialog_id(dialog_id: DialogId) -> message_entity::DialogId {
    match dialog_id.get_type() {
        DialogType::User => message_entity::DialogId::User(dialog_id.get_user_id()),
        DialogType::SecretChat => {
            message_entity::DialogId::SecretChat(dialog_id.get_secret_chat_id())
        }
        DialogType::Chat => message_entity::DialogId::Chat(dialog_id.get_chat_id()),
        DialogType::Channel => message_entity::DialogId::Channel(dialog_id.get_channel_id()),
        DialogType::None => message_entity::DialogId::None,
    }
}

/// `is_valid_color` (misc.cpp:430-432).
fn is_valid_color(color: i32) -> bool {
    (0..=0xFF_FFFF).contains(&color)
}

/// `td::BusinessConnectionId` (BusinessConnectionId.h) — the id wrapper
/// slice the manager signatures need; the binlog store is the business
/// track's.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BusinessConnectionId(String);

impl BusinessConnectionId {
    pub fn new(id: &str) -> BusinessConnectionId {
        BusinessConnectionId(id.to_owned())
    }

    /// `BusinessConnectionId::is_empty`.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// `BusinessConnectionId::is_valid` — non-empty.
    pub fn is_valid(&self) -> bool {
        !self.0.is_empty()
    }

    pub fn get(&self) -> &str {
        &self.0
    }
}

/// `td::StarGiftCollectionId` (StarGiftCollectionId.h).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StarGiftCollectionId(i32);

impl StarGiftCollectionId {
    pub fn new(id: i32) -> StarGiftCollectionId {
        StarGiftCollectionId(id)
    }

    /// `StarGiftCollectionId::get_star_gift_collection_ids`
    /// (StarGiftCollectionId.h:29-36).
    pub fn get_star_gift_collection_ids(input_ids: &[i32]) -> Vec<StarGiftCollectionId> {
        input_ids
            .iter()
            .map(|&id| StarGiftCollectionId::new(id))
            .collect()
    }

    pub fn get(self) -> i32 {
        self.0
    }

    /// `StarGiftCollectionId::is_valid` — positive.
    pub fn is_valid(self) -> bool {
        self.0 > 0
    }

    /// `StarGiftCollectionId::store` (StarGiftCollectionId.h:54-57).
    pub fn store(self, out: &mut Vec<u8>) {
        store_parse::store_i32(out, self.0);
    }

    /// `StarGiftCollectionId::parse` (StarGiftCollectionId.h:59-62).
    pub fn parse_from(parser: &mut Parser) -> Result<StarGiftCollectionId, &'static str> {
        Ok(StarGiftCollectionId::new(parser.i32_()?))
    }
}

// --- the StarAmount/TonAmount collapse (StarManager.cpp:1799-1835,
// TonAmount.cpp:14-39) — the pure amount clamps the value plane reads ---

/// `StarManager::get_star_count` (StarManager.cpp:1799-1816) — clamps to
/// ±2^51, zeroing negatives unless allowed.
pub(crate) fn get_star_count(amount: i64, allow_negative: bool) -> i64 {
    let max_amount = 1i64 << 51;
    if amount < 0 {
        if !allow_negative {
            tdutils::log_error!("Receive Telegram Star amount = {amount}");
            return 0;
        }
        if amount < -max_amount {
            tdutils::log_error!("Receive Telegram Star amount = {amount}");
            return -max_amount;
        }
    }
    if amount > max_amount {
        tdutils::log_error!("Receive Telegram Star amount = {amount}");
        return max_amount;
    }
    amount
}

/// `StarManager::get_nanostar_count` (StarManager.cpp:1818-1835) — the
/// sign-reconciling nanostar fixup; `star_count` is updated in place like
/// the C++ in/out argument.
pub(crate) fn get_nanostar_count(star_count: &mut i64, nanostar_count: i32) -> i32 {
    if -1_000_000_000 >= nanostar_count || nanostar_count >= 1_000_000_000 {
        tdutils::log_error!("Receive {star_count} + {nanostar_count} Telegram Stars");
        return nanostar_count;
    }
    let mut nanostar_count = nanostar_count;
    if *star_count < 0 && nanostar_count > 0 {
        tdutils::log_error!("Receive {star_count} + {nanostar_count} Telegram Stars");
        *star_count += 1;
        nanostar_count -= 1_000_000_000;
    }
    if *star_count > 0 && nanostar_count < 0 {
        tdutils::log_error!("Receive {star_count} + {nanostar_count} Telegram Stars");
        *star_count -= 1;
        nanostar_count += 1_000_000_000;
    }
    if !(*star_count < 0 && nanostar_count > 0 || *star_count > 0 && nanostar_count < 0) {
        return nanostar_count;
    }
    tdutils::log_error!("Receive {star_count} + {nanostar_count} Telegram Stars");
    0
}

/// `StarAmount` (StarAmount.cpp:14-19) collapsed to the pair the resale
/// price reads.
fn get_star_amount(amount: i64, nanos: i32, allow_negative: bool) -> (i64, i32) {
    let mut star_count = get_star_count(amount, allow_negative);
    let nanostar_count = get_nanostar_count(&mut star_count, nanos);
    (star_count, nanostar_count)
}

/// `TonAmount::get_ton_count` (TonAmount.cpp:26-40).
pub(crate) fn get_ton_count(amount: i64, allow_negative: bool) -> i64 {
    let max_amount = 1i64 << 51;
    if amount < 0 {
        if !allow_negative {
            tdutils::log_error!("Receive TON amount = {amount}");
            return 0;
        }
        if amount < -max_amount {
            tdutils::log_error!("Receive TON amount = {amount}");
            return -max_amount;
        }
    } else if amount > max_amount {
        tdutils::log_error!("Receive TON amount = {amount}");
        return max_amount;
    }
    amount
}

// --- StarGiftAttributeRarity (StarGiftAttributeRarity.{h,cpp,hpp}) ---

/// `StarGiftAttributeRarity::Type` (StarGiftAttributeRarity.h:17).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StarGiftAttributeRarityType {
    #[default]
    Permille = 0,
    Uncommon = 1,
    Rare = 2,
    Epic = 3,
    Legendary = 4,
}

/// `td::StarGiftAttributeRarity` (StarGiftAttributeRarity.h:16-45).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StarGiftAttributeRarity {
    r#type: StarGiftAttributeRarityType,
    rarity_permille: i32,
}

impl Default for StarGiftAttributeRarity {
    /// The C++ member initializers (StarGiftAttributeRarity.h:18-19):
    /// Permille with the -1 sentinel (an invalid rarity until set).
    fn default() -> Self {
        StarGiftAttributeRarity {
            r#type: StarGiftAttributeRarityType::Permille,
            rarity_permille: -1,
        }
    }
}

impl StarGiftAttributeRarity {
    /// `StarGiftAttributeRarity::StarGiftAttributeRarity(rarity)`
    /// (StarGiftAttributeRarity.cpp:11-34).
    pub fn from_telegram(rarity: telegram_api::StarGiftAttributeRarity) -> StarGiftAttributeRarity {
        match rarity {
            telegram_api::StarGiftAttributeRarity::starGiftAttributeRarity(rarity) => {
                StarGiftAttributeRarity {
                    r#type: StarGiftAttributeRarityType::Permille,
                    rarity_permille: rarity.permille,
                }
            }
            telegram_api::StarGiftAttributeRarity::starGiftAttributeRarityUncommon(_) => {
                StarGiftAttributeRarity {
                    r#type: StarGiftAttributeRarityType::Uncommon,
                    rarity_permille: -1,
                }
            }
            telegram_api::StarGiftAttributeRarity::starGiftAttributeRarityRare(_) => {
                StarGiftAttributeRarity {
                    r#type: StarGiftAttributeRarityType::Rare,
                    rarity_permille: -1,
                }
            }
            telegram_api::StarGiftAttributeRarity::starGiftAttributeRarityEpic(_) => {
                StarGiftAttributeRarity {
                    r#type: StarGiftAttributeRarityType::Epic,
                    rarity_permille: -1,
                }
            }
            telegram_api::StarGiftAttributeRarity::starGiftAttributeRarityLegendary(_) => {
                StarGiftAttributeRarity {
                    r#type: StarGiftAttributeRarityType::Legendary,
                    rarity_permille: -1,
                }
            }
        }
    }

    /// `StarGiftAttributeRarity::is_valid` (StarGiftAttributeRarity.h:28-30).
    pub fn is_valid(&self) -> bool {
        self.r#type != StarGiftAttributeRarityType::Permille
            || (0..=1000).contains(&self.rarity_permille)
    }

    /// `get_upgraded_gift_attribute_rarity_object`
    /// (StarGiftAttributeRarity.cpp:36-53).
    pub fn get_upgraded_gift_attribute_rarity_object(&self) -> td_api::UpgradedGiftAttributeRarity {
        match self.r#type {
            StarGiftAttributeRarityType::Permille => {
                td_api::UpgradedGiftAttributeRarity::upgradedGiftAttributeRarityPerMille(
                    td_api::upgradedGiftAttributeRarityPerMille {
                        per_mille: self.rarity_permille,
                    },
                )
            }
            StarGiftAttributeRarityType::Uncommon => {
                td_api::UpgradedGiftAttributeRarity::upgradedGiftAttributeRarityUncommon(
                    td_api::upgradedGiftAttributeRarityUncommon,
                )
            }
            StarGiftAttributeRarityType::Rare => {
                td_api::UpgradedGiftAttributeRarity::upgradedGiftAttributeRarityRare(
                    td_api::upgradedGiftAttributeRarityRare,
                )
            }
            StarGiftAttributeRarityType::Epic => {
                td_api::UpgradedGiftAttributeRarity::upgradedGiftAttributeRarityEpic(
                    td_api::upgradedGiftAttributeRarityEpic,
                )
            }
            StarGiftAttributeRarityType::Legendary => {
                td_api::UpgradedGiftAttributeRarity::upgradedGiftAttributeRarityLegendary(
                    td_api::upgradedGiftAttributeRarityLegendary,
                )
            }
        }
    }

    /// `StarGiftAttributeRarity::store` (StarGiftAttributeRarity.hpp:17-26).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_rarity_permille = self.rarity_permille >= 0;
        store_parse::store_flags(out, [has_rarity_permille]);
        store_parse::store_i32(out, self.r#type as i32);
        if has_rarity_permille {
            store_parse::store_i32(out, self.rarity_permille);
        }
    }

    /// `StarGiftAttributeRarity::parse` (StarGiftAttributeRarity.hpp:29-43)
    /// — the pre-`AddStarGiftAttributeRarity` (58) format had no flags and
    /// no type.
    pub fn parse_from(parser: &mut Parser) -> Result<StarGiftAttributeRarity, &'static str> {
        if parser.version() < store_parse::version::ADD_STAR_GIFT_ATTRIBUTE_RARITY {
            return Ok(StarGiftAttributeRarity {
                r#type: StarGiftAttributeRarityType::Permille,
                rarity_permille: parser.i32_()?,
            });
        }
        let [has_rarity_permille] = parser.parse_flags()?;
        let mut result = StarGiftAttributeRarity {
            r#type: match parser.i32_()? {
                0 => StarGiftAttributeRarityType::Permille,
                1 => StarGiftAttributeRarityType::Uncommon,
                2 => StarGiftAttributeRarityType::Rare,
                3 => StarGiftAttributeRarityType::Epic,
                4 => StarGiftAttributeRarityType::Legendary,
                _ => return Err("Invalid star gift attribute rarity type"),
            },
            rarity_permille: -1,
        };
        if has_rarity_permille {
            result.rarity_permille = parser.i32_()?;
        }
        Ok(result)
    }
}

// --- StarGiftAttributeId (StarGiftAttributeId.{h,cpp}) ---

/// `StarGiftAttributeId::Type` (StarGiftAttributeId.h:20).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum StarGiftAttributeIdType {
    #[default]
    None = 0,
    Model = 1,
    Pattern = 2,
    Backdrop = 3,
}

/// `td::StarGiftAttributeId` (StarGiftAttributeId.h:19-54).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct StarGiftAttributeId {
    r#type: StarGiftAttributeIdType,
    sticker_id: i64,
    backdrop_id: i32,
}

impl StarGiftAttributeId {
    /// The private (type, sticker_id, backdrop_id) ctor
    /// (StarGiftAttributeId.cpp:13-15).
    fn from_parts(
        r#type: StarGiftAttributeIdType,
        sticker_id: i64,
        backdrop_id: i32,
    ) -> StarGiftAttributeId {
        StarGiftAttributeId {
            r#type,
            sticker_id,
            backdrop_id,
        }
    }

    /// `StarGiftAttributeId::model` (StarGiftAttributeId.cpp:17-19).
    pub fn model(sticker_id: i64) -> StarGiftAttributeId {
        StarGiftAttributeId::from_parts(StarGiftAttributeIdType::Model, sticker_id, 0)
    }

    /// `StarGiftAttributeId::pattern` (StarGiftAttributeId.cpp:21-23).
    pub fn pattern(sticker_id: i64) -> StarGiftAttributeId {
        StarGiftAttributeId::from_parts(StarGiftAttributeIdType::Pattern, sticker_id, 0)
    }

    /// `StarGiftAttributeId::backdrop` (StarGiftAttributeId.cpp:25-27).
    pub fn backdrop(backdrop_id: i32) -> StarGiftAttributeId {
        StarGiftAttributeId::from_parts(StarGiftAttributeIdType::Backdrop, 0, backdrop_id)
    }

    /// `get_star_gift_attribute_id` (StarGiftAttributeId.cpp:29-45).
    pub fn get_star_gift_attribute_id(
        attribute: Option<&td_api::UpgradedGiftAttributeId>,
    ) -> Result<StarGiftAttributeId, Status> {
        let Some(attribute) = attribute else {
            return Err(Status::error(400, "Attribute identifier must be non-empty"));
        };
        match attribute {
            td_api::UpgradedGiftAttributeId::upgradedGiftAttributeIdModel(attribute) => {
                Ok(StarGiftAttributeId::model(attribute.sticker_id))
            }
            td_api::UpgradedGiftAttributeId::upgradedGiftAttributeIdSymbol(attribute) => {
                Ok(StarGiftAttributeId::pattern(attribute.sticker_id))
            }
            td_api::UpgradedGiftAttributeId::upgradedGiftAttributeIdBackdrop(attribute) => {
                Ok(StarGiftAttributeId::backdrop(attribute.backdrop_id))
            }
        }
    }

    /// `get_star_gift_attribute_ids` (StarGiftAttributeId.cpp:47-55).
    pub fn get_star_gift_attribute_ids(
        attributes: &[td_api::UpgradedGiftAttributeId],
    ) -> Result<Vec<StarGiftAttributeId>, Status> {
        let mut result = Vec::with_capacity(attributes.len());
        for attribute in attributes {
            result.push(StarGiftAttributeId::get_star_gift_attribute_id(Some(
                attribute,
            ))?);
        }
        Ok(result)
    }

    /// `StarGiftAttributeId(telegram_api::StarGiftAttributeId)`
    /// (StarGiftAttributeId.cpp:57-75).
    pub fn from_telegram(attribute: telegram_api::StarGiftAttributeId) -> StarGiftAttributeId {
        match attribute {
            telegram_api::StarGiftAttributeId::starGiftAttributeIdModel(attribute) => {
                StarGiftAttributeId::model(attribute.document_id)
            }
            telegram_api::StarGiftAttributeId::starGiftAttributeIdPattern(attribute) => {
                StarGiftAttributeId::pattern(attribute.document_id)
            }
            telegram_api::StarGiftAttributeId::starGiftAttributeIdBackdrop(attribute) => {
                StarGiftAttributeId::backdrop(attribute.backdrop_id)
            }
        }
    }

    /// `get_input_star_gift_attribute_id_object`
    /// (StarGiftAttributeId.cpp:77-91) — the `None` type is the C++
    /// `UNREACHABLE()`.
    pub fn get_input_star_gift_attribute_id_object(&self) -> telegram_api::StarGiftAttributeId {
        match self.r#type {
            StarGiftAttributeIdType::Model => {
                telegram_api::StarGiftAttributeId::starGiftAttributeIdModel(
                    telegram_api::starGiftAttributeIdModel {
                        document_id: self.sticker_id,
                    },
                )
            }
            StarGiftAttributeIdType::Pattern => {
                telegram_api::StarGiftAttributeId::starGiftAttributeIdPattern(
                    telegram_api::starGiftAttributeIdPattern {
                        document_id: self.sticker_id,
                    },
                )
            }
            StarGiftAttributeIdType::Backdrop => {
                telegram_api::StarGiftAttributeId::starGiftAttributeIdBackdrop(
                    telegram_api::starGiftAttributeIdBackdrop {
                        backdrop_id: self.backdrop_id,
                    },
                )
            }
            StarGiftAttributeIdType::None => unreachable!("empty attribute id has no input form"),
        }
    }

    /// `get_input_star_gift_attribute_ids_object`
    /// (StarGiftAttributeId.cpp:93-98).
    pub fn get_input_star_gift_attribute_ids_object(
        attributes: &[StarGiftAttributeId],
    ) -> Vec<telegram_api::StarGiftAttributeId> {
        attributes
            .iter()
            .map(StarGiftAttributeId::get_input_star_gift_attribute_id_object)
            .collect()
    }
}

// --- StarGiftBackground (StarGiftBackground.{h,cpp,hpp}) ---

/// `td::StarGiftBackground` (StarGiftBackground.h:17-42).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StarGiftBackground {
    center_color: i32,
    edge_color: i32,
    text_color: i32,
}

impl StarGiftBackground {
    /// The ctor over `starGiftBackground` (StarGiftBackground.h:29-33) —
    /// the single-constructor TL type is a plain struct here.
    pub fn from_telegram(background: telegram_api::StarGiftBackground) -> StarGiftBackground {
        StarGiftBackground {
            center_color: background.center_color,
            edge_color: background.edge_color,
            text_color: background.text_color,
        }
    }

    /// `get_gift_background_object` (StarGiftBackground.cpp:11-13).
    pub fn get_gift_background_object(&self) -> td_api::GiftBackground {
        td_api::GiftBackground {
            center_color: self.center_color,
            edge_color: self.edge_color,
            text_color: self.text_color,
        }
    }

    /// `StarGiftBackground::store` (StarGiftBackground.hpp:16-22) — the
    /// empty flag word is part of the C++ format.
    pub fn store(&self, out: &mut Vec<u8>) {
        store_parse::store_flags(out, []);
        store_parse::store_i32(out, self.center_color);
        store_parse::store_i32(out, self.edge_color);
        store_parse::store_i32(out, self.text_color);
    }

    /// `StarGiftBackground::parse` (StarGiftBackground.hpp:25-31).
    pub fn parse_from(parser: &mut Parser) -> Result<StarGiftBackground, &'static str> {
        parser.parse_flags::<0>()?;
        Ok(StarGiftBackground {
            center_color: parser.i32_()?,
            edge_color: parser.i32_()?,
            text_color: parser.i32_()?,
        })
    }
}

// --- StarGiftAttribute.{h,cpp,hpp} ---

/// `td::StarGiftAttributeSticker` (StarGiftAttribute.h:25-59) — the model
/// and the pattern (symbol) share the class.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StarGiftAttributeSticker {
    name: String,
    sticker_file_id: FileId,
    rarity: StarGiftAttributeRarity,
    is_crafted: bool,
}

impl StarGiftAttributeSticker {
    /// `StarGiftAttributeSticker(Td *, starGiftAttributeModel)`
    /// (StarGiftAttribute.cpp:18-27).
    pub fn from_model(
        env: &mut impl StarGiftEnv,
        attribute: telegram_api::starGiftAttributeModel,
    ) -> StarGiftAttributeSticker {
        StarGiftAttributeSticker {
            name: attribute.name,
            sticker_file_id: env
                .on_get_sticker_document(attribute.document, "StarGiftAttributeSticker")
                .unwrap_or_default(),
            rarity: attribute
                .rarity
                .map(StarGiftAttributeRarity::from_telegram)
                .unwrap_or_default(),
            is_crafted: attribute.crafted,
        }
    }

    /// `StarGiftAttributeSticker(Td *, starGiftAttributePattern)`
    /// (StarGiftAttribute.cpp:29-37).
    pub fn from_pattern(
        env: &mut impl StarGiftEnv,
        attribute: telegram_api::starGiftAttributePattern,
    ) -> StarGiftAttributeSticker {
        StarGiftAttributeSticker {
            name: attribute.name,
            sticker_file_id: env
                .on_get_sticker_document(attribute.document, "StarGiftAttributeSticker")
                .unwrap_or_default(),
            rarity: attribute
                .rarity
                .map(StarGiftAttributeRarity::from_telegram)
                .unwrap_or_default(),
            is_crafted: false,
        }
    }

    /// `StarGiftAttributeSticker::is_valid` (StarGiftAttribute.h:40-42).
    pub fn is_valid(&self) -> bool {
        self.rarity.is_valid() && self.sticker_file_id.is_valid()
    }

    /// `StarGiftAttributeSticker::is_crafted` (StarGiftAttribute.h:44-46).
    pub fn is_crafted(&self) -> bool {
        self.is_crafted
    }

    /// `get_upgraded_gift_model_object` (StarGiftAttribute.cpp:39-45).
    pub fn get_upgraded_gift_model_object(
        &self,
        env: &mut impl StarGiftEnv,
    ) -> td_api::UpgradedGiftModel {
        td_api::UpgradedGiftModel {
            name: self.name.clone(),
            sticker: env.get_sticker_object(self.sticker_file_id),
            rarity: Some(self.rarity.get_upgraded_gift_attribute_rarity_object()),
            is_crafted: self.is_crafted,
        }
    }

    /// `get_upgraded_gift_symbol_object` (StarGiftAttribute.cpp:47-53).
    pub fn get_upgraded_gift_symbol_object(
        &self,
        env: &mut impl StarGiftEnv,
    ) -> td_api::UpgradedGiftSymbol {
        td_api::UpgradedGiftSymbol {
            name: self.name.clone(),
            sticker: env.get_sticker_object(self.sticker_file_id),
            rarity: Some(self.rarity.get_upgraded_gift_attribute_rarity_object()),
        }
    }

    /// `StarGiftAttributeSticker::get_id` (StarGiftAttribute.cpp:55-59).
    pub fn get_id(&self, env: &mut impl StarGiftEnv, is_model: bool) -> StarGiftAttributeId {
        let sticker_id = env.get_sticker_id(self.sticker_file_id);
        if is_model {
            StarGiftAttributeId::model(sticker_id)
        } else {
            StarGiftAttributeId::pattern(sticker_id)
        }
    }

    /// `StarGiftAttributeSticker::store` (StarGiftAttribute.hpp:22-31).
    pub fn store(&self, env: &mut impl StarGiftEnv, out: &mut Vec<u8>) {
        store_parse::store_flags(out, [self.is_crafted]);
        store_parse::store_string(out, &self.name);
        env.store_sticker(self.sticker_file_id, out);
        self.rarity.store(out);
    }

    /// `StarGiftAttributeSticker::parse` (StarGiftAttribute.hpp:34-42).
    pub fn parse_from(
        env: &mut impl StarGiftEnv,
        parser: &mut Parser,
    ) -> Result<StarGiftAttributeSticker, &'static str> {
        let [is_crafted] = parser.parse_flags()?;
        let name = parser.string()?;
        let sticker_file_id = env.parse_sticker(parser);
        let rarity = StarGiftAttributeRarity::parse_from(parser)?;
        Ok(StarGiftAttributeSticker {
            name,
            sticker_file_id,
            rarity,
            is_crafted,
        })
    }
}

/// `td::StarGiftAttributeBackdrop` (StarGiftAttribute.h:67-94).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StarGiftAttributeBackdrop {
    name: String,
    id: i32,
    center_color: i32,
    edge_color: i32,
    pattern_color: i32,
    text_color: i32,
    rarity: StarGiftAttributeRarity,
}

impl StarGiftAttributeBackdrop {
    /// `StarGiftAttributeBackdrop(starGiftAttributeBackdrop)`
    /// (StarGiftAttribute.cpp:66-75).
    pub fn from_telegram(attribute: telegram_api::starGiftAttributeBackdrop) -> Self {
        StarGiftAttributeBackdrop {
            name: attribute.name,
            id: attribute.backdrop_id,
            center_color: attribute.center_color,
            edge_color: attribute.edge_color,
            pattern_color: attribute.pattern_color,
            text_color: attribute.text_color,
            rarity: attribute
                .rarity
                .map(StarGiftAttributeRarity::from_telegram)
                .unwrap_or_default(),
        }
    }

    /// `StarGiftAttributeBackdrop::is_valid` (StarGiftAttribute.cpp:77-80).
    pub fn is_valid(&self) -> bool {
        self.rarity.is_valid()
            && is_valid_color(self.center_color)
            && is_valid_color(self.edge_color)
            && is_valid_color(self.pattern_color)
            && is_valid_color(self.text_color)
    }

    /// `get_upgraded_gift_backdrop_object` (StarGiftAttribute.cpp:82-88).
    pub fn get_upgraded_gift_backdrop_object(&self) -> td_api::UpgradedGiftBackdrop {
        td_api::UpgradedGiftBackdrop {
            id: self.id,
            name: self.name.clone(),
            colors: Some(td_api::UpgradedGiftBackdropColors {
                center_color: self.center_color,
                edge_color: self.edge_color,
                symbol_color: self.pattern_color,
                text_color: self.text_color,
            }),
            rarity: Some(self.rarity.get_upgraded_gift_attribute_rarity_object()),
        }
    }

    /// `StarGiftAttributeBackdrop::get_id` (StarGiftAttribute.cpp:90-93).
    pub fn get_id(&self) -> StarGiftAttributeId {
        StarGiftAttributeId::backdrop(self.id)
    }

    /// `StarGiftAttributeBackdrop::store` (StarGiftAttribute.hpp:45-60).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_id = self.id != 0;
        store_parse::store_flags(out, [has_id]);
        store_parse::store_string(out, &self.name);
        store_parse::store_i32(out, self.center_color);
        store_parse::store_i32(out, self.edge_color);
        store_parse::store_i32(out, self.pattern_color);
        store_parse::store_i32(out, self.text_color);
        self.rarity.store(out);
        if has_id {
            store_parse::store_i32(out, self.id);
        }
    }

    /// `StarGiftAttributeBackdrop::parse` (StarGiftAttribute.hpp:63-77).
    pub fn parse_from(parser: &mut Parser) -> Result<StarGiftAttributeBackdrop, &'static str> {
        let [has_id] = parser.parse_flags()?;
        let name = parser.string()?;
        let center_color = parser.i32_()?;
        let edge_color = parser.i32_()?;
        let pattern_color = parser.i32_()?;
        let text_color = parser.i32_()?;
        let rarity = StarGiftAttributeRarity::parse_from(parser)?;
        let id = if has_id { parser.i32_()? } else { 0 };
        Ok(StarGiftAttributeBackdrop {
            name,
            id,
            center_color,
            edge_color,
            pattern_color,
            text_color,
            rarity,
        })
    }
}

/// `td::StarGiftAttributeOriginalDetails` (StarGiftAttribute.h:102-130).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StarGiftAttributeOriginalDetails {
    sender_dialog_id: DialogId,
    receiver_dialog_id: DialogId,
    date: i32,
    message: FormattedText,
}

impl StarGiftAttributeOriginalDetails {
    /// `StarGiftAttributeOriginalDetails(Td *,
    /// starGiftAttributeOriginalDetails)` (StarGiftAttribute.cpp:101-110).
    /// The formatted-text parsing rides the `()` user-manager resolver —
    /// the real manager's entity side effects arrive with the user
    /// integration (the M.4 convention in synchronous_requests).
    pub fn from_telegram(attribute: telegram_api::starGiftAttributeOriginalDetails) -> Self {
        let mut sender_dialog_id = DialogId::default();
        if let Some(sender_id) = &attribute.sender_id {
            sender_dialog_id = DialogId::from_peer(sender_id);
        }
        StarGiftAttributeOriginalDetails {
            sender_dialog_id,
            receiver_dialog_id: attribute
                .recipient_id
                .as_ref()
                .map(DialogId::from_peer)
                .unwrap_or_default(),
            date: attribute.date,
            message: message_entity::get_formatted_text_of_text_with_entities::<()>(
                None,
                attribute.message,
                true,
                false,
                "starGiftAttributeBackdrop",
            ),
        }
    }

    /// `StarGiftAttributeOriginalDetails::is_valid`
    /// (StarGiftAttribute.h:116-119).
    pub fn is_valid(&self) -> bool {
        (self.sender_dialog_id == DialogId::default() || self.sender_dialog_id.is_valid())
            && self.receiver_dialog_id.is_valid()
            && self.date > 0
    }

    /// `get_upgraded_gift_original_details_object`
    /// (StarGiftAttribute.cpp:112-123) — an invalid attribute has no
    /// object (the C++ nullptr).
    pub fn get_upgraded_gift_original_details_object(
        &self,
        env: &mut impl StarGiftEnv,
    ) -> Option<td_api::UpgradedGiftOriginalDetails> {
        if !self.is_valid() {
            return None;
        }
        Some(td_api::UpgradedGiftOriginalDetails {
            sender_id: if self.sender_dialog_id == DialogId::default() {
                None
            } else {
                env.get_message_sender_object(
                    self.sender_dialog_id,
                    "upgradedGiftOriginalDetails sender",
                )
            },
            receiver_id: env.get_message_sender_object(
                self.receiver_dialog_id,
                "upgradedGiftOriginalDetails receiver",
            ),
            text: Some(message_entity::get_formatted_text_object::<()>(
                None,
                &self.message,
                true,
                -1,
            )),
            date: self.date,
        })
    }

    // `add_dependencies` (StarGiftAttribute.cpp:125-128) — the sender /
    // receiver dialog dependencies ride the messages integration's
    // Dependencies plane (M.6), which this base does not carry yet.

    /// `StarGiftAttributeOriginalDetails::store`
    /// (StarGiftAttribute.hpp:80-106) — the user-sender/receiver rows
    /// store the bare user id.
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_sender_user_id = self.sender_dialog_id.get_type() == DialogType::User;
        let has_message = !self.message.text.is_empty();
        let has_sender_dialog_id =
            self.sender_dialog_id != DialogId::default() && !has_sender_user_id;
        let has_receiver_dialog_id = self.receiver_dialog_id.get_type() != DialogType::User;
        store_parse::store_flags(
            out,
            [
                has_sender_user_id,
                has_message,
                has_sender_dialog_id,
                has_receiver_dialog_id,
            ],
        );
        if has_sender_user_id {
            store_parse::store_i64(out, self.sender_dialog_id.get_user_id().get());
        } else if has_sender_dialog_id {
            self.sender_dialog_id.store(out);
        }
        if has_receiver_dialog_id {
            self.receiver_dialog_id.store(out);
        } else {
            store_parse::store_i64(out, self.receiver_dialog_id.get_user_id().get());
        }
        store_parse::store_i32(out, self.date);
        if has_message {
            self.message.store(out);
        }
    }

    /// `StarGiftAttributeOriginalDetails::parse`
    /// (StarGiftAttribute.hpp:109-138).
    pub fn parse_from(parser: &mut Parser) -> Result<Self, &'static str> {
        let [
            has_sender_user_id,
            has_message,
            has_sender_dialog_id,
            has_receiver_dialog_id,
        ] = parser.parse_flags()?;
        let mut sender_dialog_id = DialogId::default();
        if has_sender_user_id {
            sender_dialog_id = DialogId::from_user_id(UserId::new(parser.i64_()?));
        } else if has_sender_dialog_id {
            sender_dialog_id = DialogId::parse_from(parser)?;
        }
        let receiver_dialog_id = if has_receiver_dialog_id {
            DialogId::parse_from(parser)?
        } else {
            DialogId::from_user_id(UserId::new(parser.i64_()?))
        };
        let date = parser.i32_()?;
        let message = if has_message {
            FormattedText::parse(parser)?
        } else {
            FormattedText::default()
        };
        Ok(StarGiftAttributeOriginalDetails {
            sender_dialog_id,
            receiver_dialog_id,
            date,
            message,
        })
    }
}

// --- StarGiftResalePrice (StarGiftResalePrice.{h,cpp,hpp}) ---

/// `StarGiftResalePrice::Type` (StarGiftResalePrice.h:21).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StarGiftResalePriceType {
    #[default]
    None = 0,
    Star = 1,
    Ton = 2,
}

/// `td::StarGiftResalePrice` (StarGiftResalePrice.h:20-71).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StarGiftResalePrice {
    r#type: StarGiftResalePriceType,
    amount: i64,
}

/// `TON_MULTIPLIER` (StarGiftResalePrice.h:25).
const TON_MULTIPLIER: i64 = 10_000_000;

impl StarGiftResalePrice {
    /// `StarGiftResalePrice(StarsAmount)` (StarGiftResalePrice.cpp:18-52).
    pub fn from_telegram(amount_ptr: Option<telegram_api::StarsAmount>) -> StarGiftResalePrice {
        let Some(amount_ptr) = amount_ptr else {
            return StarGiftResalePrice::default();
        };
        match amount_ptr {
            telegram_api::StarsAmount::starsAmount(amount) => {
                let (star_count, nanostar_count) =
                    get_star_amount(amount.amount, amount.nanos, false);
                if nanostar_count != 0 {
                    tdutils::log_error!(
                        "Receive price of {star_count}.{nanostar_count} Telegram Stars"
                    );
                }
                if star_count == 0 {
                    return StarGiftResalePrice::default();
                }
                StarGiftResalePrice {
                    r#type: StarGiftResalePriceType::Star,
                    amount: star_count,
                }
            }
            telegram_api::StarsAmount::starsTonAmount(amount) => {
                let mut ton_amount = get_ton_count(amount.amount, false);
                if ton_amount % TON_MULTIPLIER != 0 {
                    tdutils::log_error!("Receive price of {ton_amount} Grams");
                }
                ton_amount /= TON_MULTIPLIER;
                if ton_amount == 0 {
                    return StarGiftResalePrice::default();
                }
                StarGiftResalePrice {
                    r#type: StarGiftResalePriceType::Ton,
                    amount: ton_amount,
                }
            }
        }
    }

    /// `get_star_gift_resale_price` (StarGiftResalePrice.cpp:54-99).
    pub fn get_star_gift_resale_price(
        env: &mut impl StarGiftEnv,
        price: Option<td_api::GiftResalePrice>,
        is_purchase: bool,
    ) -> Result<StarGiftResalePrice, Status> {
        let Some(price) = price else {
            if is_purchase {
                return Err(Status::error(400, "Gift resale price must be non-empty"));
            }
            return Ok(StarGiftResalePrice::default());
        };
        match price {
            td_api::GiftResalePrice::giftResalePriceStar(price) => {
                let amount = price.star_count;
                if amount <= 0 {
                    return Err(Status::error(
                        400,
                        "Invalid amount of Telegram Stars specified",
                    ));
                }
                if !is_purchase
                    && (amount < env.get_option_integer("gift_resale_star_count_min")
                        || amount > env.get_option_integer("gift_resale_star_count_max"))
                {
                    return Err(Status::error(
                        400,
                        "Invalid amount of Telegram Stars specified",
                    ));
                }
                Ok(StarGiftResalePrice {
                    r#type: StarGiftResalePriceType::Star,
                    amount,
                })
            }
            td_api::GiftResalePrice::giftResalePriceGram(price) => {
                let amount = price.gram_cent_count;
                if amount <= 0 {
                    return Err(Status::error(400, "Invalid amount of Grams specified"));
                }
                if !is_purchase
                    && (amount < env.get_option_integer("gift_resale_gram_cent_count_min")
                        || amount > env.get_option_integer("gift_resale_gram_cent_count_max"))
                {
                    return Err(Status::error(400, "Invalid amount of Gram cents specified"));
                }
                Ok(StarGiftResalePrice {
                    r#type: StarGiftResalePriceType::Ton,
                    amount,
                })
            }
        }
    }

    /// `StarGiftResalePrice::legacy` (StarGiftResalePrice.cpp:101-106).
    pub fn legacy(star_count: i64) -> StarGiftResalePrice {
        StarGiftResalePrice {
            r#type: StarGiftResalePriceType::Star,
            amount: star_count,
        }
    }

    /// `StarGiftResalePrice::is_empty` (StarGiftResalePrice.h:42-44).
    pub fn is_empty(&self) -> bool {
        self.r#type == StarGiftResalePriceType::None
    }

    /// `StarGiftResalePrice::is_star` (StarGiftResalePrice.h:46-48).
    pub fn is_star(&self) -> bool {
        self.r#type == StarGiftResalePriceType::Star
    }

    /// `StarGiftResalePrice::is_ton` (StarGiftResalePrice.h:50-52).
    pub fn is_ton(&self) -> bool {
        self.r#type == StarGiftResalePriceType::Ton
    }

    /// `StarGiftResalePrice::get_star_count` (StarGiftResalePrice.h:54-56).
    pub fn get_star_count(&self) -> i64 {
        self.amount
    }

    /// `StarGiftResalePrice::get_ton_count` (StarGiftResalePrice.h:58-60).
    pub fn get_ton_count(&self) -> i64 {
        self.amount * TON_MULTIPLIER
    }

    /// `get_input_stars_amount` (StarGiftResalePrice.cpp:108-120).
    pub fn get_input_stars_amount(&self) -> telegram_api::StarsAmount {
        match self.r#type {
            StarGiftResalePriceType::None => {
                telegram_api::StarsAmount::starsAmount(telegram_api::starsAmount {
                    amount: 0,
                    nanos: 0,
                })
            }
            StarGiftResalePriceType::Star => {
                telegram_api::StarsAmount::starsAmount(telegram_api::starsAmount {
                    amount: self.amount,
                    nanos: 0,
                })
            }
            StarGiftResalePriceType::Ton => {
                telegram_api::StarsAmount::starsTonAmount(telegram_api::starsTonAmount {
                    amount: self.amount * TON_MULTIPLIER,
                })
            }
        }
    }

    /// `get_gift_resale_price_object` (StarGiftResalePrice.cpp:122-134).
    pub fn get_gift_resale_price_object(&self) -> Option<td_api::GiftResalePrice> {
        match self.r#type {
            StarGiftResalePriceType::None => None,
            StarGiftResalePriceType::Star => Some(td_api::GiftResalePrice::giftResalePriceStar(
                td_api::giftResalePriceStar {
                    star_count: self.amount,
                },
            )),
            StarGiftResalePriceType::Ton => Some(td_api::GiftResalePrice::giftResalePriceGram(
                td_api::giftResalePriceGram {
                    gram_cent_count: self.amount,
                },
            )),
        }
    }

    /// `StarGiftResalePrice::store` (StarGiftResalePrice.hpp:16-25).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_amount = self.amount != 0;
        store_parse::store_flags(out, [has_amount]);
        store_parse::store_i32(out, self.r#type as i32);
        if has_amount {
            store_parse::store_i64(out, self.amount);
        }
    }

    /// `StarGiftResalePrice::parse` (StarGiftResalePrice.hpp:28-37).
    pub fn parse_from(parser: &mut Parser) -> Result<StarGiftResalePrice, &'static str> {
        let [has_amount] = parser.parse_flags()?;
        let r#type = match parser.i32_()? {
            0 => StarGiftResalePriceType::None,
            1 => StarGiftResalePriceType::Star,
            2 => StarGiftResalePriceType::Ton,
            _ => return Err("Invalid resale price type"),
        };
        let amount = if has_amount { parser.i64_()? } else { 0 };
        Ok(StarGiftResalePrice { r#type, amount })
    }
}

// --- StarGift (StarGift.{h,cpp,hpp}) ---

/// `td::StarGift` (StarGift.h:26-138) — the collapsed regular/unique
/// gift value class; the C++ member initializers all default through
/// `Default` (no non-zero initializers).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StarGift {
    id: i64,
    released_by_dialog_id: DialogId,
    is_premium: bool,

    sticker_file_id: FileId,
    star_count: i64,
    default_sell_star_count: i64,
    upgrade_star_count: i64,
    upgrade_variants: i32,
    availability_remains: i32,
    availability_total: i32,
    first_sale_date: i32,
    last_sale_date: i32,
    per_user_remains: i32,
    per_user_total: i32,
    locked_until_date: i32,
    background: Option<StarGiftBackground>,

    has_colors: bool,
    is_for_birthday: bool,
    is_auction: bool,

    is_unique: bool,
    resale_ton_only: bool,
    is_theme_available: bool,
    is_burned: bool,
    is_crafted: bool,
    model: StarGiftAttributeSticker,
    pattern: StarGiftAttributeSticker,
    backdrop: StarGiftAttributeBackdrop,
    original_details: StarGiftAttributeOriginalDetails,
    title: String,
    slug: String,
    auction_slug: String,
    host_dialog_id: DialogId,
    owner_dialog_id: DialogId,
    owner_address: String,
    owner_name: String,
    gift_address: String,
    num: i32,
    unique_availability_issued: i32,
    unique_availability_total: i32,
    resale_star_count: i64,
    resale_ton_count: i64,
    offer_min_star_count: i64,
    regular_gift_id: i64,
    gifts_per_round: i32,
    auction_start_date: i32,
    craft_chance_permille: i32,
    value_currency: String,
    value_amount: i64,
    value_usd_amount: i64,
    theme_dialog_id: DialogId,
    peer_color: Option<PeerColorCollectible>,
}

impl StarGift {
    /// `fix_availability` (StarGift.cpp:23-36).
    pub fn fix_availability(total: &mut i32, remains: &mut i32) {
        if *total < 0 {
            tdutils::log_error!("Receive {total} total available gifts");
            *total = 0;
        }
        if (*total != 0 || *remains != 0) && (*remains < 0 || *remains > *total) {
            tdutils::log_error!("Receive {remains} remained available gifts out of {total}");
            if *remains < 0 {
                *remains = 0;
            } else {
                *remains = *total;
            }
        }
    }

    /// `StarGift(Td *, StarGift, allow_unique_gift)` (StarGift.cpp:38-198).
    pub fn from_telegram(
        env: &mut impl StarGiftEnv,
        star_gift_ptr: telegram_api::StarGift,
        allow_unique_gift: bool,
    ) -> StarGift {
        let mut gift = StarGift::default();
        match star_gift_ptr {
            telegram_api::StarGift::starGiftUnique(star_gift) => {
                if !allow_unique_gift {
                    // the C++ falls through to the constructor-id error
                    tdutils::log_error!("Receive an unexpected upgraded gift");
                    return gift;
                }
                if star_gift.id == 0 {
                    tdutils::log_error!("Receive {star_gift:?}");
                    return gift;
                }
                gift.is_unique = true;
                gift.id = star_gift.id;
                gift.regular_gift_id = star_gift.gift_id;
                gift.title = star_gift.title;
                gift.slug = star_gift.slug;
                gift.num = star_gift.num;
                if let Some(host_id) = &star_gift.host_id {
                    gift.host_dialog_id = DialogId::from_peer(host_id);
                }
                if let Some(owner_id) = &star_gift.owner_id {
                    gift.owner_dialog_id = DialogId::from_peer(owner_id);
                }
                gift.owner_name = star_gift.owner_name;
                gift.owner_address = star_gift.owner_address;
                gift.gift_address = star_gift.gift_address;
                gift.unique_availability_issued = star_gift.availability_issued;
                gift.unique_availability_total = star_gift.availability_total;
                if !star_gift.resell_amount.is_empty() {
                    // resell_amount[0] must be starsAmount and [1]
                    // starsTonAmount (StarGift.cpp:64-74)
                    let is_supported = star_gift.resell_amount.len() >= 2
                        && matches!(
                            star_gift.resell_amount[0],
                            telegram_api::StarsAmount::starsAmount(_)
                        )
                        && matches!(
                            star_gift.resell_amount[1],
                            telegram_api::StarsAmount::starsTonAmount(_)
                        );
                    if !is_supported {
                        tdutils::log_error!("Receive unsupported resale amount");
                    } else {
                        gift.resale_star_count = StarGiftResalePrice::from_telegram(Some(
                            star_gift.resell_amount[0].clone(),
                        ))
                        .get_star_count();
                        gift.resale_ton_count = StarGiftResalePrice::from_telegram(Some(
                            star_gift.resell_amount[1].clone(),
                        ))
                        .get_ton_count();
                        gift.resale_ton_only = star_gift.resale_ton_only;
                    }
                }
                gift.offer_min_star_count = get_star_count(star_gift.offer_min_stars as i64, false);
                gift.is_burned = star_gift.burned;
                gift.is_crafted = star_gift.crafted;
                gift.is_theme_available = star_gift.theme_available;
                if let Some(released_by) = &star_gift.released_by {
                    gift.released_by_dialog_id = DialogId::from_peer(released_by);
                    env.force_create_dialog(gift.released_by_dialog_id, "StarGift");
                }
                gift.is_premium = star_gift.require_premium;
                for attribute in star_gift.attributes {
                    match attribute {
                        telegram_api::StarGiftAttribute::starGiftAttributeModel(attribute) => {
                            if gift.model.is_valid() {
                                tdutils::log_error!("Receive duplicate model for the gift");
                            }
                            gift.model = StarGiftAttributeSticker::from_model(env, attribute);
                            if !gift.model.is_valid() {
                                tdutils::log_error!("Receive invalid model for the gift");
                            }
                        }
                        telegram_api::StarGiftAttribute::starGiftAttributePattern(attribute) => {
                            if gift.pattern.is_valid() {
                                tdutils::log_error!("Receive duplicate symbol for the gift");
                            }
                            gift.pattern = StarGiftAttributeSticker::from_pattern(env, attribute);
                            if !gift.pattern.is_valid() {
                                tdutils::log_error!("Receive invalid symbol for the gift");
                            }
                        }
                        telegram_api::StarGiftAttribute::starGiftAttributeBackdrop(attribute) => {
                            if gift.backdrop.is_valid() {
                                tdutils::log_error!("Receive duplicate backdrop for the gift");
                            }
                            gift.backdrop = StarGiftAttributeBackdrop::from_telegram(attribute);
                            if !gift.backdrop.is_valid() {
                                tdutils::log_error!("Receive invalid backdrop for the gift");
                            }
                        }
                        telegram_api::StarGiftAttribute::starGiftAttributeOriginalDetails(
                            attribute,
                        ) => {
                            if gift.original_details.is_valid() {
                                tdutils::log_error!(
                                    "Receive duplicate original details for the gift"
                                );
                            }
                            gift.original_details =
                                StarGiftAttributeOriginalDetails::from_telegram(attribute);
                            if !gift.original_details.is_valid() {
                                tdutils::log_error!(
                                    "Receive invalid original details for the gift"
                                );
                            }
                        }
                    }
                }
                // the C++ resale order: [0] must be starsAmount and [1]
                // starsTonAmount together; re-checked as a pair to keep the
                // error single-shot and the ton-only flag branch-true
                gift.craft_chance_permille = 0.max(star_gift.craft_chance_permille);
                gift.value_currency = star_gift.value_currency;
                gift.value_amount = star_gift.value_amount;
                gift.value_usd_amount = star_gift.value_usd_amount;
                if let Some(theme_peer) = &star_gift.theme_peer {
                    gift.theme_dialog_id = DialogId::from_peer(theme_peer);
                    env.force_create_dialog(gift.theme_dialog_id, "StarGift");
                }
                if let Some(peer_color) = star_gift.peer_color {
                    match peer_color {
                        telegram_api::PeerColor::peerColorCollectible(peer_color) => {
                            gift.peer_color =
                                PeerColorCollectible::get_peer_color_collectible(peer_color);
                        }
                        _ => {
                            tdutils::log_error!("Receive a non-collectible peer color");
                        }
                    }
                }
                gift
            }
            telegram_api::StarGift::starGift(star_gift) => {
                if star_gift.id == 0 {
                    tdutils::log_error!("Receive {star_gift:?}");
                    return gift;
                }
                let Some(sticker_id) = env.on_get_sticker_document(star_gift.sticker, "StarGift")
                else {
                    return gift;
                };
                let mut availability_total = star_gift.availability_total;
                let mut availability_remains = star_gift.availability_remains;
                Self::fix_availability(&mut availability_total, &mut availability_remains);
                if availability_remains == 0 && availability_total > 0 {
                    gift.first_sale_date = 0.max(star_gift.first_sale_date);
                    gift.last_sale_date = gift.first_sale_date.max(star_gift.last_sale_date);
                }
                let mut per_user_total = star_gift.per_user_total;
                let mut per_user_remains = star_gift.per_user_remains;
                Self::fix_availability(&mut per_user_total, &mut per_user_remains);

                gift.id = star_gift.id;
                gift.star_count = get_star_count(star_gift.stars, false);
                gift.default_sell_star_count = get_star_count(star_gift.convert_stars, false);
                // the flag-absent field parses as 0, like the C++ read
                gift.upgrade_star_count = get_star_count(star_gift.upgrade_stars, false);
                gift.upgrade_variants = 0.max(star_gift.upgrade_variants);
                gift.sticker_file_id = sticker_id;
                gift.availability_remains = availability_remains;
                gift.availability_total = availability_total;
                gift.per_user_remains = per_user_remains;
                gift.per_user_total = per_user_total;
                gift.has_colors = star_gift.peer_color_available;
                gift.is_for_birthday = star_gift.birthday;
                if let Some(released_by) = &star_gift.released_by {
                    gift.released_by_dialog_id = DialogId::from_peer(released_by);
                    env.force_create_dialog(gift.released_by_dialog_id, "StarGift");
                }
                gift.is_premium = star_gift.require_premium;
                gift.is_auction = star_gift.auction;
                gift.auction_slug = star_gift.auction_slug;
                gift.gifts_per_round = 0.max(star_gift.gifts_per_round);
                gift.auction_start_date = 0.max(star_gift.auction_start_date);
                gift.locked_until_date = 0.max(star_gift.locked_until_date);
                if let Some(background) = star_gift.background {
                    gift.background = Some(StarGiftBackground::from_telegram(background));
                }
                if gift.is_auction && (gift.auction_slug.is_empty() || gift.gifts_per_round == 0) {
                    tdutils::log_error!("Receive invalid auctioned gift");
                }
                gift
            }
        }
    }

    /// `StarGift::is_valid` (StarGift.h:91-94).
    pub fn is_valid(&self) -> bool {
        self.id != 0
            && (if self.is_unique {
                self.model.is_valid() && self.pattern.is_valid() && self.backdrop.is_valid()
            } else {
                self.sticker_file_id.is_valid()
            })
    }

    /// `StarGift::is_unique` (StarGift.h:96-98).
    pub fn is_unique(&self) -> bool {
        self.is_unique
    }

    /// `StarGift::get_id` (StarGift.h:100-102).
    pub fn get_id(&self) -> i64 {
        self.id
    }

    /// `StarGift::get_star_count` (StarGift.h:104-107).
    pub fn get_star_count(&self) -> i64 {
        assert!(!self.is_unique);
        self.star_count
    }

    /// `StarGift::get_upgrade_star_count` (StarGift.h:109-112).
    pub fn get_upgrade_star_count(&self) -> i64 {
        assert!(!self.is_unique);
        self.upgrade_star_count
    }

    /// `StarGift::is_auction` (StarGift.h:114-116).
    pub fn is_auction(&self) -> bool {
        self.is_auction && !self.auction_slug.is_empty() && self.gifts_per_round > 0
    }

    /// `get_gift_purchase_limits_object` (StarGift.cpp:200-205).
    pub fn get_gift_purchase_limits_object(
        total: i32,
        remains: i32,
    ) -> Option<td_api::GiftPurchaseLimits> {
        if total <= 0 {
            return None;
        }
        Some(td_api::GiftPurchaseLimits {
            total_count: total,
            remaining_count: remains,
        })
    }

    /// `StarGift::get_gift_object` (StarGift.cpp:207-231).
    pub fn get_gift_object(
        &self,
        env: &mut impl StarGiftEnv,
        external_background: Option<&StarGiftBackground>,
    ) -> td_api::Gift {
        assert!(self.is_valid() && !self.is_unique);
        let mut gift_auction = None;
        if self.is_auction {
            if self.auction_slug.is_empty() || self.gifts_per_round == 0 {
                tdutils::log_error!("Receive auctioned gift without auction identifier");
            }
            gift_auction = Some(td_api::GiftAuction {
                id: self.auction_slug.clone(),
                gifts_per_round: self.gifts_per_round,
                start_date: self.auction_start_date,
            });
        }
        let mut background = self
            .background
            .as_ref()
            .map(|background| background.get_gift_background_object());
        if background.is_none()
            && let Some(external_background) = external_background
        {
            background = Some(external_background.get_gift_background_object());
        }
        td_api::Gift {
            id: self.id,
            publisher_chat_id: env.get_chat_id_object(self.released_by_dialog_id, "gift"),
            sticker: env.get_sticker_object(self.sticker_file_id),
            star_count: self.star_count,
            default_sell_star_count: self.default_sell_star_count,
            upgrade_star_count: self.upgrade_star_count,
            upgrade_variant_count: self.upgrade_variants,
            has_colors: self.has_colors,
            is_for_birthday: self.is_for_birthday,
            is_premium: self.is_premium,
            auction_info: gift_auction,
            next_send_date: self.locked_until_date,
            user_limits: Self::get_gift_purchase_limits_object(
                self.per_user_total,
                self.per_user_remains,
            ),
            overall_limits: Self::get_gift_purchase_limits_object(
                self.availability_total,
                self.availability_remains,
            ),
            background,
            first_send_date: self.first_sale_date,
            last_send_date: self.last_sale_date,
        }
    }

    /// `StarGift::get_upgraded_gift_object` (StarGift.cpp:233-253).
    pub fn get_upgraded_gift_object(&self, env: &mut impl StarGiftEnv) -> td_api::UpgradedGift {
        assert!(self.is_valid() && self.is_unique);
        let resale_parameters = if self.resale_star_count > 0 && self.resale_ton_count > 0 {
            Some(td_api::GiftResaleParameters {
                star_count: self.resale_star_count,
                gram_cent_count: self.resale_ton_count / 10_000_000,
                gram_only: self.resale_ton_only,
            })
        } else {
            None
        };
        td_api::UpgradedGift {
            id: self.id,
            regular_gift_id: self.regular_gift_id,
            publisher_chat_id: env
                .get_chat_id_object(self.released_by_dialog_id, "upgradedGift released by"),
            title: self.title.clone(),
            name: self.slug.clone(),
            number: self.num,
            total_upgraded_count: self.unique_availability_issued,
            max_upgraded_count: self.unique_availability_total,
            is_burned: self.is_burned,
            is_crafted: self.is_crafted,
            is_premium: self.is_premium,
            is_theme_available: self.is_theme_available,
            used_theme_chat_id: env.get_chat_id_object(self.theme_dialog_id, "upgradedGift theme"),
            host_id: if self.host_dialog_id.is_valid() {
                env.get_message_sender_object(self.host_dialog_id, "upgradedGift host")
            } else {
                None
            },
            owner_id: if self.owner_dialog_id.is_valid() {
                env.get_message_sender_object(self.owner_dialog_id, "upgradedGift owner")
            } else {
                None
            },
            owner_address: self.owner_address.clone(),
            owner_name: self.owner_name.clone(),
            gift_address: self.gift_address.clone(),
            model: Some(self.model.get_upgraded_gift_model_object(env)),
            symbol: Some(self.pattern.get_upgraded_gift_symbol_object(env)),
            backdrop: Some(self.backdrop.get_upgraded_gift_backdrop_object()),
            original_details: self
                .original_details
                .get_upgraded_gift_original_details_object(env),
            colors: self
                .peer_color
                .as_ref()
                .map(PeerColorCollectible::get_upgraded_gift_colors_object),
            resale_parameters,
            can_send_purchase_offer: self.offer_min_star_count > 0,
            craft_probability_per_mille: self.craft_chance_permille,
            value_currency: self.value_currency.clone(),
            value_amount: self.value_amount,
            value_usd_amount: self.value_usd_amount,
        }
    }

    /// `StarGift::get_gift_for_resale_object` (StarGift.cpp:255-262).
    pub fn get_gift_for_resale_object(&self, env: &mut impl StarGiftEnv) -> td_api::GiftForResale {
        td_api::GiftForResale {
            gift: Some(self.get_upgraded_gift_object(env)),
            received_gift_id: if self.owner_dialog_id == env.get_my_dialog_id() {
                StarGiftId::from_slug(&self.slug).get_star_gift_id()
            } else {
                String::new()
            },
        }
    }

    /// `StarGift::get_sent_gift_object` (StarGift.cpp:264-270).
    pub fn get_sent_gift_object(&self, env: &mut impl StarGiftEnv) -> td_api::SentGift {
        if self.is_unique {
            td_api::SentGift::sentGiftUpgraded(td_api::sentGiftUpgraded {
                gift: Some(self.get_upgraded_gift_object(env)),
            })
        } else {
            td_api::SentGift::sentGiftRegular(td_api::sentGiftRegular {
                gift: Some(self.get_gift_object(env, None)),
            })
        }
    }

    /// `StarGift::store` (StarGift.hpp:25-204) — byte-exact; the sticker
    /// and dialog rows ride the env.
    pub fn store(&self, env: &mut impl StarGiftEnv, out: &mut Vec<u8>) {
        assert!(self.is_valid());
        let is_limited = self.availability_total != 0;
        let has_default_sell_star_count =
            self.default_sell_star_count != self.star_count * 85 / 100;
        let has_first_sale_date = self.first_sale_date != 0;
        let has_last_sale_date = self.last_sale_date != 0;
        let has_original_details = self.original_details.is_valid();
        let has_upgrade_star_count = self.upgrade_star_count != 0;
        let has_owner_name = !self.owner_name.is_empty();
        let has_slug = !self.slug.is_empty();
        let has_owner_dialog_id = self.owner_dialog_id.is_valid();
        let has_owner_address = !self.owner_address.is_empty();
        let has_gift_address = !self.gift_address.is_empty();
        let has_resale_star_count = self.resale_star_count != 0;
        let has_released_by_dialog_id = self.released_by_dialog_id.is_valid();
        let has_per_user_remains = self.per_user_remains != 0;
        let has_per_user_total = self.per_user_total != 0;
        let has_resale_ton_count = self.resale_ton_count != 0;
        let has_regular_gift_id = self.regular_gift_id != 0;
        let has_value = !self.value_currency.is_empty();
        let has_locked_until_date = self.locked_until_date != 0;
        let has_theme_dialog_id = self.theme_dialog_id.is_valid();
        let has_host_dialog_id = self.host_dialog_id.is_valid();
        let has_peer_color = self.peer_color.is_some();
        let has_flags2 = true;
        let has_background = self.background.is_some();
        let has_auction_start_date = self.auction_start_date != 0;
        let has_upgrade_variants = self.upgrade_variants != 0;
        let has_usd_value = self.value_usd_amount != 0;
        let has_offer_min_star_count = self.offer_min_star_count != 0;
        let has_craft_chance_permille = self.craft_chance_permille != 0;
        store_parse::store_flags(
            out,
            [
                is_limited,
                has_default_sell_star_count,
                has_first_sale_date,
                has_last_sale_date,
                self.is_for_birthday,
                self.is_unique,
                has_original_details,
                false, // has_owner_user_id
                has_upgrade_star_count,
                has_owner_name,
                has_slug,
                has_owner_dialog_id,
                has_owner_address,
                has_gift_address,
                has_resale_star_count,
                has_released_by_dialog_id,
                self.is_premium,
                has_per_user_remains,
                has_per_user_total,
                self.resale_ton_only,
                has_resale_ton_count,
                has_regular_gift_id,
                has_value,
                has_locked_until_date,
                self.is_theme_available,
                has_theme_dialog_id,
                has_host_dialog_id,
                self.has_colors,
                has_peer_color,
                has_flags2,
            ],
        );
        store_parse::store_flags(
            out,
            [
                self.is_auction,
                has_background,
                has_auction_start_date,
                has_upgrade_variants,
                has_usd_value,
                has_offer_min_star_count,
                self.is_burned,
                self.is_crafted,
                has_craft_chance_permille,
            ],
        );
        store_parse::store_i64(out, self.id);
        if !self.is_unique {
            env.store_sticker(self.sticker_file_id, out);
            store_parse::store_i64(out, self.star_count);
        }
        if is_limited {
            store_parse::store_i32(out, self.availability_remains);
            store_parse::store_i32(out, self.availability_total);
        }
        if has_default_sell_star_count {
            store_parse::store_i64(out, self.default_sell_star_count);
        }
        if has_first_sale_date {
            store_parse::store_i32(out, self.first_sale_date);
        }
        if has_last_sale_date {
            store_parse::store_i32(out, self.last_sale_date);
        }
        if has_upgrade_star_count {
            store_parse::store_i64(out, self.upgrade_star_count);
        }
        if self.is_unique {
            self.model.store(env, out);
            self.pattern.store(env, out);
            self.backdrop.store(out);
            if has_original_details {
                self.original_details.store(out);
            }
            store_parse::store_string(out, &self.title);
            if has_owner_dialog_id {
                self.owner_dialog_id.store(out);
            }
            if has_owner_name {
                store_parse::store_string(out, &self.owner_name);
            }
            store_parse::store_i32(out, self.num);
            store_parse::store_i32(out, self.unique_availability_issued);
            store_parse::store_i32(out, self.unique_availability_total);
            if has_slug {
                store_parse::store_string(out, &self.slug);
            }
            if has_owner_address {
                store_parse::store_string(out, &self.owner_address);
            }
            if has_gift_address {
                store_parse::store_string(out, &self.gift_address);
            }
            if has_resale_star_count {
                store_parse::store_i64(out, self.resale_star_count);
            }
            if has_resale_ton_count {
                store_parse::store_i64(out, self.resale_ton_count);
            }
            if has_theme_dialog_id {
                self.theme_dialog_id.store(out);
            }
        }
        if has_released_by_dialog_id {
            self.released_by_dialog_id.store(out);
        }
        if has_per_user_remains {
            store_parse::store_i32(out, self.per_user_remains);
        }
        if has_per_user_total {
            store_parse::store_i32(out, self.per_user_total);
        }
        if has_regular_gift_id {
            store_parse::store_i64(out, self.regular_gift_id);
        }
        if has_value {
            store_parse::store_string(out, &self.value_currency);
            store_parse::store_i64(out, self.value_amount);
        }
        if has_locked_until_date {
            store_parse::store_i32(out, self.locked_until_date);
        }
        if has_host_dialog_id {
            self.host_dialog_id.store(out);
        }
        if has_peer_color {
            self.peer_color.as_ref().expect("checked").store(out);
        }
        if self.is_auction {
            store_parse::store_string(out, &self.auction_slug);
            store_parse::store_i32(out, self.gifts_per_round);
        }
        if has_background {
            self.background.as_ref().expect("checked").store(out);
        }
        if has_auction_start_date {
            store_parse::store_i32(out, self.auction_start_date);
        }
        if has_upgrade_variants {
            store_parse::store_i32(out, self.upgrade_variants);
        }
        if has_usd_value {
            store_parse::store_i64(out, self.value_usd_amount);
        }
        if has_offer_min_star_count {
            store_parse::store_i64(out, self.offer_min_star_count);
        }
        if has_craft_chance_permille {
            store_parse::store_i32(out, self.craft_chance_permille);
        }
    }

    /// `StarGift::parse` (StarGift.hpp:207-395) — the missing-flags2 and
    /// `has_owner_user_id` legacy quirks preserved.
    pub fn parse_from(
        env: &mut impl StarGiftEnv,
        parser: &mut Parser,
    ) -> Result<StarGift, &'static str> {
        let mut gift = StarGift::default();
        let [
            is_limited,
            has_default_sell_star_count,
            has_first_sale_date,
            has_last_sale_date,
            is_for_birthday,
            is_unique,
            has_original_details,
            has_owner_user_id,
            has_upgrade_star_count,
            has_owner_name,
            has_slug,
            has_owner_dialog_id,
            has_owner_address,
            has_gift_address,
            has_resale_star_count,
            has_released_by_dialog_id,
            is_premium,
            has_per_user_remains,
            has_per_user_total,
            resale_ton_only,
            has_resale_ton_count,
            has_regular_gift_id,
            has_value,
            has_locked_until_date,
            is_theme_available,
            has_theme_dialog_id,
            has_host_dialog_id,
            has_colors,
            has_peer_color,
            has_flags2,
        ] = parser.parse_flags()?;
        gift.is_for_birthday = is_for_birthday;
        gift.is_unique = is_unique;
        gift.is_premium = is_premium;
        gift.resale_ton_only = resale_ton_only;
        gift.is_theme_available = is_theme_available;
        gift.has_colors = has_colors;
        let [
            is_auction,
            has_background,
            has_auction_start_date,
            has_upgrade_variants,
            has_usd_value,
            has_offer_min_star_count,
            is_burned,
            is_crafted,
            has_craft_chance_permille,
        ] = if has_flags2 {
            parser.parse_flags()?
        } else {
            [false; 9]
        };
        gift.is_auction = is_auction;
        gift.is_burned = is_burned;
        gift.is_crafted = is_crafted;
        gift.id = parser.i64_()?;
        if !gift.is_unique {
            gift.sticker_file_id = env.parse_sticker(parser);
            gift.star_count = parser.i64_()?;
        }
        if is_limited {
            gift.availability_remains = parser.i32_()?;
            gift.availability_total = parser.i32_()?;
        }
        if has_default_sell_star_count {
            gift.default_sell_star_count = parser.i64_()?;
        } else {
            gift.default_sell_star_count = gift.star_count * 85 / 100;
        }
        if has_first_sale_date {
            gift.first_sale_date = parser.i32_()?;
        }
        if has_last_sale_date {
            gift.last_sale_date = parser.i32_()?;
        }
        if has_upgrade_star_count {
            gift.upgrade_star_count = parser.i64_()?;
        }
        if gift.is_unique {
            gift.model = StarGiftAttributeSticker::parse_from(env, parser)?;
            gift.pattern = StarGiftAttributeSticker::parse_from(env, parser)?;
            gift.backdrop = StarGiftAttributeBackdrop::parse_from(parser)?;
            if has_original_details {
                gift.original_details = StarGiftAttributeOriginalDetails::parse_from(parser)?;
            }
            gift.title = parser.string()?;
            if has_owner_user_id {
                let owner_user_id = UserId::new(parser.i64_()?);
                gift.owner_dialog_id = DialogId::from_user_id(owner_user_id);
            }
            if has_owner_dialog_id {
                gift.owner_dialog_id = DialogId::parse_from(parser)?;
            }
            if has_owner_name {
                gift.owner_name = parser.string()?;
            }
            gift.num = parser.i32_()?;
            gift.unique_availability_issued = parser.i32_()?;
            gift.unique_availability_total = parser.i32_()?;
            if has_slug {
                gift.slug = parser.string()?;
            }
            if has_owner_address {
                gift.owner_address = parser.string()?;
            }
            if has_gift_address {
                gift.gift_address = parser.string()?;
            }
            if has_resale_star_count {
                gift.resale_star_count = parser.i64_()?;
            }
            if has_resale_ton_count {
                gift.resale_ton_count = parser.i64_()?;
            }
            if has_theme_dialog_id {
                gift.theme_dialog_id = DialogId::parse_from(parser)?;
            }
        }
        if has_released_by_dialog_id {
            gift.released_by_dialog_id = DialogId::parse_from(parser)?;
        }
        if has_per_user_remains {
            gift.per_user_remains = parser.i32_()?;
        }
        if has_per_user_total {
            gift.per_user_total = parser.i32_()?;
        }
        if has_regular_gift_id {
            gift.regular_gift_id = parser.i64_()?;
        }
        if has_value {
            gift.value_currency = parser.string()?;
            gift.value_amount = parser.i64_()?;
        }
        if has_locked_until_date {
            gift.locked_until_date = parser.i32_()?;
        }
        if has_host_dialog_id {
            gift.host_dialog_id = DialogId::parse_from(parser)?;
        }
        if has_peer_color {
            gift.peer_color = Some(PeerColorCollectible::parse_from(parser)?);
        }
        if gift.is_auction {
            gift.auction_slug = parser.string()?;
            gift.gifts_per_round = parser.i32_()?;
        }
        if has_background {
            gift.background = Some(StarGiftBackground::parse_from(parser)?);
        }
        if has_auction_start_date {
            gift.auction_start_date = parser.i32_()?;
        }
        if has_upgrade_variants {
            gift.upgrade_variants = parser.i32_()?;
        }
        if has_usd_value {
            gift.value_usd_amount = parser.i64_()?;
        }
        if has_offer_min_star_count {
            gift.offer_min_star_count = parser.i64_()?;
        }
        if has_craft_chance_permille {
            gift.craft_chance_permille = parser.i32_()?;
        }
        Ok(gift)
    }
}

// --- UserStarGift (UserStarGift.{h,cpp}) ---

/// `td::UserStarGift` (UserStarGift.h:23-60).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UserStarGift {
    sender_dialog_id: DialogId,
    gift: StarGift,
    message: FormattedText,
    star_gift_id: StarGiftId,
    collection_ids: Vec<StarGiftCollectionId>,
    prepaid_upgrade_hash: String,
    convert_star_count: i64,
    upgrade_star_count: i64,
    transfer_star_count: i64,
    drop_original_details_star_count: i64,
    date: i32,
    can_transfer_at: i32,
    can_resell_at: i32,
    can_export_at: i32,
    gift_num: i32,
    can_craft_at: i32,
    is_name_hidden: bool,
    is_saved: bool,
    is_pinned: bool,
    can_upgrade: bool,
    can_transfer: bool,
    was_refunded: bool,
    is_upgrade_separate: bool,
}

/// The `savedStarGift::TRANSFER_STARS_MASK` (UserStarGift.cpp:41).
const SAVED_STAR_GIFT_TRANSFER_STARS_MASK: i32 = 1 << 0;

impl UserStarGift {
    /// `UserStarGift::UserStarGift(Td *, savedStarGift, DialogId)`
    /// (UserStarGift.cpp:23-79); the gift intake calls back into the
    /// manager like the C++ `td_->star_gift_manager_->on_get_star_gift`.
    pub fn from_telegram(
        env: &mut impl StarGiftEnv,
        manager: &mut StarGiftManager,
        gift: telegram_api::SavedStarGift,
        dialog_id: DialogId,
    ) -> UserStarGift {
        let mut result = UserStarGift {
            gift: StarGift::from_telegram(
                env,
                gift.gift
                    .unwrap_or(telegram_api::StarGift::starGift(telegram_api::starGift {
                        id: 0,
                        ..Default::default()
                    })),
                true,
            ),
            message: message_entity::get_formatted_text_of_text_with_entities::<()>(
                None,
                gift.message,
                true,
                false,
                "userStarGift",
            ),
            prepaid_upgrade_hash: gift.prepaid_upgrade_hash,
            // the flag-absent amount fields parse as 0, like the C++ reads
            convert_star_count: get_star_count(gift.convert_stars, false),
            upgrade_star_count: get_star_count(gift.upgrade_stars, false),
            transfer_star_count: get_star_count(gift.transfer_stars, false),
            drop_original_details_star_count: get_star_count(
                gift.drop_original_details_stars,
                false,
            ),
            date: gift.date,
            can_transfer_at: 0.max(gift.can_transfer_at),
            can_resell_at: 0.max(gift.can_resell_at),
            can_export_at: 0.max(gift.can_export_at),
            gift_num: 0.max(gift.gift_num),
            can_craft_at: 0.max(gift.can_craft_at),
            is_name_hidden: gift.name_hidden,
            is_saved: !gift.unsaved,
            is_pinned: gift.pinned_to_top,
            can_upgrade: gift.can_upgrade,
            can_transfer: gift.flags & SAVED_STAR_GIFT_TRANSFER_STARS_MASK != 0,
            was_refunded: gift.refunded,
            is_upgrade_separate: gift.upgrade_separate,
            ..UserStarGift::default()
        };
        if let Some(from_id) = &gift.from_id {
            result.sender_dialog_id = DialogId::from_peer(from_id);
            if !result.sender_dialog_id.is_valid() {
                tdutils::log_error!("Receive an invalid sender of the gift");
                result.sender_dialog_id = DialogId::default();
            }
        }
        for star_gift_collection_id in gift.collection_id {
            let collection_id = StarGiftCollectionId::new(star_gift_collection_id);
            if collection_id.is_valid() {
                result.collection_ids.push(collection_id);
            } else {
                tdutils::log_error!("Receive an invalid gift collection id");
            }
        }
        let is_user = dialog_id.get_type() == DialogType::User;
        let is_me = is_user && dialog_id == env.get_my_dialog_id();
        if is_user {
            if gift.msg_id != 0 {
                result.star_gift_id =
                    StarGiftId::from_server_message_id(ServerMessageId::new(gift.msg_id));
            }
        } else if gift.saved_id != 0 {
            result.star_gift_id = StarGiftId::from_dialog(dialog_id, gift.saved_id);
        }
        if result.sender_dialog_id != DialogId::default() && !result.sender_dialog_id.is_valid() {
            tdutils::log_error!("Receive an invalid sender of the gift");
            result.sender_dialog_id = DialogId::default();
        }
        if !result.is_saved && is_user && !is_me && !env.is_bot() {
            tdutils::log_error!("Receive a non-saved gift for a dialog");
            result.is_saved = true;
        }
        manager.on_get_star_gift(env, &result.gift, true);
        result
    }

    /// `UserStarGift::is_valid` (UserStarGift.h:51-53).
    pub fn is_valid(&self) -> bool {
        self.gift.is_valid()
            && (self.is_name_hidden || self.sender_dialog_id != DialogId::default())
    }

    /// `UserStarGift::is_unique` (UserStarGift.h:55-57).
    pub fn is_unique(&self) -> bool {
        self.gift.is_unique()
    }

    /// `get_received_gift_object` (UserStarGift.cpp:81-91).
    pub fn get_received_gift_object(&self, env: &mut impl StarGiftEnv) -> td_api::ReceivedGift {
        let collection_ids = self.collection_ids.iter().map(|id| id.get()).collect();
        td_api::ReceivedGift {
            received_gift_id: self.star_gift_id.get_star_gift_id(),
            sender_id: if self.sender_dialog_id == DialogId::default() {
                None
            } else {
                env.get_message_sender_object(self.sender_dialog_id, "receivedGift")
            },
            text: Some(message_entity::get_formatted_text_object::<()>(
                None,
                &self.message,
                true,
                -1,
            )),
            unique_gift_number: self.gift_num,
            is_private: self.is_name_hidden,
            is_saved: self.is_saved,
            is_pinned: self.is_pinned,
            can_be_upgraded: self.can_upgrade,
            can_be_transferred: self.can_transfer,
            was_refunded: self.was_refunded,
            date: self.date,
            gift: Some(self.gift.get_sent_gift_object(env)),
            collection_ids,
            sell_star_count: self.convert_star_count,
            prepaid_upgrade_star_count: self.upgrade_star_count,
            is_upgrade_separate: self.upgrade_star_count > 0 && self.is_upgrade_separate,
            transfer_star_count: self.transfer_star_count,
            drop_original_details_star_count: self.drop_original_details_star_count,
            next_transfer_date: self.can_transfer_at,
            next_resale_date: self.can_resell_at,
            export_date: self.can_export_at,
            prepaid_upgrade_hash: self.prepaid_upgrade_hash.clone(),
            craft_date: self.can_craft_at,
        }
    }
}

// --- StarGiftCollection (StarGiftCollection.{h,cpp}) ---

/// `td::StarGiftCollection` (StarGiftCollection.h:21-42).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StarGiftCollection {
    collection_id: StarGiftCollectionId,
    title: String,
    icon_file_id: FileId,
    gift_count: i32,
    hash: i64,
}

impl StarGiftCollection {
    /// `StarGiftCollection(Td *, starGiftCollection)`
    /// (StarGiftCollection.cpp:17-33) — the single-constructor TL type
    /// is a plain struct here.
    pub fn from_telegram(
        env: &mut impl StarGiftEnv,
        gift_collection: telegram_api::StarGiftCollection,
    ) -> StarGiftCollection {
        let collection_id = StarGiftCollectionId::new(gift_collection.collection_id);
        if !collection_id.is_valid() {
            tdutils::log_error!("Receive an invalid gift collection id");
            return StarGiftCollection::default();
        }
        StarGiftCollection {
            collection_id,
            title: gift_collection.title,
            icon_file_id: env
                .on_get_sticker_document(gift_collection.icon, "StarGiftCollection")
                .unwrap_or_default(),
            gift_count: 0.max(gift_collection.gifts_count),
            hash: gift_collection.hash,
        }
    }

    /// `StarGiftCollection::get_hash` (StarGiftCollection.h:37-39).
    pub fn get_hash(&self) -> i64 {
        self.hash
    }

    /// `get_gift_collection_object` (StarGiftCollection.cpp:35-38).
    pub fn get_gift_collection_object(&self, env: &mut impl StarGiftEnv) -> td_api::GiftCollection {
        td_api::GiftCollection {
            id: self.collection_id.get(),
            name: self.title.clone(),
            icon: env.get_sticker_object(self.icon_file_id),
            gift_count: self.gift_count,
        }
    }
}

// --- StarGiftId (StarGiftId.{h,cpp,hpp}) ---

/// `StarGiftId::Type` (StarGiftId.h:21).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StarGiftIdType {
    #[default]
    Empty = 0,
    ForUser = 1,
    ForDialog = 2,
    Slug = 3,
}

/// `td::StarGiftId` (StarGiftId.h:20-70).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StarGiftId {
    r#type: StarGiftIdType,
    server_message_id: ServerMessageId,
    dialog_id: DialogId,
    saved_id: i64,
    slug: String,
}

impl StarGiftId {
    /// `StarGiftId(ServerMessageId)` (StarGiftId.cpp:22-29).
    pub fn from_server_message_id(server_message_id: ServerMessageId) -> StarGiftId {
        let mut result = StarGiftId::default();
        if server_message_id.is_valid() {
            result.r#type = StarGiftIdType::ForUser;
            result.server_message_id = server_message_id;
        } else if server_message_id != ServerMessageId::default() {
            tdutils::log_error!("Receive server message {}", server_message_id.get());
        }
        result
    }

    /// `StarGiftId(DialogId, saved_id)` (StarGiftId.cpp:31-42).
    pub fn from_dialog(dialog_id: DialogId, saved_id: i64) -> StarGiftId {
        let mut result = StarGiftId::default();
        if dialog_id == DialogId::default() {
            return result;
        }
        if dialog_id.get_type() != DialogType::Channel || saved_id == 0 {
            tdutils::log_error!("Receive gift {saved_id} in dialog {}", dialog_id.get());
            return result;
        }
        result.r#type = StarGiftIdType::ForDialog;
        result.dialog_id = dialog_id;
        result.saved_id = saved_id;
        result
    }

    /// `StarGiftId::from_slug` (StarGiftId.cpp:44-49).
    pub fn from_slug(slug: &str) -> StarGiftId {
        StarGiftId {
            r#type: StarGiftIdType::Slug,
            slug: slug.to_owned(),
            ..StarGiftId::default()
        }
    }

    /// `StarGiftId(const string &)` (StarGiftId.cpp:51-72) — the
    /// round-trip check resets to empty on mismatch.
    pub fn from_string(star_gift_id: &str) -> StarGiftId {
        let mut result = StarGiftId::default();
        if star_gift_id.is_empty() {
            return result;
        }
        if let Some(slug) = star_gift_id.strip_prefix('@') {
            result.r#type = StarGiftIdType::Slug;
            result.slug = slug.to_owned();
        } else if let Some(underscore_pos) = star_gift_id.find('_') {
            result.r#type = StarGiftIdType::ForDialog;
            result.dialog_id = DialogId::new(parse_i64_prefix(&star_gift_id[..underscore_pos]));
            result.saved_id = parse_i64_prefix(&star_gift_id[underscore_pos + 1..]);
        } else {
            result.r#type = StarGiftIdType::ForUser;
            result.server_message_id = ServerMessageId::new(parse_i32_prefix(star_gift_id));
        }
        if result.get_star_gift_id() != star_gift_id {
            return StarGiftId::default();
        }
        result
    }

    /// `get_star_gift_ids` (StarGiftId.cpp:101-103).
    pub fn get_star_gift_ids(star_gift_ids: &[String]) -> Vec<StarGiftId> {
        star_gift_ids
            .iter()
            .map(|id| StarGiftId::from_string(id))
            .collect()
    }

    /// `StarGiftId::is_empty` (StarGiftId.h:46-48).
    pub fn is_empty(&self) -> bool {
        self.r#type == StarGiftIdType::Empty
    }

    /// `StarGiftId::is_valid` (StarGiftId.h:50-52).
    pub fn is_valid(&self) -> bool {
        self.r#type != StarGiftIdType::Empty
    }

    /// `get_input_saved_star_gift` (StarGiftId.cpp:74-93) — the ForDialog
    /// input-peer lookup rides the env; `None` is the C++ nullptr.
    pub fn get_input_saved_star_gift(
        &self,
        env: &mut impl StarGiftEnv,
    ) -> Option<telegram_api::InputSavedStarGift> {
        match self.r#type {
            StarGiftIdType::Empty => None,
            StarGiftIdType::ForUser => {
                Some(telegram_api::InputSavedStarGift::inputSavedStarGiftUser(
                    telegram_api::inputSavedStarGiftUser {
                        msg_id: self.server_message_id.get(),
                    },
                ))
            }
            StarGiftIdType::ForDialog => {
                let input_peer = env.get_input_peer(self.dialog_id)?;
                Some(telegram_api::InputSavedStarGift::inputSavedStarGiftChat(
                    telegram_api::inputSavedStarGiftChat {
                        peer: Some(Box::new(input_peer)),
                        saved_id: self.saved_id,
                    },
                ))
            }
            StarGiftIdType::Slug => Some(telegram_api::InputSavedStarGift::inputSavedStarGiftSlug(
                telegram_api::inputSavedStarGiftSlug {
                    slug: self.slug.clone(),
                },
            )),
        }
    }

    /// `get_input_saved_star_gifts` (StarGiftId.cpp:95-99) — the C++
    /// keeps nullptr entries (transform over possibly-null results); the
    /// port keeps them as the first None.
    pub fn get_input_saved_star_gifts(
        env: &mut impl StarGiftEnv,
        star_gift_ids: &[StarGiftId],
    ) -> Vec<Option<telegram_api::InputSavedStarGift>> {
        star_gift_ids
            .iter()
            .map(|star_gift_id| star_gift_id.get_input_saved_star_gift(env))
            .collect()
    }

    /// `get_star_gift_id` (StarGiftId.cpp:105-119).
    pub fn get_star_gift_id(&self) -> String {
        match self.r#type {
            StarGiftIdType::Empty => String::new(),
            StarGiftIdType::ForUser => self.server_message_id.get().to_string(),
            StarGiftIdType::ForDialog => format!("{}_{}", self.dialog_id.get(), self.saved_id),
            StarGiftIdType::Slug => format!("@{}", self.slug),
        }
    }

    /// `get_dialog_id` (StarGiftId.cpp:121-135).
    pub fn get_dialog_id(&self, env: &impl StarGiftEnv) -> DialogId {
        match self.r#type {
            StarGiftIdType::Empty | StarGiftIdType::Slug => DialogId::default(),
            StarGiftIdType::ForUser => env.get_my_dialog_id(),
            StarGiftIdType::ForDialog => self.dialog_id,
        }
    }

    /// `StarGiftId::store` (StarGiftId.hpp:17-42).
    pub fn store(&self, out: &mut Vec<u8>) {
        assert!(self.is_valid());
        let has_server_message_id = self.server_message_id.is_valid();
        let has_dialog_id = self.dialog_id.is_valid();
        let has_saved_id = self.saved_id != 0;
        let has_slug = !self.slug.is_empty();
        store_parse::store_flags(
            out,
            [has_server_message_id, has_dialog_id, has_saved_id, has_slug],
        );
        store_parse::store_i32(out, self.r#type as i32);
        if has_server_message_id {
            store_parse::store_i32(out, self.server_message_id.get());
        }
        if has_dialog_id {
            self.dialog_id.store(out);
        }
        if has_saved_id {
            store_parse::store_i64(out, self.saved_id);
        }
        if has_slug {
            store_parse::store_string(out, &self.slug);
        }
    }

    /// `StarGiftId::parse` (StarGiftId.hpp:45-69).
    pub fn parse_from(parser: &mut Parser) -> Result<StarGiftId, &'static str> {
        let [has_server_message_id, has_dialog_id, has_saved_id, has_slug] =
            parser.parse_flags()?;
        let mut result = StarGiftId {
            r#type: match parser.i32_()? {
                0 => StarGiftIdType::Empty,
                1 => StarGiftIdType::ForUser,
                2 => StarGiftIdType::ForDialog,
                3 => StarGiftIdType::Slug,
                _ => return Err("Invalid star gift id type"),
            },
            ..StarGiftId::default()
        };
        if has_server_message_id {
            result.server_message_id = ServerMessageId::new(parser.i32_()?);
        }
        if has_dialog_id {
            result.dialog_id = DialogId::parse_from(parser)?;
        }
        if has_saved_id {
            result.saved_id = parser.i64_()?;
        }
        if has_slug {
            result.slug = parser.string()?;
        }
        Ok(result)
    }
}

/// `to_integer<int32>` (tdutils) — the digit prefix parse (with the
/// leading `-` form) the StarGiftId string ctor rides; a non-numeric
/// prefix parses as 0 like `to_integer`.
fn parse_i32_prefix(s: &str) -> i32 {
    let (negative, rest) = if let Some(rest) = s.strip_prefix('-') {
        (true, rest)
    } else {
        (false, s)
    };
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return 0;
    }
    let value: i64 = digits.parse().unwrap_or(0);
    (if negative { -value } else { value }) as i32
}

/// `to_integer<int64>` (tdutils).
fn parse_i64_prefix(s: &str) -> i64 {
    let (negative, rest) = if let Some(rest) = s.strip_prefix('-') {
        (true, rest)
    } else {
        (false, s)
    };
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return 0;
    }
    let value: i64 = digits.parse().unwrap_or(0);
    if negative { -value } else { value }
}

// --- the auction plane (AuctionBidLevel.{h,cpp}, StarGiftAuctionRound,
// StarGiftAuctionState, StarGiftAuctionUserState) ---

/// `td::AuctionBidLevel` (AuctionBidLevel.h:17-35).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AuctionBidLevel {
    position: i32,
    star_count: i64,
    date: i32,
}

impl AuctionBidLevel {
    /// `AuctionBidLevel::AuctionBidLevel(auctionBidLevel)`
    /// (AuctionBidLevel.cpp:11-16) — the single-constructor TL type is a
    /// plain struct here.
    pub fn from_telegram(bid_level: &telegram_api::AuctionBidLevel) -> AuctionBidLevel {
        AuctionBidLevel {
            position: bid_level.pos,
            star_count: get_star_count(bid_level.amount, false),
            date: bid_level.date,
        }
    }

    /// `AuctionBidLevel::is_before` (AuctionBidLevel.cpp:18-21).
    fn is_before(&self, other: &AuctionBidLevel) -> bool {
        self.position < other.position
            && (self.star_count > other.star_count
                || (self.star_count == other.star_count && self.date <= other.date))
    }

    /// `get_auction_bid_levels` (AuctionBidLevel.cpp:23-39).
    pub fn get_auction_bid_levels(
        bid_levels: &[telegram_api::AuctionBidLevel],
    ) -> Vec<AuctionBidLevel> {
        let mut result: Vec<AuctionBidLevel> = Vec::with_capacity(bid_levels.len());
        for bid_level in bid_levels {
            let level = AuctionBidLevel::from_telegram(bid_level);
            if result.is_empty() || result.last().expect("checked").is_before(&level) {
                result.push(level);
            }
        }
        if result.len() != bid_levels.len() {
            tdutils::log_error!("Receive unsorted bid levels");
        }
        result
    }

    /// `get_auction_bid_object` (AuctionBidLevel.cpp:41-43).
    pub fn get_auction_bid_object(&self) -> td_api::AuctionBid {
        td_api::AuctionBid {
            star_count: self.star_count,
            bid_date: self.date,
            position: self.position,
        }
    }
}

/// `td::StarGiftAuctionRound` (StarGiftAuctionRound.h:16-30).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StarGiftAuctionRound {
    num: i32,
    duration: i32,
    extend_top: i32,
    extend_window: i32,
}

impl StarGiftAuctionRound {
    /// `StarGiftAuctionRound(StarGiftAuctionRound)`
    /// (StarGiftAuctionRound.cpp:11-32).
    pub fn from_telegram(round: &telegram_api::StarGiftAuctionRound) -> StarGiftAuctionRound {
        match round {
            telegram_api::StarGiftAuctionRound::starGiftAuctionRound(round) => {
                StarGiftAuctionRound {
                    num: round.num,
                    duration: round.duration,
                    ..StarGiftAuctionRound::default()
                }
            }
            telegram_api::StarGiftAuctionRound::starGiftAuctionRoundExtendable(round) => {
                StarGiftAuctionRound {
                    num: round.num,
                    duration: round.duration,
                    extend_top: round.extend_top,
                    extend_window: round.extend_window,
                }
            }
        }
    }

    /// `get_auction_round_object` (StarGiftAuctionRound.cpp:34-36).
    pub fn get_auction_round_object(&self) -> td_api::AuctionRound {
        td_api::AuctionRound {
            number: self.num,
            duration: self.duration,
            extend_time: self.extend_window,
            top_winner_count: self.extend_top,
        }
    }
}

/// `td::StarGiftAuctionUserState` (StarGiftAuctionUserState.h:19-47).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StarGiftAuctionUserState {
    bid_amount: i64,
    bid_date: i32,
    min_bid_amount: i64,
    bid_dialog_id: DialogId,
    was_returned: bool,
    acquired_count: i32,
}

impl StarGiftAuctionUserState {
    /// `StarGiftAuctionUserState(starGiftAuctionUserState)`
    /// (StarGiftAuctionUserState.cpp:14-25) — the single-constructor TL
    /// type is a plain struct here.
    pub fn from_telegram(state: &telegram_api::StarGiftAuctionUserState) -> Self {
        StarGiftAuctionUserState {
            bid_amount: get_star_count(state.bid_amount, false),
            bid_date: 0.max(state.bid_date),
            min_bid_amount: get_star_count(state.min_bid_amount, false),
            bid_dialog_id: state
                .bid_peer
                .as_ref()
                .map(DialogId::from_peer)
                .unwrap_or_default(),
            was_returned: state.returned,
            acquired_count: state.acquired_count,
        }
    }

    /// `get_user_auction_bid_object` (StarGiftAuctionUserState.cpp:27-34).
    pub fn get_user_auction_bid_object(
        &self,
        env: &mut impl StarGiftEnv,
    ) -> Option<td_api::UserAuctionBid> {
        if self.bid_amount == 0 || self.bid_date == 0 || !self.bid_dialog_id.is_valid() {
            return None;
        }
        Some(td_api::UserAuctionBid {
            star_count: self.bid_amount,
            bid_date: self.bid_date,
            next_bid_star_count: self.min_bid_amount,
            owner_id: env.get_message_sender_object(self.bid_dialog_id, "userAuctionBid"),
            was_returned: self.was_returned,
        })
    }

    /// `StarGiftAuctionUserState::get_bid_amount`
    /// (StarGiftAuctionUserState.h:36-38).
    pub fn get_bid_amount(&self) -> i64 {
        self.bid_amount
    }

    /// `StarGiftAuctionUserState::get_acquired_count`
    /// (StarGiftAuctionUserState.h:40-42).
    pub fn get_acquired_count(&self) -> i32 {
        self.acquired_count
    }

    /// `StarGiftAuctionUserState::is_active`
    /// (StarGiftAuctionUserState.h:44-46).
    pub fn is_active(&self) -> bool {
        self.bid_dialog_id != DialogId::default()
    }
}

/// `td::StarGiftAuctionState` (StarGiftAuctionState.h:22-63).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StarGiftAuctionState {
    is_not_modified: bool,
    is_active: bool,
    start_date: i32,
    end_date: i32,
    // active
    version: i32,
    min_bid_amount: i64,
    bid_levels: Vec<AuctionBidLevel>,
    top_bidder_user_ids: Vec<UserId>,
    rounds: Vec<StarGiftAuctionRound>,
    next_round_at: i32,
    last_gift_num: i32,
    gifts_left: i32,
    current_round: i32,
    total_rounds: i32,
    // finished
    average_price: i64,
    listed_count: i32,
    fragment_listed_count: i32,
    fragment_listed_url: String,
}

impl StarGiftAuctionState {
    /// `StarGiftAuctionState(StarGiftAuctionState)`
    /// (StarGiftAuctionState.cpp:20-80).
    pub fn from_telegram(state: &telegram_api::StarGiftAuctionState) -> Self {
        let mut result = StarGiftAuctionState::default();
        match state {
            telegram_api::StarGiftAuctionState::starGiftAuctionStateNotModified(_) => {
                // the C++ `is_not_modified_ = false` quirk (the flag is
                // never set true by this constructor; see the recorded
                // divergences)
                result.is_not_modified = false;
            }
            telegram_api::StarGiftAuctionState::starGiftAuctionState(state) => {
                result.is_active = true;
                result.start_date = state.start_date;
                result.end_date = state.end_date;
                result.version = state.version;
                result.min_bid_amount = get_star_count(state.min_bid_amount, false);
                result.bid_levels = AuctionBidLevel::get_auction_bid_levels(&state.bid_levels);
                for top_bidder in &state.top_bidders {
                    let top_bidder_user_id = UserId::new(*top_bidder);
                    if !top_bidder_user_id.is_valid() {
                        tdutils::log_error!("Receive an invalid top bidder");
                    } else {
                        result.top_bidder_user_ids.push(top_bidder_user_id);
                    }
                }
                const MAX_BIDDER_COUNT: usize = 3;
                if result.top_bidder_user_ids.len() > MAX_BIDDER_COUNT {
                    tdutils::log_error!("Receive too many top bidders");
                    result.top_bidder_user_ids.truncate(MAX_BIDDER_COUNT);
                }
                for round in &state.rounds {
                    result
                        .rounds
                        .push(StarGiftAuctionRound::from_telegram(round));
                }
                result.next_round_at = state.next_round_at;
                result.last_gift_num = state.last_gift_num;
                result.gifts_left = state.gifts_left;
                result.current_round = state.current_round;
                result.total_rounds = state.total_rounds;
                if result.total_rounds <= 0 {
                    tdutils::log_error!("Receive invalid total rounds");
                    result.total_rounds = 1;
                }
                if result.current_round <= 0 || result.current_round > result.total_rounds {
                    tdutils::log_error!("Receive round out of range");
                    result.current_round = result.current_round.clamp(1, result.total_rounds);
                }
            }
            telegram_api::StarGiftAuctionState::starGiftAuctionStateFinished(state) => {
                result.is_active = false;
                result.start_date = state.start_date;
                result.end_date = state.end_date;
                result.average_price = get_star_count(state.average_price, false);
                result.listed_count = 0.max(state.listed_count);
                result.fragment_listed_count = 0.max(state.fragment_listed_count);
                result.fragment_listed_url = state.fragment_listed_url.clone();
            }
        }
        result
    }

    /// `StarGiftAuctionState::is_not_modified`
    /// (StarGiftAuctionState.h:53-55).
    pub fn is_not_modified(&self) -> bool {
        self.is_not_modified
    }

    /// `StarGiftAuctionState::get_version` (StarGiftAuctionState.h:57-59).
    pub fn get_version(&self) -> i32 {
        self.version
    }

    /// `get_auction_state_object` (StarGiftAuctionState.cpp:82-99).
    pub fn get_auction_state_object(
        &self,
        env: &mut impl StarGiftEnv,
        user_state: &StarGiftAuctionUserState,
    ) -> td_api::AuctionState {
        if self.is_active {
            let bid_levels = self
                .bid_levels
                .iter()
                .map(AuctionBidLevel::get_auction_bid_object)
                .collect();
            let top_bidder_user_ids =
                env.get_user_ids_object(&self.top_bidder_user_ids, "auctionStateActive");
            let rounds = self
                .rounds
                .iter()
                .map(StarGiftAuctionRound::get_auction_round_object)
                .collect();
            td_api::AuctionState::auctionStateActive(td_api::auctionStateActive {
                start_date: self.start_date,
                end_date: self.end_date,
                min_bid: self.min_bid_amount,
                bid_levels,
                top_bidder_user_ids,
                rounds,
                current_round_end_date: self.next_round_at,
                current_round_number: self.current_round,
                total_round_count: self.total_rounds,
                distributed_item_count: self.last_gift_num,
                left_item_count: self.gifts_left,
                acquired_item_count: user_state.get_acquired_count(),
                user_bid: user_state.get_user_auction_bid_object(env),
            })
        } else {
            td_api::AuctionState::auctionStateFinished(td_api::auctionStateFinished {
                start_date: self.start_date,
                end_date: self.end_date,
                average_price: self.average_price,
                acquired_item_count: user_state.get_acquired_count(),
                telegram_listed_item_count: self.listed_count,
                fragment_listed_item_count: self.fragment_listed_count,
                fragment_url: self.fragment_listed_url.clone(),
            })
        }
    }
}

// --- the get_message_sender_dialog_id helper (MessageSender.cpp:173-213)
// — the MessageSender-plane home is its own track; the star gift request
// arms need it now ---

/// `get_message_sender_dialog_id(td, message_sender_id, true, false)` —
/// the check_access = true, allow_empty = false form every star gift arm
/// uses. `have_dialog_force` halves are the M.6.15a stubs.
pub fn get_message_sender_dialog_id(
    env: &mut impl StarGiftEnv,
    message_sender_id: Option<&td_api::MessageSender>,
) -> Result<DialogId, Status> {
    let Some(message_sender_id) = message_sender_id else {
        return Err(Status::error(400, "Message sender must be non-empty"));
    };
    match message_sender_id {
        td_api::MessageSender::messageSenderUser(sender) => {
            let user_id = UserId::new(sender.user_id);
            if !user_id.is_valid() {
                return Err(Status::error(400, "Invalid user identifier specified"));
            }
            let know_user = env.have_user_force(user_id, "get_message_sender_dialog_id");
            if !know_user {
                return Err(Status::error(400, "Unknown user identifier specified"));
            }
            Ok(DialogId::from_user_id(user_id))
        }
        td_api::MessageSender::messageSenderChat(sender) => {
            let dialog_id = DialogId::new(sender.chat_id);
            if !dialog_id.is_valid() {
                return Err(Status::error(400, "Invalid chat identifier specified"));
            }
            let know_dialog = if dialog_id.get_type() == DialogType::User {
                env.have_user_force(dialog_id.get_user_id(), "get_message_sender_dialog_id 2")
            } else {
                env.have_dialog_force(dialog_id, "get_message_sender_dialog_id")
            };
            if !know_dialog {
                return Err(Status::error(400, "Unknown chat identifier specified"));
            }
            Ok(dialog_id)
        }
    }
}

// --- the env ---

/// The `G()`/`td_` surface of the manager. Every server-bound query send
/// is a `[blocked: net]` seam that logs the query name; the answer halves
/// that only need the value plane are real manager methods.
#[allow(clippy::too_many_arguments)] // the C++ request signatures
pub trait StarGiftEnv {
    /// `td_->auth_manager_->is_authorized()`.
    fn is_authorized(&self) -> bool;

    /// `td_->auth_manager_->is_bot()`.
    fn is_bot(&self) -> bool;

    /// `G()->unix_time()`.
    fn unix_time(&self) -> i32;

    /// `td_->option_manager_->get_option_integer(name)` (the C++ default 0).
    fn get_option_integer(&mut self, name: &str) -> i64;

    /// `td_->option_manager_->get_option_boolean(name)` (the C++ default
    /// false) — the `always_parse_markdown` input of the formatted-text
    /// parses.
    fn get_option_boolean(&mut self, name: &str) -> bool {
        let _ = name;
        false
    }

    /// `td_->option_manager_->get_option_string(name, default)`.
    fn get_option_string(&mut self, name: &str, default_value: &str) -> String;

    /// `send_closure(G()->td(), &Td::send_update, ...)` — drained by
    /// `crate::td::Td::flush_user_updates`.
    fn send_update(&mut self, update: td_api::Update);

    /// `td_->user_manager_->on_get_users(users, source)`.
    fn on_get_users(&mut self, users: Vec<telegram_api::User>, source: &str);

    /// `td_->chat_manager_->on_get_chats(chats, source)`.
    fn on_get_chats(&mut self, chats: Vec<telegram_api::Chat>, source: &str);

    // -- UserManager read surface (real in the td wiring) --

    /// `td_->user_manager_->have_user_force(user_id, source)`.
    fn have_user_force(&mut self, user_id: UserId, source: &str) -> bool;

    /// `td_->user_manager_->get_user_id_object(user_id, source)`.
    fn get_user_id_object(&mut self, user_id: UserId, source: &str) -> i64;

    /// `td_->user_manager_->get_user_ids_object(user_ids, source)`.
    fn get_user_ids_object(&mut self, user_ids: &[UserId], source: &str) -> Vec<i64>;

    /// `td_->user_manager_->on_update_my_gift_count(delta)`.
    fn on_update_my_gift_count(&mut self, delta: i32);

    /// `td_->user_manager_->on_update_user_gift_count(user_id, count)`.
    fn on_update_user_gift_count(&mut self, user_id: UserId, count: i32);

    // -- DialogManager (the M.6.15a surface; chat halves are the stubs) --

    /// `td_->dialog_manager_->get_my_dialog_id()`.
    fn get_my_dialog_id(&self) -> DialogId;

    /// `td_->dialog_manager_->force_create_dialog(dialog_id, source, true)`.
    fn force_create_dialog(&mut self, dialog_id: DialogId, source: &str);

    /// `td_->dialog_manager_->check_dialog_access(dialog_id, false,
    /// AccessRights::Read, source)`.
    fn check_dialog_access(&mut self, dialog_id: DialogId, source: &str) -> Result<(), Status>;

    /// `td_->dialog_manager_->get_input_peer(dialog_id, AccessRights::Read)`.
    fn get_input_peer(&mut self, dialog_id: DialogId) -> Option<telegram_api::InputPeer>;

    /// `td_->dialog_manager_->have_dialog_force(dialog_id, source)`.
    fn have_dialog_force(&mut self, dialog_id: DialogId, source: &str) -> bool;

    /// `td_->dialog_manager_->get_chat_id_object(dialog_id, source)`.
    fn get_chat_id_object(&mut self, dialog_id: DialogId, source: &str) -> i64;

    /// `td_->dialog_manager_->is_broadcast_channel(dialog_id)`.
    fn is_broadcast_channel(&mut self, dialog_id: DialogId) -> bool;

    /// `td_->chat_manager_->get_channel_status(channel_id).
    /// can_post_messages()`.
    fn can_channel_post_messages(&mut self, channel_id: ChannelId) -> bool;

    /// `td_->chat_manager_->on_update_channel_gift_count(channel_id, count,
    /// is_relax)`.
    fn on_update_channel_gift_count(&mut self, channel_id: ChannelId, count: i32, is_relax: bool);

    // -- MessageSender/MessagesManager planes --

    /// `get_message_sender_object(td, dialog_id, source)` — the
    /// MessagesManager-backed sender cache; `None` is the unmerged-cache
    /// answer ([blocked: M.6]).
    fn get_message_sender_object(
        &mut self,
        dialog_id: DialogId,
        source: &str,
    ) -> Option<td_api::MessageSender> {
        let _ = (dialog_id, source);
        tdutils::log_info!("get_message_sender_object — [blocked: M.6 MessagesManager]");
        None
    }

    // -- StickersManager ([blocked: the sticker/file plane]) --

    /// `td_->stickers_manager_->on_get_sticker_document(document,
    /// StickerFormat::Unknown, source).second` — registers the document
    /// and answers the file id; `None` models the invalid-document bail.
    fn on_get_sticker_document(
        &mut self,
        document: Option<telegram_api::Document>,
        source: &str,
    ) -> Option<FileId> {
        let _ = source;
        match document {
            Some(_) => {
                tdutils::log_info!("on_get_sticker_document — [blocked: the sticker plane]");
                None
            }
            None => None,
        }
    }

    /// `td_->stickers_manager_->get_sticker_object(file_id)`.
    fn get_sticker_object(&mut self, file_id: FileId) -> Option<td_api::Sticker> {
        let _ = file_id;
        tdutils::log_info!("get_sticker_object — [blocked: the sticker plane]");
        None
    }

    /// `td_->stickers_manager_->get_sticker_id(file_id)`.
    fn get_sticker_id(&mut self, file_id: FileId) -> i64 {
        let _ = file_id;
        tdutils::log_info!("get_sticker_id — [blocked: the sticker plane]");
        0
    }

    /// `td_->stickers_manager_->store_sticker(file_id, false, storer,
    /// "StarGift")` — `false` models the unresolvable row until the
    /// sticker plane lands.
    fn store_sticker(&mut self, file_id: FileId, out: &mut Vec<u8>) {
        let _ = (file_id, out);
        tdutils::log_info!("store_sticker — [blocked: the sticker plane]");
    }

    /// `td_->stickers_manager_->parse_sticker(false, parser)`.
    fn parse_sticker(&mut self, parser: &mut Parser) -> FileId {
        let _ = parser;
        tdutils::log_info!("parse_sticker — [blocked: the sticker plane]");
        FileId::default()
    }

    // -- StarManager (the star balance plane) --

    /// `td_->star_manager_->has_owned_star_count(star_count)`.
    fn has_owned_star_count(&mut self, star_count: i64) -> bool {
        let _ = star_count;
        tdutils::log_info!("has_owned_star_count — [blocked: StarManager]");
        true
    }

    /// `td_->star_manager_->add_pending_owned_star_count(delta,
    /// is_successful)`.
    fn add_pending_owned_star_count(&mut self, star_count: i64, is_successful: bool) {
        let _ = (star_count, is_successful);
        tdutils::log_info!("add_pending_owned_star_count — [blocked: StarManager]");
    }

    /// `td_->star_manager_->has_owned_amount(price)`.
    fn has_owned_amount(&mut self, price: &StarGiftResalePrice) -> bool {
        let _ = price;
        tdutils::log_info!("has_owned_amount — [blocked: StarManager]");
        true
    }

    /// `td_->star_manager_->add_pending_owned_amount(price, count,
    /// is_successful)`.
    fn add_pending_owned_amount(
        &mut self,
        price: &StarGiftResalePrice,
        count: i64,
        is_successful: bool,
    ) {
        let _ = (price, count, is_successful);
        tdutils::log_info!("add_pending_owned_amount — [blocked: StarManager]");
    }

    /// `td_->star_manager_->reload_owned_star_count()`.
    fn reload_owned_star_count(&mut self) {
        tdutils::log_info!("reload_owned_star_count — [blocked: StarManager]");
    }

    // -- PasswordManager --

    /// `send_closure(td_->password_manager_,
    /// &PasswordManager::get_input_check_password_srp, password, ...)`;
    /// `None` parks the promise like the un-answered callback
    /// ([blocked: the password plane]).
    fn get_input_check_password_srp(
        &mut self,
        password: &str,
    ) -> Option<telegram_api::InputCheckPasswordSRP> {
        let _ = password;
        tdutils::log_info!("get_input_check_password_srp — [blocked: the password plane]");
        None
    }

    // -- BusinessConnectionManager --

    /// `td_->business_connection_manager_->check_business_connection(id)`.
    fn check_business_connection(
        &mut self,
        business_connection_id: &BusinessConnectionId,
    ) -> Result<(), Status> {
        let _ = business_connection_id;
        tdutils::log_info!("check_business_connection — [blocked: BusinessConnectionManager]");
        Err(Status::error(400, "Business connection not found"))
    }

    /// `td_->business_connection_manager_->get_business_connection_user_id(id)`.
    fn get_business_connection_user_id(
        &mut self,
        business_connection_id: &BusinessConnectionId,
    ) -> UserId {
        let _ = business_connection_id;
        tdutils::log_info!(
            "get_business_connection_user_id — [blocked: BusinessConnectionManager]"
        );
        UserId::default()
    }

    // -- the [blocked: net] query seams; the promise parks in the manager --

    /// `GetStarGiftsQuery::send` (StarGiftManager.cpp:59-61).
    fn send_get_star_gifts_query(&mut self) {
        tdutils::log_info!("Send payments.getStarGifts — [blocked: net]");
    }

    /// `CheckCanSendGiftQuery::send` (StarGiftManager.cpp:146-148).
    fn send_check_can_send_gift_query(&mut self, gift_id: i64) {
        tdutils::log_info!("Send payments.checkCanSendGift(gift_id = {gift_id}) — [blocked: net]");
    }

    /// `GetGiftPaymentFormQuery::send` (StarGiftManager.cpp:234-241) —
    /// the send-gift payment chain head; the invoice bits ride the log
    /// until the net step carries the real payload.
    fn send_get_gift_payment_form_query(
        &mut self,
        gift_id: i64,
        star_count: i64,
        is_private: bool,
        pay_for_upgrade: bool,
        has_message: bool,
    ) {
        tdutils::log_info!(
            "Send payments.getPaymentForm for the star gift {gift_id} (star_count = {star_count}, \
             is_private = {is_private}, pay_for_upgrade = {pay_for_upgrade}, has_message = {has_message}) — [blocked: net]"
        );
    }

    /// `GetStarGiftAuctionStateQuery::send`
    /// (StarGiftManager.cpp:404-407).
    fn send_get_star_gift_auction_state_query(
        &mut self,
        auction: &telegram_api::InputStarGiftAuction,
        version: i32,
    ) {
        let target = match auction {
            telegram_api::InputStarGiftAuction::inputStarGiftAuction(auction) => {
                format!("gift_id = {}", auction.gift_id)
            }
            telegram_api::InputStarGiftAuction::inputStarGiftAuctionSlug(auction) => {
                format!("slug = {}", auction.slug)
            }
        };
        tdutils::log_info!(
            "Send payments.getStarGiftAuctionState({target}, version = {version}) — [blocked: net]"
        );
    }

    /// `GetStarGiftActiveAuctionsQuery::send` (StarGiftManager.cpp:434-436).
    fn send_get_star_gift_active_auctions_query(&mut self) {
        tdutils::log_info!("Send payments.getStarGiftActiveAuctions — [blocked: net]");
    }

    /// `GetStarGiftAuctionAcquiredGiftsQuery::send`
    /// (StarGiftManager.cpp:462-464).
    fn send_get_star_gift_auction_acquired_gifts_query(&mut self, gift_id: i64) {
        tdutils::log_info!(
            "Send payments.getStarGiftAuctionAcquiredGifts(gift_id = {gift_id}) — [blocked: net]"
        );
    }

    /// `ConvertStarGiftQuery::send` (StarGiftManager.cpp:502-509).
    fn send_convert_star_gift_query(&mut self, star_gift_id: &StarGiftId) {
        let _ = star_gift_id;
        tdutils::log_info!("Send payments.convertStarGift — [blocked: net]");
    }

    /// `SaveStarGiftQuery::send` (StarGiftManager.cpp:542-548).
    fn send_save_star_gift_query(&mut self, star_gift_id: &StarGiftId, is_saved: bool) {
        let _ = star_gift_id;
        tdutils::log_info!("Send payments.saveStarGift(is_saved = {is_saved}) — [blocked: net]");
    }

    /// `ToggleStarGiftsPinnedToTopQuery::send` (StarGiftManager.cpp:573-581).
    fn send_toggle_star_gifts_pinned_to_top_query(&mut self, star_gift_ids: &[StarGiftId]) {
        tdutils::log_info!(
            "Send payments.toggleStarGiftsPinnedToTop({} gifts) — [blocked: net]",
            star_gift_ids.len()
        );
    }

    /// `ToggleChatStarGiftNotificationsQuery::send`
    /// (StarGiftManager.cpp:606-612).
    fn send_toggle_chat_star_gift_notifications_query(
        &mut self,
        dialog_id: DialogId,
        are_enabled: bool,
    ) {
        tdutils::log_info!(
            "Send payments.toggleChatStarGiftNotifications(dialog {}, enabled = {are_enabled}) — [blocked: net]",
            dialog_id.get()
        );
    }

    /// `GetUpgradeGiftPreviewQuery::send` (StarGiftManager.cpp:637-639).
    fn send_get_upgrade_gift_preview_query(&mut self, gift_id: i64) {
        tdutils::log_info!(
            "Send payments.getStarGiftUpgradePreview(gift_id = {gift_id}) — [blocked: net]"
        );
    }

    /// `GetStarGiftUpgradeAttributesQuery::send`
    /// (StarGiftManager.cpp:715-719).
    fn send_get_star_gift_upgrade_attributes_query(&mut self, gift_id: i64) {
        tdutils::log_info!(
            "Send payments.getStarGiftUpgradeAttributes(gift_id = {gift_id}) — [blocked: net]"
        );
    }

    /// the upgrade payment chains (`UpgradeStarGiftQuery`,
    /// `GetGiftUpgradePaymentFormQuery`, `PrepaidUpgradeGiftQuery`,
    /// `GetGiftPrepaidUpgradePaymentFormQuery`) — StarGiftManager.cpp:832-838/
    /// 923-934/986-990/1035-1042.
    fn send_upgrade_gift_query(&mut self, star_count: i64, keep_original_details: bool) {
        tdutils::log_info!(
            "Send the gift upgrade payment chain (star_count = {star_count}, \
             keep_original_details = {keep_original_details}) — [blocked: net]"
        );
    }

    /// `CraftStarGiftQuery::send` (StarGiftManager.cpp:1131-1134).
    fn send_craft_star_gift_query(&mut self, star_gift_ids: &[StarGiftId]) {
        tdutils::log_info!(
            "Send payments.craftStarGift({} gifts) — [blocked: net]",
            star_gift_ids.len()
        );
    }

    /// the transfer chains (`TransferStarGiftQuery`,
    /// `GetGiftTransferPaymentFormQuery`) — StarGiftManager.cpp:1169-1176/
    /// 1257-1268.
    fn send_transfer_gift_query(&mut self, star_count: i64) {
        tdutils::log_info!(
            "Send the gift transfer payment chain (star_count = {star_count}) — [blocked: net]"
        );
    }

    /// the drop-original-details chains
    /// (`DropGiftOriginalDetailsQuery`,
    /// `GetGiftDropOriginalDetailsPaymentFormQuery`) —
    /// StarGiftManager.cpp:1316-1320/1364-1372.
    fn send_drop_gift_original_details_query(&mut self, star_count: i64) {
        tdutils::log_info!(
            "Send the drop original details payment chain (star_count = {star_count}) — [blocked: net]"
        );
    }

    /// the resale chains (`ResaleGiftQuery`,
    /// `GetGiftResalePaymentFormQuery`) — StarGiftManager.cpp:1447-1452/
    /// 1500-1508.
    fn send_resale_gift_query(&mut self, gift_name: &str) {
        tdutils::log_info!("Send the gift resale payment chain for {gift_name} — [blocked: net]");
    }

    /// `SendStarGiftOfferQuery::send` (StarGiftManager.cpp:1584-1598).
    fn send_send_star_gift_offer_query(&mut self, gift_name: &str) {
        tdutils::log_info!("Send payments.sendStarGiftOffer for {gift_name} — [blocked: net]");
    }

    /// `ResolveStarGiftOfferQuery::send` (StarGiftManager.cpp:1629-1632).
    fn send_resolve_star_gift_offer_query(&mut self, server_message_id: i32, decline: bool) {
        tdutils::log_info!(
            "Send payments.resolveStarGiftOffer(message {}, decline = {decline}) — [blocked: net]",
            server_message_id
        );
    }

    /// `GetSavedStarGiftsQuery::send` (StarGiftManager.cpp:1661-1688).
    fn send_get_saved_star_gifts_query(&mut self, dialog_id: DialogId) {
        tdutils::log_info!(
            "Send payments.getSavedStarGifts(dialog {}) — [blocked: net]",
            dialog_id.get()
        );
    }

    /// `GetSavedStarGiftQuery::send` (StarGiftManager.cpp:1751-1759).
    fn send_get_saved_star_gift_query(&mut self, star_gift_id: &StarGiftId) {
        let _ = star_gift_id;
        tdutils::log_info!("Send payments.getSavedStarGift — [blocked: net]");
    }

    /// `GetCraftStarGiftsQuery::send` (StarGiftManager.cpp:1796-1798).
    fn send_get_craft_star_gifts_query(&mut self, gift_id: i64) {
        tdutils::log_info!("Send payments.getCraftStarGifts(gift_id = {gift_id}) — [blocked: net]");
    }

    /// `GetUniqueStarGiftQuery::send` (StarGiftManager.cpp:1861-1863).
    fn send_get_unique_star_gift_query(&mut self, name: &str) {
        tdutils::log_info!("Send payments.getUniqueStarGift({name}) — [blocked: net]");
    }

    /// `GetUniqueStarGiftValueInfoQuery::send`
    /// (StarGiftManager.cpp:1898-1900).
    fn send_get_unique_star_gift_value_info_query(&mut self, name: &str) {
        tdutils::log_info!("Send payments.getUniqueStarGiftValueInfo({name}) — [blocked: net]");
    }

    /// `GetStarGiftWithdrawalUrlQuery::send`
    /// (StarGiftManager.cpp:1930-1938).
    fn send_get_star_gift_withdrawal_url_query(&mut self, star_gift_id: &StarGiftId) {
        let _ = star_gift_id;
        tdutils::log_info!("Send payments.getStarGiftWithdrawalUrl — [blocked: net]");
    }

    /// `UpdateStarGiftPriceQuery::send` (StarGiftManager.cpp:1962-1969).
    fn send_update_star_gift_price_query(&mut self, star_gift_id: &StarGiftId) {
        let _ = star_gift_id;
        tdutils::log_info!("Send payments.updateStarGiftPrice — [blocked: net]");
    }

    /// `GetResaleStarGiftsQuery::send` (StarGiftManager.cpp:1996-2010).
    fn send_get_resale_star_gifts_query(&mut self, gift_id: i64) {
        tdutils::log_info!(
            "Send payments.getResaleStarGifts(gift_id = {gift_id}) — [blocked: net]"
        );
    }

    /// `GetStarGiftCollectionsQuery::send` (StarGiftManager.cpp:2139-2144).
    fn send_get_star_gift_collections_query(&mut self, dialog_id: DialogId) {
        tdutils::log_info!(
            "Send payments.getStarGiftCollections(dialog {}) — [blocked: net]",
            dialog_id.get()
        );
    }

    /// `CreateStarGiftCollectionQuery::send` (StarGiftManager.cpp:2188-2195).
    fn send_create_star_gift_collection_query(&mut self, dialog_id: DialogId, title: &str) {
        tdutils::log_info!(
            "Send payments.createStarGiftCollection(dialog {}, title = {title}) — [blocked: net]",
            dialog_id.get()
        );
    }

    /// `ReorderStarGiftCollectionsQuery::send`
    /// (StarGiftManager.cpp:2224-2232).
    fn send_reorder_star_gift_collections_query(&mut self, dialog_id: DialogId) {
        tdutils::log_info!(
            "Send payments.reorderStarGiftCollections(dialog {}) — [blocked: net]",
            dialog_id.get()
        );
    }

    /// `DeleteStarGiftCollectionQuery::send` (StarGiftManager.cpp:2258-2263).
    fn send_delete_star_gift_collection_query(&mut self, dialog_id: DialogId) {
        tdutils::log_info!(
            "Send payments.deleteStarGiftCollection(dialog {}) — [blocked: net]",
            dialog_id.get()
        );
    }

    /// `UpdateStarGiftCollectionQuery::send` (StarGiftManager.cpp:2290-2314).
    fn send_update_star_gift_collection_query(
        &mut self,
        dialog_id: DialogId,
        collection_id: StarGiftCollectionId,
    ) {
        tdutils::log_info!(
            "Send payments.updateStarGiftCollection(dialog {}, collection {}) — [blocked: net]",
            dialog_id.get(),
            collection_id.get()
        );
    }

    /// `GetStarGiftPromoAnimationQuery::send` (StarGiftManager.cpp:2343-2345).
    fn send_get_star_gift_promo_animation_query(&mut self) {
        tdutils::log_info!("Send help.getPremiumPromo — [blocked: net]");
    }

    /// `MessagesManager::get_message_from_server(message_full_id, ...)`
    /// — the gift-message poll ([blocked: M.6]).
    fn get_message_from_server(&mut self, message_full_id: crate::message_id::MessageFullId) {
        let _ = message_full_id;
        tdutils::log_info!(
            "get_message_from_server for the gift poll — [blocked: M.6 MessagesManager]"
        );
    }
}

// --- the manager ---

/// A parked `get_gift_auction_state` continuation: the C++
/// `PromiseCreator::lambda([actor_id, ...])(r_state)` chains collapsed
/// into the answer handler's dispatch (StarGiftManager.cpp:2529-2533,
/// 2730-2737, 2768-2777, 2826-2836).
enum PendingAuctionStateQuery {
    /// `get_gift_auction_state` — answer the state object.
    State(Promise<td_api::GiftAuctionState>),
    /// `open_gift_auction`'s reload recursion.
    Open { gift_id: i64, promise: Promise<()> },
    /// `place_gift_auction_bid`'s reload recursion.
    PlaceBid {
        gift_id: i64,
        star_count: i64,
        user_id: UserId,
        text: Option<td_api::FormattedText>,
        is_private: bool,
        promise: Promise<()>,
    },
    /// `update_gift_auction_bid`'s reload recursion.
    UpdateBid {
        gift_id: i64,
        star_count: i64,
        promise: Promise<()>,
    },
}

/// `StarGiftManager::AuctionInfo` (StarGiftManager.h:175-183).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AuctionInfo {
    gift: StarGift,
    state: StarGiftAuctionState,
    user_state: StarGiftAuctionUserState,
}

/// The parked promises of the `[blocked: net]` query seams, one list per
/// answer family (the C++ promises ride the ResultHandlers; the collapsed
/// port parks them until the answer handlers fire or the net step lands).
#[derive(Default)]
struct PendingQueries {
    gift_payment_options: Vec<Promise<td_api::AvailableGifts>>,
    can_send_gift: Vec<Promise<td_api::CanSendGiftResult>>,
    auction_state: Vec<PendingAuctionStateQuery>,
    gift_auction_acquired_gifts: Vec<Promise<td_api::GiftAuctionAcquiredGifts>>,
    gift_upgrade_preview: Vec<Promise<td_api::GiftUpgradePreview>>,
    gift_upgrade_variants: Vec<(bool, bool, Promise<td_api::GiftUpgradeVariants>)>,
    saved_star_gifts: Vec<(DialogId, bool, Promise<td_api::ReceivedGifts>)>,
    saved_star_gift: Vec<(DialogId, Promise<td_api::ReceivedGift>)>,
    gifts_for_crafting: Vec<Promise<td_api::GiftsForCrafting>>,
    upgraded_gift: Vec<Promise<td_api::UpgradedGift>>,
    upgraded_gift_value_info: Vec<Promise<td_api::UpgradedGiftValueInfo>>,
    star_gift_withdrawal_url: Vec<Promise<String>>,
    gifts_for_resale: Vec<Promise<td_api::GiftsForResale>>,
    gift_collections: Vec<Promise<td_api::GiftCollections>>,
    gift_collection: Vec<Promise<td_api::GiftCollection>>,
    star_gift_promo_animation: Vec<Promise<td_api::Animation>>,
    resolve_star_gift_offer: Vec<Promise<()>>,
    /// `UpgradeStarGiftQuery`-family answers (the object results the
    /// payment chains complete).
    star_gift_result: Vec<Promise<td_api::UpgradeGiftResult>>,
    /// `CraftStarGiftQuery` answers.
    craft_result: Vec<Promise<td_api::CraftGiftResult>>,
    /// `GetGiftResalePaymentFormQuery`/`ResaleGiftQuery` answers.
    gift_resale_result: Vec<Promise<td_api::GiftResaleResult>>,
    /// The answerless chains (payment forms, updates-routed queries):
    /// the promise parks until the net step completes it.
    unit_answers: Vec<Promise<()>>,
}

/// The port of `td::StarGiftManager` (StarGiftManager.h:41-261).
#[derive(Default)]
pub struct StarGiftManager {
    /// `gift_prices_`.
    gift_prices: HashMap<i64, (i64, i64)>,
    /// `gift_auction_infos_`.
    gift_auction_infos: HashMap<i64, AuctionInfo>,
    /// `active_gift_auctions_`.
    active_gift_auctions: Vec<AuctionInfo>,
    /// `gift_auction_open_counts_`.
    gift_auction_open_counts: HashMap<i64, i32>,
    /// `active_gift_auctions_reload_timeout_.has_timeout()` — the
    /// scheduled-reload flag (the timer itself is the scheduler seam).
    active_gift_auctions_reload_scheduled: bool,
    /// `gift_message_count_`.
    gift_message_count: i64,
    /// `gift_message_full_ids_`.
    gift_message_full_ids: HashMap<crate::message_id::MessageFullId, i64>,
    /// `gift_message_full_ids_by_id_`.
    gift_message_full_ids_by_id: HashMap<i64, crate::message_id::MessageFullId>,
    /// `being_reloaded_gift_messages_`.
    being_reloaded_gift_messages: Vec<crate::message_id::MessageFullId>,
    /// The parked query promises.
    pending: PendingQueries,
}

impl StarGiftManager {
    /// `StarGiftManager::StarGiftManager` (StarGiftManager.cpp:2391-2397).
    pub fn new() -> StarGiftManager {
        StarGiftManager::default()
    }

    // -- the star gift price cache --

    /// `StarGiftManager::on_get_star_gift` (StarGiftManager.cpp:2432-2440).
    pub fn on_get_star_gift(
        &mut self,
        env: &mut impl StarGiftEnv,
        star_gift: &StarGift,
        from_server: bool,
    ) {
        if env.is_bot() || !star_gift.is_valid() || star_gift.is_unique() {
            return;
        }
        if !from_server && self.gift_prices.contains_key(&star_gift.get_id()) {
            return;
        }
        self.gift_prices.insert(
            star_gift.get_id(),
            (
                star_gift.get_star_count(),
                star_gift.get_upgrade_star_count(),
            ),
        );
    }

    /// `StarGiftManager::get_gift_payment_options`
    /// (StarGiftManager.cpp:2428-2430) — the query seam parks the
    /// promise; the answer arrives through
    /// [`Self::on_get_available_gifts`].
    pub fn get_gift_payment_options(
        &mut self,
        env: &mut impl StarGiftEnv,
        promise: Promise<td_api::AvailableGifts>,
    ) {
        env.send_get_star_gifts_query();
        self.pending.gift_payment_options.push(promise);
    }

    /// `GetStarGiftsQuery::on_result` (StarGiftManager.cpp:63-131).
    pub fn on_get_available_gifts(
        &mut self,
        env: &mut impl StarGiftEnv,
        result: Result<telegram_api::payments_starGifts, Status>,
    ) {
        let pending = std::mem::take(&mut self.pending.gift_payment_options);
        let Ok(results) = result else {
            let status = result.unwrap_err();
            for mut query in pending {
                query.set_error(status.clone());
            }
            return;
        };
        env.on_get_users(results.users, "GetStarGiftsQuery");
        env.on_get_chats(results.chats, "GetStarGiftsQuery");

        let is_bot = env.is_bot();
        let mut options = Vec::new();
        for gift in results.gifts {
            let mut availability_resale = 0i64;
            let mut resell_min_stars = 0i64;
            let mut title = String::new();
            if let telegram_api::StarGift::starGift(star_gift) = &gift {
                if is_bot {
                    if star_gift.availability_total > 0 && star_gift.availability_remains == 0 {
                        continue;
                    }
                    if star_gift.per_user_total > 0 && star_gift.per_user_remains == 0 {
                        continue;
                    }
                    if star_gift.require_premium {
                        continue;
                    }
                    if star_gift.auction {
                        continue;
                    }
                    if star_gift.locked_until_date > env.unix_time() {
                        continue;
                    }
                } else {
                    availability_resale = star_gift.availability_resale;
                    resell_min_stars = get_star_count(star_gift.resell_min_stars, false);
                    title = star_gift.title.clone();
                    if !(0..=1_000_000_000).contains(&availability_resale) {
                        tdutils::log_error!("Receive {availability_resale} available gifts");
                        availability_resale = 0;
                    } else if resell_min_stars == 0 && availability_resale > 0 {
                        tdutils::log_error!(
                            "Receive {availability_resale} available gifts with the minimum price of {resell_min_stars}"
                        );
                        availability_resale = 0;
                    }
                    if availability_resale == 0 {
                        resell_min_stars = 0;
                        title.clear();
                    }
                }
            }

            let star_gift = StarGift::from_telegram(env, gift, false);
            if !star_gift.is_valid() {
                continue;
            }
            self.on_get_star_gift(env, &star_gift, true);
            options.push(td_api::AvailableGift {
                gift: Some(star_gift.get_gift_object(env, None)),
                resale_count: availability_resale as i32,
                min_resale_star_count: resell_min_stars,
                title,
            });
        }

        let answer = td_api::AvailableGifts { gifts: options };
        for mut query in pending {
            query.set_value(answer.clone());
        }
    }

    /// `StarGiftManager::can_send_gift` (StarGiftManager.cpp:2442-2444).
    pub fn can_send_gift(
        &mut self,
        env: &mut impl StarGiftEnv,
        gift_id: i64,
        promise: Promise<td_api::CanSendGiftResult>,
    ) {
        env.send_check_can_send_gift_query(gift_id);
        self.pending.can_send_gift.push(promise);
    }

    /// `CheckCanSendGiftQuery::on_result` (StarGiftManager.cpp:150-171).
    pub fn on_get_can_send_gift_result(
        &mut self,
        _env: &mut impl StarGiftEnv,
        result: Result<telegram_api::payments_CheckCanSendGiftResult, Status>,
    ) {
        let pending = std::mem::take(&mut self.pending.can_send_gift);
        let Ok(ptr) = result else {
            let status = result.unwrap_err();
            for mut query in pending {
                query.set_error(status.clone());
            }
            return;
        };
        let answer = match ptr {
            telegram_api::payments_CheckCanSendGiftResult::payments_checkCanSendGiftResultOk(_) => {
                td_api::CanSendGiftResult::canSendGiftResultOk(td_api::canSendGiftResultOk)
            }
            telegram_api::payments_CheckCanSendGiftResult::payments_checkCanSendGiftResultFail(
                result,
            ) => {
                let reason = message_entity::get_formatted_text_of_text_with_entities::<()>(
                    None,
                    result.reason,
                    true,
                    false,
                    "CheckCanSendGiftQuery",
                );
                td_api::CanSendGiftResult::canSendGiftResultFail(td_api::canSendGiftResultFail {
                    reason: Some(message_entity::get_formatted_text_object::<()>(
                        None, &reason, true, 1,
                    )),
                })
            }
        };
        for mut query in pending {
            query.set_value(answer.clone());
        }
    }

    /// `StarGiftManager::send_gift` (StarGiftManager.cpp:2446-2485) — the
    /// local halves; the payment-form chain head is the seam.
    #[allow(clippy::too_many_arguments)]
    pub fn send_gift(
        &mut self,
        env: &mut impl StarGiftEnv,
        gift_id: i64,
        dialog_id: DialogId,
        text: Option<td_api::FormattedText>,
        is_private: bool,
        pay_for_upgrade: bool,
        mut promise: Promise<()>,
    ) {
        let mut star_count = 0i64;
        if !env.is_bot() {
            let Some(&(price, upgrade_price)) = self.gift_prices.get(&gift_id) else {
                return promise.set_error(Status::error(400, "Gift not found"));
            };
            star_count = price;
            if pay_for_upgrade {
                star_count += upgrade_price;
            }
            if !env.has_owned_star_count(star_count) {
                return promise.set_error(Status::error(400, "Have not enough Telegram Stars"));
            }
        }
        let input_peer = env.get_input_peer(dialog_id);
        let send_input_peer = env.get_input_peer(dialog_id);
        if input_peer.is_none() || send_input_peer.is_none() {
            return promise.set_error(Status::error(400, "Have no access to the gift receiver"));
        }
        // get_formatted_text(td_, my_dialog_id, text, is_bot = false,
        // allow_empty = true, skip_media_timestamps = true, skip_trim =
        // false) — the option-threaded inputs ride the env (the C++
        // reads them off td_ directly)
        let mut message = match message_entity::get_formatted_text_of_input::<()>(
            None,
            to_entity_dialog_id(env.get_my_dialog_id()),
            text,
            false,
            true,
            true,
            false,
            None,
            env.get_option_boolean("always_parse_markdown"),
            0, // the session-count plane rides the connection integration
            None,
            false,
        ) {
            Result::Ok(message) => message,
            Result::Err(status) => return promise.set_error(status),
        };
        message_entity::remove_unallowed_quote_entities(&mut message);

        // the inputInvoiceStarGift pair + the MESSAGE_MASK flag; the
        // GetGiftPaymentFormQuery head is the seam
        if !message.text.is_empty() {
            let _ = message_entity::get_input_text_with_entities::<()>(None, &message, "send_gift");
        }
        env.send_get_gift_payment_form_query(
            gift_id,
            star_count,
            is_private,
            pay_for_upgrade,
            !message.text.is_empty(),
        );
        self.pending.unit_answers.push(promise);
    }

    // -- the auction plane --

    /// `StarGiftManager::get_auction_info` (StarGiftManager.cpp:2493-2524)
    /// — returns a clone of the stored info (the C++ returns a pointer
    /// into the map).
    fn get_auction_info(
        &mut self,
        env: &mut impl StarGiftEnv,
        star_gift: telegram_api::StarGift,
        state: telegram_api::StarGiftAuctionState,
        user_state: telegram_api::StarGiftAuctionUserState,
    ) -> Option<AuctionInfo> {
        let gift = StarGift::from_telegram(env, star_gift, false);
        if !gift.is_valid() || gift.get_id() == 0 || !gift.is_auction() {
            tdutils::log_error!("Receive invalid auction gift");
            return None;
        }

        let mut is_changed = false;
        let gift_id = gift.get_id();
        {
            let info = self.gift_auction_infos.entry(gift_id).or_default();
            if info.gift != gift {
                info.gift = gift;
                is_changed = true;
            }
            let new_state = StarGiftAuctionState::from_telegram(&state);
            if !new_state.is_not_modified()
                && new_state.get_version() >= info.state.get_version()
                && new_state != info.state
            {
                info.state = new_state;
                is_changed = true;
            }
            let new_user_state = StarGiftAuctionUserState::from_telegram(&user_state);
            if info.user_state != new_user_state {
                info.user_state = new_user_state;
                is_changed = true;
            }
        }
        if is_changed {
            let info = self.gift_auction_infos[&gift_id].clone();
            self.send_update_gift_auction_state(env, &info);
        }
        self.gift_auction_infos.get(&gift_id).cloned()
    }

    /// `StarGiftManager::get_gift_auction_state`
    /// (StarGiftManager.cpp:2487-2491).
    pub fn get_gift_auction_state(
        &mut self,
        env: &mut impl StarGiftEnv,
        auction_id: &str,
        promise: Promise<td_api::GiftAuctionState>,
    ) {
        self.reload_gift_auction_state(
            env,
            telegram_api::InputStarGiftAuction::inputStarGiftAuctionSlug(
                telegram_api::inputStarGiftAuctionSlug {
                    slug: auction_id.to_owned(),
                },
            ),
            0,
            PendingAuctionStateQuery::State(promise),
        );
    }

    /// `StarGiftManager::reload_gift_auction_state`
    /// (StarGiftManager.cpp:2526-2535) — the query seam parks the
    /// continuation; the answer arrives through
    /// [`Self::on_get_auction_state`].
    fn reload_gift_auction_state(
        &mut self,
        env: &mut impl StarGiftEnv,
        input_auction: telegram_api::InputStarGiftAuction,
        version: i32,
        pending: PendingAuctionStateQuery,
    ) {
        env.send_get_star_gift_auction_state_query(&input_auction, version);
        self.pending.auction_state.push(pending);
    }

    /// `StarGiftManager::on_get_auction_state`
    /// (StarGiftManager.cpp:2537-2561).
    pub fn on_get_auction_state(
        &mut self,
        env: &mut impl StarGiftEnv,
        result: Result<telegram_api::payments_StarGiftAuctionState, Status>,
    ) {
        let Ok(state) = result else {
            let status = result.unwrap_err();
            for pending in std::mem::take(&mut self.pending.auction_state) {
                match pending {
                    PendingAuctionStateQuery::State(mut promise) => {
                        promise.set_error(status.clone())
                    }
                    PendingAuctionStateQuery::Open { mut promise, .. } => {
                        promise.set_error(status.clone())
                    }
                    PendingAuctionStateQuery::PlaceBid { mut promise, .. } => {
                        promise.set_error(status.clone())
                    }
                    PendingAuctionStateQuery::UpdateBid { mut promise, .. } => {
                        promise.set_error(status.clone())
                    }
                }
            }
            return;
        };

        env.on_get_users(state.users, "on_get_auction_state");
        env.on_get_chats(state.chats, "on_get_auction_state");

        let gift_star_gift =
            state
                .gift
                .unwrap_or(telegram_api::StarGift::starGift(telegram_api::starGift {
                    id: 0,
                    ..Default::default()
                }));
        let empty_state = telegram_api::StarGiftAuctionState::starGiftAuctionStateNotModified(
            telegram_api::starGiftAuctionStateNotModified,
        );
        let state_ptr = state.state.unwrap_or(empty_state);
        let empty_user_state = telegram_api::StarGiftAuctionUserState {
            ..Default::default()
        };
        let user_state_ptr = state.user_state.unwrap_or(empty_user_state);
        let Some(info) = self.get_auction_info(env, gift_star_gift, state_ptr, user_state_ptr)
        else {
            for pending in std::mem::take(&mut self.pending.auction_state) {
                match pending {
                    PendingAuctionStateQuery::State(mut promise) => {
                        promise.set_error(Status::error(500, "Receive invalid response"));
                    }
                    PendingAuctionStateQuery::Open { mut promise, .. } => {
                        promise.set_error(Status::error(500, "Receive invalid response"));
                    }
                    PendingAuctionStateQuery::PlaceBid { mut promise, .. } => {
                        promise.set_error(Status::error(500, "Receive invalid response"));
                    }
                    PendingAuctionStateQuery::UpdateBid { mut promise, .. } => {
                        promise.set_error(Status::error(500, "Receive invalid response"));
                    }
                }
            }
            return;
        };

        let gift_id = info.gift.get_id();
        if self.gift_auction_infos.contains_key(&gift_id) {
            // reload_gift_auction_timeout_.set_timeout_in(gift_id, timeout)
            // — the timer itself is the scheduler seam
        }
        let _ = state.timeout;
        let pending = std::mem::take(&mut self.pending.auction_state);
        for query in pending {
            match query {
                PendingAuctionStateQuery::State(mut promise) => {
                    promise.set_value(self.get_gift_auction_state_object(env, &info));
                }
                PendingAuctionStateQuery::Open { gift_id, promise } => {
                    self.open_gift_auction(env, gift_id, true, promise);
                }
                PendingAuctionStateQuery::PlaceBid {
                    gift_id,
                    star_count,
                    user_id,
                    text,
                    is_private,
                    promise,
                } => {
                    self.do_place_gift_auction_bid(
                        env, gift_id, star_count, user_id, text, is_private, promise,
                    );
                }
                PendingAuctionStateQuery::UpdateBid {
                    gift_id,
                    star_count,
                    promise,
                } => {
                    self.do_update_gift_auction_bid(env, gift_id, star_count, promise);
                }
            }
        }
    }

    /// `StarGiftManager::on_update_gift_auction_state`
    /// (StarGiftManager.cpp:2563-2581).
    pub fn on_update_gift_auction_state(
        &mut self,
        env: &mut impl StarGiftEnv,
        gift_id: i64,
        state: Option<telegram_api::StarGiftAuctionState>,
    ) {
        if !self.gift_auction_infos.contains_key(&gift_id) {
            self.reload_active_gift_auctions(env);
            return;
        }
        let Some(state) = state else {
            tdutils::log_error!("Receive no auction state");
            return;
        };
        let new_state = StarGiftAuctionState::from_telegram(&state);
        if new_state.is_not_modified() {
            tdutils::log_error!("Receive an auction state not modified");
            return;
        }
        let info = self.gift_auction_infos.get_mut(&gift_id).expect("checked");
        if new_state.get_version() <= info.state.get_version() || new_state == info.state {
            return;
        }
        info.state = new_state;
        let info = info.clone();
        self.send_update_gift_auction_state(env, &info);
    }

    /// `StarGiftManager::on_update_gift_auction_user_state`
    /// (StarGiftManager.cpp:2583-2597).
    pub fn on_update_gift_auction_user_state(
        &mut self,
        env: &mut impl StarGiftEnv,
        gift_id: i64,
        user_state: Option<telegram_api::StarGiftAuctionUserState>,
    ) {
        if !self.gift_auction_infos.contains_key(&gift_id) {
            self.reload_active_gift_auctions(env);
            return;
        }
        let Some(user_state) = user_state else {
            tdutils::log_error!("Receive no auction user state");
            return;
        };
        let new_user_state = StarGiftAuctionUserState::from_telegram(&user_state);
        let info = self.gift_auction_infos.get_mut(&gift_id).expect("checked");
        if info.user_state == new_user_state {
            return;
        }
        info.user_state = new_user_state;
        let info = info.clone();
        self.send_update_gift_auction_state(env, &info);
    }

    /// `StarGiftManager::schedule_active_gift_auctions_reload`
    /// (StarGiftManager.cpp:2599-2608) — the flag-only collapse.
    fn schedule_active_gift_auctions_reload(&mut self) {
        if self.active_gift_auctions_reload_scheduled {
            return;
        }
        tdutils::log_info!("Schedule active gift auctions reload");
        self.active_gift_auctions_reload_scheduled = true;
    }

    /// `StarGiftManager::reload_active_gift_auctions`
    /// (StarGiftManager.cpp:2619-2626) — the public fire point of the
    /// reload timeout.
    pub fn reload_active_gift_auctions(&mut self, env: &mut impl StarGiftEnv) {
        env.send_get_star_gift_active_auctions_query();
        // the answer lands through on_get_active_gift_auctions; the C++
        // parks nothing (the promise is the answer hop)
        self.pending.unit_answers.push(Promise::default());
    }

    /// `StarGiftManager::on_get_active_gift_auctions`
    /// (StarGiftManager.cpp:2628-2665).
    pub fn on_get_active_gift_auctions(
        &mut self,
        env: &mut impl StarGiftEnv,
        result: Result<telegram_api::payments_StarGiftActiveAuctions, Status>,
    ) {
        if result.is_err() {
            self.schedule_active_gift_auctions_reload();
            return;
        }
        let auctions = result.unwrap();
        match auctions {
            telegram_api::payments_StarGiftActiveAuctions::payments_starGiftActiveAuctionsNotModified(_) => {
                tdutils::log_error!("Receive active auctions not modified");
            }
            telegram_api::payments_StarGiftActiveAuctions::payments_starGiftActiveAuctions(auctions) => {
                env.on_get_users(auctions.users, "on_get_active_gift_auctions");
                env.on_get_chats(auctions.chats, "on_get_active_gift_auctions");
                let mut new_active_auctions = Vec::new();
                for auction in auctions.auctions {
                    let empty_state = telegram_api::StarGiftAuctionState::starGiftAuctionStateNotModified(
                        telegram_api::starGiftAuctionStateNotModified,
                    );
                    let Some(info) = self.get_auction_info(
                        env,
                        auction.gift.unwrap_or(telegram_api::StarGift::starGift(
                            telegram_api::starGift {
                                id: 0,
                                ..Default::default()
                            },
                        )),
                        auction.state.unwrap_or(empty_state),
                        auction.user_state.unwrap_or_default(),
                    ) else {
                        continue;
                    };
                    new_active_auctions.push(info);
                }
                if self.active_gift_auctions != new_active_auctions {
                    self.active_gift_auctions = new_active_auctions;
                    self.send_update_active_gift_auctions(env);
                }
            }
        }
    }

    /// `StarGiftManager::get_gift_auction_acquired_gifts`
    /// (StarGiftManager.cpp:2667-2670).
    pub fn get_gift_auction_acquired_gifts(
        &mut self,
        env: &mut impl StarGiftEnv,
        gift_id: i64,
        promise: Promise<td_api::GiftAuctionAcquiredGifts>,
    ) {
        env.send_get_star_gift_auction_acquired_gifts_query(gift_id);
        self.pending.gift_auction_acquired_gifts.push(promise);
    }

    /// `GetStarGiftAuctionAcquiredGiftsQuery::on_result`
    /// (StarGiftManager.cpp:466-487).
    pub fn on_get_gift_auction_acquired_gifts(
        &mut self,
        env: &mut impl StarGiftEnv,
        result: Result<telegram_api::payments_StarGiftAuctionAcquiredGifts, Status>,
    ) {
        let pending = std::mem::take(&mut self.pending.gift_auction_acquired_gifts);
        let Ok(ptr) = result else {
            let status = result.unwrap_err();
            for mut query in pending {
                query.set_error(status.clone());
            }
            return;
        };
        env.on_get_users(ptr.users, "GetStarGiftAuctionAcquiredGiftsQuery");
        env.on_get_chats(ptr.chats, "GetStarGiftAuctionAcquiredGiftsQuery");

        let mut gifts = Vec::new();
        for gift in ptr.gifts {
            let text = message_entity::get_formatted_text_of_text_with_entities::<()>(
                None,
                gift.message,
                true,
                false,
                "giftAuctionAcquiredGift",
            );
            gifts.push(td_api::GiftAuctionAcquiredGift {
                receiver_id: gift.peer.as_ref().and_then(|peer| {
                    env.get_message_sender_object(
                        DialogId::from_peer(peer),
                        "giftAuctionAcquiredGift",
                    )
                }),
                date: gift.date,
                star_count: get_star_count(gift.bid_amount, false),
                auction_round_number: gift.round,
                auction_round_position: gift.pos,
                unique_gift_number: gift.gift_num,
                text: Some(message_entity::get_formatted_text_object::<()>(
                    None, &text, true, -1,
                )),
                is_private: gift.name_hidden,
            });
        }
        let answer = td_api::GiftAuctionAcquiredGifts { gifts };
        for mut query in pending {
            query.set_value(answer.clone());
        }
    }

    /// `StarGiftManager::get_gift_auction_state_object`
    /// (StarGiftManager.cpp:2672-2676).
    fn get_gift_auction_state_object(
        &self,
        env: &mut impl StarGiftEnv,
        info: &AuctionInfo,
    ) -> td_api::GiftAuctionState {
        td_api::GiftAuctionState {
            gift: Some(info.gift.get_gift_object(env, None)),
            state: Some(info.state.get_auction_state_object(env, &info.user_state)),
        }
    }

    /// `StarGiftManager::send_update_gift_auction_state`
    /// (StarGiftManager.cpp:2678-2708).
    fn send_update_gift_auction_state(&mut self, env: &mut impl StarGiftEnv, info: &AuctionInfo) {
        if env.is_bot() {
            return;
        }
        let is_active = info.user_state.is_active();
        let gift_id = info.gift.get_id();
        let mut is_found = false;
        let mut replaced = false;
        for index in 0..self.active_gift_auctions.len() {
            if self.active_gift_auctions[index].gift.get_id() == gift_id {
                is_found = true;
                if is_active {
                    if self.active_gift_auctions[index].state != info.state
                        || self.active_gift_auctions[index].user_state != info.user_state
                    {
                        self.active_gift_auctions[index].state = info.state.clone();
                        self.active_gift_auctions[index].user_state = info.user_state;
                        replaced = true;
                    }
                } else {
                    self.active_gift_auctions.remove(index);
                    replaced = true;
                }
                break;
            }
        }
        if replaced {
            self.send_update_active_gift_auctions(env);
        }
        if !is_found && is_active {
            self.active_gift_auctions.push(info.clone());
            self.send_update_active_gift_auctions(env);
        }
        let state_object = self.get_gift_auction_state_object(env, info);
        env.send_update(td_api::Update::updateGiftAuctionState(
            td_api::updateGiftAuctionState {
                state: Some(state_object),
            },
        ));
    }

    /// `StarGiftManager::send_update_active_gift_auctions`
    /// (StarGiftManager.cpp:2710-2721).
    fn send_update_active_gift_auctions(&mut self, env: &mut impl StarGiftEnv) {
        if env.is_bot() {
            return;
        }
        if !self.active_gift_auctions.is_empty() {
            self.schedule_active_gift_auctions_reload();
        }
        let states = self
            .active_gift_auctions
            .iter()
            .map(|info| self.get_gift_auction_state_object(env, info))
            .collect();
        env.send_update(td_api::Update::updateActiveGiftAuctions(
            td_api::updateActiveGiftAuctions { states },
        ));
    }

    /// `StarGiftManager::open_gift_auction` (StarGiftManager.cpp:2723-2749).
    pub fn open_gift_auction(
        &mut self,
        env: &mut impl StarGiftEnv,
        gift_id: i64,
        is_recursive: bool,
        mut promise: Promise<()>,
    ) {
        if !self.gift_auction_infos.contains_key(&gift_id) {
            assert!(!is_recursive);
            return self.reload_gift_auction_state(
                env,
                telegram_api::InputStarGiftAuction::inputStarGiftAuction(
                    telegram_api::inputStarGiftAuction { gift_id },
                ),
                0,
                PendingAuctionStateQuery::Open { gift_id, promise },
            );
        }

        let count = self.gift_auction_open_counts.entry(gift_id).or_default();
        *count += 1;
        if *count == 1 {
            tdutils::log_info!("Opened auction for gift {gift_id}");
            if !is_recursive {
                let info = self.gift_auction_infos.get(&gift_id).expect("checked");
                let version = info.state.get_version();
                self.reload_gift_auction_state(
                    env,
                    telegram_api::InputStarGiftAuction::inputStarGiftAuction(
                        telegram_api::inputStarGiftAuction { gift_id },
                    ),
                    version,
                    PendingAuctionStateQuery::State(Promise::default()),
                );
            }
        }
        promise.set_value(());
    }

    /// `StarGiftManager::close_gift_auction` (StarGiftManager.cpp:2751-2760).
    pub fn close_gift_auction(&mut self, gift_id: i64, mut promise: Promise<()>) {
        let Some(count) = self.gift_auction_open_counts.get_mut(&gift_id) else {
            return promise.set_error(Status::error(400, "The gift auction isn't open"));
        };
        *count -= 1;
        if *count == 0 {
            self.gift_auction_open_counts.remove(&gift_id);
        }
        promise.set_value(());
    }

    /// `StarGiftManager::place_gift_auction_bid`
    /// (StarGiftManager.cpp:2762-2780).
    #[allow(clippy::too_many_arguments)] // the C++ manager signature
    pub fn place_gift_auction_bid(
        &mut self,
        env: &mut impl StarGiftEnv,
        gift_id: i64,
        star_count: i64,
        user_id: UserId,
        text: Option<td_api::FormattedText>,
        is_private: bool,
        promise: Promise<()>,
    ) {
        if !self.gift_auction_infos.contains_key(&gift_id) {
            return self.reload_gift_auction_state(
                env,
                telegram_api::InputStarGiftAuction::inputStarGiftAuction(
                    telegram_api::inputStarGiftAuction { gift_id },
                ),
                0,
                PendingAuctionStateQuery::PlaceBid {
                    gift_id,
                    star_count,
                    user_id,
                    text,
                    is_private,
                    promise,
                },
            );
        }
        self.do_place_gift_auction_bid(
            env, gift_id, star_count, user_id, text, is_private, promise,
        );
    }

    /// `StarGiftManager::do_place_gift_auction_bid`
    /// (StarGiftManager.cpp:2782-2822).
    #[allow(clippy::too_many_arguments)] // the C++ manager signature
    fn do_place_gift_auction_bid(
        &mut self,
        env: &mut impl StarGiftEnv,
        gift_id: i64,
        star_count: i64,
        user_id: UserId,
        text: Option<td_api::FormattedText>,
        is_private: bool,
        mut promise: Promise<()>,
    ) {
        let old_star_count = self
            .gift_auction_infos
            .get(&gift_id)
            .expect("the auction is open")
            .user_state
            .get_bid_amount();
        if star_count <= old_star_count {
            return promise.set_error(Status::error(400, "Too small bid specified"));
        }
        if !env.has_owned_star_count(star_count - old_star_count) {
            return promise.set_error(Status::error(400, "Have not enough Telegram Stars"));
        }
        let dialog_id = DialogId::from_user_id(user_id);
        let input_peer = env.get_input_peer(dialog_id);
        let send_input_peer = env.get_input_peer(dialog_id);
        if input_peer.is_none() || send_input_peer.is_none() {
            return promise.set_error(Status::error(400, "Have no access to the gift receiver"));
        }
        let mut message = match message_entity::get_formatted_text_of_input::<()>(
            None,
            to_entity_dialog_id(env.get_my_dialog_id()),
            text,
            false,
            true,
            true,
            false,
            None,
            env.get_option_boolean("always_parse_markdown"),
            0, // the session-count plane rides the connection integration
            None,
            false,
        ) {
            Result::Ok(message) => message,
            Result::Err(status) => return promise.set_error(status),
        };
        message_entity::remove_unallowed_quote_entities(&mut message);
        // the GetGiftAuctionBidPaymentFormQuery head — the seam; the
        // invoice bits (is_private, the message) ride the log
        let _ = message;
        let _ = is_private;
        env.add_pending_owned_star_count(old_star_count - star_count, false);
        env.send_get_star_gift_auction_state_query(
            &telegram_api::InputStarGiftAuction::inputStarGiftAuction(
                telegram_api::inputStarGiftAuction { gift_id },
            ),
            0,
        );
        self.pending.unit_answers.push(promise);
    }

    /// `StarGiftManager::update_gift_auction_bid`
    /// (StarGiftManager.cpp:2824-2839).
    pub fn update_gift_auction_bid(
        &mut self,
        env: &mut impl StarGiftEnv,
        gift_id: i64,
        star_count: i64,
        promise: Promise<()>,
    ) {
        if !self.gift_auction_infos.contains_key(&gift_id) {
            return self.reload_gift_auction_state(
                env,
                telegram_api::InputStarGiftAuction::inputStarGiftAuction(
                    telegram_api::inputStarGiftAuction { gift_id },
                ),
                0,
                PendingAuctionStateQuery::UpdateBid {
                    gift_id,
                    star_count,
                    promise,
                },
            );
        }
        self.do_update_gift_auction_bid(env, gift_id, star_count, promise);
    }

    /// `StarGiftManager::do_update_gift_auction_bid`
    /// (StarGiftManager.cpp:2841-2861).
    fn do_update_gift_auction_bid(
        &mut self,
        env: &mut impl StarGiftEnv,
        gift_id: i64,
        star_count: i64,
        mut promise: Promise<()>,
    ) {
        let old_star_count = self
            .gift_auction_infos
            .get(&gift_id)
            .expect("the auction is open")
            .user_state
            .get_bid_amount();
        if old_star_count == 0 {
            return promise.set_error(Status::error(400, "There is no bid to update"));
        }
        if star_count <= old_star_count {
            return promise.set_error(Status::error(400, "Too small bid specified"));
        }
        if !env.has_owned_star_count(star_count - old_star_count) {
            return promise.set_error(Status::error(400, "Have not enough Telegram Stars"));
        }
        // the GetGiftAuctionBidPaymentFormQuery head — the seam
        env.add_pending_owned_star_count(old_star_count - star_count, false);
        env.send_get_star_gift_auction_state_query(
            &telegram_api::InputStarGiftAuction::inputStarGiftAuction(
                telegram_api::inputStarGiftAuction { gift_id },
            ),
            0,
        );
        self.pending.unit_answers.push(promise);
    }

    // -- the convert/save/pin/notifications plane --

    /// `StarGiftManager::convert_gift` (StarGiftManager.cpp:2863-2885).
    pub fn convert_gift(
        &mut self,
        env: &mut impl StarGiftEnv,
        business_connection_id: BusinessConnectionId,
        star_gift_id: StarGiftId,
        mut promise: Promise<()>,
    ) {
        if business_connection_id.is_valid()
            && let Err(status) = env.check_business_connection(&business_connection_id)
        {
            return promise.set_error(status);
        }
        if star_gift_id.get_input_saved_star_gift(env).is_none() {
            return promise.set_error(Status::error(400, "Invalid gift identifier specified"));
        }
        let dialog_id = if business_connection_id.is_valid() {
            DialogId::from_user_id(env.get_business_connection_user_id(&business_connection_id))
        } else {
            star_gift_id.get_dialog_id(env)
        };
        // the ConvertStarGiftQuery head; the answer continues through
        // on_dialog_gift_transferred with to_dialog_id empty
        let _ = dialog_id;
        env.send_convert_star_gift_query(&star_gift_id);
        self.pending.unit_answers.push(promise);
    }

    /// `StarGiftManager::save_gift` (StarGiftManager.cpp:2887-2892).
    pub fn save_gift(
        &mut self,
        env: &mut impl StarGiftEnv,
        star_gift_id: StarGiftId,
        is_saved: bool,
        mut promise: Promise<()>,
    ) {
        if star_gift_id.get_input_saved_star_gift(env).is_none() {
            return promise.set_error(Status::error(400, "Invalid gift identifier specified"));
        }
        env.send_save_star_gift_query(&star_gift_id, is_saved);
        self.pending.unit_answers.push(promise);
    }

    /// `StarGiftManager::check_star_gift_id` (StarGiftManager.cpp:2894-2902).
    fn check_star_gift_id(
        &self,
        env: &mut impl StarGiftEnv,
        star_gift_id: &StarGiftId,
        dialog_id: DialogId,
    ) -> Result<(), Status> {
        if star_gift_id.get_input_saved_star_gift(env).is_none() {
            return Err(Status::error(400, "Invalid gift identifier specified"));
        }
        if dialog_id != DialogId::default() && star_gift_id.get_dialog_id(env) != dialog_id {
            return Err(Status::error(400, "The gift is not from the chat"));
        }
        Ok(())
    }

    /// `StarGiftManager::check_star_gift_ids` (StarGiftManager.cpp:2904-2909).
    fn check_star_gift_ids(
        &self,
        env: &mut impl StarGiftEnv,
        star_gift_ids: &[StarGiftId],
        dialog_id: DialogId,
    ) -> Result<(), Status> {
        for star_gift_id in star_gift_ids {
            self.check_star_gift_id(env, star_gift_id, dialog_id)?;
        }
        Ok(())
    }

    /// `StarGiftManager::set_dialog_pinned_gifts`
    /// (StarGiftManager.cpp:2911-2917).
    pub fn set_dialog_pinned_gifts(
        &mut self,
        env: &mut impl StarGiftEnv,
        dialog_id: DialogId,
        star_gift_ids: Vec<StarGiftId>,
        mut promise: Promise<()>,
    ) {
        if let Err(status) = env.check_dialog_access(dialog_id, "set_dialog_pinned_gifts") {
            return promise.set_error(status);
        }
        if let Err(status) = self.check_star_gift_ids(env, &star_gift_ids, dialog_id) {
            return promise.set_error(status);
        }
        env.send_toggle_star_gifts_pinned_to_top_query(&star_gift_ids);
        self.pending.unit_answers.push(promise);
    }

    /// `StarGiftManager::toggle_chat_star_gift_notifications`
    /// (StarGiftManager.cpp:2919-2928).
    pub fn toggle_chat_star_gift_notifications(
        &mut self,
        env: &mut impl StarGiftEnv,
        dialog_id: DialogId,
        are_enabled: bool,
        mut promise: Promise<()>,
    ) {
        if let Err(status) =
            env.check_dialog_access(dialog_id, "toggle_chat_star_gift_notifications")
        {
            return promise.set_error(status);
        }
        if !env.is_broadcast_channel(dialog_id)
            || !env.can_channel_post_messages(dialog_id.get_channel_id())
        {
            return promise.set_error(Status::error(400, "Wrong chat specified"));
        }
        env.send_toggle_chat_star_gift_notifications_query(dialog_id, are_enabled);
        self.pending.unit_answers.push(promise);
    }

    // -- the upgrade plane --

    /// `StarGiftManager::get_gift_upgrade_preview`
    /// (StarGiftManager.cpp:2930-2933).
    pub fn get_gift_upgrade_preview(
        &mut self,
        env: &mut impl StarGiftEnv,
        gift_id: i64,
        promise: Promise<td_api::GiftUpgradePreview>,
    ) {
        env.send_get_upgrade_gift_preview_query(gift_id);
        self.pending.gift_upgrade_preview.push(promise);
    }

    /// `GetUpgradeGiftPreviewQuery::on_result` (StarGiftManager.cpp:641-698).
    pub fn on_get_gift_upgrade_preview(
        &mut self,
        env: &mut impl StarGiftEnv,
        result: Result<telegram_api::payments_StarGiftUpgradePreview, Status>,
    ) {
        let pending = std::mem::take(&mut self.pending.gift_upgrade_preview);
        let Ok(ptr) = result else {
            let status = result.unwrap_err();
            for mut query in pending {
                query.set_error(status.clone());
            }
            return;
        };
        let mut previews = td_api::GiftUpgradePreview {
            models: Vec::new(),
            symbols: Vec::new(),
            backdrops: Vec::new(),
            prices: Vec::new(),
            next_prices: Vec::new(),
        };
        for attribute in ptr.sample_attributes {
            match attribute {
                telegram_api::StarGiftAttribute::starGiftAttributeModel(attribute) => {
                    let model = StarGiftAttributeSticker::from_model(env, attribute);
                    if !model.is_valid() {
                        tdutils::log_error!("Receive invalid model");
                    } else {
                        previews
                            .models
                            .push(model.get_upgraded_gift_model_object(env));
                    }
                }
                telegram_api::StarGiftAttribute::starGiftAttributePattern(attribute) => {
                    let pattern = StarGiftAttributeSticker::from_pattern(env, attribute);
                    if !pattern.is_valid() {
                        tdutils::log_error!("Receive invalid symbol");
                    } else {
                        previews
                            .symbols
                            .push(pattern.get_upgraded_gift_symbol_object(env));
                    }
                }
                telegram_api::StarGiftAttribute::starGiftAttributeBackdrop(attribute) => {
                    let backdrop = StarGiftAttributeBackdrop::from_telegram(attribute);
                    if !backdrop.is_valid() {
                        tdutils::log_error!("Receive invalid backdrop");
                    } else {
                        previews
                            .backdrops
                            .push(backdrop.get_upgraded_gift_backdrop_object());
                    }
                }
                telegram_api::StarGiftAttribute::starGiftAttributeOriginalDetails(_) => {
                    tdutils::log_error!("Receive unexpected original details");
                }
            }
        }
        for price in ptr.prices {
            previews.prices.push(td_api::GiftUpgradePrice {
                date: price.date,
                star_count: get_star_count(price.upgrade_stars, false),
            });
        }
        for price in ptr.next_prices {
            previews.next_prices.push(td_api::GiftUpgradePrice {
                date: price.date,
                star_count: get_star_count(price.upgrade_stars, false),
            });
        }
        for mut query in pending {
            query.set_value(previews.clone());
        }
    }

    /// `StarGiftManager::get_gift_upgrade_variants`
    /// (StarGiftManager.cpp:2935-2939).
    pub fn get_gift_upgrade_variants(
        &mut self,
        env: &mut impl StarGiftEnv,
        gift_id: i64,
        return_upgrade_models: bool,
        return_craft_models: bool,
        promise: Promise<td_api::GiftUpgradeVariants>,
    ) {
        env.send_get_star_gift_upgrade_attributes_query(gift_id);
        self.pending.gift_upgrade_variants.push((
            return_upgrade_models,
            return_craft_models,
            promise,
        ));
    }

    /// `GetStarGiftUpgradeAttributesQuery::on_result`
    /// (StarGiftManager.cpp:721-770).
    pub fn on_get_gift_upgrade_variants(
        &mut self,
        env: &mut impl StarGiftEnv,
        result: Result<telegram_api::payments_StarGiftUpgradeAttributes, Status>,
    ) {
        let pending = std::mem::take(&mut self.pending.gift_upgrade_variants);
        let Ok(ptr) = result else {
            let status = result.unwrap_err();
            for (_, _, mut query) in pending {
                query.set_error(status.clone());
            }
            return;
        };
        let mut variants = td_api::GiftUpgradeVariants {
            models: Vec::new(),
            symbols: Vec::new(),
            backdrops: Vec::new(),
        };
        for attribute in ptr.attributes {
            match attribute {
                telegram_api::StarGiftAttribute::starGiftAttributeModel(attribute) => {
                    let model = StarGiftAttributeSticker::from_model(env, attribute);
                    if !model.is_valid() {
                        tdutils::log_error!("Receive invalid model");
                    } else {
                        for (return_upgrade_models, return_craft_models, _) in &pending {
                            if (!model.is_crafted() && *return_upgrade_models)
                                || (model.is_crafted() && *return_craft_models)
                            {
                                variants
                                    .models
                                    .push(model.get_upgraded_gift_model_object(env));
                                break;
                            }
                        }
                    }
                }
                telegram_api::StarGiftAttribute::starGiftAttributePattern(attribute) => {
                    let pattern = StarGiftAttributeSticker::from_pattern(env, attribute);
                    if !pattern.is_valid() {
                        tdutils::log_error!("Receive invalid symbol");
                    } else {
                        variants
                            .symbols
                            .push(pattern.get_upgraded_gift_symbol_object(env));
                    }
                }
                telegram_api::StarGiftAttribute::starGiftAttributeBackdrop(attribute) => {
                    let backdrop = StarGiftAttributeBackdrop::from_telegram(attribute);
                    if !backdrop.is_valid() {
                        tdutils::log_error!("Receive invalid backdrop");
                    } else {
                        variants
                            .backdrops
                            .push(backdrop.get_upgraded_gift_backdrop_object());
                    }
                }
                telegram_api::StarGiftAttribute::starGiftAttributeOriginalDetails(_) => {
                    tdutils::log_error!("Receive unexpected original details");
                }
            }
        }
        for (_, _, mut query) in pending {
            query.set_value(variants.clone());
        }
    }

    /// `StarGiftManager::upgrade_gift` (StarGiftManager.cpp:2941-2969).
    pub fn upgrade_gift(
        &mut self,
        env: &mut impl StarGiftEnv,
        business_connection_id: BusinessConnectionId,
        star_gift_id: StarGiftId,
        keep_original_details: bool,
        star_count: i64,
        mut promise: Promise<td_api::UpgradeGiftResult>,
    ) {
        let as_business = business_connection_id.is_valid();
        if as_business && let Err(status) = env.check_business_connection(&business_connection_id) {
            return promise.set_error(status);
        }
        if star_gift_id.get_input_saved_star_gift(env).is_none() {
            return promise.set_error(Status::error(400, "Invalid gift identifier specified"));
        }
        if star_count < 0 {
            return promise.set_error(Status::error(
                400,
                "Invalid amount of Telegram Stars specified",
            ));
        }
        if star_count != 0 {
            if !as_business && !env.has_owned_star_count(star_count) {
                return promise.set_error(Status::error(400, "Have not enough Telegram Stars"));
            }
            env.add_pending_owned_star_count(-star_count, false);
        }
        env.send_upgrade_gift_query(star_count, keep_original_details);
        self.pending.star_gift_result.push(promise);
    }

    /// `StarGiftManager::buy_gift_upgrade` (StarGiftManager.cpp:2971-2990).
    pub fn buy_gift_upgrade(
        &mut self,
        env: &mut impl StarGiftEnv,
        dialog_id: DialogId,
        prepaid_upgrade_hash: &str,
        star_count: i64,
        mut promise: Promise<()>,
    ) {
        let input_peer = env.get_input_peer(dialog_id);
        let upgrade_input_peer = env.get_input_peer(dialog_id);
        if input_peer.is_none() || upgrade_input_peer.is_none() {
            return promise.set_error(Status::error(400, "Have no access to the gift owner"));
        }
        if star_count <= 0 {
            return promise.set_error(Status::error(
                400,
                "Invalid amount of Telegram Stars specified",
            ));
        }
        if !env.has_owned_star_count(star_count) {
            return promise.set_error(Status::error(400, "Have not enough Telegram Stars"));
        }
        let _ = prepaid_upgrade_hash;
        env.add_pending_owned_star_count(-star_count, false);
        env.send_get_gift_payment_form_query(0, star_count, false, false, false);
        self.pending.unit_answers.push(promise);
    }

    /// `StarGiftManager::craft_gift` (StarGiftManager.cpp:2992-2996).
    pub fn craft_gift(
        &mut self,
        env: &mut impl StarGiftEnv,
        star_gift_ids: Vec<StarGiftId>,
        mut promise: Promise<td_api::CraftGiftResult>,
    ) {
        let my_dialog_id = env.get_my_dialog_id();
        if let Err(status) = self.check_star_gift_ids(env, &star_gift_ids, my_dialog_id) {
            return promise.set_error(status);
        }
        env.send_craft_star_gift_query(&star_gift_ids);
        self.pending.craft_result.push(promise);
    }

    /// `StarGiftManager::transfer_gift` (StarGiftManager.cpp:2998-3042).
    pub fn transfer_gift(
        &mut self,
        env: &mut impl StarGiftEnv,
        business_connection_id: BusinessConnectionId,
        star_gift_id: StarGiftId,
        receiver_dialog_id: DialogId,
        star_count: i64,
        mut promise: Promise<()>,
    ) {
        let as_business = business_connection_id.is_valid();
        if as_business && let Err(status) = env.check_business_connection(&business_connection_id) {
            return promise.set_error(status);
        }
        let input_peer = env.get_input_peer(receiver_dialog_id);
        let transfer_input_peer = env.get_input_peer(receiver_dialog_id);
        if input_peer.is_none() || transfer_input_peer.is_none() {
            return promise.set_error(Status::error(400, "Have no access to the new gift owner"));
        }
        if star_gift_id.get_input_saved_star_gift(env).is_none() {
            return promise.set_error(Status::error(400, "Invalid gift identifier specified"));
        }
        if star_count < 0 {
            return promise.set_error(Status::error(
                400,
                "Invalid amount of Telegram Stars specified",
            ));
        }
        let dialog_id = if as_business {
            DialogId::from_user_id(env.get_business_connection_user_id(&business_connection_id))
        } else {
            star_gift_id.get_dialog_id(env)
        };
        let _ = dialog_id;
        if star_count != 0 && !as_business && !env.has_owned_star_count(star_count) {
            return promise.set_error(Status::error(400, "Have not enough Telegram Stars"));
        }
        if star_count != 0 {
            env.add_pending_owned_star_count(-star_count, false);
        }
        env.send_transfer_gift_query(star_count);
        self.pending.unit_answers.push(promise);
    }

    /// `StarGiftManager::on_dialog_gift_transferred`
    /// (StarGiftManager.cpp:3044-3059) — the public fire point of the
    /// transfer/convert query continuations; the gift-count updates ride
    /// the env seams.
    pub fn on_dialog_gift_transferred(
        &mut self,
        env: &mut impl StarGiftEnv,
        from_dialog_id: DialogId,
        to_dialog_id: DialogId,
        mut promise: Promise<()>,
    ) {
        let my_dialog_id = env.get_my_dialog_id();
        if from_dialog_id == my_dialog_id {
            env.on_update_my_gift_count(-1);
        } else if from_dialog_id.get_type() == DialogType::Channel {
            env.on_update_channel_gift_count(from_dialog_id.get_channel_id(), -1, true);
        }
        if to_dialog_id == my_dialog_id {
            env.on_update_my_gift_count(1);
        } else if to_dialog_id.get_type() == DialogType::Channel
            && env.can_channel_post_messages(to_dialog_id.get_channel_id())
        {
            env.on_update_channel_gift_count(to_dialog_id.get_channel_id(), 1, true);
        }
        promise.set_value(());
    }

    /// `StarGiftManager::drop_gift_original_details`
    /// (StarGiftManager.cpp:3061-3078).
    pub fn drop_gift_original_details(
        &mut self,
        env: &mut impl StarGiftEnv,
        star_gift_id: StarGiftId,
        star_count: i64,
        mut promise: Promise<()>,
    ) {
        if star_gift_id.get_input_saved_star_gift(env).is_none() {
            return promise.set_error(Status::error(400, "Invalid gift identifier specified"));
        }
        if star_count <= 0 {
            return promise.set_error(Status::error(
                400,
                "Invalid amount of Telegram Stars specified",
            ));
        }
        if !env.has_owned_star_count(star_count) {
            return promise.set_error(Status::error(400, "Have not enough Telegram Stars"));
        }
        env.add_pending_owned_star_count(-star_count, false);
        env.send_drop_gift_original_details_query(star_count);
        self.pending.unit_answers.push(promise);
    }

    /// `StarGiftManager::send_resold_gift` (StarGiftManager.cpp:3080-3114).
    #[allow(clippy::too_many_arguments)] // the C++ manager signature
    pub fn send_resold_gift(
        &mut self,
        env: &mut impl StarGiftEnv,
        gift_name: &str,
        receiver_dialog_id: DialogId,
        price: StarGiftResalePrice,
        text: Option<td_api::FormattedText>,
        is_private: bool,
        mut promise: Promise<td_api::GiftResaleResult>,
    ) {
        let input_peer = env.get_input_peer(receiver_dialog_id);
        let resale_input_peer = env.get_input_peer(receiver_dialog_id);
        if input_peer.is_none() || resale_input_peer.is_none() {
            return promise.set_error(Status::error(400, "Have no access to the new gift owner"));
        }
        if !env.has_owned_amount(&price) {
            return promise.set_error(Status::error(400, "Have not enough funds"));
        }
        let mut message = match message_entity::get_formatted_text_of_input::<()>(
            None,
            to_entity_dialog_id(env.get_my_dialog_id()),
            text,
            false,
            true,
            true,
            false,
            None,
            env.get_option_boolean("always_parse_markdown"),
            0, // the session-count plane rides the connection integration
            None,
            false,
        ) {
            Result::Ok(message) => message,
            Result::Err(status) => return promise.set_error(status),
        };
        message_entity::remove_unallowed_quote_entities(&mut message);
        if !message.text.is_empty() {
            let _ = message_entity::get_input_text_with_entities::<()>(
                None,
                &message,
                "send_resold_gift",
            );
        }
        let my_dialog_id = env.get_my_dialog_id();
        let star_gift_id = if receiver_dialog_id == my_dialog_id {
            StarGiftId::from_slug(gift_name)
        } else {
            StarGiftId::default()
        };
        let _ = star_gift_id;
        let _ = is_private;
        env.send_resale_gift_query(gift_name);
        self.pending.gift_resale_result.push(promise);
    }

    /// `StarGiftManager::send_gift_offer` (StarGiftManager.cpp:3116-3135).
    #[allow(clippy::too_many_arguments)] // the C++ manager signature
    pub fn send_gift_offer(
        &mut self,
        env: &mut impl StarGiftEnv,
        owner_dialog_id: DialogId,
        gift_name: &str,
        price: StarGiftResalePrice,
        duration: i32,
        paid_message_star_count: i64,
        mut promise: Promise<()>,
    ) {
        let _ = duration;
        if !env.have_dialog_force(owner_dialog_id, "send_gift_offer") {
            return promise.set_error(Status::error(400, "Can't access the chat"));
        }
        if paid_message_star_count < 0 {
            return promise.set_error(Status::error(
                400,
                "Invalid amount of Telegram Stars specified",
            ));
        }
        if !env.has_owned_star_count(paid_message_star_count) {
            return promise.set_error(Status::error(400, "Have not enough Telegram Stars"));
        }
        env.add_pending_owned_star_count(-paid_message_star_count, false);
        if !env.has_owned_amount(&price) {
            env.add_pending_owned_star_count(paid_message_star_count, false);
            return promise.set_error(Status::error(400, "Have not enough funds"));
        }
        env.add_pending_owned_amount(&price, -1, false);
        env.send_send_star_gift_offer_query(gift_name);
        self.pending.unit_answers.push(promise);
    }

    /// `StarGiftManager::process_gift_offer` (StarGiftManager.cpp:3137-3142).
    pub fn process_gift_offer(
        &mut self,
        env: &mut impl StarGiftEnv,
        message_id: i64,
        decline: bool,
        mut promise: Promise<()>,
    ) {
        // `MessageId(message_id).is_server()` — a positive raw id with a
        // zero low SERVER_ID_SHIFT part (MessageId.h); the full MessageId
        // layout is the message-sending step's.
        const SERVER_ID_SHIFT: i64 = 20;
        let is_server = message_id > 0 && message_id & ((1 << SERVER_ID_SHIFT) - 1) == 0;
        if !is_server {
            return promise.set_error(Status::error(
                400,
                "Invalid offer message identifier specified",
            ));
        }
        env.send_resolve_star_gift_offer_query((message_id >> SERVER_ID_SHIFT) as i32, decline);
        self.pending.resolve_star_gift_offer.push(promise);
    }

    // -- the saved-gifts plane --

    /// `StarGiftManager::get_saved_star_gifts` (StarGiftManager.cpp:3144-3160).
    #[allow(clippy::too_many_arguments)]
    pub fn get_saved_star_gifts(
        &mut self,
        env: &mut impl StarGiftEnv,
        business_connection_id: BusinessConnectionId,
        dialog_id: DialogId,
        collection_id: StarGiftCollectionId,
        exclude_unsaved: bool,
        exclude_saved: bool,
        exclude_unlimited: bool,
        exclude_upgradable: bool,
        exclude_non_upgradable: bool,
        exclude_unique: bool,
        peer_color_available: bool,
        exclude_hosted: bool,
        sort_by_value: bool,
        offset: &str,
        limit: i32,
        mut promise: Promise<td_api::ReceivedGifts>,
    ) {
        let _ = (
            exclude_unsaved,
            exclude_saved,
            exclude_unlimited,
            exclude_upgradable,
        );
        let _ = (
            exclude_non_upgradable,
            exclude_unique,
            peer_color_available,
            exclude_hosted,
        );
        let _ = (sort_by_value, offset);
        if limit < 0 {
            return promise.set_error(Status::error(400, "Limit must be non-negative"));
        }
        if business_connection_id.is_valid()
            && let Err(status) = env.check_business_connection(&business_connection_id)
        {
            return promise.set_error(status);
        }
        let dialog_id = if business_connection_id.is_valid() {
            DialogId::from_user_id(env.get_business_connection_user_id(&business_connection_id))
        } else {
            dialog_id
        };
        let is_full_list = !collection_id.is_valid()
            && !exclude_unsaved
            && !exclude_saved
            && !exclude_unlimited
            && !exclude_upgradable
            && !exclude_non_upgradable
            && !exclude_unique
            && !peer_color_available
            && !exclude_hosted;
        if env.get_input_peer(dialog_id).is_none() {
            return promise.set_error(Status::error(400, "Can't access the chat"));
        }
        env.send_get_saved_star_gifts_query(dialog_id);
        self.pending
            .saved_star_gifts
            .push((dialog_id, is_full_list, promise));
    }

    /// `GetSavedStarGiftsQuery::on_result` (StarGiftManager.cpp:1690-1739).
    pub fn on_get_saved_star_gifts(
        &mut self,
        env: &mut impl StarGiftEnv,
        result: Result<telegram_api::payments_SavedStarGifts, Status>,
    ) {
        let pending = std::mem::take(&mut self.pending.saved_star_gifts);
        let Ok(ptr) = result else {
            let status = result.unwrap_err();
            for (_, _, mut query) in pending {
                query.set_error(status.clone());
            }
            return;
        };
        // the single-query collapse: one answer completes every parked
        // get_saved_star_gifts (the C++ keeps dialog_id_/is_full_list_ on
        // the query object)
        let (dialog_id, is_full_list) = match pending.first() {
            Some((dialog_id, is_full_list, _)) => (*dialog_id, *is_full_list),
            None => return,
        };
        env.on_get_users(ptr.users, "GetSavedStarGiftsQuery");
        env.on_get_chats(ptr.chats, "GetSavedStarGiftsQuery");

        let mut total_count = ptr.count;
        if total_count < ptr.gifts.len() as i32 {
            tdutils::log_error!("Receive gifts with a too small total count");
            total_count = ptr.gifts.len() as i32;
        }
        let mut gifts = Vec::new();
        for gift in ptr.gifts {
            let user_gift = UserStarGift::from_telegram(env, self, gift, dialog_id);
            if !user_gift.is_valid() {
                tdutils::log_error!("Receive invalid user gift");
                continue;
            }
            gifts.push(user_gift.get_received_gift_object(env));
        }
        let mut are_notifications_enabled = false;
        if dialog_id.get_type() == DialogType::User {
            if dialog_id != env.get_my_dialog_id() {
                if is_full_list {
                    env.on_update_user_gift_count(dialog_id.get_user_id(), total_count);
                }
            } else {
                are_notifications_enabled = true;
            }
        } else if dialog_id.get_type() == DialogType::Channel {
            if is_full_list {
                env.on_update_channel_gift_count(dialog_id.get_channel_id(), total_count, false);
            }
            are_notifications_enabled = ptr.chat_notifications_enabled;
        }
        let answer = td_api::ReceivedGifts {
            total_count,
            gifts,
            are_notifications_enabled,
            next_offset: ptr.next_offset,
        };
        for (_, _, mut query) in pending {
            query.set_value(answer.clone());
        }
    }

    /// `StarGiftManager::get_saved_star_gift` (StarGiftManager.cpp:3162-3168).
    pub fn get_saved_star_gift(
        &mut self,
        env: &mut impl StarGiftEnv,
        star_gift_id: StarGiftId,
        mut promise: Promise<td_api::ReceivedGift>,
    ) {
        if !star_gift_id.is_valid() {
            return promise.set_error(Status::error(400, "Invalid gift identifier specified"));
        }
        let dialog_id = star_gift_id.get_dialog_id(env);
        env.send_get_saved_star_gift_query(&star_gift_id);
        self.pending.saved_star_gift.push((dialog_id, promise));
    }

    /// `GetSavedStarGiftQuery::on_result` (StarGiftManager.cpp:1762-1780).
    pub fn on_get_saved_star_gift(
        &mut self,
        env: &mut impl StarGiftEnv,
        result: Result<telegram_api::payments_SavedStarGifts, Status>,
    ) {
        let pending = std::mem::take(&mut self.pending.saved_star_gift);
        let Ok(ptr) = result else {
            let status = result.unwrap_err();
            for (_, mut query) in pending {
                query.set_error(status.clone());
            }
            return;
        };
        let (dialog_id, query) = match pending.into_iter().next() {
            Some(pending) => pending,
            None => return,
        };
        for gift in ptr.gifts {
            let user_gift = UserStarGift::from_telegram(env, self, gift, dialog_id);
            if !user_gift.is_valid() {
                tdutils::log_error!("Receive invalid user gift");
                continue;
            }
            let mut query = query;
            query.set_value(user_gift.get_received_gift_object(env));
            return;
        }
        let mut query = query;
        query.set_error(Status::error(400, "Gift not found"));
    }

    /// `StarGiftManager::get_craft_star_gifts` (StarGiftManager.cpp:3170-3176).
    pub fn get_craft_star_gifts(
        &mut self,
        env: &mut impl StarGiftEnv,
        gift_id: i64,
        offset: &str,
        limit: i32,
        mut promise: Promise<td_api::GiftsForCrafting>,
    ) {
        let _ = offset;
        if limit < 0 {
            return promise.set_error(Status::error(400, "Limit must be non-negative"));
        }
        env.send_get_craft_star_gifts_query(gift_id);
        self.pending.gifts_for_crafting.push(promise);
    }

    /// `GetCraftStarGiftsQuery::on_result` (StarGiftManager.cpp:1800-1846).
    pub fn on_get_craft_star_gifts(
        &mut self,
        env: &mut impl StarGiftEnv,
        result: Result<telegram_api::payments_SavedStarGifts, Status>,
    ) {
        let pending = std::mem::take(&mut self.pending.gifts_for_crafting);
        let Ok(ptr) = result else {
            let status = result.unwrap_err();
            for mut query in pending {
                query.set_error(status.clone());
            }
            return;
        };
        env.on_get_users(ptr.users, "GetCraftStarGiftsQuery");
        env.on_get_chats(ptr.chats, "GetCraftStarGiftsQuery");

        let mut total_count = ptr.count;
        if total_count < ptr.gifts.len() as i32 {
            tdutils::log_error!("Receive gifts with a too small total count");
            total_count = ptr.gifts.len() as i32;
        }
        let my_dialog_id = env.get_my_dialog_id();
        let mut gifts = Vec::new();
        for gift in ptr.gifts {
            let user_gift = UserStarGift::from_telegram(env, self, gift, my_dialog_id);
            if !user_gift.is_valid() {
                tdutils::log_error!("Receive invalid gift for crafting");
                continue;
            }
            if !user_gift.is_unique() {
                tdutils::log_error!("Receive non-unique gift for crafting");
                continue;
            }
            gifts.push(user_gift.get_received_gift_object(env));
        }
        let defaults = "100,80,400,70,350,700,60,300,600,1000";
        let option = env.get_option_string("stargifts_craft_attribute_permilles", defaults);
        let attribute_persistence_per_mille: Vec<i32> =
            option.split(',').map(parse_i32_prefix).collect();
        assert_eq!(attribute_persistence_per_mille.len(), 10);
        let mut probabilities = Vec::new();
        for num in 1..=4usize {
            let mut numbers = Vec::new();
            for i in 1..=4usize {
                numbers.push(if i <= num {
                    attribute_persistence_per_mille[num * (num - 1) / 2 + i - 1]
                } else {
                    0
                });
            }
            probabilities.push(td_api::AttributeCraftPersistenceProbability {
                persistence_chance_per_mille: numbers,
            });
        }
        let answer = td_api::GiftsForCrafting {
            total_count,
            gifts,
            attribute_persistence_probabilities: probabilities,
            next_offset: ptr.next_offset,
        };
        for mut query in pending {
            query.set_value(answer.clone());
        }
    }

    /// `StarGiftManager::get_upgraded_gift` (StarGiftManager.cpp:3178-3181).
    pub fn get_upgraded_gift(
        &mut self,
        env: &mut impl StarGiftEnv,
        name: &str,
        promise: Promise<td_api::UpgradedGift>,
    ) {
        env.send_get_unique_star_gift_query(name);
        self.pending.upgraded_gift.push(promise);
    }

    /// `GetUniqueStarGiftQuery::on_result` (StarGiftManager.cpp:1865-1883).
    pub fn on_get_upgraded_gift(
        &mut self,
        env: &mut impl StarGiftEnv,
        result: Result<telegram_api::payments_UniqueStarGift, Status>,
    ) {
        let pending = std::mem::take(&mut self.pending.upgraded_gift);
        let Ok(ptr) = result else {
            let status = result.unwrap_err();
            for mut query in pending {
                query.set_error(status.clone());
            }
            return;
        };
        env.on_get_users(ptr.users, "GetUniqueStarGiftQuery");
        env.on_get_chats(ptr.chats, "GetUniqueStarGiftQuery");

        let Some(gift) = ptr.gift else {
            for mut query in pending {
                query.set_error(Status::error(400, "Gift not found"));
            }
            return;
        };
        let star_gift = StarGift::from_telegram(env, gift, true);
        if !star_gift.is_valid() || !star_gift.is_unique() {
            tdutils::log_error!("Receive invalid user gift");
            for mut query in pending {
                query.set_error(Status::error(400, "Gift not found"));
            }
            return;
        }
        let answer = star_gift.get_upgraded_gift_object(env);
        for mut query in pending {
            query.set_value(answer.clone());
        }
    }

    /// `StarGiftManager::get_upgraded_gift_value_info`
    /// (StarGiftManager.cpp:3183-3186).
    pub fn get_upgraded_gift_value_info(
        &mut self,
        env: &mut impl StarGiftEnv,
        name: &str,
        promise: Promise<td_api::UpgradedGiftValueInfo>,
    ) {
        env.send_get_unique_star_gift_value_info_query(name);
        self.pending.upgraded_gift_value_info.push(promise);
    }

    /// `GetUniqueStarGiftValueInfoQuery::on_result`
    /// (StarGiftManager.cpp:1902-1916).
    pub fn on_get_upgraded_gift_value_info(
        &mut self,
        result: Result<telegram_api::payments_UniqueStarGiftValueInfo, Status>,
    ) {
        let pending = std::mem::take(&mut self.pending.upgraded_gift_value_info);
        let Ok(ptr) = result else {
            let status = result.unwrap_err();
            for mut query in pending {
                query.set_error(status.clone());
            }
            return;
        };
        let answer = td_api::UpgradedGiftValueInfo {
            currency: ptr.currency,
            value: ptr.value,
            is_value_average: ptr.value_is_average,
            initial_sale_date: 0.max(ptr.initial_sale_date),
            initial_sale_star_count: get_star_count(ptr.initial_sale_stars, false),
            initial_sale_price: ptr.initial_sale_price,
            last_sale_date: 0.max(ptr.last_sale_date),
            last_sale_price: ptr.last_sale_price,
            is_last_sale_on_fragment: ptr.last_sale_on_fragment,
            minimum_price: ptr.floor_price,
            average_sale_price: ptr.average_price,
            telegram_listed_gift_count: ptr.listed_count,
            fragment_listed_gift_count: ptr.fragment_listed_count,
            fragment_url: ptr.fragment_listed_url,
        };
        for mut query in pending {
            query.set_value(answer.clone());
        }
    }

    /// `StarGiftManager::get_star_gift_withdrawal_url`
    /// (StarGiftManager.cpp:3188-3206) — the srp callback hop collapses
    /// into the seam answer.
    pub fn get_star_gift_withdrawal_url(
        &mut self,
        env: &mut impl StarGiftEnv,
        star_gift_id: StarGiftId,
        password: &str,
        mut promise: Promise<String>,
    ) {
        if !star_gift_id.is_valid() {
            return promise.set_error(Status::error(400, "Invalid gift identifier specified"));
        }
        if password.is_empty() {
            return promise.set_error(Status::error(400, "PASSWORD_HASH_INVALID"));
        }
        let Some(input_check_password) = env.get_input_check_password_srp(password) else {
            // the parked PasswordManager callback — the promise rides the
            // seam until the password plane lands
            self.pending.star_gift_withdrawal_url.push(promise);
            return;
        };
        // send_get_star_gift_withdrawal_url_query
        // (StarGiftManager.cpp:3208-3215)
        let _ = input_check_password;
        if star_gift_id.get_input_saved_star_gift(env).is_none() {
            return promise.set_error(Status::error(400, "Gift not found"));
        }
        env.send_get_star_gift_withdrawal_url_query(&star_gift_id);
        self.pending.star_gift_withdrawal_url.push(promise);
    }

    /// `StarGiftManager::set_star_gift_price` (StarGiftManager.cpp:3217-3222).
    pub fn set_star_gift_price(
        &mut self,
        env: &mut impl StarGiftEnv,
        star_gift_id: StarGiftId,
        price: StarGiftResalePrice,
        mut promise: Promise<()>,
    ) {
        if !star_gift_id.is_valid() {
            return promise.set_error(Status::error(400, "Invalid gift identifier specified"));
        }
        let _ = price;
        env.send_update_star_gift_price_query(&star_gift_id);
        self.pending.unit_answers.push(promise);
    }

    /// `StarGiftManager::get_resale_star_gifts` (StarGiftManager.cpp:3224-3238).
    #[allow(clippy::too_many_arguments)] // the C++ manager signature
    pub fn get_resale_star_gifts(
        &mut self,
        env: &mut impl StarGiftEnv,
        gift_id: i64,
        order: Option<&td_api::GiftForResaleOrder>,
        for_craft: bool,
        for_stars: bool,
        attributes: &[td_api::UpgradedGiftAttributeId],
        offset: &str,
        limit: i32,
        mut promise: Promise<td_api::GiftsForResale>,
    ) {
        let _ = (for_craft, for_stars, offset);
        if limit < 0 {
            return promise.set_error(Status::error(400, "Limit must be non-negative"));
        }
        if order.is_none() {
            return promise.set_error(Status::error(400, "Gift sort order must be non-empty"));
        }
        let attribute_ids = match StarGiftAttributeId::get_star_gift_attribute_ids(attributes) {
            Ok(attribute_ids) => attribute_ids,
            Err(status) => return promise.set_error(status),
        };
        let _ = attribute_ids;
        env.send_get_resale_star_gifts_query(gift_id);
        self.pending.gifts_for_resale.push(promise);
    }

    /// `GetResaleStarGiftsQuery::on_result` (StarGiftManager.cpp:2012-2123).
    pub fn on_get_resale_star_gifts(
        &mut self,
        env: &mut impl StarGiftEnv,
        result: Result<telegram_api::payments_ResaleStarGifts, Status>,
    ) {
        let pending = std::mem::take(&mut self.pending.gifts_for_resale);
        let Ok(ptr) = result else {
            let status = result.unwrap_err();
            for mut query in pending {
                query.set_error(status.clone());
            }
            return;
        };
        env.on_get_users(ptr.users, "GetResaleStarGiftsQuery");
        env.on_get_chats(ptr.chats, "GetResaleStarGiftsQuery");

        let mut total_count = ptr.count;
        if total_count < ptr.gifts.len() as i32 {
            tdutils::log_error!("Receive gifts with a too small total count");
            total_count = ptr.gifts.len() as i32;
        }
        let mut gifts = Vec::new();
        for gift in ptr.gifts {
            let star_gift = StarGift::from_telegram(env, gift, true);
            if !star_gift.is_valid() || !star_gift.is_unique() {
                tdutils::log_error!("Receive invalid upgraded gift");
                continue;
            }
            gifts.push(star_gift.get_gift_for_resale_object(env));
        }
        let mut counters: HashMap<StarGiftAttributeId, i32> = HashMap::new();
        for counter in ptr.counters {
            if counter.count <= 0 {
                tdutils::log_error!("Receive an invalid attribute counter");
                continue;
            }
            let Some(attribute) = counter.attribute else {
                tdutils::log_error!("Receive an attribute counter without an attribute");
                continue;
            };
            counters.insert(StarGiftAttributeId::from_telegram(attribute), counter.count);
        }
        let get_count = |attribute_id: StarGiftAttributeId,
                         counters: &mut HashMap<StarGiftAttributeId, i32>|
         -> i32 {
            match counters.remove(&attribute_id) {
                Some(count) => count,
                None => {
                    tdutils::log_error!("Can't find counter for the attribute");
                    0
                }
            }
        };
        let mut models = Vec::new();
        let mut symbols = Vec::new();
        let mut backdrops = Vec::new();
        for attribute in ptr.attributes {
            match attribute {
                telegram_api::StarGiftAttribute::starGiftAttributeModel(attribute) => {
                    let model = StarGiftAttributeSticker::from_model(env, attribute);
                    if !model.is_valid() {
                        tdutils::log_error!("Receive invalid model");
                        continue;
                    }
                    let count = get_count(model.get_id(env, true), &mut counters);
                    if count > 0 {
                        models.push(td_api::UpgradedGiftModelCount {
                            model: Some(model.get_upgraded_gift_model_object(env)),
                            total_count: count,
                        });
                    }
                }
                telegram_api::StarGiftAttribute::starGiftAttributePattern(attribute) => {
                    let pattern = StarGiftAttributeSticker::from_pattern(env, attribute);
                    if !pattern.is_valid() {
                        tdutils::log_error!("Receive invalid symbol");
                        continue;
                    }
                    let count = get_count(pattern.get_id(env, false), &mut counters);
                    if count > 0 {
                        symbols.push(td_api::UpgradedGiftSymbolCount {
                            symbol: Some(pattern.get_upgraded_gift_symbol_object(env)),
                            total_count: count,
                        });
                    }
                }
                telegram_api::StarGiftAttribute::starGiftAttributeBackdrop(attribute) => {
                    let backdrop = StarGiftAttributeBackdrop::from_telegram(attribute);
                    if !backdrop.is_valid() {
                        tdutils::log_error!("Receive invalid backdrop");
                        continue;
                    }
                    let count = get_count(backdrop.get_id(), &mut counters);
                    if count > 0 {
                        backdrops.push(td_api::UpgradedGiftBackdropCount {
                            backdrop: Some(backdrop.get_upgraded_gift_backdrop_object()),
                            total_count: count,
                        });
                    }
                }
                telegram_api::StarGiftAttribute::starGiftAttributeOriginalDetails(_) => {
                    tdutils::log_error!("Receive original details");
                }
            }
        }
        models.sort_by_key(|model| std::cmp::Reverse(model.total_count));
        symbols.sort_by_key(|symbol| std::cmp::Reverse(symbol.total_count));
        backdrops.sort_by_key(|backdrop| std::cmp::Reverse(backdrop.total_count));
        if !counters.is_empty() {
            tdutils::log_error!("Receive {} unused counters", counters.len());
        }
        let answer = td_api::GiftsForResale {
            total_count,
            gifts,
            models,
            symbols,
            backdrops,
            next_offset: ptr.next_offset,
        };
        for mut query in pending {
            query.set_value(answer.clone());
        }
    }

    // -- the collections plane --

    /// `StarGiftManager::get_gift_collections` (StarGiftManager.cpp:3240-3245).
    pub fn get_gift_collections(
        &mut self,
        env: &mut impl StarGiftEnv,
        dialog_id: DialogId,
        mut promise: Promise<td_api::GiftCollections>,
    ) {
        if let Err(status) = env.check_dialog_access(dialog_id, "get_gift_collections") {
            return promise.set_error(status);
        }
        env.send_get_star_gift_collections_query(dialog_id);
        self.pending.gift_collections.push(promise);
    }

    /// `GetStarGiftCollectionsQuery::on_result`
    /// (StarGiftManager.cpp:2147-2171).
    pub fn on_get_gift_collections(
        &mut self,
        env: &mut impl StarGiftEnv,
        result: Result<telegram_api::payments_StarGiftCollections, Status>,
    ) {
        let pending = std::mem::take(&mut self.pending.gift_collections);
        let Ok(ptr) = result else {
            let status = result.unwrap_err();
            for mut query in pending {
                query.set_error(status.clone());
            }
            return;
        };
        let answer = match ptr {
            telegram_api::payments_StarGiftCollections::payments_starGiftCollectionsNotModified(
                _,
            ) => {
                tdutils::log_error!("Receive collections not modified");
                td_api::GiftCollections {
                    collections: Vec::new(),
                }
            }
            telegram_api::payments_StarGiftCollections::payments_starGiftCollections(
                collections,
            ) => {
                let mut result = td_api::GiftCollections {
                    collections: Vec::new(),
                };
                for collection in collections.collections {
                    let gift_collection = StarGiftCollection::from_telegram(env, collection);
                    result
                        .collections
                        .push(gift_collection.get_gift_collection_object(env));
                }
                result
            }
        };
        for mut query in pending {
            query.set_value(answer.clone());
        }
    }

    /// `StarGiftManager::create_gift_collection` (StarGiftManager.cpp:3247-3254).
    pub fn create_gift_collection(
        &mut self,
        env: &mut impl StarGiftEnv,
        dialog_id: DialogId,
        title: &str,
        star_gift_ids: Vec<StarGiftId>,
        mut promise: Promise<td_api::GiftCollection>,
    ) {
        if let Err(status) = env.check_dialog_access(dialog_id, "create_gift_collection") {
            return promise.set_error(status);
        }
        if let Err(status) = self.check_star_gift_ids(env, &star_gift_ids, dialog_id) {
            return promise.set_error(status);
        }
        env.send_create_star_gift_collection_query(dialog_id, title);
        self.pending.gift_collection.push(promise);
    }

    /// `CreateStarGiftCollectionQuery::on_result`
    /// (StarGiftManager.cpp:2198-2208).
    pub fn on_create_gift_collection(
        &mut self,
        env: &mut impl StarGiftEnv,
        result: Result<telegram_api::StarGiftCollection, Status>,
    ) {
        let pending = std::mem::take(&mut self.pending.gift_collection);
        let Ok(ptr) = result else {
            let status = result.unwrap_err();
            for mut query in pending {
                query.set_error(status.clone());
            }
            return;
        };
        let gift_collection = StarGiftCollection::from_telegram(env, ptr);
        let answer = gift_collection.get_gift_collection_object(env);
        for mut query in pending {
            query.set_value(answer.clone());
        }
    }

    /// `StarGiftManager::reorder_gift_collections`
    /// (StarGiftManager.cpp:3256-3266).
    pub fn reorder_gift_collections(
        &mut self,
        env: &mut impl StarGiftEnv,
        dialog_id: DialogId,
        collection_ids: Vec<StarGiftCollectionId>,
        mut promise: Promise<()>,
    ) {
        if let Err(status) = env.check_dialog_access(dialog_id, "reorder_gift_collections") {
            return promise.set_error(status);
        }
        for collection_id in &collection_ids {
            if !collection_id.is_valid() {
                return promise.set_error(Status::error(
                    400,
                    "Invalid collection identifier specified",
                ));
            }
        }
        env.send_reorder_star_gift_collections_query(dialog_id);
        self.pending.unit_answers.push(promise);
    }

    /// `StarGiftManager::delete_gift_collection` (StarGiftManager.cpp:3268-3276).
    pub fn delete_gift_collection(
        &mut self,
        env: &mut impl StarGiftEnv,
        dialog_id: DialogId,
        collection_id: StarGiftCollectionId,
        mut promise: Promise<()>,
    ) {
        if let Err(status) = env.check_dialog_access(dialog_id, "delete_gift_collection") {
            return promise.set_error(status);
        }
        if !collection_id.is_valid() {
            return promise.set_error(Status::error(
                400,
                "Invalid collection identifier specified",
            ));
        }
        env.send_delete_star_gift_collection_query(dialog_id);
        self.pending.unit_answers.push(promise);
    }

    /// `StarGiftManager::set_gift_collection_title` (StarGiftManager.cpp:3278-3291).
    pub fn set_gift_collection_title(
        &mut self,
        env: &mut impl StarGiftEnv,
        dialog_id: DialogId,
        collection_id: StarGiftCollectionId,
        title: &str,
        mut promise: Promise<td_api::GiftCollection>,
    ) {
        if let Err(status) = env.check_dialog_access(dialog_id, "set_gift_collection_title") {
            return promise.set_error(status);
        }
        if !collection_id.is_valid() {
            return promise.set_error(Status::error(
                400,
                "Invalid collection identifier specified",
            ));
        }
        if title.is_empty() {
            return promise.set_error(Status::error(400, "Gift collection name must be non-empty"));
        }
        env.send_update_star_gift_collection_query(dialog_id, collection_id);
        self.pending.gift_collection.push(promise);
    }

    /// `StarGiftManager::add_gift_collection_gifts` (StarGiftManager.cpp:3293-3306).
    pub fn add_gift_collection_gifts(
        &mut self,
        env: &mut impl StarGiftEnv,
        dialog_id: DialogId,
        collection_id: StarGiftCollectionId,
        star_gift_ids: Vec<StarGiftId>,
        mut promise: Promise<td_api::GiftCollection>,
    ) {
        if let Err(status) = env.check_dialog_access(dialog_id, "add_gift_collection_gifts") {
            return promise.set_error(status);
        }
        if !collection_id.is_valid() {
            return promise.set_error(Status::error(
                400,
                "Invalid collection identifier specified",
            ));
        }
        if star_gift_ids.is_empty() {
            return promise.set_error(Status::error(400, "Gift list must be non-empty"));
        }
        if let Err(status) = self.check_star_gift_ids(env, &star_gift_ids, dialog_id) {
            return promise.set_error(status);
        }
        env.send_update_star_gift_collection_query(dialog_id, collection_id);
        self.pending.gift_collection.push(promise);
    }

    /// `StarGiftManager::remove_gift_collection_gifts` (StarGiftManager.cpp:3308-3321).
    pub fn remove_gift_collection_gifts(
        &mut self,
        env: &mut impl StarGiftEnv,
        dialog_id: DialogId,
        collection_id: StarGiftCollectionId,
        star_gift_ids: Vec<StarGiftId>,
        mut promise: Promise<td_api::GiftCollection>,
    ) {
        if let Err(status) = env.check_dialog_access(dialog_id, "remove_gift_collection_gifts") {
            return promise.set_error(status);
        }
        if !collection_id.is_valid() {
            return promise.set_error(Status::error(
                400,
                "Invalid collection identifier specified",
            ));
        }
        if star_gift_ids.is_empty() {
            return promise.set_error(Status::error(400, "Gift list must be non-empty"));
        }
        env.send_update_star_gift_collection_query(dialog_id, collection_id);
        self.pending.gift_collection.push(promise);
    }

    /// `StarGiftManager::reorder_gift_collection_gifts` (StarGiftManager.cpp:3323-3336).
    pub fn reorder_gift_collection_gifts(
        &mut self,
        env: &mut impl StarGiftEnv,
        dialog_id: DialogId,
        collection_id: StarGiftCollectionId,
        star_gift_ids: Vec<StarGiftId>,
        mut promise: Promise<td_api::GiftCollection>,
    ) {
        if let Err(status) = env.check_dialog_access(dialog_id, "reorder_gift_collection_gifts") {
            return promise.set_error(status);
        }
        if !collection_id.is_valid() {
            return promise.set_error(Status::error(
                400,
                "Invalid collection identifier specified",
            ));
        }
        if star_gift_ids.is_empty() {
            return promise.set_error(Status::error(400, "Gift list must be non-empty"));
        }
        env.send_update_star_gift_collection_query(dialog_id, collection_id);
        self.pending.gift_collection.push(promise);
    }

    /// `UpdateStarGiftCollectionQuery::on_result` (StarGiftManager.cpp:2317-2327)
    /// — shared by the title/add/remove/reorder update arms.
    pub fn on_update_gift_collection(
        &mut self,
        env: &mut impl StarGiftEnv,
        result: Result<telegram_api::StarGiftCollection, Status>,
    ) {
        self.on_create_gift_collection(env, result);
    }

    // -- the promo animation --

    /// `StarGiftManager::get_star_gift_promo_animation`
    /// (StarGiftManager.cpp:3338-3340).
    pub fn get_star_gift_promo_animation(
        &mut self,
        env: &mut impl StarGiftEnv,
        promise: Promise<td_api::Animation>,
    ) {
        env.send_get_star_gift_promo_animation_query();
        self.pending.star_gift_promo_animation.push(promise);
    }

    // -- the gift-message polling registry --

    /// `StarGiftManager::register_gift` (StarGiftManager.cpp:3342-3354).
    pub fn register_gift(
        &mut self,
        message_full_id: crate::message_id::MessageFullId,
        source: &str,
    ) {
        let message_id = message_full_id.get_message_id();
        if message_id.is_scheduled() {
            return;
        }
        assert!(message_id.get() > 0 && message_id.get() & ((1 << 20) - 1) == 0);
        tdutils::log_info!(
            "Register gift in {} from {}",
            message_full_id.get_message_id().get(),
            source
        );
        self.gift_message_count += 1;
        let gift_message_number = self.gift_message_count;
        self.gift_message_full_ids
            .insert(message_full_id, gift_message_number);
        self.gift_message_full_ids_by_id
            .insert(gift_message_number, message_full_id);
        // update_gift_message_timeout_.add_timeout_in(number, 0) — fires
        // through poll_gift (the scheduler seam)
    }

    /// `StarGiftManager::unregister_gift` (StarGiftManager.cpp:3356-3371).
    pub fn unregister_gift(
        &mut self,
        message_full_id: crate::message_id::MessageFullId,
        source: &str,
    ) {
        let message_id = message_full_id.get_message_id();
        if message_id.is_scheduled() {
            return;
        }
        assert!(message_id.get() > 0 && message_id.get() & ((1 << 20) - 1) == 0);
        tdutils::log_info!("Unregister gift in {} from {}", message_id.get(), source);
        let message_number = self
            .gift_message_full_ids
            .get(&message_full_id)
            .copied()
            .unwrap_or(0);
        if message_number == 0 {
            tdutils::log_error!("Unregistered gift was not found");
        }
        self.gift_message_full_ids_by_id.remove(&message_number);
        self.gift_message_full_ids.remove(&message_full_id);
    }

    /// `StarGiftManager::on_update_gift_message_timeout`
    /// (StarGiftManager.cpp:3400-3418) — the public fire point of
    /// `update_gift_message_timeout_`.
    pub fn poll_gift(&mut self, env: &mut impl StarGiftEnv, message_number: i64) {
        let Some(&message_full_id) = self.gift_message_full_ids_by_id.get(&message_number) else {
            return;
        };
        if self.being_reloaded_gift_messages.contains(&message_full_id) {
            return;
        }
        tdutils::log_info!(
            "Fetching gift from message {}",
            message_full_id.get_message_id().get()
        );
        self.being_reloaded_gift_messages.push(message_full_id);
        env.get_message_from_server(message_full_id);
        // the answer continues through on_gift_message_updated
    }

    /// `StarGiftManager::on_update_gift_message`
    /// (StarGiftManager.cpp:3420-3434) — the answer side of the poll.
    pub fn on_gift_message_updated(&mut self, message_full_id: crate::message_id::MessageFullId) {
        if let Some(pos) = self
            .being_reloaded_gift_messages
            .iter()
            .position(|&id| id == message_full_id)
        {
            self.being_reloaded_gift_messages.remove(pos);
        }
        if !self.gift_message_full_ids.contains_key(&message_full_id) {}
        // update_gift_message_timeout_.add_timeout_in(number, timeout) —
        // the scheduler seam re-arms the poll
    }

    /// `StarGiftManager::on_online` (StarGiftManager.cpp:3378-3388) — the
    /// StateManager callback hop; the timer re-arm is bookkeeping only.
    pub fn on_online(&mut self) {
        // gift_message_full_ids_.foreach: re-arm pending polls — the
        // collapsed port keeps no per-entry deadlines to refresh
    }

    /// `StarGiftManager::on_reload_gift_auction_timeout`
    /// (StarGiftManager.cpp:3446-3457) — the public fire point of
    /// `reload_gift_auction_timeout_`.
    pub fn reload_gift_auction(&mut self, env: &mut impl StarGiftEnv, gift_id: i64) {
        let version = self
            .gift_auction_infos
            .get(&gift_id)
            .map(|info| info.state.get_version())
            .unwrap_or(0);
        self.reload_gift_auction_state(
            env,
            telegram_api::InputStarGiftAuction::inputStarGiftAuction(
                telegram_api::inputStarGiftAuction { gift_id },
            ),
            version,
            PendingAuctionStateQuery::State(Promise::default()),
        );
    }
}

/// The map-key projection used after the auction-info entry move.
#[allow(dead_code)] // the C++ pointer-return analog; kept for parity
fn gift_id_of(gift: &StarGift) -> i64 {
    gift.get_id()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::FileId as FilesFileId;
    use tdutils::promise::PromiseInterface;

    /// The recording env: the sticker/file registry, the option values,
    /// the sent updates and the fired query seams.
    struct TestEnv {
        authorized: bool,
        bot: bool,
        unix_time: i32,
        options_integer: HashMap<String, i64>,
        options_string: HashMap<String, String>,
        users: Vec<i64>,
        my_dialog_id: DialogId,
        updates: Vec<td_api::Update>,
        queries: Vec<String>,
        // the sticker registry: document registration order → the file id
        // row bytes are the stored document id
        sticker_rows: HashMap<i32, i64>,
        next_document_id: i64,
        owned_star_count: i64,
        gift_counts: Vec<String>,
    }

    impl Default for TestEnv {
        fn default() -> Self {
            TestEnv {
                authorized: true,
                bot: false,
                unix_time: 1000,
                options_integer: HashMap::new(),
                options_string: HashMap::new(),
                users: Vec::new(),
                my_dialog_id: DialogId::from_user_id(UserId::new(42)),
                updates: Vec::new(),
                queries: Vec::new(),
                sticker_rows: HashMap::new(),
                next_document_id: 100,
                owned_star_count: i64::MAX,
                gift_counts: Vec::new(),
            }
        }
    }

    impl StarGiftEnv for TestEnv {
        fn is_authorized(&self) -> bool {
            self.authorized
        }

        fn is_bot(&self) -> bool {
            self.bot
        }

        fn unix_time(&self) -> i32 {
            self.unix_time
        }

        fn get_option_integer(&mut self, name: &str) -> i64 {
            *self.options_integer.get(name).unwrap_or(&0)
        }

        fn get_option_string(&mut self, name: &str, default_value: &str) -> String {
            self.options_string
                .get(name)
                .cloned()
                .unwrap_or_else(|| default_value.to_owned())
        }

        fn send_update(&mut self, update: td_api::Update) {
            self.updates.push(update);
        }

        fn on_get_users(&mut self, users: Vec<telegram_api::User>, source: &str) {
            self.queries.push(format!("{source}:{} users", users.len()));
        }

        fn on_get_chats(&mut self, chats: Vec<telegram_api::Chat>, source: &str) {
            self.queries.push(format!("{source}:{} chats", chats.len()));
        }

        fn have_user_force(&mut self, user_id: UserId, _source: &str) -> bool {
            self.users.contains(&user_id.get())
        }

        fn get_user_id_object(&mut self, user_id: UserId, _source: &str) -> i64 {
            user_id.get()
        }

        fn get_user_ids_object(&mut self, user_ids: &[UserId], _source: &str) -> Vec<i64> {
            user_ids.iter().map(|id| id.get()).collect()
        }

        fn on_update_my_gift_count(&mut self, delta: i32) {
            self.gift_counts.push(format!("my:{delta}"));
        }

        fn on_update_user_gift_count(&mut self, user_id: UserId, count: i32) {
            self.gift_counts
                .push(format!("user {}:{count}", user_id.get()));
        }

        fn get_my_dialog_id(&self) -> DialogId {
            self.my_dialog_id
        }

        fn force_create_dialog(&mut self, dialog_id: DialogId, _source: &str) {
            self.queries
                .push(format!("force_create_dialog:{}", dialog_id.get()));
        }

        fn check_dialog_access(
            &mut self,
            dialog_id: DialogId,
            _source: &str,
        ) -> Result<(), Status> {
            if dialog_id.is_valid() {
                Ok(())
            } else {
                Err(Status::error(400, "Chat not found"))
            }
        }

        fn get_input_peer(&mut self, dialog_id: DialogId) -> Option<telegram_api::InputPeer> {
            if dialog_id.is_valid() {
                Some(telegram_api::InputPeer::inputPeerEmpty(
                    telegram_api::inputPeerEmpty {},
                ))
            } else {
                None
            }
        }

        fn have_dialog_force(&mut self, dialog_id: DialogId, _source: &str) -> bool {
            dialog_id.is_valid()
        }

        fn get_chat_id_object(&mut self, dialog_id: DialogId, _source: &str) -> i64 {
            dialog_id.get()
        }

        fn is_broadcast_channel(&mut self, dialog_id: DialogId) -> bool {
            dialog_id.get_type() == DialogType::Channel
        }

        fn can_channel_post_messages(&mut self, _channel_id: ChannelId) -> bool {
            true
        }

        fn on_update_channel_gift_count(
            &mut self,
            channel_id: ChannelId,
            count: i32,
            is_relax: bool,
        ) {
            self.gift_counts
                .push(format!("channel {}:{count}:{is_relax}", channel_id.get()));
        }

        fn get_message_sender_object(
            &mut self,
            dialog_id: DialogId,
            _source: &str,
        ) -> Option<td_api::MessageSender> {
            match dialog_id.get_type() {
                DialogType::User => Some(td_api::MessageSender::messageSenderUser(
                    td_api::messageSenderUser {
                        user_id: dialog_id.get_user_id().get(),
                    },
                )),
                _ => Some(td_api::MessageSender::messageSenderChat(
                    td_api::messageSenderChat {
                        chat_id: dialog_id.get(),
                    },
                )),
            }
        }

        fn on_get_sticker_document(
            &mut self,
            document: Option<telegram_api::Document>,
            _source: &str,
        ) -> Option<FileId> {
            // the interim row: register a fresh document id; the row maps
            // it to itself so the store/parse legs stay symmetric
            self.next_document_id += 1;
            self.sticker_rows
                .insert(self.next_document_id as i32, self.next_document_id);
            let _ = document;
            Some(FilesFileId::new(self.next_document_id as i32, 0))
        }

        fn get_sticker_object(&mut self, file_id: FileId) -> Option<td_api::Sticker> {
            if self.sticker_rows.contains_key(&file_id.id()) {
                Some(td_api::Sticker::default())
            } else {
                None
            }
        }

        fn get_sticker_id(&mut self, file_id: FileId) -> i64 {
            file_id.id() as i64
        }

        fn store_sticker(&mut self, file_id: FileId, out: &mut Vec<u8>) {
            store_parse::store_i64(out, *self.sticker_rows.get(&file_id.id()).unwrap_or(&0));
        }

        fn parse_sticker(&mut self, parser: &mut Parser) -> FileId {
            // the row stores the document id; the parsed file id carries it
            let Ok(document_id) = parser.i64_() else {
                return FileId::default();
            };
            self.sticker_rows.insert(document_id as i32, document_id);
            FileId::new(document_id as i32, 0)
        }

        fn has_owned_star_count(&mut self, star_count: i64) -> bool {
            self.owned_star_count >= star_count
        }

        fn send_get_star_gifts_query(&mut self) {
            self.queries.push("getStarGifts".to_owned());
        }

        fn send_check_can_send_gift_query(&mut self, gift_id: i64) {
            self.queries.push(format!("checkCanSendGift:{gift_id}"));
        }

        fn send_get_star_gift_active_auctions_query(&mut self) {
            self.queries.push("getStarGiftActiveAuctions".to_owned());
        }

        fn send_get_unique_star_gift_query(&mut self, name: &str) {
            self.queries.push(format!("getUniqueStarGift:{name}"));
        }

        fn send_get_saved_star_gift_query(&mut self, _star_gift_id: &StarGiftId) {
            self.queries.push("getSavedStarGift".to_owned());
        }

        fn send_get_saved_star_gifts_query(&mut self, dialog_id: crate::dialog_id::DialogId) {
            self.queries
                .push(format!("getSavedStarGifts:{}", dialog_id.get()));
        }

        fn send_get_star_gift_auction_state_query(
            &mut self,
            auction: &telegram_api::InputStarGiftAuction,
            version: i32,
        ) {
            let target = match auction {
                telegram_api::InputStarGiftAuction::inputStarGiftAuction(auction) => {
                    format!("gift:{}", auction.gift_id)
                }
                telegram_api::InputStarGiftAuction::inputStarGiftAuctionSlug(auction) => {
                    format!("slug:{}", auction.slug)
                }
            };
            self.queries
                .push(format!("getStarGiftAuctionState:{target}:{version}"));
        }
    }

    use crate::files::FileId as FilesFileIdAccess;

    trait FileIdAccess {
        fn id(&self) -> i32;
    }

    impl FileIdAccess for FilesFileIdAccess {
        fn id(&self) -> i32 {
            self.get()
        }
    }

    // --- the test promise helpers (the M.40 RecordingPromise convention) ---

    struct Recorded<T: Send + 'static>(
        std::sync::Arc<std::sync::Mutex<Option<std::result::Result<T, Status>>>>,
    );

    struct RecordingPromise<T: Send + 'static> {
        recorded: Recorded<T>,
    }

    impl<T: Send + 'static> PromiseInterface<T> for RecordingPromise<T> {
        fn set_value(&mut self, value: T) {
            *self.recorded.0.lock().unwrap() = Some(Result::Ok(value));
        }

        fn set_error(&mut self, error: Status) {
            *self.recorded.0.lock().unwrap() = Some(Result::Err(error));
        }
    }

    fn recording_promise<T: Send + 'static>() -> (Promise<T>, Recorded<T>) {
        let recorded = Recorded(std::sync::Arc::new(std::sync::Mutex::new(None)));
        (
            Promise::from_interface(RecordingPromise {
                recorded: Recorded(recorded.0.clone()),
            }),
            recorded,
        )
    }

    impl<T: Send + 'static> Recorded<T> {
        fn value(&self) -> Option<std::result::Result<T, Status>>
        where
            T: Clone,
        {
            self.0.lock().unwrap().clone()
        }
    }

    fn user_gift_id_string(user_gift: &UserStarGift) -> String {
        // the received-gift object carries the id string
        let mut env = TestEnv::default();
        user_gift
            .get_received_gift_object(&mut env)
            .received_gift_id
    }

    fn sample_rarity(permille: i32) -> telegram_api::StarGiftAttributeRarity {
        telegram_api::StarGiftAttributeRarity::starGiftAttributeRarity(
            telegram_api::starGiftAttributeRarity { permille },
        )
    }

    fn sample_backdrop_attribute() -> telegram_api::StarGiftAttribute {
        telegram_api::StarGiftAttribute::starGiftAttributeBackdrop(
            telegram_api::starGiftAttributeBackdrop {
                name: "sky".to_owned(),
                backdrop_id: 7,
                center_color: 0x305070,
                edge_color: 0x203040,
                pattern_color: 0xFFFFFF,
                text_color: 0x000000,
                rarity: Some(sample_rarity(500)),
            },
        )
    }

    fn sample_unique_gift() -> telegram_api::StarGift {
        telegram_api::StarGift::starGiftUnique(telegram_api::starGiftUnique {
            flags: 0,
            require_premium: false,
            resale_ton_only: false,
            theme_available: false,
            burned: false,
            crafted: false,
            id: 777,
            gift_id: 5,
            title: "Sky Gift".to_owned(),
            slug: "SkyGift".to_owned(),
            num: 42,
            owner_id: Some(telegram_api::Peer::peerUser(telegram_api::peerUser {
                user_id: 42,
            })),
            owner_name: String::new(),
            owner_address: String::new(),
            attributes: vec![
                telegram_api::StarGiftAttribute::starGiftAttributeModel(
                    telegram_api::starGiftAttributeModel {
                        flags: 0,
                        crafted: false,
                        name: "rocket".to_owned(),
                        document: None,
                        rarity: Some(sample_rarity(500)),
                    },
                ),
                telegram_api::StarGiftAttribute::starGiftAttributePattern(
                    telegram_api::starGiftAttributePattern {
                        name: "star".to_owned(),
                        document: None,
                        rarity: Some(sample_rarity(100)),
                    },
                ),
                sample_backdrop_attribute(),
            ],
            availability_issued: 100,
            availability_total: 1000,
            gift_address: String::new(),
            resell_amount: vec![
                telegram_api::StarsAmount::starsAmount(telegram_api::starsAmount {
                    amount: 250,
                    nanos: 0,
                }),
                telegram_api::StarsAmount::starsTonAmount(telegram_api::starsTonAmount {
                    amount: 20_000_000,
                }),
            ],
            released_by: None,
            value_amount: 0,
            value_currency: String::new(),
            value_usd_amount: 0,
            theme_peer: None,
            peer_color: None,
            host_id: Some(telegram_api::Peer::peerUser(telegram_api::peerUser {
                user_id: 1,
            })),
            offer_min_stars: 30,
            craft_chance_permille: 0,
        })
    }

    #[test]
    fn star_gift_attribute_rarity_store_parse_round_trip() {
        for rarity in [
            StarGiftAttributeRarity::from_telegram(sample_rarity(500)),
            StarGiftAttributeRarity::from_telegram(
                telegram_api::StarGiftAttributeRarity::starGiftAttributeRarityLegendary(
                    telegram_api::starGiftAttributeRarityLegendary,
                ),
            ),
            StarGiftAttributeRarity::default(), // the invalid -1 permille
        ] {
            let mut out = Vec::new();
            rarity.store(&mut out);
            let mut parser = Parser::new(&out);
            let parsed = StarGiftAttributeRarity::parse_from(&mut parser).expect("parse");
            assert_eq!(parsed, rarity);
        }
        assert!(!StarGiftAttributeRarity::default().is_valid());
        assert!(!StarGiftAttributeRarity::from_telegram(sample_rarity(1001)).is_valid());
    }

    #[test]
    fn star_gift_background_store_parse_round_trip_and_object() {
        let background = StarGiftBackground {
            center_color: 1,
            edge_color: 2,
            text_color: 3,
        };
        assert_eq!(
            background.get_gift_background_object(),
            td_api::GiftBackground {
                center_color: 1,
                edge_color: 2,
                text_color: 3,
            }
        );
        let mut out = Vec::new();
        background.store(&mut out);
        let mut parser = Parser::new(&out);
        let parsed = StarGiftBackground::parse_from(&mut parser).expect("parse");
        assert_eq!(parsed, background);
    }

    #[test]
    fn star_gift_attribute_backdrop_store_parse_round_trip() {
        let attribute =
            StarGiftAttributeBackdrop::from_telegram(match sample_backdrop_attribute() {
                telegram_api::StarGiftAttribute::starGiftAttributeBackdrop(attribute) => attribute,
                _ => unreachable!(),
            });
        assert!(attribute.is_valid());
        assert_eq!(attribute.get_id(), StarGiftAttributeId::backdrop(7));
        let object = attribute.get_upgraded_gift_backdrop_object();
        assert_eq!(
            object.colors.as_ref().map(|colors| colors.symbol_color),
            Some(0xFFFFFF)
        );

        let mut out = Vec::new();
        attribute.store(&mut out);
        let mut parser = Parser::new(&out);
        let parsed = StarGiftAttributeBackdrop::parse_from(&mut parser).expect("parse");
        assert_eq!(parsed, attribute);

        // an out-of-range color invalidates the attribute
        let mut invalid = attribute;
        invalid.center_color = -1;
        assert!(!invalid.is_valid());
    }

    #[test]
    fn star_gift_attribute_sticker_store_parse_round_trip() {
        let mut env = TestEnv::default();
        let attribute = StarGiftAttributeSticker::from_model(
            &mut env,
            telegram_api::starGiftAttributeModel {
                flags: 0,
                crafted: true,
                name: "rocket".to_owned(),
                document: None,
                rarity: Some(sample_rarity(250)),
            },
        );
        assert!(attribute.is_valid());
        assert!(attribute.is_crafted());

        let mut out = Vec::new();
        attribute.store(&mut env, &mut out);
        let mut parser = Parser::new(&out);
        let parsed = StarGiftAttributeSticker::parse_from(&mut env, &mut parser).expect("parse");
        assert_eq!(parsed, attribute);
    }

    #[test]
    fn star_gift_attribute_original_details_store_parse_round_trip_and_object() {
        let mut env = TestEnv::default();
        let attribute = StarGiftAttributeOriginalDetails::from_telegram(
            telegram_api::starGiftAttributeOriginalDetails {
                flags: 0,
                sender_id: Some(telegram_api::Peer::peerUser(telegram_api::peerUser {
                    user_id: 42,
                })),
                recipient_id: Some(telegram_api::Peer::peerUser(telegram_api::peerUser {
                    user_id: 7,
                })),
                date: 2000,
                message: Some(telegram_api::TextWithEntities {
                    text: "congrats".to_owned(),
                    entities: Vec::new(),
                }),
            },
        );
        assert!(attribute.is_valid());

        let object = attribute
            .get_upgraded_gift_original_details_object(&mut env)
            .expect("object");
        assert!(matches!(
            object.sender_id,
            Some(td_api::MessageSender::messageSenderUser(_))
        ));

        let mut out = Vec::new();
        attribute.store(&mut out);
        let mut parser = Parser::new(&out);
        let parsed = StarGiftAttributeOriginalDetails::parse_from(&mut parser).expect("parse");
        assert_eq!(parsed, attribute);
    }

    #[test]
    fn star_gift_id_string_round_trip_and_store_parse() {
        // the string forms
        let user_id = StarGiftId::from_string("123");
        assert_eq!(
            user_id,
            StarGiftId::from_server_message_id(ServerMessageId::new(123))
        );
        let dialog_id = StarGiftId::from_string("-1000001234567_89");
        assert_eq!(dialog_id.get_star_gift_id(), "-1000001234567_89");
        let slug = StarGiftId::from_string("@name");
        assert_eq!(slug, StarGiftId::from_slug("name"));
        assert_eq!(slug.get_star_gift_id(), "@name");
        // a mismatching round trip resets to empty
        assert!(StarGiftId::from_string("12a3").is_empty());

        // the store/parse round trip over every variant
        for star_gift_id in [user_id, dialog_id.clone(), slug] {
            let mut out = Vec::new();
            star_gift_id.store(&mut out);
            let mut parser = Parser::new(&out);
            let parsed = StarGiftId::parse_from(&mut parser).expect("parse");
            assert_eq!(parsed, star_gift_id);
        }

        // the ForDialog input form rides the env input peer
        let mut env = TestEnv::default();
        assert!(dialog_id.get_input_saved_star_gift(&mut env).is_some());
        assert!(
            StarGiftId::default()
                .get_input_saved_star_gift(&mut env)
                .is_none()
        );
    }

    #[test]
    fn star_gift_resale_price_round_trip_and_validation() {
        // the star amount form
        let star_price = StarGiftResalePrice::from_telegram(Some(
            telegram_api::StarsAmount::starsAmount(telegram_api::starsAmount {
                amount: 500,
                nanos: 0,
            }),
        ));
        assert!(star_price.is_star());
        assert_eq!(star_price.get_star_count(), 500);

        // the nanostar price is logged and kept
        let nanostar_price = StarGiftResalePrice::from_telegram(Some(
            telegram_api::StarsAmount::starsAmount(telegram_api::starsAmount {
                amount: 500,
                nanos: 1,
            }),
        ));
        assert_eq!(nanostar_price.get_star_count(), 500);

        // the ton form divides the gram count
        let ton_price =
            StarGiftResalePrice::from_telegram(Some(telegram_api::StarsAmount::starsTonAmount(
                telegram_api::starsTonAmount { amount: 30_000_000 },
            )));
        assert!(ton_price.is_ton());
        assert_eq!(ton_price.get_ton_count(), 30_000_000);
        assert_eq!(ton_price.get_star_count(), 3);

        // the input amount round trip
        match ton_price.get_input_stars_amount() {
            telegram_api::StarsAmount::starsTonAmount(amount) => {
                assert_eq!(amount.amount, 30_000_000)
            }
            _ => panic!("wrong input amount"),
        }
        assert!(ton_price.get_gift_resale_price_object().is_some());
        assert!(
            StarGiftResalePrice::default()
                .get_gift_resale_price_object()
                .is_none()
        );

        // the store/parse round trips
        for price in [star_price, ton_price, StarGiftResalePrice::legacy(7)] {
            let mut out = Vec::new();
            price.store(&mut out);
            let mut parser = Parser::new(&out);
            let parsed = StarGiftResalePrice::parse_from(&mut parser).expect("parse");
            assert_eq!(parsed, price);
        }

        // the td_api validation: the bounds ride the option values
        let mut env = TestEnv::default();
        env.options_integer
            .insert("gift_resale_star_count_min".to_owned(), 10);
        env.options_integer
            .insert("gift_resale_star_count_max".to_owned(), 100);
        let price = td_api::GiftResalePrice::giftResalePriceStar(td_api::giftResalePriceStar {
            star_count: 50,
        });
        assert!(
            StarGiftResalePrice::get_star_gift_resale_price(&mut env, Some(price), false).is_ok()
        );
        let small = td_api::GiftResalePrice::giftResalePriceStar(td_api::giftResalePriceStar {
            star_count: 5,
        });
        let error = StarGiftResalePrice::get_star_gift_resale_price(&mut env, Some(small), false)
            .unwrap_err();
        assert_eq!(error.code(), 400);
        // a purchase skips the bounds but keeps the positivity gate
        let small = td_api::GiftResalePrice::giftResalePriceStar(td_api::giftResalePriceStar {
            star_count: 5,
        });
        assert!(
            StarGiftResalePrice::get_star_gift_resale_price(&mut env, Some(small), true).is_ok()
        );
        let zero = td_api::GiftResalePrice::giftResalePriceStar(td_api::giftResalePriceStar {
            star_count: 0,
        });
        assert!(
            StarGiftResalePrice::get_star_gift_resale_price(&mut env, Some(zero), true).is_err()
        );
        // the null price is only allowed off-purchase
        assert!(StarGiftResalePrice::get_star_gift_resale_price(&mut env, None, false).is_ok());
        assert!(StarGiftResalePrice::get_star_gift_resale_price(&mut env, None, true).is_err());
    }

    #[test]
    fn star_gift_regular_from_telegram_object_and_round_trip() {
        let mut env = TestEnv::default();
        let telegram_gift = telegram_api::StarGift::starGift(telegram_api::starGift {
            flags: (1 << 0) | (1 << 8) | (1 << 3),
            limited: true,
            sold_out: false,
            birthday: false,
            require_premium: false,
            limited_per_user: true,
            peer_color_available: false,
            auction: false,
            id: 1,
            sticker: None,
            stars: 25,
            availability_remains: 1000,
            availability_total: 2000,
            availability_resale: 5,
            convert_stars: 21,
            first_sale_date: 100,
            last_sale_date: 200,
            upgrade_stars: 7,
            resell_min_stars: 0,
            title: String::new(),
            released_by: Some(telegram_api::Peer::peerChannel(telegram_api::peerChannel {
                channel_id: 1,
            })),
            per_user_total: 10,
            per_user_remains: 5,
            locked_until_date: 0,
            auction_slug: String::new(),
            gifts_per_round: 0,
            auction_start_date: 0,
            upgrade_variants: 0,
            background: None,
        });
        env.unix_time = 0;
        let gift = StarGift::from_telegram(&mut env, telegram_gift, false);
        assert!(!gift.is_unique());
        assert!(gift.is_valid());
        assert_eq!(gift.get_id(), 1);
        assert_eq!(gift.get_star_count(), 25);
        assert_eq!(gift.get_upgrade_star_count(), 7);
        assert_eq!(gift.default_sell_star_count, 21);
        // fix_availability clamps the negative remains
        let mut total = 100i32;
        let mut remains = -1i32;
        StarGift::fix_availability(&mut total, &mut remains);
        assert_eq!((total, remains), (100, 0));
        let mut remains = 200i32;
        StarGift::fix_availability(&mut total, &mut remains);
        assert_eq!((total, remains), (100, 100));

        // the gift object; the released-by dialog was force-created
        assert!(
            env.queries
                .contains(&"force_create_dialog:-1000000000001".to_owned())
        );
        let object = gift.get_gift_object(&mut env, None);
        assert_eq!(object.star_count, 25);
        assert_eq!(object.default_sell_star_count, 21);
        assert_eq!(
            object
                .overall_limits
                .as_ref()
                .map(|limits| limits.total_count),
            Some(2000)
        );

        // the store/parse round trip
        let mut out = Vec::new();
        gift.store(&mut env, &mut out);
        let mut parser = Parser::new(&out);
        let parsed = StarGift::parse_from(&mut env, &mut parser).expect("parse");
        assert_eq!(parsed, gift);

        // the sent-gift form wraps the regular object
        assert!(matches!(
            gift.get_sent_gift_object(&mut env),
            td_api::SentGift::sentGiftRegular(_)
        ));
    }

    #[test]
    fn star_gift_unique_from_telegram_object_and_round_trip() {
        let mut env = TestEnv::default();
        let gift = StarGift::from_telegram(&mut env, sample_unique_gift(), true);
        assert!(gift.is_unique());
        assert!(gift.is_valid());
        assert_eq!(gift.get_id(), 777);

        // the resale parameters come from the stars + ton pair
        let object = gift.get_upgraded_gift_object(&mut env);
        assert_eq!(object.number, 42);
        let resale = object.resale_parameters.expect("resale parameters");
        assert_eq!(resale.star_count, 250);
        assert_eq!(resale.gram_cent_count, 2);

        // the store/parse round trip
        let mut out = Vec::new();
        gift.store(&mut env, &mut out);
        let mut parser = Parser::new(&out);
        let parsed = StarGift::parse_from(&mut env, &mut parser).expect("parse");
        assert_eq!(parsed, gift);

        // the resale object carries the slug-derived id for the owner
        let resale_object = gift.get_gift_for_resale_object(&mut env);
        assert_eq!(resale_object.received_gift_id, "@SkyGift");
        assert!(matches!(
            gift.get_sent_gift_object(&mut env),
            td_api::SentGift::sentGiftUpgraded(_)
        ));

        // a zero id bails the construction
        let mut invalid = match sample_unique_gift() {
            telegram_api::StarGift::starGiftUnique(mut star_gift) => {
                star_gift.id = 0;
                telegram_api::StarGift::starGiftUnique(star_gift)
            }
            _ => unreachable!(),
        };
        let _ = &mut invalid;
        let gift = StarGift::from_telegram(&mut env, invalid, true);
        assert_eq!(gift, StarGift::default());
    }

    #[test]
    fn user_star_gift_received_object_flow() {
        let mut env = TestEnv::default();
        let mut manager = StarGiftManager::new();
        let gift = telegram_api::SavedStarGift {
            flags: 0,
            name_hidden: false,
            unsaved: false,
            refunded: false,
            can_upgrade: true,
            pinned_to_top: false,
            upgrade_separate: false,
            from_id: Some(telegram_api::Peer::peerUser(telegram_api::peerUser {
                user_id: 7,
            })),
            date: 500,
            gift: Some(sample_unique_gift()),
            message: None,
            msg_id: 9,
            saved_id: 0,
            convert_stars: 10,
            upgrade_stars: 0,
            can_export_at: 0,
            transfer_stars: 0,
            can_transfer_at: 0,
            can_resell_at: 0,
            collection_id: vec![1, 0],
            prepaid_upgrade_hash: "hash".to_owned(),
            drop_original_details_stars: 0,
            gift_num: 3,
            can_craft_at: 0,
        };
        let user_gift = UserStarGift::from_telegram(
            &mut env,
            &mut manager,
            gift,
            DialogId::from_user_id(UserId::new(42)),
        );
        assert!(user_gift.is_valid());
        assert!(user_gift.is_unique());
        assert_eq!(user_gift_id_string(&user_gift), "9");

        let object = user_gift.get_received_gift_object(&mut env);
        assert_eq!(object.sell_star_count, 10);
        assert_eq!(object.prepaid_upgrade_hash, "hash");
        assert_eq!(object.collection_ids, vec![1]);
        assert!(matches!(
            object.gift,
            Some(td_api::SentGift::sentGiftUpgraded(_))
        ));

        // the intake registered the underlying regular-gift prices
        assert!(manager.gift_prices.is_empty()); // unique gifts are not cached
    }

    #[test]
    fn on_get_star_gift_price_cache_gates() {
        let mut env = TestEnv::default();
        let mut manager = StarGiftManager::new();
        let regular = StarGift::from_telegram(
            &mut env,
            telegram_api::StarGift::starGift(telegram_api::starGift {
                id: 1,
                stars: 25,
                upgrade_stars: 7,
                ..Default::default()
            }),
            false,
        );
        manager.on_get_star_gift(&mut env, &regular, true);
        assert_eq!(manager.gift_prices.get(&1), Some(&(25, 7)));
        // a non-server update keeps the first price
        manager.on_get_star_gift(&mut env, &regular, false);
        // a unique gift is not cached
        let unique = StarGift::from_telegram(&mut env, sample_unique_gift(), true);
        manager.on_get_star_gift(&mut env, &unique, true);
        assert!(!manager.gift_prices.contains_key(&777));

        // bots skip the cache entirely
        let mut env = TestEnv {
            bot: true,
            ..TestEnv::default()
        };
        let mut manager = StarGiftManager::new();
        manager.on_get_star_gift(&mut env, &regular, true);
        assert!(manager.gift_prices.is_empty());
    }

    #[test]
    fn get_gift_payment_options_parks_and_answer_completes() {
        let mut env = TestEnv::default();
        let mut manager = StarGiftManager::new();

        let (promise, recorded) = recording_promise();
        manager.get_gift_payment_options(&mut env, promise);
        assert_eq!(env.queries, vec!["getStarGifts"]);
        assert!(recorded.value().is_none());

        let answer = telegram_api::payments_starGifts {
            hash: 0,
            gifts: vec![telegram_api::StarGift::starGift(telegram_api::starGift {
                id: 1,
                stars: 25,
                availability_resale: 3,
                resell_min_stars: 20,
                title: "Heart".to_owned(),
                ..Default::default()
            })],
            chats: vec![],
            users: vec![],
        };
        manager.on_get_available_gifts(&mut env, Result::Ok(answer));
        assert_eq!(
            env.queries,
            vec![
                "getStarGifts",
                "GetStarGiftsQuery:0 users",
                "GetStarGiftsQuery:0 chats"
            ]
        );
        let answer = recorded.value().expect("answered").expect("ok");
        assert_eq!(answer.gifts.len(), 1);
        assert_eq!(answer.gifts[0].resale_count, 3);
        assert_eq!(answer.gifts[0].min_resale_star_count, 20);
        assert_eq!(answer.gifts[0].title, "Heart");
        // the intake cached the price
        assert_eq!(manager.gift_prices.get(&1), Some(&(25, 0)));

        // an error completes the parked promise with the status
        let (promise, recorded) = recording_promise();
        manager.get_gift_payment_options(&mut env, promise);
        manager.on_get_available_gifts(&mut env, Result::Err(Status::error(400, "oops")));
        assert_eq!(recorded.value().expect("answered").unwrap_err().code(), 400);
    }

    #[test]
    fn on_get_available_gifts_bot_filters() {
        let mut env = TestEnv {
            bot: true,
            ..TestEnv::default()
        };
        let mut manager = StarGiftManager::new();
        let answer = telegram_api::payments_starGifts {
            hash: 0,
            gifts: vec![
                // sold out → dropped for bots
                telegram_api::StarGift::starGift(telegram_api::starGift {
                    id: 1,
                    stars: 25,
                    availability_total: 10,
                    availability_remains: 0,
                    ..Default::default()
                }),
                // locked → dropped for bots
                telegram_api::StarGift::starGift(telegram_api::starGift {
                    id: 2,
                    stars: 25,
                    locked_until_date: 5000,
                    ..Default::default()
                }),
                // fine
                telegram_api::StarGift::starGift(telegram_api::starGift {
                    id: 3,
                    stars: 25,
                    ..Default::default()
                }),
            ],
            chats: vec![],
            users: vec![],
        };
        let (promise, recorded) = recording_promise();
        manager.get_gift_payment_options(&mut env, promise);
        manager.on_get_available_gifts(&mut env, Result::Ok(answer));
        let answer = recorded.value().expect("answered").expect("ok");
        assert_eq!(answer.gifts.len(), 1);
        assert_eq!(answer.gifts[0].gift.as_ref().map(|gift| gift.id), Some(3));
    }

    #[test]
    fn can_send_gift_parks_and_answers() {
        let mut env = TestEnv::default();
        let mut manager = StarGiftManager::new();
        let (promise, recorded) = recording_promise();
        manager.can_send_gift(&mut env, 5, promise);
        assert_eq!(env.queries, vec!["checkCanSendGift:5"]);

        manager.on_get_can_send_gift_result(
            &mut env,
            Result::Ok(
                telegram_api::payments_CheckCanSendGiftResult::payments_checkCanSendGiftResultOk(
                    telegram_api::payments_checkCanSendGiftResultOk,
                ),
            ),
        );
        assert!(matches!(
            recorded.value().expect("answered").expect("ok"),
            td_api::CanSendGiftResult::canSendGiftResultOk(_)
        ));
    }

    #[test]
    fn auction_bid_levels_filter_unsorted() {
        let bid =
            |pos: i32, amount: i64, date: i32| telegram_api::AuctionBidLevel { pos, amount, date };
        let levels = AuctionBidLevel::get_auction_bid_levels(&[
            bid(1, 50, 10),
            bid(2, 60, 11), // is_before needs a not-higher price → dropped
            bid(1, 70, 12), // a position regression → dropped
        ]);
        assert_eq!(levels.len(), 1);
        assert_eq!(levels[0].get_auction_bid_object().star_count, 50);
    }

    #[test]
    fn auction_state_from_telegram_and_object() {
        let mut env = TestEnv::default();
        let user_state = telegram_api::StarGiftAuctionUserState {
            flags: 0,
            returned: false,
            bid_amount: 100,
            bid_date: 50,
            min_bid_amount: 150,
            bid_peer: Some(telegram_api::Peer::peerUser(telegram_api::peerUser {
                user_id: 42,
            })),
            acquired_count: 2,
        };
        let parsed_user_state = StarGiftAuctionUserState::from_telegram(&user_state);
        assert_eq!(parsed_user_state.get_bid_amount(), 100);
        assert_eq!(parsed_user_state.get_acquired_count(), 2);
        assert!(parsed_user_state.is_active());

        let state = telegram_api::StarGiftAuctionState::starGiftAuctionState(
            telegram_api::starGiftAuctionState {
                version: 3,
                start_date: 10,
                end_date: 20,
                min_bid_amount: 100,
                bid_levels: vec![telegram_api::AuctionBidLevel {
                    pos: 1,
                    amount: 90,
                    date: 11,
                }],
                top_bidders: vec![42, 43, 44, 45, 0], // the last two are dropped
                next_round_at: 15,
                last_gift_num: 4,
                gifts_left: 6,
                current_round: 2,
                total_rounds: 5,
                rounds: vec![
                    telegram_api::StarGiftAuctionRound::starGiftAuctionRoundExtendable(
                        telegram_api::starGiftAuctionRoundExtendable {
                            num: 2,
                            duration: 60,
                            extend_top: 3,
                            extend_window: 30,
                        },
                    ),
                ],
            },
        );
        let parsed = StarGiftAuctionState::from_telegram(&state);
        assert_eq!(parsed.get_version(), 3);
        assert_eq!(parsed.top_bidder_user_ids.len(), 3);

        let object = parsed.get_auction_state_object(&mut env, &parsed_user_state);
        match object {
            td_api::AuctionState::auctionStateActive(active) => {
                assert_eq!(active.current_round_number, 2);
                assert_eq!(active.total_round_count, 5);
                assert_eq!(active.acquired_item_count, 2);
                assert_eq!(active.top_bidder_user_ids, vec![42, 43, 44]);
                assert!(active.user_bid.is_some());
            }
            _ => panic!("wrong auction state kind"),
        }

        // the finished state
        let finished = StarGiftAuctionState::from_telegram(
            &telegram_api::StarGiftAuctionState::starGiftAuctionStateFinished(
                telegram_api::starGiftAuctionStateFinished {
                    flags: 0,
                    start_date: 1,
                    end_date: 2,
                    average_price: 70,
                    listed_count: 3,
                    fragment_listed_count: 4,
                    fragment_listed_url: "https://fragment.com".to_owned(),
                },
            ),
        );
        match finished.get_auction_state_object(&mut env, &parsed_user_state) {
            td_api::AuctionState::auctionStateFinished(finished) => {
                assert_eq!(finished.average_price, 70);
                assert_eq!(finished.fragment_url, "https://fragment.com");
            }
            _ => panic!("wrong auction state kind"),
        }
    }

    fn sample_auction_answer(
        gift_id: i64,
        version: i32,
    ) -> telegram_api::payments_StarGiftAuctionState {
        telegram_api::payments_StarGiftAuctionState {
            gift: Some(telegram_api::StarGift::starGift(telegram_api::starGift {
                id: gift_id,
                stars: 100,
                auction: true,
                auction_slug: "auction".to_owned(),
                gifts_per_round: 10,
                auction_start_date: 5,
                ..Default::default()
            })),
            state: Some(telegram_api::StarGiftAuctionState::starGiftAuctionState(
                telegram_api::starGiftAuctionState {
                    version,
                    start_date: 10,
                    end_date: 20,
                    min_bid_amount: 100,
                    bid_levels: vec![],
                    top_bidders: vec![],
                    next_round_at: 15,
                    last_gift_num: 0,
                    gifts_left: 10,
                    current_round: 1,
                    total_rounds: 3,
                    rounds: vec![],
                },
            )),
            user_state: Some(telegram_api::StarGiftAuctionUserState {
                flags: 0,
                returned: false,
                bid_amount: 0,
                bid_date: 0,
                min_bid_amount: 100,
                bid_peer: Some(telegram_api::Peer::peerUser(telegram_api::peerUser {
                    user_id: 42,
                })),
                acquired_count: 0,
            }),
            timeout: 30,
            users: vec![],
            chats: vec![],
        }
    }

    #[test]
    fn get_gift_auction_state_parks_and_answer_completes() {
        let mut env = TestEnv::default();
        let mut manager = StarGiftManager::new();

        let (promise, recorded) = recording_promise();
        manager.get_gift_auction_state(&mut env, "auction", promise);
        assert_eq!(env.queries, vec!["getStarGiftAuctionState:slug:auction:0"]);
        assert!(recorded.value().is_none());

        manager.on_get_auction_state(&mut env, Result::Ok(sample_auction_answer(1, 1)));
        let answer = recorded.value().expect("answered").expect("ok");
        assert!(answer.gift.is_some());
        assert!(answer.state.is_some());
        // the update went out and the auction landed in the active list
        assert!(!env.updates.is_empty());
        assert_eq!(manager.active_gift_auctions.len(), 1);
    }

    #[test]
    fn open_close_gift_auction_flow() {
        let mut env = TestEnv::default();
        let mut manager = StarGiftManager::new();

        // opening an unknown auction reloads first, then the recursion
        // completes through the parked continuation
        let (promise, recorded) = recording_promise();
        manager.open_gift_auction(&mut env, 1, false, promise);
        assert_eq!(env.queries, vec!["getStarGiftAuctionState:gift:1:0"]);
        assert!(recorded.value().is_none());
        manager.on_get_auction_state(&mut env, Result::Ok(sample_auction_answer(1, 1)));
        assert_eq!(recorded.value().expect("answered").expect("ok"), ());

        // the second open is local and fires no reload (the C++ reloads
        // only when the open count transitions to 1)
        let queries_after_open = env.queries.len();
        let (promise, recorded) = recording_promise();
        manager.open_gift_auction(&mut env, 1, false, promise);
        assert_eq!(recorded.value().expect("answered").expect("ok"), ());
        assert_eq!(env.queries.len(), queries_after_open);

        // the close errors for an unknown auction and pops the counter
        let (promise, recorded) = recording_promise();
        manager.close_gift_auction(99, promise);
        assert_eq!(recorded.value().expect("answered").unwrap_err().code(), 400);
        let (promise, recorded) = recording_promise();
        manager.close_gift_auction(1, promise);
        assert_eq!(recorded.value().expect("answered").expect("ok"), ());
        assert!(manager.gift_auction_open_counts.contains_key(&1));
        // the second close drops the counter
        let (promise, recorded) = recording_promise();
        manager.close_gift_auction(1, promise);
        assert_eq!(recorded.value().expect("answered").expect("ok"), ());
        assert!(!manager.gift_auction_open_counts.contains_key(&1));
    }

    #[test]
    fn on_update_gift_auction_state_gates() {
        let mut env = TestEnv::default();
        let mut manager = StarGiftManager::new();
        manager.on_get_auction_state(&mut env, Result::Ok(sample_auction_answer(1, 1)));
        let updates_after_load = env.updates.len();

        // a same-version state is dropped
        let answer = sample_auction_answer(1, 1);
        let state = answer.state;
        manager.on_update_gift_auction_state(&mut env, 1, state);
        assert_eq!(env.updates.len(), updates_after_load);

        // a newer state updates and broadcasts
        let answer = sample_auction_answer(1, 2);
        let state = answer.state;
        manager.on_update_gift_auction_state(&mut env, 1, state);
        assert!(env.updates.len() > updates_after_load);

        // an unknown gift triggers the active-auctions reload
        let answer = sample_auction_answer(1, 5);
        let state = answer.state;
        manager.on_update_gift_auction_state(&mut env, 99, state);
        assert!(
            env.queries
                .iter()
                .any(|query| query == "getStarGiftActiveAuctions")
        );

        // the user-state update broadcasts on change only
        let updates_before = env.updates.len();
        let user_state = telegram_api::StarGiftAuctionUserState {
            flags: 0,
            returned: false,
            bid_amount: 120,
            bid_date: 50,
            min_bid_amount: 150,
            bid_peer: Some(telegram_api::Peer::peerUser(telegram_api::peerUser {
                user_id: 42,
            })),
            acquired_count: 0,
        };
        manager.on_update_gift_auction_user_state(&mut env, 1, Some(user_state));
        assert!(env.updates.len() > updates_before);
        let updates_before = env.updates.len();
        let user_state = telegram_api::StarGiftAuctionUserState {
            flags: 0,
            returned: false,
            bid_amount: 120,
            bid_date: 50,
            min_bid_amount: 150,
            bid_peer: Some(telegram_api::Peer::peerUser(telegram_api::peerUser {
                user_id: 42,
            })),
            acquired_count: 0,
        };
        manager.on_update_gift_auction_user_state(&mut env, 1, Some(user_state));
        assert_eq!(env.updates.len(), updates_before);
    }

    #[test]
    fn send_gift_error_paths() {
        let mut env = TestEnv::default();
        let my_dialog_id = env.my_dialog_id;
        let mut manager = StarGiftManager::new();

        // an unknown gift
        let (promise, recorded) = recording_promise();
        manager.send_gift(&mut env, 1, my_dialog_id, None, false, false, promise);
        assert_eq!(recorded.value().expect("answered").unwrap_err().code(), 400);

        // the price cache feeds the star count gate
        let regular = StarGift::from_telegram(
            &mut env,
            telegram_api::StarGift::starGift(telegram_api::starGift {
                id: 1,
                stars: 25,
                ..Default::default()
            }),
            false,
        );
        manager.on_get_star_gift(&mut env, &regular, true);
        env.owned_star_count = 10;
        let (promise, recorded) = recording_promise();
        manager.send_gift(&mut env, 1, my_dialog_id, None, false, false, promise);
        assert_eq!(
            recorded.value().expect("answered").unwrap_err().message(),
            "Have not enough Telegram Stars"
        );

        // an inaccessible receiver
        env.owned_star_count = i64::MAX;
        let (promise, recorded) = recording_promise();
        manager.send_gift(
            &mut env,
            1,
            DialogId::default(),
            None,
            false,
            false,
            promise,
        );
        assert_eq!(
            recorded.value().expect("answered").unwrap_err().message(),
            "Have no access to the gift receiver"
        );

        // the valid path parks on the payment chain
        let (promise, recorded) = recording_promise();
        manager.send_gift(&mut env, 1, my_dialog_id, None, false, true, promise);
        assert!(recorded.value().is_none());
        assert_eq!(manager.pending.unit_answers.len(), 1);
    }

    #[test]
    fn place_gift_auction_bid_error_paths() {
        let mut env = TestEnv::default();
        let mut manager = StarGiftManager::new();
        manager.on_get_auction_state(&mut env, Result::Ok(sample_auction_answer(1, 1)));

        // a bid not above the current one (the sample bid is 0)
        let (promise, recorded) = recording_promise();
        manager.place_gift_auction_bid(&mut env, 1, 0, UserId::new(42), None, false, promise);
        assert_eq!(
            recorded.value().expect("answered").unwrap_err().message(),
            "Too small bid specified"
        );

        // the valid path parks (with a bid placed on the user's state)
        let (promise, recorded) = recording_promise();
        manager.place_gift_auction_bid(&mut env, 1, 200, UserId::new(42), None, false, promise);
        assert!(recorded.value().is_none());
        assert_eq!(manager.pending.unit_answers.len(), 1);

        // updating without a bid errors
        let mut env = TestEnv::default();
        let mut manager = StarGiftManager::new();
        manager.on_get_auction_state(&mut env, Result::Ok(sample_auction_answer(1, 1)));
        let (promise, recorded) = recording_promise();
        manager.update_gift_auction_bid(&mut env, 1, 200, promise);
        assert_eq!(
            recorded.value().expect("answered").unwrap_err().message(),
            "There is no bid to update"
        );
    }

    #[test]
    fn convert_and_save_gift_gates() {
        let mut env = TestEnv::default();
        let mut manager = StarGiftManager::new();

        // convert: an invalid gift identifier
        let (promise, recorded) = recording_promise();
        manager.convert_gift(
            &mut env,
            BusinessConnectionId::default(),
            StarGiftId::default(),
            promise,
        );
        assert_eq!(
            recorded.value().expect("answered").unwrap_err().message(),
            "Invalid gift identifier specified"
        );

        // save: a valid slug parks
        let (promise, recorded) = recording_promise();
        manager.save_gift(&mut env, StarGiftId::from_slug("name"), true, promise);
        assert!(recorded.value().is_none());
        assert_eq!(manager.pending.unit_answers.len(), 1);
    }

    #[test]
    fn set_dialog_pinned_gifts_and_notifications_gates() {
        let mut env = TestEnv::default();
        let mut manager = StarGiftManager::new();

        // an invalid dialog
        let (promise, recorded) = recording_promise();
        manager.set_dialog_pinned_gifts(&mut env, DialogId::default(), Vec::new(), promise);
        assert_eq!(recorded.value().expect("answered").unwrap_err().code(), 400);

        // a gift from another dialog
        let (promise, recorded) = recording_promise();
        manager.set_dialog_pinned_gifts(
            &mut env,
            DialogId::from_user_id(UserId::new(7)),
            vec![StarGiftId::from_slug("name")],
            promise,
        );
        assert_eq!(
            recorded.value().expect("answered").unwrap_err().message(),
            "The gift is not from the chat"
        );

        // notifications need a broadcast channel with the post right
        let (promise, recorded) = recording_promise();
        manager.toggle_chat_star_gift_notifications(
            &mut env,
            DialogId::from_user_id(UserId::new(7)),
            true,
            promise,
        );
        assert_eq!(
            recorded.value().expect("answered").unwrap_err().message(),
            "Wrong chat specified"
        );

        // a channel parks
        let (promise, recorded) = recording_promise();
        manager.toggle_chat_star_gift_notifications(
            &mut env,
            DialogId::from_channel_id(ChannelId::new(1)),
            true,
            promise,
        );
        assert!(recorded.value().is_none());
    }

    #[test]
    fn upgrade_gift_gates() {
        let mut env = TestEnv::default();
        let mut manager = StarGiftManager::new();

        // an invalid identifier
        let (promise, recorded) = recording_promise();
        manager.upgrade_gift(
            &mut env,
            BusinessConnectionId::default(),
            StarGiftId::default(),
            false,
            0,
            promise,
        );
        assert_eq!(
            recorded.value().expect("answered").unwrap_err().message(),
            "Invalid gift identifier specified"
        );

        // a negative star count
        let (promise, recorded) = recording_promise();
        manager.upgrade_gift(
            &mut env,
            BusinessConnectionId::default(),
            StarGiftId::from_slug("name"),
            false,
            -1,
            promise,
        );
        assert_eq!(
            recorded.value().expect("answered").unwrap_err().message(),
            "Invalid amount of Telegram Stars specified"
        );

        // a star-priced upgrade parks on the payment chain
        let (promise, recorded) = recording_promise();
        manager.upgrade_gift(
            &mut env,
            BusinessConnectionId::default(),
            StarGiftId::from_slug("name"),
            true,
            15,
            promise,
        );
        assert!(recorded.value().is_none());
        assert_eq!(manager.pending.star_gift_result.len(), 1);
    }

    #[test]
    fn transfer_and_drop_gift_gates() {
        let mut env = TestEnv::default();
        let my_dialog_id = env.my_dialog_id;
        let mut manager = StarGiftManager::new();

        // transfer: an inaccessible receiver
        let (promise, recorded) = recording_promise();
        manager.transfer_gift(
            &mut env,
            BusinessConnectionId::default(),
            StarGiftId::from_slug("name"),
            DialogId::default(),
            0,
            promise,
        );
        assert_eq!(
            recorded.value().expect("answered").unwrap_err().message(),
            "Have no access to the new gift owner"
        );

        // transfer: the happy path parks
        let (promise, recorded) = recording_promise();
        manager.transfer_gift(
            &mut env,
            BusinessConnectionId::default(),
            StarGiftId::from_slug("name"),
            DialogId::from_user_id(UserId::new(7)),
            0,
            promise,
        );
        assert!(recorded.value().is_none());

        // drop: a non-positive star count
        let (promise, recorded) = recording_promise();
        manager.drop_gift_original_details(&mut env, StarGiftId::from_slug("name"), 0, promise);
        assert_eq!(
            recorded.value().expect("answered").unwrap_err().message(),
            "Invalid amount of Telegram Stars specified"
        );

        // the transferred-dialog continuation moves the gift counts
        let (promise, recorded) = recording_promise();
        manager.on_dialog_gift_transferred(
            &mut env,
            my_dialog_id,
            DialogId::from_user_id(UserId::new(7)),
            promise,
        );
        assert_eq!(recorded.value().expect("answered").expect("ok"), ());
        assert!(env.gift_counts.contains(&"my:-1".to_owned()));
    }

    #[test]
    fn send_resold_gift_and_offer_gates() {
        let mut env = TestEnv::default();
        let mut manager = StarGiftManager::new();

        // resale: an inaccessible receiver
        let price = StarGiftResalePrice::legacy(50);
        let (promise, recorded) = recording_promise();
        manager.send_resold_gift(
            &mut env,
            "name",
            DialogId::default(),
            price,
            None,
            false,
            promise,
        );
        assert_eq!(
            recorded.value().expect("answered").unwrap_err().message(),
            "Have no access to the new gift owner"
        );

        // resale: the happy path parks
        let (promise, recorded) = recording_promise();
        manager.send_resold_gift(
            &mut env,
            "name",
            DialogId::from_user_id(UserId::new(7)),
            price,
            None,
            false,
            promise,
        );
        assert!(recorded.value().is_none());
        assert_eq!(manager.pending.gift_resale_result.len(), 1);

        // offer: an inaccessible owner
        let (promise, recorded) = recording_promise();
        manager.send_gift_offer(&mut env, DialogId::default(), "name", price, 60, 0, promise);
        assert_eq!(
            recorded.value().expect("answered").unwrap_err().message(),
            "Can't access the chat"
        );

        // offer: a negative paid-message count
        let (promise, recorded) = recording_promise();
        manager.send_gift_offer(
            &mut env,
            DialogId::from_user_id(UserId::new(7)),
            "name",
            price,
            60,
            -1,
            promise,
        );
        assert_eq!(
            recorded.value().expect("answered").unwrap_err().message(),
            "Invalid amount of Telegram Stars specified"
        );

        // offer: the happy path parks
        let (promise, recorded) = recording_promise();
        manager.send_gift_offer(
            &mut env,
            DialogId::from_user_id(UserId::new(7)),
            "name",
            price,
            60,
            5,
            promise,
        );
        assert!(recorded.value().is_none());
        assert_eq!(manager.pending.unit_answers.len(), 1);
    }

    #[test]
    fn process_gift_offer_rejects_non_server_ids() {
        let mut env = TestEnv::default();
        let mut manager = StarGiftManager::new();
        let (promise, recorded) = recording_promise();
        manager.process_gift_offer(&mut env, 5, false, promise);
        assert_eq!(
            recorded.value().expect("answered").unwrap_err().message(),
            "Invalid offer message identifier specified"
        );

        let (promise, recorded) = recording_promise();
        manager.process_gift_offer(&mut env, 1 << 20, true, promise);
        assert!(recorded.value().is_none());
        assert_eq!(manager.pending.resolve_star_gift_offer.len(), 1);
    }

    #[test]
    fn get_saved_star_gifts_parks_and_answer_completes() {
        let mut env = TestEnv::default();
        let mut manager = StarGiftManager::new();
        let dialog_id = DialogId::from_user_id(UserId::new(42));

        let (promise, recorded) = recording_promise();
        manager.get_saved_star_gifts(
            &mut env,
            BusinessConnectionId::default(),
            dialog_id,
            StarGiftCollectionId::default(),
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            "",
            10,
            promise,
        );
        assert_eq!(env.queries.len(), 1);

        let answer = telegram_api::payments_SavedStarGifts {
            flags: (1 << 1) | 1,
            count: 1,
            chat_notifications_enabled: true,
            gifts: vec![telegram_api::SavedStarGift {
                flags: 0,
                name_hidden: false,
                unsaved: false,
                refunded: false,
                can_upgrade: false,
                pinned_to_top: false,
                upgrade_separate: false,
                from_id: Some(telegram_api::Peer::peerUser(telegram_api::peerUser {
                    user_id: 7,
                })),
                date: 500,
                gift: Some(sample_unique_gift()),
                message: None,
                msg_id: 9,
                saved_id: 0,
                convert_stars: 0,
                upgrade_stars: 0,
                can_export_at: 0,
                transfer_stars: 0,
                can_transfer_at: 0,
                can_resell_at: 0,
                collection_id: vec![],
                prepaid_upgrade_hash: String::new(),
                drop_original_details_stars: 0,
                gift_num: 0,
                can_craft_at: 0,
            }],
            next_offset: "next".to_owned(),
            chats: vec![],
            users: vec![],
        };
        manager.on_get_saved_star_gifts(&mut env, Result::Ok(answer));
        let answer = recorded.value().expect("answered").expect("ok");
        assert_eq!(answer.total_count, 1);
        assert!(answer.are_notifications_enabled); // the own dialog answers true
        assert_eq!(answer.next_offset, "next");
        // the own-dialog full list answers no user gift count
        assert!(env.gift_counts.is_empty());

        // the limit gate
        let (promise, recorded) = recording_promise();
        manager.get_saved_star_gifts(
            &mut env,
            BusinessConnectionId::default(),
            dialog_id,
            StarGiftCollectionId::default(),
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            "",
            -1,
            promise,
        );
        assert_eq!(
            recorded.value().expect("answered").unwrap_err().message(),
            "Limit must be non-negative"
        );
    }

    #[test]
    fn get_saved_star_gift_gates() {
        let mut env = TestEnv::default();
        let mut manager = StarGiftManager::new();

        // an invalid identifier
        let (promise, recorded) = recording_promise();
        manager.get_saved_star_gift(&mut env, StarGiftId::default(), promise);
        assert_eq!(
            recorded.value().expect("answered").unwrap_err().message(),
            "Invalid gift identifier specified"
        );

        // the answer without gifts errors
        let (promise, recorded) = recording_promise();
        manager.get_saved_star_gift(&mut env, StarGiftId::from_slug("name"), promise);
        manager.on_get_saved_star_gift(
            &mut env,
            Result::Ok(telegram_api::payments_SavedStarGifts {
                flags: 0,
                count: 0,
                chat_notifications_enabled: false,
                gifts: vec![],
                next_offset: String::new(),
                chats: vec![],
                users: vec![],
            }),
        );
        assert_eq!(
            recorded.value().expect("answered").unwrap_err().message(),
            "Gift not found"
        );
    }

    #[test]
    fn get_upgraded_gift_answer_flow() {
        let mut env = TestEnv::default();
        let mut manager = StarGiftManager::new();

        let (promise, recorded) = recording_promise();
        manager.get_upgraded_gift(&mut env, "SkyGift", promise);
        assert_eq!(env.queries, vec!["getUniqueStarGift:SkyGift"]);

        manager.on_get_upgraded_gift(
            &mut env,
            Result::Ok(telegram_api::payments_UniqueStarGift {
                gift: Some(sample_unique_gift()),
                chats: vec![],
                users: vec![],
            }),
        );
        let answer = recorded.value().expect("answered").expect("ok");
        assert_eq!(answer.name, "SkyGift");

        // an invalid answer errors with the C++ text
        let (promise, recorded) = recording_promise();
        manager.get_upgraded_gift(&mut env, "missing", promise);
        manager.on_get_upgraded_gift(
            &mut env,
            Result::Ok(telegram_api::payments_UniqueStarGift {
                gift: None,
                chats: vec![],
                users: vec![],
            }),
        );
        assert_eq!(
            recorded.value().expect("answered").unwrap_err().message(),
            "Gift not found"
        );
    }

    #[test]
    fn get_upgraded_gift_value_info_answer_flow() {
        let mut env = TestEnv::default();
        let mut manager = StarGiftManager::new();
        let (promise, recorded) = recording_promise();
        manager.get_upgraded_gift_value_info(&mut env, "SkyGift", promise);

        manager.on_get_upgraded_gift_value_info(Result::Ok(
            telegram_api::payments_UniqueStarGiftValueInfo {
                flags: 1,
                last_sale_on_fragment: true,
                value_is_average: true,
                currency: "TON".to_owned(),
                value: 7,
                initial_sale_date: -5,
                initial_sale_stars: 25,
                initial_sale_price: 3,
                last_sale_date: 100,
                last_sale_price: 5,
                floor_price: 1,
                average_price: 2,
                listed_count: 10,
                fragment_listed_count: 4,
                fragment_listed_url: "url".to_owned(),
            },
        ));
        let answer = recorded.value().expect("answered").expect("ok");
        assert_eq!(answer.initial_sale_date, 0); // max(0, -5)
        assert_eq!(answer.initial_sale_star_count, 25);
        assert!(answer.is_last_sale_on_fragment);
        assert_eq!(answer.fragment_url, "url");
    }

    #[test]
    fn withdrawal_url_and_set_price_gates() {
        let mut env = TestEnv::default();
        let mut manager = StarGiftManager::new();

        // an invalid identifier
        let (promise, recorded) = recording_promise();
        manager.get_star_gift_withdrawal_url(&mut env, StarGiftId::default(), "pass", promise);
        assert_eq!(
            recorded.value().expect("answered").unwrap_err().message(),
            "Invalid gift identifier specified"
        );

        // the empty password
        let (promise, recorded) = recording_promise();
        manager.get_star_gift_withdrawal_url(&mut env, StarGiftId::from_slug("name"), "", promise);
        assert_eq!(
            recorded.value().expect("answered").unwrap_err().message(),
            "PASSWORD_HASH_INVALID"
        );

        // the parked password plane keeps the promise
        let (promise, recorded) = recording_promise();
        manager.get_star_gift_withdrawal_url(
            &mut env,
            StarGiftId::from_slug("name"),
            "pass",
            promise,
        );
        assert!(recorded.value().is_none());
        assert_eq!(manager.pending.star_gift_withdrawal_url.len(), 1);

        // the price update parks
        let (promise, recorded) = recording_promise();
        manager.set_star_gift_price(
            &mut env,
            StarGiftId::from_slug("name"),
            StarGiftResalePrice::legacy(5),
            promise,
        );
        assert!(recorded.value().is_none());
        assert_eq!(manager.pending.unit_answers.len(), 1);
    }

    #[test]
    fn get_resale_star_gifts_answer_flow() {
        let mut env = TestEnv::default();
        let mut manager = StarGiftManager::new();

        // the null order gate
        let (promise, recorded) = recording_promise();
        manager.get_resale_star_gifts(&mut env, 1, None, false, false, &[], "", 10, promise);
        assert_eq!(
            recorded.value().expect("answered").unwrap_err().message(),
            "Gift sort order must be non-empty"
        );

        let (promise, recorded) = recording_promise();
        manager.get_resale_star_gifts(
            &mut env,
            1,
            Some(&td_api::GiftForResaleOrder::giftForResaleOrderPrice(
                td_api::giftForResaleOrderPrice,
            )),
            false,
            false,
            &[],
            "",
            10,
            promise,
        );

        let answer = telegram_api::payments_ResaleStarGifts {
            flags: 0,
            count: 1,
            gifts: vec![{
                // a unique gift resold by the env owner (42) → the slug id
                match sample_unique_gift() {
                    telegram_api::StarGift::starGiftUnique(mut star_gift) => {
                        star_gift.owner_id =
                            Some(telegram_api::Peer::peerUser(telegram_api::peerUser {
                                user_id: 42,
                            }));
                        telegram_api::StarGift::starGiftUnique(star_gift)
                    }
                    _ => unreachable!(),
                }
            }],
            next_offset: String::new(),
            attributes: vec![sample_backdrop_attribute()],
            attributes_hash: 0,
            chats: vec![],
            counters: vec![telegram_api::StarGiftAttributeCounter {
                attribute: Some(
                    telegram_api::StarGiftAttributeId::starGiftAttributeIdBackdrop(
                        telegram_api::starGiftAttributeIdBackdrop { backdrop_id: 7 },
                    ),
                ),
                count: 2,
            }],
            users: vec![],
        };
        manager.on_get_resale_star_gifts(&mut env, Result::Ok(answer));
        let answer = recorded.value().expect("answered").expect("ok");
        assert_eq!(answer.total_count, 1);
        assert_eq!(answer.backdrops.len(), 1);
        assert_eq!(answer.backdrops[0].total_count, 2);
        assert_eq!(answer.gifts[0].received_gift_id, "@SkyGift");
    }

    #[test]
    fn collections_flows_and_gates() {
        let mut env = TestEnv::default();
        let mut manager = StarGiftManager::new();
        let dialog_id = DialogId::from_user_id(UserId::new(42));

        // create: a gift from another dialog is rejected at the id check
        let (promise, recorded) = recording_promise();
        manager.create_gift_collection(
            &mut env,
            dialog_id,
            "name",
            vec![StarGiftId::from_slug("other")],
            promise,
        );
        assert_eq!(
            recorded.value().expect("answered").unwrap_err().message(),
            "The gift is not from the chat"
        );

        // create: an empty list passes the id check (the C++ has no
        // collection-id gate in create) and parks
        let (promise, recorded) = recording_promise();
        manager.create_gift_collection(&mut env, dialog_id, "name", Vec::new(), promise);
        assert!(recorded.value().is_none());

        // create: the valid path parks and the answer completes
        let (promise, recorded) = recording_promise();
        manager.create_gift_collection(
            &mut env,
            dialog_id,
            "name",
            vec![StarGiftId::from_string("7")],
            promise,
        );
        assert!(recorded.value().is_none());
        manager.on_create_gift_collection(
            &mut env,
            Result::Ok(telegram_api::StarGiftCollection {
                flags: 0,
                collection_id: 5,
                title: "name".to_owned(),
                icon: None,
                gifts_count: 1,
                hash: 9,
            }),
        );
        let answer = recorded.value().expect("answered").expect("ok");
        assert_eq!(answer.id, 5);
        assert_eq!(answer.name, "name");

        // title updates reject the empty name
        let (promise, recorded) = recording_promise();
        manager.set_gift_collection_title(
            &mut env,
            dialog_id,
            StarGiftCollectionId::new(5),
            "",
            promise,
        );
        assert_eq!(
            recorded.value().expect("answered").unwrap_err().message(),
            "Gift collection name must be non-empty"
        );

        // add/remove/reorder reject the empty and invalid inputs
        let (promise, recorded) = recording_promise();
        manager.add_gift_collection_gifts(
            &mut env,
            dialog_id,
            StarGiftCollectionId::new(0),
            vec![StarGiftId::from_string("7")],
            promise,
        );
        assert_eq!(
            recorded.value().expect("answered").unwrap_err().message(),
            "Invalid collection identifier specified"
        );
        let (promise, recorded) = recording_promise();
        manager.remove_gift_collection_gifts(
            &mut env,
            dialog_id,
            StarGiftCollectionId::new(5),
            Vec::new(),
            promise,
        );
        assert_eq!(
            recorded.value().expect("answered").unwrap_err().message(),
            "Gift list must be non-empty"
        );

        // reorder collections: an invalid id
        let (promise, recorded) = recording_promise();
        manager.reorder_gift_collections(
            &mut env,
            dialog_id,
            StarGiftCollectionId::get_star_gift_collection_ids(&[0]),
            promise,
        );
        assert_eq!(
            recorded.value().expect("answered").unwrap_err().message(),
            "Invalid collection identifier specified"
        );

        // the delete parks
        let (promise, recorded) = recording_promise();
        manager.delete_gift_collection(&mut env, dialog_id, StarGiftCollectionId::new(5), promise);
        assert!(recorded.value().is_none());

        // the collections answer maps the server objects
        let (promise, recorded) = recording_promise();
        manager.get_gift_collections(&mut env, dialog_id, promise);
        manager.on_get_gift_collections(
            &mut env,
            Result::Ok(
                telegram_api::payments_StarGiftCollections::payments_starGiftCollections(
                    telegram_api::payments_starGiftCollections {
                        collections: vec![telegram_api::StarGiftCollection {
                            flags: 0,
                            collection_id: 5,
                            title: "name".to_owned(),
                            icon: None,
                            gifts_count: 1,
                            hash: 9,
                        }],
                    },
                ),
            ),
        );
        let answer = recorded.value().expect("answered").expect("ok");
        assert_eq!(answer.collections.len(), 1);
        assert_eq!(answer.collections[0].id, 5);
    }

    #[test]
    fn craft_and_promo_parking() {
        let mut env = TestEnv::default();
        let mut manager = StarGiftManager::new();

        // craft: a gift from another dialog is rejected
        let (promise, recorded) = recording_promise();
        manager.craft_gift(&mut env, vec![StarGiftId::from_slug("name")], promise);
        assert_eq!(
            recorded.value().expect("answered").unwrap_err().message(),
            "The gift is not from the chat"
        );

        // craft: my-dialog ids park
        let my_server_message_id = StarGiftId::from_string("7");
        let (promise, recorded) = recording_promise();
        manager.craft_gift(&mut env, vec![my_server_message_id], promise);
        assert!(recorded.value().is_none());
        assert_eq!(manager.pending.craft_result.len(), 1);

        // the crafting answer maps the probabilities from the option
        let (promise, recorded) = recording_promise();
        manager.get_craft_star_gifts(&mut env, 1, "", 10, promise);
        manager.on_get_craft_star_gifts(
            &mut env,
            Result::Ok(telegram_api::payments_SavedStarGifts {
                flags: 0,
                count: 0,
                chat_notifications_enabled: false,
                gifts: vec![],
                next_offset: String::new(),
                chats: vec![],
                users: vec![],
            }),
        );
        let answer = recorded.value().expect("answered").expect("ok");
        assert_eq!(answer.attribute_persistence_probabilities.len(), 4);
        assert_eq!(
            answer.attribute_persistence_probabilities[0].persistence_chance_per_mille,
            vec![100, 0, 0, 0]
        );
        // the triangle indexing for num = 4 reads the 7th..10th entries
        assert_eq!(
            answer.attribute_persistence_probabilities[3].persistence_chance_per_mille,
            vec![60, 300, 600, 1000]
        );

        // the promo animation parks
        let (promise, recorded) = recording_promise();
        manager.get_star_gift_promo_animation(&mut env, promise);
        assert!(recorded.value().is_none());
        assert_eq!(manager.pending.star_gift_promo_animation.len(), 1);
    }

    #[test]
    fn gift_message_registry_flow() {
        let mut env = TestEnv::default();
        let mut manager = StarGiftManager::new();
        let message_full_id = crate::message_id::MessageFullId::new(
            crate::message_id::DialogId::new(-(crate::user_manager::UserId::new(42).get() * 2)),
            crate::message_id::MessageId::new(1 << 20),
        );

        manager.register_gift(message_full_id, "test");
        assert_eq!(manager.gift_message_count, 1);
        assert_eq!(
            manager.gift_message_full_ids.get(&message_full_id),
            Some(&1)
        );

        // the poll marks the message and answers through the seam
        manager.poll_gift(&mut env, 1);
        assert_eq!(manager.being_reloaded_gift_messages.len(), 1);
        // a second poll is a no-op while being reloaded
        manager.poll_gift(&mut env, 1);
        assert_eq!(manager.being_reloaded_gift_messages.len(), 1);
        // the answer reschedules
        manager.on_gift_message_updated(message_full_id);
        assert!(manager.being_reloaded_gift_messages.is_empty());

        // the unregistered message id answers nothing
        manager.unregister_gift(message_full_id, "test");
        assert!(manager.gift_message_full_ids.is_empty());
        manager.on_gift_message_updated(message_full_id);
    }

    #[test]
    fn get_message_sender_dialog_id_rules() {
        let mut env = TestEnv::default();
        env.users.push(42);

        // an unknown user with the access check
        let error = get_message_sender_dialog_id(
            &mut env,
            Some(&td_api::MessageSender::messageSenderUser(
                td_api::messageSenderUser { user_id: 99 },
            )),
        )
        .unwrap_err();
        assert_eq!(error.message(), "Unknown user identifier specified");

        // a known user resolves
        assert!(
            get_message_sender_dialog_id(
                &mut env,
                Some(&td_api::MessageSender::messageSenderUser(
                    td_api::messageSenderUser { user_id: 42 },
                )),
            )
            .is_ok()
        );

        // a null sender errors
        assert!(get_message_sender_dialog_id(&mut env, None).is_err());
    }
}
