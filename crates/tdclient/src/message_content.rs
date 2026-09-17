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
use crate::message_entity::{FormattedText, MessageEntity};
use crate::messages_manager::{DialogId, StoryFullId};
use crate::photo::{Dimensions, Photo};
use crate::store_parse::{Parser, store_i64};
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

#[cfg(test)]
mod tests {
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
                create_animation_message_content(FileId::new(1)),
                MessageContentType::Animation,
            ),
            (
                create_audio_message_content(FileId::new(2)),
                MessageContentType::Audio,
            ),
            (
                create_document_message_content(FileId::new(3)),
                MessageContentType::Document,
            ),
            (
                create_photo_message_content(Photo::default(), FileId::new(4)),
                MessageContentType::Photo,
            ),
            (
                create_video_message_content(FileId::new(5), Photo::default(), 0),
                MessageContentType::Video,
            ),
            (
                create_voice_note_message_content(FileId::new(6)),
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
        let animation = create_animation_message_content(FileId::new(11));
        match &animation {
            MessageContent::Animation(content) => {
                assert_eq!(content.file_id, FileId::new(11));
                assert!(content.caption.text.is_empty());
                assert!(!content.has_spoiler);
            }
            _ => panic!(
                "create_animation_message_content built {:?}",
                animation.get_type()
            ),
        }

        let video = create_video_message_content(FileId::new(12), Photo::default(), 30);
        match &video {
            MessageContent::Video(content) => {
                assert_eq!(content.file_id, FileId::new(12));
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

        let photo = create_photo_message_content(Photo::default(), FileId::new(13));
        match &photo {
            MessageContent::Photo(content) => {
                assert!(content.photo.is_empty());
                assert_eq!(content.video_file_id, FileId::new(13));
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
            FileId::new(1),
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
