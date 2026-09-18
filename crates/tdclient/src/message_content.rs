//! Port of `td/telegram/MessageContent.{h,cpp}` — the per-message content
//! model (M.9).
//!
//! M.9.1 is the foundation slice: the `MessageContentType` discriminant
//! enum, the companion value types the model needs, the [`MessageContent`]
//! enum-of-structs, the per-class `get_type` mapping, and the `create_*`
//! factories (MessageContent.cpp:4653-4704).
//!
//! Landed in steps: M.9.1a (the discriminant enum, the trivial id
//! wrappers, 61 variants), M.9.1c/M.9.1d (the value companions and 16
//! more variants: geo/topic/background/giveaway/invoice/game/paid-media/
//! passport), and M.9.1b (the `DialogId` plane — 12 more variants plus the
//! `ToDoCompletion`/`SharedDialog`/`CommunityId`/`Address`/`OrderInfo`
//! companions they need). 89 of the 96 variants are modeled.
//!
//! The remaining 7 variants are pinned by companion types that do not
//! exist on this line yet and land with them:
//! - the `StarGift` family (`StarGift`, `StarGiftUnique`,
//!   `StarGiftPurchaseOffer`, `StarGiftPurchaseOfferDeclined`) — M.9.1b
//!   re-checked the C++ and found these need the `StarGift` VALUE class
//!   (StarGift.h, with its `StarGiftAttribute`/`StarGiftBackground`/
//!   `PeerColorCollectible` plane and `FileId` stickers), not just the
//!   `DialogId`s M.9.1 recorded; the value plane is the Wave-3
//!   Gifts/Stickers slice (`StarGiftResalePrice`, the only other missing
//!   piece, is trivial and lands with its consumers).
//! - `Contact` — needs `td::Contact`: M.4.14's `contact.rs` landed on the
//!   master line *after* this branch's base (`85af8aa`), so the value
//!   arrives with the master-line integration, not with this slice.
//! - `ChatSetTheme` — needs `StarGift` + `ThemeSettings` (ChatTheme.h).
//! - `RichText` — needs the `WebPageBlock`/`RichMessageMedia` machinery
//!   (RichMessage.h); Wave-2 WebPages scope. `create_rich_message_content`
//!   is deferred with it.
//!
//! The db `store`/`parse` paths are M.9.2/M.9.3; the td_api/telegram_api
//! conversions arrive with their own slices. Value companions living here
//! that later move to their owners' modules: `Location`/`Venue` (the
//! DialogManager/m54-union geo plane), `BackgroundId`/`BackgroundFill`/
//! `BackgroundType`/`BackgroundInfo` (Wave-3 Backgrounds), `Game`
//! (Wave-2 WebPages/inline), `Invoice`/`LabeledPricePart` +
//! `CurrencyAmount`/`StarAmount`/`TonAmount`/`SuggestedPostPrice`
//! (Wave-3 Payments), the `SecureValue` encrypted subset (Wave-3
//! Passport), and `ToDoList`/`ToDoItem` (Wave-2 messaging-adjacent).

use std::hash::{Hash, Hasher};

use tdutils::emoji::remove_emoji_modifiers;

use crate::birthdate::Birthdate;
use crate::chat_manager::{ChannelId, ChatId, MessageId};
use crate::emoji_status::CustomEmojiId;
use crate::message_entity::{FormattedText, MessageEntity, find_entities, remove_empty_entities};
use crate::messages_manager::{DialogId, DialogType, StoryFullId, StoryId};
use crate::photo::{
    AnimationSize, Dimensions, Photo, PhotoSize, StickerPhotoSize, StickerPhotoSizeType,
};
use crate::store_parse::{
    Parser, store_bool, store_f64, store_flags, store_i32, store_i32_vec, store_i64, store_string,
    store_string_vec, store_u32, version,
};
use crate::user_manager::{FileId, UserId};

/// `td::MessageContentType` (MessageContentType.h:15-113) — the discriminant
/// order is the C++ enum order; `MessageUnsupported::CURRENT_VERSION` bumps
/// with every addition. The single copy on the union line (the m10-line
/// duplicate in messages_manager.rs was deleted in favor of this module at
/// the M.9.1 integration, as that commit note reserved).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum MessageContentType {
    None = -1,
    #[default]
    Text = 0,
    Animation,
    Audio,
    Document,
    Photo,
    Sticker,
    Video,
    VoiceNote,
    Contact,
    Location,
    Venue,
    ChatCreate,
    ChatChangeTitle,
    ChatChangePhoto,
    ChatDeletePhoto,
    ChatDeleteHistory,
    ChatAddUsers,
    ChatJoinedByLink,
    ChatDeleteUser,
    ChatMigrateTo,
    ChannelCreate,
    ChannelMigrateFrom,
    PinMessage,
    Game,
    GameScore,
    ScreenshotTaken,
    ChatSetTtl,
    Unsupported,
    Call,
    Invoice,
    PaymentSuccessful,
    VideoNote,
    ContactRegistered,
    ExpiredPhoto,
    ExpiredVideo,
    LiveLocation,
    CustomServiceAction,
    WebsiteConnected,
    PassportDataSent,
    PassportDataReceived,
    Poll,
    Dice,
    ProximityAlertTriggered,
    GroupCall,
    InviteToGroupCall,
    ChatSetTheme,
    WebViewDataSent,
    WebViewDataReceived,
    GiftPremium,
    TopicCreate,
    TopicEdit,
    SuggestProfilePhoto,
    WriteAccessAllowed,
    RequestedDialog,
    WebViewWriteAccessAllowed,
    SetBackground,
    Story,
    WriteAccessAllowedByRequest,
    GiftCode,
    Giveaway,
    GiveawayLaunch,
    GiveawayResults,
    GiveawayWinners,
    ExpiredVideoNote,
    ExpiredVoiceNote,
    BoostApply,
    DialogShared,
    PaidMedia,
    PaymentRefunded,
    GiftStars,
    PrizeStars,
    StarGift,
    StarGiftUnique,
    PaidMessagesRefunded,
    PaidMessagesPrice,
    ConferenceCall,
    ToDoList,
    TodoCompletions,
    TodoAppendTasks,
    GiftTon,
    SuggestedPostSuccess,
    SuggestedPostRefund,
    SuggestedPostApproval,
    SuggestBirthday,
    StarGiftPurchaseOffer,
    StarGiftPurchaseOfferDeclined,
    NewCreatorPending,
    ChangeCreator,
    NoForwardsToggle,
    NoForwardsRequest,
    ManagedBotCreated,
    PollAppendAnswer,
    PollDeleteAnswer,
    RichText,
    ChangeCommunity,
    ChatJoinedViaCommunity,
}

/// `can_be_secret_message_content` (MessageContentType.cpp) — the C++
/// `default: UNREACHABLE()` covers only `None`.
pub fn can_be_secret_message_content(content_type: MessageContentType) -> bool {
    use MessageContentType::*;
    matches!(
        content_type,
        Animation | Audio | Photo | Video | VideoNote | VoiceNote
    )
}

/// `td::WebPageId` (WebPageId.h:17-50) — the instant-view page id; `0` is
/// the "no web page" sentinel. The store/parse is the raw `int64`
/// (WebPageId.h:41-48) and lands with M.9.2.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct WebPageId(i64);

impl WebPageId {
    pub fn new(id: i64) -> WebPageId {
        WebPageId(id)
    }

    pub fn get(self) -> i64 {
        self.0
    }

    /// `WebPageId::is_valid` (WebPageId.h:34-36).
    pub fn is_valid(self) -> bool {
        self.0 != 0
    }
}

/// `td::PollId` (PollId.h:16-46).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct PollId(i64);

impl PollId {
    pub fn new(id: i64) -> PollId {
        PollId(id)
    }

    pub fn get(self) -> i64 {
        self.0
    }

    /// `PollId::is_valid` (PollId.h:31-33).
    pub fn is_valid(self) -> bool {
        self.0 != 0
    }
}

/// `td::InputGroupCallId` (InputGroupCallId.h:15-66) — the group call id
/// plus its access hash. The C++ equality compares only the call id
/// (InputGroupCallId.h:31-33); two ids with different access hashes are
/// equal but not `is_identical`. The hash is the C++ `InputGroupCallIdHash`
/// (the call id only).
#[derive(Clone, Copy, Debug, Default)]
pub struct InputGroupCallId {
    group_call_id: i64,
    access_hash: i64,
}

impl Eq for InputGroupCallId {}

impl Hash for InputGroupCallId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.group_call_id.hash(state);
    }
}

impl InputGroupCallId {
    /// `InputGroupCallId(int64, int64)` (InputGroupCallId.h:24-26).
    pub fn new(group_call_id: i64, access_hash: i64) -> InputGroupCallId {
        InputGroupCallId {
            group_call_id,
            access_hash,
        }
    }

    pub fn get_group_call_id(self) -> i64 {
        self.group_call_id
    }

    pub fn get_access_hash(self) -> i64 {
        self.access_hash
    }

    /// `InputGroupCallId::is_valid` (InputGroupCallId.h:39-41).
    pub fn is_valid(self) -> bool {
        self.group_call_id != 0
    }

    /// `InputGroupCallId::is_identical` (InputGroupCallId.h:35-37).
    pub fn is_identical(self, other: InputGroupCallId) -> bool {
        self.group_call_id == other.group_call_id && self.access_hash == other.access_hash
    }
}

impl PartialEq for InputGroupCallId {
    fn eq(&self, other: &Self) -> bool {
        self.group_call_id == other.group_call_id
    }
}

/// `td::CallDiscardReasonType` — the nested
/// `CallDiscardReason::Type` (CallDiscardReason.h:17).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum CallDiscardReasonType {
    #[default]
    Empty,
    Missed,
    Disconnected,
    HungUp,
    Declined,
    UpgradeToGroupCall,
}

/// `td::CallDiscardReason` (CallDiscardReason.h:16-19). The conversions
/// (`get_call_discard_reason` family, CallDiscardReason.cpp) land with the
/// calls-facing slices.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CallDiscardReason {
    pub r#type: CallDiscardReasonType,
    pub slug: String,
}

/// `td::Location` (Location.h:23-31). The wire/td_api conversions and the
/// input-media bridges land with their slices; the access-hash
/// registration side effect of `Location::init` (Location.cpp:27-38, the
/// non-bot `add_location_access_hash` call) belongs to the wire-intake
/// path, not to this value constructor.
#[derive(Clone, Debug)]
pub struct Location {
    is_empty: bool,
    latitude: f64,
    longitude: f64,
    horizontal_accuracy: f64,
    access_hash: i64,
}

impl Default for Location {
    fn default() -> Self {
        Location {
            is_empty: true,
            latitude: 0.0,
            longitude: 0.0,
            horizontal_accuracy: 0.0,
            access_hash: 0,
        }
    }
}

impl Location {
    /// `Location::fix_accuracy` (Location.cpp:17-25).
    fn fix_accuracy(accuracy: f64) -> f64 {
        if !accuracy.is_finite() || accuracy <= 0.0 {
            return 0.0;
        }
        if accuracy >= 1500.0 {
            return 1500.0;
        }
        accuracy
    }

    /// The value half of `Location::init` (Location.cpp:27-38): an
    /// out-of-range or non-finite coordinate leaves the location empty.
    pub fn new(
        latitude: f64,
        longitude: f64,
        horizontal_accuracy: f64,
        access_hash: i64,
    ) -> Location {
        let mut location = Location::default();
        if latitude.is_finite()
            && longitude.is_finite()
            && latitude.abs() <= 90.0
            && longitude.abs() <= 180.0
        {
            location.is_empty = false;
            location.latitude = latitude;
            location.longitude = longitude;
            location.horizontal_accuracy = Self::fix_accuracy(horizontal_accuracy);
            location.access_hash = access_hash;
        }
        location
    }

    /// `Location::empty` (Location.cpp:74-76).
    pub fn is_empty(&self) -> bool {
        self.is_empty
    }

    /// `Location::is_valid_map_point` (Location.cpp:77-80).
    pub fn is_valid_map_point(&self) -> bool {
        const MAX_VALID_MAP_LATITUDE: f64 = 85.05112877;
        !self.is_empty && self.latitude.abs() <= MAX_VALID_MAP_LATITUDE
    }

    pub fn get_latitude(&self) -> f64 {
        self.latitude
    }

    pub fn get_longitude(&self) -> f64 {
        self.longitude
    }

    pub fn get_access_hash(&self) -> i64 {
        self.access_hash
    }

    /// `Location::set_access_hash` (Location.h:66-68).
    pub fn set_access_hash(&mut self, access_hash: i64) {
        self.access_hash = access_hash;
    }
}

/// The C++ `operator==` (Location.cpp:229-238): an empty location equals
/// only another empty one; the coordinates compare with the 1e-6 epsilon
/// and the access hash is ignored. The comparison is not transitive, so
/// `Eq` is deliberately not implemented.
impl PartialEq for Location {
    fn eq(&self, other: &Self) -> bool {
        if self.is_empty {
            return other.is_empty;
        }
        !other.is_empty
            && (self.latitude - other.latitude).abs() < 1e-6
            && (self.longitude - other.longitude).abs() < 1e-6
            && (self.horizontal_accuracy - other.horizontal_accuracy).abs() < 1e-6
    }
}

/// `td::Venue` (Venue.h:24-32). The conversions land with their slices.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Venue {
    pub location: Location,
    pub title: String,
    pub address: String,
    pub provider: String,
    pub id: String,
    pub r#type: String,
}

impl Venue {
    /// `Venue::empty` (Venue.cpp:42-44) — only the location decides.
    pub fn is_empty(&self) -> bool {
        self.location.is_empty()
    }

    /// `Venue::is_same` (Venue.h:40-42).
    pub fn is_same(&self, provider: &str, id: &str) -> bool {
        self.provider == provider && self.id == id
    }
}

/// `td::StarAmount` (StarAmount.h:13-17). The wire constructor
/// (StarAmount.cpp) lands with the payments-facing slices.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct StarAmount {
    pub star_count: i64,
    pub nanostar_count: i32,
}

impl StarAmount {
    /// `StarAmount::is_positive` (StarAmount.h:28-31).
    pub fn is_positive(&self) -> bool {
        self.star_count > 0 || self.nanostar_count > 0
    }
}

/// `td::TonAmount` (TonAmount.h:13-15).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct TonAmount {
    pub ton_amount: i64,
}

impl TonAmount {
    /// `TonAmount::is_positive` (TonAmount.h:25-27).
    pub fn is_positive(&self) -> bool {
        self.ton_amount > 0
    }
}

/// The nested `CurrencyAmount::Type` (CurrencyAmount.h:15).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum CurrencyAmountType {
    #[default]
    None,
    Star,
    Ton,
}

/// `td::CurrencyAmount` (CurrencyAmount.h:14-19). The wire constructor
/// (CurrencyAmount.cpp) lands with the payments-facing slices.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct CurrencyAmount {
    pub r#type: CurrencyAmountType,
    pub star_amount: StarAmount,
    pub ton_amount: TonAmount,
}

/// The nested `SuggestedPostPrice::Type` (SuggestedPostPrice.h:16).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SuggestedPostPriceType {
    #[default]
    None,
    Star,
    Ton,
}

/// `td::SuggestedPostPrice` (SuggestedPostPrice.h:15-21).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct SuggestedPostPrice {
    pub r#type: SuggestedPostPriceType,
    pub amount: i64,
}

impl SuggestedPostPrice {
    /// `SuggestedPostPrice::TON_MULTIPLIER` (SuggestedPostPrice.h:20).
    pub const TON_MULTIPLIER: i64 = 10000000;

    /// `SuggestedPostPrice::is_empty` (SuggestedPostPrice.h:34-36).
    pub fn is_empty(&self) -> bool {
        self.r#type == SuggestedPostPriceType::None
    }
}

/// `td::ForumTopicIcon` (ForumTopicIcon.h:16-20).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ForumTopicIcon {
    color: i32,
    custom_emoji_id: CustomEmojiId,
}

impl Default for ForumTopicIcon {
    fn default() -> Self {
        ForumTopicIcon {
            color: 0x6FB9F0,
            custom_emoji_id: CustomEmojiId::default(),
        }
    }
}

impl ForumTopicIcon {
    /// The `ForumTopicIcon(int32, int64)` constructor
    /// (ForumTopicIcon.cpp:12-15): the color is masked to 24 bits.
    pub fn new(color: i32, custom_emoji_id: CustomEmojiId) -> ForumTopicIcon {
        ForumTopicIcon {
            color: color & 0xFFFFFF,
            custom_emoji_id,
        }
    }

    pub fn get_color(&self) -> i32 {
        self.color
    }

    pub fn get_custom_emoji_id(&self) -> CustomEmojiId {
        self.custom_emoji_id
    }

    /// `ForumTopicIcon::edit_custom_emoji_id` (ForumTopicIcon.cpp:16-22).
    pub fn edit_custom_emoji_id(&mut self, custom_emoji_id: CustomEmojiId) -> bool {
        if self.custom_emoji_id != custom_emoji_id {
            self.custom_emoji_id = custom_emoji_id;
            return true;
        }
        false
    }
}

/// `td::ForumTopicEditedData` (ForumTopicEditedData.h:13-21).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ForumTopicEditedData {
    pub title: String,
    pub icon_custom_emoji_id: CustomEmojiId,
    pub edit_icon_custom_emoji_id: bool,
    pub edit_is_closed: bool,
    pub is_closed: bool,
    pub edit_is_hidden: bool,
    pub is_hidden: bool,
}

impl ForumTopicEditedData {
    /// `ForumTopicEditedData::is_empty` (ForumTopicEditedData.cpp).
    pub fn is_empty(&self) -> bool {
        self.title.is_empty()
            && !self.edit_icon_custom_emoji_id
            && !self.edit_is_closed
            && !self.edit_is_hidden
    }
}

/// `td::WebApp` (WebApp.h:21-31). The wire constructor and the
/// object/file-id helpers land with their slices.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WebApp {
    pub id: i64,
    pub access_hash: i64,
    pub short_name: String,
    pub title: String,
    pub description: String,
    pub photo: Photo,
    pub animation_file_id: FileId,
    pub hash: i64,
}

impl WebApp {
    /// `WebApp::is_empty` (WebApp.cpp).
    pub fn is_empty(&self) -> bool {
        self.short_name.is_empty()
    }
}

/// `td::BackgroundId` (BackgroundId.h:14-17).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct BackgroundId(i64);

impl BackgroundId {
    pub fn new(id: i64) -> BackgroundId {
        BackgroundId(id)
    }

    pub fn get(self) -> i64 {
        self.0
    }

    /// `BackgroundId::is_valid` (BackgroundId.h:33-35).
    pub fn is_valid(self) -> bool {
        self.0 != 0
    }

    /// `BackgroundId::is_local` (BackgroundId.h:36-38).
    pub fn is_local(self) -> bool {
        0 < self.0 && self.0 <= 0x7FFFFFFF
    }
}

/// `td::BackgroundFill` (BackgroundType.h:15-20) — a flat color model
/// where the non-`-1` `third_color` promotes it to a freeform gradient
/// and `top_color == bottom_color` collapses it to a solid.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BackgroundFill {
    pub top_color: i32,
    pub bottom_color: i32,
    pub rotation_angle: i32,
    pub third_color: i32,
    pub fourth_color: i32,
}

impl Default for BackgroundFill {
    fn default() -> Self {
        BackgroundFill {
            top_color: 0,
            bottom_color: 0,
            rotation_angle: 0,
            third_color: -1,
            fourth_color: -1,
        }
    }
}

/// The nested `BackgroundFill::Type` (BackgroundType.h:60-64).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BackgroundFillType {
    Solid,
    Gradient,
    FreeformGradient,
}

impl BackgroundFill {
    /// `BackgroundFill(int32)` (BackgroundType.h:19-21).
    pub fn solid(color: i32) -> BackgroundFill {
        BackgroundFill {
            top_color: color,
            bottom_color: color,
            rotation_angle: 0,
            third_color: -1,
            fourth_color: -1,
        }
    }

    /// `BackgroundFill(int32, int32, int32)` (BackgroundType.h:22-27): the
    /// rotation angle survives only for a real gradient.
    pub fn gradient(top_color: i32, bottom_color: i32, rotation_angle: i32) -> BackgroundFill {
        let mut fill = BackgroundFill {
            top_color,
            bottom_color,
            rotation_angle,
            third_color: -1,
            fourth_color: -1,
        };
        if fill.get_type() != BackgroundFillType::Gradient {
            fill.rotation_angle = 0;
        }
        fill
    }

    /// `BackgroundFill(int32, int32, int32, int32)` (BackgroundType.h:28-33).
    pub fn freeform(
        first_color: i32,
        second_color: i32,
        third_color: i32,
        fourth_color: i32,
    ) -> BackgroundFill {
        BackgroundFill {
            top_color: first_color,
            bottom_color: second_color,
            rotation_angle: 0,
            third_color,
            fourth_color,
        }
    }

    /// `BackgroundFill::get_type` (BackgroundType.h:66-73).
    pub fn get_type(&self) -> BackgroundFillType {
        if self.third_color != -1 {
            return BackgroundFillType::FreeformGradient;
        }
        if self.top_color == self.bottom_color {
            return BackgroundFillType::Solid;
        }
        BackgroundFillType::Gradient
    }
}

/// The nested `BackgroundType::Type` (BackgroundType.h:112). The C++
/// default member init is `Fill`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum BackgroundTypeType {
    Wallpaper,
    Pattern,
    #[default]
    Fill,
    ChatTheme,
}

/// `td::BackgroundType` (BackgroundType.h:111-119) — the C++ private
/// constructors become the `wallpaper`/`pattern`/`fill`/`chat_theme`
/// associated functions; the wire constructor lands with the
/// backgrounds-facing slices.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BackgroundType {
    pub r#type: BackgroundTypeType,
    pub is_blurred: bool,
    pub is_moving: bool,
    pub intensity: i32,
    pub fill: BackgroundFill,
    pub theme_name: String,
}

impl BackgroundType {
    /// `BackgroundType(bool, bool, int32)` (BackgroundType.h:120-124).
    pub fn wallpaper(is_blurred: bool, is_moving: bool, dark_theme_dimming: i32) -> BackgroundType {
        BackgroundType {
            r#type: BackgroundTypeType::Wallpaper,
            is_blurred,
            is_moving,
            intensity: dark_theme_dimming,
            fill: BackgroundFill::default(),
            theme_name: String::new(),
        }
    }

    /// `BackgroundType(bool, BackgroundFill, int32)` (BackgroundType.h:125-129).
    pub fn pattern(is_moving: bool, fill: BackgroundFill, intensity: i32) -> BackgroundType {
        BackgroundType {
            r#type: BackgroundTypeType::Pattern,
            is_blurred: false,
            is_moving,
            intensity,
            fill,
            theme_name: String::new(),
        }
    }

    /// `BackgroundType(BackgroundFill, int32)` (BackgroundType.h:130-134).
    pub fn fill(fill: BackgroundFill, dark_theme_dimming: i32) -> BackgroundType {
        BackgroundType {
            r#type: BackgroundTypeType::Fill,
            is_blurred: false,
            is_moving: false,
            intensity: dark_theme_dimming,
            fill,
            theme_name: String::new(),
        }
    }

    /// `BackgroundType(string)` (BackgroundType.h:135-137).
    pub fn chat_theme(theme_name: String) -> BackgroundType {
        BackgroundType {
            r#type: BackgroundTypeType::ChatTheme,
            is_blurred: false,
            is_moving: false,
            intensity: 0,
            fill: BackgroundFill::default(),
            theme_name,
        }
    }

    /// `BackgroundType::has_file` (BackgroundType.h).
    pub fn has_file(&self) -> bool {
        self.r#type == BackgroundTypeType::Wallpaper || self.r#type == BackgroundTypeType::Pattern
    }

    /// `BackgroundType::has_gradient_fill` (BackgroundType.h).
    pub fn has_gradient_fill(&self) -> bool {
        self.r#type == BackgroundTypeType::Fill && self.fill.get_type() != BackgroundFillType::Solid
    }
}

/// `td::BackgroundInfo` (BackgroundInfo.h:17-20).
#[derive(Clone, Debug, Default)]
pub struct BackgroundInfo {
    pub background_id: BackgroundId,
    pub background_type: BackgroundType,
}

impl BackgroundInfo {
    /// `BackgroundInfo::is_valid` (BackgroundInfo.h:31-33).
    pub fn is_valid(&self) -> bool {
        self.background_id.is_valid()
    }
}

/// The C++ `operator==` (BackgroundInfo.h:35-39): the types must be equal
/// and the ids equal — or both local (local ids name the same built-in
/// background regardless of the exact id).
impl PartialEq for BackgroundInfo {
    fn eq(&self, other: &Self) -> bool {
        self.background_type == other.background_type
            && (self.background_id == other.background_id
                || (self.background_id.is_local() && other.background_id.is_local()))
    }
}

impl Eq for BackgroundInfo {}

/// `td::GiveawayParameters` (GiveawayParameters.h:14-23).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GiveawayParameters {
    pub boosted_channel_id: ChannelId,
    pub additional_channel_ids: Vec<ChannelId>,
    pub only_new_subscribers: bool,
    pub winners_are_visible: bool,
    pub date: i32,
    pub country_codes: Vec<String>,
    pub prize_description: String,
}

impl GiveawayParameters {
    /// `GiveawayParameters::is_valid` (GiveawayParameters.cpp).
    pub fn is_valid(&self) -> bool {
        for channel_id in &self.additional_channel_ids {
            if !channel_id.is_valid() {
                return false;
            }
        }
        self.boosted_channel_id.is_valid() && self.date > 0
    }
}

/// `td::Game` (Game.h:18-29). The wire/td_api conversions and the
/// input-media helpers land with their slices.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Game {
    pub id: i64,
    pub access_hash: i64,
    pub bot_user_id: UserId,
    pub short_name: String,
    pub title: String,
    pub description: String,
    pub photo: Photo,
    pub animation_file_id: FileId,
    pub text: FormattedText,
}

impl Game {
    /// `Game::is_empty` (Game.cpp).
    pub fn is_empty(&self) -> bool {
        self.short_name.is_empty()
    }
}

/// `td::LabeledPricePart` (LabeledPricePart.h:13-16).
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct LabeledPricePart {
    pub label: String,
    pub amount: i64,
}

/// `td::InputInvoice::Invoice` (InputInvoice.h:17-39).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Invoice {
    pub currency: String,
    pub price_parts: Vec<LabeledPricePart>,
    pub subscription_period: i32,
    pub max_tip_amount: i64,
    pub suggested_tip_amounts: Vec<i64>,
    pub recurring_payment_terms_of_service_url: String,
    pub terms_of_service_url: String,
    pub is_test: bool,
    pub need_name: bool,
    pub need_phone_number: bool,
    pub need_email_address: bool,
    pub need_shipping_address: bool,
    pub send_phone_number_to_provider: bool,
    pub send_email_address_to_provider: bool,
    pub is_flexible: bool,
}

/// `td::InputInvoice` (InputInvoice.h:44-58).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InputInvoice {
    pub title: String,
    pub description: String,
    pub photo: Photo,
    pub start_parameter: String,
    pub invoice: Invoice,
    pub payload: String,
    pub provider_token: String,
    pub provider_data: String,
    pub extended_media: MessageExtendedMedia,
    pub extended_media_caption: FormattedText,
    pub total_amount: i64,
    pub receipt_message_id: MessageId,
}

/// The nested `MessageExtendedMedia::Type` (MessageExtendedMedia.h:13).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum MessageExtendedMediaType {
    #[default]
    Empty,
    Unsupported,
    Preview,
    Photo,
    Video,
}

/// `td::MessageExtendedMedia` (MessageExtendedMedia.h:12-36) — the C++
/// tagged fields flattened; the wire/td_api constructors and the
/// file-id/merge helpers land with their slices.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageExtendedMedia {
    pub r#type: MessageExtendedMediaType,
    pub unsupported_version: i32,
    pub duration: i32,
    pub dimensions: Dimensions,
    pub minithumbnail: String,
    pub photo: Photo,
    pub video_file_id: FileId,
    pub start_timestamp: i32,
}

impl MessageExtendedMedia {
    /// `MessageExtendedMedia::CURRENT_VERSION` (MessageExtendedMedia.h:16).
    pub const CURRENT_VERSION: i32 = 1;

    /// `MessageExtendedMedia::is_empty` (MessageExtendedMedia.h).
    pub fn is_empty(&self) -> bool {
        self.r#type == MessageExtendedMediaType::Empty
    }

    /// `MessageExtendedMedia::has_input_media` (MessageExtendedMedia.h).
    pub fn has_input_media(&self) -> bool {
        self.r#type == MessageExtendedMediaType::Photo
            || self.r#type == MessageExtendedMediaType::Video
    }

    /// `MessageExtendedMedia::is_unsupported` (MessageExtendedMedia.h).
    pub fn is_unsupported(&self) -> bool {
        self.r#type == MessageExtendedMediaType::Unsupported
    }

    /// `MessageExtendedMedia::need_reget` (MessageExtendedMedia.h).
    pub fn need_reget(&self) -> bool {
        self.r#type == MessageExtendedMediaType::Unsupported
            && self.unsupported_version < Self::CURRENT_VERSION
    }

    /// `MessageExtendedMedia::need_poll` (MessageExtendedMedia.h).
    pub fn need_poll(&self) -> bool {
        self.r#type == MessageExtendedMediaType::Preview
    }

    /// `MessageExtendedMedia::has_media_timestamp` (MessageExtendedMedia.h).
    pub fn has_media_timestamp(&self) -> bool {
        self.r#type == MessageExtendedMediaType::Video
    }
}

/// `td::ToDoItem` (ToDoItem.h:14-17). The wire/td_api constructors and
/// `validate` land with their slices.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ToDoItem {
    pub id: i32,
    pub title: FormattedText,
}

/// `td::ToDoList` (ToDoList.h:15-20).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ToDoList {
    pub title: FormattedText,
    pub items: Vec<ToDoItem>,
    pub others_can_append: bool,
    pub others_can_complete: bool,
}

/// `td::ToDoCompletion` (ToDoCompletion.h:18-24). The wire constructor,
/// `store`/`parse` and `add_dependencies` land with their slices.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ToDoCompletion {
    pub id: i32,
    pub completed_by_dialog_id: DialogId,
    pub date: i32,
}

impl ToDoCompletion {
    /// `ToDoCompletion::is_valid` (ToDoCompletion.h:29-31).
    pub fn is_valid(&self) -> bool {
        self.completed_by_dialog_id.is_valid() && self.date > 0
    }
}

/// `td::CommunityId` (CommunityId.h) — the id of a (newer-tree) community;
/// `0` is the unset default and the only invalid value.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct CommunityId(i64);

impl CommunityId {
    pub fn new(id: i64) -> CommunityId {
        CommunityId(id)
    }

    /// `CommunityId::is_valid` (CommunityId.h).
    pub fn is_valid(self) -> bool {
        self.0 > 0
    }

    pub fn get(self) -> i64 {
        self.0
    }

    /// `CommunityId::get_fake_dialog_id` (CommunityId.h) — the community
    /// reuses the channel dialog-id band.
    pub fn get_fake_dialog_id(self) -> DialogId {
        DialogId::from_channel_id(ChannelId::new(self.0))
    }

    /// `CommunityId::store` — raw `store_long`.
    pub fn store(self, out: &mut Vec<u8>) {
        store_i64(out, self.0);
    }

    /// `CommunityId::parse` — raw `fetch_long`.
    pub fn parse_value(input: &[u8]) -> Result<CommunityId, &'static str> {
        let mut parser = Parser::new(input);
        let id = parser.i64_()?;
        parser.fetch_end()?;
        Ok(CommunityId(id))
    }
}

/// `td::SharedDialog` (SharedDialog.h:21-60) — a dialog shared through a
/// `messageActionRequestedDialog`; `first_name`/`last_name`/`username`/
/// `photo` are filled only for shared users. The wire constructor,
/// `store`/`parse` and the td_api objects land with their slices.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SharedDialog {
    pub dialog_id: DialogId,
    pub first_name: String,
    pub last_name: String,
    pub username: String,
    pub photo: Photo,
}

impl SharedDialog {
    /// `SharedDialog::is_valid` (SharedDialog.h:30-32).
    pub fn is_valid(&self) -> bool {
        self.dialog_id.is_valid()
    }

    /// `SharedDialog::is_user` (SharedDialog.h:34-36).
    pub fn is_user(&self) -> bool {
        self.dialog_id.get_type() == crate::messages_manager::DialogType::User
    }

    /// `SharedDialog::is_dialog` (SharedDialog.h:38-42) — chats and
    /// channels; users and secret chats are not "dialogs" here.
    pub fn is_dialog(&self) -> bool {
        matches!(
            self.dialog_id.get_type(),
            crate::messages_manager::DialogType::Chat
                | crate::messages_manager::DialogType::Channel
        )
    }
}

/// `td::Address` (OrderInfo.h:16-24) — the Wave-3 Payments value the
/// `OrderInfo` carries; the wire/td_api constructors land with their
/// slices.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Address {
    pub country_code: String,
    pub state: String,
    pub city: String,
    pub street_line1: String,
    pub street_line2: String,
    pub postal_code: String,
}

/// `td::OrderInfo` (OrderInfo.h:38-45) — the bot-side order details the
/// payment-successful content carries behind `unique_ptr` (an `Option`
/// here).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OrderInfo {
    pub name: String,
    pub phone_number: String,
    pub email_address: String,
    pub shipping_address: Option<Box<Address>>,
}

/// `td::SecureValueType` (SecureValue.h:27-42) — the element-type enum the
/// passport contents carry. The full `SecureValue` plane is Wave-3
/// Passport; only the encrypted subset the message contents use is ported.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SecureValueType {
    #[default]
    None,
    PersonalDetails,
    Passport,
    DriverLicense,
    IdentityCard,
    InternalPassport,
    Address,
    UtilityBill,
    BankStatement,
    RentalAgreement,
    PassportRegistration,
    TemporaryRegistration,
    PhoneNumber,
    EmailAddress,
}

/// `td::DatedFile` (SecureValue.h:83-86).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DatedFile {
    pub file_id: FileId,
    pub date: i32,
}

/// `td::EncryptedSecureFile` (SecureValue.h:91-95).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EncryptedSecureFile {
    pub file: DatedFile,
    pub file_hash: String,
    pub encrypted_secret: String,
}

/// `td::EncryptedSecureData` (SecureValue.h:117-121).
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct EncryptedSecureData {
    pub data: String,
    pub hash: String,
    pub encrypted_secret: String,
}

/// `td::EncryptedSecureValue` (SecureValue.h:130-140).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EncryptedSecureValue {
    pub r#type: SecureValueType,
    pub data: EncryptedSecureData,
    pub files: Vec<EncryptedSecureFile>,
    pub front_side: EncryptedSecureFile,
    pub reverse_side: EncryptedSecureFile,
    pub selfie: EncryptedSecureFile,
    pub translations: Vec<EncryptedSecureFile>,
    pub hash: String,
}

/// `td::EncryptedSecureCredentials` (SecureValue.h:160-164).
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct EncryptedSecureCredentials {
    pub data: String,
    pub hash: String,
    pub encrypted_secret: String,
}

/// `MessageText` (MessageContent.cpp:155-181).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageText {
    pub text: FormattedText,
    pub web_page_id: WebPageId,
    pub force_small_media: bool,
    pub force_large_media: bool,
    pub skip_web_page_confirmation: bool,
    pub web_page_url: String,
}

impl MessageText {
    /// The `MessageText` constructor (MessageContent.cpp:162-177): the
    /// force-media flags only survive with a web page url, and a forced
    /// large media clears the small-media request.
    pub fn new(
        text: FormattedText,
        web_page_id: WebPageId,
        force_small_media: bool,
        force_large_media: bool,
        skip_web_page_confirmation: bool,
        web_page_url: String,
    ) -> MessageText {
        let (force_small_media, force_large_media) = if web_page_url.is_empty() {
            (false, false)
        } else if force_large_media {
            (false, true)
        } else {
            (force_small_media, force_large_media)
        };
        MessageText {
            text,
            web_page_id,
            force_small_media,
            force_large_media,
            skip_web_page_confirmation,
            web_page_url,
        }
    }
}

/// `MessageAnimation` (MessageContent.cpp:186-201).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageAnimation {
    pub file_id: FileId,
    pub caption: FormattedText,
    pub has_spoiler: bool,
}

/// `MessageAudio` (MessageContent.cpp:203-216).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageAudio {
    pub file_id: FileId,
    pub caption: FormattedText,
}

/// `MessageDocument` (MessageContent.cpp:218-231).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageDocument {
    pub file_id: FileId,
    pub caption: FormattedText,
}

/// `MessagePhoto` (MessageContent.cpp:233-249).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessagePhoto {
    pub photo: Photo,
    pub video_file_id: FileId,
    pub caption: FormattedText,
    pub has_spoiler: bool,
}

/// `MessageSticker` (MessageContent.cpp:251-263).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageSticker {
    pub file_id: FileId,
    pub is_premium: bool,
}

/// `MessageVideo` (MessageContent.cpp:265-296).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageVideo {
    pub file_id: FileId,
    pub alternative_file_ids: Vec<FileId>,
    pub hls_file_ids: Vec<FileId>,
    pub storyboard_file_ids: Vec<FileId>,
    pub storyboard_map_file_ids: Vec<FileId>,
    pub cover: Photo,
    pub start_timestamp: i32,
    pub caption: FormattedText,
    pub has_spoiler: bool,
}

/// `MessageVoiceNote` (MessageContent.cpp:298-313).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageVoiceNote {
    pub file_id: FileId,
    pub caption: FormattedText,
    pub is_listened: bool,
}

/// `MessageVideoNote` (MessageContent.cpp:637-650).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageVideoNote {
    pub file_id: FileId,
    pub is_viewed: bool,
}

/// `MessageChatCreate` (MessageContent.cpp:354-367).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageChatCreate {
    pub title: String,
    pub participant_user_ids: Vec<UserId>,
}

/// `MessageChatChangeTitle` (MessageContent.cpp:369-380).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageChatChangeTitle {
    pub title: String,
}

/// `MessageChatChangePhoto` (MessageContent.cpp:382-393).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageChatChangePhoto {
    pub photo: Photo,
}

/// `MessageChatDeletePhoto` (MessageContent.cpp:395-400).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageChatDeletePhoto;

/// `MessageChatDeleteHistory` (MessageContent.cpp:402-407).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageChatDeleteHistory;

/// `MessageChatAddUsers` (MessageContent.cpp:409-420).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageChatAddUsers {
    pub user_ids: Vec<UserId>,
}

/// `MessageChatJoinedByLink` (MessageContent.cpp:422-433).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageChatJoinedByLink {
    pub is_approved: bool,
}

/// `MessageChatDeleteUser` (MessageContent.cpp:435-446).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageChatDeleteUser {
    pub user_id: UserId,
}

/// `MessageChatMigrateTo` (MessageContent.cpp:448-459).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageChatMigrateTo {
    pub migrated_to_channel_id: ChannelId,
}

/// `MessageChannelCreate` (MessageContent.cpp:461-472).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageChannelCreate {
    pub title: String,
}

/// `MessageChannelMigrateFrom` (MessageContent.cpp:474-487).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageChannelMigrateFrom {
    pub title: String,
    pub migrated_from_chat_id: ChatId,
}

/// `MessagePinMessage` (MessageContent.cpp:489-500).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessagePinMessage {
    pub message_id: MessageId,
}

/// `MessageGameScore` (MessageContent.cpp:515-529).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageGameScore {
    pub game_message_id: MessageId,
    pub game_id: i64,
    pub score: i32,
}

/// `MessageScreenshotTaken` (MessageContent.cpp:531-536).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageScreenshotTaken;

/// `MessageChatSetTtl` (MessageContent.cpp:538-550).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageChatSetTtl {
    pub ttl: i32,
    pub from_user_id: UserId,
}

/// `MessageUnsupported` (MessageContent.cpp:552-564).
#[derive(Clone, Debug, PartialEq)]
pub struct MessageUnsupported {
    pub version: i32,
}

impl MessageUnsupported {
    /// `MessageUnsupported::CURRENT_VERSION` (MessageContent.cpp:554) —
    /// bumped each time a new message content type is added.
    pub const CURRENT_VERSION: i32 = 62;
}

impl Default for MessageUnsupported {
    fn default() -> Self {
        MessageUnsupported {
            version: Self::CURRENT_VERSION,
        }
    }
}

/// `MessageContactRegistered` (MessageContent.cpp:652-657).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageContactRegistered;

/// `MessageExpiredPhoto` (MessageContent.cpp:659-666).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageExpiredPhoto;

/// `MessageExpiredVideo` (MessageContent.cpp:668-675).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageExpiredVideo;

/// `MessageExpiredVideoNote` (MessageContent.cpp:1191-1198).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageExpiredVideoNote;

/// `MessageExpiredVoiceNote` (MessageContent.cpp:1200-1207).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageExpiredVoiceNote;

/// `MessageCustomServiceAction` (MessageContent.cpp:707-718).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageCustomServiceAction {
    pub message: String,
}

/// `MessageWebsiteConnected` (MessageContent.cpp:720-731).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageWebsiteConnected {
    pub domain_name: String,
}

/// `MessagePoll` (MessageContent.cpp:761-776).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessagePoll {
    pub poll_id: PollId,
    pub caption: FormattedText,
    pub attached_media: Option<Box<MessageContent>>,
}

/// `MessageDice` (MessageContent.cpp:778-836).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageDice {
    pub emoji: String,
    pub dice_value: i32,
    pub is_stake: bool,
    pub seed: String,
    pub state_hash: String,
    pub stake_ton_count: i64,
    pub prize_ton_count: i64,
}

impl MessageDice {
    /// `MessageDice::DEFAULT_EMOJI` (MessageContent.cpp:785).
    pub const DEFAULT_EMOJI: &'static str = "🎲";

    /// The `MessageDice` constructor (MessageContent.cpp:787-797): an empty
    /// emoji falls back to the default d20-family die and the rest runs
    /// through `remove_emoji_modifiers` (the C++ default
    /// `remove_selectors = true`).
    pub fn new(
        emoji: &str,
        dice_value: i32,
        is_stake: bool,
        seed: String,
        state_hash: String,
        stake_ton_count: i64,
        prize_ton_count: i64,
    ) -> MessageDice {
        MessageDice {
            emoji: if emoji.is_empty() {
                Self::DEFAULT_EMOJI.to_string()
            } else {
                remove_emoji_modifiers(emoji, true)
            },
            dice_value,
            is_stake,
            seed,
            state_hash,
            stake_ton_count,
            prize_ton_count,
        }
    }

    /// `MessageDice::is_valid` (MessageContent.cpp:804-825).
    pub fn is_valid(&self) -> bool {
        if self.dice_value < 0 {
            return false;
        }
        if self.emoji == Self::DEFAULT_EMOJI || self.emoji == "🎯" {
            if self.dice_value > 6 {
                return false;
            }
        } else if self.dice_value > 1000 {
            return false;
        }
        if self.is_stake {
            if self.dice_value != 0 {
                if self.emoji != Self::DEFAULT_EMOJI {
                    return false;
                }
                if self.seed.is_empty() {
                    return false;
                }
                if self.stake_ton_count <= 0 || self.prize_ton_count < 0 {
                    return false;
                }
            } else if self.state_hash.is_empty() || self.seed.len() != 32 {
                return false;
            }
        }
        true
    }
}

/// `MessageWebViewDataSent` (MessageContent.cpp:898-909).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageWebViewDataSent {
    pub button_text: String,
}

/// `MessageWebViewDataReceived` (MessageContent.cpp:911-924).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageWebViewDataReceived {
    pub button_text: String,
    pub data: String,
}

/// `MessageGiftPremium` (MessageContent.cpp:926-949).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageGiftPremium {
    pub text: FormattedText,
    pub currency: String,
    pub amount: i64,
    pub crypto_currency: String,
    pub crypto_amount: i64,
    pub days: i32,
}

/// `MessageGiftStars` (MessageContent.cpp:1279-1299).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageGiftStars {
    pub currency: String,
    pub amount: i64,
    pub crypto_currency: String,
    pub crypto_amount: i64,
    pub star_count: i64,
    pub transaction_id: String,
}

/// `MessageGiveawayLaunch` (MessageContent.cpp:1118-1128).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageGiveawayLaunch {
    pub star_count: i64,
}

/// `MessageGiveawayResults` (MessageContent.cpp:1131-1150).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageGiveawayResults {
    pub giveaway_message_id: MessageId,
    pub winner_count: i32,
    pub unclaimed_count: i32,
    pub is_star_giveaway: bool,
}

/// `MessageGiveawayWinners` (MessageContent.cpp:1152-1189).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageGiveawayWinners {
    pub giveaway_message_id: MessageId,
    pub boosted_channel_id: ChannelId,
    pub additional_dialog_count: i32,
    pub month_count: i32,
    pub star_count: i64,
    pub prize_description: String,
    pub winners_selection_date: i32,
    pub only_new_subscribers: bool,
    pub was_refunded: bool,
    pub winner_count: i32,
    pub unclaimed_count: i32,
    pub winner_user_ids: Vec<UserId>,
}

/// `MessageBoostApply` (MessageContent.cpp:1209-1215).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageBoostApply {
    pub boost_count: i32,
}

/// `MessagePaidMessagesRefunded` (MessageContent.cpp:1449-1458).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessagePaidMessagesRefunded {
    pub message_count: i32,
    pub star_count: i64,
}

/// `MessagePaidMessagesPrice` (MessageContent.cpp:1464-1474).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessagePaidMessagesPrice {
    pub star_count: i64,
    pub broadcast_messages_allowed: bool,
}

/// `MessageTodoCompletions` (MessageContent.cpp:1519-1533).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageTodoCompletions {
    pub to_do_message_id: MessageId,
    pub completed_task_ids: Vec<i32>,
    pub incompleted_task_ids: Vec<i32>,
}

/// `MessageGiftTon` (MessageContent.cpp:1553-1570).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageGiftTon {
    pub currency: String,
    pub amount: i64,
    pub crypto_currency: String,
    pub crypto_amount: i64,
    pub transaction_id: String,
}

/// `MessageSuggestedPostRefund` (MessageContent.cpp:1591-1602).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageSuggestedPostRefund {
    pub suggested_post_message_id: MessageId,
    pub is_payer_initiated: bool,
}

/// `MessageNewCreatorPending` (MessageContent.cpp:1688-1697).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageNewCreatorPending {
    pub new_creator_user_id: UserId,
}

/// `MessageChangeCreator` (MessageContent.cpp:1701-1710).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageChangeCreator {
    pub new_creator_user_id: UserId,
}

/// `MessageNoForwardsToggle` (MessageContent.cpp:1714-1728).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageNoForwardsToggle {
    pub request_message_id: MessageId,
    pub prev_value: bool,
    pub new_value: bool,
}

/// `MessageNoForwardsRequest` (MessageContent.cpp:1730-1740).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageNoForwardsRequest {
    pub is_expired: bool,
    pub prev_value: bool,
    pub new_value: bool,
}

/// `MessageManagedBotCreated` (MessageContent.cpp:1746-1754).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageManagedBotCreated {
    pub bot_user_id: UserId,
}

/// `MessagePollAppendAnswer` (MessageContent.cpp:1759-1771).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessagePollAppendAnswer {
    pub poll_message_id: MessageId,
    pub text: FormattedText,
    pub data: String,
}

/// `MessagePollDeleteAnswer` (MessageContent.cpp:1775-1787).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessagePollDeleteAnswer {
    pub poll_message_id: MessageId,
    pub text: FormattedText,
    pub data: String,
}

/// `MessageWriteAccessAllowed` (MessageContent.cpp:993-998).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageWriteAccessAllowed;

/// `MessageWriteAccessAllowedByRequest` (MessageContent.cpp:1058-1063).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageWriteAccessAllowedByRequest;

/// `MessageSuggestProfilePhoto` (MessageContent.cpp:980-991).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageSuggestProfilePhoto {
    pub photo: Photo,
}

/// `MessageSuggestBirthday` (MessageContent.cpp:1631-1639) — the field is
/// the packed `td::Birthdate`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageSuggestBirthday {
    pub birthdate: Birthdate,
}

/// `MessageCall` (MessageContent.cpp:566-586).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageCall {
    pub call_id: i64,
    pub call_access_hash: i64,
    pub duration: i32,
    pub discard_reason: CallDiscardReason,
    pub is_video: bool,
}

/// `MessageGroupCall` (MessageContent.cpp:854-868).
#[derive(Clone, Debug, PartialEq)]
pub struct MessageGroupCall {
    pub input_group_call_id: InputGroupCallId,
    pub duration: i32,
    pub schedule_date: i32,
}

impl Default for MessageGroupCall {
    fn default() -> Self {
        MessageGroupCall {
            input_group_call_id: InputGroupCallId::default(),
            duration: -1,
            schedule_date: -1,
        }
    }
}

/// `MessageInviteToGroupCall` (MessageContent.cpp:870-883).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageInviteToGroupCall {
    pub input_group_call_id: InputGroupCallId,
    pub user_ids: Vec<UserId>,
}

/// `MessageLocation` (MessageContent.cpp:328-339).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageLocation {
    pub location: Location,
}

/// `MessageVenue` (MessageContent.cpp:341-352).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageVenue {
    pub venue: Venue,
}

/// `MessageLiveLocation` (MessageContent.cpp:677-711).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageLiveLocation {
    pub location: Location,
    pub period: i32,
    pub heading: i32,
    pub proximity_alert_radius: i32,
}

impl MessageLiveLocation {
    /// The `MessageLiveLocation` constructor (MessageContent.cpp:684-710):
    /// a negative period/radius resets to zero and a heading outside
    /// 0..=360 logs and resets.
    pub fn new(
        location: Location,
        period: i32,
        heading: i32,
        proximity_alert_radius: i32,
    ) -> MessageLiveLocation {
        let mut result = MessageLiveLocation {
            location,
            period,
            heading,
            proximity_alert_radius,
        };
        if result.period < 0 {
            result.period = 0;
        }
        if !(0..=360).contains(&result.heading) {
            tdutils::log_error!("Receive wrong heading {}", heading);
            result.heading = 0;
        }
        if result.proximity_alert_radius < 0 {
            result.proximity_alert_radius = 0;
        }
        result
    }
}

/// `MessageTopicCreate` (MessageContent.cpp:951-965).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageTopicCreate {
    pub title: String,
    pub icon: ForumTopicIcon,
    pub title_missing: bool,
}

/// `MessageTopicEdit` (MessageContent.cpp:967-977).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageTopicEdit {
    pub edited_data: ForumTopicEditedData,
}

/// `MessageSuggestedPostSuccess` (MessageContent.cpp:1576-1590).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageSuggestedPostSuccess {
    pub suggested_post_message_id: MessageId,
    pub amount: CurrencyAmount,
}

/// `MessageSuggestedPostApproval` (MessageContent.cpp:1606-1630).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageSuggestedPostApproval {
    pub suggested_post_message_id: MessageId,
    pub price: SuggestedPostPrice,
    pub comment: String,
    pub send_date: i32,
    pub is_rejected: bool,
    pub is_balance_too_low: bool,
}

/// `MessageWebViewWriteAccessAllowed` (MessageContent.cpp:1015-1026).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageWebViewWriteAccessAllowed {
    pub web_app: WebApp,
}

/// `MessageSetBackground` (MessageContent.cpp:1028-1044).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageSetBackground {
    pub old_message_id: MessageId,
    pub background_info: BackgroundInfo,
    pub for_both: bool,
}

/// `MessageGiveaway` (MessageContent.cpp:1098-1120).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageGiveaway {
    pub giveaway_parameters: GiveawayParameters,
    pub quantity: i32,
    pub months: i32,
    pub star_count: i64,
}

/// `MessageGame` (MessageContent.cpp:502-513).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageGame {
    pub game: Game,
}

/// `MessageInvoice` (MessageContent.cpp:588-599).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageInvoice {
    pub input_invoice: InputInvoice,
}

/// `MessagePassportDataSent` (MessageContent.cpp:733-744).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessagePassportDataSent {
    pub types: Vec<SecureValueType>,
}

/// `MessagePassportDataReceived` (MessageContent.cpp:746-765).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessagePassportDataReceived {
    pub values: Vec<EncryptedSecureValue>,
    pub credentials: EncryptedSecureCredentials,
}

/// `MessagePaidMedia` (MessageContent.cpp:1237-1255).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessagePaidMedia {
    pub media: Vec<MessageExtendedMedia>,
    pub caption: FormattedText,
    pub star_count: i64,
    pub payload: String,
}

/// `MessageTodoAppendTasks` (MessageContent.cpp:1538-1551).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageTodoAppendTasks {
    pub to_do_message_id: MessageId,
    pub items: Vec<ToDoItem>,
}

/// `MessageProximityAlertTriggered` (MessageContent.cpp:838-853).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageProximityAlertTriggered {
    pub traveler_dialog_id: DialogId,
    pub watcher_dialog_id: DialogId,
    pub distance: i32,
}

/// `MessagePaymentSuccessful` (MessageContent.cpp:601-633) — the bots-only
/// tail (`shipping_option_id`/`order_info`/charge ids) is filled only for
/// bot receivers. The wire-ctor keeps `is_recurring || is_first_recurring`
/// normalization for the conversion slice.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessagePaymentSuccessful {
    pub invoice_dialog_id: DialogId,
    pub invoice_message_id: MessageId,
    pub currency: String,
    pub total_amount: i64,
    pub invoice_payload: String,
    pub subscription_until_date: i32,
    pub is_recurring: bool,
    pub is_first_recurring: bool,
    pub shipping_option_id: String,
    pub order_info: Option<OrderInfo>,
    pub telegram_payment_charge_id: String,
    pub provider_payment_charge_id: String,
}

/// `MessageRequestedDialog` (MessageContent.cpp:1000-1012).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageRequestedDialog {
    pub shared_dialog_ids: Vec<DialogId>,
    pub button_id: i32,
}

/// `MessageStory` (MessageContent.cpp:1044-1056).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageStory {
    pub story_full_id: StoryFullId,
    pub via_mention: bool,
}

/// `MessageGiftCode` (MessageContent.cpp:1065-1095) — the wire-ctor keeps
/// the `via_giveaway || is_unclaimed` normalization for the conversion
/// slice.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageGiftCode {
    pub creator_dialog_id: DialogId,
    pub text: FormattedText,
    pub days: i32,
    pub currency: String,
    pub amount: i64,
    pub crypto_currency: String,
    pub crypto_amount: i64,
    pub via_giveaway: bool,
    pub is_unclaimed: bool,
    pub code: String,
}

/// `MessageDialogShared` (MessageContent.cpp:1222-1234).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageDialogShared {
    pub shared_dialogs: Vec<SharedDialog>,
    pub button_id: i32,
}

/// `MessagePaymentRefunded` (MessageContent.cpp:1254-1276).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessagePaymentRefunded {
    pub dialog_id: DialogId,
    pub currency: String,
    pub total_amount: i64,
    pub invoice_payload: String,
    pub telegram_payment_charge_id: String,
    pub provider_payment_charge_id: String,
}

/// `MessagePrizeStars` (MessageContent.cpp:1304-1324).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessagePrizeStars {
    pub star_count: i64,
    pub transaction_id: String,
    pub boosted_dialog_id: DialogId,
    pub giveaway_message_id: MessageId,
    pub is_unclaimed: bool,
}

/// `MessageConferenceCall` (MessageContent.cpp:1479-1501).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageConferenceCall {
    pub call_id: i64,
    pub duration: i32,
    pub other_participant_dialog_ids: Vec<DialogId>,
    pub is_active: bool,
    pub is_video: bool,
    pub was_missed: bool,
}

/// `MessageToDoList` (MessageContent.cpp:1504-1516).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageToDoList {
    pub list: ToDoList,
    pub completions: Vec<ToDoCompletion>,
}

/// `MessageChangeCommunity` (MessageContent.cpp:1804-1812).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageChangeCommunity {
    pub community_id: CommunityId,
}

/// `MessageChatJoinedViaCommunity` (MessageContent.cpp:1817-1825).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageChatJoinedViaCommunity {
    pub community_id: CommunityId,
}

/// `td::MessageContent` (MessageContent.h:62-72) — the C++ class hierarchy
/// flattened into an enum of structs. The C++ header comment applies to
/// every inheritor: do not forget to update `merge_message_contents` and
/// `compare_message_contents` (M.9.10/M.9.11) when a variant changes.
///
/// The C++ subclasses live behind `unique_ptr<MessageContent>`; this enum
/// stores them inline, so the heavy tails (`Invoice` 688 bytes vs
/// `Video` 304) widen every slot. Boxing the tail is the recorded
/// alternative if a consumer ever pays for it (the M.7
/// `PendingUpdate::Wire` precedent).
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
pub enum MessageContent {
    Text(MessageText),
    Animation(MessageAnimation),
    Audio(MessageAudio),
    Document(MessageDocument),
    Photo(MessagePhoto),
    Sticker(MessageSticker),
    Video(MessageVideo),
    VoiceNote(MessageVoiceNote),
    VideoNote(MessageVideoNote),
    ChatCreate(MessageChatCreate),
    ChatChangeTitle(MessageChatChangeTitle),
    ChatChangePhoto(MessageChatChangePhoto),
    ChatDeletePhoto(MessageChatDeletePhoto),
    ChatDeleteHistory(MessageChatDeleteHistory),
    ChatAddUsers(MessageChatAddUsers),
    ChatJoinedByLink(MessageChatJoinedByLink),
    ChatDeleteUser(MessageChatDeleteUser),
    ChatMigrateTo(MessageChatMigrateTo),
    ChannelCreate(MessageChannelCreate),
    ChannelMigrateFrom(MessageChannelMigrateFrom),
    PinMessage(MessagePinMessage),
    GameScore(MessageGameScore),
    ScreenshotTaken(MessageScreenshotTaken),
    ChatSetTtl(MessageChatSetTtl),
    Unsupported(MessageUnsupported),
    ContactRegistered(MessageContactRegistered),
    ExpiredPhoto(MessageExpiredPhoto),
    ExpiredVideo(MessageExpiredVideo),
    ExpiredVideoNote(MessageExpiredVideoNote),
    ExpiredVoiceNote(MessageExpiredVoiceNote),
    CustomServiceAction(MessageCustomServiceAction),
    WebsiteConnected(MessageWebsiteConnected),
    Poll(MessagePoll),
    Dice(MessageDice),
    WebViewDataSent(MessageWebViewDataSent),
    WebViewDataReceived(MessageWebViewDataReceived),
    GiftPremium(MessageGiftPremium),
    GiftStars(MessageGiftStars),
    GiveawayLaunch(MessageGiveawayLaunch),
    GiveawayResults(MessageGiveawayResults),
    GiveawayWinners(MessageGiveawayWinners),
    BoostApply(MessageBoostApply),
    PaidMessagesRefunded(MessagePaidMessagesRefunded),
    PaidMessagesPrice(MessagePaidMessagesPrice),
    TodoCompletions(MessageTodoCompletions),
    GiftTon(MessageGiftTon),
    SuggestedPostRefund(MessageSuggestedPostRefund),
    NewCreatorPending(MessageNewCreatorPending),
    ChangeCreator(MessageChangeCreator),
    NoForwardsToggle(MessageNoForwardsToggle),
    NoForwardsRequest(MessageNoForwardsRequest),
    ManagedBotCreated(MessageManagedBotCreated),
    PollAppendAnswer(MessagePollAppendAnswer),
    PollDeleteAnswer(MessagePollDeleteAnswer),
    WriteAccessAllowed(MessageWriteAccessAllowed),
    WriteAccessAllowedByRequest(MessageWriteAccessAllowedByRequest),
    SuggestProfilePhoto(MessageSuggestProfilePhoto),
    SuggestBirthday(MessageSuggestBirthday),
    Call(MessageCall),
    GroupCall(MessageGroupCall),
    InviteToGroupCall(MessageInviteToGroupCall),
    // The M.9.1c/M.9.1d additions; the C++ positions are pinned by the
    // `MessageContentType` discriminant enum, not by this model enum.
    Location(MessageLocation),
    Venue(MessageVenue),
    LiveLocation(MessageLiveLocation),
    TopicCreate(MessageTopicCreate),
    TopicEdit(MessageTopicEdit),
    SuggestedPostSuccess(MessageSuggestedPostSuccess),
    SuggestedPostApproval(MessageSuggestedPostApproval),
    WebViewWriteAccessAllowed(MessageWebViewWriteAccessAllowed),
    SetBackground(MessageSetBackground),
    Giveaway(MessageGiveaway),
    Game(MessageGame),
    Invoice(MessageInvoice),
    PassportDataSent(MessagePassportDataSent),
    PassportDataReceived(MessagePassportDataReceived),
    PaidMedia(MessagePaidMedia),
    TodoAppendTasks(MessageTodoAppendTasks),
    // The M.9.1b additions; the C++ positions are pinned by the
    // `MessageContentType` discriminant enum, not by this model enum.
    PaymentSuccessful(MessagePaymentSuccessful),
    ProximityAlertTriggered(MessageProximityAlertTriggered),
    RequestedDialog(MessageRequestedDialog),
    Story(MessageStory),
    GiftCode(MessageGiftCode),
    DialogShared(MessageDialogShared),
    PaymentRefunded(MessagePaymentRefunded),
    PrizeStars(MessagePrizeStars),
    ConferenceCall(MessageConferenceCall),
    ToDoList(MessageToDoList),
    ChangeCommunity(MessageChangeCommunity),
    ChatJoinedViaCommunity(MessageChatJoinedViaCommunity),
}

impl MessageContent {
    /// The per-class `MessageContent::get_type` overrides
    /// (MessageContent.cpp:155-1791).
    pub fn get_type(&self) -> MessageContentType {
        match self {
            MessageContent::Text(_) => MessageContentType::Text,
            MessageContent::Animation(_) => MessageContentType::Animation,
            MessageContent::Audio(_) => MessageContentType::Audio,
            MessageContent::Document(_) => MessageContentType::Document,
            MessageContent::Photo(_) => MessageContentType::Photo,
            MessageContent::Sticker(_) => MessageContentType::Sticker,
            MessageContent::Video(_) => MessageContentType::Video,
            MessageContent::VoiceNote(_) => MessageContentType::VoiceNote,
            MessageContent::VideoNote(_) => MessageContentType::VideoNote,
            MessageContent::ChatCreate(_) => MessageContentType::ChatCreate,
            MessageContent::ChatChangeTitle(_) => MessageContentType::ChatChangeTitle,
            MessageContent::ChatChangePhoto(_) => MessageContentType::ChatChangePhoto,
            MessageContent::ChatDeletePhoto(_) => MessageContentType::ChatDeletePhoto,
            MessageContent::ChatDeleteHistory(_) => MessageContentType::ChatDeleteHistory,
            MessageContent::ChatAddUsers(_) => MessageContentType::ChatAddUsers,
            MessageContent::ChatJoinedByLink(_) => MessageContentType::ChatJoinedByLink,
            MessageContent::ChatDeleteUser(_) => MessageContentType::ChatDeleteUser,
            MessageContent::ChatMigrateTo(_) => MessageContentType::ChatMigrateTo,
            MessageContent::ChannelCreate(_) => MessageContentType::ChannelCreate,
            MessageContent::ChannelMigrateFrom(_) => MessageContentType::ChannelMigrateFrom,
            MessageContent::PinMessage(_) => MessageContentType::PinMessage,
            MessageContent::GameScore(_) => MessageContentType::GameScore,
            MessageContent::ScreenshotTaken(_) => MessageContentType::ScreenshotTaken,
            MessageContent::ChatSetTtl(_) => MessageContentType::ChatSetTtl,
            MessageContent::Unsupported(_) => MessageContentType::Unsupported,
            MessageContent::ContactRegistered(_) => MessageContentType::ContactRegistered,
            MessageContent::ExpiredPhoto(_) => MessageContentType::ExpiredPhoto,
            MessageContent::ExpiredVideo(_) => MessageContentType::ExpiredVideo,
            MessageContent::ExpiredVideoNote(_) => MessageContentType::ExpiredVideoNote,
            MessageContent::ExpiredVoiceNote(_) => MessageContentType::ExpiredVoiceNote,
            MessageContent::CustomServiceAction(_) => MessageContentType::CustomServiceAction,
            MessageContent::WebsiteConnected(_) => MessageContentType::WebsiteConnected,
            MessageContent::Poll(_) => MessageContentType::Poll,
            MessageContent::Dice(_) => MessageContentType::Dice,
            MessageContent::WebViewDataSent(_) => MessageContentType::WebViewDataSent,
            MessageContent::WebViewDataReceived(_) => MessageContentType::WebViewDataReceived,
            MessageContent::GiftPremium(_) => MessageContentType::GiftPremium,
            MessageContent::GiftStars(_) => MessageContentType::GiftStars,
            MessageContent::GiveawayLaunch(_) => MessageContentType::GiveawayLaunch,
            MessageContent::GiveawayResults(_) => MessageContentType::GiveawayResults,
            MessageContent::GiveawayWinners(_) => MessageContentType::GiveawayWinners,
            MessageContent::BoostApply(_) => MessageContentType::BoostApply,
            MessageContent::PaidMessagesRefunded(_) => MessageContentType::PaidMessagesRefunded,
            MessageContent::PaidMessagesPrice(_) => MessageContentType::PaidMessagesPrice,
            MessageContent::TodoCompletions(_) => MessageContentType::TodoCompletions,
            MessageContent::GiftTon(_) => MessageContentType::GiftTon,
            MessageContent::SuggestedPostRefund(_) => MessageContentType::SuggestedPostRefund,
            MessageContent::NewCreatorPending(_) => MessageContentType::NewCreatorPending,
            MessageContent::ChangeCreator(_) => MessageContentType::ChangeCreator,
            MessageContent::NoForwardsToggle(_) => MessageContentType::NoForwardsToggle,
            MessageContent::NoForwardsRequest(_) => MessageContentType::NoForwardsRequest,
            MessageContent::ManagedBotCreated(_) => MessageContentType::ManagedBotCreated,
            MessageContent::PollAppendAnswer(_) => MessageContentType::PollAppendAnswer,
            MessageContent::PollDeleteAnswer(_) => MessageContentType::PollDeleteAnswer,
            MessageContent::WriteAccessAllowed(_) => MessageContentType::WriteAccessAllowed,
            MessageContent::WriteAccessAllowedByRequest(_) => {
                MessageContentType::WriteAccessAllowedByRequest
            }
            MessageContent::SuggestProfilePhoto(_) => MessageContentType::SuggestProfilePhoto,
            MessageContent::SuggestBirthday(_) => MessageContentType::SuggestBirthday,
            MessageContent::Call(_) => MessageContentType::Call,
            MessageContent::GroupCall(_) => MessageContentType::GroupCall,
            MessageContent::InviteToGroupCall(_) => MessageContentType::InviteToGroupCall,
            MessageContent::Location(_) => MessageContentType::Location,
            MessageContent::Venue(_) => MessageContentType::Venue,
            MessageContent::LiveLocation(_) => MessageContentType::LiveLocation,
            MessageContent::TopicCreate(_) => MessageContentType::TopicCreate,
            MessageContent::TopicEdit(_) => MessageContentType::TopicEdit,
            MessageContent::SuggestedPostSuccess(_) => MessageContentType::SuggestedPostSuccess,
            MessageContent::SuggestedPostApproval(_) => MessageContentType::SuggestedPostApproval,
            MessageContent::WebViewWriteAccessAllowed(_) => {
                MessageContentType::WebViewWriteAccessAllowed
            }
            MessageContent::SetBackground(_) => MessageContentType::SetBackground,
            MessageContent::Giveaway(_) => MessageContentType::Giveaway,
            MessageContent::Game(_) => MessageContentType::Game,
            MessageContent::Invoice(_) => MessageContentType::Invoice,
            MessageContent::PassportDataSent(_) => MessageContentType::PassportDataSent,
            MessageContent::PassportDataReceived(_) => MessageContentType::PassportDataReceived,
            MessageContent::PaidMedia(_) => MessageContentType::PaidMedia,
            MessageContent::TodoAppendTasks(_) => MessageContentType::TodoAppendTasks,
            MessageContent::PaymentSuccessful(_) => MessageContentType::PaymentSuccessful,
            MessageContent::ProximityAlertTriggered(_) => {
                MessageContentType::ProximityAlertTriggered
            }
            MessageContent::RequestedDialog(_) => MessageContentType::RequestedDialog,
            MessageContent::Story(_) => MessageContentType::Story,
            MessageContent::GiftCode(_) => MessageContentType::GiftCode,
            MessageContent::DialogShared(_) => MessageContentType::DialogShared,
            MessageContent::PaymentRefunded(_) => MessageContentType::PaymentRefunded,
            MessageContent::PrizeStars(_) => MessageContentType::PrizeStars,
            MessageContent::ConferenceCall(_) => MessageContentType::ConferenceCall,
            MessageContent::ToDoList(_) => MessageContentType::ToDoList,
            MessageContent::ChangeCommunity(_) => MessageContentType::ChangeCommunity,
            MessageContent::ChatJoinedViaCommunity(_) => MessageContentType::ChatJoinedViaCommunity,
        }
    }
}

/// `create_empty_text_message_content` (MessageContent.cpp:4653-4655).
pub fn create_empty_text_message_content() -> MessageContent {
    MessageContent::Text(MessageText::default())
}

/// `create_text_message_content` (MessageContent.cpp:4657-4663).
pub fn create_text_message_content(
    text: String,
    entities: Vec<MessageEntity>,
    web_page_id: WebPageId,
    force_small_media: bool,
    force_large_media: bool,
    skip_confirmation: bool,
    web_page_url: String,
) -> MessageContent {
    MessageContent::Text(MessageText::new(
        FormattedText { text, entities },
        web_page_id,
        force_small_media,
        force_large_media,
        skip_confirmation,
        web_page_url,
    ))
}

/// `create_animation_message_content` (MessageContent.cpp:4669-4671).
pub fn create_animation_message_content(animation_file_id: FileId) -> MessageContent {
    MessageContent::Animation(MessageAnimation {
        file_id: animation_file_id,
        caption: FormattedText::default(),
        has_spoiler: false,
    })
}

/// `create_audio_message_content` (MessageContent.cpp:4673-4675).
pub fn create_audio_message_content(audio_file_id: FileId) -> MessageContent {
    MessageContent::Audio(MessageAudio {
        file_id: audio_file_id,
        caption: FormattedText::default(),
    })
}

/// `create_document_message_content` (MessageContent.cpp:4677-4679).
pub fn create_document_message_content(document_file_id: FileId) -> MessageContent {
    MessageContent::Document(MessageDocument {
        file_id: document_file_id,
        caption: FormattedText::default(),
    })
}

/// `create_photo_message_content` (MessageContent.cpp:4681-4683).
pub fn create_photo_message_content(photo: Photo, video_file_id: FileId) -> MessageContent {
    MessageContent::Photo(MessagePhoto {
        photo,
        video_file_id,
        caption: FormattedText::default(),
        has_spoiler: false,
    })
}

/// `create_video_message_content` (MessageContent.cpp:4685-4688).
pub fn create_video_message_content(
    file_id: FileId,
    cover: Photo,
    start_timestamp: i32,
) -> MessageContent {
    MessageContent::Video(MessageVideo {
        file_id,
        alternative_file_ids: Vec::new(),
        hls_file_ids: Vec::new(),
        storyboard_file_ids: Vec::new(),
        storyboard_map_file_ids: Vec::new(),
        cover,
        start_timestamp,
        caption: FormattedText::default(),
        has_spoiler: false,
    })
}

/// `create_voice_note_message_content` (MessageContent.cpp:4690-4692).
pub fn create_voice_note_message_content(voice_note_file_id: FileId) -> MessageContent {
    MessageContent::VoiceNote(MessageVoiceNote {
        file_id: voice_note_file_id,
        caption: FormattedText::default(),
        is_listened: false,
    })
}

/// `create_contact_registered_message_content` (MessageContent.cpp:4694-4696).
pub fn create_contact_registered_message_content() -> MessageContent {
    MessageContent::ContactRegistered(MessageContactRegistered)
}

/// `create_screenshot_taken_message_content` (MessageContent.cpp:4698-4700).
pub fn create_screenshot_taken_message_content() -> MessageContent {
    MessageContent::ScreenshotTaken(MessageScreenshotTaken)
}

/// `create_chat_set_ttl_message_content` (MessageContent.cpp:4702-4704).
pub fn create_chat_set_ttl_message_content(ttl: i32, from_user_id: UserId) -> MessageContent {
    MessageContent::ChatSetTtl(MessageChatSetTtl { ttl, from_user_id })
}

// ---------------------------------------------------------------------
// M.9.2/M.9.3 — the database/log-event serialization plane
// (`store_message_content`/`parse_message_content` from
// MessageContent.cpp:1830-3035/3035-4505 plus every companion-type
// store/parse template the two dispatchers call through).
//
// The C++ walks file-bearing types through the owning manager
// (`animations_manager_->store_animation(...)` and friends), which
// serializes the manager-side record and ends in
// `FileManager::store_file`. Those manager records are the owners'
// slices (M.12.2 for the file plane, the sticker/animations/... manager
// slices for their records, Waves 2/3), so they stay behind the
// [`ContentStoreContext`] seam — byte-format-exact elsewhere.
// ---------------------------------------------------------------------

/// The manager-mediated half of the content serialization — the Rust
/// stand-in for the `Td *` context the C++ pulls out of the storer.
///
/// Every method here is byte-exact in the C++ but owned by a slice that
/// has not landed on this line yet; the round-trip test implementation
/// covers the dispatcher plumbing until they land.
pub trait ContentStoreContext {
    /// `AnimationsManager::store_animation` (AnimationsManager.hpp:21-41).
    fn store_animation(&mut self, file_id: FileId, out: &mut Vec<u8>);
    /// `AnimationsManager::parse_animation` (AnimationsManager.hpp:43-...).
    fn parse_animation(&mut self, input: &mut Parser) -> Result<FileId, &'static str>;
    /// `AudiosManager::store_audio`.
    fn store_audio(&mut self, file_id: FileId, out: &mut Vec<u8>);
    /// `AudiosManager::parse_audio`.
    fn parse_audio(&mut self, input: &mut Parser) -> Result<FileId, &'static str>;
    /// `DocumentsManager::store_document`.
    fn store_document(&mut self, file_id: FileId, out: &mut Vec<u8>);
    /// `DocumentsManager::parse_document`.
    fn parse_document(&mut self, input: &mut Parser) -> Result<FileId, &'static str>;
    /// `StickersManager::store_sticker` — the content dispatcher always
    /// passes `in_sticker_set = false` (MessageContent.cpp:1905).
    fn store_sticker(&mut self, file_id: FileId, in_sticker_set: bool, out: &mut Vec<u8>);
    /// `StickersManager::parse_sticker`.
    fn parse_sticker(
        &mut self,
        in_sticker_set: bool,
        input: &mut Parser,
    ) -> Result<FileId, &'static str>;
    /// `VideosManager::store_video`.
    fn store_video(&mut self, file_id: FileId, out: &mut Vec<u8>);
    /// `VideosManager::parse_video`.
    fn parse_video(&mut self, input: &mut Parser) -> Result<FileId, &'static str>;
    /// `VideoNotesManager::store_video_note`.
    fn store_video_note(&mut self, file_id: FileId, out: &mut Vec<u8>);
    /// `VideoNotesManager::parse_video_note`.
    fn parse_video_note(&mut self, input: &mut Parser) -> Result<FileId, &'static str>;
    /// `VoiceNotesManager::store_voice_note`.
    fn store_voice_note(&mut self, file_id: FileId, out: &mut Vec<u8>);
    /// `VoiceNotesManager::parse_voice_note`.
    fn parse_voice_note(&mut self, input: &mut Parser) -> Result<FileId, &'static str>;
    /// `FileManager::store_file` (FileManager.hpp:30-131) — the
    /// `store(FileId)` overload every photo-side structure ends in.
    fn store_file(&mut self, file_id: FileId, out: &mut Vec<u8>);
    /// `FileManager::parse_file` (FileManager.hpp:135-...).
    fn parse_file(&mut self, input: &mut Parser) -> Result<FileId, &'static str>;
    /// `BackgroundManager::store_background` (BackgroundInfo.hpp:8-12).
    fn store_background_id(&mut self, background_id: BackgroundId, out: &mut Vec<u8>);
    /// `BackgroundManager::parse_background`.
    fn parse_background_id(&mut self, input: &mut Parser) -> Result<BackgroundId, &'static str>;
    /// `WebPagesManager::on_load_web_page_url_from_database` — the
    /// side-effect of `MessageText` parse (MessageContent.cpp:3174-3176);
    /// the cache update is the WebPages slice (Wave 2).
    fn on_loaded_web_page_url(&mut self, _web_page_id: WebPageId, _url: &str) {}
}

/// `td::store(vector<int64>)` framing (i32 size + elements), which
/// [`store_parse`] has no shared helper for.
fn store_i64_vec(out: &mut Vec<u8>, values: &[i64]) {
    store_i32(out, values.len() as i32);
    for value in values {
        store_i64(out, *value);
    }
}

/// The [`store_i64_vec`] reader (`"Wrong vector length"` on a negative
/// size, like the shared helpers).
fn parse_i64_vec(input: &mut Parser) -> Result<Vec<i64>, &'static str> {
    let len = input.i32_()?;
    if len < 0 {
        return Err("Wrong vector length");
    }
    let mut result = Vec::with_capacity(len as usize);
    for _ in 0..len {
        result.push(input.i64_()?);
    }
    Ok(result)
}

/// `get_premium_duration_day_count` (misc.cpp:423-428).
pub(crate) fn get_premium_duration_day_count(month_count: i32) -> i32 {
    if month_count <= 0 || month_count > 10_000_000 {
        return 7;
    }
    month_count * 30 + month_count / 3 + month_count / 12
}

/// `Location::store` (Location.h:60-77).
fn store_location(location: &Location, out: &mut Vec<u8>) {
    let has_access_hash = location.access_hash != 0;
    let has_horizontal_accuracy = location.horizontal_accuracy > 0.0;
    store_flags::<3>(
        out,
        [location.is_empty, has_access_hash, has_horizontal_accuracy],
    );
    store_f64(out, location.latitude);
    store_f64(out, location.longitude);
    if has_access_hash {
        store_i64(out, location.access_hash);
    }
    if has_horizontal_accuracy {
        store_f64(out, location.horizontal_accuracy);
    }
}

/// `Location::parse` (Location.h:79-100). The C++ also feeds parsed
/// access hashes into the Global cache (`add_location_access_hash`);
/// that cache is the Global slice, and the omission is byte-invisible.
fn parse_location(input: &mut Parser) -> Result<Location, &'static str> {
    let [is_empty, has_access_hash, has_horizontal_accuracy] = input.parse_flags::<3>()?;
    let latitude = input.f64_()?;
    let longitude = input.f64_()?;
    let access_hash = if has_access_hash { input.i64_()? } else { 0 };
    let horizontal_accuracy = if has_horizontal_accuracy {
        input.f64_()?
    } else {
        0.0
    };
    Ok(Location {
        is_empty,
        latitude,
        longitude,
        horizontal_accuracy,
        access_hash,
    })
}

/// `Venue::store` (Venue.h:66-80).
fn store_venue(venue: &Venue, out: &mut Vec<u8>) {
    store_location(&venue.location, out);
    store_string(out, &venue.title);
    store_string(out, &venue.address);
    store_string(out, &venue.provider);
    store_string(out, &venue.id);
    store_string(out, &venue.r#type);
}

/// `Venue::parse` (Venue.h:82-95) — the type rides the
/// `Version::AddVenueType` gate.
fn parse_venue(input: &mut Parser) -> Result<Venue, &'static str> {
    let location = parse_location(input)?;
    let title = input.string()?;
    let address = input.string()?;
    let provider = input.string()?;
    let id = input.string()?;
    let r#type = if input.version() >= version::ADD_VENUE_TYPE {
        input.string()?
    } else {
        String::new()
    };
    Ok(Venue {
        location,
        title,
        address,
        provider,
        id,
        r#type,
    })
}

/// `store(Dimensions)` (Dimensions.hpp:8-11).
fn store_dimensions(dimensions: &Dimensions, out: &mut Vec<u8>) {
    let packed = (dimensions.width as u32) << 16 | dimensions.height as u32;
    store_u32(out, packed);
}

/// `parse(Dimensions)` (Dimensions.hpp:13-19).
fn parse_dimensions(input: &mut Parser) -> Result<Dimensions, &'static str> {
    let width_height = input.u32_()?;
    Ok(Dimensions {
        width: (width_height >> 16) as u16,
        height: (width_height & 0xFFFF) as u16,
    })
}

/// `PhotoSize::store` (PhotoSize.hpp:6-13).
fn store_photo_size(photo_size: &PhotoSize, ctx: &mut dyn ContentStoreContext, out: &mut Vec<u8>) {
    store_i32(out, photo_size.size_type);
    store_dimensions(&photo_size.dimensions, out);
    store_i32(out, photo_size.size);
    ctx.store_file(photo_size.file_id, out);
    store_i32_vec(out, &photo_size.progressive_sizes);
}

/// `PhotoSize::parse` (PhotoSize.hpp:15-26) — the progressive sizes
/// ride `Version::AddPhotoProgressiveSizes`.
fn parse_photo_size(
    ctx: &mut dyn ContentStoreContext,
    input: &mut Parser,
) -> Result<PhotoSize, &'static str> {
    let size_type = input.i32_()?;
    if !(0..128).contains(&size_type) {
        return Err("Wrong photo size type");
    }
    let dimensions = parse_dimensions(input)?;
    let size = input.i32_()?;
    let file_id = ctx.parse_file(input)?;
    let progressive_sizes = if input.version() >= version::ADD_PHOTO_PROGRESSIVE_SIZES {
        input.i32_vec()?
    } else {
        Vec::new()
    };
    Ok(PhotoSize {
        size_type,
        size,
        dimensions,
        file_id,
        progressive_sizes,
    })
}

/// `AnimationSize::store` (PhotoSize.hpp:28-33).
fn store_animation_size(
    animation_size: &AnimationSize,
    ctx: &mut dyn ContentStoreContext,
    out: &mut Vec<u8>,
) {
    store_photo_size(&animation_size.photo_size, ctx, out);
    store_f64(out, animation_size.main_frame_timestamp);
}

/// `AnimationSize::parse` (PhotoSize.hpp:35-44) — the stream start
/// rides `Version::AddDialogPhotoHasAnimation`.
fn parse_animation_size(
    ctx: &mut dyn ContentStoreContext,
    input: &mut Parser,
) -> Result<AnimationSize, &'static str> {
    let photo_size = parse_photo_size(ctx, input)?;
    let main_frame_timestamp = if input.version() >= version::ADD_DIALOG_PHOTO_HAS_ANIMATION {
        input.f64_()?
    } else {
        0.0
    };
    Ok(AnimationSize {
        photo_size,
        main_frame_timestamp,
    })
}

/// `StickerPhotoSize::store` (StickerPhotoSize.hpp:15-30).
fn store_sticker_photo_size(sticker_photo_size: &StickerPhotoSize, out: &mut Vec<u8>) {
    let is_custom_emoji = sticker_photo_size.get_type() == StickerPhotoSizeType::CustomEmoji;
    let is_sticker = sticker_photo_size.get_type() == StickerPhotoSizeType::Sticker;
    store_flags::<2>(out, [is_custom_emoji, is_sticker]);
    if is_custom_emoji {
        store_i64(out, sticker_photo_size.get_custom_emoji_id());
    } else if is_sticker {
        store_i64(out, sticker_photo_size.get_sticker_set_id());
        store_i64(out, sticker_photo_size.get_sticker_id());
    }
    store_i32_vec(out, sticker_photo_size.get_background_colors());
}

/// `StickerPhotoSize::parse` (StickerPhotoSize.hpp:32-...).
fn parse_sticker_photo_size(input: &mut Parser) -> Result<StickerPhotoSize, &'static str> {
    let [is_custom_emoji, is_sticker] = input.parse_flags::<2>()?;
    let mut sticker_photo_size = StickerPhotoSize::default();
    if is_custom_emoji {
        sticker_photo_size.type_ = StickerPhotoSizeType::CustomEmoji;
        sticker_photo_size.custom_emoji_id = input.i64_()?;
    } else if is_sticker {
        sticker_photo_size.type_ = StickerPhotoSizeType::Sticker;
        sticker_photo_size.sticker_set_id = input.i64_()?;
        sticker_photo_size.sticker_id = input.i64_()?;
    }
    sticker_photo_size.background_colors = input.i32_vec()?;
    Ok(sticker_photo_size)
}

/// `Photo::store` (Photo.hpp:19-44).
fn store_photo(photo: &Photo, ctx: &mut dyn ContentStoreContext, out: &mut Vec<u8>) {
    let has_minithumbnail = !photo.minithumbnail.is_empty();
    let has_animations = !photo.animations.is_empty();
    let has_sticker_photo_size = photo.sticker_photo_size.is_some();
    store_flags::<4>(
        out,
        [
            photo.has_stickers,
            has_minithumbnail,
            has_animations,
            has_sticker_photo_size,
        ],
    );
    store_i64(out, photo.id);
    store_i32(out, photo.date);
    store_i32(out, photo.photos.len() as i32);
    for photo_size in &photo.photos {
        store_photo_size(photo_size, ctx, out);
    }
    if photo.has_stickers {
        store_file_id_vec(&photo.sticker_file_ids, ctx, out);
    }
    if has_minithumbnail {
        store_string(out, &photo.minithumbnail);
    }
    if has_animations {
        store_i32(out, photo.animations.len() as i32);
        for animation_size in &photo.animations {
            store_animation_size(animation_size, ctx, out);
        }
    }
    if let Some(sticker_photo_size) = &photo.sticker_photo_size {
        store_sticker_photo_size(sticker_photo_size, out);
    }
}

/// `Photo::parse` (Photo.hpp:47-74).
fn parse_photo(
    ctx: &mut dyn ContentStoreContext,
    input: &mut Parser,
) -> Result<Photo, &'static str> {
    let [
        has_stickers,
        has_minithumbnail,
        has_animations,
        has_sticker_photo_size,
    ] = input.parse_flags::<4>()?;
    let id = input.i64_()?;
    let date = input.i32_()?;
    let photos_len = input.i32_()?;
    if photos_len < 0 {
        return Err("Wrong vector length");
    }
    let mut photos = Vec::with_capacity(photos_len as usize);
    for _ in 0..photos_len {
        photos.push(parse_photo_size(ctx, input)?);
    }
    let sticker_file_ids = if has_stickers {
        parse_i64_file_id_vec(ctx, input)?
    } else {
        Vec::new()
    };
    let minithumbnail = if has_minithumbnail {
        input.string()?
    } else {
        String::new()
    };
    let animations = if has_animations {
        let len = input.i32_()?;
        if len < 0 {
            return Err("Wrong vector length");
        }
        let mut sizes = Vec::with_capacity(len as usize);
        for _ in 0..len {
            sizes.push(parse_animation_size(ctx, input)?);
        }
        sizes
    } else {
        Vec::new()
    };
    let sticker_photo_size = if has_sticker_photo_size {
        Some(parse_sticker_photo_size(input)?)
    } else {
        None
    };
    Ok(Photo {
        id,
        date,
        minithumbnail,
        photos,
        animations,
        sticker_photo_size,
        has_stickers,
        sticker_file_ids,
    })
}

/// `td::store(vector<FileId>)` through the file seam — the
/// `Photo::sticker_file_ids` framing.
fn store_file_id_vec(file_ids: &[FileId], ctx: &mut dyn ContentStoreContext, out: &mut Vec<u8>) {
    store_i32(out, file_ids.len() as i32);
    for file_id in file_ids {
        ctx.store_file(*file_id, out);
    }
}

/// The [`store_file_id_vec`] reader.
fn parse_i64_file_id_vec(
    ctx: &mut dyn ContentStoreContext,
    input: &mut Parser,
) -> Result<Vec<FileId>, &'static str> {
    let len = input.i32_()?;
    if len < 0 {
        return Err("Wrong vector length");
    }
    let mut result = Vec::with_capacity(len as usize);
    for _ in 0..len {
        result.push(ctx.parse_file(input)?);
    }
    Ok(result)
}

/// `ForumTopicIcon::store` (ForumTopicIcon.hpp:8-19).
fn store_forum_topic_icon(icon: &ForumTopicIcon, out: &mut Vec<u8>) {
    let has_custom_emoji_id = icon.custom_emoji_id.is_valid();
    store_flags::<1>(out, [has_custom_emoji_id]);
    store_i32(out, icon.get_color());
    if has_custom_emoji_id {
        store_i64(out, icon.custom_emoji_id.get());
    }
}

/// `ForumTopicIcon::parse` (ForumTopicIcon.hpp:21-...).
fn parse_forum_topic_icon(input: &mut Parser) -> Result<ForumTopicIcon, &'static str> {
    let [has_custom_emoji_id] = input.parse_flags::<1>()?;
    let color = input.i32_()?;
    let custom_emoji_id = if has_custom_emoji_id {
        CustomEmojiId::new(input.i64_()?)
    } else {
        CustomEmojiId::default()
    };
    Ok(ForumTopicIcon::new(color, custom_emoji_id))
}

/// `ToDoItem::store` (ToDoItem.hpp:6-11).
fn store_to_do_item(item: &ToDoItem, out: &mut Vec<u8>) {
    store_flags::<0>(out, []);
    store_i32(out, item.id);
    item.title.store(out);
}

/// `ToDoItem::parse` (ToDoItem.hpp:13-...).
fn parse_to_do_item(input: &mut Parser) -> Result<ToDoItem, &'static str> {
    input.parse_flags::<0>()?;
    let id = input.i32_()?;
    let title = FormattedText::parse(input)?;
    Ok(ToDoItem { id, title })
}

/// `ToDoList::store` (ToDoList.hpp:6-13) — the flags head is cut off in
/// the read above; the C++ stores `others_can_append_`/`others_can_complete_`
/// then title and items.
fn store_to_do_list(list: &ToDoList, out: &mut Vec<u8>) {
    store_flags::<2>(out, [list.others_can_append, list.others_can_complete]);
    list.title.store(out);
    store_i32(out, list.items.len() as i32);
    for item in &list.items {
        store_to_do_item(item, out);
    }
}

/// `ToDoList::parse` (ToDoList.hpp:15-...).
fn parse_to_do_list(input: &mut Parser) -> Result<ToDoList, &'static str> {
    let [others_can_append, others_can_complete] = input.parse_flags::<2>()?;
    let title = FormattedText::parse(input)?;
    let len = input.i32_()?;
    if len < 0 {
        return Err("Wrong vector length");
    }
    let mut items = Vec::with_capacity(len as usize);
    for _ in 0..len {
        items.push(parse_to_do_item(input)?);
    }
    Ok(ToDoList {
        title,
        items,
        others_can_append,
        others_can_complete,
    })
}

/// `ToDoCompletion::store` (ToDoCompletion.hpp:6-20).
fn store_to_do_completion(completion: &ToDoCompletion, out: &mut Vec<u8>) {
    let is_completed_by_dialog = completion.completed_by_dialog_id.get_type() != DialogType::User;
    store_flags::<1>(out, [is_completed_by_dialog]);
    store_i32(out, completion.id);
    if is_completed_by_dialog {
        store_i64(out, completion.completed_by_dialog_id.get());
    } else {
        store_i64(out, completion.completed_by_dialog_id.get_user_id().get());
    }
    store_i32(out, completion.date);
}

/// `ToDoCompletion::parse` (ToDoCompletion.hpp:22-...).
fn parse_to_do_completion(input: &mut Parser) -> Result<ToDoCompletion, &'static str> {
    let [is_completed_by_dialog] = input.parse_flags::<1>()?;
    let id = input.i32_()?;
    let completed_by_dialog_id = if is_completed_by_dialog {
        DialogId::new(input.i64_()?)
    } else {
        DialogId::from_user_id(UserId::new(input.i64_()?))
    };
    let date = input.i32_()?;
    Ok(ToDoCompletion {
        id,
        completed_by_dialog_id,
        date,
    })
}

/// `GiveawayParameters::store` (GiveawayParameters.hpp:7-31).
fn store_giveaway_parameters(parameters: &GiveawayParameters, out: &mut Vec<u8>) {
    let has_additional_channel_ids = !parameters.additional_channel_ids.is_empty();
    let has_country_codes = !parameters.country_codes.is_empty();
    let has_prize_description = !parameters.prize_description.is_empty();
    store_flags::<5>(
        out,
        [
            parameters.only_new_subscribers,
            has_additional_channel_ids,
            has_country_codes,
            parameters.winners_are_visible,
            has_prize_description,
        ],
    );
    store_i64(out, parameters.boosted_channel_id.get());
    if has_additional_channel_ids {
        store_i32(out, parameters.additional_channel_ids.len() as i32);
        for channel_id in &parameters.additional_channel_ids {
            store_i64(out, channel_id.get());
        }
    }
    store_i32(out, parameters.date);
    if has_country_codes {
        store_string_vec(out, &parameters.country_codes);
    }
    if has_prize_description {
        store_string(out, &parameters.prize_description);
    }
}

/// `GiveawayParameters::parse` (GiveawayParameters.hpp:33-...).
fn parse_giveaway_parameters(input: &mut Parser) -> Result<GiveawayParameters, &'static str> {
    let [
        only_new_subscribers,
        has_additional_channel_ids,
        has_country_codes,
        winners_are_visible,
        has_prize_description,
    ] = input.parse_flags::<5>()?;
    let boosted_channel_id = ChannelId::new(input.i64_()?);
    let additional_channel_ids = if has_additional_channel_ids {
        let len = input.i32_()?;
        if len < 0 {
            return Err("Wrong vector length");
        }
        let mut result = Vec::with_capacity(len as usize);
        for _ in 0..len {
            result.push(ChannelId::new(input.i64_()?));
        }
        result
    } else {
        Vec::new()
    };
    let date = input.i32_()?;
    let country_codes = if has_country_codes {
        input.string_vec()?
    } else {
        Vec::new()
    };
    let prize_description = if has_prize_description {
        input.string()?
    } else {
        String::new()
    };
    Ok(GiveawayParameters {
        boosted_channel_id,
        additional_channel_ids,
        only_new_subscribers,
        winners_are_visible,
        date,
        country_codes,
        prize_description,
    })
}

/// `store(Address)` (OrderInfo.hpp:5-18).
fn store_address(address: &Address, out: &mut Vec<u8>) {
    store_string(out, &address.country_code);
    store_string(out, &address.state);
    store_string(out, &address.city);
    store_string(out, &address.street_line1);
    store_string(out, &address.street_line2);
    store_string(out, &address.postal_code);
}

/// `parse(Address)` (OrderInfo.hpp:20-31).
fn parse_address(input: &mut Parser) -> Result<Address, &'static str> {
    Ok(Address {
        country_code: input.string()?,
        state: input.string()?,
        city: input.string()?,
        street_line1: input.string()?,
        street_line2: input.string()?,
        postal_code: input.string()?,
    })
}

/// `store(OrderInfo)` (OrderInfo.hpp:33-55).
fn store_order_info(order_info: &OrderInfo, out: &mut Vec<u8>) {
    let has_name = !order_info.name.is_empty();
    let has_phone_number = !order_info.phone_number.is_empty();
    let has_email_address = !order_info.email_address.is_empty();
    let has_shipping_address = order_info.shipping_address.is_some();
    store_flags::<4>(
        out,
        [
            has_name,
            has_phone_number,
            has_email_address,
            has_shipping_address,
        ],
    );
    if has_name {
        store_string(out, &order_info.name);
    }
    if has_phone_number {
        store_string(out, &order_info.phone_number);
    }
    if has_email_address {
        store_string(out, &order_info.email_address);
    }
    if let Some(shipping_address) = &order_info.shipping_address {
        store_address(shipping_address, out);
    }
}

/// `parse(OrderInfo)` (OrderInfo.hpp:57-...).
fn parse_order_info(input: &mut Parser) -> Result<OrderInfo, &'static str> {
    let [
        has_name,
        has_phone_number,
        has_email_address,
        has_shipping_address,
    ] = input.parse_flags::<4>()?;
    let name = if has_name {
        input.string()?
    } else {
        String::new()
    };
    let phone_number = if has_phone_number {
        input.string()?
    } else {
        String::new()
    };
    let email_address = if has_email_address {
        input.string()?
    } else {
        String::new()
    };
    let shipping_address = if has_shipping_address {
        Some(Box::new(parse_address(input)?))
    } else {
        None
    };
    Ok(OrderInfo {
        name,
        phone_number,
        email_address,
        shipping_address,
    })
}

/// `CurrencyAmount::store` (CurrencyAmount.hpp:6-24).
fn store_currency_amount(amount: &CurrencyAmount, out: &mut Vec<u8>) {
    let has_star_amount = amount.star_amount != StarAmount::default();
    let has_ton_amount = amount.ton_amount != TonAmount::default();
    store_flags::<2>(out, [has_star_amount, has_ton_amount]);
    store_i32(out, amount.r#type as i32);
    if has_star_amount {
        store_i64(out, amount.star_amount.star_count);
        store_i32(out, amount.star_amount.nanostar_count);
    }
    if has_ton_amount {
        store_i64(out, amount.ton_amount.ton_amount);
    }
}

/// `CurrencyAmount::parse` (CurrencyAmount.hpp:26-...).
fn parse_currency_amount(input: &mut Parser) -> Result<CurrencyAmount, &'static str> {
    let [has_star_amount, has_ton_amount] = input.parse_flags::<2>()?;
    let type_raw = input.i32_()?;
    let r#type = match type_raw {
        0 => CurrencyAmountType::None,
        1 => CurrencyAmountType::Star,
        2 => CurrencyAmountType::Ton,
        _ => CurrencyAmountType::default(),
    };
    let star_amount = if has_star_amount {
        StarAmount {
            star_count: input.i64_()?,
            nanostar_count: input.i32_()?,
        }
    } else {
        StarAmount::default()
    };
    let ton_amount = if has_ton_amount {
        TonAmount {
            ton_amount: input.i64_()?,
        }
    } else {
        TonAmount::default()
    };
    Ok(CurrencyAmount {
        r#type,
        star_amount,
        ton_amount,
    })
}

/// `SharedDialog::store` (SharedDialog.hpp:6-28).
fn store_shared_dialog(
    shared_dialog: &SharedDialog,
    ctx: &mut dyn ContentStoreContext,
    out: &mut Vec<u8>,
) {
    let has_first_name = !shared_dialog.first_name.is_empty();
    let has_last_name = !shared_dialog.last_name.is_empty();
    let has_username = !shared_dialog.username.is_empty();
    let has_photo = !shared_dialog.photo.is_empty();
    store_flags::<4>(
        out,
        [has_first_name, has_last_name, has_username, has_photo],
    );
    store_i64(out, shared_dialog.dialog_id.get());
    if has_first_name {
        store_string(out, &shared_dialog.first_name);
    }
    if has_last_name {
        store_string(out, &shared_dialog.last_name);
    }
    if has_username {
        store_string(out, &shared_dialog.username);
    }
    if has_photo {
        store_photo(&shared_dialog.photo, ctx, out);
    }
}

/// `SharedDialog::parse` (SharedDialog.hpp:30-...).
fn parse_shared_dialog(
    ctx: &mut dyn ContentStoreContext,
    input: &mut Parser,
) -> Result<SharedDialog, &'static str> {
    let [has_first_name, has_last_name, has_username, has_photo] = input.parse_flags::<4>()?;
    let dialog_id = DialogId::new(input.i64_()?);
    let first_name = if has_first_name {
        input.string()?
    } else {
        String::new()
    };
    let last_name = if has_last_name {
        input.string()?
    } else {
        String::new()
    };
    let username = if has_username {
        input.string()?
    } else {
        String::new()
    };
    let photo = if has_photo {
        parse_photo(ctx, input)?
    } else {
        Photo::default()
    };
    Ok(SharedDialog {
        dialog_id,
        first_name,
        last_name,
        username,
        photo,
    })
}

/// `ForumTopicEditedData::store` (ForumTopicEditedData.hpp:7-24).
fn store_forum_topic_edited_data(edited_data: &ForumTopicEditedData, out: &mut Vec<u8>) {
    let has_title = !edited_data.title.is_empty();
    let has_icon_custom_emoji_id = edited_data.icon_custom_emoji_id.is_valid();
    store_flags::<6>(
        out,
        [
            edited_data.edit_icon_custom_emoji_id,
            edited_data.edit_is_closed,
            edited_data.is_closed,
            has_title,
            has_icon_custom_emoji_id,
            edited_data.is_hidden,
        ],
    );
    if has_title {
        store_string(out, &edited_data.title);
    }
    if has_icon_custom_emoji_id {
        store_i64(out, edited_data.icon_custom_emoji_id.get());
    }
}

/// `ForumTopicEditedData::parse` (ForumTopicEditedData.hpp:26-...).
fn parse_forum_topic_edited_data(input: &mut Parser) -> Result<ForumTopicEditedData, &'static str> {
    let [
        edit_icon_custom_emoji_id,
        edit_is_closed,
        is_closed,
        has_title,
        has_icon_custom_emoji_id,
        is_hidden,
    ] = input.parse_flags::<6>()?;
    let title = if has_title {
        input.string()?
    } else {
        String::new()
    };
    let icon_custom_emoji_id = if has_icon_custom_emoji_id {
        CustomEmojiId::new(input.i64_()?)
    } else {
        CustomEmojiId::default()
    };
    Ok(ForumTopicEditedData {
        title,
        icon_custom_emoji_id,
        edit_icon_custom_emoji_id,
        edit_is_closed,
        is_closed,
        edit_is_hidden: false,
        is_hidden,
    })
}

/// `WebApp::store` (WebApp.hpp:10-36).
fn store_web_app(web_app: &WebApp, ctx: &mut dyn ContentStoreContext, out: &mut Vec<u8>) {
    let has_animation = web_app.animation_file_id.is_valid();
    store_flags::<1>(out, [has_animation]);
    store_i64(out, web_app.id);
    store_i64(out, web_app.access_hash);
    store_string(out, &web_app.short_name);
    store_string(out, &web_app.title);
    store_string(out, &web_app.description);
    store_photo(&web_app.photo, ctx, out);
    if has_animation {
        ctx.store_animation(web_app.animation_file_id, out);
    }
    store_i64(out, web_app.hash);
}

/// `WebApp::parse` (WebApp.hpp:38-...).
fn parse_web_app(
    ctx: &mut dyn ContentStoreContext,
    input: &mut Parser,
) -> Result<WebApp, &'static str> {
    let [has_animation] = input.parse_flags::<1>()?;
    let id = input.i64_()?;
    let access_hash = input.i64_()?;
    let short_name = input.string()?;
    let title = input.string()?;
    let description = input.string()?;
    let photo = parse_photo(ctx, input)?;
    let animation_file_id = if has_animation {
        ctx.parse_animation(input)?
    } else {
        FileId::default()
    };
    let hash = input.i64_()?;
    Ok(WebApp {
        id,
        access_hash,
        short_name,
        title,
        description,
        photo,
        animation_file_id,
        hash,
    })
}

/// `Game::store` (Game.hpp:8-27).
fn store_game(game: &Game, ctx: &mut dyn ContentStoreContext, out: &mut Vec<u8>) {
    let has_animation = game.animation_file_id.is_valid();
    store_flags::<1>(out, [has_animation]);
    store_i64(out, game.id);
    store_i64(out, game.access_hash);
    store_i64(out, game.bot_user_id.get());
    store_string(out, &game.short_name);
    store_string(out, &game.title);
    store_string(out, &game.description);
    store_photo(&game.photo, ctx, out);
    if has_animation {
        ctx.store_animation(game.animation_file_id, out);
    }
    game.text.store(out);
}

/// `Game::parse` (Game.hpp:29-...) — the animation flag rides
/// `Version::FixStoreGameWithoutAnimation`.
fn parse_game(ctx: &mut dyn ContentStoreContext, input: &mut Parser) -> Result<Game, &'static str> {
    let has_animation = if input.version() >= version::FIX_STORE_GAME_WITHOUT_ANIMATION {
        input.parse_flags::<1>()?[0]
    } else {
        true
    };
    let id = input.i64_()?;
    let access_hash = input.i64_()?;
    let bot_user_id = UserId::new(input.i64_()?);
    let short_name = input.string()?;
    let title = input.string()?;
    let description = input.string()?;
    let photo = parse_photo(ctx, input)?;
    let animation_file_id = if has_animation {
        ctx.parse_animation(input)?
    } else {
        FileId::default()
    };
    let text = FormattedText::parse(input)?;
    Ok(Game {
        id,
        access_hash,
        bot_user_id,
        short_name,
        title,
        description,
        photo,
        animation_file_id,
        text,
    })
}

/// `InputInvoice::Invoice::store` (InputInvoice.hpp:8-38).
fn store_invoice(invoice: &Invoice, out: &mut Vec<u8>) {
    let has_tip = invoice.max_tip_amount != 0;
    let is_recurring = !invoice.recurring_payment_terms_of_service_url.is_empty();
    let has_terms_of_service = !invoice.terms_of_service_url.is_empty();
    let has_subscription_period = invoice.subscription_period != 0;
    store_flags::<12>(
        out,
        [
            invoice.is_test,
            invoice.need_name,
            invoice.need_phone_number,
            invoice.need_email_address,
            invoice.need_shipping_address,
            invoice.is_flexible,
            invoice.send_phone_number_to_provider,
            invoice.send_email_address_to_provider,
            has_tip,
            is_recurring,
            has_terms_of_service,
            has_subscription_period,
        ],
    );
    store_string(out, &invoice.currency);
    store_i32(out, invoice.price_parts.len() as i32);
    for part in &invoice.price_parts {
        store_string(out, &part.label);
        store_i64(out, part.amount);
    }
    if has_tip {
        store_i64(out, invoice.max_tip_amount);
        store_i64_vec(out, &invoice.suggested_tip_amounts);
    }
    if is_recurring {
        store_string(out, &invoice.recurring_payment_terms_of_service_url);
    }
    if has_terms_of_service {
        store_string(out, &invoice.terms_of_service_url);
    }
    if has_subscription_period {
        store_i32(out, invoice.subscription_period);
    }
}

/// `InputInvoice::Invoice::parse` (InputInvoice.hpp:40-...).
fn parse_invoice(input: &mut Parser) -> Result<Invoice, &'static str> {
    let [
        is_test,
        need_name,
        need_phone_number,
        need_email_address,
        need_shipping_address,
        is_flexible,
        send_phone_number_to_provider,
        send_email_address_to_provider,
        has_tip,
        is_recurring,
        has_terms_of_service,
        has_subscription_period,
    ] = input.parse_flags::<12>()?;
    let currency = input.string()?;
    let price_parts_len = input.i32_()?;
    if price_parts_len < 0 {
        return Err("Wrong vector length");
    }
    let mut price_parts = Vec::with_capacity(price_parts_len as usize);
    for _ in 0..price_parts_len {
        let label = input.string()?;
        let amount = input.i64_()?;
        price_parts.push(LabeledPricePart { label, amount });
    }
    let max_tip_amount = if has_tip { input.i64_()? } else { 0 };
    let suggested_tip_amounts = if has_tip {
        parse_i64_vec(input)?
    } else {
        Vec::new()
    };
    let recurring_payment_terms_of_service_url = if is_recurring {
        input.string()?
    } else {
        String::new()
    };
    let terms_of_service_url = if has_terms_of_service {
        input.string()?
    } else {
        String::new()
    };
    let subscription_period = if has_subscription_period {
        input.i32_()?
    } else {
        0
    };
    Ok(Invoice {
        currency,
        price_parts,
        subscription_period,
        max_tip_amount,
        suggested_tip_amounts,
        recurring_payment_terms_of_service_url,
        terms_of_service_url,
        is_test,
        need_name,
        need_phone_number,
        need_email_address,
        need_shipping_address,
        send_phone_number_to_provider,
        send_email_address_to_provider,
        is_flexible,
    })
}

/// `InputInvoice::store` (InputInvoice.hpp:65-115).
fn store_input_invoice(
    input_invoice: &InputInvoice,
    ctx: &mut dyn ContentStoreContext,
    out: &mut Vec<u8>,
) {
    let has_description = !input_invoice.description.is_empty();
    let has_photo = !input_invoice.photo.is_empty();
    let has_start_parameter = !input_invoice.start_parameter.is_empty();
    let has_payload = !input_invoice.payload.is_empty();
    let has_provider_token = !input_invoice.provider_token.is_empty();
    let has_provider_data = !input_invoice.provider_data.is_empty();
    let has_total_amount = input_invoice.total_amount != 0;
    let has_receipt_message_id = input_invoice.receipt_message_id.is_valid();
    let has_extended_media = !input_invoice.extended_media.is_empty();
    let has_extended_media_caption = !input_invoice.extended_media_caption.text.is_empty();
    store_flags::<10>(
        out,
        [
            has_description,
            has_photo,
            has_start_parameter,
            has_payload,
            has_provider_token,
            has_provider_data,
            has_total_amount,
            has_receipt_message_id,
            has_extended_media,
            has_extended_media_caption,
        ],
    );
    store_string(out, &input_invoice.title);
    if has_description {
        store_string(out, &input_invoice.description);
    }
    if has_photo {
        store_photo(&input_invoice.photo, ctx, out);
    }
    if has_start_parameter {
        store_string(out, &input_invoice.start_parameter);
    }
    store_invoice(&input_invoice.invoice, out);
    if has_payload {
        store_string(out, &input_invoice.payload);
    }
    if has_provider_token {
        store_string(out, &input_invoice.provider_token);
    }
    if has_provider_data {
        store_string(out, &input_invoice.provider_data);
    }
    if has_total_amount {
        store_i64(out, input_invoice.total_amount);
    }
    if has_receipt_message_id {
        store_i32(out, input_invoice.receipt_message_id.get() as i32);
    }
    if has_extended_media {
        store_message_extended_media(&input_invoice.extended_media, ctx, out);
    }
    if has_extended_media_caption {
        input_invoice.extended_media_caption.store(out);
    }
}

/// `InputInvoice::parse` (InputInvoice.hpp:117-...) — the flags block
/// rides `Version::AddInputInvoiceFlags`; pre-flags records fall back to
/// the positional fields, with provider data gated on
/// `Version::AddMessageInvoiceProviderData`.
fn parse_input_invoice(
    ctx: &mut dyn ContentStoreContext,
    input: &mut Parser,
) -> Result<InputInvoice, &'static str> {
    let (
        has_description,
        has_photo,
        has_start_parameter,
        has_payload,
        has_provider_token,
        has_provider_data,
        has_total_amount,
        has_receipt_message_id,
        has_extended_media,
        has_extended_media_caption,
    ) = if input.version() >= version::ADD_INPUT_INVOICE_FLAGS {
        let flags = input.parse_flags::<10>()?;
        (
            flags[0], flags[1], flags[2], flags[3], flags[4], flags[5], flags[6], flags[7],
            flags[8], flags[9],
        )
    } else {
        (
            true,
            true,
            true,
            true,
            true,
            input.version() >= version::ADD_MESSAGE_INVOICE_PROVIDER_DATA,
            true,
            true,
            false,
            false,
        )
    };
    let title = input.string()?;
    let description = if has_description {
        input.string()?
    } else {
        String::new()
    };
    let photo = if has_photo {
        parse_photo(ctx, input)?
    } else {
        Photo::default()
    };
    let start_parameter = if has_start_parameter {
        input.string()?
    } else {
        String::new()
    };
    let invoice = parse_invoice(input)?;
    let payload = if has_payload {
        input.string()?
    } else {
        String::new()
    };
    let provider_token = if has_provider_token {
        input.string()?
    } else {
        String::new()
    };
    let provider_data = if has_provider_data {
        input.string()?
    } else {
        String::new()
    };
    let total_amount = if has_total_amount { input.i64_()? } else { 0 };
    let receipt_message_id = if has_receipt_message_id {
        parse_message_id(input)?
    } else {
        MessageId::default()
    };
    let extended_media = if has_extended_media {
        parse_message_extended_media(ctx, input)?
    } else {
        MessageExtendedMedia::default()
    };
    let extended_media_caption = if has_extended_media_caption {
        FormattedText::parse(input)?
    } else {
        FormattedText::default()
    };
    Ok(InputInvoice {
        title,
        description,
        photo,
        start_parameter,
        invoice,
        payload,
        provider_token,
        provider_data,
        extended_media,
        extended_media_caption,
        total_amount,
        receipt_message_id,
    })
}

/// `MessageExtendedMedia::store` (MessageExtendedMedia.hpp:14-56).
fn store_message_extended_media(
    extended_media: &MessageExtendedMedia,
    ctx: &mut dyn ContentStoreContext,
    out: &mut Vec<u8>,
) {
    let has_unsupported_version = extended_media.unsupported_version != 0;
    let has_duration = extended_media.duration != 0;
    let has_dimensions =
        extended_media.dimensions.width != 0 || extended_media.dimensions.height != 0;
    let has_minithumbnail = !extended_media.minithumbnail.is_empty();
    let has_photo = !extended_media.photo.is_empty();
    let has_video = extended_media.video_file_id.is_valid();
    let has_start_timestamp = extended_media.start_timestamp != 0;
    // STORE_FLAG(false); // has_caption — the legacy caption slot.
    store_flags::<8>(
        out,
        [
            false,
            has_unsupported_version,
            has_duration,
            has_dimensions,
            has_minithumbnail,
            has_photo,
            has_video,
            has_start_timestamp,
        ],
    );
    store_i32(out, extended_media.r#type as i32);
    if has_unsupported_version {
        store_i32(out, extended_media.unsupported_version);
    }
    if has_duration {
        store_i32(out, extended_media.duration);
    }
    if has_dimensions {
        store_dimensions(&extended_media.dimensions, out);
    }
    if has_minithumbnail {
        store_string(out, &extended_media.minithumbnail);
    }
    if has_photo {
        store_photo(&extended_media.photo, ctx, out);
    }
    if has_video {
        ctx.store_video(extended_media.video_file_id, out);
    }
    if has_start_timestamp {
        store_i32(out, extended_media.start_timestamp);
    }
}

/// `MessageExtendedMedia::parse` (MessageExtendedMedia.hpp:58-...) — the
/// legacy caption slot is parsed and dropped, and a Photo-typed record
/// without a photo degrades to Unsupported by the caller's `is_bad`
/// check (the C++ sets `type_ = Type::Unsupported` there).
fn parse_message_extended_media(
    ctx: &mut dyn ContentStoreContext,
    input: &mut Parser,
) -> Result<MessageExtendedMedia, &'static str> {
    let [
        has_caption,
        has_unsupported_version,
        has_duration,
        has_dimensions,
        has_minithumbnail,
        has_photo,
        has_video,
        has_start_timestamp,
    ] = input.parse_flags::<8>()?;
    let type_raw = input.i32_()?;
    let r#type = message_extended_media_type_from_raw(type_raw);
    if has_caption {
        FormattedText::parse(input)?;
    }
    let unsupported_version = if has_unsupported_version {
        input.i32_()?
    } else {
        0
    };
    let duration = if has_duration { input.i32_()? } else { 0 };
    let dimensions = if has_dimensions {
        parse_dimensions(input)?
    } else {
        Dimensions::default()
    };
    let minithumbnail = if has_minithumbnail {
        input.string()?
    } else {
        String::new()
    };
    let photo = if has_photo {
        parse_photo(ctx, input)?
    } else {
        Photo::default()
    };
    let video_file_id = if has_video {
        ctx.parse_video(input)?
    } else {
        FileId::default()
    };
    let start_timestamp = if has_start_timestamp {
        input.i32_()?
    } else {
        0
    };
    let mut result = MessageExtendedMedia {
        r#type,
        unsupported_version,
        duration,
        dimensions,
        minithumbnail,
        photo,
        video_file_id,
        start_timestamp,
    };
    if !has_photo && r#type == MessageExtendedMediaType::Photo {
        result.r#type = MessageExtendedMediaType::Unsupported;
    }
    Ok(result)
}

/// `DatedFile::store` (SecureValue.hpp:6-11).
fn store_dated_file(file: &DatedFile, ctx: &mut dyn ContentStoreContext, out: &mut Vec<u8>) {
    ctx.store_file(file.file_id, out);
    store_i32(out, file.date);
}

/// `DatedFile::parse` (SecureValue.hpp:13-17).
fn parse_dated_file(
    ctx: &mut dyn ContentStoreContext,
    input: &mut Parser,
) -> Result<DatedFile, &'static str> {
    let file_id = ctx.parse_file(input)?;
    let date = input.i32_()?;
    Ok(DatedFile { file_id, date })
}

/// `EncryptedSecureFile::store` (SecureValue.hpp:19-25).
fn store_encrypted_secure_file(
    file: &EncryptedSecureFile,
    ctx: &mut dyn ContentStoreContext,
    out: &mut Vec<u8>,
) {
    store_dated_file(&file.file, ctx, out);
    store_string(out, &file.file_hash);
    store_string(out, &file.encrypted_secret);
}

/// `EncryptedSecureFile::parse` (SecureValue.hpp:27-...).
fn parse_encrypted_secure_file(
    ctx: &mut dyn ContentStoreContext,
    input: &mut Parser,
) -> Result<EncryptedSecureFile, &'static str> {
    let file = parse_dated_file(ctx, input)?;
    let file_hash = input.string()?;
    let encrypted_secret = input.string()?;
    Ok(EncryptedSecureFile {
        file,
        file_hash,
        encrypted_secret,
    })
}

/// `EncryptedSecureData::store` (SecureValue.hpp:29-35).
fn store_encrypted_secure_data(data: &EncryptedSecureData, out: &mut Vec<u8>) {
    store_string(out, &data.data);
    store_string(out, &data.hash);
    store_string(out, &data.encrypted_secret);
}

/// `EncryptedSecureData::parse` (SecureValue.hpp:37-...).
fn parse_encrypted_secure_data(input: &mut Parser) -> Result<EncryptedSecureData, &'static str> {
    Ok(EncryptedSecureData {
        data: input.string()?,
        hash: input.string()?,
        encrypted_secret: input.string()?,
    })
}

/// `EncryptedSecureCredentials::store` (SecureValue.hpp:43-49).
fn store_encrypted_secure_credentials(credentials: &EncryptedSecureCredentials, out: &mut Vec<u8>) {
    store_string(out, &credentials.data);
    store_string(out, &credentials.hash);
    store_string(out, &credentials.encrypted_secret);
}

/// `EncryptedSecureCredentials::parse` (SecureValue.hpp:51-...).
fn parse_encrypted_secure_credentials(
    input: &mut Parser,
) -> Result<EncryptedSecureCredentials, &'static str> {
    Ok(EncryptedSecureCredentials {
        data: input.string()?,
        hash: input.string()?,
        encrypted_secret: input.string()?,
    })
}

/// `EncryptedSecureValue::store` (SecureValue.hpp:55-94).
fn store_encrypted_secure_value(
    value: &EncryptedSecureValue,
    ctx: &mut dyn ContentStoreContext,
    out: &mut Vec<u8>,
) {
    let has_data_hash = !value.data.hash.is_empty();
    let has_files = !value.files.is_empty();
    let has_front_side = value.front_side.file.file_id.is_valid();
    let has_reverse_side = value.reverse_side.file.file_id.is_valid();
    let has_selfie = value.selfie.file.file_id.is_valid();
    let has_hash = !value.hash.is_empty();
    let has_translations = !value.translations.is_empty();
    store_flags::<7>(
        out,
        [
            has_data_hash,
            has_files,
            has_front_side,
            has_reverse_side,
            has_selfie,
            has_hash,
            has_translations,
        ],
    );
    store_i32(out, value.r#type as i32);
    if has_data_hash {
        store_encrypted_secure_data(&value.data, out);
    } else {
        store_string(out, &value.data.data);
    }
    if has_files {
        store_i32(out, value.files.len() as i32);
        for file in &value.files {
            store_encrypted_secure_file(file, ctx, out);
        }
    }
    if has_front_side {
        store_encrypted_secure_file(&value.front_side, ctx, out);
    }
    if has_reverse_side {
        store_encrypted_secure_file(&value.reverse_side, ctx, out);
    }
    if has_selfie {
        store_encrypted_secure_file(&value.selfie, ctx, out);
    }
    if has_hash {
        store_string(out, &value.hash);
    }
    if has_translations {
        store_i32(out, value.translations.len() as i32);
        for file in &value.translations {
            store_encrypted_secure_file(file, ctx, out);
        }
    }
}

/// `EncryptedSecureValue::parse` (SecureValue.hpp:96-...).
fn parse_encrypted_secure_value(
    ctx: &mut dyn ContentStoreContext,
    input: &mut Parser,
) -> Result<EncryptedSecureValue, &'static str> {
    let [
        has_data_hash,
        has_files,
        has_front_side,
        has_reverse_side,
        has_selfie,
        has_hash,
        has_translations,
    ] = input.parse_flags::<7>()?;
    let type_raw = input.i32_()?;
    let r#type = secure_value_type_from_raw(type_raw);
    let data = if has_data_hash {
        parse_encrypted_secure_data(input)?
    } else {
        EncryptedSecureData {
            data: input.string()?,
            hash: String::new(),
            encrypted_secret: String::new(),
        }
    };
    let files = if has_files {
        let len = input.i32_()?;
        if len < 0 {
            return Err("Wrong vector length");
        }
        let mut result = Vec::with_capacity(len as usize);
        for _ in 0..len {
            result.push(parse_encrypted_secure_file(ctx, input)?);
        }
        result
    } else {
        Vec::new()
    };
    let front_side = if has_front_side {
        parse_encrypted_secure_file(ctx, input)?
    } else {
        EncryptedSecureFile::default()
    };
    let reverse_side = if has_reverse_side {
        parse_encrypted_secure_file(ctx, input)?
    } else {
        EncryptedSecureFile::default()
    };
    let selfie = if has_selfie {
        parse_encrypted_secure_file(ctx, input)?
    } else {
        EncryptedSecureFile::default()
    };
    let hash = if has_hash {
        input.string()?
    } else {
        String::new()
    };
    let translations = if has_translations {
        let len = input.i32_()?;
        if len < 0 {
            return Err("Wrong vector length");
        }
        let mut result = Vec::with_capacity(len as usize);
        for _ in 0..len {
            result.push(parse_encrypted_secure_file(ctx, input)?);
        }
        result
    } else {
        Vec::new()
    };
    Ok(EncryptedSecureValue {
        r#type,
        data,
        files,
        front_side,
        reverse_side,
        selfie,
        translations,
        hash,
    })
}

/// `parse_caption` (MessageContent.cpp:3031-3047) — entities ride
/// `Version::AddCaptionEntities`; older records re-run the entity search
/// over the raw text (with the utf8 sanity check).
fn parse_caption(input: &mut Parser) -> Result<FormattedText, &'static str> {
    if input.version() >= version::ADD_CAPTION_ENTITIES {
        let text = input.string()?;
        let len = input.i32_()?;
        if len < 0 {
            return Err("Wrong vector length");
        }
        let mut entities = Vec::with_capacity(len as usize);
        for _ in 0..len {
            entities.push(MessageEntity::parse(input)?);
        }
        remove_empty_entities(&mut entities);
        Ok(FormattedText { text, entities })
    } else {
        let mut text = input.string()?;
        if !tdutils::utf8::check_utf8(text.as_bytes()) {
            text.clear();
        }
        let entities = find_entities(&text, false, true);
        Ok(FormattedText { text, entities })
    }
}

/// The `MessageContentType` value reader for [`parse_message_content`] —
/// the inverse of the `as i32` the store writes. `None`/out-of-range
/// values return `None` and degrade to `MessageUnsupported`, matching
/// the C++ `default: is_bad = true` path.
fn content_type_from_raw(raw: i32) -> Option<MessageContentType> {
    let content_type = match raw {
        0 => MessageContentType::Text,
        1 => MessageContentType::Animation,
        2 => MessageContentType::Audio,
        3 => MessageContentType::Document,
        4 => MessageContentType::Photo,
        5 => MessageContentType::Sticker,
        6 => MessageContentType::Video,
        7 => MessageContentType::VoiceNote,
        8 => MessageContentType::Contact,
        9 => MessageContentType::Location,
        10 => MessageContentType::Venue,
        11 => MessageContentType::ChatCreate,
        12 => MessageContentType::ChatChangeTitle,
        13 => MessageContentType::ChatChangePhoto,
        14 => MessageContentType::ChatDeletePhoto,
        15 => MessageContentType::ChatDeleteHistory,
        16 => MessageContentType::ChatAddUsers,
        17 => MessageContentType::ChatJoinedByLink,
        18 => MessageContentType::ChatDeleteUser,
        19 => MessageContentType::ChatMigrateTo,
        20 => MessageContentType::ChannelCreate,
        21 => MessageContentType::ChannelMigrateFrom,
        22 => MessageContentType::PinMessage,
        23 => MessageContentType::Game,
        24 => MessageContentType::GameScore,
        25 => MessageContentType::ScreenshotTaken,
        26 => MessageContentType::ChatSetTtl,
        27 => MessageContentType::Unsupported,
        28 => MessageContentType::Call,
        29 => MessageContentType::Invoice,
        30 => MessageContentType::PaymentSuccessful,
        31 => MessageContentType::VideoNote,
        32 => MessageContentType::ContactRegistered,
        33 => MessageContentType::ExpiredPhoto,
        34 => MessageContentType::ExpiredVideo,
        35 => MessageContentType::LiveLocation,
        36 => MessageContentType::CustomServiceAction,
        37 => MessageContentType::WebsiteConnected,
        38 => MessageContentType::PassportDataSent,
        39 => MessageContentType::PassportDataReceived,
        40 => MessageContentType::Poll,
        41 => MessageContentType::Dice,
        42 => MessageContentType::ProximityAlertTriggered,
        43 => MessageContentType::GroupCall,
        44 => MessageContentType::InviteToGroupCall,
        45 => MessageContentType::ChatSetTheme,
        46 => MessageContentType::WebViewDataSent,
        47 => MessageContentType::WebViewDataReceived,
        48 => MessageContentType::GiftPremium,
        49 => MessageContentType::TopicCreate,
        50 => MessageContentType::TopicEdit,
        51 => MessageContentType::SuggestProfilePhoto,
        52 => MessageContentType::WriteAccessAllowed,
        53 => MessageContentType::RequestedDialog,
        54 => MessageContentType::WebViewWriteAccessAllowed,
        55 => MessageContentType::SetBackground,
        56 => MessageContentType::Story,
        57 => MessageContentType::WriteAccessAllowedByRequest,
        58 => MessageContentType::GiftCode,
        59 => MessageContentType::Giveaway,
        60 => MessageContentType::GiveawayLaunch,
        61 => MessageContentType::GiveawayResults,
        62 => MessageContentType::GiveawayWinners,
        63 => MessageContentType::ExpiredVideoNote,
        64 => MessageContentType::ExpiredVoiceNote,
        65 => MessageContentType::BoostApply,
        66 => MessageContentType::DialogShared,
        67 => MessageContentType::PaidMedia,
        68 => MessageContentType::PaymentRefunded,
        69 => MessageContentType::GiftStars,
        70 => MessageContentType::PrizeStars,
        71 => MessageContentType::StarGift,
        72 => MessageContentType::StarGiftUnique,
        73 => MessageContentType::PaidMessagesRefunded,
        74 => MessageContentType::PaidMessagesPrice,
        75 => MessageContentType::ConferenceCall,
        76 => MessageContentType::ToDoList,
        77 => MessageContentType::TodoCompletions,
        78 => MessageContentType::TodoAppendTasks,
        79 => MessageContentType::GiftTon,
        80 => MessageContentType::SuggestedPostSuccess,
        81 => MessageContentType::SuggestedPostRefund,
        82 => MessageContentType::SuggestedPostApproval,
        83 => MessageContentType::SuggestBirthday,
        84 => MessageContentType::StarGiftPurchaseOffer,
        85 => MessageContentType::StarGiftPurchaseOfferDeclined,
        86 => MessageContentType::NewCreatorPending,
        87 => MessageContentType::ChangeCreator,
        88 => MessageContentType::NoForwardsToggle,
        89 => MessageContentType::NoForwardsRequest,
        90 => MessageContentType::ManagedBotCreated,
        91 => MessageContentType::PollAppendAnswer,
        92 => MessageContentType::PollDeleteAnswer,
        93 => MessageContentType::RichText,
        94 => MessageContentType::ChangeCommunity,
        95 => MessageContentType::ChatJoinedViaCommunity,
        _ => return None,
    };
    Some(content_type)
}

/// `store(const MessageContent *content, StorerT &storer)`
/// (MessageContent.cpp:1830-3035).
///
/// The seven deferred payload types (Contact, ChatSetTheme, the StarGift
/// family, RichText) have no [`MessageContent`] variant on this line
/// (module-doc deferrals), so they cannot be constructed here; their
/// parse side degrades to `MessageUnsupported` exactly like the C++
/// `is_bad` fallback.
pub fn store_message_content(
    ctx: &mut dyn ContentStoreContext,
    content: &MessageContent,
    out: &mut Vec<u8>,
) {
    store_i32(out, content.get_type() as i32);
    match content {
        MessageContent::Animation(m) => {
            ctx.store_animation(m.file_id, out);
            store_flags::<1>(out, [m.has_spoiler]);
            m.caption.store(out);
        }
        MessageContent::Audio(m) => {
            ctx.store_audio(m.file_id, out);
            m.caption.store(out);
            store_bool(out, true);
        }
        MessageContent::Document(m) => {
            ctx.store_document(m.file_id, out);
            m.caption.store(out);
        }
        MessageContent::Game(m) => {
            store_game(&m.game, ctx, out);
        }
        MessageContent::Invoice(m) => {
            store_input_invoice(&m.input_invoice, ctx, out);
        }
        MessageContent::LiveLocation(m) => {
            store_location(&m.location, out);
            store_i32(out, m.period);
            store_i32(out, m.heading);
            store_i32(out, m.proximity_alert_radius);
        }
        MessageContent::Location(m) => {
            store_location(&m.location, out);
        }
        MessageContent::Photo(m) => {
            let has_video_file_id = m.video_file_id.is_valid();
            store_photo(&m.photo, ctx, out);
            store_flags::<2>(out, [m.has_spoiler, has_video_file_id]);
            m.caption.store(out);
            if has_video_file_id {
                ctx.store_video(m.video_file_id, out);
            }
        }
        MessageContent::Sticker(m) => {
            ctx.store_sticker(m.file_id, false, out);
            store_flags::<1>(out, [m.is_premium]);
        }
        MessageContent::Text(m) => {
            let has_web_page_id = m.web_page_id.is_valid();
            let has_web_page_url = !m.web_page_url.is_empty();
            store_flags::<5>(
                out,
                [
                    has_web_page_id,
                    m.force_small_media,
                    m.force_large_media,
                    has_web_page_url,
                    m.skip_web_page_confirmation,
                ],
            );
            m.text.store(out);
            if has_web_page_id {
                store_i64(out, m.web_page_id.get());
            }
            if has_web_page_url {
                store_string(out, &m.web_page_url);
            }
        }
        MessageContent::Unsupported(m) => {
            store_i32(out, m.version);
        }
        MessageContent::Venue(m) => {
            store_venue(&m.venue, out);
        }
        MessageContent::Video(m) => {
            ctx.store_video(m.file_id, out);
            let has_alternative_videos = !m.alternative_file_ids.is_empty();
            let has_hls_files = !m.hls_file_ids.is_empty();
            let has_cover = !m.cover.is_empty();
            let has_start_timestamp = m.start_timestamp != 0;
            let has_storyboard_files = !m.storyboard_file_ids.is_empty();
            let has_storyboard_map_files = !m.storyboard_map_file_ids.is_empty();
            store_flags::<7>(
                out,
                [
                    m.has_spoiler,
                    has_alternative_videos,
                    has_hls_files,
                    has_cover,
                    has_start_timestamp,
                    has_storyboard_files,
                    has_storyboard_map_files,
                ],
            );
            m.caption.store(out);
            if has_alternative_videos {
                store_i32(out, m.alternative_file_ids.len() as i32);
                for file_id in &m.alternative_file_ids {
                    ctx.store_video(*file_id, out);
                }
            }
            if has_hls_files {
                store_i32(out, m.hls_file_ids.len() as i32);
                for file_id in &m.hls_file_ids {
                    ctx.store_document(*file_id, out);
                }
            }
            if has_cover {
                store_photo(&m.cover, ctx, out);
            }
            if has_start_timestamp {
                store_i32(out, m.start_timestamp);
            }
            if has_storyboard_files {
                store_i32(out, m.storyboard_file_ids.len() as i32);
                for file_id in &m.storyboard_file_ids {
                    ctx.store_document(*file_id, out);
                }
            }
            if has_storyboard_map_files {
                store_i32(out, m.storyboard_map_file_ids.len() as i32);
                for file_id in &m.storyboard_map_file_ids {
                    ctx.store_document(*file_id, out);
                }
            }
        }
        MessageContent::VideoNote(m) => {
            ctx.store_video_note(m.file_id, out);
            store_bool(out, m.is_viewed);
        }
        MessageContent::VoiceNote(m) => {
            ctx.store_voice_note(m.file_id, out);
            m.caption.store(out);
            store_bool(out, m.is_listened);
        }
        MessageContent::ChatCreate(m) => {
            store_string(out, &m.title);
            store_i32(out, m.participant_user_ids.len() as i32);
            for user_id in &m.participant_user_ids {
                store_i64(out, user_id.get());
            }
        }
        MessageContent::ChatChangeTitle(m) => {
            store_string(out, &m.title);
        }
        MessageContent::ChatChangePhoto(m) => {
            store_photo(&m.photo, ctx, out);
        }
        MessageContent::ChatDeletePhoto(_) => {}
        MessageContent::ChatDeleteHistory(_) => {}
        MessageContent::ChatAddUsers(m) => {
            store_i32(out, m.user_ids.len() as i32);
            for user_id in &m.user_ids {
                store_i64(out, user_id.get());
            }
        }
        MessageContent::ChatJoinedByLink(m) => {
            store_flags::<1>(out, [m.is_approved]);
        }
        MessageContent::ChatDeleteUser(m) => {
            store_i64(out, m.user_id.get());
        }
        MessageContent::ChatMigrateTo(m) => {
            // `store(ChannelId)` — the C++ id is an int32.
            store_i32(out, m.migrated_to_channel_id.get() as i32);
        }
        MessageContent::ChannelCreate(m) => {
            store_string(out, &m.title);
        }
        MessageContent::ChannelMigrateFrom(m) => {
            store_string(out, &m.title);
            // `store(ChatId)` — the C++ id is an int32.
            store_i32(out, m.migrated_from_chat_id.get() as i32);
        }
        MessageContent::PinMessage(m) => {
            store_i32(out, m.message_id.get() as i32);
        }
        MessageContent::GameScore(m) => {
            store_i32(out, m.game_message_id.get() as i32);
            store_i64(out, m.game_id);
            store_i32(out, m.score);
        }
        MessageContent::ScreenshotTaken(_) => {}
        MessageContent::ChatSetTtl(m) => {
            let has_from_user_id = m.from_user_id.is_valid();
            store_flags::<1>(out, [has_from_user_id]);
            store_i32(out, m.ttl);
            if has_from_user_id {
                store_i64(out, m.from_user_id.get());
            }
        }
        MessageContent::Call(m) => {
            let has_call_access_hash = m.call_access_hash != 0;
            store_flags::<2>(out, [m.is_video, has_call_access_hash]);
            store_i64(out, m.call_id);
            store_i32(out, m.duration);
            store_i32(out, m.discard_reason.r#type as i32);
            if has_call_access_hash {
                store_i64(out, m.call_access_hash);
            }
        }
        MessageContent::PaymentSuccessful(m) => {
            let has_payload = !m.invoice_payload.is_empty();
            let has_shipping_option_id = !m.shipping_option_id.is_empty();
            let has_order_info = m.order_info.is_some();
            let has_telegram_payment_charge_id = !m.telegram_payment_charge_id.is_empty();
            let has_provider_payment_charge_id = !m.provider_payment_charge_id.is_empty();
            let has_invoice_message_id = m.invoice_message_id.is_valid();
            let is_correctly_stored = true;
            let has_invoice_dialog_id = m.invoice_dialog_id.is_valid();
            let has_subscription_until_date = m.subscription_until_date != 0;
            store_flags::<11>(
                out,
                [
                    has_payload,
                    has_shipping_option_id,
                    has_order_info,
                    has_telegram_payment_charge_id,
                    has_provider_payment_charge_id,
                    has_invoice_message_id,
                    is_correctly_stored,
                    has_invoice_dialog_id,
                    m.is_recurring,
                    m.is_first_recurring,
                    has_subscription_until_date,
                ],
            );
            store_string(out, &m.currency);
            store_i64(out, m.total_amount);
            if has_payload {
                store_string(out, &m.invoice_payload);
            }
            if has_shipping_option_id {
                store_string(out, &m.shipping_option_id);
            }
            if let Some(order_info) = &m.order_info {
                store_order_info(order_info, out);
            }
            if has_telegram_payment_charge_id {
                store_string(out, &m.telegram_payment_charge_id);
            }
            if has_provider_payment_charge_id {
                store_string(out, &m.provider_payment_charge_id);
            }
            if has_invoice_message_id {
                store_i32(out, m.invoice_message_id.get() as i32);
            }
            if has_invoice_dialog_id {
                store_i64(out, m.invoice_dialog_id.get());
            }
            if has_subscription_until_date {
                store_i32(out, m.subscription_until_date);
            }
        }
        MessageContent::ContactRegistered(_) => {}
        MessageContent::ExpiredPhoto(_) => {}
        MessageContent::ExpiredVideo(_) => {}
        MessageContent::CustomServiceAction(m) => {
            store_string(out, &m.message);
        }
        MessageContent::WebsiteConnected(m) => {
            store_string(out, &m.domain_name);
        }
        MessageContent::PassportDataSent(m) => {
            store_i32(out, m.types.len() as i32);
            for secure_value_type in &m.types {
                store_i32(out, *secure_value_type as i32);
            }
        }
        MessageContent::PassportDataReceived(m) => {
            store_i32(out, m.values.len() as i32);
            for value in &m.values {
                store_encrypted_secure_value(value, ctx, out);
            }
            store_encrypted_secure_credentials(&m.credentials, out);
        }
        MessageContent::Poll(m) => {
            let has_caption = !m.caption.text.is_empty();
            let has_attached_media = m.attached_media.is_some();
            store_flags::<2>(out, [has_caption, has_attached_media]);
            store_i64(out, m.poll_id.get());
            if has_caption {
                m.caption.store(out);
            }
            if let Some(attached_media) = &m.attached_media {
                store_message_content(ctx, attached_media, out);
            }
        }
        MessageContent::Dice(m) => {
            let has_seed = !m.seed.is_empty();
            let has_stake_ton_count = m.stake_ton_count != 0;
            let has_prize_ton_count = m.prize_ton_count != 0;
            let has_state_hash = !m.state_hash.is_empty();
            store_flags::<5>(
                out,
                [
                    m.is_stake,
                    has_seed,
                    has_stake_ton_count,
                    has_prize_ton_count,
                    has_state_hash,
                ],
            );
            store_string(out, &m.emoji);
            store_i32(out, m.dice_value);
            if has_seed {
                store_string(out, &m.seed);
            }
            if has_stake_ton_count {
                store_i64(out, m.stake_ton_count);
            }
            if has_prize_ton_count {
                store_i64(out, m.prize_ton_count);
            }
            if has_state_hash {
                store_string(out, &m.state_hash);
            }
        }
        MessageContent::ProximityAlertTriggered(m) => {
            store_i64(out, m.traveler_dialog_id.get());
            store_i64(out, m.watcher_dialog_id.get());
            store_i32(out, m.distance);
        }
        MessageContent::GroupCall(m) => {
            let has_duration = m.duration >= 0;
            let has_schedule_date = m.schedule_date > 0;
            store_flags::<2>(out, [has_duration, has_schedule_date]);
            store_i64(out, m.input_group_call_id.get_group_call_id());
            store_i64(out, m.input_group_call_id.get_access_hash());
            if has_duration {
                store_i32(out, m.duration);
            }
            if has_schedule_date {
                store_i32(out, m.schedule_date);
            }
        }
        MessageContent::InviteToGroupCall(m) => {
            store_i64(out, m.input_group_call_id.get_group_call_id());
            store_i64(out, m.input_group_call_id.get_access_hash());
            store_i32(out, m.user_ids.len() as i32);
            for user_id in &m.user_ids {
                store_i64(out, user_id.get());
            }
        }
        MessageContent::WebViewDataSent(m) => {
            store_string(out, &m.button_text);
        }
        MessageContent::WebViewDataReceived(m) => {
            store_string(out, &m.button_text);
            store_string(out, &m.data);
        }
        MessageContent::GiftPremium(m) => {
            let has_crypto_amount = !m.crypto_currency.is_empty();
            let has_text = !m.text.text.is_empty();
            let has_days = true;
            store_flags::<3>(out, [has_crypto_amount, has_text, has_days]);
            store_string(out, &m.currency);
            store_i64(out, m.amount);
            store_i32(out, m.days);
            if has_crypto_amount {
                store_string(out, &m.crypto_currency);
                store_i64(out, m.crypto_amount);
            }
            if has_text {
                m.text.store(out);
            }
        }
        MessageContent::TopicCreate(m) => {
            store_flags::<1>(out, [m.title_missing]);
            store_string(out, &m.title);
            store_forum_topic_icon(&m.icon, out);
        }
        MessageContent::TopicEdit(m) => {
            store_forum_topic_edited_data(&m.edited_data, out);
        }
        MessageContent::SuggestProfilePhoto(m) => {
            store_photo(&m.photo, ctx, out);
        }
        MessageContent::WriteAccessAllowed(_) => {}
        MessageContent::RequestedDialog(m) => {
            let has_one_shared_dialog = m.shared_dialog_ids.len() == 1;
            store_flags::<1>(out, [has_one_shared_dialog]);
            if has_one_shared_dialog {
                store_i64(out, m.shared_dialog_ids[0].get());
            } else {
                store_i32(out, m.shared_dialog_ids.len() as i32);
                for dialog_id in &m.shared_dialog_ids {
                    store_i64(out, dialog_id.get());
                }
            }
            store_i32(out, m.button_id);
        }
        MessageContent::WebViewWriteAccessAllowed(m) => {
            store_web_app(&m.web_app, ctx, out);
        }
        MessageContent::SetBackground(m) => {
            let has_message_id = m.old_message_id.is_valid();
            store_flags::<2>(out, [has_message_id, m.for_both]);
            if has_message_id {
                store_i32(out, m.old_message_id.get() as i32);
            }
            ctx.store_background_id(m.background_info.background_id, out);
            store_background_type(&m.background_info.background_type, out);
        }
        MessageContent::Story(m) => {
            store_flags::<1>(out, [m.via_mention]);
            m.story_full_id.store(out);
        }
        MessageContent::WriteAccessAllowedByRequest(_) => {}
        MessageContent::GiftCode(m) => {
            let has_creator_dialog_id = m.creator_dialog_id.is_valid();
            let has_currency = !m.currency.is_empty();
            let has_amount = m.amount > 0;
            let has_crypto_currency = !m.crypto_currency.is_empty();
            let has_crypto_amount = m.crypto_amount > 0;
            let has_text = !m.text.text.is_empty();
            let has_days = true;
            store_flags::<9>(
                out,
                [
                    m.via_giveaway,
                    has_creator_dialog_id,
                    m.is_unclaimed,
                    has_currency,
                    has_amount,
                    has_crypto_currency,
                    has_crypto_amount,
                    has_text,
                    has_days,
                ],
            );
            if has_creator_dialog_id {
                store_i64(out, m.creator_dialog_id.get());
            }
            store_i32(out, m.days);
            store_string(out, &m.code);
            if has_currency {
                store_string(out, &m.currency);
            }
            if has_amount {
                store_i64(out, m.amount);
            }
            if has_crypto_currency {
                store_string(out, &m.crypto_currency);
            }
            if has_crypto_amount {
                store_i64(out, m.crypto_amount);
            }
            if has_text {
                m.text.store(out);
            }
        }
        MessageContent::Giveaway(m) => {
            let has_star_count = m.star_count != 0;
            store_flags::<1>(out, [has_star_count]);
            store_giveaway_parameters(&m.giveaway_parameters, out);
            store_i32(out, m.quantity);
            store_i32(out, m.months);
            if has_star_count {
                store_i64(out, m.star_count);
            }
        }
        MessageContent::GiveawayLaunch(m) => {
            let has_star_count = m.star_count != 0;
            store_flags::<1>(out, [has_star_count]);
            if has_star_count {
                store_i64(out, m.star_count);
            }
        }
        MessageContent::GiveawayResults(m) => {
            let has_winner_count = m.winner_count != 0;
            let has_unclaimed_count = m.unclaimed_count != 0;
            let has_giveaway_message_id = m.giveaway_message_id.is_valid();
            store_flags::<4>(
                out,
                [
                    has_winner_count,
                    has_unclaimed_count,
                    has_giveaway_message_id,
                    m.is_star_giveaway,
                ],
            );
            if has_winner_count {
                store_i32(out, m.winner_count);
            }
            if has_unclaimed_count {
                store_i32(out, m.unclaimed_count);
            }
            if has_giveaway_message_id {
                store_i32(out, m.giveaway_message_id.get() as i32);
            }
        }
        MessageContent::GiveawayWinners(m) => {
            let has_giveaway_message_id = m.giveaway_message_id.is_valid();
            let has_boosted_channel_id = m.boosted_channel_id.is_valid();
            let has_additional_dialog_count = m.additional_dialog_count != 0;
            let has_month_count = m.month_count != 0;
            let has_prize_description = !m.prize_description.is_empty();
            let has_winners_selection_date = m.winners_selection_date != 0;
            let has_winner_count = m.winner_count != 0;
            let has_unclaimed_count = m.unclaimed_count != 0;
            let has_winner_user_ids = !m.winner_user_ids.is_empty();
            let has_star_count = m.star_count != 0;
            store_flags::<12>(
                out,
                [
                    m.only_new_subscribers,
                    m.was_refunded,
                    has_giveaway_message_id,
                    has_boosted_channel_id,
                    has_additional_dialog_count,
                    has_month_count,
                    has_prize_description,
                    has_winners_selection_date,
                    has_winner_count,
                    has_unclaimed_count,
                    has_winner_user_ids,
                    has_star_count,
                ],
            );
            if has_giveaway_message_id {
                store_i32(out, m.giveaway_message_id.get() as i32);
            }
            if has_boosted_channel_id {
                store_i64(out, m.boosted_channel_id.get());
            }
            if has_additional_dialog_count {
                store_i32(out, m.additional_dialog_count);
            }
            if has_month_count {
                store_i32(out, m.month_count);
            }
            if has_prize_description {
                store_string(out, &m.prize_description);
            }
            if has_winners_selection_date {
                store_i32(out, m.winners_selection_date);
            }
            if has_winner_count {
                store_i32(out, m.winner_count);
            }
            if has_unclaimed_count {
                store_i32(out, m.unclaimed_count);
            }
            if has_winner_user_ids {
                store_i32(out, m.winner_user_ids.len() as i32);
                for user_id in &m.winner_user_ids {
                    store_i64(out, user_id.get());
                }
            }
            if has_star_count {
                store_i64(out, m.star_count);
            }
        }
        MessageContent::ExpiredVideoNote(_) => {}
        MessageContent::ExpiredVoiceNote(_) => {}
        MessageContent::BoostApply(m) => {
            store_flags::<0>(out, []);
            store_i32(out, m.boost_count);
        }
        MessageContent::DialogShared(m) => {
            store_flags::<0>(out, []);
            store_i32(out, m.shared_dialogs.len() as i32);
            for shared_dialog in &m.shared_dialogs {
                store_shared_dialog(shared_dialog, ctx, out);
            }
            store_i32(out, m.button_id);
        }
        MessageContent::PaidMedia(m) => {
            let has_caption = !m.caption.text.is_empty();
            let has_payload = !m.payload.is_empty();
            store_flags::<2>(out, [has_caption, has_payload]);
            store_i32(out, m.media.len() as i32);
            for extended_media in &m.media {
                store_message_extended_media(extended_media, ctx, out);
            }
            if has_caption {
                m.caption.store(out);
            }
            store_i64(out, m.star_count);
            if has_payload {
                store_string(out, &m.payload);
            }
        }
        MessageContent::PaymentRefunded(m) => {
            let has_invoice_payload = !m.invoice_payload.is_empty();
            store_flags::<1>(out, [has_invoice_payload]);
            store_i64(out, m.dialog_id.get());
            store_string(out, &m.currency);
            store_i64(out, m.total_amount);
            if has_invoice_payload {
                store_string(out, &m.invoice_payload);
            }
            store_string(out, &m.telegram_payment_charge_id);
            store_string(out, &m.provider_payment_charge_id);
        }
        MessageContent::GiftStars(m) => {
            let has_crypto_amount = !m.crypto_currency.is_empty();
            let has_transaction_id = !m.transaction_id.is_empty();
            store_flags::<2>(out, [has_crypto_amount, has_transaction_id]);
            store_string(out, &m.currency);
            store_i64(out, m.amount);
            store_i64(out, m.star_count);
            if has_crypto_amount {
                store_string(out, &m.crypto_currency);
                store_i64(out, m.crypto_amount);
            }
            if has_transaction_id {
                store_string(out, &m.transaction_id);
            }
        }
        MessageContent::PrizeStars(m) => {
            store_flags::<1>(out, [m.is_unclaimed]);
            store_i64(out, m.star_count);
            store_string(out, &m.transaction_id);
            store_i64(out, m.boosted_dialog_id.get());
            store_i32(out, m.giveaway_message_id.get() as i32);
        }
        MessageContent::PaidMessagesRefunded(m) => {
            store_flags::<0>(out, []);
            store_i32(out, m.message_count);
            store_i64(out, m.star_count);
        }
        MessageContent::PaidMessagesPrice(m) => {
            store_flags::<1>(out, [m.broadcast_messages_allowed]);
            store_i64(out, m.star_count);
        }
        MessageContent::ConferenceCall(m) => {
            let has_duration = m.duration != 0;
            let has_other_participant_dialog_ids = !m.other_participant_dialog_ids.is_empty();
            store_flags::<5>(
                out,
                [
                    m.is_active,
                    m.is_video,
                    m.was_missed,
                    has_duration,
                    has_other_participant_dialog_ids,
                ],
            );
            store_i64(out, m.call_id);
            if has_duration {
                store_i32(out, m.duration);
            }
            if has_other_participant_dialog_ids {
                store_i32(out, m.other_participant_dialog_ids.len() as i32);
                for dialog_id in &m.other_participant_dialog_ids {
                    store_i64(out, dialog_id.get());
                }
            }
        }
        MessageContent::ToDoList(m) => {
            store_flags::<0>(out, []);
            store_to_do_list(&m.list, out);
            store_i32(out, m.completions.len() as i32);
            for completion in &m.completions {
                store_to_do_completion(completion, out);
            }
        }
        MessageContent::TodoCompletions(m) => {
            let has_to_do_message_id = m.to_do_message_id.is_valid();
            let has_completed_task_ids = !m.completed_task_ids.is_empty();
            let has_incompleted_task_ids = !m.incompleted_task_ids.is_empty();
            store_flags::<3>(
                out,
                [
                    has_to_do_message_id,
                    has_completed_task_ids,
                    has_incompleted_task_ids,
                ],
            );
            if has_to_do_message_id {
                store_i32(out, m.to_do_message_id.get() as i32);
            }
            if has_completed_task_ids {
                store_i32_vec(out, &m.completed_task_ids);
            }
            if has_incompleted_task_ids {
                store_i32_vec(out, &m.incompleted_task_ids);
            }
        }
        MessageContent::TodoAppendTasks(m) => {
            let has_to_do_message_id = m.to_do_message_id.is_valid();
            let has_items = !m.items.is_empty();
            store_flags::<2>(out, [has_to_do_message_id, has_items]);
            if has_to_do_message_id {
                store_i32(out, m.to_do_message_id.get() as i32);
            }
            if has_items {
                store_i32(out, m.items.len() as i32);
                for item in &m.items {
                    store_to_do_item(item, out);
                }
            }
        }
        MessageContent::GiftTon(m) => {
            let has_transaction_id = !m.transaction_id.is_empty();
            store_flags::<1>(out, [has_transaction_id]);
            store_string(out, &m.currency);
            store_i64(out, m.amount);
            store_string(out, &m.crypto_currency);
            store_i64(out, m.crypto_amount);
            if has_transaction_id {
                store_string(out, &m.transaction_id);
            }
        }
        MessageContent::SuggestedPostSuccess(m) => {
            let has_suggested_post_message_id = m.suggested_post_message_id.is_valid();
            let has_amount = m.amount != CurrencyAmount::default();
            store_flags::<2>(out, [has_suggested_post_message_id, has_amount]);
            if has_suggested_post_message_id {
                store_i32(out, m.suggested_post_message_id.get() as i32);
            }
            if has_amount {
                store_currency_amount(&m.amount, out);
            }
        }
        MessageContent::SuggestedPostRefund(m) => {
            let has_suggested_post_message_id = m.suggested_post_message_id.is_valid();
            store_flags::<2>(out, [has_suggested_post_message_id, m.is_payer_initiated]);
            if has_suggested_post_message_id {
                store_i32(out, m.suggested_post_message_id.get() as i32);
            }
        }
        MessageContent::SuggestedPostApproval(m) => {
            let has_suggested_post_message_id = m.suggested_post_message_id.is_valid();
            let has_comment = !m.comment.is_empty();
            let has_price = m.price != SuggestedPostPrice::default();
            let has_send_date = m.send_date != 0;
            store_flags::<6>(
                out,
                [
                    has_suggested_post_message_id,
                    has_price,
                    has_comment,
                    has_send_date,
                    m.is_rejected,
                    m.is_balance_too_low,
                ],
            );
            if has_suggested_post_message_id {
                store_i32(out, m.suggested_post_message_id.get() as i32);
            }
            if has_price {
                store_suggested_post_price(&m.price, out);
            }
            if has_comment {
                store_string(out, &m.comment);
            }
            if has_send_date {
                store_i32(out, m.send_date);
            }
        }
        MessageContent::SuggestBirthday(m) => {
            store_flags::<0>(out, []);
            m.birthdate.store(out);
        }
        MessageContent::NewCreatorPending(m) => {
            let has_new_creator_user_id = m.new_creator_user_id.is_valid();
            store_flags::<1>(out, [has_new_creator_user_id]);
            if has_new_creator_user_id {
                store_i64(out, m.new_creator_user_id.get());
            }
        }
        MessageContent::ChangeCreator(m) => {
            store_flags::<0>(out, []);
            store_i64(out, m.new_creator_user_id.get());
        }
        MessageContent::NoForwardsToggle(m) => {
            let has_request_message_id = m.request_message_id.is_valid();
            store_flags::<3>(out, [m.prev_value, m.new_value, has_request_message_id]);
            if has_request_message_id {
                store_i32(out, m.request_message_id.get() as i32);
            }
        }
        MessageContent::NoForwardsRequest(m) => {
            store_flags::<3>(out, [m.is_expired, m.prev_value, m.new_value]);
        }
        MessageContent::ManagedBotCreated(m) => {
            store_flags::<0>(out, []);
            store_i64(out, m.bot_user_id.get());
        }
        MessageContent::PollAppendAnswer(m) => {
            let has_poll_message_id = m.poll_message_id.is_valid();
            store_flags::<1>(out, [has_poll_message_id]);
            if has_poll_message_id {
                store_i32(out, m.poll_message_id.get() as i32);
            }
            m.text.store(out);
            store_string(out, &m.data);
        }
        MessageContent::PollDeleteAnswer(m) => {
            let has_poll_message_id = m.poll_message_id.is_valid();
            store_flags::<1>(out, [has_poll_message_id]);
            if has_poll_message_id {
                store_i32(out, m.poll_message_id.get() as i32);
            }
            m.text.store(out);
            store_string(out, &m.data);
        }
        MessageContent::ChangeCommunity(m) => {
            let has_community_id = m.community_id.is_valid();
            store_flags::<1>(out, [has_community_id]);
            if has_community_id {
                m.community_id.store(out);
            }
        }
        MessageContent::ChatJoinedViaCommunity(m) => {
            store_flags::<0>(out, []);
            m.community_id.store(out);
        }
    }
}

/// `SuggestedPostPrice::store` (SuggestedPostPrice.hpp:6-16).
fn store_suggested_post_price(price: &SuggestedPostPrice, out: &mut Vec<u8>) {
    let has_amount = price.amount != 0;
    store_flags::<1>(out, [has_amount]);
    store_i32(out, price.r#type as i32);
    if has_amount {
        store_i64(out, price.amount);
    }
}

/// `SuggestedPostPrice::parse` (SuggestedPostPrice.hpp:18-...).
fn parse_suggested_post_price(input: &mut Parser) -> Result<SuggestedPostPrice, &'static str> {
    let [has_amount] = input.parse_flags::<1>()?;
    let type_raw = input.i32_()?;
    let r#type = match type_raw {
        0 => SuggestedPostPriceType::None,
        1 => SuggestedPostPriceType::Star,
        2 => SuggestedPostPriceType::Ton,
        _ => SuggestedPostPriceType::default(),
    };
    let amount = if has_amount { input.i64_()? } else { 0 };
    Ok(SuggestedPostPrice { r#type, amount })
}

/// The `BackgroundFill::get_type` discriminator (BackgroundType.h:48-57).
fn background_fill_get_type(fill: &BackgroundFill) -> BackgroundFillType {
    fill.get_type()
}

/// `BackgroundType::store` (BackgroundType.hpp:6-40).
fn store_background_type(background_type: &BackgroundType, out: &mut Vec<u8>) {
    let has_fill = background_type.fill.top_color != 0 || background_type.fill.bottom_color != 0;
    let has_intensity = background_type.intensity != 0;
    let fill_type = background_fill_get_type(&background_type.fill);
    let is_gradient = matches!(fill_type, BackgroundFillType::Gradient);
    let is_freeform_gradient = matches!(fill_type, BackgroundFillType::FreeformGradient);
    let has_theme_name = !background_type.theme_name.is_empty();
    store_flags::<7>(
        out,
        [
            background_type.is_blurred,
            background_type.is_moving,
            has_fill,
            has_intensity,
            is_gradient,
            is_freeform_gradient,
            has_theme_name,
        ],
    );
    store_i32(out, background_type.r#type as i32);
    if is_freeform_gradient {
        store_i32(out, background_type.fill.top_color);
        store_i32(out, background_type.fill.bottom_color);
        store_i32(out, background_type.fill.third_color);
        store_i32(out, background_type.fill.fourth_color);
    } else if has_fill {
        store_i32(out, background_type.fill.top_color);
        if is_gradient {
            store_i32(out, background_type.fill.bottom_color);
            store_i32(out, background_type.fill.rotation_angle);
        }
    }
    if has_intensity {
        store_i32(out, background_type.intensity);
    }
    if has_theme_name {
        store_string(out, &background_type.theme_name);
    }
}

/// `BackgroundType::parse` (BackgroundType.hpp:42-...).
fn parse_background_type(input: &mut Parser) -> Result<BackgroundType, &'static str> {
    let [
        is_blurred,
        is_moving,
        has_fill,
        has_intensity,
        is_gradient,
        is_freeform_gradient,
        has_theme_name,
    ] = input.parse_flags::<7>()?;
    let type_raw = input.i32_()?;
    let r#type = match type_raw {
        0 => BackgroundTypeType::Wallpaper,
        1 => BackgroundTypeType::Pattern,
        2 => BackgroundTypeType::Fill,
        3 => BackgroundTypeType::ChatTheme,
        _ => BackgroundTypeType::default(),
    };
    let fill = if is_freeform_gradient {
        BackgroundFill {
            top_color: input.i32_()?,
            bottom_color: input.i32_()?,
            rotation_angle: 0,
            third_color: input.i32_()?,
            fourth_color: input.i32_()?,
        }
    } else if has_fill {
        let top_color = input.i32_()?;
        if is_gradient {
            BackgroundFill {
                top_color,
                bottom_color: input.i32_()?,
                rotation_angle: input.i32_()?,
                third_color: -1,
                fourth_color: -1,
            }
        } else {
            BackgroundFill {
                top_color,
                bottom_color: top_color,
                rotation_angle: 0,
                third_color: -1,
                fourth_color: -1,
            }
        }
    } else {
        BackgroundFill {
            top_color: 0,
            bottom_color: 0,
            rotation_angle: 0,
            third_color: -1,
            fourth_color: -1,
        }
    };
    let intensity = if has_intensity { input.i32_()? } else { 0 };
    let theme_name = if has_theme_name {
        input.string()?
    } else {
        String::new()
    };
    Ok(BackgroundType {
        r#type,
        is_blurred,
        is_moving,
        intensity,
        fill,
        theme_name,
    })
}

/// The `MessageId` value reader — the C++ id is an int32 on the wire
/// (the Rust model widens it to i64).
fn parse_message_id(input: &mut Parser) -> Result<MessageId, &'static str> {
    Ok(MessageId::new(i64::from(input.i32_()?)))
}

/// The C++ `is_bad → MessageUnsupported(0)` fallback record.
fn unsupported_content() -> MessageContent {
    MessageContent::Unsupported(MessageUnsupported { version: 0 })
}

/// `Photo::is_bad` (Photo.h:51-61) — an empty photo or any size without
/// a valid file id makes the record unusable.
fn photo_is_bad(photo: &Photo) -> bool {
    photo.is_empty()
        || photo
            .photos
            .iter()
            .any(|photo_size| !photo_size.file_id.is_valid())
}

/// The `MessageExtendedMedia::Type` value reader — the inverse of the
/// `as i32` the store writes; unknown values keep the enum default,
/// exactly like the C++ enum assignment.
fn message_extended_media_type_from_raw(raw: i32) -> MessageExtendedMediaType {
    match raw {
        0 => MessageExtendedMediaType::Empty,
        1 => MessageExtendedMediaType::Unsupported,
        2 => MessageExtendedMediaType::Preview,
        3 => MessageExtendedMediaType::Photo,
        4 => MessageExtendedMediaType::Video,
        _ => MessageExtendedMediaType::default(),
    }
}

/// The `SecureValueType` value reader for [`parse_message_content`] —
/// the inverse of the `as i32` the store writes; unknown values keep
/// the enum default, exactly like the C++ enum assignment.
fn secure_value_type_from_raw(raw: i32) -> SecureValueType {
    match raw {
        0 => SecureValueType::None,
        1 => SecureValueType::PersonalDetails,
        2 => SecureValueType::Passport,
        3 => SecureValueType::DriverLicense,
        4 => SecureValueType::IdentityCard,
        5 => SecureValueType::InternalPassport,
        6 => SecureValueType::Address,
        7 => SecureValueType::UtilityBill,
        8 => SecureValueType::BankStatement,
        9 => SecureValueType::RentalAgreement,
        10 => SecureValueType::PassportRegistration,
        11 => SecureValueType::TemporaryRegistration,
        12 => SecureValueType::PhoneNumber,
        13 => SecureValueType::EmailAddress,
        _ => SecureValueType::default(),
    }
}

/// `parse(unique_ptr<MessageContent> &content, ParserT &parser)`
/// (MessageContent.cpp:3048-4505).
///
/// Parse errors and the C++ `is_bad` validations all degrade to
/// [`unsupported_content`]. The seven deferred payload types (Contact,
/// ChatSetTheme, StarGift, StarGiftUnique, StarGiftPurchaseOffer,
/// StarGiftPurchaseOfferDeclined, RichText) have no model to rebuild on
/// this line and degrade the same way — the C++ degrades to the same
/// record for any type it cannot rebuild (`[deferred-variant]`).
pub fn parse_message_content(
    ctx: &mut dyn ContentStoreContext,
    input: &mut Parser,
) -> MessageContent {
    let raw = match input.i32_() {
        Ok(raw) => raw,
        Err(_) => return unsupported_content(),
    };
    let content_type = match content_type_from_raw(raw) {
        Some(content_type) => content_type,
        None => {
            tdutils::log_error!("Load a message with an invalid content of type {}", raw);
            return unsupported_content();
        }
    };
    let parsed: Result<MessageContent, &'static str> =
        (|| -> Result<MessageContent, &'static str> {
            match content_type {
                MessageContentType::Animation => {
                    let file_id = ctx.parse_animation(input)?;
                    let has_spoiler = if input.version() >= version::ADD_MESSAGE_MEDIA_SPOILER {
                        input.parse_flags::<1>()?[0]
                    } else {
                        false
                    };
                    let caption = parse_caption(input)?;
                    if !file_id.is_valid() {
                        return Ok(unsupported_content());
                    }
                    Ok(MessageContent::Animation(MessageAnimation {
                        file_id,
                        caption,
                        has_spoiler,
                    }))
                }
                MessageContentType::Audio => {
                    let file_id = ctx.parse_audio(input)?;
                    let caption = parse_caption(input)?;
                    let _legacy_is_listened = input.bool()?;
                    if !file_id.is_valid() {
                        return Ok(unsupported_content());
                    }
                    Ok(MessageContent::Audio(MessageAudio { file_id, caption }))
                }
                MessageContentType::Contact => Err("[deferred-variant] MessageContact"),
                MessageContentType::Document => {
                    let file_id = ctx.parse_document(input)?;
                    let caption = parse_caption(input)?;
                    if !file_id.is_valid() {
                        return Ok(unsupported_content());
                    }
                    Ok(MessageContent::Document(MessageDocument {
                        file_id,
                        caption,
                    }))
                }
                MessageContentType::Game => Ok(MessageContent::Game(MessageGame {
                    game: parse_game(ctx, input)?,
                })),
                MessageContentType::Invoice => Ok(MessageContent::Invoice(MessageInvoice {
                    input_invoice: parse_input_invoice(ctx, input)?,
                })),
                MessageContentType::LiveLocation => {
                    let location = parse_location(input)?;
                    let period = input.i32_()?;
                    let heading = if input.version() >= version::ADD_LIVE_LOCATION_HEADING {
                        input.i32_()?
                    } else {
                        0
                    };
                    let proximity_alert_radius =
                        if input.version() >= version::ADD_LIVE_LOCATION_PROXIMITY_ALERT_DISTANCE {
                            input.i32_()?
                        } else {
                            0
                        };
                    if period <= 0 {
                        return Ok(unsupported_content());
                    }
                    Ok(MessageContent::LiveLocation(MessageLiveLocation {
                        location,
                        period,
                        heading,
                        proximity_alert_radius,
                    }))
                }
                MessageContentType::Location => Ok(MessageContent::Location(MessageLocation {
                    location: parse_location(input)?,
                })),
                MessageContentType::Photo => {
                    let photo = parse_photo(ctx, input)?;
                    if photo_is_bad(&photo) {
                        return Ok(unsupported_content());
                    }
                    let (has_spoiler, has_video_file_id) =
                        if input.version() >= version::ADD_MESSAGE_MEDIA_SPOILER {
                            let flags = input.parse_flags::<2>()?;
                            (flags[0], flags[1])
                        } else {
                            (false, false)
                        };
                    let caption = parse_caption(input)?;
                    let video_file_id = if has_video_file_id {
                        ctx.parse_video(input)?
                    } else {
                        FileId::default()
                    };
                    Ok(MessageContent::Photo(MessagePhoto {
                        photo,
                        video_file_id,
                        caption,
                        has_spoiler,
                    }))
                }
                MessageContentType::Sticker => {
                    let file_id = ctx.parse_sticker(false, input)?;
                    let is_premium = if input.version() >= version::ADD_MESSAGE_STICKER_FLAGS {
                        input.parse_flags::<1>()?[0]
                    } else {
                        false
                    };
                    if !file_id.is_valid() {
                        return Ok(unsupported_content());
                    }
                    Ok(MessageContent::Sticker(MessageSticker {
                        file_id,
                        is_premium,
                    }))
                }
                MessageContentType::Text => {
                    let (
                        has_web_page_id,
                        force_small_media,
                        force_large_media,
                        has_web_page_url,
                        skip_web_page_confirmation,
                    ) = if input.version() >= version::ADD_MESSAGE_TEXT_FLAGS {
                        let flags = input.parse_flags::<5>()?;
                        (flags[0], flags[1], flags[2], flags[3], flags[4])
                    } else {
                        (true, false, false, false, false)
                    };
                    let text = FormattedText::parse(input)?;
                    let web_page_id = if has_web_page_id {
                        WebPageId::new(input.i64_()?)
                    } else {
                        WebPageId::default()
                    };
                    let web_page_url = if has_web_page_url {
                        input.string()?
                    } else {
                        String::new()
                    };
                    if has_web_page_url && has_web_page_id {
                        ctx.on_loaded_web_page_url(web_page_id, &web_page_url);
                    }
                    Ok(MessageContent::Text(MessageText {
                        text,
                        web_page_id,
                        force_small_media,
                        force_large_media,
                        skip_web_page_confirmation,
                        web_page_url,
                    }))
                }
                MessageContentType::Unsupported => {
                    let version_value =
                        if input.version() >= version::ADD_MESSAGE_UNSUPPORTED_VERSION {
                            input.i32_()?
                        } else {
                            0
                        };
                    Ok(MessageContent::Unsupported(MessageUnsupported {
                        version: version_value,
                    }))
                }
                MessageContentType::Venue => Ok(MessageContent::Venue(MessageVenue {
                    venue: parse_venue(input)?,
                })),
                MessageContentType::Video => {
                    let file_id = ctx.parse_video(input)?;
                    let (
                        has_spoiler,
                        has_alternative_videos,
                        has_hls_files,
                        has_cover,
                        has_start_timestamp,
                        has_storyboard_files,
                        has_storyboard_map_files,
                    ) = if input.version() >= version::ADD_MESSAGE_MEDIA_SPOILER {
                        let flags = input.parse_flags::<7>()?;
                        (
                            flags[0], flags[1], flags[2], flags[3], flags[4], flags[5], flags[6],
                        )
                    } else {
                        (false, false, false, false, false, false, false)
                    };
                    let caption = parse_caption(input)?;
                    let mut is_bad = !file_id.is_valid();
                    let mut alternative_file_ids = Vec::new();
                    if has_alternative_videos {
                        let count = input.i32_()?;
                        for _ in 0..count.max(0) {
                            let alternative = ctx.parse_video(input)?;
                            if !alternative.is_valid() {
                                is_bad = true;
                            } else {
                                alternative_file_ids.push(alternative);
                            }
                        }
                    }
                    let mut hls_file_ids = Vec::new();
                    if has_hls_files {
                        let count = input.i32_()?;
                        for _ in 0..count.max(0) {
                            let hls = ctx.parse_document(input)?;
                            if !hls.is_valid() {
                                is_bad = true;
                            } else {
                                hls_file_ids.push(hls);
                            }
                        }
                    }
                    let cover = if has_cover {
                        parse_photo(ctx, input)?
                    } else {
                        Photo::default()
                    };
                    let start_timestamp = if has_start_timestamp {
                        input.i32_()?
                    } else {
                        0
                    };
                    let mut storyboard_file_ids = Vec::new();
                    if has_storyboard_files {
                        let count = input.i32_()?;
                        for _ in 0..count.max(0) {
                            let storyboard = ctx.parse_document(input)?;
                            if !storyboard.is_valid() {
                                is_bad = true;
                            } else {
                                storyboard_file_ids.push(storyboard);
                            }
                        }
                    }
                    let mut storyboard_map_file_ids = Vec::new();
                    if has_storyboard_map_files {
                        let count = input.i32_()?;
                        for _ in 0..count.max(0) {
                            let storyboard_map = ctx.parse_document(input)?;
                            if !storyboard_map.is_valid() {
                                is_bad = true;
                            } else {
                                storyboard_map_file_ids.push(storyboard_map);
                            }
                        }
                    }
                    if is_bad {
                        return Ok(unsupported_content());
                    }
                    Ok(MessageContent::Video(MessageVideo {
                        file_id,
                        alternative_file_ids,
                        hls_file_ids,
                        storyboard_file_ids,
                        storyboard_map_file_ids,
                        cover,
                        start_timestamp,
                        caption,
                        has_spoiler,
                    }))
                }
                MessageContentType::VideoNote => {
                    let file_id = ctx.parse_video_note(input)?;
                    let is_viewed = input.bool()?;
                    if !file_id.is_valid() {
                        return Ok(unsupported_content());
                    }
                    Ok(MessageContent::VideoNote(MessageVideoNote {
                        file_id,
                        is_viewed,
                    }))
                }
                MessageContentType::VoiceNote => {
                    let file_id = ctx.parse_voice_note(input)?;
                    let caption = parse_caption(input)?;
                    let is_listened = input.bool()?;
                    if !file_id.is_valid() {
                        return Ok(unsupported_content());
                    }
                    Ok(MessageContent::VoiceNote(MessageVoiceNote {
                        file_id,
                        caption,
                        is_listened,
                    }))
                }
                MessageContentType::ChatCreate => {
                    let title = input.string()?;
                    let len = input.i32_()?;
                    if len < 0 {
                        return Err("Wrong vector length");
                    }
                    let mut participant_user_ids = Vec::with_capacity(len as usize);
                    for _ in 0..len {
                        participant_user_ids.push(UserId::new(input.i64_()?));
                    }
                    Ok(MessageContent::ChatCreate(MessageChatCreate {
                        title,
                        participant_user_ids,
                    }))
                }
                MessageContentType::ChatChangeTitle => {
                    Ok(MessageContent::ChatChangeTitle(MessageChatChangeTitle {
                        title: input.string()?,
                    }))
                }
                MessageContentType::ChatChangePhoto => {
                    let photo = parse_photo(ctx, input)?;
                    if photo.is_empty() {
                        return Ok(unsupported_content());
                    }
                    Ok(MessageContent::ChatChangePhoto(MessageChatChangePhoto {
                        photo,
                    }))
                }
                MessageContentType::ChatDeletePhoto => {
                    Ok(MessageContent::ChatDeletePhoto(MessageChatDeletePhoto))
                }
                MessageContentType::ChatDeleteHistory => {
                    Ok(MessageContent::ChatDeleteHistory(MessageChatDeleteHistory))
                }
                MessageContentType::ChatAddUsers => {
                    let len = input.i32_()?;
                    if len < 0 {
                        return Err("Wrong vector length");
                    }
                    let mut user_ids = Vec::with_capacity(len as usize);
                    for _ in 0..len {
                        user_ids.push(UserId::new(input.i64_()?));
                    }
                    Ok(MessageContent::ChatAddUsers(MessageChatAddUsers {
                        user_ids,
                    }))
                }
                MessageContentType::ChatJoinedByLink => {
                    let is_approved =
                        if input.version() >= version::ADD_INVITE_LINKS_REQUIRING_APPROVAL {
                            input.parse_flags::<1>()?[0]
                        } else {
                            false
                        };
                    Ok(MessageContent::ChatJoinedByLink(MessageChatJoinedByLink {
                        is_approved,
                    }))
                }
                MessageContentType::ChatDeleteUser => {
                    Ok(MessageContent::ChatDeleteUser(MessageChatDeleteUser {
                        user_id: UserId::new(input.i64_()?),
                    }))
                }
                MessageContentType::ChatMigrateTo => {
                    Ok(MessageContent::ChatMigrateTo(MessageChatMigrateTo {
                        migrated_to_channel_id: ChannelId::new(input.i32_()? as i64),
                    }))
                }
                MessageContentType::ChannelCreate => {
                    Ok(MessageContent::ChannelCreate(MessageChannelCreate {
                        title: input.string()?,
                    }))
                }
                MessageContentType::ChannelMigrateFrom => {
                    let title = input.string()?;
                    let migrated_from_chat_id = ChatId::new(input.i32_()? as i64);
                    Ok(MessageContent::ChannelMigrateFrom(
                        MessageChannelMigrateFrom {
                            title,
                            migrated_from_chat_id,
                        },
                    ))
                }
                MessageContentType::PinMessage => {
                    Ok(MessageContent::PinMessage(MessagePinMessage {
                        message_id: parse_message_id(input)?,
                    }))
                }
                MessageContentType::GameScore => {
                    let game_message_id = parse_message_id(input)?;
                    let game_id = input.i64_()?;
                    let score = input.i32_()?;
                    Ok(MessageContent::GameScore(MessageGameScore {
                        game_message_id,
                        game_id,
                        score,
                    }))
                }
                MessageContentType::ScreenshotTaken => {
                    Ok(MessageContent::ScreenshotTaken(MessageScreenshotTaken))
                }
                MessageContentType::ChatSetTtl => {
                    let has_from_user_id =
                        if input.version() >= version::ADD_MESSAGE_CHAT_SET_TTL_FLAGS {
                            input.parse_flags::<1>()?[0]
                        } else {
                            false
                        };
                    let ttl = input.i32_()?;
                    let from_user_id = if has_from_user_id {
                        UserId::new(input.i64_()?)
                    } else {
                        UserId::default()
                    };
                    Ok(MessageContent::ChatSetTtl(MessageChatSetTtl {
                        ttl,
                        from_user_id,
                    }))
                }
                MessageContentType::Call => {
                    let (is_video, has_call_access_hash) =
                        if input.version() >= version::ADD_VIDEO_CALLS_SUPPORT {
                            let flags = input.parse_flags::<2>()?;
                            (flags[0], flags[1])
                        } else {
                            (false, false)
                        };
                    let call_id = input.i64_()?;
                    let duration = input.i32_()?;
                    let discard_reason_type_raw = input.i32_()?;
                    let call_access_hash = if has_call_access_hash {
                        input.i64_()?
                    } else {
                        0
                    };
                    let discard_reason = CallDiscardReason {
                        r#type: match discard_reason_type_raw {
                            0 => CallDiscardReasonType::Empty,
                            1 => CallDiscardReasonType::Missed,
                            2 => CallDiscardReasonType::Disconnected,
                            3 => CallDiscardReasonType::HungUp,
                            4 => CallDiscardReasonType::Declined,
                            5 => CallDiscardReasonType::UpgradeToGroupCall,
                            _ => CallDiscardReasonType::default(),
                        },
                        slug: String::new(),
                    };
                    Ok(MessageContent::Call(MessageCall {
                        call_id,
                        call_access_hash,
                        duration,
                        discard_reason,
                        is_video,
                    }))
                }
                MessageContentType::PaymentSuccessful => {
                    let flags = input.parse_flags::<11>()?;
                    let (
                        has_payload,
                        has_shipping_option_id,
                        has_order_info,
                        has_telegram_payment_charge_id,
                        has_provider_payment_charge_id,
                        has_invoice_message_id,
                        is_correctly_stored,
                        has_invoice_dialog_id,
                        is_recurring,
                        is_first_recurring,
                        has_subscription_until_date,
                    ) = (
                        flags[0], flags[1], flags[2], flags[3], flags[4], flags[5], flags[6],
                        flags[7], flags[8], flags[9], flags[10],
                    );
                    let currency = input.string()?;
                    let total_amount = input.i64_()?;
                    // The pre-fix records stored payload/shipping swapped with the
                    // total amount (MessageContent.cpp:3402-3412).
                    let (invoice_payload, shipping_option_id) = if is_correctly_stored {
                        (
                            if has_payload {
                                input.string()?
                            } else {
                                String::new()
                            },
                            if has_shipping_option_id {
                                input.string()?
                            } else {
                                String::new()
                            },
                        )
                    } else {
                        (
                            if has_payload {
                                input.i64_()?.to_string()
                            } else {
                                String::new()
                            },
                            if has_shipping_option_id {
                                input.string()?
                            } else {
                                String::new()
                            },
                        )
                    };
                    let order_info = if has_order_info {
                        Some(parse_order_info(input)?)
                    } else {
                        None
                    };
                    let telegram_payment_charge_id = if has_telegram_payment_charge_id {
                        input.string()?
                    } else {
                        String::new()
                    };
                    let provider_payment_charge_id = if has_provider_payment_charge_id {
                        input.string()?
                    } else {
                        String::new()
                    };
                    let invoice_message_id = if has_invoice_message_id {
                        parse_message_id(input)?
                    } else {
                        MessageId::default()
                    };
                    let invoice_dialog_id = if has_invoice_dialog_id {
                        DialogId::new(input.i64_()?)
                    } else {
                        DialogId::default()
                    };
                    let subscription_until_date = if has_subscription_until_date {
                        input.i32_()?
                    } else {
                        0
                    };
                    if !is_correctly_stored {
                        return Ok(unsupported_content());
                    }
                    Ok(MessageContent::PaymentSuccessful(
                        MessagePaymentSuccessful {
                            invoice_dialog_id,
                            invoice_message_id,
                            currency,
                            total_amount,
                            invoice_payload,
                            subscription_until_date,
                            is_recurring,
                            is_first_recurring,
                            shipping_option_id,
                            order_info,
                            telegram_payment_charge_id,
                            provider_payment_charge_id,
                        },
                    ))
                }
                MessageContentType::ContactRegistered => {
                    Ok(MessageContent::ContactRegistered(MessageContactRegistered))
                }
                MessageContentType::ExpiredPhoto => {
                    Ok(MessageContent::ExpiredPhoto(MessageExpiredPhoto))
                }
                MessageContentType::ExpiredVideo => {
                    Ok(MessageContent::ExpiredVideo(MessageExpiredVideo))
                }
                MessageContentType::CustomServiceAction => Ok(MessageContent::CustomServiceAction(
                    MessageCustomServiceAction {
                        message: input.string()?,
                    },
                )),
                MessageContentType::WebsiteConnected => {
                    Ok(MessageContent::WebsiteConnected(MessageWebsiteConnected {
                        domain_name: input.string()?,
                    }))
                }
                MessageContentType::PassportDataSent => {
                    let len = input.i32_()?;
                    if len < 0 {
                        return Err("Wrong vector length");
                    }
                    let mut types = Vec::with_capacity(len as usize);
                    for _ in 0..len {
                        let raw_type = input.i32_()?;
                        types.push(secure_value_type_from_raw(raw_type));
                    }
                    Ok(MessageContent::PassportDataSent(MessagePassportDataSent {
                        types,
                    }))
                }
                MessageContentType::PassportDataReceived => {
                    let len = input.i32_()?;
                    if len < 0 {
                        return Err("Wrong vector length");
                    }
                    let mut values = Vec::with_capacity(len as usize);
                    for _ in 0..len {
                        values.push(parse_encrypted_secure_value(ctx, input)?);
                    }
                    let credentials = parse_encrypted_secure_credentials(input)?;
                    Ok(MessageContent::PassportDataReceived(
                        MessagePassportDataReceived {
                            values,
                            credentials,
                        },
                    ))
                }
                MessageContentType::Poll => {
                    let (has_caption, has_attached_media) =
                        if input.version() >= version::ADD_POLL_CAPTION {
                            let flags = input.parse_flags::<2>()?;
                            (flags[0], flags[1])
                        } else {
                            (false, false)
                        };
                    let poll_id = PollId::new(input.i64_()?);
                    let caption = if has_caption {
                        parse_caption(input)?
                    } else {
                        FormattedText::default()
                    };
                    let attached_media = if has_attached_media {
                        let attached = parse_message_content(ctx, input);
                        Some(Box::new(attached))
                    } else {
                        None
                    };
                    if !poll_id.is_valid() {
                        return Ok(unsupported_content());
                    }
                    Ok(MessageContent::Poll(MessagePoll {
                        poll_id,
                        caption,
                        attached_media,
                    }))
                }
                MessageContentType::Dice => {
                    let (
                        is_stake,
                        has_seed,
                        has_stake_ton_count,
                        has_prize_ton_count,
                        has_state_hash,
                    ) = if input.version() >= version::ADD_DICE_FLAGS {
                        let flags = input.parse_flags::<5>()?;
                        (flags[0], flags[1], flags[2], flags[3], flags[4])
                    } else {
                        (false, false, false, false, false)
                    };
                    let emoji = if input.version() >= version::ADD_DICE_EMOJI {
                        remove_emoji_modifiers(&input.string()?, true)
                    } else {
                        MessageDice::DEFAULT_EMOJI.to_string()
                    };
                    let dice_value = input.i32_()?;
                    let seed = if has_seed {
                        input.string()?
                    } else {
                        String::new()
                    };
                    let stake_ton_count = if has_stake_ton_count {
                        input.i64_()?
                    } else {
                        0
                    };
                    let prize_ton_count = if has_prize_ton_count {
                        input.i64_()?
                    } else {
                        0
                    };
                    let state_hash = if has_state_hash {
                        input.string()?
                    } else {
                        String::new()
                    };
                    let dice = MessageDice {
                        emoji,
                        dice_value,
                        is_stake,
                        seed,
                        state_hash,
                        stake_ton_count,
                        prize_ton_count,
                    };
                    if !dice.is_valid() {
                        return Ok(unsupported_content());
                    }
                    Ok(MessageContent::Dice(dice))
                }
                MessageContentType::ProximityAlertTriggered => {
                    let traveler_dialog_id = DialogId::new(input.i64_()?);
                    let watcher_dialog_id = DialogId::new(input.i64_()?);
                    let distance = input.i32_()?;
                    Ok(MessageContent::ProximityAlertTriggered(
                        MessageProximityAlertTriggered {
                            traveler_dialog_id,
                            watcher_dialog_id,
                            distance,
                        },
                    ))
                }
                MessageContentType::GroupCall => {
                    let (has_duration, has_schedule_date) = {
                        let flags = input.parse_flags::<2>()?;
                        (flags[0], flags[1])
                    };
                    let input_group_call_id = InputGroupCallId::new(input.i64_()?, input.i64_()?);
                    let duration = if has_duration { input.i32_()? } else { -1 };
                    let schedule_date = if has_schedule_date { input.i32_()? } else { -1 };
                    Ok(MessageContent::GroupCall(MessageGroupCall {
                        input_group_call_id,
                        duration,
                        schedule_date,
                    }))
                }
                MessageContentType::InviteToGroupCall => {
                    let input_group_call_id = InputGroupCallId::new(input.i64_()?, input.i64_()?);
                    let len = input.i32_()?;
                    if len < 0 {
                        return Err("Wrong vector length");
                    }
                    let mut user_ids = Vec::with_capacity(len as usize);
                    for _ in 0..len {
                        user_ids.push(UserId::new(input.i64_()?));
                    }
                    Ok(MessageContent::InviteToGroupCall(
                        MessageInviteToGroupCall {
                            input_group_call_id,
                            user_ids,
                        },
                    ))
                }
                MessageContentType::ChatSetTheme => Err("[deferred-variant] MessageChatSetTheme"),
                MessageContentType::WebViewDataSent => {
                    Ok(MessageContent::WebViewDataSent(MessageWebViewDataSent {
                        button_text: input.string()?,
                    }))
                }
                MessageContentType::WebViewDataReceived => {
                    let button_text = input.string()?;
                    let data = input.string()?;
                    Ok(MessageContent::WebViewDataReceived(
                        MessageWebViewDataReceived { button_text, data },
                    ))
                }
                MessageContentType::GiftPremium => {
                    let (has_crypto_amount, has_text, has_days) = {
                        let flags = input.parse_flags::<3>()?;
                        (flags[0], flags[1], flags[2])
                    };
                    let currency = input.string()?;
                    let amount = input.i64_()?;
                    let mut days = input.i32_()?;
                    if !has_days {
                        days = get_premium_duration_day_count(days);
                    }
                    let (crypto_currency, crypto_amount) = if has_crypto_amount {
                        (input.string()?, input.i64_()?)
                    } else {
                        (String::new(), 0)
                    };
                    let text = if has_text {
                        FormattedText::parse(input)?
                    } else {
                        FormattedText::default()
                    };
                    Ok(MessageContent::GiftPremium(MessageGiftPremium {
                        text,
                        currency,
                        amount,
                        crypto_currency,
                        crypto_amount,
                        days,
                    }))
                }
                MessageContentType::TopicCreate => {
                    let title_missing = if input.version() >= version::SUPPORT_BOT_TOPICS {
                        input.parse_flags::<1>()?[0]
                    } else {
                        false
                    };
                    let title = input.string()?;
                    let icon = parse_forum_topic_icon(input)?;
                    Ok(MessageContent::TopicCreate(MessageTopicCreate {
                        title,
                        icon,
                        title_missing,
                    }))
                }
                MessageContentType::TopicEdit => Ok(MessageContent::TopicEdit(MessageTopicEdit {
                    edited_data: parse_forum_topic_edited_data(input)?,
                })),
                MessageContentType::SuggestProfilePhoto => {
                    let photo = parse_photo(ctx, input)?;
                    if photo.is_empty() {
                        return Ok(unsupported_content());
                    }
                    Ok(MessageContent::SuggestProfilePhoto(
                        MessageSuggestProfilePhoto { photo },
                    ))
                }
                MessageContentType::WriteAccessAllowed => Ok(MessageContent::WriteAccessAllowed(
                    MessageWriteAccessAllowed,
                )),
                MessageContentType::RequestedDialog => {
                    let has_one_shared_dialog =
                        if input.version() >= version::SUPPORT_MULTIPLE_SHARED_USERS {
                            input.parse_flags::<1>()?[0]
                        } else {
                            true
                        };
                    let shared_dialog_ids = if has_one_shared_dialog {
                        vec![DialogId::new(input.i64_()?)]
                    } else {
                        let len = input.i32_()?;
                        if len < 0 {
                            return Err("Wrong vector length");
                        }
                        let mut ids = Vec::with_capacity(len as usize);
                        for _ in 0..len {
                            ids.push(DialogId::new(input.i64_()?));
                        }
                        if ids.len() > 1
                            && ids
                                .iter()
                                .any(|dialog_id| dialog_id.get_type() != DialogType::User)
                        {
                            return Ok(unsupported_content());
                        }
                        ids
                    };
                    if shared_dialog_ids.is_empty() || !shared_dialog_ids[0].is_valid() {
                        return Ok(unsupported_content());
                    }
                    let button_id = input.i32_()?;
                    Ok(MessageContent::RequestedDialog(MessageRequestedDialog {
                        shared_dialog_ids,
                        button_id,
                    }))
                }
                MessageContentType::WebViewWriteAccessAllowed => Ok(
                    MessageContent::WebViewWriteAccessAllowed(MessageWebViewWriteAccessAllowed {
                        web_app: parse_web_app(ctx, input)?,
                    }),
                ),
                MessageContentType::SetBackground => {
                    let (has_message_id, for_both) = {
                        let flags = input.parse_flags::<2>()?;
                        (flags[0], flags[1])
                    };
                    let old_message_id = if has_message_id {
                        parse_message_id(input)?
                    } else {
                        MessageId::default()
                    };
                    let background_id = ctx.parse_background_id(input)?;
                    let background_type = parse_background_type(input)?;
                    Ok(MessageContent::SetBackground(MessageSetBackground {
                        old_message_id,
                        background_info: BackgroundInfo {
                            background_id,
                            background_type,
                        },
                        for_both,
                    }))
                }
                MessageContentType::Story => {
                    let via_mention = input.parse_flags::<1>()?[0];
                    let dialog_id = DialogId::new(input.i64_()?);
                    let story_id = StoryId::new(input.i32_()?);
                    let story_full_id = StoryFullId::new(dialog_id, story_id);
                    if !story_full_id.is_server() {
                        return Ok(unsupported_content());
                    }
                    Ok(MessageContent::Story(MessageStory {
                        story_full_id,
                        via_mention,
                    }))
                }
                MessageContentType::WriteAccessAllowedByRequest => Ok(
                    MessageContent::WriteAccessAllowedByRequest(MessageWriteAccessAllowedByRequest),
                ),
                MessageContentType::GiftCode => {
                    let flags = input.parse_flags::<9>()?;
                    let (
                        via_giveaway,
                        has_creator_dialog_id,
                        is_unclaimed,
                        has_currency,
                        has_amount,
                        has_crypto_currency,
                        has_crypto_amount,
                        has_text,
                        has_days,
                    ) = (
                        flags[0], flags[1], flags[2], flags[3], flags[4], flags[5], flags[6],
                        flags[7], flags[8],
                    );
                    let creator_dialog_id = if has_creator_dialog_id {
                        DialogId::new(input.i64_()?)
                    } else {
                        DialogId::default()
                    };
                    let mut days = input.i32_()?;
                    if !has_days {
                        days = get_premium_duration_day_count(days);
                    }
                    let code = input.string()?;
                    let currency = if has_currency {
                        input.string()?
                    } else {
                        String::new()
                    };
                    let amount = if has_amount { input.i64_()? } else { 0 };
                    let crypto_currency = if has_crypto_currency {
                        input.string()?
                    } else {
                        String::new()
                    };
                    let crypto_amount = if has_crypto_amount { input.i64_()? } else { 0 };
                    let text = if has_text {
                        FormattedText::parse(input)?
                    } else {
                        FormattedText::default()
                    };
                    Ok(MessageContent::GiftCode(MessageGiftCode {
                        creator_dialog_id,
                        text,
                        days,
                        currency,
                        amount,
                        crypto_currency,
                        crypto_amount,
                        via_giveaway,
                        is_unclaimed,
                        code,
                    }))
                }
                MessageContentType::Giveaway => {
                    let has_star_count = input.parse_flags::<1>()?[0];
                    let giveaway_parameters = parse_giveaway_parameters(input)?;
                    let quantity = input.i32_()?;
                    let months = input.i32_()?;
                    let star_count = if has_star_count { input.i64_()? } else { 0 };
                    if !giveaway_parameters.is_valid() {
                        return Ok(unsupported_content());
                    }
                    Ok(MessageContent::Giveaway(MessageGiveaway {
                        giveaway_parameters,
                        quantity,
                        months,
                        star_count,
                    }))
                }
                MessageContentType::GiveawayLaunch => {
                    let has_star_count = if input.version() >= version::SUPPORT_STAR_GIVEAWAYS {
                        input.parse_flags::<1>()?[0]
                    } else {
                        false
                    };
                    let star_count = if has_star_count { input.i64_()? } else { 0 };
                    Ok(MessageContent::GiveawayLaunch(MessageGiveawayLaunch {
                        star_count,
                    }))
                }
                MessageContentType::GiveawayResults => {
                    let (
                        has_winner_count,
                        has_unclaimed_count,
                        has_giveaway_message_id,
                        is_star_giveaway,
                    ) = {
                        let flags = input.parse_flags::<4>()?;
                        (flags[0], flags[1], flags[2], flags[3])
                    };
                    let winner_count = if has_winner_count { input.i32_()? } else { 0 };
                    let unclaimed_count = if has_unclaimed_count {
                        input.i32_()?
                    } else {
                        0
                    };
                    let giveaway_message_id = if has_giveaway_message_id {
                        parse_message_id(input)?
                    } else {
                        MessageId::default()
                    };
                    if winner_count < 0 || unclaimed_count < 0 {
                        return Ok(unsupported_content());
                    }
                    Ok(MessageContent::GiveawayResults(MessageGiveawayResults {
                        giveaway_message_id,
                        winner_count,
                        unclaimed_count,
                        is_star_giveaway,
                    }))
                }
                MessageContentType::GiveawayWinners => {
                    let flags = input.parse_flags::<12>()?;
                    let (
                        only_new_subscribers,
                        was_refunded,
                        has_giveaway_message_id,
                        has_boosted_channel_id,
                        has_additional_dialog_count,
                        has_month_count,
                        has_prize_description,
                        has_winners_selection_date,
                        has_winner_count,
                        has_unclaimed_count,
                        has_winner_user_ids,
                        has_star_count,
                    ) = (
                        flags[0], flags[1], flags[2], flags[3], flags[4], flags[5], flags[6],
                        flags[7], flags[8], flags[9], flags[10], flags[11],
                    );
                    let giveaway_message_id = if has_giveaway_message_id {
                        parse_message_id(input)?
                    } else {
                        MessageId::default()
                    };
                    let boosted_channel_id = if has_boosted_channel_id {
                        ChannelId::new(input.i64_()?)
                    } else {
                        ChannelId::default()
                    };
                    let additional_dialog_count = if has_additional_dialog_count {
                        input.i32_()?
                    } else {
                        0
                    };
                    let month_count = if has_month_count { input.i32_()? } else { 0 };
                    let prize_description = if has_prize_description {
                        input.string()?
                    } else {
                        String::new()
                    };
                    let winners_selection_date = if has_winners_selection_date {
                        input.i32_()?
                    } else {
                        0
                    };
                    let winner_count = if has_winner_count { input.i32_()? } else { 0 };
                    let unclaimed_count = if has_unclaimed_count {
                        input.i32_()?
                    } else {
                        0
                    };
                    let winner_user_ids = if has_winner_user_ids {
                        let len = input.i32_()?;
                        if len < 0 {
                            return Err("Wrong vector length");
                        }
                        let mut ids = Vec::with_capacity(len as usize);
                        for _ in 0..len {
                            ids.push(UserId::new(input.i64_()?));
                        }
                        ids
                    } else {
                        Vec::new()
                    };
                    let star_count = if has_star_count { input.i64_()? } else { 0 };
                    if winner_count < 0 || unclaimed_count < 0 {
                        return Ok(unsupported_content());
                    }
                    Ok(MessageContent::GiveawayWinners(MessageGiveawayWinners {
                        giveaway_message_id,
                        boosted_channel_id,
                        additional_dialog_count,
                        month_count,
                        star_count,
                        prize_description,
                        winners_selection_date,
                        only_new_subscribers,
                        was_refunded,
                        winner_count,
                        unclaimed_count,
                        winner_user_ids,
                    }))
                }
                MessageContentType::ExpiredVideoNote => {
                    Ok(MessageContent::ExpiredVideoNote(MessageExpiredVideoNote))
                }
                MessageContentType::ExpiredVoiceNote => {
                    Ok(MessageContent::ExpiredVoiceNote(MessageExpiredVoiceNote))
                }
                MessageContentType::BoostApply => {
                    input.parse_flags::<0>()?;
                    let boost_count = input.i32_()?;
                    if boost_count < 0 {
                        return Ok(unsupported_content());
                    }
                    Ok(MessageContent::BoostApply(MessageBoostApply {
                        boost_count,
                    }))
                }
                MessageContentType::DialogShared => {
                    input.parse_flags::<0>()?;
                    let len = input.i32_()?;
                    if len < 0 {
                        return Err("Wrong vector length");
                    }
                    let mut shared_dialogs = Vec::with_capacity(len as usize);
                    for _ in 0..len {
                        shared_dialogs.push(parse_shared_dialog(ctx, input)?);
                    }
                    if shared_dialogs.is_empty()
                        || shared_dialogs
                            .iter()
                            .any(|shared_dialog| !shared_dialog.is_valid())
                    {
                        return Ok(unsupported_content());
                    }
                    let button_id = input.i32_()?;
                    Ok(MessageContent::DialogShared(MessageDialogShared {
                        shared_dialogs,
                        button_id,
                    }))
                }
                MessageContentType::PaidMedia => {
                    let (has_caption, has_payload) = {
                        let flags = input.parse_flags::<2>()?;
                        (flags[0], flags[1])
                    };
                    let len = input.i32_()?;
                    if len < 0 {
                        return Err("Wrong vector length");
                    }
                    let mut media = Vec::with_capacity(len as usize);
                    for _ in 0..len {
                        media.push(parse_message_extended_media(ctx, input)?);
                    }
                    let caption = if has_caption {
                        parse_caption(input)?
                    } else {
                        FormattedText::default()
                    };
                    let star_count = input.i64_()?;
                    let payload = if has_payload {
                        input.string()?
                    } else {
                        String::new()
                    };
                    if media.iter().any(|extended_media| extended_media.is_empty()) {
                        return Ok(unsupported_content());
                    }
                    Ok(MessageContent::PaidMedia(MessagePaidMedia {
                        media,
                        caption,
                        star_count,
                        payload,
                    }))
                }
                MessageContentType::PaymentRefunded => {
                    let has_invoice_payload = input.parse_flags::<1>()?[0];
                    let dialog_id = DialogId::new(input.i64_()?);
                    let currency = input.string()?;
                    let total_amount = input.i64_()?;
                    let invoice_payload = if has_invoice_payload {
                        input.string()?
                    } else {
                        String::new()
                    };
                    let telegram_payment_charge_id = input.string()?;
                    let provider_payment_charge_id = input.string()?;
                    Ok(MessageContent::PaymentRefunded(MessagePaymentRefunded {
                        dialog_id,
                        currency,
                        total_amount,
                        invoice_payload,
                        telegram_payment_charge_id,
                        provider_payment_charge_id,
                    }))
                }
                MessageContentType::GiftStars => {
                    let (has_crypto_amount, has_transaction_id) = {
                        let flags = input.parse_flags::<2>()?;
                        (flags[0], flags[1])
                    };
                    let currency = input.string()?;
                    let amount = input.i64_()?;
                    let star_count = input.i64_()?;
                    let (crypto_currency, crypto_amount) = if has_crypto_amount {
                        (input.string()?, input.i64_()?)
                    } else {
                        (String::new(), 0)
                    };
                    let transaction_id = if has_transaction_id {
                        input.string()?
                    } else {
                        String::new()
                    };
                    Ok(MessageContent::GiftStars(MessageGiftStars {
                        currency,
                        amount,
                        crypto_currency,
                        crypto_amount,
                        star_count,
                        transaction_id,
                    }))
                }
                MessageContentType::PrizeStars => {
                    let is_unclaimed = input.parse_flags::<1>()?[0];
                    let star_count = input.i64_()?;
                    let transaction_id = input.string()?;
                    let boosted_dialog_id = DialogId::new(input.i64_()?);
                    let giveaway_message_id = parse_message_id(input)?;
                    Ok(MessageContent::PrizeStars(MessagePrizeStars {
                        star_count,
                        transaction_id,
                        boosted_dialog_id,
                        giveaway_message_id,
                        is_unclaimed,
                    }))
                }
                MessageContentType::StarGift
                | MessageContentType::StarGiftUnique
                | MessageContentType::StarGiftPurchaseOffer
                | MessageContentType::StarGiftPurchaseOfferDeclined => {
                    Err("[deferred-variant] StarGift family")
                }
                MessageContentType::PaidMessagesRefunded => {
                    input.parse_flags::<0>()?;
                    let message_count = input.i32_()?;
                    let star_count = input.i64_()?;
                    Ok(MessageContent::PaidMessagesRefunded(
                        MessagePaidMessagesRefunded {
                            message_count,
                            star_count,
                        },
                    ))
                }
                MessageContentType::PaidMessagesPrice => {
                    let broadcast_messages_allowed = input.parse_flags::<1>()?[0];
                    let star_count = input.i64_()?;
                    Ok(MessageContent::PaidMessagesPrice(
                        MessagePaidMessagesPrice {
                            star_count,
                            broadcast_messages_allowed,
                        },
                    ))
                }
                MessageContentType::ConferenceCall => {
                    let (
                        is_active,
                        is_video,
                        was_missed,
                        has_duration,
                        has_other_participant_dialog_ids,
                    ) = {
                        let flags = input.parse_flags::<5>()?;
                        (flags[0], flags[1], flags[2], flags[3], flags[4])
                    };
                    let call_id = input.i64_()?;
                    let duration = if has_duration { input.i32_()? } else { 0 };
                    let other_participant_dialog_ids = if has_other_participant_dialog_ids {
                        let len = input.i32_()?;
                        if len < 0 {
                            return Err("Wrong vector length");
                        }
                        let mut ids = Vec::with_capacity(len as usize);
                        for _ in 0..len {
                            ids.push(DialogId::new(input.i64_()?));
                        }
                        ids
                    } else {
                        Vec::new()
                    };
                    Ok(MessageContent::ConferenceCall(MessageConferenceCall {
                        call_id,
                        duration,
                        other_participant_dialog_ids,
                        is_active,
                        is_video,
                        was_missed,
                    }))
                }
                MessageContentType::ToDoList => {
                    input.parse_flags::<0>()?;
                    let list = parse_to_do_list(input)?;
                    let len = input.i32_()?;
                    if len < 0 {
                        return Err("Wrong vector length");
                    }
                    let mut completions = Vec::with_capacity(len as usize);
                    for _ in 0..len {
                        completions.push(parse_to_do_completion(input)?);
                    }
                    Ok(MessageContent::ToDoList(MessageToDoList {
                        list,
                        completions,
                    }))
                }
                MessageContentType::TodoCompletions => {
                    let (has_to_do_message_id, has_completed_task_ids, has_incompleted_task_ids) = {
                        let flags = input.parse_flags::<3>()?;
                        (flags[0], flags[1], flags[2])
                    };
                    let to_do_message_id = if has_to_do_message_id {
                        parse_message_id(input)?
                    } else {
                        MessageId::default()
                    };
                    let completed_task_ids = if has_completed_task_ids {
                        input.i32_vec()?
                    } else {
                        Vec::new()
                    };
                    let incompleted_task_ids = if has_incompleted_task_ids {
                        input.i32_vec()?
                    } else {
                        Vec::new()
                    };
                    Ok(MessageContent::TodoCompletions(MessageTodoCompletions {
                        to_do_message_id,
                        completed_task_ids,
                        incompleted_task_ids,
                    }))
                }
                MessageContentType::TodoAppendTasks => {
                    let (has_to_do_message_id, has_items) = {
                        let flags = input.parse_flags::<2>()?;
                        (flags[0], flags[1])
                    };
                    let to_do_message_id = if has_to_do_message_id {
                        parse_message_id(input)?
                    } else {
                        MessageId::default()
                    };
                    let items = if has_items {
                        let len = input.i32_()?;
                        if len < 0 {
                            return Err("Wrong vector length");
                        }
                        let mut result = Vec::with_capacity(len as usize);
                        for _ in 0..len {
                            result.push(parse_to_do_item(input)?);
                        }
                        result
                    } else {
                        Vec::new()
                    };
                    Ok(MessageContent::TodoAppendTasks(MessageTodoAppendTasks {
                        to_do_message_id,
                        items,
                    }))
                }
                MessageContentType::GiftTon => {
                    let has_transaction_id = input.parse_flags::<1>()?[0];
                    let currency = input.string()?;
                    let amount = input.i64_()?;
                    let crypto_currency = input.string()?;
                    let crypto_amount = input.i64_()?;
                    let transaction_id = if has_transaction_id {
                        input.string()?
                    } else {
                        String::new()
                    };
                    Ok(MessageContent::GiftTon(MessageGiftTon {
                        currency,
                        amount,
                        crypto_currency,
                        crypto_amount,
                        transaction_id,
                    }))
                }
                MessageContentType::SuggestedPostSuccess => {
                    let (has_suggested_post_message_id, has_amount) = {
                        let flags = input.parse_flags::<2>()?;
                        (flags[0], flags[1])
                    };
                    let suggested_post_message_id = if has_suggested_post_message_id {
                        parse_message_id(input)?
                    } else {
                        MessageId::default()
                    };
                    let amount = if has_amount {
                        parse_currency_amount(input)?
                    } else {
                        CurrencyAmount::default()
                    };
                    Ok(MessageContent::SuggestedPostSuccess(
                        MessageSuggestedPostSuccess {
                            suggested_post_message_id,
                            amount,
                        },
                    ))
                }
                MessageContentType::SuggestedPostRefund => {
                    let (has_suggested_post_message_id, is_payer_initiated) = {
                        let flags = input.parse_flags::<2>()?;
                        (flags[0], flags[1])
                    };
                    let suggested_post_message_id = if has_suggested_post_message_id {
                        parse_message_id(input)?
                    } else {
                        MessageId::default()
                    };
                    Ok(MessageContent::SuggestedPostRefund(
                        MessageSuggestedPostRefund {
                            suggested_post_message_id,
                            is_payer_initiated,
                        },
                    ))
                }
                MessageContentType::SuggestedPostApproval => {
                    let (
                        has_suggested_post_message_id,
                        has_price,
                        has_comment,
                        has_send_date,
                        is_rejected,
                        is_balance_too_low,
                    ) = {
                        let flags = input.parse_flags::<6>()?;
                        (flags[0], flags[1], flags[2], flags[3], flags[4], flags[5])
                    };
                    let suggested_post_message_id = if has_suggested_post_message_id {
                        parse_message_id(input)?
                    } else {
                        MessageId::default()
                    };
                    let price = if has_price {
                        parse_suggested_post_price(input)?
                    } else {
                        SuggestedPostPrice::default()
                    };
                    let comment = if has_comment {
                        input.string()?
                    } else {
                        String::new()
                    };
                    let send_date = if has_send_date { input.i32_()? } else { 0 };
                    Ok(MessageContent::SuggestedPostApproval(
                        MessageSuggestedPostApproval {
                            suggested_post_message_id,
                            price,
                            comment,
                            send_date,
                            is_rejected,
                            is_balance_too_low,
                        },
                    ))
                }
                MessageContentType::SuggestBirthday => {
                    input.parse_flags::<0>()?;
                    let birthdate = Birthdate::parse_from(input)?;
                    Ok(MessageContent::SuggestBirthday(MessageSuggestBirthday {
                        birthdate,
                    }))
                }
                MessageContentType::NewCreatorPending => {
                    let has_new_creator_user_id = input.parse_flags::<1>()?[0];
                    let new_creator_user_id = if has_new_creator_user_id {
                        UserId::new(input.i64_()?)
                    } else {
                        UserId::default()
                    };
                    Ok(MessageContent::NewCreatorPending(
                        MessageNewCreatorPending {
                            new_creator_user_id,
                        },
                    ))
                }
                MessageContentType::ChangeCreator => {
                    input.parse_flags::<0>()?;
                    let new_creator_user_id = UserId::new(input.i64_()?);
                    if !new_creator_user_id.is_valid() {
                        return Ok(unsupported_content());
                    }
                    Ok(MessageContent::ChangeCreator(MessageChangeCreator {
                        new_creator_user_id,
                    }))
                }
                MessageContentType::NoForwardsToggle => {
                    let (prev_value, new_value, has_request_message_id) = {
                        let flags = input.parse_flags::<3>()?;
                        (flags[0], flags[1], flags[2])
                    };
                    let request_message_id = if has_request_message_id {
                        parse_message_id(input)?
                    } else {
                        MessageId::default()
                    };
                    Ok(MessageContent::NoForwardsToggle(MessageNoForwardsToggle {
                        request_message_id,
                        prev_value,
                        new_value,
                    }))
                }
                MessageContentType::NoForwardsRequest => {
                    let (is_expired, prev_value, new_value) = {
                        let flags = input.parse_flags::<3>()?;
                        (flags[0], flags[1], flags[2])
                    };
                    Ok(MessageContent::NoForwardsRequest(
                        MessageNoForwardsRequest {
                            is_expired,
                            prev_value,
                            new_value,
                        },
                    ))
                }
                MessageContentType::ManagedBotCreated => {
                    input.parse_flags::<0>()?;
                    let bot_user_id = UserId::new(input.i64_()?);
                    if !bot_user_id.is_valid() {
                        return Ok(unsupported_content());
                    }
                    Ok(MessageContent::ManagedBotCreated(
                        MessageManagedBotCreated { bot_user_id },
                    ))
                }
                MessageContentType::PollAppendAnswer => {
                    let has_poll_message_id = input.parse_flags::<1>()?[0];
                    let poll_message_id = if has_poll_message_id {
                        parse_message_id(input)?
                    } else {
                        MessageId::default()
                    };
                    let text = FormattedText::parse(input)?;
                    let data = input.string()?;
                    Ok(MessageContent::PollAppendAnswer(MessagePollAppendAnswer {
                        poll_message_id,
                        text,
                        data,
                    }))
                }
                MessageContentType::PollDeleteAnswer => {
                    let has_poll_message_id = input.parse_flags::<1>()?[0];
                    let poll_message_id = if has_poll_message_id {
                        parse_message_id(input)?
                    } else {
                        MessageId::default()
                    };
                    let text = FormattedText::parse(input)?;
                    let data = input.string()?;
                    Ok(MessageContent::PollDeleteAnswer(MessagePollDeleteAnswer {
                        poll_message_id,
                        text,
                        data,
                    }))
                }
                MessageContentType::RichText => Err("[deferred-variant] MessageRichText"),
                MessageContentType::ChangeCommunity => {
                    let has_community_id = input.parse_flags::<1>()?[0];
                    let community_id = if has_community_id {
                        CommunityId::new(input.i64_()?)
                    } else {
                        CommunityId::default()
                    };
                    Ok(MessageContent::ChangeCommunity(MessageChangeCommunity {
                        community_id,
                    }))
                }
                MessageContentType::ChatJoinedViaCommunity => {
                    input.parse_flags::<0>()?;
                    let community_id = CommunityId::new(input.i64_()?);
                    Ok(MessageContent::ChatJoinedViaCommunity(
                        MessageChatJoinedViaCommunity { community_id },
                    ))
                }
                MessageContentType::None => {
                    tdutils::log_error!("Load a message with an invalid content of type {}", raw);
                    Ok(unsupported_content())
                }
            }
        })();
    parsed.unwrap_or_else(|_| unsupported_content())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store_parse::{Parser, store_i32, store_log_event_version};

    /// The seam stand-in: every manager record becomes a tag word plus
    /// the plain file-id pair, so the dispatcher plumbing round-trips
    /// byte-exactly while the manager slices are pending.
    struct RoundTripContext;

    const TAG_ANIMATION: i32 = 1;
    const TAG_AUDIO: i32 = 2;
    const TAG_DOCUMENT: i32 = 3;
    const TAG_STICKER: i32 = 4;
    const TAG_VIDEO: i32 = 5;
    const TAG_VIDEO_NOTE: i32 = 6;
    const TAG_VOICE_NOTE: i32 = 7;
    const TAG_FILE: i32 = 8;
    const TAG_BACKGROUND: i32 = 9;

    impl RoundTripContext {
        fn write_tagged(tag: i32, file_id: FileId, out: &mut Vec<u8>) {
            store_i32(out, tag);
            store_i32(out, file_id.get());
            store_i32(out, file_id.get_remote());
        }

        fn read_tagged(expected: i32, input: &mut Parser) -> Result<FileId, &'static str> {
            let tag = input.i32_()?;
            if tag != expected {
                return Err("wrong manager record tag");
            }
            let id = input.i32_()?;
            let remote_id = input.i32_()?;
            Ok(FileId::new(id, remote_id))
        }
    }

    impl ContentStoreContext for RoundTripContext {
        fn store_animation(&mut self, file_id: FileId, out: &mut Vec<u8>) {
            Self::write_tagged(TAG_ANIMATION, file_id, out);
        }
        fn parse_animation(&mut self, input: &mut Parser) -> Result<FileId, &'static str> {
            Self::read_tagged(TAG_ANIMATION, input)
        }
        fn store_audio(&mut self, file_id: FileId, out: &mut Vec<u8>) {
            Self::write_tagged(TAG_AUDIO, file_id, out);
        }
        fn parse_audio(&mut self, input: &mut Parser) -> Result<FileId, &'static str> {
            Self::read_tagged(TAG_AUDIO, input)
        }
        fn store_document(&mut self, file_id: FileId, out: &mut Vec<u8>) {
            Self::write_tagged(TAG_DOCUMENT, file_id, out);
        }
        fn parse_document(&mut self, input: &mut Parser) -> Result<FileId, &'static str> {
            Self::read_tagged(TAG_DOCUMENT, input)
        }
        fn store_sticker(&mut self, file_id: FileId, _in_sticker_set: bool, out: &mut Vec<u8>) {
            Self::write_tagged(TAG_STICKER, file_id, out);
        }
        fn parse_sticker(
            &mut self,
            _in_sticker_set: bool,
            input: &mut Parser,
        ) -> Result<FileId, &'static str> {
            Self::read_tagged(TAG_STICKER, input)
        }
        fn store_video(&mut self, file_id: FileId, out: &mut Vec<u8>) {
            Self::write_tagged(TAG_VIDEO, file_id, out);
        }
        fn parse_video(&mut self, input: &mut Parser) -> Result<FileId, &'static str> {
            Self::read_tagged(TAG_VIDEO, input)
        }
        fn store_video_note(&mut self, file_id: FileId, out: &mut Vec<u8>) {
            Self::write_tagged(TAG_VIDEO_NOTE, file_id, out);
        }
        fn parse_video_note(&mut self, input: &mut Parser) -> Result<FileId, &'static str> {
            Self::read_tagged(TAG_VIDEO_NOTE, input)
        }
        fn store_voice_note(&mut self, file_id: FileId, out: &mut Vec<u8>) {
            Self::write_tagged(TAG_VOICE_NOTE, file_id, out);
        }
        fn parse_voice_note(&mut self, input: &mut Parser) -> Result<FileId, &'static str> {
            Self::read_tagged(TAG_VOICE_NOTE, input)
        }
        fn store_file(&mut self, file_id: FileId, out: &mut Vec<u8>) {
            Self::write_tagged(TAG_FILE, file_id, out);
        }
        fn parse_file(&mut self, input: &mut Parser) -> Result<FileId, &'static str> {
            Self::read_tagged(TAG_FILE, input)
        }
        fn store_background_id(&mut self, background_id: BackgroundId, out: &mut Vec<u8>) {
            store_i32(out, TAG_BACKGROUND);
            store_i64(out, background_id.get());
        }
        fn parse_background_id(
            &mut self,
            input: &mut Parser,
        ) -> Result<BackgroundId, &'static str> {
            let tag = input.i32_()?;
            if tag != TAG_BACKGROUND {
                return Err("wrong manager record tag");
            }
            Ok(BackgroundId::new(input.i64_()?))
        }
    }

    fn round_trip(content: &MessageContent) -> MessageContent {
        let mut bytes = Vec::new();
        store_log_event_version(&mut bytes);
        let mut ctx = RoundTripContext;
        store_message_content(&mut ctx, content, &mut bytes);
        let mut parser = Parser::new_log_event(&bytes).expect("version prefix");
        let parsed = parse_message_content(&mut ctx, &mut parser);
        parser.fetch_end().expect("whole record consumed");
        parsed
    }

    fn assert_round_trips(content: MessageContent) {
        let expected_type = content.get_type();
        let reparsed = round_trip(&content);
        assert_eq!(
            reparsed.get_type(),
            expected_type,
            "type lost for {content:?}"
        );
        assert_eq!(reparsed, content, "payload mismatch for {content:?}");
    }

    #[test]
    fn service_contents_round_trip() {
        assert_round_trips(MessageContent::ChatDeletePhoto(MessageChatDeletePhoto));
        assert_round_trips(MessageContent::ChatDeleteHistory(MessageChatDeleteHistory));
        assert_round_trips(MessageContent::ScreenshotTaken(MessageScreenshotTaken));
        assert_round_trips(MessageContent::ContactRegistered(MessageContactRegistered));
        assert_round_trips(MessageContent::ExpiredPhoto(MessageExpiredPhoto));
        assert_round_trips(MessageContent::ExpiredVideo(MessageExpiredVideo));
        assert_round_trips(MessageContent::ExpiredVideoNote(MessageExpiredVideoNote));
        assert_round_trips(MessageContent::ExpiredVoiceNote(MessageExpiredVoiceNote));
        assert_round_trips(MessageContent::WriteAccessAllowed(
            MessageWriteAccessAllowed,
        ));
        assert_round_trips(MessageContent::WriteAccessAllowedByRequest(
            MessageWriteAccessAllowedByRequest,
        ));
        assert_round_trips(MessageContent::ChatJoinedByLink(MessageChatJoinedByLink {
            is_approved: true,
        }));
        assert_round_trips(MessageContent::ChatChangeTitle(MessageChatChangeTitle {
            title: "title".to_string(),
        }));
        assert_round_trips(MessageContent::ChatAddUsers(MessageChatAddUsers {
            user_ids: vec![UserId::new(1), UserId::new(2)],
        }));
        assert_round_trips(MessageContent::ChatDeleteUser(MessageChatDeleteUser {
            user_id: UserId::new(3),
        }));
        assert_round_trips(MessageContent::ChatMigrateTo(MessageChatMigrateTo {
            migrated_to_channel_id: ChannelId::new(4),
        }));
        assert_round_trips(MessageContent::ChannelCreate(MessageChannelCreate {
            title: "channel".to_string(),
        }));
        assert_round_trips(MessageContent::ChannelMigrateFrom(
            MessageChannelMigrateFrom {
                title: "chat".to_string(),
                migrated_from_chat_id: ChatId::new(5),
            },
        ));
        assert_round_trips(MessageContent::PinMessage(MessagePinMessage {
            message_id: MessageId::new(6),
        }));
        assert_round_trips(MessageContent::GameScore(MessageGameScore {
            game_message_id: MessageId::new(7),
            game_id: 8,
            score: 9,
        }));
        assert_round_trips(MessageContent::ChatSetTtl(MessageChatSetTtl {
            ttl: 3600,
            from_user_id: UserId::new(10),
        }));
        assert_round_trips(MessageContent::Call(MessageCall {
            call_id: 11,
            call_access_hash: 12,
            duration: 60,
            discard_reason: CallDiscardReason {
                r#type: CallDiscardReasonType::HungUp,
                slug: String::new(),
            },
            is_video: true,
        }));
        assert_round_trips(MessageContent::CustomServiceAction(
            MessageCustomServiceAction {
                message: "custom".to_string(),
            },
        ));
        assert_round_trips(MessageContent::WebsiteConnected(MessageWebsiteConnected {
            domain_name: "telegram.org".to_string(),
        }));
        assert_round_trips(MessageContent::WebViewDataSent(MessageWebViewDataSent {
            button_text: "button".to_string(),
        }));
        assert_round_trips(MessageContent::WebViewDataReceived(
            MessageWebViewDataReceived {
                button_text: "button".to_string(),
                data: "data".to_string(),
            },
        ));
        assert_round_trips(MessageContent::BoostApply(MessageBoostApply {
            boost_count: 3,
        }));
        assert_round_trips(MessageContent::PaidMessagesRefunded(
            MessagePaidMessagesRefunded {
                message_count: 4,
                star_count: 100,
            },
        ));
        assert_round_trips(MessageContent::PaidMessagesPrice(
            MessagePaidMessagesPrice {
                star_count: 25,
                broadcast_messages_allowed: true,
            },
        ));
        assert_round_trips(MessageContent::ConferenceCall(MessageConferenceCall {
            call_id: 13,
            duration: 600,
            other_participant_dialog_ids: vec![DialogId::new(14), DialogId::new(15)],
            is_active: true,
            is_video: true,
            was_missed: false,
        }));
        assert_round_trips(MessageContent::TodoCompletions(MessageTodoCompletions {
            to_do_message_id: MessageId::new(34),
            completed_task_ids: vec![1, 2],
            incompleted_task_ids: vec![3],
        }));
        assert_round_trips(MessageContent::TodoAppendTasks(MessageTodoAppendTasks {
            to_do_message_id: MessageId::new(17),
            items: vec![ToDoItem {
                id: 1,
                title: FormattedText::default(),
            }],
        }));
        assert_round_trips(MessageContent::GiftTon(MessageGiftTon {
            currency: "USD".to_string(),
            amount: 5,
            crypto_currency: "TON".to_string(),
            crypto_amount: 50_000_000,
            transaction_id: "tx".to_string(),
        }));
        assert_round_trips(MessageContent::GiftStars(MessageGiftStars {
            currency: "USD".to_string(),
            amount: 5,
            crypto_currency: String::new(),
            crypto_amount: 0,
            star_count: 250,
            transaction_id: "tx".to_string(),
        }));
        assert_round_trips(MessageContent::PrizeStars(MessagePrizeStars {
            star_count: 100,
            transaction_id: "tx".to_string(),
            boosted_dialog_id: DialogId::new(18),
            giveaway_message_id: MessageId::new(19),
            is_unclaimed: true,
        }));
        assert_round_trips(MessageContent::PaymentRefunded(MessagePaymentRefunded {
            dialog_id: DialogId::new(20),
            currency: "USD".to_string(),
            total_amount: 10,
            invoice_payload: "payload".to_string(),
            telegram_payment_charge_id: "tg".to_string(),
            provider_payment_charge_id: "provider".to_string(),
        }));
        assert_round_trips(MessageContent::NewCreatorPending(
            MessageNewCreatorPending {
                new_creator_user_id: UserId::new(21),
            },
        ));
        assert_round_trips(MessageContent::ChangeCreator(MessageChangeCreator {
            new_creator_user_id: UserId::new(22),
        }));
        assert_round_trips(MessageContent::NoForwardsToggle(MessageNoForwardsToggle {
            request_message_id: MessageId::new(98),
            prev_value: false,
            new_value: true,
        }));
        assert_round_trips(MessageContent::NoForwardsRequest(
            MessageNoForwardsRequest {
                is_expired: true,
                prev_value: false,
                new_value: true,
            },
        ));
        assert_round_trips(MessageContent::ManagedBotCreated(
            MessageManagedBotCreated {
                bot_user_id: UserId::new(24),
            },
        ));
        assert_round_trips(MessageContent::PollAppendAnswer(MessagePollAppendAnswer {
            poll_message_id: MessageId::new(25),
            text: FormattedText::default(),
            data: "data".to_string(),
        }));
        assert_round_trips(MessageContent::PollDeleteAnswer(MessagePollDeleteAnswer {
            poll_message_id: MessageId::new(26),
            text: FormattedText::default(),
            data: "data".to_string(),
        }));
        assert_round_trips(MessageContent::ChangeCommunity(MessageChangeCommunity {
            community_id: CommunityId::new(27),
        }));
        assert_round_trips(MessageContent::ChatJoinedViaCommunity(
            MessageChatJoinedViaCommunity {
                community_id: CommunityId::new(28),
            },
        ));
        assert_round_trips(MessageContent::Unsupported(MessageUnsupported {
            version: 5,
        }));
    }

    #[test]
    fn text_and_dice_round_trip() {
        assert_round_trips(MessageContent::Text(MessageText::new(
            FormattedText {
                text: "hello".to_string(),
                entities: Vec::new(),
            },
            WebPageId::new(42),
            false,
            false,
            false,
            "https://telegram.org".to_string(),
        )));
        assert_round_trips(MessageContent::Dice(MessageDice::new(
            "",
            3,
            false,
            String::new(),
            String::new(),
            0,
            0,
        )));
        assert_round_trips(MessageContent::Dice(MessageDice::new(
            MessageDice::DEFAULT_EMOJI,
            0,
            true,
            "a".repeat(32),
            "state".to_string(),
            100,
            200,
        )));
    }

    #[test]
    fn location_plane_round_trip() {
        assert_round_trips(MessageContent::Location(MessageLocation {
            location: Location::default(),
        }));
        assert_round_trips(MessageContent::Venue(MessageVenue {
            venue: Venue {
                location: Location::default(),
                title: "title".to_string(),
                address: "address".to_string(),
                provider: "provider".to_string(),
                id: "id".to_string(),
                r#type: "type".to_string(),
            },
        }));
        assert_round_trips(MessageContent::LiveLocation(MessageLiveLocation::new(
            Location::default(),
            900,
            180,
            50,
        )));
    }

    #[test]
    fn payment_successful_round_trip() {
        assert_round_trips(MessageContent::PaymentSuccessful(
            MessagePaymentSuccessful {
                invoice_dialog_id: DialogId::new(30),
                invoice_message_id: MessageId::new(74),
                currency: "USD".to_string(),
                total_amount: 100,
                invoice_payload: "payload".to_string(),
                subscription_until_date: 0,
                is_recurring: false,
                is_first_recurring: false,
                shipping_option_id: "option".to_string(),
                order_info: Some(OrderInfo {
                    name: "name".to_string(),
                    phone_number: String::new(),
                    email_address: String::new(),
                    shipping_address: Some(Box::new(Address {
                        country_code: "US".to_string(),
                        state: String::new(),
                        city: "city".to_string(),
                        street_line1: String::new(),
                        street_line2: String::new(),
                        postal_code: String::new(),
                    })),
                }),
                telegram_payment_charge_id: "tg".to_string(),
                provider_payment_charge_id: "provider".to_string(),
            },
        ));
    }

    #[test]
    fn poll_recursion_round_trip() {
        let attached = MessageContent::Photo(MessagePhoto {
            photo: photo_with_size(),
            video_file_id: FileId::new(9, 0),
            caption: FormattedText::default(),
            has_spoiler: false,
        });
        assert_round_trips(MessageContent::Poll(MessagePoll {
            poll_id: PollId::new(100),
            caption: FormattedText {
                text: "caption".to_string(),
                entities: Vec::new(),
            },
            attached_media: Some(Box::new(attached)),
        }));
    }

    fn photo_with_size() -> Photo {
        Photo {
            id: 1,
            date: 2,
            minithumbnail: String::new(),
            photos: vec![PhotoSize {
                size_type: 'x' as i32,
                size: 100,
                dimensions: Dimensions {
                    width: 10,
                    height: 10,
                },
                file_id: FileId::new(7, 8),
                progressive_sizes: Vec::new(),
            }],
            animations: Vec::new(),
            sticker_photo_size: None,
            has_stickers: false,
            sticker_file_ids: Vec::new(),
        }
    }

    #[test]
    fn requested_dialog_shapes_round_trip() {
        assert_round_trips(MessageContent::RequestedDialog(MessageRequestedDialog {
            shared_dialog_ids: vec![DialogId::new(1000000001)],
            button_id: 1,
        }));
        assert_round_trips(MessageContent::RequestedDialog(MessageRequestedDialog {
            shared_dialog_ids: vec![
                DialogId::new(1000000001),
                DialogId::new(1000000002),
                DialogId::new(1000000003),
            ],
            button_id: 2,
        }));
    }

    #[test]
    fn giveaway_plane_round_trip() {
        let valid_parameters = GiveawayParameters {
            boosted_channel_id: ChannelId::new(40),
            additional_channel_ids: vec![ChannelId::new(41)],
            only_new_subscribers: true,
            winners_are_visible: false,
            date: 1700000000,
            country_codes: vec!["US".to_string()],
            prize_description: "prize".to_string(),
        };
        assert!(valid_parameters.is_valid());
        assert_round_trips(MessageContent::Giveaway(MessageGiveaway {
            giveaway_parameters: valid_parameters,
            quantity: 3,
            months: 5,
            star_count: 500,
        }));
        assert_round_trips(MessageContent::GiveawayLaunch(MessageGiveawayLaunch {
            star_count: 1000,
        }));
        assert_round_trips(MessageContent::GiveawayResults(MessageGiveawayResults {
            giveaway_message_id: MessageId::new(42),
            winner_count: 10,
            unclaimed_count: 2,
            is_star_giveaway: true,
        }));
        assert_round_trips(MessageContent::GiveawayWinners(MessageGiveawayWinners {
            giveaway_message_id: MessageId::new(50),
            boosted_channel_id: ChannelId::new(44),
            additional_dialog_count: 1,
            month_count: 6,
            star_count: 0,
            prize_description: "prize".to_string(),
            winners_selection_date: 1700000001,
            only_new_subscribers: true,
            was_refunded: false,
            winner_count: 5,
            unclaimed_count: 0,
            winner_user_ids: vec![UserId::new(45), UserId::new(46)],
        }));
    }

    #[test]
    fn suggested_post_family_round_trip() {
        assert_round_trips(MessageContent::SuggestedPostSuccess(
            MessageSuggestedPostSuccess {
                suggested_post_message_id: MessageId::new(50),
                amount: CurrencyAmount {
                    r#type: CurrencyAmountType::Star,
                    star_amount: StarAmount {
                        star_count: 100,
                        nanostar_count: 0,
                    },
                    ton_amount: TonAmount::default(),
                },
            },
        ));
        assert_round_trips(MessageContent::SuggestedPostRefund(
            MessageSuggestedPostRefund {
                suggested_post_message_id: MessageId::new(58),
                is_payer_initiated: true,
            },
        ));
        assert_round_trips(MessageContent::SuggestedPostApproval(
            MessageSuggestedPostApproval {
                suggested_post_message_id: MessageId::new(26),
                price: SuggestedPostPrice {
                    r#type: SuggestedPostPriceType::Ton,
                    amount: 10_000_000,
                },
                comment: "comment".to_string(),
                send_date: 1700000002,
                is_rejected: false,
                is_balance_too_low: false,
            },
        ));
    }

    #[test]
    fn gift_code_and_premium_round_trip() {
        assert_round_trips(MessageContent::GiftCode(MessageGiftCode {
            creator_dialog_id: DialogId::new(60),
            text: FormattedText::default(),
            days: 12,
            currency: "USD".to_string(),
            amount: 5,
            crypto_currency: String::new(),
            crypto_amount: 0,
            via_giveaway: false,
            is_unclaimed: true,
            code: "CODE".to_string(),
        }));
        assert_round_trips(MessageContent::GiftPremium(MessageGiftPremium {
            text: FormattedText {
                text: "gift".to_string(),
                entities: Vec::new(),
            },
            currency: "USD".to_string(),
            amount: 5,
            crypto_currency: "TON".to_string(),
            crypto_amount: 1,
            days: 3,
        }));
    }

    #[test]
    fn background_round_trip() {
        assert_round_trips(MessageContent::SetBackground(MessageSetBackground {
            old_message_id: MessageId::new(66),
            background_info: BackgroundInfo {
                background_id: BackgroundId::new(71),
                background_type: BackgroundType {
                    r#type: BackgroundTypeType::Pattern,
                    is_blurred: false,
                    is_moving: true,
                    intensity: 50,
                    fill: BackgroundFill {
                        top_color: 0x305070,
                        bottom_color: 0x907050,
                        rotation_angle: 45,
                        third_color: -1,
                        fourth_color: -1,
                    },
                    theme_name: String::new(),
                },
            },
            for_both: true,
        }));
    }

    #[test]
    fn to_do_list_round_trip() {
        assert_round_trips(MessageContent::ToDoList(MessageToDoList {
            list: ToDoList {
                title: FormattedText::default(),
                items: vec![ToDoItem {
                    id: 1,
                    title: FormattedText::default(),
                }],
                others_can_append: true,
                others_can_complete: false,
            },
            completions: vec![ToDoCompletion {
                id: 1,
                completed_by_dialog_id: DialogId::new(1000000009),
                date: 1700000003,
            }],
        }));
    }

    #[test]
    fn deferred_types_degrade_to_unsupported() {
        // raw = 8 (Contact): the deferred payload has no model, so the
        // parse degrades to the C++ is_bad fallback.
        let mut bytes = Vec::new();
        store_log_event_version(&mut bytes);
        store_i32(&mut bytes, 8);
        let mut parser = Parser::new_log_event(&bytes).expect("version prefix");
        let parsed = parse_message_content(&mut RoundTripContext, &mut parser);
        assert_eq!(parsed.get_type(), MessageContentType::Unsupported);
    }

    #[test]
    fn premium_duration_day_count_matches_cpp() {
        assert_eq!(get_premium_duration_day_count(0), 7);
        assert_eq!(get_premium_duration_day_count(-5), 7);
        assert_eq!(
            get_premium_duration_day_count(12),
            12 * 30 + 12 / 3 + 12 / 12
        );
        assert_eq!(get_premium_duration_day_count(10_000_001), 7);
    }
}

#[cfg(test)]
mod content_store_tests {
    use super::*;

    /// The full C++ discriminant order (MessageContentType.h:15-113):
    /// `None = -1`, then the content types from `Text = 0` in declaration
    /// order. Any C++ insertion re-orders everything after it and must bump
    /// `MessageUnsupported::CURRENT_VERSION`.
    #[test]
    fn content_type_discriminants_follow_the_cpp_order() {
        let all = [
            MessageContentType::None,
            MessageContentType::Text,
            MessageContentType::Animation,
            MessageContentType::Audio,
            MessageContentType::Document,
            MessageContentType::Photo,
            MessageContentType::Sticker,
            MessageContentType::Video,
            MessageContentType::VoiceNote,
            MessageContentType::Contact,
            MessageContentType::Location,
            MessageContentType::Venue,
            MessageContentType::ChatCreate,
            MessageContentType::ChatChangeTitle,
            MessageContentType::ChatChangePhoto,
            MessageContentType::ChatDeletePhoto,
            MessageContentType::ChatDeleteHistory,
            MessageContentType::ChatAddUsers,
            MessageContentType::ChatJoinedByLink,
            MessageContentType::ChatDeleteUser,
            MessageContentType::ChatMigrateTo,
            MessageContentType::ChannelCreate,
            MessageContentType::ChannelMigrateFrom,
            MessageContentType::PinMessage,
            MessageContentType::Game,
            MessageContentType::GameScore,
            MessageContentType::ScreenshotTaken,
            MessageContentType::ChatSetTtl,
            MessageContentType::Unsupported,
            MessageContentType::Call,
            MessageContentType::Invoice,
            MessageContentType::PaymentSuccessful,
            MessageContentType::VideoNote,
            MessageContentType::ContactRegistered,
            MessageContentType::ExpiredPhoto,
            MessageContentType::ExpiredVideo,
            MessageContentType::LiveLocation,
            MessageContentType::CustomServiceAction,
            MessageContentType::WebsiteConnected,
            MessageContentType::PassportDataSent,
            MessageContentType::PassportDataReceived,
            MessageContentType::Poll,
            MessageContentType::Dice,
            MessageContentType::ProximityAlertTriggered,
            MessageContentType::GroupCall,
            MessageContentType::InviteToGroupCall,
            MessageContentType::ChatSetTheme,
            MessageContentType::WebViewDataSent,
            MessageContentType::WebViewDataReceived,
            MessageContentType::GiftPremium,
            MessageContentType::TopicCreate,
            MessageContentType::TopicEdit,
            MessageContentType::SuggestProfilePhoto,
            MessageContentType::WriteAccessAllowed,
            MessageContentType::RequestedDialog,
            MessageContentType::WebViewWriteAccessAllowed,
            MessageContentType::SetBackground,
            MessageContentType::Story,
            MessageContentType::WriteAccessAllowedByRequest,
            MessageContentType::GiftCode,
            MessageContentType::Giveaway,
            MessageContentType::GiveawayLaunch,
            MessageContentType::GiveawayResults,
            MessageContentType::GiveawayWinners,
            MessageContentType::ExpiredVideoNote,
            MessageContentType::ExpiredVoiceNote,
            MessageContentType::BoostApply,
            MessageContentType::DialogShared,
            MessageContentType::PaidMedia,
            MessageContentType::PaymentRefunded,
            MessageContentType::GiftStars,
            MessageContentType::PrizeStars,
            MessageContentType::StarGift,
            MessageContentType::StarGiftUnique,
            MessageContentType::PaidMessagesRefunded,
            MessageContentType::PaidMessagesPrice,
            MessageContentType::ConferenceCall,
            MessageContentType::ToDoList,
            MessageContentType::TodoCompletions,
            MessageContentType::TodoAppendTasks,
            MessageContentType::GiftTon,
            MessageContentType::SuggestedPostSuccess,
            MessageContentType::SuggestedPostRefund,
            MessageContentType::SuggestedPostApproval,
            MessageContentType::SuggestBirthday,
            MessageContentType::StarGiftPurchaseOffer,
            MessageContentType::StarGiftPurchaseOfferDeclined,
            MessageContentType::NewCreatorPending,
            MessageContentType::ChangeCreator,
            MessageContentType::NoForwardsToggle,
            MessageContentType::NoForwardsRequest,
            MessageContentType::ManagedBotCreated,
            MessageContentType::PollAppendAnswer,
            MessageContentType::PollDeleteAnswer,
            MessageContentType::RichText,
            MessageContentType::ChangeCommunity,
            MessageContentType::ChatJoinedViaCommunity,
        ];
        for (index, content_type) in all.iter().enumerate() {
            assert_eq!(*content_type as i32, index as i32 - 1);
        }
    }

    /// Every built variant maps to its C++ `get_type` override.
    #[test]
    fn get_type_matches_the_cpp_overrides() {
        let contents = [
            (
                create_empty_text_message_content(),
                MessageContentType::Text,
            ),
            (
                create_text_message_content(
                    "text".to_string(),
                    Vec::new(),
                    WebPageId::default(),
                    false,
                    false,
                    false,
                    String::new(),
                ),
                MessageContentType::Text,
            ),
            (
                create_animation_message_content(FileId::new(1, 0)),
                MessageContentType::Animation,
            ),
            (
                create_audio_message_content(FileId::new(2, 0)),
                MessageContentType::Audio,
            ),
            (
                create_document_message_content(FileId::new(3, 0)),
                MessageContentType::Document,
            ),
            (
                create_photo_message_content(Photo::default(), FileId::new(4, 0)),
                MessageContentType::Photo,
            ),
            (
                create_video_message_content(FileId::new(5, 0), Photo::default(), 0),
                MessageContentType::Video,
            ),
            (
                create_voice_note_message_content(FileId::new(6, 0)),
                MessageContentType::VoiceNote,
            ),
            (
                create_contact_registered_message_content(),
                MessageContentType::ContactRegistered,
            ),
            (
                create_screenshot_taken_message_content(),
                MessageContentType::ScreenshotTaken,
            ),
            (
                create_chat_set_ttl_message_content(60, UserId::new(7)),
                MessageContentType::ChatSetTtl,
            ),
            (
                MessageContent::Sticker(MessageSticker::default()),
                MessageContentType::Sticker,
            ),
            (
                MessageContent::VideoNote(MessageVideoNote::default()),
                MessageContentType::VideoNote,
            ),
            (
                MessageContent::ChatCreate(MessageChatCreate::default()),
                MessageContentType::ChatCreate,
            ),
            (
                MessageContent::ChatChangeTitle(MessageChatChangeTitle::default()),
                MessageContentType::ChatChangeTitle,
            ),
            (
                MessageContent::ChatChangePhoto(MessageChatChangePhoto::default()),
                MessageContentType::ChatChangePhoto,
            ),
            (
                MessageContent::ChatDeletePhoto(MessageChatDeletePhoto),
                MessageContentType::ChatDeletePhoto,
            ),
            (
                MessageContent::ChatDeleteHistory(MessageChatDeleteHistory),
                MessageContentType::ChatDeleteHistory,
            ),
            (
                MessageContent::ChatAddUsers(MessageChatAddUsers::default()),
                MessageContentType::ChatAddUsers,
            ),
            (
                MessageContent::ChatJoinedByLink(MessageChatJoinedByLink::default()),
                MessageContentType::ChatJoinedByLink,
            ),
            (
                MessageContent::ChatDeleteUser(MessageChatDeleteUser::default()),
                MessageContentType::ChatDeleteUser,
            ),
            (
                MessageContent::ChatMigrateTo(MessageChatMigrateTo::default()),
                MessageContentType::ChatMigrateTo,
            ),
            (
                MessageContent::ChannelCreate(MessageChannelCreate::default()),
                MessageContentType::ChannelCreate,
            ),
            (
                MessageContent::ChannelMigrateFrom(MessageChannelMigrateFrom::default()),
                MessageContentType::ChannelMigrateFrom,
            ),
            (
                MessageContent::PinMessage(MessagePinMessage::default()),
                MessageContentType::PinMessage,
            ),
            (
                MessageContent::GameScore(MessageGameScore::default()),
                MessageContentType::GameScore,
            ),
            (
                MessageContent::ChatSetTtl(MessageChatSetTtl::default()),
                MessageContentType::ChatSetTtl,
            ),
            (
                MessageContent::Unsupported(MessageUnsupported::default()),
                MessageContentType::Unsupported,
            ),
            (
                MessageContent::ExpiredPhoto(MessageExpiredPhoto),
                MessageContentType::ExpiredPhoto,
            ),
            (
                MessageContent::ExpiredVideo(MessageExpiredVideo),
                MessageContentType::ExpiredVideo,
            ),
            (
                MessageContent::ExpiredVideoNote(MessageExpiredVideoNote),
                MessageContentType::ExpiredVideoNote,
            ),
            (
                MessageContent::ExpiredVoiceNote(MessageExpiredVoiceNote),
                MessageContentType::ExpiredVoiceNote,
            ),
            (
                MessageContent::CustomServiceAction(MessageCustomServiceAction::default()),
                MessageContentType::CustomServiceAction,
            ),
            (
                MessageContent::WebsiteConnected(MessageWebsiteConnected::default()),
                MessageContentType::WebsiteConnected,
            ),
            (
                MessageContent::Poll(MessagePoll::default()),
                MessageContentType::Poll,
            ),
            (
                MessageContent::Dice(MessageDice::default()),
                MessageContentType::Dice,
            ),
            (
                MessageContent::WebViewDataSent(MessageWebViewDataSent::default()),
                MessageContentType::WebViewDataSent,
            ),
            (
                MessageContent::WebViewDataReceived(MessageWebViewDataReceived::default()),
                MessageContentType::WebViewDataReceived,
            ),
            (
                MessageContent::GiftPremium(MessageGiftPremium::default()),
                MessageContentType::GiftPremium,
            ),
            (
                MessageContent::GiftStars(MessageGiftStars::default()),
                MessageContentType::GiftStars,
            ),
            (
                MessageContent::GiveawayLaunch(MessageGiveawayLaunch::default()),
                MessageContentType::GiveawayLaunch,
            ),
            (
                MessageContent::GiveawayResults(MessageGiveawayResults::default()),
                MessageContentType::GiveawayResults,
            ),
            (
                MessageContent::GiveawayWinners(MessageGiveawayWinners::default()),
                MessageContentType::GiveawayWinners,
            ),
            (
                MessageContent::BoostApply(MessageBoostApply::default()),
                MessageContentType::BoostApply,
            ),
            (
                MessageContent::PaidMessagesRefunded(MessagePaidMessagesRefunded::default()),
                MessageContentType::PaidMessagesRefunded,
            ),
            (
                MessageContent::PaidMessagesPrice(MessagePaidMessagesPrice::default()),
                MessageContentType::PaidMessagesPrice,
            ),
            (
                MessageContent::TodoCompletions(MessageTodoCompletions::default()),
                MessageContentType::TodoCompletions,
            ),
            (
                MessageContent::GiftTon(MessageGiftTon::default()),
                MessageContentType::GiftTon,
            ),
            (
                MessageContent::SuggestedPostRefund(MessageSuggestedPostRefund::default()),
                MessageContentType::SuggestedPostRefund,
            ),
            (
                MessageContent::NewCreatorPending(MessageNewCreatorPending::default()),
                MessageContentType::NewCreatorPending,
            ),
            (
                MessageContent::ChangeCreator(MessageChangeCreator::default()),
                MessageContentType::ChangeCreator,
            ),
            (
                MessageContent::NoForwardsToggle(MessageNoForwardsToggle::default()),
                MessageContentType::NoForwardsToggle,
            ),
            (
                MessageContent::NoForwardsRequest(MessageNoForwardsRequest::default()),
                MessageContentType::NoForwardsRequest,
            ),
            (
                MessageContent::ManagedBotCreated(MessageManagedBotCreated::default()),
                MessageContentType::ManagedBotCreated,
            ),
            (
                MessageContent::PollAppendAnswer(MessagePollAppendAnswer::default()),
                MessageContentType::PollAppendAnswer,
            ),
            (
                MessageContent::PollDeleteAnswer(MessagePollDeleteAnswer::default()),
                MessageContentType::PollDeleteAnswer,
            ),
            (
                MessageContent::WriteAccessAllowed(MessageWriteAccessAllowed),
                MessageContentType::WriteAccessAllowed,
            ),
            (
                MessageContent::WriteAccessAllowedByRequest(MessageWriteAccessAllowedByRequest),
                MessageContentType::WriteAccessAllowedByRequest,
            ),
            (
                MessageContent::SuggestProfilePhoto(MessageSuggestProfilePhoto::default()),
                MessageContentType::SuggestProfilePhoto,
            ),
            (
                MessageContent::SuggestBirthday(MessageSuggestBirthday::default()),
                MessageContentType::SuggestBirthday,
            ),
            (
                MessageContent::Call(MessageCall::default()),
                MessageContentType::Call,
            ),
            (
                MessageContent::GroupCall(MessageGroupCall::default()),
                MessageContentType::GroupCall,
            ),
            (
                MessageContent::InviteToGroupCall(MessageInviteToGroupCall::default()),
                MessageContentType::InviteToGroupCall,
            ),
        ];
        for (content, expected_type) in contents {
            assert_eq!(content.get_type(), expected_type);
        }
    }

    /// The `MessageText` constructor flag normalization
    /// (MessageContent.cpp:170-176).
    #[test]
    fn text_flags_normalize_without_a_web_page_url() {
        let no_url = MessageText::new(
            FormattedText::default(),
            WebPageId::default(),
            true,
            true,
            false,
            String::new(),
        );
        assert!(!no_url.force_small_media);
        assert!(!no_url.force_large_media);

        let large = MessageText::new(
            FormattedText::default(),
            WebPageId::new(-1),
            true,
            true,
            false,
            "https://telegram.org".to_string(),
        );
        assert!(!large.force_small_media);
        assert!(large.force_large_media);

        let small = MessageText::new(
            FormattedText::default(),
            WebPageId::new(-1),
            true,
            false,
            false,
            "https://telegram.org".to_string(),
        );
        assert!(small.force_small_media);
        assert!(!small.force_large_media);
    }

    /// The `MessageDice` constructor normalization and the `is_valid`
    /// decision table (MessageContent.cpp:787-825).
    #[test]
    fn dice_emoji_normalizes_and_validates() {
        let default_emoji = MessageDice::new("", 3, false, String::new(), String::new(), 0, 0);
        assert_eq!(default_emoji.emoji, MessageDice::DEFAULT_EMOJI);

        // The C++ default `remove_selectors = true`.
        let stripped = MessageDice::new("🎲\u{fe0f}", 3, false, String::new(), String::new(), 0, 0);
        assert_eq!(stripped.emoji, MessageDice::DEFAULT_EMOJI);

        assert!(
            MessageDice::new(
                MessageDice::DEFAULT_EMOJI,
                6,
                false,
                String::new(),
                String::new(),
                0,
                0
            )
            .is_valid()
        );
        assert!(
            !MessageDice::new(
                MessageDice::DEFAULT_EMOJI,
                7,
                false,
                String::new(),
                String::new(),
                0,
                0
            )
            .is_valid()
        );
        assert!(MessageDice::new("🎯", 6, false, String::new(), String::new(), 0, 0).is_valid());
        assert!(!MessageDice::new("🎯", 7, false, String::new(), String::new(), 0, 0).is_valid());
        assert!(MessageDice::new("⚽", 1000, false, String::new(), String::new(), 0, 0).is_valid());
        assert!(
            !MessageDice::new("⚽", 1001, false, String::new(), String::new(), 0, 0).is_valid()
        );
        assert!(!MessageDice::new("⚽", -1, false, String::new(), String::new(), 0, 0).is_valid());

        // A stake with a value needs the default die, a seed and a positive
        // stake amount.
        assert!(
            MessageDice::new(
                MessageDice::DEFAULT_EMOJI,
                3,
                true,
                "seed".to_string(),
                String::new(),
                10,
                0
            )
            .is_valid()
        );
        assert!(
            !MessageDice::new("⚽", 3, true, "seed".to_string(), String::new(), 10, 0).is_valid()
        );
        assert!(
            !MessageDice::new(
                MessageDice::DEFAULT_EMOJI,
                3,
                true,
                String::new(),
                String::new(),
                10,
                0
            )
            .is_valid()
        );
        assert!(
            !MessageDice::new(
                MessageDice::DEFAULT_EMOJI,
                3,
                true,
                "seed".to_string(),
                String::new(),
                0,
                0
            )
            .is_valid()
        );
        assert!(
            !MessageDice::new(
                MessageDice::DEFAULT_EMOJI,
                3,
                true,
                "seed".to_string(),
                String::new(),
                10,
                -1
            )
            .is_valid()
        );

        // A stake without a value needs a state hash and a 32-byte seed.
        assert!(
            MessageDice::new(
                MessageDice::DEFAULT_EMOJI,
                0,
                true,
                "01234567890123456789012345678901".to_string(),
                "state".to_string(),
                0,
                0
            )
            .is_valid()
        );
        assert!(
            !MessageDice::new(
                MessageDice::DEFAULT_EMOJI,
                0,
                true,
                "0123456789012345678901234567890123456789".to_string(),
                "state".to_string(),
                0,
                0
            )
            .is_valid()
        );
        assert!(
            !MessageDice::new(
                MessageDice::DEFAULT_EMOJI,
                0,
                true,
                "short seed".to_string(),
                "state".to_string(),
                0,
                0
            )
            .is_valid()
        );
    }

    /// `MessageUnsupported::CURRENT_VERSION` (MessageContent.cpp:554) and
    /// the default instance.
    #[test]
    fn unsupported_defaults_to_the_current_version() {
        assert_eq!(MessageUnsupported::CURRENT_VERSION, 62);
        let unsupported = MessageUnsupported::default();
        assert_eq!(unsupported.version, MessageUnsupported::CURRENT_VERSION);
        let downgraded = MessageUnsupported { version: 3 };
        assert_ne!(downgraded.version, MessageUnsupported::CURRENT_VERSION);
    }

    /// The factory-produced defaults mirror the C++ factory bodies
    /// (MessageContent.cpp:4653-4704).
    #[test]
    fn factories_build_the_cpp_defaults() {
        let animation = create_animation_message_content(FileId::new(11, 0));
        match &animation {
            MessageContent::Animation(content) => {
                assert_eq!(content.file_id, FileId::new(11, 0));
                assert!(content.caption.text.is_empty());
                assert!(!content.has_spoiler);
            }
            _ => panic!(
                "create_animation_message_content built {:?}",
                animation.get_type()
            ),
        }

        let video = create_video_message_content(FileId::new(12, 0), Photo::default(), 30);
        match &video {
            MessageContent::Video(content) => {
                assert_eq!(content.file_id, FileId::new(12, 0));
                assert!(content.alternative_file_ids.is_empty());
                assert!(content.hls_file_ids.is_empty());
                assert!(content.storyboard_file_ids.is_empty());
                assert!(content.storyboard_map_file_ids.is_empty());
                assert!(content.cover.is_empty());
                assert_eq!(content.start_timestamp, 30);
                assert!(!content.has_spoiler);
            }
            _ => panic!("create_video_message_content built {:?}", video.get_type()),
        }

        let photo = create_photo_message_content(Photo::default(), FileId::new(13, 0));
        match &photo {
            MessageContent::Photo(content) => {
                assert!(content.photo.is_empty());
                assert_eq!(content.video_file_id, FileId::new(13, 0));
                assert!(!content.has_spoiler);
            }
            _ => panic!("create_photo_message_content built {:?}", photo.get_type()),
        }

        let set_ttl = create_chat_set_ttl_message_content(3600, UserId::new(14));
        match &set_ttl {
            MessageContent::ChatSetTtl(content) => {
                assert_eq!(content.ttl, 3600);
                assert_eq!(content.from_user_id, UserId::new(14));
            }
            _ => panic!(
                "create_chat_set_ttl_message_content built {:?}",
                set_ttl.get_type()
            ),
        }

        assert_eq!(
            create_contact_registered_message_content().get_type(),
            MessageContentType::ContactRegistered
        );
        assert_eq!(
            create_screenshot_taken_message_content().get_type(),
            MessageContentType::ScreenshotTaken
        );

        assert_eq!(
            MessageContent::Text(MessageText::default()),
            create_empty_text_message_content()
        );
    }

    /// The wrapper semantics the C++ ids pin: validity sentinels and the
    /// group-call equality that ignores the access hash
    /// (InputGroupCallId.h:31-41).
    #[test]
    fn companion_id_wrappers_keep_the_cpp_semantics() {
        assert!(!WebPageId::default().is_valid());
        assert!(WebPageId::new(-3).is_valid());
        assert_eq!(WebPageId::new(-3).get(), -3);

        assert!(!PollId::default().is_valid());
        assert!(PollId::new(9).is_valid());

        let call = InputGroupCallId::new(5, 100);
        let same_call = InputGroupCallId::new(5, 200);
        let other_call = InputGroupCallId::new(6, 100);
        assert_eq!(call, same_call); // the C++ operator== ignores the access hash
        assert_ne!(call, other_call);
        assert!(!call.is_identical(same_call));
        assert!(call.is_identical(InputGroupCallId::new(5, 100)));
        assert!(!InputGroupCallId::default().is_valid());
        assert!(call.is_valid());

        assert_eq!(
            CallDiscardReason::default().r#type,
            CallDiscardReasonType::Empty
        );
        assert!(CallDiscardReason::default().slug.is_empty());

        // The group call content defaults the C++ -1 sentinels
        // (MessageContent.cpp:854-861).
        let group_call = MessageGroupCall::default();
        assert_eq!(group_call.duration, -1);
        assert_eq!(group_call.schedule_date, -1);
        assert!(!group_call.input_group_call_id.is_valid());
    }

    /// The `MessagePoll` attached-media slot is a boxed content
    /// (MessageContent.cpp:761-776).
    #[test]
    fn poll_carries_boxed_attached_media() {
        let mut poll = MessagePoll::default();
        assert!(poll.attached_media.is_none());
        assert_eq!(
            MessageContent::Poll(poll.clone()).get_type(),
            MessageContentType::Poll
        );

        poll.attached_media = Some(Box::new(create_photo_message_content(
            Photo::default(),
            FileId::new(1, 0),
        )));
        match &poll.attached_media {
            Some(boxed) => assert_eq!(boxed.get_type(), MessageContentType::Photo),
            None => panic!("the attached media disappeared"),
        }
    }

    /// The `Location` constructor normalization, the map-point bound and
    /// the epsilon equality (Location.cpp:17-38, 74-80, 229-238).
    #[test]
    fn location_normalizes_and_compares_like_the_cpp() {
        let empty = Location::default();
        assert!(empty.is_empty());
        assert!(!empty.is_valid_map_point());
        assert_eq!(empty, Location::new(91.0, 0.0, 0.0, 0)); // |lat| > 90 stays empty
        assert_eq!(empty, Location::new(f64::NAN, 0.0, 0.0, 0));

        let location = Location::new(55.0, 37.0, 2000.0, 42);
        assert!(!location.is_empty());
        assert_eq!(location.get_latitude(), 55.0);
        assert_eq!(location.get_longitude(), 37.0);
        assert_eq!(location.get_access_hash(), 42);
        // The accuracy clamps to 1500 (Location.cpp:21-23).
        assert_eq!(location, Location::new(55.0, 37.0, 1500.0, 0));
        assert_eq!(
            Location::new(55.0, 37.0, 1501.0, 0),
            Location::new(55.0, 37.0, 1500.0, 0)
        );

        let accurate = Location::new(55.0, 37.0, 20.0, 0);
        assert!(accurate.is_valid_map_point());
        let too_far_north = Location::new(86.0, 37.0, 0.0, 0);
        assert!(!too_far_north.is_valid_map_point());
        assert!(!too_far_north.is_empty());

        // The epsilon equality ignores the access hash; the accuracy
        // participates.
        assert_eq!(accurate, Location::new(55.0000009, 37.0000009, 20.0, 43));
        assert_ne!(accurate, Location::new(55.0, 37.0, 21.0, 0));

        let mut cloned = accurate.clone();
        cloned.set_access_hash(7);
        assert_eq!(cloned.get_access_hash(), 7);
        assert_eq!(cloned, accurate);
    }

    /// The `BackgroundFill`/`BackgroundType` classification and the
    /// `BackgroundInfo` both-local equality (BackgroundType.h, BackgroundInfo.h:35-39).
    #[test]
    fn background_types_classify_and_compare_like_the_cpp() {
        assert_eq!(
            BackgroundFill::solid(7).get_type(),
            BackgroundFillType::Solid
        );
        assert_eq!(
            BackgroundFill::default().get_type(),
            BackgroundFillType::Solid
        );

        let gradient = BackgroundFill::gradient(1, 2, 90);
        assert_eq!(gradient.get_type(), BackgroundFillType::Gradient);
        assert_eq!(gradient.rotation_angle, 90);
        // A solid "gradient" loses its rotation angle (BackgroundType.h:25-26).
        assert_eq!(BackgroundFill::gradient(5, 5, 90).rotation_angle, 0);

        let freeform = BackgroundFill::freeform(1, 2, 3, 4);
        assert_eq!(freeform.get_type(), BackgroundFillType::FreeformGradient);

        assert!(BackgroundType::wallpaper(true, false, 10).has_file());
        assert!(!BackgroundType::wallpaper(true, false, 10).has_gradient_fill());
        assert!(BackgroundType::pattern(false, BackgroundFill::solid(3), 40).has_file());
        let fill = BackgroundType::fill(BackgroundFill::gradient(1, 2, 0), 50);
        assert!(!fill.has_file());
        assert!(fill.has_gradient_fill());
        assert!(!BackgroundType::fill(BackgroundFill::solid(1), 50).has_gradient_fill());
        assert!(!BackgroundType::chat_theme("dark".to_string()).has_file());

        // The both-local rule (BackgroundInfo.h:35-39).
        let local_a = BackgroundInfo {
            background_id: BackgroundId::new(5),
            background_type: BackgroundType::wallpaper(false, false, 0),
        };
        let local_b = BackgroundInfo {
            background_id: BackgroundId::new(6),
            background_type: BackgroundType::wallpaper(false, false, 0),
        };
        assert_eq!(local_a, local_b);
        let remote = BackgroundInfo {
            background_id: BackgroundId::new(0x100000000),
            background_type: BackgroundType::wallpaper(false, false, 0),
        };
        assert_ne!(local_a, remote);
        assert!(remote.is_valid());
        let other_type = BackgroundInfo {
            background_id: BackgroundId::new(5),
            background_type: BackgroundType::fill(BackgroundFill::solid(1), 0),
        };
        assert_ne!(local_a, other_type);
    }

    /// The `ForumTopicIcon` color mask, the C++ default color and the
    /// edit flip (ForumTopicIcon.cpp:12-22).
    #[test]
    fn forum_topic_icon_masks_and_edits() {
        assert_eq!(ForumTopicIcon::default().get_color(), 0x6FB9F0);
        let icon = ForumTopicIcon::new(0x1234567, CustomEmojiId::new(9));
        assert_eq!(icon.get_color(), 0x234567);
        assert_eq!(icon.get_custom_emoji_id(), CustomEmojiId::new(9));

        let mut icon = ForumTopicIcon::default();
        assert!(icon.edit_custom_emoji_id(CustomEmojiId::new(5)));
        assert!(!icon.edit_custom_emoji_id(CustomEmojiId::new(5)));
        assert!(icon.edit_custom_emoji_id(CustomEmojiId::default()));
    }

    /// The `MessageLiveLocation` constructor clamps
    /// (MessageContent.cpp:684-710).
    #[test]
    fn live_location_clamps_negative_and_out_of_range_values() {
        let live = MessageLiveLocation::new(Location::default(), -5, 361, -1);
        assert_eq!(live.period, 0);
        assert_eq!(live.heading, 0);
        assert_eq!(live.proximity_alert_radius, 0);

        let boundary = MessageLiveLocation::new(Location::default(), 0, 360, 0);
        assert_eq!(boundary.heading, 360);
    }

    /// The `MessageExtendedMedia` state predicates
    /// (MessageExtendedMedia.h:16, 91-119).
    #[test]
    fn extended_media_state_predicates() {
        let empty = MessageExtendedMedia::default();
        assert!(empty.is_empty());
        assert!(!empty.has_input_media());
        assert!(!empty.need_reget());
        assert_eq!(MessageExtendedMedia::CURRENT_VERSION, 1);

        let old_unsupported = MessageExtendedMedia {
            r#type: MessageExtendedMediaType::Unsupported,
            unsupported_version: 0,
            ..MessageExtendedMedia::default()
        };
        assert!(old_unsupported.is_unsupported());
        assert!(old_unsupported.need_reget());
        let current_unsupported = MessageExtendedMedia {
            unsupported_version: MessageExtendedMedia::CURRENT_VERSION,
            ..old_unsupported
        };
        assert!(!current_unsupported.need_reget());

        let photo = MessageExtendedMedia {
            r#type: MessageExtendedMediaType::Photo,
            ..MessageExtendedMedia::default()
        };
        assert!(photo.has_input_media());
        assert!(!photo.has_media_timestamp());
        let video = MessageExtendedMedia {
            r#type: MessageExtendedMediaType::Video,
            ..MessageExtendedMedia::default()
        };
        assert!(video.has_input_media());
        assert!(video.has_media_timestamp());
        let preview = MessageExtendedMedia {
            r#type: MessageExtendedMediaType::Preview,
            ..MessageExtendedMedia::default()
        };
        assert!(preview.need_poll());
        assert!(!preview.has_input_media());
    }

    /// The currency values: `StarAmount::is_positive`, the TON multiplier
    /// pin and `SuggestedPostPrice::is_empty` (StarAmount.h:28-31,
    /// SuggestedPostPrice.h:20, 34-36).
    #[test]
    fn currency_values_keep_the_cpp_semantics() {
        assert!(
            StarAmount {
                star_count: 1,
                nanostar_count: 0
            }
            .is_positive()
        );
        assert!(
            StarAmount {
                star_count: 0,
                nanostar_count: 5
            }
            .is_positive()
        );
        assert!(!StarAmount::default().is_positive());
        assert!(TonAmount { ton_amount: 1 }.is_positive());
        assert!(!TonAmount::default().is_positive());

        assert_eq!(SuggestedPostPrice::TON_MULTIPLIER, 10000000);
        assert!(SuggestedPostPrice::default().is_empty());
        assert!(
            !SuggestedPostPrice {
                r#type: SuggestedPostPriceType::Star,
                amount: 100
            }
            .is_empty()
        );
    }

    /// The `SecureValueType` discriminant order (SecureValue.h:27-42), the
    /// to-do aggregates and `ForumTopicEditedData::is_empty`.
    #[test]
    fn todo_and_secure_value_aggregates() {
        let secure_types = [
            SecureValueType::None,
            SecureValueType::PersonalDetails,
            SecureValueType::Passport,
            SecureValueType::DriverLicense,
            SecureValueType::IdentityCard,
            SecureValueType::InternalPassport,
            SecureValueType::Address,
            SecureValueType::UtilityBill,
            SecureValueType::BankStatement,
            SecureValueType::RentalAgreement,
            SecureValueType::PassportRegistration,
            SecureValueType::TemporaryRegistration,
            SecureValueType::PhoneNumber,
            SecureValueType::EmailAddress,
        ];
        for (index, secure_type) in secure_types.iter().enumerate() {
            assert_eq!(*secure_type as i32, index as i32);
        }

        let list = ToDoList::default();
        assert!(list.items.is_empty());
        assert!(!list.others_can_append);
        assert!(!list.others_can_complete);

        assert!(ForumTopicEditedData::default().is_empty());
        // `is_empty` reads only the edit flags, not the state flags
        // (ForumTopicEditedData.cpp): a closed topic alone stays "empty".
        assert!(
            ForumTopicEditedData {
                is_closed: true,
                is_hidden: true,
                ..ForumTopicEditedData::default()
            }
            .is_empty()
        );
        assert!(
            !ForumTopicEditedData {
                edit_is_closed: true,
                ..ForumTopicEditedData::default()
            }
            .is_empty()
        );
        assert!(
            !ForumTopicEditedData {
                edit_icon_custom_emoji_id: true,
                ..ForumTopicEditedData::default()
            }
            .is_empty()
        );
    }

    /// Every variant added in M.9.1c/M.9.1d maps to its C++ `get_type`
    /// override.
    #[test]
    fn new_variants_map_to_the_cpp_types() {
        let contents = [
            (
                MessageContent::Location(MessageLocation::default()),
                MessageContentType::Location,
            ),
            (
                MessageContent::Venue(MessageVenue::default()),
                MessageContentType::Venue,
            ),
            (
                MessageContent::LiveLocation(MessageLiveLocation::default()),
                MessageContentType::LiveLocation,
            ),
            (
                MessageContent::TopicCreate(MessageTopicCreate::default()),
                MessageContentType::TopicCreate,
            ),
            (
                MessageContent::TopicEdit(MessageTopicEdit::default()),
                MessageContentType::TopicEdit,
            ),
            (
                MessageContent::SuggestedPostSuccess(MessageSuggestedPostSuccess::default()),
                MessageContentType::SuggestedPostSuccess,
            ),
            (
                MessageContent::SuggestedPostApproval(MessageSuggestedPostApproval::default()),
                MessageContentType::SuggestedPostApproval,
            ),
            (
                MessageContent::WebViewWriteAccessAllowed(
                    MessageWebViewWriteAccessAllowed::default(),
                ),
                MessageContentType::WebViewWriteAccessAllowed,
            ),
            (
                MessageContent::SetBackground(MessageSetBackground::default()),
                MessageContentType::SetBackground,
            ),
            (
                MessageContent::Giveaway(MessageGiveaway::default()),
                MessageContentType::Giveaway,
            ),
            (
                MessageContent::Game(MessageGame::default()),
                MessageContentType::Game,
            ),
            (
                MessageContent::Invoice(MessageInvoice::default()),
                MessageContentType::Invoice,
            ),
            (
                MessageContent::PassportDataSent(MessagePassportDataSent::default()),
                MessageContentType::PassportDataSent,
            ),
            (
                MessageContent::PassportDataReceived(MessagePassportDataReceived::default()),
                MessageContentType::PassportDataReceived,
            ),
            (
                MessageContent::PaidMedia(MessagePaidMedia::default()),
                MessageContentType::PaidMedia,
            ),
            (
                MessageContent::TodoAppendTasks(MessageTodoAppendTasks::default()),
                MessageContentType::TodoAppendTasks,
            ),
        ];
        for (content, expected_type) in contents {
            assert_eq!(content.get_type(), expected_type);
        }
    }

    #[test]
    fn m91b_variants_map_to_the_cpp_types() {
        let contents = [
            (
                MessageContent::PaymentSuccessful(MessagePaymentSuccessful::default()),
                MessageContentType::PaymentSuccessful,
            ),
            (
                MessageContent::ProximityAlertTriggered(MessageProximityAlertTriggered::default()),
                MessageContentType::ProximityAlertTriggered,
            ),
            (
                MessageContent::RequestedDialog(MessageRequestedDialog::default()),
                MessageContentType::RequestedDialog,
            ),
            (
                MessageContent::Story(MessageStory::default()),
                MessageContentType::Story,
            ),
            (
                MessageContent::GiftCode(MessageGiftCode::default()),
                MessageContentType::GiftCode,
            ),
            (
                MessageContent::DialogShared(MessageDialogShared::default()),
                MessageContentType::DialogShared,
            ),
            (
                MessageContent::PaymentRefunded(MessagePaymentRefunded::default()),
                MessageContentType::PaymentRefunded,
            ),
            (
                MessageContent::PrizeStars(MessagePrizeStars::default()),
                MessageContentType::PrizeStars,
            ),
            (
                MessageContent::ConferenceCall(MessageConferenceCall::default()),
                MessageContentType::ConferenceCall,
            ),
            (
                MessageContent::ToDoList(MessageToDoList::default()),
                MessageContentType::ToDoList,
            ),
            (
                MessageContent::ChangeCommunity(MessageChangeCommunity::default()),
                MessageContentType::ChangeCommunity,
            ),
            (
                MessageContent::ChatJoinedViaCommunity(MessageChatJoinedViaCommunity::default()),
                MessageContentType::ChatJoinedViaCommunity,
            ),
        ];
        for (content, expected_type) in contents {
            assert_eq!(content.get_type(), expected_type);
        }
    }

    #[test]
    fn community_id_semantics() {
        assert!(!CommunityId::default().is_valid());
        assert!(CommunityId::new(42).is_valid());
        assert_eq!(CommunityId::new(42).get(), 42);

        // the fake dialog id rides the channel band: DialogId(ChannelId(id))
        let fake = CommunityId::new(7).get_fake_dialog_id();
        assert_eq!(fake, DialogId::from_channel_id(ChannelId::new(7)));
        assert_eq!(
            fake.get_type(),
            crate::messages_manager::DialogType::Channel
        );

        let mut bytes = Vec::new();
        CommunityId::new(123456789).store(&mut bytes);
        assert_eq!(
            CommunityId::parse_value(&bytes).unwrap(),
            CommunityId::new(123456789)
        );
    }

    #[test]
    fn shared_dialog_and_todo_completion_predicates() {
        let user_shared = SharedDialog {
            dialog_id: DialogId::from_user_id(crate::user_manager::UserId::new(7)),
            first_name: "A".to_string(),
            ..SharedDialog::default()
        };
        assert!(user_shared.is_valid());
        assert!(user_shared.is_user());
        assert!(!user_shared.is_dialog());

        let chat_shared = SharedDialog {
            dialog_id: DialogId::from_chat_id(ChatId::new(123)),
            ..SharedDialog::default()
        };
        assert!(chat_shared.is_dialog());
        assert!(!chat_shared.is_user());

        assert!(!SharedDialog::default().is_valid());

        let mut completion = ToDoCompletion {
            id: 1,
            date: 1000,
            ..ToDoCompletion::default()
        };
        assert!(!completion.is_valid());
        completion.completed_by_dialog_id =
            DialogId::from_user_id(crate::user_manager::UserId::new(7));
        assert!(completion.is_valid());
        completion.date = 0;
        assert!(!completion.is_valid());
    }
}
