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
//! The remaining 6 variants are pinned by companion types that do not
//! exist on this line yet and land with them:
//! - the `StarGift` family (`StarGift`, `StarGiftUnique`,
//!   `StarGiftPurchaseOffer`, `StarGiftPurchaseOfferDeclined`) — M.9.1b
//!   re-checked the C++ and found these need the `StarGift` VALUE class
//!   (StarGift.h, with its `StarGiftAttribute`/`StarGiftBackground`/
//!   `PeerColorCollectible` plane and `FileId` stickers), not just the
//!   `DialogId`s M.9.1 recorded; the value plane is the Wave-3
//!   Gifts/Stickers slice (`StarGiftResalePrice`, the only other missing
//!   piece, is trivial and lands with its consumers).
//! - `ChatSetTheme` — needs `StarGift` + `ThemeSettings` (ChatTheme.h).
//! - `RichText` — needs the `WebPageBlock`/`RichMessageMedia` machinery
//!   (RichMessage.h); Wave-2 WebPages scope. `create_rich_message_content`
//!   is deferred with it.
//!
//! M.9.2/M.9.3 are the db `store`/`parse` paths (the M.6 MessageDb
//! byte-compat bar, D1): [`store_message_content`]/[`parse_message_content`]
//! mirror the C++ `store`/`parse` template dispatchers byte for byte, with
//! the manager-mediated FileId calls going through the
//! [`MessageContentStorage`] seam — the Rust shape of the C++
//! `storer.context()->td()` indirection. FileId bytes stay
//! FileManager-mediated in the C++ (`FileManager::store_file`, the full
//! file-view plane), so they are gated until the M.12.2 FileManager, exactly
//! like the photo structures' file-id stores; the media metadata helpers
//! (`store_animation`/`store_sticker`/...) are gated with them because the
//! `Animation`/`Sticker`/`Document`/`Video`/`Audio`/`VideoNote`/
//! `VoiceNote` metadata value structs are the M.9.5/M.12 media plane.
//! Everything else — flags blocks, version gates, id wrappers, the Photo/
//! PhotoSize/Invoice/Game/SecureValue/... companion formats — is exact.
//! `Photo::sticker_file_ids` (present in the C++ `Photo`, not yet in the
//! Rust model) is stored as an empty vector and read-and-discarded on
//! parse, which keeps the byte layout parseable in both directions.
//! `Contact` is modeled since M.9.2 (the value landed in `contact.rs`).
//!
//! The td_api/telegram_api
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
use crate::chat_manager::{ChannelId, ChannelType, ChatId, MessageId, RestrictedRights};
use crate::contact::Contact;
use crate::emoji_status::CustomEmojiId;
use crate::message_entity::{
    FormattedText, MessageEntity, Type, find_entities, remove_empty_entities,
};
use crate::messages_manager::{
    DialogId, DialogType, MessageFullId, MessageSearchFilter, StoryFullId, StoryId,
    message_search_filter_index_mask,
};
use crate::misc::is_empty_string;
use crate::photo::{AnimationSize, Dimensions, Photo, PhotoSize, StickerPhotoSize};
use crate::store_parse::{
    Parser, store_bool, store_f64, store_flags, store_i32, store_i32_vec, store_i64, store_string,
    store_string_vec, store_u32, version,
};
use crate::user_manager::{FileId, SecretChatId, UserId};
use tdmtproto::telegram_api;
use tdutils::promise::Promise;
use tdutils::status::Status;

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

/// `is_service_message_content` (MessageContentType.cpp:623-731) — the C++
/// switch enumerates all 97 types (28 returning false, the rest true), so
/// the `default: UNREACHABLE()` arm covers nothing. The two unmodeled
/// members keep their C++ answers recorded here: `RichText` is a
/// non-service type and `ChatSetTheme` is a service type; both land with
/// their value planes.
pub fn is_service_message_content(content_type: MessageContentType) -> bool {
    use MessageContentType::*;
    !matches!(
        content_type,
        Animation
            | Audio
            | Contact
            | Dice
            | Document
            | ExpiredPhoto
            | ExpiredVideo
            | ExpiredVideoNote
            | ExpiredVoiceNote
            | Game
            | Giveaway
            | GiveawayWinners
            | Invoice
            | LiveLocation
            | Location
            | PaidMedia
            | Photo
            | Poll
            | Sticker
            | Story
            | Text
            | ToDoList
            | Unsupported
            | Venue
            | Video
            | VideoNote
            | VoiceNote
    )
}

/// `is_expired_message_content` (MessageContentType.cpp:886-895).
pub fn is_expired_message_content(content_type: MessageContentType) -> bool {
    use MessageContentType::*;
    matches!(
        content_type,
        ExpiredPhoto | ExpiredVideo | ExpiredVideoNote | ExpiredVoiceNote
    )
}

/// `get_expired_message_content_type` (MessageContentType.cpp:897-909).
pub fn get_expired_message_content_type(content_type: MessageContentType) -> MessageContentType {
    use MessageContentType::*;
    match content_type {
        Photo => ExpiredPhoto,
        Video => ExpiredVideo,
        VideoNote => ExpiredVideoNote,
        VoiceNote => ExpiredVoiceNote,
        _ => None,
    }
}

/// `can_send_message_content_to_secret_chat` (MessageContentType.cpp:1019-1121).
/// The C++ third group — `None`, the service types, `Unsupported` and the
/// expired types — is `default: UNREACHABLE()`: those contents are never
/// offered for sending, and the Rust mirrors the abort. `RichText` (a
/// false-group member) is unmodeled and lands with WebPageBlock.
pub fn can_send_message_content_to_secret_chat(content_type: MessageContentType) -> bool {
    use MessageContentType::*;
    match content_type {
        Animation | Audio | Contact | Document | LiveLocation | Location | Photo | Sticker
        | Text | Venue | Video | VideoNote | VoiceNote => true,
        Dice | Game | Giveaway | GiveawayWinners | Invoice | PaidMedia | Poll | Story
        | ToDoList => false,
        _ => unreachable!(),
    }
}

impl MessageContentType {
    /// The int the C++ `store(content_type, storer)` writes and the
    /// `parse` dispatcher switches on; `None` (never a real content) and
    /// anything a newer binary inserted return `None`, which the parse
    /// dispatcher degrades to `MessageUnsupported(0)` like the C++
    /// `default:` arm.
    pub(crate) fn from_raw(raw: i32) -> Option<MessageContentType> {
        MessageContentType::try_from(raw).ok()
    }
}

impl TryFrom<i32> for MessageContentType {
    type Error = ();

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        // Explicit, like the pinned-discriminant-order test: a C++
        // insertion re-orders everything after it and both must fail loudly.
        match value {
            -1 => Ok(MessageContentType::None),
            0 => Ok(MessageContentType::Text),
            1 => Ok(MessageContentType::Animation),
            2 => Ok(MessageContentType::Audio),
            3 => Ok(MessageContentType::Document),
            4 => Ok(MessageContentType::Photo),
            5 => Ok(MessageContentType::Sticker),
            6 => Ok(MessageContentType::Video),
            7 => Ok(MessageContentType::VoiceNote),
            8 => Ok(MessageContentType::Contact),
            9 => Ok(MessageContentType::Location),
            10 => Ok(MessageContentType::Venue),
            11 => Ok(MessageContentType::ChatCreate),
            12 => Ok(MessageContentType::ChatChangeTitle),
            13 => Ok(MessageContentType::ChatChangePhoto),
            14 => Ok(MessageContentType::ChatDeletePhoto),
            15 => Ok(MessageContentType::ChatDeleteHistory),
            16 => Ok(MessageContentType::ChatAddUsers),
            17 => Ok(MessageContentType::ChatJoinedByLink),
            18 => Ok(MessageContentType::ChatDeleteUser),
            19 => Ok(MessageContentType::ChatMigrateTo),
            20 => Ok(MessageContentType::ChannelCreate),
            21 => Ok(MessageContentType::ChannelMigrateFrom),
            22 => Ok(MessageContentType::PinMessage),
            23 => Ok(MessageContentType::Game),
            24 => Ok(MessageContentType::GameScore),
            25 => Ok(MessageContentType::ScreenshotTaken),
            26 => Ok(MessageContentType::ChatSetTtl),
            27 => Ok(MessageContentType::Unsupported),
            28 => Ok(MessageContentType::Call),
            29 => Ok(MessageContentType::Invoice),
            30 => Ok(MessageContentType::PaymentSuccessful),
            31 => Ok(MessageContentType::VideoNote),
            32 => Ok(MessageContentType::ContactRegistered),
            33 => Ok(MessageContentType::ExpiredPhoto),
            34 => Ok(MessageContentType::ExpiredVideo),
            35 => Ok(MessageContentType::LiveLocation),
            36 => Ok(MessageContentType::CustomServiceAction),
            37 => Ok(MessageContentType::WebsiteConnected),
            38 => Ok(MessageContentType::PassportDataSent),
            39 => Ok(MessageContentType::PassportDataReceived),
            40 => Ok(MessageContentType::Poll),
            41 => Ok(MessageContentType::Dice),
            42 => Ok(MessageContentType::ProximityAlertTriggered),
            43 => Ok(MessageContentType::GroupCall),
            44 => Ok(MessageContentType::InviteToGroupCall),
            45 => Ok(MessageContentType::ChatSetTheme),
            46 => Ok(MessageContentType::WebViewDataSent),
            47 => Ok(MessageContentType::WebViewDataReceived),
            48 => Ok(MessageContentType::GiftPremium),
            49 => Ok(MessageContentType::TopicCreate),
            50 => Ok(MessageContentType::TopicEdit),
            51 => Ok(MessageContentType::SuggestProfilePhoto),
            52 => Ok(MessageContentType::WriteAccessAllowed),
            53 => Ok(MessageContentType::RequestedDialog),
            54 => Ok(MessageContentType::WebViewWriteAccessAllowed),
            55 => Ok(MessageContentType::SetBackground),
            56 => Ok(MessageContentType::Story),
            57 => Ok(MessageContentType::WriteAccessAllowedByRequest),
            58 => Ok(MessageContentType::GiftCode),
            59 => Ok(MessageContentType::Giveaway),
            60 => Ok(MessageContentType::GiveawayLaunch),
            61 => Ok(MessageContentType::GiveawayResults),
            62 => Ok(MessageContentType::GiveawayWinners),
            63 => Ok(MessageContentType::ExpiredVideoNote),
            64 => Ok(MessageContentType::ExpiredVoiceNote),
            65 => Ok(MessageContentType::BoostApply),
            66 => Ok(MessageContentType::DialogShared),
            67 => Ok(MessageContentType::PaidMedia),
            68 => Ok(MessageContentType::PaymentRefunded),
            69 => Ok(MessageContentType::GiftStars),
            70 => Ok(MessageContentType::PrizeStars),
            71 => Ok(MessageContentType::StarGift),
            72 => Ok(MessageContentType::StarGiftUnique),
            73 => Ok(MessageContentType::PaidMessagesRefunded),
            74 => Ok(MessageContentType::PaidMessagesPrice),
            75 => Ok(MessageContentType::ConferenceCall),
            76 => Ok(MessageContentType::ToDoList),
            77 => Ok(MessageContentType::TodoCompletions),
            78 => Ok(MessageContentType::TodoAppendTasks),
            79 => Ok(MessageContentType::GiftTon),
            80 => Ok(MessageContentType::SuggestedPostSuccess),
            81 => Ok(MessageContentType::SuggestedPostRefund),
            82 => Ok(MessageContentType::SuggestedPostApproval),
            83 => Ok(MessageContentType::SuggestBirthday),
            84 => Ok(MessageContentType::StarGiftPurchaseOffer),
            85 => Ok(MessageContentType::StarGiftPurchaseOfferDeclined),
            86 => Ok(MessageContentType::NewCreatorPending),
            87 => Ok(MessageContentType::ChangeCreator),
            88 => Ok(MessageContentType::NoForwardsToggle),
            89 => Ok(MessageContentType::NoForwardsRequest),
            90 => Ok(MessageContentType::ManagedBotCreated),
            91 => Ok(MessageContentType::PollAppendAnswer),
            92 => Ok(MessageContentType::PollDeleteAnswer),
            93 => Ok(MessageContentType::RichText),
            94 => Ok(MessageContentType::ChangeCommunity),
            95 => Ok(MessageContentType::ChatJoinedViaCommunity),
            _ => Err(()),
        }
    }
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

    /// The access-hash half of the C++ `store`/`parse`
    /// (InputGroupCallId.h:55-65).
    pub fn get_access_hash(self) -> i64 {
        self.access_hash
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

impl CallDiscardReasonType {
    /// The enum `td::parse` (tl_helpers.h:244-248) is a raw cast; an
    /// out-of-range stored int (garbage in a corrupted blob) falls back to
    /// `Empty` with a log instead of holding an unrepresentable value.
    pub(crate) fn from_raw(raw: i32) -> CallDiscardReasonType {
        match CallDiscardReasonType::try_from(raw) {
            Ok(value) => value,
            Err(_) => {
                tdutils::log_error!("Wrong call discard reason {raw}");
                CallDiscardReasonType::Empty
            }
        }
    }
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

    /// The C++ private-field read the `store` overload needs
    /// (Location.h:81-98): the packed horizontal accuracy.
    pub fn get_horizontal_accuracy(&self) -> f64 {
        self.horizontal_accuracy
    }

    /// `Location::parse` (Location.h:101-119) — the stored-shape
    /// constructor: the C++ parser assigns the private fields directly, so
    /// the `fix_accuracy` normalization of `Location::init` does not run.
    pub(crate) fn from_stored(
        is_empty: bool,
        latitude: f64,
        longitude: f64,
        horizontal_accuracy: f64,
        access_hash: i64,
    ) -> Location {
        Location {
            is_empty,
            latitude,
            longitude,
            horizontal_accuracy,
            access_hash,
        }
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

    /// `GiveawayParameters::get_channel_ids` (GiveawayParameters.cpp) —
    /// the additional channels first, then the boosted one.
    pub fn get_channel_ids(&self) -> Vec<ChannelId> {
        let mut result = self.additional_channel_ids.clone();
        result.push(self.boosted_channel_id);
        result
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

    /// `Game::get_text` (Game.h:55).
    pub fn get_text(&self) -> &FormattedText {
        &self.text
    }

    /// `Game::get_bot_user_id` (Game.cpp).
    pub fn get_bot_user_id(&self) -> UserId {
        self.bot_user_id
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

    /// `MessageExtendedMedia::get_message_content` (MessageExtendedMedia.cpp)
    /// — the per-type content the paid-media album splat produces. The C++
    /// `default: UNREACHABLE()` covers Empty/Unsupported/Preview, which the
    /// caller (`get_individual_message_contents`) never sees for a real
    /// paid media.
    pub fn get_message_content(&self) -> MessageContent {
        match self.r#type {
            MessageExtendedMediaType::Photo => {
                create_photo_message_content(self.photo.clone(), self.video_file_id)
            }
            MessageExtendedMediaType::Video => create_video_message_content(
                self.video_file_id,
                self.photo.clone(),
                self.start_timestamp,
            ),
            MessageExtendedMediaType::Empty
            | MessageExtendedMediaType::Unsupported
            | MessageExtendedMediaType::Preview => {
                unreachable!()
            }
        }
    }
}

impl InputInvoice {
    /// `InputInvoice::has_media_timestamp` (InputInvoice.cpp).
    pub fn has_media_timestamp(&self) -> bool {
        self.extended_media.has_media_timestamp()
    }

    /// `InputInvoice::get_caption` (InputInvoice.cpp) — the caption of the
    /// extended media, never null.
    pub fn get_caption(&self) -> &FormattedText {
        &self.extended_media_caption
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

impl ToDoList {
    /// `ToDoList::get_can_append_items` (ToDoList.cpp:65-74). The C++
    /// reads the `checklist_task_count_max` option through `td` at the
    /// call site, so the resolved maximum is a parameter here.
    pub fn get_can_append_items(&self, max_task_count: i64, item_count: i32) -> bool {
        if item_count < 0 {
            return false;
        }
        (self.items.len() as i64) + i64::from(item_count) <= max_task_count
    }
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

/// `MessageContact` (MessageContent.cpp:317-326) — landed with M.9.2: the
/// `td::Contact` value lives in `contact.rs` (the M.4.14 master-line
/// module this branch merged).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageContact {
    pub contact: Contact,
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
    Contact(MessageContact),
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
            MessageContent::Contact(_) => MessageContentType::Contact,
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

/// `create_contact_message_content` (MessageContent.cpp:4681 region) —
/// the M.9.2 addition with the `Contact` value.
pub fn create_contact_message_content(contact: Contact) -> MessageContent {
    MessageContent::Contact(MessageContact { contact })
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

// ==== M.9.2/M.9.3: the db store/parse paths ====
//
// `store`/`parse` (MessageContent.cpp:1831-3021 and 3021-4507) plus the
// `store_message_content`/`parse_message_content` entries (4507-4516) and
// the companion `td::store`/`td::parse` overloads from the value headers
// (Photo.hpp, PhotoSize.hpp, Location.h, Venue.h, Game.hpp, WebApp.hpp,
// InputInvoice.hpp, OrderInfo.hpp, BackgroundType.hpp, BackgroundInfo.hpp,
// GiveawayParameters.hpp, ForumTopic{,Icon,EditedData}.hpp, ToDoList.hpp,
// ToDoItem.hpp, ToDoCompletion.hpp, SecureValue.hpp, CurrencyAmount.hpp,
// SuggestedPostPrice.hpp, MessageExtendedMedia.hpp, StickerPhotoSize.hpp,
// SharedDialog.hpp and the id wrappers).

/// The manager-mediated serialization calls — the Rust shape of the C++
/// `storer.context()->td()` indirection the `store`/`parse` templates go
/// through. Every method is a 1-to-1 port seam for the named C++ manager
/// helper; the default real implementations land with their owners
/// (FileManager store/parse is the M.12.2 FileManager, the media metadata
/// helpers are the M.9.5/M.12 media plane, `store_poll` is Wave-2
/// PollManager, `store_background` is Wave-3 Backgrounds).
pub trait MessageContentStorage {
    /// `FileManager::store_file(file_id, storer)` (FileManager.hpp:30) —
    /// the FileView-based serialization the raw `store(FileId, ...)`
    /// overload routes through.
    fn store_file(&self, file_id: FileId, out: &mut Vec<u8>);
    /// `FileManager::parse_file(parser)` (FileManager.hpp:135); the
    /// pre-`Version::StoreFileId` empty-FileId legacy is handled here.
    fn parse_file(&mut self, parser: &mut Parser) -> Result<FileId, &'static str>;
    /// `AnimationsManager::store_animation` (AnimationsManager.hpp:21).
    fn store_animation(&self, file_id: FileId, out: &mut Vec<u8>);
    /// `AnimationsManager::parse_animation`.
    fn parse_animation(&mut self, parser: &mut Parser) -> Result<FileId, &'static str>;
    /// `AudiosManager::store_audio`.
    fn store_audio(&self, file_id: FileId, out: &mut Vec<u8>);
    /// `AudiosManager::parse_audio`.
    fn parse_audio(&mut self, parser: &mut Parser) -> Result<FileId, &'static str>;
    /// `DocumentsManager::store_document`.
    fn store_document(&self, file_id: FileId, out: &mut Vec<u8>);
    /// `DocumentsManager::parse_document`.
    fn parse_document(&mut self, parser: &mut Parser) -> Result<FileId, &'static str>;
    /// `VideosManager::store_video`.
    fn store_video(&self, file_id: FileId, out: &mut Vec<u8>);
    /// `VideosManager::parse_video`.
    fn parse_video(&mut self, parser: &mut Parser) -> Result<FileId, &'static str>;
    /// `VideoNotesManager::store_video_note`.
    fn store_video_note(&self, file_id: FileId, out: &mut Vec<u8>);
    /// `VideoNotesManager::parse_video_note`.
    fn parse_video_note(&mut self, parser: &mut Parser) -> Result<FileId, &'static str>;
    /// `VoiceNotesManager::store_voice_note`.
    fn store_voice_note(&self, file_id: FileId, out: &mut Vec<u8>);
    /// `VoiceNotesManager::parse_voice_note`.
    fn parse_voice_note(&mut self, parser: &mut Parser) -> Result<FileId, &'static str>;
    /// `StickersManager::store_sticker(file_id, false, storer, source)` —
    /// message stickers always store with `in_sticker_set = false`
    /// (MessageContent.cpp:1929).
    fn store_sticker(&self, file_id: FileId, out: &mut Vec<u8>);
    /// `StickersManager::parse_sticker(false, parser)`.
    fn parse_sticker(&mut self, parser: &mut Parser) -> Result<FileId, &'static str>;
    /// `PollManager::store_poll` (PollManager.hpp:226).
    fn store_poll(&self, poll_id: PollId, out: &mut Vec<u8>);
    /// `PollManager::parse_poll`.
    fn parse_poll(&mut self, parser: &mut Parser) -> Result<PollId, &'static str>;
    /// `BackgroundManager::store_background` (BackgroundManager.cpp:480).
    fn store_background(&self, background_id: BackgroundId, out: &mut Vec<u8>);
    /// `BackgroundManager::parse_background`.
    fn parse_background(&mut self, parser: &mut Parser) -> Result<BackgroundId, &'static str>;
    /// `WebPagesManager::on_load_web_page_url_from_database` — the
    /// `MessageText::parse` side effect (MessageContent.cpp:3225-3228).
    fn on_load_web_page_url_from_database(&mut self, web_page_id: WebPageId, url: &str);
}

/// `get_premium_duration_day_count` (misc.cpp:423-429) — the legacy
/// month-to-day conversion the `!has_days` GiftPremium/GiftCode parses run.
fn get_premium_duration_day_count(month_count: i32) -> i32 {
    if month_count <= 0 || month_count > 10_000_000 {
        return 7;
    }
    month_count * 30 + month_count / 3 + month_count / 12
}

/// The `td::parse` enum cast (tl_helpers.h:244-248) for the small
/// value-plane enums is a raw cast; out-of-range stored ints fall back to
/// the default with a log instead of holding an unrepresentable value.
impl TryFrom<i32> for SecureValueType {
    type Error = ();

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(SecureValueType::None),
            1 => Ok(SecureValueType::PersonalDetails),
            2 => Ok(SecureValueType::Passport),
            3 => Ok(SecureValueType::DriverLicense),
            4 => Ok(SecureValueType::IdentityCard),
            5 => Ok(SecureValueType::InternalPassport),
            6 => Ok(SecureValueType::Address),
            7 => Ok(SecureValueType::UtilityBill),
            8 => Ok(SecureValueType::BankStatement),
            9 => Ok(SecureValueType::RentalAgreement),
            10 => Ok(SecureValueType::PassportRegistration),
            11 => Ok(SecureValueType::TemporaryRegistration),
            12 => Ok(SecureValueType::PhoneNumber),
            13 => Ok(SecureValueType::EmailAddress),
            _ => Err(()),
        }
    }
}

impl TryFrom<i32> for BackgroundTypeType {
    type Error = ();

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(BackgroundTypeType::Wallpaper),
            1 => Ok(BackgroundTypeType::Pattern),
            2 => Ok(BackgroundTypeType::Fill),
            3 => Ok(BackgroundTypeType::ChatTheme),
            _ => Err(()),
        }
    }
}

impl TryFrom<i32> for CurrencyAmountType {
    type Error = ();

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(CurrencyAmountType::None),
            1 => Ok(CurrencyAmountType::Star),
            2 => Ok(CurrencyAmountType::Ton),
            _ => Err(()),
        }
    }
}

impl TryFrom<i32> for SuggestedPostPriceType {
    type Error = ();

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(SuggestedPostPriceType::None),
            1 => Ok(SuggestedPostPriceType::Star),
            2 => Ok(SuggestedPostPriceType::Ton),
            _ => Err(()),
        }
    }
}

impl TryFrom<i32> for MessageExtendedMediaType {
    type Error = ();

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(MessageExtendedMediaType::Empty),
            1 => Ok(MessageExtendedMediaType::Unsupported),
            2 => Ok(MessageExtendedMediaType::Preview),
            3 => Ok(MessageExtendedMediaType::Photo),
            4 => Ok(MessageExtendedMediaType::Video),
            _ => Err(()),
        }
    }
}

impl TryFrom<i32> for CallDiscardReasonType {
    type Error = ();

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(CallDiscardReasonType::Empty),
            1 => Ok(CallDiscardReasonType::Missed),
            2 => Ok(CallDiscardReasonType::Disconnected),
            3 => Ok(CallDiscardReasonType::HungUp),
            4 => Ok(CallDiscardReasonType::Declined),
            5 => Ok(CallDiscardReasonType::UpgradeToGroupCall),
            _ => Err(()),
        }
    }
}

/// `store(Dimensions, ...)` (Dimensions.hpp:16-18) — the packed
/// `(width << 16) | height` uint32.
fn store_dimensions(dimensions: Dimensions, out: &mut Vec<u8>) {
    store_u32(
        out,
        ((dimensions.width as u32) << 16) | dimensions.height as u32,
    );
}

/// `parse(Dimensions &, ...)` (Dimensions.hpp:21-26).
fn parse_dimensions(parser: &mut Parser) -> Result<Dimensions, &'static str> {
    let width_height = parser.u32_()?;
    Ok(Dimensions {
        width: (width_height >> 16) as u16,
        height: (width_height & 0xFFFF) as u16,
    })
}

/// `Location::store` (Location.h:81-98).
fn store_location(location: &Location, out: &mut Vec<u8>) {
    let has_access_hash = location.get_access_hash() != 0;
    let has_horizontal_accuracy = location.get_horizontal_accuracy() > 0.0;
    store_flags(
        out,
        [
            location.is_empty(),
            has_access_hash,
            has_horizontal_accuracy,
        ],
    );
    store_f64(out, location.get_latitude());
    store_f64(out, location.get_longitude());
    if has_access_hash {
        store_i64(out, location.get_access_hash());
    }
    if has_horizontal_accuracy {
        store_f64(out, location.get_horizontal_accuracy());
    }
}

/// `Location::parse` (Location.h:101-119). The C++ access-hash registration
/// side effect (`G()->add_location_access_hash`) belongs to the wire-intake
/// path and is not repeated here (the module doc records the split).
fn parse_location(parser: &mut Parser) -> Result<Location, &'static str> {
    let flags = parser.parse_flags::<3>()?;
    let latitude = parser.f64_()?;
    let longitude = parser.f64_()?;
    let mut access_hash = 0;
    if flags[1] {
        access_hash = parser.i64_()?;
    }
    let mut horizontal_accuracy = 0.0;
    if flags[2] {
        horizontal_accuracy = parser.f64_()?;
    }
    Ok(Location::from_stored(
        flags[0],
        latitude,
        longitude,
        horizontal_accuracy,
        access_hash,
    ))
}

/// `Venue::store` (Venue.h:69-77).
fn store_venue(venue: &Venue, out: &mut Vec<u8>) {
    store_location(&venue.location, out);
    store_string(out, &venue.title);
    store_string(out, &venue.address);
    store_string(out, &venue.provider);
    store_string(out, &venue.id);
    store_string(out, &venue.r#type);
}

/// `Venue::parse` (Venue.h:80-88).
fn parse_venue(parser: &mut Parser) -> Result<Venue, &'static str> {
    let location = parse_location(parser)?;
    let title = parser.string()?;
    let address = parser.string()?;
    let provider = parser.string()?;
    let id = parser.string()?;
    let r#type = parser.string()?;
    Ok(Venue {
        location,
        title,
        address,
        provider,
        id,
        r#type,
    })
}

/// `PhotoSize::store` (PhotoSize.hpp:20-26). The `file_id` store is
/// FileManager-mediated (the module doc records the M.12.2 gate).
fn store_photo_size(photo_size: &PhotoSize, ctx: &dyn MessageContentStorage, out: &mut Vec<u8>) {
    store_i32(out, photo_size.size_type);
    store_dimensions(photo_size.dimensions, out);
    store_i32(out, photo_size.size);
    ctx.store_file(photo_size.file_id, out);
    store_i32_vec(out, &photo_size.progressive_sizes);
}

/// `PhotoSize::parse` (PhotoSize.hpp:29-39).
fn parse_photo_size(
    parser: &mut Parser,
    ctx: &mut dyn MessageContentStorage,
) -> Result<PhotoSize, &'static str> {
    let size_type = parser.i32_()?;
    let dimensions = parse_dimensions(parser)?;
    let size = parser.i32_()?;
    let file_id = ctx.parse_file(parser)?;
    let progressive_sizes = if parser.version() >= version::ADD_PHOTO_PROGRESSIVE_SIZES {
        parser.i32_vec()?
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

/// `AnimationSize::store` (PhotoSize.hpp:42-45).
fn store_animation_size(
    animation_size: &AnimationSize,
    ctx: &dyn MessageContentStorage,
    out: &mut Vec<u8>,
) {
    store_photo_size(&animation_size.photo_size, ctx, out);
    store_f64(out, animation_size.main_frame_timestamp);
}

/// `AnimationSize::parse` (PhotoSize.hpp:48-55).
fn parse_animation_size(
    parser: &mut Parser,
    ctx: &mut dyn MessageContentStorage,
) -> Result<AnimationSize, &'static str> {
    let photo_size = parse_photo_size(parser, ctx)?;
    let main_frame_timestamp = if parser.version() >= version::ADD_DIALOG_PHOTO_HAS_ANIMATION {
        parser.f64_()?
    } else {
        0.0
    };
    Ok(AnimationSize {
        photo_size,
        main_frame_timestamp,
    })
}

/// `StickerPhotoSize::store` (StickerPhotoSize.hpp:16-30) — lives on the
/// value in `photo.rs`.
fn store_sticker_photo_size(size: &StickerPhotoSize, out: &mut Vec<u8>) {
    size.store(out);
}

/// `StickerPhotoSize::parse` (StickerPhotoSize.hpp:33-51).
fn parse_sticker_photo_size(parser: &mut Parser) -> Result<StickerPhotoSize, &'static str> {
    StickerPhotoSize::parse_from(parser)
}

/// `Photo::is_bad` (Photo.h:49-59).
fn photo_is_bad(photo: &Photo) -> bool {
    if photo.is_empty() {
        return true;
    }
    for photo_size in &photo.photos {
        if !photo_size.file_id.is_valid() {
            return true;
        }
    }
    false
}

/// `store(const Photo &, ...)` (Photo.hpp:19-44). `sticker_file_ids` has no
/// Rust model yet (photo.rs documents the stand-in): the store writes the
/// empty vector, keeping the layout parseable.
fn store_photo(photo: &Photo, ctx: &dyn MessageContentStorage, out: &mut Vec<u8>) {
    let has_minithumbnail = !photo.minithumbnail.is_empty();
    let has_animations = !photo.animations.is_empty();
    let has_sticker_photo_size = photo.sticker_photo_size.is_some();
    store_flags(
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
        store_i32(out, 0); // sticker_file_ids — not modeled yet
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
    if has_sticker_photo_size {
        store_sticker_photo_size(photo.sticker_photo_size.as_ref().unwrap(), out);
    }
}

/// `parse(Photo &, ...)` (Photo.hpp:47-74). `sticker_file_ids` is
/// read-and-discarded while the has_stickers vector is present.
fn parse_photo(
    parser: &mut Parser,
    ctx: &mut dyn MessageContentStorage,
) -> Result<Photo, &'static str> {
    let flags = parser.parse_flags::<4>()?;
    let id = parser.i64_()?;
    let date = parser.i32_()?;
    let photos_len = parser.i32_()?;
    if photos_len < 0 || parser.left_len() < photos_len as usize {
        return Err("Wrong vector length");
    }
    let mut photos = Vec::with_capacity(photos_len as usize);
    for _ in 0..photos_len {
        photos.push(parse_photo_size(parser, ctx)?);
    }
    if flags[0] {
        let sticker_file_ids_len = parser.i32_()?;
        if sticker_file_ids_len < 0 || parser.left_len() < sticker_file_ids_len as usize {
            return Err("Wrong vector length");
        }
        for _ in 0..sticker_file_ids_len {
            ctx.parse_file(parser)?;
        }
    }
    let minithumbnail = if flags[1] {
        parser.string()?
    } else {
        String::new()
    };
    let animations_len = if flags[2] { parser.i32_()? } else { 0 };
    if animations_len < 0 || parser.left_len() < animations_len as usize {
        return Err("Wrong vector length");
    }
    let mut animations = Vec::with_capacity(animations_len as usize);
    for _ in 0..animations_len {
        animations.push(parse_animation_size(parser, ctx)?);
    }
    let sticker_photo_size = if flags[3] {
        Some(parse_sticker_photo_size(parser)?)
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
        has_stickers: flags[0],
    })
}

/// `Game::store` (Game.hpp:22-39).
fn store_game(game: &Game, ctx: &dyn MessageContentStorage, out: &mut Vec<u8>) {
    let has_animation = game.animation_file_id.is_valid();
    store_flags(out, [has_animation]);
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

/// `Game::parse` (Game.hpp:42-61).
fn parse_game(
    parser: &mut Parser,
    ctx: &mut dyn MessageContentStorage,
) -> Result<Game, &'static str> {
    let has_animation = if parser.version() >= version::FIX_STORE_GAME_WITHOUT_ANIMATION {
        parser.parse_flags::<1>()?[0]
    } else {
        true
    };
    let id = parser.i64_()?;
    let access_hash = parser.i64_()?;
    let bot_user_id = parse_user_id(parser)?;
    let short_name = parser.string()?;
    let title = parser.string()?;
    let description = parser.string()?;
    let photo = parse_photo(parser, ctx)?;
    let animation_file_id = if has_animation {
        ctx.parse_animation(parser)?
    } else {
        FileId::default()
    };
    let text = parse_formatted_text(parser)?;
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

/// `WebApp::store` (WebApp.hpp:21-37).
fn store_web_app(web_app: &WebApp, ctx: &dyn MessageContentStorage, out: &mut Vec<u8>) {
    let has_animation = web_app.animation_file_id.is_valid();
    store_flags(out, [has_animation]);
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

/// `WebApp::parse` (WebApp.hpp:40-56).
fn parse_web_app(
    parser: &mut Parser,
    ctx: &mut dyn MessageContentStorage,
) -> Result<WebApp, &'static str> {
    let has_animation = parser.parse_flags::<1>()?[0];
    let id = parser.i64_()?;
    let access_hash = parser.i64_()?;
    let short_name = parser.string()?;
    let title = parser.string()?;
    let description = parser.string()?;
    let photo = parse_photo(parser, ctx)?;
    let animation_file_id = if has_animation {
        ctx.parse_animation(parser)?
    } else {
        FileId::default()
    };
    let hash = parser.i64_()?;
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

/// `BackgroundType::store` (BackgroundType.hpp:16-52).
fn store_background_type(background_type: &BackgroundType, out: &mut Vec<u8>) {
    let has_fill = background_type.fill.top_color != 0 || background_type.fill.bottom_color != 0;
    let has_intensity = background_type.intensity != 0;
    let fill_type = background_type.fill.get_type();
    let is_gradient = fill_type == BackgroundFillType::Gradient;
    let is_freeform_gradient = fill_type == BackgroundFillType::FreeformGradient;
    let has_theme_name = !background_type.theme_name.is_empty();
    store_flags(
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

/// `BackgroundType::parse` (BackgroundType.hpp:55-92+). An out-of-range
/// type int (garbage in a corrupted blob; the C++ cast would hold it
/// unchecked) falls back to the enum default with a log.
fn parse_background_type(parser: &mut Parser) -> Result<BackgroundType, &'static str> {
    let flags = parser.parse_flags::<7>()?;
    let type_raw = parser.i32_()?;
    let r#type = match BackgroundTypeType::try_from(type_raw) {
        Ok(value) => value,
        Err(_) => {
            tdutils::log_error!("Wrong background type {type_raw}");
            BackgroundTypeType::default()
        }
    };
    let mut fill = BackgroundFill::default();
    if flags[5] {
        fill.top_color = parser.i32_()?;
        fill.bottom_color = parser.i32_()?;
        fill.third_color = parser.i32_()?;
        fill.fourth_color = parser.i32_()?;
    } else if flags[2] {
        fill.top_color = parser.i32_()?;
        if flags[4] {
            fill.bottom_color = parser.i32_()?;
            fill.rotation_angle = parser.i32_()?;
        } else {
            fill.bottom_color = fill.top_color;
        }
    }
    let intensity = if flags[3] { parser.i32_()? } else { 0 };
    let theme_name = if flags[6] {
        parser.string()?
    } else {
        String::new()
    };
    Ok(BackgroundType {
        r#type,
        is_blurred: flags[0],
        is_moving: flags[1],
        intensity,
        fill,
        theme_name,
    })
}

/// `BackgroundInfo::store` (BackgroundInfo.hpp:19-22). The id goes through
/// the BackgroundManager seam; the type is stored inline.
fn store_background_info(
    background_info: &BackgroundInfo,
    ctx: &dyn MessageContentStorage,
    out: &mut Vec<u8>,
) {
    ctx.store_background(background_info.background_id, out);
    store_background_type(&background_info.background_type, out);
}

/// `BackgroundInfo::parse` (BackgroundInfo.hpp:25-28).
fn parse_background_info(
    parser: &mut Parser,
    ctx: &mut dyn MessageContentStorage,
) -> Result<BackgroundInfo, &'static str> {
    let background_id = ctx.parse_background(parser)?;
    let background_type = parse_background_type(parser)?;
    Ok(BackgroundInfo {
        background_id,
        background_type,
    })
}

/// `GiveawayParameters::store` (GiveawayParameters.hpp:16-38). The
/// `ChannelId` store is the raw `store_long`.
fn store_giveaway_parameters(parameters: &GiveawayParameters, out: &mut Vec<u8>) {
    let has_additional_channel_ids = !parameters.additional_channel_ids.is_empty();
    let has_country_codes = !parameters.country_codes.is_empty();
    let has_prize_description = !parameters.prize_description.is_empty();
    store_flags(
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

/// `GiveawayParameters::parse` (GiveawayParameters.hpp:41-63).
fn parse_giveaway_parameters(parser: &mut Parser) -> Result<GiveawayParameters, &'static str> {
    let flags = parser.parse_flags::<5>()?;
    let boosted_channel_id = parse_channel_id(parser)?;
    let additional_channel_ids = if flags[1] {
        let len = parser.i32_()?;
        if len < 0 || parser.left_len() < len as usize {
            return Err("Wrong vector length");
        }
        let mut channel_ids = Vec::with_capacity(len as usize);
        for _ in 0..len {
            channel_ids.push(parse_channel_id(parser)?);
        }
        channel_ids
    } else {
        Vec::new()
    };
    let date = parser.i32_()?;
    let country_codes = if flags[2] {
        parser.string_vec()?
    } else {
        Vec::new()
    };
    let prize_description = if flags[4] {
        parser.string()?
    } else {
        String::new()
    };
    Ok(GiveawayParameters {
        boosted_channel_id,
        additional_channel_ids,
        only_new_subscribers: flags[0],
        winners_are_visible: flags[3],
        date,
        country_codes,
        prize_description,
    })
}

/// `ForumTopicIcon::store` (ForumTopicIcon.hpp:17-26).
fn store_forum_topic_icon(icon: &ForumTopicIcon, out: &mut Vec<u8>) {
    let has_custom_emoji_id = icon.get_custom_emoji_id().get() != 0;
    store_flags(out, [has_custom_emoji_id]);
    store_i32(out, icon.get_color());
    if has_custom_emoji_id {
        store_i64(out, icon.get_custom_emoji_id().get());
    }
}

/// `ForumTopicIcon::parse` (ForumTopicIcon.hpp:29-38).
fn parse_forum_topic_icon(parser: &mut Parser) -> Result<ForumTopicIcon, &'static str> {
    let has_custom_emoji_id = parser.parse_flags::<1>()?[0];
    let color = parser.i32_()?;
    let mut icon = ForumTopicIcon::new(color, CustomEmojiId::default());
    if has_custom_emoji_id {
        icon.edit_custom_emoji_id(CustomEmojiId::new(parser.i64_()?));
    }
    Ok(icon)
}

/// `ForumTopicEditedData::store` (ForumTopicEditedData.hpp:17-34).
fn store_forum_topic_edited_data(data: &ForumTopicEditedData, out: &mut Vec<u8>) {
    let has_title = !data.title.is_empty();
    let has_icon_custom_emoji_id = data.icon_custom_emoji_id.get() != 0;
    store_flags(
        out,
        [
            data.edit_icon_custom_emoji_id,
            data.edit_is_closed,
            data.is_closed,
            has_title,
            has_icon_custom_emoji_id,
            data.is_hidden,
        ],
    );
    if has_title {
        store_string(out, &data.title);
    }
    if has_icon_custom_emoji_id {
        store_i64(out, data.icon_custom_emoji_id.get());
    }
}

/// `ForumTopicEditedData::parse` (ForumTopicEditedData.hpp:37-54).
fn parse_forum_topic_edited_data(
    parser: &mut Parser,
) -> Result<ForumTopicEditedData, &'static str> {
    let flags = parser.parse_flags::<6>()?;
    let title = if flags[3] {
        parser.string()?
    } else {
        String::new()
    };
    let icon_custom_emoji_id = if flags[4] {
        CustomEmojiId::new(parser.i64_()?)
    } else {
        CustomEmojiId::default()
    };
    Ok(ForumTopicEditedData {
        title,
        icon_custom_emoji_id,
        edit_icon_custom_emoji_id: flags[0],
        edit_is_closed: flags[1],
        is_closed: flags[2],
        edit_is_hidden: flags[5],
        is_hidden: flags[5],
    })
}

/// `ToDoItem::store` (ToDoItem.hpp:18-23).
fn store_to_do_item(item: &ToDoItem, out: &mut Vec<u8>) {
    store_flags::<0>(out, []);
    store_i32(out, item.id);
    item.title.store(out);
}

/// `ToDoItem::parse` (ToDoItem.hpp:26-32). The C++ `validate(0, "parse")`
/// tail mutates only under `check_utf8` failures a Rust `String` cannot
/// carry, so it is a no-op here.
fn parse_to_do_item(parser: &mut Parser) -> Result<ToDoItem, &'static str> {
    parser.parse_flags::<0>()?;
    let id = parser.i32_()?;
    let title = parse_formatted_text(parser)?;
    Ok(ToDoItem { id, title })
}

/// `ToDoList::store` (ToDoList.hpp:19-26).
fn store_to_do_list(list: &ToDoList, out: &mut Vec<u8>) {
    store_flags(out, [list.others_can_append, list.others_can_complete]);
    list.title.store(out);
    store_i32(out, list.items.len() as i32);
    for item in &list.items {
        store_to_do_item(item, out);
    }
}

/// `ToDoList::parse` (ToDoList.hpp:29-37). The C++ `validate` tail drops
/// unallowed quote entities from the title; the entity filter lands with
/// the Wave-2 to-do consumers (logged divergence, layout-identical).
fn parse_to_do_list(parser: &mut Parser) -> Result<ToDoList, &'static str> {
    let flags = parser.parse_flags::<2>()?;
    let title = parse_formatted_text(parser)?;
    let items_len = parser.i32_()?;
    if items_len < 0 || parser.left_len() < items_len as usize {
        return Err("Wrong vector length");
    }
    let mut items = Vec::with_capacity(items_len as usize);
    for _ in 0..items_len {
        items.push(parse_to_do_item(parser)?);
    }
    Ok(ToDoList {
        title,
        items,
        others_can_append: flags[0],
        others_can_complete: flags[1],
    })
}

/// `ToDoCompletion::store` (ToDoCompletion.hpp:17-29).
fn store_to_do_completion(completion: &ToDoCompletion, out: &mut Vec<u8>) {
    let is_completed_by_dialog =
        completion.completed_by_dialog_id.get_type() != crate::messages_manager::DialogType::User;
    store_flags(out, [is_completed_by_dialog]);
    store_i32(out, completion.id);
    if is_completed_by_dialog {
        store_i64(out, completion.completed_by_dialog_id.get());
    } else {
        store_i64(out, completion.completed_by_dialog_id.get_user_id().get());
    }
    store_i32(out, completion.date);
}

/// `ToDoCompletion::parse` (ToDoCompletion.hpp:32-46).
fn parse_to_do_completion(parser: &mut Parser) -> Result<ToDoCompletion, &'static str> {
    let is_completed_by_dialog = parser.parse_flags::<1>()?[0];
    let id = parser.i32_()?;
    let completed_by_dialog_id = if is_completed_by_dialog {
        parse_dialog_id(parser)?
    } else {
        DialogId::from_user_id(parse_user_id(parser)?)
    };
    let date = parser.i32_()?;
    Ok(ToDoCompletion {
        id,
        completed_by_dialog_id,
        date,
    })
}

/// `store(DatedFile, ...)` (SecureValue.hpp:18-21).
fn store_dated_file(file: &DatedFile, ctx: &dyn MessageContentStorage, out: &mut Vec<u8>) {
    ctx.store_file(file.file_id, out);
    store_i32(out, file.date);
}

/// `parse(DatedFile &, ...)`.
fn parse_dated_file(
    parser: &mut Parser,
    ctx: &mut dyn MessageContentStorage,
) -> Result<DatedFile, &'static str> {
    let file_id = ctx.parse_file(parser)?;
    let date = parser.i32_()?;
    Ok(DatedFile { file_id, date })
}

/// `store(EncryptedSecureFile, ...)` (SecureValue.hpp:30-34).
fn store_encrypted_secure_file(
    file: &EncryptedSecureFile,
    ctx: &dyn MessageContentStorage,
    out: &mut Vec<u8>,
) {
    store_dated_file(&file.file, ctx, out);
    store_string(out, &file.file_hash);
    store_string(out, &file.encrypted_secret);
}

/// `parse(EncryptedSecureFile &, ...)`.
fn parse_encrypted_secure_file(
    parser: &mut Parser,
    ctx: &mut dyn MessageContentStorage,
) -> Result<EncryptedSecureFile, &'static str> {
    let file = parse_dated_file(parser, ctx)?;
    let file_hash = parser.string()?;
    let encrypted_secret = parser.string()?;
    Ok(EncryptedSecureFile {
        file,
        file_hash,
        encrypted_secret,
    })
}

/// `store(const EncryptedSecureData &, ...)` (SecureValue.hpp:44-48).
fn store_encrypted_secure_data(data: &EncryptedSecureData, out: &mut Vec<u8>) {
    store_string(out, &data.data);
    store_string(out, &data.hash);
    store_string(out, &data.encrypted_secret);
}

/// `parse(const EncryptedSecureData &, ...)`.
fn parse_encrypted_secure_data(parser: &mut Parser) -> Result<EncryptedSecureData, &'static str> {
    let data = parser.string()?;
    let hash = parser.string()?;
    let encrypted_secret = parser.string()?;
    Ok(EncryptedSecureData {
        data,
        hash,
        encrypted_secret,
    })
}

/// `store(const EncryptedSecureValue &, ...)` (SecureValue.hpp:72-113).
fn store_encrypted_secure_value(
    value: &EncryptedSecureValue,
    ctx: &dyn MessageContentStorage,
    out: &mut Vec<u8>,
) {
    let has_data_hash = !value.data.hash.is_empty();
    let has_files = !value.files.is_empty();
    let has_front_side = value.front_side.file.file_id.is_valid();
    let has_reverse_side = value.reverse_side.file.file_id.is_valid();
    let has_selfie = value.selfie.file.file_id.is_valid();
    let has_hash = !value.hash.is_empty();
    let has_translations = !value.translations.is_empty();
    store_flags(
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

/// `parse(const EncryptedSecureValue &, ...)` (SecureValue.hpp:116-157).
fn parse_encrypted_secure_value(
    parser: &mut Parser,
    ctx: &mut dyn MessageContentStorage,
) -> Result<EncryptedSecureValue, &'static str> {
    let flags = parser.parse_flags::<7>()?;
    let type_raw = parser.i32_()?;
    let r#type = match SecureValueType::try_from(type_raw) {
        Ok(value) => value,
        Err(_) => {
            tdutils::log_error!("Wrong secure value type {type_raw}");
            SecureValueType::None
        }
    };
    let data = if flags[0] {
        parse_encrypted_secure_data(parser)?
    } else {
        EncryptedSecureData {
            data: parser.string()?,
            hash: String::new(),
            encrypted_secret: String::new(),
        }
    };
    let files_len = if flags[1] { parser.i32_()? } else { 0 };
    if files_len < 0 || parser.left_len() < files_len as usize {
        return Err("Wrong vector length");
    }
    let mut files = Vec::with_capacity(files_len as usize);
    for _ in 0..files_len {
        files.push(parse_encrypted_secure_file(parser, ctx)?);
    }
    let front_side = if flags[2] {
        parse_encrypted_secure_file(parser, ctx)?
    } else {
        EncryptedSecureFile::default()
    };
    let reverse_side = if flags[3] {
        parse_encrypted_secure_file(parser, ctx)?
    } else {
        EncryptedSecureFile::default()
    };
    let selfie = if flags[4] {
        parse_encrypted_secure_file(parser, ctx)?
    } else {
        EncryptedSecureFile::default()
    };
    let hash = if flags[5] {
        parser.string()?
    } else {
        String::new()
    };
    let translations_len = if flags[6] { parser.i32_()? } else { 0 };
    if translations_len < 0 || parser.left_len() < translations_len as usize {
        return Err("Wrong vector length");
    }
    let mut translations = Vec::with_capacity(translations_len as usize);
    for _ in 0..translations_len {
        translations.push(parse_encrypted_secure_file(parser, ctx)?);
    }
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

/// `store(const EncryptedSecureCredentials &, ...)` (SecureValue.hpp:58-62).
fn store_encrypted_secure_credentials(credentials: &EncryptedSecureCredentials, out: &mut Vec<u8>) {
    store_string(out, &credentials.data);
    store_string(out, &credentials.hash);
    store_string(out, &credentials.encrypted_secret);
}

/// `parse(const EncryptedSecureCredentials &, ...)`.
fn parse_encrypted_secure_credentials(
    parser: &mut Parser,
) -> Result<EncryptedSecureCredentials, &'static str> {
    let data = parser.string()?;
    let hash = parser.string()?;
    let encrypted_secret = parser.string()?;
    Ok(EncryptedSecureCredentials {
        data,
        hash,
        encrypted_secret,
    })
}

/// `StarAmount::store` (StarAmount.hpp).
fn store_star_amount(amount: &StarAmount, out: &mut Vec<u8>) {
    store_i64(out, amount.star_count);
    store_i32(out, amount.nanostar_count);
}

/// `StarAmount::parse`.
fn parse_star_amount(parser: &mut Parser) -> Result<StarAmount, &'static str> {
    let star_count = parser.i64_()?;
    let nanostar_count = parser.i32_()?;
    Ok(StarAmount {
        star_count,
        nanostar_count,
    })
}

/// `TonAmount::store` (TonAmount.hpp).
fn store_ton_amount(amount: &TonAmount, out: &mut Vec<u8>) {
    store_i64(out, amount.ton_amount);
}

/// `TonAmount::parse`.
fn parse_ton_amount(parser: &mut Parser) -> Result<TonAmount, &'static str> {
    Ok(TonAmount {
        ton_amount: parser.i64_()?,
    })
}

/// `CurrencyAmount::store` (CurrencyAmount.hpp:18-32).
fn store_currency_amount(amount: &CurrencyAmount, out: &mut Vec<u8>) {
    let has_star_amount = amount.star_amount != StarAmount::default();
    let has_ton_amount = amount.ton_amount != TonAmount::default();
    store_flags(out, [has_star_amount, has_ton_amount]);
    store_i32(out, amount.r#type as i32);
    if has_star_amount {
        store_star_amount(&amount.star_amount, out);
    }
    if has_ton_amount {
        store_ton_amount(&amount.ton_amount, out);
    }
}

/// `CurrencyAmount::parse` (CurrencyAmount.hpp:35-49).
fn parse_currency_amount(parser: &mut Parser) -> Result<CurrencyAmount, &'static str> {
    let flags = parser.parse_flags::<2>()?;
    let type_raw = parser.i32_()?;
    let r#type = match CurrencyAmountType::try_from(type_raw) {
        Ok(value) => value,
        Err(_) => {
            tdutils::log_error!("Wrong currency amount type {type_raw}");
            CurrencyAmountType::None
        }
    };
    let star_amount = if flags[0] {
        parse_star_amount(parser)?
    } else {
        StarAmount::default()
    };
    let ton_amount = if flags[1] {
        parse_ton_amount(parser)?
    } else {
        TonAmount::default()
    };
    Ok(CurrencyAmount {
        r#type,
        star_amount,
        ton_amount,
    })
}

/// `SuggestedPostPrice::store` (SuggestedPostPrice.hpp:16-25).
fn store_suggested_post_price(price: &SuggestedPostPrice, out: &mut Vec<u8>) {
    let has_amount = price.amount != 0;
    store_flags(out, [has_amount]);
    store_i32(out, price.r#type as i32);
    if has_amount {
        store_i64(out, price.amount);
    }
}

/// `SuggestedPostPrice::parse` (SuggestedPostPrice.hpp:28-37).
fn parse_suggested_post_price(parser: &mut Parser) -> Result<SuggestedPostPrice, &'static str> {
    let has_amount = parser.parse_flags::<1>()?[0];
    let type_raw = parser.i32_()?;
    let r#type = match SuggestedPostPriceType::try_from(type_raw) {
        Ok(value) => value,
        Err(_) => {
            tdutils::log_error!("Wrong suggested post price type {type_raw}");
            SuggestedPostPriceType::None
        }
    };
    let amount = if has_amount { parser.i64_()? } else { 0 };
    Ok(SuggestedPostPrice { r#type, amount })
}

/// `MessageExtendedMedia::store` (MessageExtendedMedia.hpp:20-61).
fn store_message_extended_media(
    media: &MessageExtendedMedia,
    ctx: &dyn MessageContentStorage,
    out: &mut Vec<u8>,
) {
    let has_unsupported_version = media.unsupported_version != 0;
    let has_duration = media.duration != 0;
    let has_dimensions = media.dimensions.width != 0 || media.dimensions.height != 0;
    let has_minithumbnail = !media.minithumbnail.is_empty();
    let has_photo = !media.photo.is_empty();
    let has_video = media.video_file_id.is_valid();
    let has_start_timestamp = media.start_timestamp != 0;
    store_flags(
        out,
        [
            false, // has_caption — the legacy field, never stored
            has_unsupported_version,
            has_duration,
            has_dimensions,
            has_minithumbnail,
            has_photo,
            has_video,
            has_start_timestamp,
        ],
    );
    store_i32(out, media.r#type as i32);
    if has_unsupported_version {
        store_i32(out, media.unsupported_version);
    }
    if has_duration {
        store_i32(out, media.duration);
    }
    if has_dimensions {
        store_dimensions(media.dimensions, out);
    }
    if has_minithumbnail {
        store_string(out, &media.minithumbnail);
    }
    if has_photo {
        store_photo(&media.photo, ctx, out);
    }
    if has_video {
        ctx.store_video(media.video_file_id, out);
    }
    if has_start_timestamp {
        store_i32(out, media.start_timestamp);
    }
}

/// `MessageExtendedMedia::parse` (MessageExtendedMedia.hpp:64-127).
fn parse_message_extended_media(
    parser: &mut Parser,
    ctx: &mut dyn MessageContentStorage,
) -> Result<MessageExtendedMedia, &'static str> {
    let flags = parser.parse_flags::<8>()?;
    let type_raw = parser.i32_()?;
    let r#type = match MessageExtendedMediaType::try_from(type_raw) {
        Ok(value) => value,
        Err(_) => {
            tdutils::log_error!("Wrong extended media type {type_raw}");
            MessageExtendedMediaType::Empty
        }
    };
    if flags[0] {
        // has_caption — parsed and dropped, like the C++ local.
        parse_formatted_text(parser)?;
    }
    let unsupported_version = if flags[1] { parser.i32_()? } else { 0 };
    let duration = if flags[2] { parser.i32_()? } else { 0 };
    let dimensions = if flags[3] {
        parse_dimensions(parser)?
    } else {
        Dimensions::default()
    };
    let minithumbnail = if flags[4] {
        parser.string()?
    } else {
        String::new()
    };
    let mut is_bad = false;
    let photo = if flags[5] {
        let photo = parse_photo(parser, ctx)?;
        is_bad |= photo_is_bad(&photo);
        photo
    } else {
        if r#type == MessageExtendedMediaType::Photo {
            is_bad = true;
        }
        Photo::default()
    };
    let video_file_id = if flags[6] {
        let file_id = ctx.parse_video(parser)?;
        is_bad = !file_id.is_valid();
        file_id
    } else {
        if r#type == MessageExtendedMediaType::Video {
            is_bad = true;
        }
        FileId::default()
    };
    let start_timestamp = if flags[7] { parser.i32_()? } else { 0 };
    if is_bad || flags[0] {
        if is_bad {
            tdutils::log_error!("Failed to parse MessageExtendedMedia");
        }
        return Ok(MessageExtendedMedia {
            r#type: MessageExtendedMediaType::Unsupported,
            unsupported_version: 0,
            duration,
            dimensions,
            minithumbnail,
            photo: Photo::default(),
            video_file_id: FileId::default(),
            start_timestamp,
        });
    }
    Ok(MessageExtendedMedia {
        r#type,
        unsupported_version,
        duration,
        dimensions,
        minithumbnail,
        photo,
        video_file_id,
        start_timestamp,
    })
}

/// `InputInvoice::Invoice::store` (InputInvoice.hpp:21-56).
fn store_invoice(invoice: &Invoice, out: &mut Vec<u8>) {
    let has_tip = invoice.max_tip_amount != 0;
    let is_recurring = !invoice.recurring_payment_terms_of_service_url.is_empty();
    let has_terms_of_service = !invoice.terms_of_service_url.is_empty();
    let has_subscription_period = invoice.subscription_period != 0;
    store_flags(
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
        // `LabeledPricePart::store` (LabeledPricePart.h:41-44).
        store_string(out, &part.label);
        store_i64(out, part.amount);
    }
    if has_tip {
        store_i64(out, invoice.max_tip_amount);
        store_i32(out, invoice.suggested_tip_amounts.len() as i32);
        for amount in &invoice.suggested_tip_amounts {
            store_i64(out, *amount);
        }
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

/// `InputInvoice::Invoice::parse` (InputInvoice.hpp:59-94).
fn parse_invoice(parser: &mut Parser) -> Result<Invoice, &'static str> {
    let flags = parser.parse_flags::<12>()?;
    let currency = parser.string()?;
    let price_parts_len = parser.i32_()?;
    if price_parts_len < 0 || parser.left_len() < price_parts_len as usize {
        return Err("Wrong vector length");
    }
    let mut price_parts = Vec::with_capacity(price_parts_len as usize);
    for _ in 0..price_parts_len {
        let label = parser.string()?;
        let amount = parser.i64_()?;
        price_parts.push(LabeledPricePart { label, amount });
    }
    let max_tip_amount = if flags[8] { parser.i64_()? } else { 0 };
    let suggested_tip_amounts = if flags[8] {
        let len = parser.i32_()?;
        if len < 0 || parser.left_len() < len as usize {
            return Err("Wrong vector length");
        }
        let mut amounts = Vec::with_capacity(len as usize);
        for _ in 0..len {
            amounts.push(parser.i64_()?);
        }
        amounts
    } else {
        Vec::new()
    };
    let recurring_payment_terms_of_service_url = if flags[9] {
        parser.string()?
    } else {
        String::new()
    };
    let terms_of_service_url = if flags[10] {
        parser.string()?
    } else {
        String::new()
    };
    let subscription_period = if flags[11] { parser.i32_()? } else { 0 };
    Ok(Invoice {
        currency,
        price_parts,
        subscription_period,
        max_tip_amount,
        suggested_tip_amounts,
        recurring_payment_terms_of_service_url,
        terms_of_service_url,
        is_test: flags[0],
        need_name: flags[1],
        need_phone_number: flags[2],
        need_email_address: flags[3],
        need_shipping_address: flags[4],
        send_phone_number_to_provider: flags[6],
        send_email_address_to_provider: flags[7],
        is_flexible: flags[5],
    })
}

/// `InputInvoice::store` (InputInvoice.hpp:97-153).
fn store_input_invoice(invoice: &InputInvoice, ctx: &dyn MessageContentStorage, out: &mut Vec<u8>) {
    let has_description = !invoice.description.is_empty();
    let has_photo = !invoice.photo.is_empty();
    let has_start_parameter = !invoice.start_parameter.is_empty();
    let has_payload = !invoice.payload.is_empty();
    let has_provider_token = !invoice.provider_token.is_empty();
    let has_provider_data = !invoice.provider_data.is_empty();
    let has_total_amount = invoice.total_amount != 0;
    let has_receipt_message_id = invoice.receipt_message_id.is_valid();
    let has_extended_media = !invoice.extended_media.is_empty();
    let has_extended_media_caption = !invoice.extended_media_caption.text.is_empty();
    store_flags(
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
    store_string(out, &invoice.title);
    if has_description {
        store_string(out, &invoice.description);
    }
    if has_photo {
        store_photo(&invoice.photo, ctx, out);
    }
    if has_start_parameter {
        store_string(out, &invoice.start_parameter);
    }
    store_invoice(&invoice.invoice, out);
    if has_payload {
        store_string(out, &invoice.payload);
    }
    if has_provider_token {
        store_string(out, &invoice.provider_token);
    }
    if has_provider_data {
        store_string(out, &invoice.provider_data);
    }
    if has_total_amount {
        store_i64(out, invoice.total_amount);
    }
    if has_receipt_message_id {
        store_i64(out, invoice.receipt_message_id.get());
    }
    if has_extended_media {
        store_message_extended_media(&invoice.extended_media, ctx, out);
    }
    if has_extended_media_caption {
        invoice.extended_media_caption.store(out);
    }
}

/// `InputInvoice::parse` (InputInvoice.hpp:156-224) with the legacy
/// pre-`AddInputInvoiceFlags` branch.
fn parse_input_invoice(
    parser: &mut Parser,
    ctx: &mut dyn MessageContentStorage,
) -> Result<InputInvoice, &'static str> {
    let (flags, has_provider_data) = if parser.version() >= version::ADD_INPUT_INVOICE_FLAGS {
        (parser.parse_flags::<10>()?, true)
    } else {
        (
            [true, true, true, true, true, true, true, true, false, false],
            parser.version() >= version::ADD_MESSAGE_INVOICE_PROVIDER_DATA,
        )
    };
    let title = parser.string()?;
    let description = if flags[0] {
        parser.string()?
    } else {
        String::new()
    };
    let photo = if flags[1] {
        parse_photo(parser, ctx)?
    } else {
        Photo::default()
    };
    let start_parameter = if flags[2] {
        parser.string()?
    } else {
        String::new()
    };
    let invoice = parse_invoice(parser)?;
    let payload = if flags[3] {
        parser.string()?
    } else {
        String::new()
    };
    let provider_token = if flags[4] {
        parser.string()?
    } else {
        String::new()
    };
    let provider_data = if has_provider_data {
        parser.string()?
    } else {
        String::new()
    };
    let total_amount = if flags[6] { parser.i64_()? } else { 0 };
    let receipt_message_id = if flags[7] {
        parse_message_id(parser)?
    } else {
        MessageId::default()
    };
    let extended_media = if flags[8] {
        parse_message_extended_media(parser, ctx)?
    } else {
        MessageExtendedMedia::default()
    };
    let extended_media_caption = if flags[9] {
        parse_formatted_text(parser)?
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

/// `store(const Address &)`/`store(const OrderInfo &)`
/// (OrderInfo.hpp:16-59).
fn store_order_info(order_info: &OrderInfo, out: &mut Vec<u8>) {
    let has_name = !order_info.name.is_empty();
    let has_phone_number = !order_info.phone_number.is_empty();
    let has_email_address = !order_info.email_address.is_empty();
    let has_shipping_address = order_info.shipping_address.is_some();
    store_flags(
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
        store_string(out, &shipping_address.country_code);
        store_string(out, &shipping_address.state);
        store_string(out, &shipping_address.city);
        store_string(out, &shipping_address.street_line1);
        store_string(out, &shipping_address.street_line2);
        store_string(out, &shipping_address.postal_code);
    }
}

/// `parse(Address &)`/`parse(OrderInfo &)` (OrderInfo.hpp:26-85).
fn parse_order_info(parser: &mut Parser) -> Result<OrderInfo, &'static str> {
    let flags = parser.parse_flags::<4>()?;
    let name = if flags[0] {
        parser.string()?
    } else {
        String::new()
    };
    let phone_number = if flags[1] {
        parser.string()?
    } else {
        String::new()
    };
    let email_address = if flags[2] {
        parser.string()?
    } else {
        String::new()
    };
    let shipping_address = if flags[3] {
        Some(Box::new(Address {
            country_code: parser.string()?,
            state: parser.string()?,
            city: parser.string()?,
            street_line1: parser.string()?,
            street_line2: parser.string()?,
            postal_code: parser.string()?,
        }))
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

/// `SharedDialog::store` (SharedDialog.hpp:16-43).
fn store_shared_dialog(
    shared_dialog: &SharedDialog,
    ctx: &dyn MessageContentStorage,
    out: &mut Vec<u8>,
) {
    let has_first_name = !shared_dialog.first_name.is_empty();
    let has_last_name = !shared_dialog.last_name.is_empty();
    let has_username = !shared_dialog.username.is_empty();
    let has_photo = !shared_dialog.photo.is_empty();
    store_flags(
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

/// `SharedDialog::parse` (SharedDialog.hpp:46-72).
fn parse_shared_dialog(
    parser: &mut Parser,
    ctx: &mut dyn MessageContentStorage,
) -> Result<SharedDialog, &'static str> {
    let flags = parser.parse_flags::<4>()?;
    let dialog_id = parse_dialog_id(parser)?;
    let first_name = if flags[0] {
        parser.string()?
    } else {
        String::new()
    };
    let last_name = if flags[1] {
        parser.string()?
    } else {
        String::new()
    };
    let username = if flags[2] {
        parser.string()?
    } else {
        String::new()
    };
    let photo = if flags[3] {
        parse_photo(parser, ctx)?
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

/// The id-wrapper stores: `store_long`/`fetch_long` with the pre-64-bit
/// int32 legacy the C++ wrappers keep (`UserId.h:57-64`, `ChannelId.h:61`,
/// `ChatId.h:54`); `DialogId`/`MessageId`/`WebPageId` are unconditionally
/// 64-bit.
fn store_user_id(user_id: UserId, out: &mut Vec<u8>) {
    store_i64(out, user_id.get());
}

fn parse_user_id(parser: &mut Parser) -> Result<UserId, &'static str> {
    Ok(UserId::new(
        if parser.version() >= version::SUPPORT_64_BIT_IDS {
            parser.i64_()?
        } else {
            parser.i32_()? as i64
        },
    ))
}

fn store_channel_id(channel_id: ChannelId, out: &mut Vec<u8>) {
    store_i64(out, channel_id.get());
}

fn parse_channel_id(parser: &mut Parser) -> Result<ChannelId, &'static str> {
    Ok(ChannelId::new(
        if parser.version() >= version::SUPPORT_64_BIT_IDS {
            parser.i64_()?
        } else {
            parser.i32_()? as i64
        },
    ))
}

fn store_chat_id(chat_id: ChatId, out: &mut Vec<u8>) {
    store_i64(out, chat_id.get());
}

fn parse_chat_id(parser: &mut Parser) -> Result<ChatId, &'static str> {
    Ok(ChatId::new(
        if parser.version() >= version::SUPPORT_64_BIT_IDS {
            parser.i64_()?
        } else {
            parser.i32_()? as i64
        },
    ))
}

fn parse_dialog_id(parser: &mut Parser) -> Result<DialogId, &'static str> {
    Ok(DialogId::new(parser.i64_()?))
}

fn parse_message_id(parser: &mut Parser) -> Result<MessageId, &'static str> {
    Ok(MessageId::new(parser.i64_()?))
}

/// `parse_caption` (MessageContent.cpp:3021-3032): the pre-`AddCaptionEntities`
/// legacy re-derives the entities; the `check_utf8` half of that branch is
/// unreachable on a Rust `String` (invalid UTF-8 fails the read instead).
fn parse_caption(parser: &mut Parser) -> Result<FormattedText, &'static str> {
    let text = parser.string()?;
    if parser.version() >= version::ADD_CAPTION_ENTITIES {
        let entities_len = parser.i32_()?;
        if entities_len < 0 || parser.left_len() < entities_len as usize {
            return Err("Wrong vector length");
        }
        let mut entities = Vec::with_capacity(entities_len as usize);
        for _ in 0..entities_len {
            entities.push(MessageEntity::parse(parser)?);
        }
        remove_empty_entities(&mut entities);
        Ok(FormattedText { text, entities })
    } else {
        let entities = find_entities(&text, false, true);
        Ok(FormattedText { text, entities })
    }
}

/// `store(const MessageContent *, ...)` (MessageContent.cpp:1831-3021): the
/// per-type store switch after the `MessageContentType` int.
pub fn store_message_content(
    content: &MessageContent,
    ctx: &dyn MessageContentStorage,
    out: &mut Vec<u8>,
) {
    let content_type = content.get_type();
    store_i32(out, content_type as i32);
    match content {
        MessageContent::Animation(m) => {
            ctx.store_animation(m.file_id, out);
            store_flags(out, [m.has_spoiler]);
            m.caption.store(out);
        }
        MessageContent::Audio(m) => {
            ctx.store_audio(m.file_id, out);
            m.caption.store(out);
            store_bool(out, true);
        }
        MessageContent::Contact(m) => {
            m.contact.store(out);
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
            store_flags(out, [m.has_spoiler, has_video_file_id]);
            m.caption.store(out);
            if has_video_file_id {
                ctx.store_video(m.video_file_id, out);
            }
        }
        MessageContent::Sticker(m) => {
            ctx.store_sticker(m.file_id, out);
            store_flags(out, [m.is_premium]);
        }
        MessageContent::Text(m) => {
            let has_web_page_id = m.web_page_id.is_valid();
            let has_web_page_url = !m.web_page_url.is_empty();
            store_flags(
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
            store_flags(
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
                store_user_id(*user_id, out);
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
                store_user_id(*user_id, out);
            }
        }
        MessageContent::ChatJoinedByLink(m) => {
            store_flags(out, [m.is_approved]);
        }
        MessageContent::ChatDeleteUser(m) => {
            store_user_id(m.user_id, out);
        }
        MessageContent::ChatMigrateTo(m) => {
            store_channel_id(m.migrated_to_channel_id, out);
        }
        MessageContent::ChannelCreate(m) => {
            store_string(out, &m.title);
        }
        MessageContent::ChannelMigrateFrom(m) => {
            store_string(out, &m.title);
            store_chat_id(m.migrated_from_chat_id, out);
        }
        MessageContent::PinMessage(m) => {
            store_i64(out, m.message_id.get());
        }
        MessageContent::GameScore(m) => {
            store_i64(out, m.game_message_id.get());
            store_i64(out, m.game_id);
            store_i32(out, m.score);
        }
        MessageContent::ScreenshotTaken(_) => {}
        MessageContent::ChatSetTtl(m) => {
            let has_from_user_id = m.from_user_id.is_valid();
            store_flags(out, [has_from_user_id]);
            store_i32(out, m.ttl);
            if has_from_user_id {
                store_user_id(m.from_user_id, out);
            }
        }
        MessageContent::Call(m) => {
            let has_call_access_hash = m.call_access_hash != 0;
            store_flags(out, [m.is_video, has_call_access_hash]);
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
            store_flags(
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
            if has_order_info {
                store_order_info(m.order_info.as_ref().unwrap(), out);
            }
            if has_telegram_payment_charge_id {
                store_string(out, &m.telegram_payment_charge_id);
            }
            if has_provider_payment_charge_id {
                store_string(out, &m.provider_payment_charge_id);
            }
            if has_invoice_message_id {
                store_i64(out, m.invoice_message_id.get());
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
            store_flags(out, [has_caption, has_attached_media]);
            ctx.store_poll(m.poll_id, out);
            if has_caption {
                m.caption.store(out);
            }
            if let Some(attached_media) = &m.attached_media {
                store_message_content(attached_media, ctx, out);
            }
        }
        MessageContent::Dice(m) => {
            let has_seed = !m.seed.is_empty();
            let has_stake_ton_count = m.stake_ton_count != 0;
            let has_prize_ton_count = m.prize_ton_count != 0;
            let has_state_hash = !m.state_hash.is_empty();
            store_flags(
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
            store_flags(out, [has_duration, has_schedule_date]);
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
                store_user_id(*user_id, out);
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
            store_flags(out, [has_crypto_amount, has_text, has_days]);
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
            store_flags(out, [m.title_missing]);
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
            store_flags(out, [has_one_shared_dialog]);
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
            store_flags(out, [has_message_id, m.for_both]);
            if has_message_id {
                store_i64(out, m.old_message_id.get());
            }
            store_background_info(&m.background_info, ctx, out);
        }
        MessageContent::Story(m) => {
            store_flags(out, [m.via_mention]);
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
            store_flags(
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
            store_flags(out, [has_star_count]);
            store_giveaway_parameters(&m.giveaway_parameters, out);
            store_i32(out, m.quantity);
            store_i32(out, m.months);
            if has_star_count {
                store_i64(out, m.star_count);
            }
        }
        MessageContent::GiveawayLaunch(m) => {
            let has_star_count = m.star_count != 0;
            store_flags(out, [has_star_count]);
            if has_star_count {
                store_i64(out, m.star_count);
            }
        }
        MessageContent::GiveawayResults(m) => {
            let has_winner_count = m.winner_count != 0;
            let has_unclaimed_count = m.unclaimed_count != 0;
            let has_giveaway_message_id = m.giveaway_message_id.is_valid();
            store_flags(
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
                store_i64(out, m.giveaway_message_id.get());
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
            store_flags(
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
                store_i64(out, m.giveaway_message_id.get());
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
                    store_user_id(*user_id, out);
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
            store_flags(out, [has_caption, has_payload]);
            store_i32(out, m.media.len() as i32);
            for media in &m.media {
                store_message_extended_media(media, ctx, out);
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
            store_flags(out, [has_invoice_payload]);
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
            store_flags(out, [has_crypto_amount, has_transaction_id]);
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
            store_flags(out, [m.is_unclaimed]);
            store_i64(out, m.star_count);
            store_string(out, &m.transaction_id);
            store_i64(out, m.boosted_dialog_id.get());
            store_i64(out, m.giveaway_message_id.get());
        }
        MessageContent::PaidMessagesRefunded(m) => {
            store_flags::<0>(out, []);
            store_i32(out, m.message_count);
            store_i64(out, m.star_count);
        }
        MessageContent::PaidMessagesPrice(m) => {
            store_flags(out, [m.broadcast_messages_allowed]);
            store_i64(out, m.star_count);
        }
        MessageContent::ConferenceCall(m) => {
            let has_duration = m.duration != 0;
            let has_other_participant_dialog_ids = !m.other_participant_dialog_ids.is_empty();
            store_flags(
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
            store_flags(
                out,
                [
                    has_to_do_message_id,
                    has_completed_task_ids,
                    has_incompleted_task_ids,
                ],
            );
            if has_to_do_message_id {
                store_i64(out, m.to_do_message_id.get());
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
            store_flags(out, [has_to_do_message_id, has_items]);
            if has_to_do_message_id {
                store_i64(out, m.to_do_message_id.get());
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
            store_flags(out, [has_transaction_id]);
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
            store_flags(out, [has_suggested_post_message_id, has_amount]);
            if has_suggested_post_message_id {
                store_i64(out, m.suggested_post_message_id.get());
            }
            if has_amount {
                store_currency_amount(&m.amount, out);
            }
        }
        MessageContent::SuggestedPostRefund(m) => {
            let has_suggested_post_message_id = m.suggested_post_message_id.is_valid();
            store_flags(out, [has_suggested_post_message_id, m.is_payer_initiated]);
            if has_suggested_post_message_id {
                store_i64(out, m.suggested_post_message_id.get());
            }
        }
        MessageContent::SuggestedPostApproval(m) => {
            let has_suggested_post_message_id = m.suggested_post_message_id.is_valid();
            let has_comment = !m.comment.is_empty();
            let has_price = !m.price.is_empty();
            let has_send_date = m.send_date != 0;
            store_flags(
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
                store_i64(out, m.suggested_post_message_id.get());
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
            store_flags(out, [has_new_creator_user_id]);
            if has_new_creator_user_id {
                store_user_id(m.new_creator_user_id, out);
            }
        }
        MessageContent::ChangeCreator(m) => {
            store_flags::<0>(out, []);
            store_user_id(m.new_creator_user_id, out);
        }
        MessageContent::NoForwardsToggle(m) => {
            let has_request_message_id = m.request_message_id.is_valid();
            store_flags(out, [m.prev_value, m.new_value, has_request_message_id]);
            if has_request_message_id {
                store_i64(out, m.request_message_id.get());
            }
        }
        MessageContent::NoForwardsRequest(m) => {
            store_flags(out, [m.is_expired, m.prev_value, m.new_value]);
        }
        MessageContent::ManagedBotCreated(m) => {
            store_flags::<0>(out, []);
            store_user_id(m.bot_user_id, out);
        }
        MessageContent::PollAppendAnswer(m) => {
            let has_poll_message_id = m.poll_message_id.is_valid();
            store_flags(out, [has_poll_message_id]);
            if has_poll_message_id {
                store_i64(out, m.poll_message_id.get());
            }
            m.text.store(out);
            store_string(out, &m.data);
        }
        MessageContent::PollDeleteAnswer(m) => {
            let has_poll_message_id = m.poll_message_id.is_valid();
            store_flags(out, [has_poll_message_id]);
            if has_poll_message_id {
                store_i64(out, m.poll_message_id.get());
            }
            m.text.store(out);
            store_string(out, &m.data);
        }
        MessageContent::ChangeCommunity(m) => {
            let has_community_id = m.community_id.is_valid();
            store_flags(out, [has_community_id]);
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

/// `parse(unique_ptr<MessageContent> &, ...)` (MessageContent.cpp:3034-4506):
/// the per-type parse switch after the `MessageContentType` int. Unknown
/// types return `MessageUnsupported(0)` with the remainder unconsumed, like
/// the C++ `default:` arm; a truncated blob is a hard `Err`.
pub fn parse_message_content(
    parser: &mut Parser,
    ctx: &mut dyn MessageContentStorage,
) -> Result<MessageContent, &'static str> {
    let type_raw = parser.i32_()?;
    let content_type = MessageContentType::from_raw(type_raw);
    let Some(content_type) = content_type else {
        // The C++ `default: is_bad = true` arm (MessageContent.cpp:4499) —
        // known-to-a-newer-binary and the six unmodeled types land here.
        tdutils::log_error!("Load a message with an invalid content of type {type_raw}");
        return Ok(MessageContent::Unsupported(MessageUnsupported {
            version: 0,
        }));
    };
    let mut is_bad = false;
    let content = match content_type {
        MessageContentType::Animation => {
            let file_id = ctx.parse_animation(parser)?;
            let mut has_spoiler = false;
            if parser.version() >= version::ADD_MESSAGE_MEDIA_SPOILER {
                has_spoiler = parser.parse_flags::<1>()?[0];
            }
            let caption = parse_caption(parser)?;
            is_bad = !file_id.is_valid();
            MessageContent::Animation(MessageAnimation {
                file_id,
                caption,
                has_spoiler,
            })
        }
        MessageContentType::Audio => {
            let file_id = ctx.parse_audio(parser)?;
            let caption = parse_caption(parser)?;
            let _legacy_is_listened = parser.bool()?;
            is_bad = !file_id.is_valid();
            MessageContent::Audio(MessageAudio { file_id, caption })
        }
        MessageContentType::Contact => {
            let contact = Contact::parse_from(parser)?;
            MessageContent::Contact(MessageContact { contact })
        }
        MessageContentType::Document => {
            let file_id = ctx.parse_document(parser)?;
            let caption = parse_caption(parser)?;
            is_bad = !file_id.is_valid();
            MessageContent::Document(MessageDocument { file_id, caption })
        }
        MessageContentType::Game => MessageContent::Game(MessageGame {
            game: parse_game(parser, ctx)?,
        }),
        MessageContentType::Invoice => MessageContent::Invoice(MessageInvoice {
            input_invoice: parse_input_invoice(parser, ctx)?,
        }),
        MessageContentType::LiveLocation => {
            let location = parse_location(parser)?;
            let period = parser.i32_()?;
            let heading = if parser.version() >= version::ADD_LIVE_LOCATION_HEADING {
                parser.i32_()?
            } else {
                0
            };
            let proximity_alert_radius =
                if parser.version() >= version::ADD_LIVE_LOCATION_PROXIMITY_ALERT_DISTANCE {
                    parser.i32_()?
                } else {
                    0
                };
            if period <= 0 {
                is_bad = true;
            }
            MessageContent::LiveLocation(MessageLiveLocation {
                location,
                period,
                heading,
                proximity_alert_radius,
            })
        }
        MessageContentType::Location => MessageContent::Location(MessageLocation {
            location: parse_location(parser)?,
        }),
        MessageContentType::Photo => {
            let photo = parse_photo(parser, ctx)?;
            is_bad |= photo_is_bad(&photo);
            let mut has_spoiler = false;
            let mut has_video_file_id = false;
            if parser.version() >= version::ADD_MESSAGE_MEDIA_SPOILER {
                let flags = parser.parse_flags::<2>()?;
                has_spoiler = flags[0];
                has_video_file_id = flags[1];
            }
            let caption = parse_caption(parser)?;
            let video_file_id = if has_video_file_id {
                ctx.parse_video(parser)?
            } else {
                FileId::default()
            };
            MessageContent::Photo(MessagePhoto {
                photo,
                video_file_id,
                caption,
                has_spoiler,
            })
        }
        MessageContentType::Sticker => {
            let file_id = ctx.parse_sticker(parser)?;
            let mut is_premium = false;
            if parser.version() >= version::ADD_MESSAGE_STICKER_FLAGS {
                is_premium = parser.parse_flags::<1>()?[0];
            }
            is_bad = !file_id.is_valid();
            MessageContent::Sticker(MessageSticker {
                file_id,
                is_premium,
            })
        }
        MessageContentType::Text => {
            let mut has_web_page_id = true;
            let mut force_small_media = false;
            let mut force_large_media = false;
            let mut has_web_page_url = false;
            let mut skip_web_page_confirmation = false;
            if parser.version() >= version::ADD_MESSAGE_TEXT_FLAGS {
                let flags = parser.parse_flags::<5>()?;
                has_web_page_id = flags[0];
                force_small_media = flags[1];
                force_large_media = flags[2];
                has_web_page_url = flags[3];
                skip_web_page_confirmation = flags[4];
            }
            let text = parse_formatted_text(parser)?;
            let web_page_id = if has_web_page_id {
                WebPageId::new(parser.i64_()?)
            } else {
                WebPageId::default()
            };
            let web_page_url = if has_web_page_url {
                parser.string()?
            } else {
                String::new()
            };
            if has_web_page_url && has_web_page_id {
                ctx.on_load_web_page_url_from_database(web_page_id, &web_page_url);
            }
            MessageContent::Text(MessageText {
                text,
                web_page_id,
                force_small_media,
                force_large_media,
                skip_web_page_confirmation,
                web_page_url,
            })
        }
        MessageContentType::Unsupported => {
            let version_ = if parser.version() >= version::ADD_MESSAGE_UNSUPPORTED_VERSION {
                parser.i32_()?
            } else {
                0
            };
            MessageContent::Unsupported(MessageUnsupported { version: version_ })
        }
        MessageContentType::Venue => MessageContent::Venue(MessageVenue {
            venue: parse_venue(parser)?,
        }),
        MessageContentType::Video => {
            let file_id = ctx.parse_video(parser)?;
            let mut has_spoiler = false;
            let mut has_alternative_videos = false;
            let mut has_hls_files = false;
            let mut has_cover = false;
            let mut has_start_timestamp = false;
            let mut has_storyboard_files = false;
            let mut has_storyboard_map_files = false;
            if parser.version() >= version::ADD_MESSAGE_MEDIA_SPOILER {
                let flags = parser.parse_flags::<7>()?;
                has_spoiler = flags[0];
                has_alternative_videos = flags[1];
                has_hls_files = flags[2];
                has_cover = flags[3];
                has_start_timestamp = flags[4];
                has_storyboard_files = flags[5];
                has_storyboard_map_files = flags[6];
            }
            let caption = parse_caption(parser)?;
            let mut alternative_file_ids = Vec::new();
            if has_alternative_videos {
                let video_count = parser.i32_()?;
                if video_count < 0 || parser.left_len() < video_count as usize {
                    return Err("Wrong vector length");
                }
                for _ in 0..video_count {
                    let file_id = ctx.parse_video(parser)?;
                    if !file_id.is_valid() {
                        is_bad = true;
                    } else {
                        alternative_file_ids.push(file_id);
                    }
                }
            }
            let mut hls_file_ids = Vec::new();
            if has_hls_files {
                let file_count = parser.i32_()?;
                if file_count < 0 || parser.left_len() < file_count as usize {
                    return Err("Wrong vector length");
                }
                for _ in 0..file_count {
                    let file_id = ctx.parse_document(parser)?;
                    if !file_id.is_valid() {
                        is_bad = true;
                    } else {
                        hls_file_ids.push(file_id);
                    }
                }
            }
            let cover = if has_cover {
                parse_photo(parser, ctx)?
            } else {
                Photo::default()
            };
            let start_timestamp = if has_start_timestamp {
                parser.i32_()?
            } else {
                0
            };
            let mut storyboard_file_ids = Vec::new();
            if has_storyboard_files {
                let file_count = parser.i32_()?;
                if file_count < 0 || parser.left_len() < file_count as usize {
                    return Err("Wrong vector length");
                }
                for _ in 0..file_count {
                    let file_id = ctx.parse_document(parser)?;
                    if !file_id.is_valid() {
                        is_bad = true;
                    } else {
                        storyboard_file_ids.push(file_id);
                    }
                }
            }
            let mut storyboard_map_file_ids = Vec::new();
            if has_storyboard_map_files {
                let file_count = parser.i32_()?;
                if file_count < 0 || parser.left_len() < file_count as usize {
                    return Err("Wrong vector length");
                }
                for _ in 0..file_count {
                    let file_id = ctx.parse_document(parser)?;
                    if !file_id.is_valid() {
                        is_bad = true;
                    } else {
                        storyboard_map_file_ids.push(file_id);
                    }
                }
            }
            if !file_id.is_valid() {
                is_bad = true;
            }
            MessageContent::Video(MessageVideo {
                file_id,
                alternative_file_ids,
                hls_file_ids,
                storyboard_file_ids,
                storyboard_map_file_ids,
                cover,
                start_timestamp,
                caption,
                has_spoiler,
            })
        }
        MessageContentType::VideoNote => {
            let file_id = ctx.parse_video_note(parser)?;
            let is_viewed = parser.bool()?;
            is_bad = !file_id.is_valid();
            MessageContent::VideoNote(MessageVideoNote { file_id, is_viewed })
        }
        MessageContentType::VoiceNote => {
            let file_id = ctx.parse_voice_note(parser)?;
            let caption = parse_caption(parser)?;
            let is_listened = parser.bool()?;
            is_bad = !file_id.is_valid();
            MessageContent::VoiceNote(MessageVoiceNote {
                file_id,
                caption,
                is_listened,
            })
        }
        MessageContentType::ChatCreate => {
            let title = parser.string()?;
            let participant_user_ids = parse_user_id_vec(parser)?;
            MessageContent::ChatCreate(MessageChatCreate {
                title,
                participant_user_ids,
            })
        }
        MessageContentType::ChatChangeTitle => {
            MessageContent::ChatChangeTitle(MessageChatChangeTitle {
                title: parser.string()?,
            })
        }
        MessageContentType::ChatChangePhoto => {
            let photo = parse_photo(parser, ctx)?;
            if photo.is_empty() {
                is_bad = true;
            }
            MessageContent::ChatChangePhoto(MessageChatChangePhoto { photo })
        }
        MessageContentType::ChatDeletePhoto => {
            MessageContent::ChatDeletePhoto(MessageChatDeletePhoto)
        }
        MessageContentType::ChatDeleteHistory => {
            MessageContent::ChatDeleteHistory(MessageChatDeleteHistory)
        }
        MessageContentType::ChatAddUsers => MessageContent::ChatAddUsers(MessageChatAddUsers {
            user_ids: parse_user_id_vec(parser)?,
        }),
        MessageContentType::ChatJoinedByLink => {
            let is_approved = if parser.version() >= version::ADD_INVITE_LINKS_REQUIRING_APPROVAL {
                parser.parse_flags::<1>()?[0]
            } else {
                false
            };
            MessageContent::ChatJoinedByLink(MessageChatJoinedByLink { is_approved })
        }
        MessageContentType::ChatDeleteUser => {
            MessageContent::ChatDeleteUser(MessageChatDeleteUser {
                user_id: parse_user_id(parser)?,
            })
        }
        MessageContentType::ChatMigrateTo => MessageContent::ChatMigrateTo(MessageChatMigrateTo {
            migrated_to_channel_id: parse_channel_id(parser)?,
        }),
        MessageContentType::ChannelCreate => MessageContent::ChannelCreate(MessageChannelCreate {
            title: parser.string()?,
        }),
        MessageContentType::ChannelMigrateFrom => {
            let title = parser.string()?;
            let migrated_from_chat_id = parse_chat_id(parser)?;
            MessageContent::ChannelMigrateFrom(MessageChannelMigrateFrom {
                title,
                migrated_from_chat_id,
            })
        }
        MessageContentType::PinMessage => MessageContent::PinMessage(MessagePinMessage {
            message_id: parse_message_id(parser)?,
        }),
        MessageContentType::GameScore => {
            let game_message_id = parse_message_id(parser)?;
            let game_id = parser.i64_()?;
            let score = parser.i32_()?;
            MessageContent::GameScore(MessageGameScore {
                game_message_id,
                game_id,
                score,
            })
        }
        MessageContentType::ScreenshotTaken => {
            MessageContent::ScreenshotTaken(MessageScreenshotTaken)
        }
        MessageContentType::ChatSetTtl => {
            let mut has_from_user_id = false;
            if parser.version() >= version::ADD_MESSAGE_CHAT_SET_TTL_FLAGS {
                has_from_user_id = parser.parse_flags::<1>()?[0];
            }
            let ttl = parser.i32_()?;
            let from_user_id = if has_from_user_id {
                parse_user_id(parser)?
            } else {
                UserId::default()
            };
            MessageContent::ChatSetTtl(MessageChatSetTtl { ttl, from_user_id })
        }
        MessageContentType::Call => {
            let mut is_video = false;
            let mut has_call_access_hash = false;
            if parser.version() >= version::ADD_VIDEO_CALLS_SUPPORT {
                let flags = parser.parse_flags::<2>()?;
                is_video = flags[0];
                has_call_access_hash = flags[1];
            }
            let call_id = parser.i64_()?;
            let duration = parser.i32_()?;
            let discard_reason_type_raw = parser.i32_()?;
            let discard_reason = CallDiscardReason {
                r#type: CallDiscardReasonType::from_raw(discard_reason_type_raw),
                slug: String::new(),
            };
            let call_access_hash = if has_call_access_hash {
                parser.i64_()?
            } else {
                0
            };
            MessageContent::Call(MessageCall {
                call_id,
                call_access_hash,
                duration,
                discard_reason,
                is_video,
            })
        }
        MessageContentType::PaymentSuccessful => {
            let flags = parser.parse_flags::<11>()?;
            let currency = parser.string()?;
            let mut total_amount = parser.i64_()?;
            // The legacy `is_correctly_stored = false` layout stored
            // `total_amount` in the payload slot and `invoice_payload` in
            // the shipping slot (MessageContent.cpp:3720-3728).
            let invoice_payload;
            let shipping_option_id;
            if flags[6] {
                invoice_payload = if flags[0] {
                    parser.string()?
                } else {
                    String::new()
                };
                shipping_option_id = if flags[1] {
                    parser.string()?
                } else {
                    String::new()
                };
            } else {
                if flags[0] {
                    total_amount = parser.i64_()?;
                }
                invoice_payload = if flags[1] {
                    parser.string()?
                } else {
                    String::new()
                };
                shipping_option_id = String::new();
            }
            let order_info = if flags[2] {
                Some(parse_order_info(parser)?)
            } else {
                None
            };
            let telegram_payment_charge_id = if flags[3] {
                parser.string()?
            } else {
                String::new()
            };
            let provider_payment_charge_id = if flags[4] {
                parser.string()?
            } else {
                String::new()
            };
            let invoice_message_id = if flags[5] {
                parse_message_id(parser)?
            } else {
                MessageId::default()
            };
            let invoice_dialog_id = if flags[7] {
                parse_dialog_id(parser)?
            } else {
                DialogId::default()
            };
            let subscription_until_date = if flags[10] { parser.i32_()? } else { 0 };
            if !flags[6] {
                return Ok(MessageContent::Unsupported(MessageUnsupported {
                    version: 0,
                }));
            }
            MessageContent::PaymentSuccessful(MessagePaymentSuccessful {
                invoice_dialog_id,
                invoice_message_id,
                currency,
                total_amount,
                invoice_payload,
                subscription_until_date,
                is_recurring: flags[8],
                is_first_recurring: flags[9],
                shipping_option_id,
                order_info,
                telegram_payment_charge_id,
                provider_payment_charge_id,
            })
        }
        MessageContentType::ContactRegistered => {
            MessageContent::ContactRegistered(MessageContactRegistered)
        }
        MessageContentType::ExpiredPhoto => MessageContent::ExpiredPhoto(MessageExpiredPhoto),
        MessageContentType::ExpiredVideo => MessageContent::ExpiredVideo(MessageExpiredVideo),
        MessageContentType::CustomServiceAction => {
            MessageContent::CustomServiceAction(MessageCustomServiceAction {
                message: parser.string()?,
            })
        }
        MessageContentType::WebsiteConnected => {
            MessageContent::WebsiteConnected(MessageWebsiteConnected {
                domain_name: parser.string()?,
            })
        }
        MessageContentType::PassportDataSent => {
            let types_len = parser.i32_()?;
            if types_len < 0 || parser.left_len() < types_len as usize {
                return Err("Wrong vector length");
            }
            let mut types = Vec::with_capacity(types_len as usize);
            for _ in 0..types_len {
                let raw = parser.i32_()?;
                types.push(match SecureValueType::try_from(raw) {
                    Ok(value) => value,
                    Err(_) => {
                        tdutils::log_error!("Wrong secure value type {raw}");
                        SecureValueType::None
                    }
                });
            }
            MessageContent::PassportDataSent(MessagePassportDataSent { types })
        }
        MessageContentType::PassportDataReceived => {
            let values_len = parser.i32_()?;
            if values_len < 0 || parser.left_len() < values_len as usize {
                return Err("Wrong vector length");
            }
            let mut values = Vec::with_capacity(values_len as usize);
            for _ in 0..values_len {
                values.push(parse_encrypted_secure_value(parser, ctx)?);
            }
            let credentials = parse_encrypted_secure_credentials(parser)?;
            MessageContent::PassportDataReceived(MessagePassportDataReceived {
                values,
                credentials,
            })
        }
        MessageContentType::Poll => {
            let mut has_caption = false;
            let mut has_attached_media = false;
            if parser.version() >= version::ADD_POLL_CAPTION {
                let flags = parser.parse_flags::<2>()?;
                has_caption = flags[0];
                has_attached_media = flags[1];
            }
            let poll_id = ctx.parse_poll(parser)?;
            let caption = if has_caption {
                parse_caption(parser)?
            } else {
                FormattedText::default()
            };
            let attached_media = if has_attached_media {
                Some(Box::new(parse_message_content(parser, ctx)?))
            } else {
                None
            };
            is_bad = !poll_id.is_valid();
            MessageContent::Poll(MessagePoll {
                poll_id,
                caption,
                attached_media,
            })
        }
        MessageContentType::Dice => {
            let mut is_stake = false;
            let mut has_seed = false;
            let mut has_stake_ton_count = false;
            let mut has_prize_ton_count = false;
            let mut has_state_hash = false;
            if parser.version() >= version::ADD_DICE_FLAGS {
                let flags = parser.parse_flags::<5>()?;
                is_stake = flags[0];
                has_seed = flags[1];
                has_stake_ton_count = flags[2];
                has_prize_ton_count = flags[3];
                has_state_hash = flags[4];
            }
            let emoji = if parser.version() >= version::ADD_DICE_EMOJI {
                remove_emoji_modifiers(&parser.string()?, true)
            } else {
                MessageDice::DEFAULT_EMOJI.to_string()
            };
            let dice_value = parser.i32_()?;
            let seed = if has_seed {
                parser.string()?
            } else {
                String::new()
            };
            let stake_ton_count = if has_stake_ton_count {
                parser.i64_()?
            } else {
                0
            };
            let prize_ton_count = if has_prize_ton_count {
                parser.i64_()?
            } else {
                0
            };
            let state_hash = if has_state_hash {
                parser.string()?
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
            is_bad = !dice.is_valid();
            MessageContent::Dice(dice)
        }
        MessageContentType::ProximityAlertTriggered => {
            let traveler_dialog_id = parse_dialog_id(parser)?;
            let watcher_dialog_id = parse_dialog_id(parser)?;
            let distance = parser.i32_()?;
            MessageContent::ProximityAlertTriggered(MessageProximityAlertTriggered {
                traveler_dialog_id,
                watcher_dialog_id,
                distance,
            })
        }
        MessageContentType::GroupCall => {
            let flags = parser.parse_flags::<2>()?;
            let group_call_id = parser.i64_()?;
            let access_hash = parser.i64_()?;
            let duration = if flags[0] { parser.i32_()? } else { -1 };
            let schedule_date = if flags[1] { parser.i32_()? } else { -1 };
            MessageContent::GroupCall(MessageGroupCall {
                input_group_call_id: InputGroupCallId::new(group_call_id, access_hash),
                duration,
                schedule_date,
            })
        }
        MessageContentType::InviteToGroupCall => {
            let group_call_id = parser.i64_()?;
            let access_hash = parser.i64_()?;
            let user_ids = parse_user_id_vec(parser)?;
            MessageContent::InviteToGroupCall(MessageInviteToGroupCall {
                input_group_call_id: InputGroupCallId::new(group_call_id, access_hash),
                user_ids,
            })
        }
        MessageContentType::WebViewDataSent => {
            MessageContent::WebViewDataSent(MessageWebViewDataSent {
                button_text: parser.string()?,
            })
        }
        MessageContentType::WebViewDataReceived => {
            let button_text = parser.string()?;
            let data = parser.string()?;
            MessageContent::WebViewDataReceived(MessageWebViewDataReceived { button_text, data })
        }
        MessageContentType::GiftPremium => {
            let flags = parser.parse_flags::<3>()?;
            let currency = parser.string()?;
            let amount = parser.i64_()?;
            let mut days = parser.i32_()?;
            if !flags[2] {
                days = get_premium_duration_day_count(days);
            }
            let crypto_currency = if flags[0] {
                parser.string()?
            } else {
                String::new()
            };
            let crypto_amount = if flags[0] { parser.i64_()? } else { 0 };
            let text = if flags[1] {
                parse_formatted_text(parser)?
            } else {
                FormattedText::default()
            };
            MessageContent::GiftPremium(MessageGiftPremium {
                text,
                currency,
                amount,
                crypto_currency,
                crypto_amount,
                days,
            })
        }
        MessageContentType::TopicCreate => {
            let mut title_missing = false;
            if parser.version() >= version::SUPPORT_BOT_TOPICS {
                title_missing = parser.parse_flags::<1>()?[0];
            }
            let title = parser.string()?;
            let icon = parse_forum_topic_icon(parser)?;
            MessageContent::TopicCreate(MessageTopicCreate {
                title,
                icon,
                title_missing,
            })
        }
        MessageContentType::TopicEdit => MessageContent::TopicEdit(MessageTopicEdit {
            edited_data: parse_forum_topic_edited_data(parser)?,
        }),
        MessageContentType::SuggestProfilePhoto => {
            let photo = parse_photo(parser, ctx)?;
            if photo.is_empty() {
                is_bad = true;
            }
            MessageContent::SuggestProfilePhoto(MessageSuggestProfilePhoto { photo })
        }
        MessageContentType::WriteAccessAllowed => {
            MessageContent::WriteAccessAllowed(MessageWriteAccessAllowed)
        }
        MessageContentType::RequestedDialog => {
            let mut has_one_shared_dialog = true;
            if parser.version() >= version::SUPPORT_MULTIPLE_SHARED_USERS {
                has_one_shared_dialog = parser.parse_flags::<1>()?[0];
            }
            let shared_dialog_ids = if has_one_shared_dialog {
                vec![parse_dialog_id(parser)?]
            } else {
                let len = parser.i32_()?;
                if len < 0 || parser.left_len() < len as usize {
                    return Err("Wrong vector length");
                }
                let mut dialog_ids = Vec::with_capacity(len as usize);
                for _ in 0..len {
                    dialog_ids.push(parse_dialog_id(parser)?);
                }
                if dialog_ids.len() > 1 {
                    for dialog_id in &dialog_ids {
                        if dialog_id.get_type() != crate::messages_manager::DialogType::User {
                            is_bad = true;
                        }
                    }
                }
                dialog_ids
            };
            if shared_dialog_ids.is_empty() || !shared_dialog_ids[0].is_valid() {
                is_bad = true;
            }
            let button_id = parser.i32_()?;
            MessageContent::RequestedDialog(MessageRequestedDialog {
                shared_dialog_ids,
                button_id,
            })
        }
        MessageContentType::WebViewWriteAccessAllowed => {
            MessageContent::WebViewWriteAccessAllowed(MessageWebViewWriteAccessAllowed {
                web_app: parse_web_app(parser, ctx)?,
            })
        }
        MessageContentType::SetBackground => {
            let flags = parser.parse_flags::<2>()?;
            let old_message_id = if flags[0] {
                parse_message_id(parser)?
            } else {
                MessageId::default()
            };
            let background_info = parse_background_info(parser, ctx)?;
            MessageContent::SetBackground(MessageSetBackground {
                old_message_id,
                background_info,
                for_both: flags[1],
            })
        }
        MessageContentType::Story => {
            let via_mention = parser.parse_flags::<1>()?[0];
            let dialog_id = parse_dialog_id(parser)?;
            let story_raw = parser.i32_()?;
            let story_full_id = StoryFullId::new(dialog_id, StoryId::new(story_raw));
            if !story_full_id.is_server() {
                is_bad = true;
            }
            MessageContent::Story(MessageStory {
                story_full_id,
                via_mention,
            })
        }
        MessageContentType::WriteAccessAllowedByRequest => {
            MessageContent::WriteAccessAllowedByRequest(MessageWriteAccessAllowedByRequest)
        }
        MessageContentType::GiftCode => {
            let flags = parser.parse_flags::<9>()?;
            let creator_dialog_id = if flags[1] {
                parse_dialog_id(parser)?
            } else {
                DialogId::default()
            };
            let mut days = parser.i32_()?;
            if !flags[8] {
                days = get_premium_duration_day_count(days);
            }
            let code = parser.string()?;
            let currency = if flags[3] {
                parser.string()?
            } else {
                String::new()
            };
            let amount = if flags[4] { parser.i64_()? } else { 0 };
            let crypto_currency = if flags[5] {
                parser.string()?
            } else {
                String::new()
            };
            let crypto_amount = if flags[6] { parser.i64_()? } else { 0 };
            let text = if flags[7] {
                parse_formatted_text(parser)?
            } else {
                FormattedText::default()
            };
            MessageContent::GiftCode(MessageGiftCode {
                creator_dialog_id,
                text,
                days,
                currency,
                amount,
                crypto_currency,
                crypto_amount,
                via_giveaway: flags[0],
                is_unclaimed: flags[2],
                code,
            })
        }
        MessageContentType::Giveaway => {
            let has_star_count = parser.parse_flags::<1>()?[0];
            let giveaway_parameters = parse_giveaway_parameters(parser)?;
            let quantity = parser.i32_()?;
            let months = parser.i32_()?;
            let star_count = if has_star_count { parser.i64_()? } else { 0 };
            if !giveaway_parameters.is_valid() {
                is_bad = true;
            }
            MessageContent::Giveaway(MessageGiveaway {
                giveaway_parameters,
                quantity,
                months,
                star_count,
            })
        }
        MessageContentType::GiveawayLaunch => {
            let mut has_star_count = false;
            if parser.version() >= version::SUPPORT_STAR_GIVEAWAYS {
                has_star_count = parser.parse_flags::<1>()?[0];
            }
            let star_count = if has_star_count { parser.i64_()? } else { 0 };
            MessageContent::GiveawayLaunch(MessageGiveawayLaunch { star_count })
        }
        MessageContentType::GiveawayResults => {
            let flags = parser.parse_flags::<4>()?;
            let winner_count = if flags[0] { parser.i32_()? } else { 0 };
            let unclaimed_count = if flags[1] { parser.i32_()? } else { 0 };
            let giveaway_message_id = if flags[2] {
                parse_message_id(parser)?
            } else {
                MessageId::default()
            };
            if winner_count < 0 || unclaimed_count < 0 {
                is_bad = true;
            }
            MessageContent::GiveawayResults(MessageGiveawayResults {
                giveaway_message_id,
                winner_count,
                unclaimed_count,
                is_star_giveaway: flags[3],
            })
        }
        MessageContentType::GiveawayWinners => {
            let flags = parser.parse_flags::<12>()?;
            let giveaway_message_id = if flags[2] {
                parse_message_id(parser)?
            } else {
                MessageId::default()
            };
            let boosted_channel_id = if flags[3] {
                parse_channel_id(parser)?
            } else {
                ChannelId::default()
            };
            let additional_dialog_count = if flags[4] { parser.i32_()? } else { 0 };
            let month_count = if flags[5] { parser.i32_()? } else { 0 };
            let prize_description = if flags[6] {
                parser.string()?
            } else {
                String::new()
            };
            let winners_selection_date = if flags[7] { parser.i32_()? } else { 0 };
            let winner_count = if flags[8] { parser.i32_()? } else { 0 };
            let unclaimed_count = if flags[9] { parser.i32_()? } else { 0 };
            let winner_user_ids = if flags[10] {
                parse_user_id_vec(parser)?
            } else {
                Vec::new()
            };
            let star_count = if flags[11] { parser.i64_()? } else { 0 };
            if winner_count < 0 || unclaimed_count < 0 {
                is_bad = true;
            }
            MessageContent::GiveawayWinners(MessageGiveawayWinners {
                giveaway_message_id,
                boosted_channel_id,
                additional_dialog_count,
                month_count,
                star_count,
                prize_description,
                winners_selection_date,
                only_new_subscribers: flags[0],
                was_refunded: flags[1],
                winner_count,
                unclaimed_count,
                winner_user_ids,
            })
        }
        MessageContentType::ExpiredVideoNote => {
            MessageContent::ExpiredVideoNote(MessageExpiredVideoNote)
        }
        MessageContentType::ExpiredVoiceNote => {
            MessageContent::ExpiredVoiceNote(MessageExpiredVoiceNote)
        }
        MessageContentType::BoostApply => {
            parser.parse_flags::<0>()?;
            let boost_count = parser.i32_()?;
            if boost_count < 0 {
                is_bad = true;
            }
            MessageContent::BoostApply(MessageBoostApply { boost_count })
        }
        MessageContentType::DialogShared => {
            parser.parse_flags::<0>()?;
            let len = parser.i32_()?;
            if len < 0 || parser.left_len() < len as usize {
                return Err("Wrong vector length");
            }
            let mut shared_dialogs = Vec::with_capacity(len as usize);
            for _ in 0..len {
                shared_dialogs.push(parse_shared_dialog(parser, ctx)?);
            }
            if shared_dialogs.is_empty() || shared_dialogs.iter().any(|d| !d.is_valid()) {
                is_bad = true;
            }
            let button_id = parser.i32_()?;
            MessageContent::DialogShared(MessageDialogShared {
                shared_dialogs,
                button_id,
            })
        }
        MessageContentType::PaidMedia => {
            let flags = parser.parse_flags::<2>()?;
            let media_len = parser.i32_()?;
            if media_len < 0 || parser.left_len() < media_len as usize {
                return Err("Wrong vector length");
            }
            let mut media = Vec::with_capacity(media_len as usize);
            for _ in 0..media_len {
                media.push(parse_message_extended_media(parser, ctx)?);
            }
            let caption = if flags[0] {
                parse_caption(parser)?
            } else {
                FormattedText::default()
            };
            let star_count = parser.i64_()?;
            let payload = if flags[1] {
                parser.string()?
            } else {
                String::new()
            };
            for media_item in &media {
                if media_item.is_empty() {
                    is_bad = true;
                }
            }
            MessageContent::PaidMedia(MessagePaidMedia {
                media,
                caption,
                star_count,
                payload,
            })
        }
        MessageContentType::PaymentRefunded => {
            let has_invoice_payload = parser.parse_flags::<1>()?[0];
            let dialog_id = parse_dialog_id(parser)?;
            let currency = parser.string()?;
            let total_amount = parser.i64_()?;
            let invoice_payload = if has_invoice_payload {
                parser.string()?
            } else {
                String::new()
            };
            let telegram_payment_charge_id = parser.string()?;
            let provider_payment_charge_id = parser.string()?;
            MessageContent::PaymentRefunded(MessagePaymentRefunded {
                dialog_id,
                currency,
                total_amount,
                invoice_payload,
                telegram_payment_charge_id,
                provider_payment_charge_id,
            })
        }
        MessageContentType::GiftStars => {
            let flags = parser.parse_flags::<2>()?;
            let currency = parser.string()?;
            let amount = parser.i64_()?;
            let star_count = parser.i64_()?;
            let crypto_currency = if flags[0] {
                parser.string()?
            } else {
                String::new()
            };
            let crypto_amount = if flags[0] { parser.i64_()? } else { 0 };
            let transaction_id = if flags[1] {
                parser.string()?
            } else {
                String::new()
            };
            MessageContent::GiftStars(MessageGiftStars {
                currency,
                amount,
                crypto_currency,
                crypto_amount,
                star_count,
                transaction_id,
            })
        }
        MessageContentType::PrizeStars => {
            let is_unclaimed = parser.parse_flags::<1>()?[0];
            let star_count = parser.i64_()?;
            let transaction_id = parser.string()?;
            let boosted_dialog_id = parse_dialog_id(parser)?;
            let giveaway_message_id = parse_message_id(parser)?;
            MessageContent::PrizeStars(MessagePrizeStars {
                star_count,
                transaction_id,
                boosted_dialog_id,
                giveaway_message_id,
                is_unclaimed,
            })
        }
        MessageContentType::PaidMessagesRefunded => {
            parser.parse_flags::<0>()?;
            let message_count = parser.i32_()?;
            let star_count = parser.i64_()?;
            MessageContent::PaidMessagesRefunded(MessagePaidMessagesRefunded {
                message_count,
                star_count,
            })
        }
        MessageContentType::PaidMessagesPrice => {
            let broadcast_messages_allowed = parser.parse_flags::<1>()?[0];
            let star_count = parser.i64_()?;
            MessageContent::PaidMessagesPrice(MessagePaidMessagesPrice {
                star_count,
                broadcast_messages_allowed,
            })
        }
        MessageContentType::ConferenceCall => {
            let flags = parser.parse_flags::<5>()?;
            let call_id = parser.i64_()?;
            let duration = if flags[3] { parser.i32_()? } else { 0 };
            let other_participant_dialog_ids = if flags[4] {
                let len = parser.i32_()?;
                if len < 0 || parser.left_len() < len as usize {
                    return Err("Wrong vector length");
                }
                let mut dialog_ids = Vec::with_capacity(len as usize);
                for _ in 0..len {
                    dialog_ids.push(parse_dialog_id(parser)?);
                }
                dialog_ids
            } else {
                Vec::new()
            };
            MessageContent::ConferenceCall(MessageConferenceCall {
                call_id,
                duration,
                other_participant_dialog_ids,
                is_active: flags[0],
                is_video: flags[1],
                was_missed: flags[2],
            })
        }
        MessageContentType::ToDoList => {
            parser.parse_flags::<0>()?;
            let list = parse_to_do_list(parser)?;
            let completions_len = parser.i32_()?;
            if completions_len < 0 || parser.left_len() < completions_len as usize {
                return Err("Wrong vector length");
            }
            let mut completions = Vec::with_capacity(completions_len as usize);
            for _ in 0..completions_len {
                completions.push(parse_to_do_completion(parser)?);
            }
            MessageContent::ToDoList(MessageToDoList { list, completions })
        }
        MessageContentType::TodoCompletions => {
            let flags = parser.parse_flags::<3>()?;
            let to_do_message_id = if flags[0] {
                parse_message_id(parser)?
            } else {
                MessageId::default()
            };
            let completed_task_ids = if flags[1] {
                parser.i32_vec()?
            } else {
                Vec::new()
            };
            let incompleted_task_ids = if flags[2] {
                parser.i32_vec()?
            } else {
                Vec::new()
            };
            MessageContent::TodoCompletions(MessageTodoCompletions {
                to_do_message_id,
                completed_task_ids,
                incompleted_task_ids,
            })
        }
        MessageContentType::TodoAppendTasks => {
            let flags = parser.parse_flags::<2>()?;
            let to_do_message_id = if flags[0] {
                parse_message_id(parser)?
            } else {
                MessageId::default()
            };
            let items = if flags[1] {
                let len = parser.i32_()?;
                if len < 0 || parser.left_len() < len as usize {
                    return Err("Wrong vector length");
                }
                let mut items = Vec::with_capacity(len as usize);
                for _ in 0..len {
                    items.push(parse_to_do_item(parser)?);
                }
                items
            } else {
                Vec::new()
            };
            MessageContent::TodoAppendTasks(MessageTodoAppendTasks {
                to_do_message_id,
                items,
            })
        }
        MessageContentType::GiftTon => {
            let has_transaction_id = parser.parse_flags::<1>()?[0];
            let currency = parser.string()?;
            let amount = parser.i64_()?;
            let crypto_currency = parser.string()?;
            let crypto_amount = parser.i64_()?;
            let transaction_id = if has_transaction_id {
                parser.string()?
            } else {
                String::new()
            };
            MessageContent::GiftTon(MessageGiftTon {
                currency,
                amount,
                crypto_currency,
                crypto_amount,
                transaction_id,
            })
        }
        MessageContentType::SuggestedPostSuccess => {
            let flags = parser.parse_flags::<2>()?;
            let suggested_post_message_id = if flags[0] {
                parse_message_id(parser)?
            } else {
                MessageId::default()
            };
            let amount = if flags[1] {
                parse_currency_amount(parser)?
            } else {
                CurrencyAmount::default()
            };
            MessageContent::SuggestedPostSuccess(MessageSuggestedPostSuccess {
                suggested_post_message_id,
                amount,
            })
        }
        MessageContentType::SuggestedPostRefund => {
            let flags = parser.parse_flags::<2>()?;
            let suggested_post_message_id = if flags[0] {
                parse_message_id(parser)?
            } else {
                MessageId::default()
            };
            MessageContent::SuggestedPostRefund(MessageSuggestedPostRefund {
                suggested_post_message_id,
                is_payer_initiated: flags[1],
            })
        }
        MessageContentType::SuggestedPostApproval => {
            let flags = parser.parse_flags::<6>()?;
            let suggested_post_message_id = if flags[0] {
                parse_message_id(parser)?
            } else {
                MessageId::default()
            };
            let price = if flags[1] {
                parse_suggested_post_price(parser)?
            } else {
                SuggestedPostPrice::default()
            };
            let comment = if flags[2] {
                parser.string()?
            } else {
                String::new()
            };
            let send_date = if flags[3] { parser.i32_()? } else { 0 };
            MessageContent::SuggestedPostApproval(MessageSuggestedPostApproval {
                suggested_post_message_id,
                price,
                comment,
                send_date,
                is_rejected: flags[4],
                is_balance_too_low: flags[5],
            })
        }
        MessageContentType::SuggestBirthday => {
            parser.parse_flags::<0>()?;
            MessageContent::SuggestBirthday(MessageSuggestBirthday {
                birthdate: Birthdate::parse_from(parser)?,
            })
        }
        MessageContentType::NewCreatorPending => {
            let has_new_creator_user_id = parser.parse_flags::<1>()?[0];
            let new_creator_user_id = if has_new_creator_user_id {
                parse_user_id(parser)?
            } else {
                UserId::default()
            };
            MessageContent::NewCreatorPending(MessageNewCreatorPending {
                new_creator_user_id,
            })
        }
        MessageContentType::ChangeCreator => {
            parser.parse_flags::<0>()?;
            let new_creator_user_id = parse_user_id(parser)?;
            if !new_creator_user_id.is_valid() {
                is_bad = true;
            }
            MessageContent::ChangeCreator(MessageChangeCreator {
                new_creator_user_id,
            })
        }
        MessageContentType::NoForwardsToggle => {
            let flags = parser.parse_flags::<3>()?;
            let request_message_id = if flags[2] {
                parse_message_id(parser)?
            } else {
                MessageId::default()
            };
            MessageContent::NoForwardsToggle(MessageNoForwardsToggle {
                request_message_id,
                prev_value: flags[0],
                new_value: flags[1],
            })
        }
        MessageContentType::NoForwardsRequest => {
            let flags = parser.parse_flags::<3>()?;
            MessageContent::NoForwardsRequest(MessageNoForwardsRequest {
                is_expired: flags[0],
                prev_value: flags[1],
                new_value: flags[2],
            })
        }
        MessageContentType::ManagedBotCreated => {
            parser.parse_flags::<0>()?;
            let bot_user_id = parse_user_id(parser)?;
            if !bot_user_id.is_valid() {
                is_bad = true;
            }
            MessageContent::ManagedBotCreated(MessageManagedBotCreated { bot_user_id })
        }
        MessageContentType::PollAppendAnswer => {
            let has_poll_message_id = parser.parse_flags::<1>()?[0];
            let poll_message_id = if has_poll_message_id {
                parse_message_id(parser)?
            } else {
                MessageId::default()
            };
            let text = parse_formatted_text(parser)?;
            let data = parser.string()?;
            MessageContent::PollAppendAnswer(MessagePollAppendAnswer {
                poll_message_id,
                text,
                data,
            })
        }
        MessageContentType::PollDeleteAnswer => {
            let has_poll_message_id = parser.parse_flags::<1>()?[0];
            let poll_message_id = if has_poll_message_id {
                parse_message_id(parser)?
            } else {
                MessageId::default()
            };
            let text = parse_formatted_text(parser)?;
            let data = parser.string()?;
            MessageContent::PollDeleteAnswer(MessagePollDeleteAnswer {
                poll_message_id,
                text,
                data,
            })
        }
        MessageContentType::ChangeCommunity => {
            let has_community_id = parser.parse_flags::<1>()?[0];
            let community_id = if has_community_id {
                CommunityId::new(parser.i64_()?)
            } else {
                CommunityId::default()
            };
            MessageContent::ChangeCommunity(MessageChangeCommunity { community_id })
        }
        MessageContentType::ChatJoinedViaCommunity => {
            parser.parse_flags::<0>()?;
            let community_id = CommunityId::new(parser.i64_()?);
            MessageContent::ChatJoinedViaCommunity(MessageChatJoinedViaCommunity { community_id })
        }
        // The unmodeled six: StarGift/StarGiftUnique/StarGiftPurchaseOffer/
        // StarGiftPurchaseOfferDeclined/ChatSetTheme/RichText.
        MessageContentType::StarGift
        | MessageContentType::StarGiftUnique
        | MessageContentType::StarGiftPurchaseOffer
        | MessageContentType::StarGiftPurchaseOfferDeclined
        | MessageContentType::ChatSetTheme
        | MessageContentType::RichText
        | MessageContentType::None => {
            tdutils::log_error!("Load a message with an unmodeled content of type {type_raw}");
            return Ok(MessageContent::Unsupported(MessageUnsupported {
                version: 0,
            }));
        }
    };
    if is_bad {
        tdutils::log_error!(
            "Load a message with an invalid content of type {}",
            content_type as i32
        );
        return Ok(MessageContent::Unsupported(MessageUnsupported {
            version: 0,
        }));
    }
    Ok(content)
}

fn parse_user_id_vec(parser: &mut Parser) -> Result<Vec<UserId>, &'static str> {
    let len = parser.i32_()?;
    if len < 0 || parser.left_len() < len as usize {
        return Err("Wrong vector length");
    }
    let mut user_ids = Vec::with_capacity(len as usize);
    for _ in 0..len {
        user_ids.push(parse_user_id(parser)?);
    }
    Ok(user_ids)
}

fn parse_formatted_text(parser: &mut Parser) -> Result<FormattedText, &'static str> {
    FormattedText::parse(parser)
}

// ---------------------------------------------------------------------------
// M.9.7/M.9.8/M.9.9 — the classification, policy and accessor planes
// (MessageContentType.cpp, MessageContent.cpp:6207-7415). The manager
// reads go through the [`MessageContentContext`] seam — the Rust shape of
// the `const Td *td` parameter these C++ functions take.
// ---------------------------------------------------------------------------

/// `td::StickerType` (StickerType.h:17). Owned by StickersManager (the
/// Wave-2/3 stickers slice); the value lands here because
/// [`get_message_content_sticker_type`] is its first consumer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum StickerType {
    #[default]
    Regular = 0,
    Mask,
    CustomEmoji,
}

/// The `const Td *td` surface of the M.9.7/M.9.8/M.9.9 planes — the
/// manager queries the C++ free functions make through `td`. Real
/// implementations land with each owner (the UserManager reads with the
/// Td wiring of this line, ChatManager with M.5, PollManager with
/// Wave 2, StickersManager/WebPagesManager with their slices); tests use
/// a table stand-in the way the store/parse plane uses `RawIdStorage`.
pub trait MessageContentContext {
    // -- UserManager (UserManager.cpp:5387/5610/5620/5720) --
    fn get_user_default_permissions(&self, user_id: UserId) -> RestrictedRights;
    fn get_secret_chat_default_permissions(&self, secret_chat_id: SecretChatId)
    -> RestrictedRights;
    /// `UserManager::is_user_bot` (UserManager.cpp:5387-5393).
    fn is_user_bot(&self, user_id: UserId) -> bool;
    /// `UserManager::get_user_voice_messages_forbidden`
    /// (UserManager.cpp:5720-5729).
    fn get_user_voice_messages_forbidden(&self, user_id: UserId) -> bool;

    // -- ChatManager (M.5) --
    /// `td->chat_manager_->get_chat_permissions(chat_id)
    /// .get_effective_restricted_rights()` (MessageContent.cpp:6218-6220).
    fn get_chat_effective_restricted_rights(&self, chat_id: ChatId) -> RestrictedRights;
    /// The channel twin of the chat read (MessageContent.cpp:6221-6222).
    fn get_channel_effective_restricted_rights(&self, channel_id: ChannelId) -> RestrictedRights;
    /// `ChatManager::is_broadcast_channel`.
    fn is_broadcast_channel(&self, channel_id: ChannelId) -> bool;
    /// `ChatManager::is_monoforum_channel`.
    fn is_monoforum_channel(&self, channel_id: ChannelId) -> bool;

    // -- AuthManager / DialogManager --
    /// `td->auth_manager_->is_bot()`.
    fn is_bot(&self) -> bool;
    /// `td->dialog_manager_->get_my_dialog_id()`.
    fn get_my_dialog_id(&self) -> DialogId;

    // -- PollManager (Wave 2) --
    fn get_poll_is_anonymous(&self, poll_id: PollId) -> bool;
    fn get_poll_is_closed(&self, poll_id: PollId) -> bool;
    fn get_poll_can_add_option(&self, poll_id: PollId) -> bool;
    fn get_poll_can_view_stats(&self, poll_id: PollId) -> bool;
    fn get_poll_has_unread_votes(&self, poll_id: PollId) -> bool;
    fn remove_poll_has_unread_votes(&self, poll_id: PollId);
    /// `PollManager::has_input_media` (the forward gate,
    /// MessageContent.cpp:6503-6506).
    fn has_input_media_poll(&self, poll_id: PollId) -> bool;
    /// `PollManager::get_individual_message_contents`
    /// (MessageContent.cpp:6702-6705); `attached_media` is the stored
    /// `MessagePoll::attached_media` of the message.
    fn get_poll_individual_message_contents(
        &self,
        poll_id: PollId,
        attached_media: Option<&MessageContent>,
    ) -> Vec<MessageContent>;
    /// `PollManager::get_poll_option_properties`
    /// (PollManager.cpp:1147) — the td_api request arm.
    #[allow(clippy::too_many_arguments)] // the C++ handler has the same shape
    fn get_poll_option_properties(
        &self,
        poll_id: PollId,
        option_id: &str,
        dialog_id: DialogId,
        message_id: MessageId,
        can_be_replied: bool,
        can_be_replied_in_another_chat: bool,
        can_get_link: bool,
        is_forward: bool,
        is_outgoing: bool,
        promise: Promise<crate::td_api::PollOptionProperties>,
    );

    // -- StickersManager (Wave 2/3) --
    /// `StickersManager::get_sticker_type(file_id)`
    /// (MessageContent.cpp:6714-6717).
    fn get_sticker_type(&self, file_id: FileId) -> StickerType;

    // -- WebPagesManager (Wave 2) --
    /// `WebPagesManager::get_web_page_user_ids`
    /// (MessageContent.cpp:6902-6905).
    fn get_web_page_user_ids(&self, web_page_id: WebPageId) -> Vec<UserId>;
    /// `WebPagesManager::get_web_page_channel_ids`
    /// (MessageContent.cpp:7207-7212).
    fn get_web_page_channel_ids(&self, web_page_id: WebPageId) -> Vec<ChannelId>;

    // -- OptionManager --
    /// `td->option_manager_->get_option_integer(name, default)` — the
    /// `checklist_task_count_max` read of
    /// `ToDoList::get_can_append_items` (ToDoList.cpp:70).
    fn get_option_integer(&self, name: &str, default: i64) -> i64;
}

/// `is_local_poll_id` (PollManager.cpp:449-452) — the local (unsent)
/// poll-id range; a pure helper, so it ports ahead of its Wave-2 owner.
fn is_local_poll_id(poll_id: PollId) -> bool {
    poll_id.get() < 0 && poll_id.get() > i32::MIN as i64
}

/// The pre-integration line carries the M.5.3 `chat_manager::MessageId`
/// beside the M.10.1a `messages_manager::MessageId`; the integrator
/// collapses them, and this bridge keeps the replied-id arms 1-to-1
/// until then.
fn to_messages_manager_message_id(message_id: MessageId) -> crate::messages_manager::MessageId {
    crate::messages_manager::MessageId::new(message_id.get())
}

/// `can_send_message_content` (MessageContent.cpp:6207-6481). The
/// `RichText` arm (`RichMessage::can_send`) is deferred with the
/// unmodeled variant; the `ChatSetTheme` service arm is part of the
/// `UNREACHABLE()` tail, which aborts exactly like the C++.
pub fn can_send_message_content(
    td: &dyn MessageContentContext,
    dialog_id: DialogId,
    content: &MessageContent,
    is_forward: bool,
    check_permissions: bool,
) -> Result<(), Status> {
    let dialog_type = dialog_id.get_type();
    let permissions = if !check_permissions {
        RestrictedRights::from_bools(
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            ChannelType::Unknown,
        )
    } else {
        match dialog_type {
            DialogType::User => td.get_user_default_permissions(dialog_id.get_user_id()),
            DialogType::Chat => td.get_chat_effective_restricted_rights(dialog_id.get_chat_id()),
            DialogType::Channel => {
                td.get_channel_effective_restricted_rights(dialog_id.get_channel_id())
            }
            DialogType::SecretChat => {
                td.get_secret_chat_default_permissions(dialog_id.get_secret_chat_id())
            }
            DialogType::None => unreachable!(),
        }
    };

    let content_type = content.get_type();
    if dialog_type == DialogType::SecretChat
        && !can_send_message_content_to_secret_chat(content_type)
    {
        return Err(Status::error(400, "Message can't be sent to a secret chat"));
    }
    match content_type {
        MessageContentType::Animation => {
            if !permissions.can_send_animations() {
                return Err(Status::error(
                    400,
                    "Not enough rights to send animations to the chat",
                ));
            }
        }
        MessageContentType::Audio => {
            if !permissions.can_send_audios() {
                return Err(Status::error(
                    400,
                    "Not enough rights to send music to the chat",
                ));
            }
        }
        MessageContentType::Contact => {
            if !permissions.can_send_messages() {
                return Err(Status::error(
                    400,
                    "Not enough rights to send contacts to the chat",
                ));
            }
        }
        MessageContentType::Dice => {
            if !permissions.can_send_stickers() {
                return Err(Status::error(
                    400,
                    "Not enough rights to send dice to the chat",
                ));
            }
        }
        MessageContentType::Document => {
            if !permissions.can_send_documents() {
                return Err(Status::error(
                    400,
                    "Not enough rights to send documents to the chat",
                ));
            }
        }
        MessageContentType::Game => {
            if dialog_type == DialogType::Channel {
                let channel_id = dialog_id.get_channel_id();
                if td.is_broadcast_channel(channel_id) {
                    // return Status::Error(400, "Games can't be sent to channel chats");
                }
                if td.is_monoforum_channel(channel_id) {
                    return Err(Status::error(
                        400,
                        "Games can't be sent to channel direct messages chats",
                    ));
                }
            }
            if !permissions.can_send_games() {
                return Err(Status::error(
                    400,
                    "Not enough rights to send games to the chat",
                ));
            }
        }
        MessageContentType::Giveaway => {
            if !permissions.can_send_messages() {
                return Err(Status::error(
                    400,
                    "Not enough rights to send giveaways to the chat",
                ));
            }
        }
        MessageContentType::GiveawayWinners => {
            if !permissions.can_send_messages() {
                return Err(Status::error(
                    400,
                    "Not enough rights to send giveaway winners to the chat",
                ));
            }
        }
        MessageContentType::Invoice => {
            if !permissions.can_send_messages() {
                return Err(Status::error(
                    400,
                    "Not enough rights to send invoice messages to the chat",
                ));
            }
        }
        MessageContentType::LiveLocation => {
            if !permissions.can_send_messages() {
                return Err(Status::error(
                    400,
                    "Not enough rights to send live locations to the chat",
                ));
            }
        }
        MessageContentType::Location => {
            if !permissions.can_send_messages() {
                return Err(Status::error(
                    400,
                    "Not enough rights to send locations to the chat",
                ));
            }
        }
        MessageContentType::PaidMedia => {
            if is_forward {
                if !permissions.can_send_photos() || !permissions.can_send_videos() {
                    return Err(Status::error(
                        400,
                        "Not enough rights to send paid media to the chat",
                    ));
                }
            } else if !td.is_bot()
                && (dialog_type != DialogType::Channel
                    || !td.is_broadcast_channel(dialog_id.get_channel_id()))
            {
                return Err(Status::error(
                    400,
                    "Paid media can be sent only in channel chats",
                ));
            }
        }
        MessageContentType::Photo => {
            if !permissions.can_send_photos() {
                return Err(Status::error(
                    400,
                    "Not enough rights to send photos to the chat",
                ));
            }
        }
        MessageContentType::Poll => {
            if !permissions.can_send_polls() {
                return Err(Status::error(
                    400,
                    "Not enough rights to send polls to the chat",
                ));
            }
            if dialog_type == DialogType::Channel {
                let channel_id = dialog_id.get_channel_id();
                let MessageContent::Poll(m) = content else {
                    unreachable!()
                };
                if td.is_broadcast_channel(channel_id) && !td.get_poll_is_anonymous(m.poll_id) {
                    return Err(Status::error(
                        400,
                        "Non-anonymous polls can't be sent to channel chats",
                    ));
                }
                if td.is_monoforum_channel(channel_id) {
                    return Err(Status::error(
                        400,
                        "Polls can't be sent to channel direct messages chats",
                    ));
                }
            }
            if dialog_type == DialogType::User
                && !is_forward
                && !td.is_bot()
                && !td.is_user_bot(dialog_id.get_user_id())
                && dialog_id != td.get_my_dialog_id()
            {
                return Err(Status::error(
                    400,
                    "Polls can't be sent to the private chat",
                ));
            }
        }
        MessageContentType::Sticker => {
            if !permissions.can_send_stickers() {
                return Err(Status::error(
                    400,
                    "Not enough rights to send stickers to the chat",
                ));
            }
            if get_message_content_sticker_type(td, content) == StickerType::CustomEmoji {
                return Err(Status::error(400, "Can't send emoji stickers in messages"));
            }
        }
        MessageContentType::Story => {
            if !permissions.can_send_photos() || !permissions.can_send_videos() {
                return Err(Status::error(
                    400,
                    "Not enough rights to send stories to the chat",
                ));
            }
        }
        MessageContentType::Text => {
            if !permissions.can_send_messages() {
                return Err(Status::error(
                    400,
                    "Not enough rights to send text messages to the chat",
                ));
            }
        }
        MessageContentType::ToDoList => {
            if !permissions.can_send_polls() {
                return Err(Status::error(
                    400,
                    "Not enough rights to send checklists to the chat",
                ));
            }
            if dialog_type == DialogType::Channel {
                let channel_id = dialog_id.get_channel_id();
                if td.is_broadcast_channel(channel_id) {
                    return Err(Status::error(
                        400,
                        "Checklists can't be sent to channel chats",
                    ));
                }
                if td.is_monoforum_channel(channel_id) {
                    return Err(Status::error(
                        400,
                        "Checklists can't be sent to channel direct messages chats",
                    ));
                }
            }
        }
        MessageContentType::Venue => {
            if !permissions.can_send_messages() {
                return Err(Status::error(
                    400,
                    "Not enough rights to send venues to the chat",
                ));
            }
        }
        MessageContentType::Video => {
            if !permissions.can_send_videos() {
                return Err(Status::error(
                    400,
                    "Not enough rights to send videos to the chat",
                ));
            }
        }
        MessageContentType::VideoNote => {
            if !permissions.can_send_video_notes() {
                return Err(Status::error(
                    400,
                    "Not enough rights to send video notes to the chat",
                ));
            }
            if dialog_type == DialogType::User
                && td.get_user_voice_messages_forbidden(dialog_id.get_user_id())
            {
                return Err(Status::error(
                    400,
                    "User restricted receiving of video note messages",
                ));
            }
        }
        MessageContentType::VoiceNote => {
            if !permissions.can_send_voice_notes() {
                return Err(Status::error(
                    400,
                    "Not enough rights to send voice notes to the chat",
                ));
            }
            if dialog_type == DialogType::User
                && td.get_user_voice_messages_forbidden(dialog_id.get_user_id())
            {
                return Err(Status::error(
                    400,
                    "User restricted receiving of voice note messages",
                ));
            }
        }
        MessageContentType::Unsupported => {}
        // The C++ tail is the enumerated service types plus
        // `ChatSetTheme`, all `UNREACHABLE()`: sending never offers them.
        _ => unreachable!(),
    }
    Ok(())
}

/// `can_forward_message_content` (MessageContent.cpp:6483-6512). The
/// `RichText` arm is deferred with the unmodeled variant.
pub fn can_forward_message_content(
    td: &dyn MessageContentContext,
    content: &MessageContent,
    is_copy: bool,
) -> bool {
    let content_type = content.get_type();
    if content_type == MessageContentType::Text {
        let MessageContent::Text(text) = content else {
            unreachable!()
        };
        // text must be non-empty if there is no link preview
        return !is_empty_string(&text.text.text)
            || text.web_page_id.is_valid()
            || !text.web_page_url.is_empty();
    }
    if content_type == MessageContentType::Poll {
        let MessageContent::Poll(poll) = content else {
            unreachable!()
        };
        return !is_local_poll_id(poll.poll_id);
    }
    if is_copy {
        if matches!(
            content_type,
            MessageContentType::Giveaway
                | MessageContentType::GiveawayWinners
                | MessageContentType::PaidMedia
                | MessageContentType::Invoice
        ) {
            return false;
        }
        if content_type == MessageContentType::Poll {
            let MessageContent::Poll(poll) = content else {
                unreachable!()
            };
            if !td.has_input_media_poll(poll.poll_id) {
                return false;
            }
        }
    }

    !is_service_message_content(content_type)
        && content_type != MessageContentType::Unsupported
        && !is_expired_message_content(content_type)
}

/// `update_opened_message_content` (MessageContent.cpp:6514-6531).
pub fn update_opened_message_content(content: &mut MessageContent) -> bool {
    match content {
        MessageContent::VideoNote(video_note) => {
            if video_note.is_viewed {
                return false;
            }
            video_note.is_viewed = true;
            true
        }
        MessageContent::VoiceNote(voice_note) => {
            if voice_note.is_listened {
                return false;
            }
            voice_note.is_listened = true;
            true
        }
        _ => false,
    }
}

/// `get_message_content_caption` (MessageContent.cpp) — the private
/// caption lookup `get_message_content_text` falls back to.
fn get_message_content_caption(content: &MessageContent) -> Option<&FormattedText> {
    match content {
        MessageContent::Animation(m) => Some(&m.caption),
        MessageContent::Audio(m) => Some(&m.caption),
        MessageContent::Document(m) => Some(&m.caption),
        MessageContent::Invoice(m) => Some(m.input_invoice.get_caption()),
        MessageContent::PaidMedia(m) => Some(&m.caption),
        MessageContent::Photo(m) => Some(&m.caption),
        MessageContent::Video(m) => Some(&m.caption),
        MessageContent::VoiceNote(m) => Some(&m.caption),
        _ => None,
    }
}

/// `get_message_content_text` (MessageContent.cpp) — the text every
/// search-index and min-id consumer shares; `StarGift` (an unmodeled
/// text-carrying type) lands with its arm.
pub fn get_message_content_text(content: &MessageContent) -> Option<&FormattedText> {
    match content {
        MessageContent::Text(m) => Some(&m.text),
        MessageContent::Game(m) => Some(m.game.get_text()),
        MessageContent::GiftPremium(m) => Some(&m.text),
        MessageContent::GiftCode(m) => Some(&m.text),
        MessageContent::Poll(m) => Some(&m.caption),
        _ => get_message_content_caption(content),
    }
}

/// `get_message_content_text_index_mask` (MessageContent.cpp:6537-6559).
fn get_message_content_text_index_mask(content: &MessageContent) -> i32 {
    let text = get_message_content_text(content);
    match text {
        Some(text) if content.get_type() != MessageContentType::Game => {
            for entity in &text.entities {
                if matches!(
                    entity.r#type,
                    Some(Type::Url) | Some(Type::EmailAddress) | Some(Type::TextUrl)
                ) {
                    return message_search_filter_index_mask(MessageSearchFilter::Url);
                }
            }
            0
        }
        _ => 0,
    }
}

/// `get_message_content_media_index_mask` (MessageContent.cpp:6561-6685).
/// The C++ `td` only feeds the `RichText` arm, which lands with the
/// unmodeled variant.
fn get_message_content_media_index_mask(
    _td: &dyn MessageContentContext,
    content: &MessageContent,
    is_outgoing: bool,
) -> i32 {
    match content {
        MessageContent::Animation(_) => {
            message_search_filter_index_mask(MessageSearchFilter::Animation)
        }
        MessageContent::Audio(_) => message_search_filter_index_mask(MessageSearchFilter::Audio),
        MessageContent::Document(_) => {
            message_search_filter_index_mask(MessageSearchFilter::Document)
        }
        MessageContent::Photo(_) => {
            message_search_filter_index_mask(MessageSearchFilter::Photo)
                | message_search_filter_index_mask(MessageSearchFilter::PhotoAndVideo)
        }
        MessageContent::Video(_) => {
            message_search_filter_index_mask(MessageSearchFilter::Video)
                | message_search_filter_index_mask(MessageSearchFilter::PhotoAndVideo)
        }
        MessageContent::VideoNote(_) => {
            message_search_filter_index_mask(MessageSearchFilter::VideoNote)
                | message_search_filter_index_mask(MessageSearchFilter::VoiceAndVideoNote)
        }
        MessageContent::VoiceNote(_) => {
            message_search_filter_index_mask(MessageSearchFilter::VoiceNote)
                | message_search_filter_index_mask(MessageSearchFilter::VoiceAndVideoNote)
        }
        MessageContent::ChatChangePhoto(_) => {
            message_search_filter_index_mask(MessageSearchFilter::ChatPhoto)
        }
        MessageContent::Call(m) => {
            let mut index_mask = message_search_filter_index_mask(MessageSearchFilter::Call);
            if !is_outgoing
                && (m.discard_reason.r#type == CallDiscardReasonType::Declined
                    || m.discard_reason.r#type == CallDiscardReasonType::Missed)
            {
                index_mask |= message_search_filter_index_mask(MessageSearchFilter::MissedCall);
            }
            index_mask
        }
        MessageContent::ConferenceCall(m) => {
            let mut index_mask = message_search_filter_index_mask(MessageSearchFilter::Call);
            if !is_outgoing && m.was_missed {
                index_mask |= message_search_filter_index_mask(MessageSearchFilter::MissedCall);
            }
            index_mask
        }
        MessageContent::Poll(_) => message_search_filter_index_mask(MessageSearchFilter::Poll),
        // The C++ zero tail enumerates every remaining type explicitly;
        // the modeled set matches it exactly.
        _ => 0,
    }
}

/// `get_message_content_index_mask` (MessageContent.cpp:6687-6691).
pub fn get_message_content_index_mask(
    td: &dyn MessageContentContext,
    content: &MessageContent,
    is_outgoing: bool,
) -> i32 {
    get_message_content_text_index_mask(content)
        | get_message_content_media_index_mask(td, content, is_outgoing)
}

/// `get_individual_message_contents` (MessageContent.cpp:6693-6712) —
/// the album splat. The `Poll` arm delegates through the seam; the
/// `RichText` arm lands with the unmodeled variant.
pub fn get_individual_message_contents(
    td: &dyn MessageContentContext,
    content: &MessageContent,
) -> Vec<MessageContent> {
    match content {
        MessageContent::PaidMedia(m) => m
            .media
            .iter()
            .map(MessageExtendedMedia::get_message_content)
            .collect(),
        MessageContent::Poll(m) => {
            td.get_poll_individual_message_contents(m.poll_id, m.attached_media.as_deref())
        }
        _ => unreachable!(),
    }
}

/// `get_message_content_sticker_type` (MessageContent.cpp:6714-6718).
pub fn get_message_content_sticker_type(
    td: &dyn MessageContentContext,
    content: &MessageContent,
) -> StickerType {
    assert!(matches!(content, MessageContent::Sticker(_)));
    match content {
        MessageContent::Sticker(m) => td.get_sticker_type(m.file_id),
        _ => unreachable!(),
    }
}

/// `get_message_content_pinned_message_id` (MessageContent.cpp:6720-6728).
pub fn get_message_content_pinned_message_id(content: &MessageContent) -> MessageId {
    match content {
        MessageContent::PinMessage(m) => m.message_id,
        _ => MessageId::default(),
    }
}

/// `get_message_content_my_background_info` (MessageContent.cpp:6730-6754).
pub fn get_message_content_my_background_info(
    content: &MessageContent,
    is_outgoing: bool,
) -> BackgroundInfo {
    match content {
        MessageContent::SetBackground(set_background) => {
            if is_outgoing || set_background.for_both {
                return set_background.background_info.clone();
            }
            BackgroundInfo::default()
        }
        _ => BackgroundInfo::default(),
    }
}

// `get_message_content_chat_theme` (MessageContent.cpp:6756-6764) is
// deferred with the unmodeled `ChatSetTheme` variant.

/// `get_message_content_replied_message_full_id` (MessageContent.cpp:6766-6872).
/// The `StarGiftUnique` arm lands with the unmodeled variant.
pub fn get_message_content_replied_message_full_id(
    dialog_id: DialogId,
    content: &MessageContent,
) -> MessageFullId {
    match content {
        MessageContent::PinMessage(m) => {
            MessageFullId::new(dialog_id, to_messages_manager_message_id(m.message_id))
        }
        MessageContent::GameScore(m) => {
            MessageFullId::new(dialog_id, to_messages_manager_message_id(m.game_message_id))
        }
        MessageContent::PaymentSuccessful(m) => {
            if !m.invoice_message_id.is_valid() {
                return MessageFullId::default();
            }
            let reply_in_dialog_id = if m.invoice_dialog_id.is_valid() {
                m.invoice_dialog_id
            } else {
                dialog_id
            };
            MessageFullId::new(
                reply_in_dialog_id,
                to_messages_manager_message_id(m.invoice_message_id),
            )
        }
        MessageContent::SetBackground(m) => {
            if !m.old_message_id.is_valid() {
                return MessageFullId::default();
            }
            MessageFullId::new(dialog_id, to_messages_manager_message_id(m.old_message_id))
        }
        MessageContent::GiveawayResults(m) => {
            if !m.giveaway_message_id.is_valid() {
                return MessageFullId::default();
            }
            MessageFullId::new(
                dialog_id,
                to_messages_manager_message_id(m.giveaway_message_id),
            )
        }
        MessageContent::TodoCompletions(m) => {
            if !m.to_do_message_id.is_valid() {
                return MessageFullId::default();
            }
            MessageFullId::new(
                dialog_id,
                to_messages_manager_message_id(m.to_do_message_id),
            )
        }
        MessageContent::TodoAppendTasks(m) => {
            if !m.to_do_message_id.is_valid() {
                return MessageFullId::default();
            }
            MessageFullId::new(
                dialog_id,
                to_messages_manager_message_id(m.to_do_message_id),
            )
        }
        MessageContent::SuggestedPostSuccess(m) => {
            if !m.suggested_post_message_id.is_valid() {
                return MessageFullId::default();
            }
            MessageFullId::new(
                dialog_id,
                to_messages_manager_message_id(m.suggested_post_message_id),
            )
        }
        MessageContent::SuggestedPostRefund(m) => {
            if !m.suggested_post_message_id.is_valid() {
                return MessageFullId::default();
            }
            MessageFullId::new(
                dialog_id,
                to_messages_manager_message_id(m.suggested_post_message_id),
            )
        }
        MessageContent::SuggestedPostApproval(m) => {
            if !m.suggested_post_message_id.is_valid() {
                return MessageFullId::default();
            }
            MessageFullId::new(
                dialog_id,
                to_messages_manager_message_id(m.suggested_post_message_id),
            )
        }
        MessageContent::NoForwardsToggle(m) => {
            if !m.request_message_id.is_valid() {
                return MessageFullId::default();
            }
            MessageFullId::new(
                dialog_id,
                to_messages_manager_message_id(m.request_message_id),
            )
        }
        MessageContent::PollAppendAnswer(m) => {
            if !m.poll_message_id.is_valid() {
                return MessageFullId::default();
            }
            MessageFullId::new(dialog_id, to_messages_manager_message_id(m.poll_message_id))
        }
        MessageContent::PollDeleteAnswer(m) => {
            if !m.poll_message_id.is_valid() {
                return MessageFullId::default();
            }
            MessageFullId::new(dialog_id, to_messages_manager_message_id(m.poll_message_id))
        }
        // update getRepliedMessage documentation
        _ => MessageFullId::default(),
    }
}

/// `get_message_content_group_call_info` (MessageContent.cpp:6874-6881).
pub fn get_message_content_group_call_info(content: &MessageContent) -> (InputGroupCallId, bool) {
    assert!(matches!(content, MessageContent::GroupCall(_)));
    match content {
        MessageContent::GroupCall(m) => (m.input_group_call_id, m.duration >= 0),
        _ => unreachable!(),
    }
}

/// `get_formatted_text_user_ids` (MessageContent.cpp:6883-6895).
fn get_formatted_text_user_ids(formatted_text: Option<&FormattedText>) -> Vec<UserId> {
    let mut user_ids = Vec::new();
    if let Some(formatted_text) = formatted_text {
        for entity in &formatted_text.entities {
            if entity.user_id.is_valid() {
                user_ids.push(entity.user_id);
            }
        }
    }
    user_ids
}

/// `td::combine` (td/utils/algorithm.h) — append, sort, unique; both
/// inputs are sorted-unique vectors at the call sites.
fn combine_sorted_unique<T: Ord>(mut first: Vec<T>, mut second: Vec<T>) -> Vec<T> {
    first.append(&mut second);
    first.sort_unstable();
    first.dedup();
    first
}

/// `get_message_content_min_user_ids` (MessageContent.cpp:6897-7204).
/// The `Text` web-page arm reads through the seam; the `RichText` arm
/// lands with the unmodeled variant. Every modeled type is the C++
/// `break` arm falling through to the formatted-text fallback.
pub fn get_message_content_min_user_ids(
    td: &dyn MessageContentContext,
    message_content: &MessageContent,
) -> Vec<UserId> {
    match message_content {
        MessageContent::Text(content) => {
            let mut user_ids = get_formatted_text_user_ids(Some(&content.text));
            if content.web_page_id.is_valid() {
                user_ids =
                    combine_sorted_unique(user_ids, td.get_web_page_user_ids(content.web_page_id));
            }
            user_ids
        }
        MessageContent::Contact(content) => {
            let user_id = content.contact.get_user_id();
            if user_id.is_valid() {
                return vec![user_id];
            }
            get_formatted_text_user_ids(get_message_content_text(message_content))
        }
        MessageContent::Game(content) => {
            let user_id = content.game.get_bot_user_id();
            if user_id.is_valid() {
                return vec![user_id];
            }
            get_formatted_text_user_ids(get_message_content_text(message_content))
        }
        MessageContent::ChatCreate(content) => content.participant_user_ids.clone(),
        MessageContent::ChatAddUsers(content) => content.user_ids.clone(),
        MessageContent::ChatDeleteUser(content) => vec![content.user_id],
        MessageContent::ProximityAlertTriggered(content) => {
            let mut user_ids = Vec::new();
            if content.traveler_dialog_id.get_type() == DialogType::User {
                user_ids.push(content.traveler_dialog_id.get_user_id());
            }
            if content.watcher_dialog_id.get_type() == DialogType::User {
                user_ids.push(content.watcher_dialog_id.get_user_id());
            }
            user_ids
        }
        MessageContent::InviteToGroupCall(content) => content.user_ids.clone(),
        MessageContent::Story(content) => {
            let dialog_id = content.story_full_id.get_dialog_id();
            if dialog_id.get_type() == DialogType::User {
                return vec![dialog_id.get_user_id()];
            }
            get_formatted_text_user_ids(get_message_content_text(message_content))
        }
        MessageContent::GiveawayWinners(content) => content.winner_user_ids.clone(),
        MessageContent::NewCreatorPending(content) => {
            if content.new_creator_user_id.is_valid() {
                return vec![content.new_creator_user_id];
            }
            get_formatted_text_user_ids(get_message_content_text(message_content))
        }
        MessageContent::ChangeCreator(content) => vec![content.new_creator_user_id],
        MessageContent::ManagedBotCreated(content) => vec![content.bot_user_id],
        // `ChatSetTtl` keeps its C++ comment: the `from_user_id` user
        // can't be min. The "private chats only" types (DialogShared,
        // PaymentRefunded, PrizeStars, the StarGift family,
        // ConferenceCall) fall through the same way.
        _ => get_formatted_text_user_ids(get_message_content_text(message_content)),
    }
}

/// `get_message_content_min_channel_ids` (MessageContent.cpp:7206-7238).
pub fn get_message_content_min_channel_ids(
    td: &dyn MessageContentContext,
    message_content: &MessageContent,
) -> Vec<ChannelId> {
    match message_content {
        MessageContent::Text(content) => {
            if content.web_page_id.is_valid() {
                return td.get_web_page_channel_ids(content.web_page_id);
            }
            Vec::new()
        }
        MessageContent::ProximityAlertTriggered(content) => {
            let mut channel_ids = Vec::new();
            if content.traveler_dialog_id.get_type() == DialogType::Channel {
                channel_ids.push(content.traveler_dialog_id.get_channel_id());
            }
            if content.watcher_dialog_id.get_type() == DialogType::Channel {
                channel_ids.push(content.watcher_dialog_id.get_channel_id());
            }
            channel_ids
        }
        MessageContent::Story(content) => {
            let dialog_id = content.story_full_id.get_dialog_id();
            if dialog_id.get_type() == DialogType::Channel {
                return vec![dialog_id.get_channel_id()];
            }
            Vec::new()
        }
        MessageContent::Giveaway(content) => content.giveaway_parameters.get_channel_ids(),
        MessageContent::GiveawayWinners(content) => vec![content.boosted_channel_id],
        _ => Vec::new(),
    }
}

/// `get_message_content_added_user_ids` (MessageContent.cpp:7240-7245).
pub fn get_message_content_added_user_ids(content: &MessageContent) -> Vec<UserId> {
    assert!(matches!(content, MessageContent::ChatAddUsers(_)));
    match content {
        MessageContent::ChatAddUsers(m) => m.user_ids.clone(),
        _ => unreachable!(),
    }
}

/// `get_message_content_deleted_user_id` (MessageContent.cpp:7247-7255).
pub fn get_message_content_deleted_user_id(content: &MessageContent) -> UserId {
    match content {
        MessageContent::ChatDeleteUser(m) => m.user_id,
        _ => UserId::default(),
    }
}

/// `get_message_content_input_phone_call` (MessageContent.cpp:7257-7266).
pub fn get_message_content_input_phone_call(
    content: &MessageContent,
) -> Option<telegram_api::InputPhoneCall> {
    assert!(matches!(content, MessageContent::Call(_)));
    match content {
        MessageContent::Call(call) => {
            if call.call_access_hash == 0 {
                return None;
            }
            Some(telegram_api::InputPhoneCall {
                id: call.call_id,
                access_hash: call.call_access_hash,
            })
        }
        _ => unreachable!(),
    }
}

/// `get_message_content_live_location_period` (MessageContent.cpp:7268-7276).
pub fn get_message_content_live_location_period(content: &MessageContent) -> i32 {
    match content {
        MessageContent::LiveLocation(m) => m.period,
        _ => 0,
    }
}

/// `get_message_content_poll_id` (MessageContent.cpp:7278-7286).
pub fn get_message_content_poll_id(content: &MessageContent) -> PollId {
    match content {
        MessageContent::Poll(m) => m.poll_id,
        _ => PollId::default(),
    }
}

/// `get_message_content_poll_is_anonymous` (MessageContent.cpp:7288-7296).
pub fn get_message_content_poll_is_anonymous(
    td: &dyn MessageContentContext,
    content: &MessageContent,
) -> bool {
    match content {
        MessageContent::Poll(m) => td.get_poll_is_anonymous(m.poll_id),
        _ => false,
    }
}

/// `get_message_content_poll_is_closed` (MessageContent.cpp:7298-7306).
pub fn get_message_content_poll_is_closed(
    td: &dyn MessageContentContext,
    content: &MessageContent,
) -> bool {
    match content {
        MessageContent::Poll(m) => td.get_poll_is_closed(m.poll_id),
        _ => true,
    }
}

/// `get_message_content_poll_can_add_option` (MessageContent.cpp:7308-7316).
pub fn get_message_content_poll_can_add_option(
    td: &dyn MessageContentContext,
    content: &MessageContent,
) -> bool {
    match content {
        MessageContent::Poll(m) => td.get_poll_can_add_option(m.poll_id),
        _ => false,
    }
}

/// `get_message_content_poll_can_view_stats` (MessageContent.cpp:7318-7326).
pub fn get_message_content_poll_can_view_stats(
    td: &dyn MessageContentContext,
    content: &MessageContent,
) -> bool {
    match content {
        MessageContent::Poll(m) => td.get_poll_can_view_stats(m.poll_id),
        _ => false,
    }
}

/// `get_message_content_poll_has_unread_votes` (MessageContent.cpp:7328-7336).
pub fn get_message_content_poll_has_unread_votes(
    td: &dyn MessageContentContext,
    content: &MessageContent,
) -> bool {
    match content {
        MessageContent::Poll(m) => td.get_poll_has_unread_votes(m.poll_id),
        _ => false,
    }
}

/// `remove_message_content_poll_has_unread_votes` (MessageContent.cpp:7338-7346).
pub fn remove_message_content_poll_has_unread_votes(
    td: &dyn MessageContentContext,
    content: &MessageContent,
) {
    if let MessageContent::Poll(m) = content {
        td.remove_poll_has_unread_votes(m.poll_id);
    }
}

/// `get_message_content_poll_option_properties` (MessageContent.cpp:7348-7359).
#[allow(clippy::too_many_arguments)] // the C++ handler has the same shape
pub fn get_message_content_poll_option_properties(
    td: &dyn MessageContentContext,
    content: &MessageContent,
    option_id: &str,
    dialog_id: DialogId,
    message_id: MessageId,
    can_be_replied: bool,
    can_be_replied_in_another_chat: bool,
    can_get_link: bool,
    is_forward: bool,
    is_outgoing: bool,
    promise: Promise<crate::td_api::PollOptionProperties>,
) {
    assert!(matches!(content, MessageContent::Poll(_)));
    match content {
        MessageContent::Poll(m) => td.get_poll_option_properties(
            m.poll_id,
            option_id,
            dialog_id,
            message_id,
            can_be_replied,
            can_be_replied_in_another_chat,
            can_get_link,
            is_forward,
            is_outgoing,
            promise,
        ),
        _ => unreachable!(),
    }
}

/// `get_message_content_to_do_list_others_can_append`
/// (MessageContent.cpp:7361-7369).
pub fn get_message_content_to_do_list_others_can_append(content: &MessageContent) -> bool {
    match content {
        MessageContent::ToDoList(m) => m.list.others_can_append,
        _ => false,
    }
}

/// `get_message_content_to_do_list_can_append_items`
/// (MessageContent.cpp:7371-7379) — the C++ reads the
/// `checklist_task_count_max` option through `td`.
pub fn get_message_content_to_do_list_can_append_items(
    td: &dyn MessageContentContext,
    content: &MessageContent,
    item_count: i32,
) -> bool {
    match content {
        MessageContent::ToDoList(m) => {
            let max_task_count = td.get_option_integer("checklist_task_count_max", 0);
            m.list.get_can_append_items(max_task_count, item_count)
        }
        _ => false,
    }
}

/// `get_message_content_to_do_list_others_can_complete`
/// (MessageContent.cpp:7381-7389).
pub fn get_message_content_to_do_list_others_can_complete(content: &MessageContent) -> bool {
    match content {
        MessageContent::ToDoList(m) => m.list.others_can_complete,
        _ => false,
    }
}

/// `get_message_content_photo` (MessageContent.cpp:7391-7399).
pub fn get_message_content_photo(content: &MessageContent) -> Option<&Photo> {
    match content {
        MessageContent::Photo(m) => Some(&m.photo),
        _ => None,
    }
}

/// `get_message_content_venue` (MessageContent.cpp:7401-7409).
pub fn get_message_content_venue(content: &MessageContent) -> Option<&Venue> {
    match content {
        MessageContent::Venue(m) => Some(&m.venue),
        _ => None,
    }
}

/// `get_message_content_web_page_id` (MessageContent.cpp:7411-7415) —
/// the C++ takes a nullable content.
pub fn get_message_content_web_page_id(content: Option<&MessageContent>) -> WebPageId {
    if let Some(MessageContent::Text(m)) = content {
        return m.web_page_id;
    }
    WebPageId::default()
}

/// `has_message_content_web_page` (MessageContent.cpp:7417-7423).
pub fn has_message_content_web_page(content: &MessageContent) -> bool {
    matches!(content, MessageContent::Text(m) if m.web_page_id.is_valid())
}

/// `remove_message_content_web_page` (MessageContent.cpp:7425-7434).
pub fn remove_message_content_web_page(content: &mut MessageContent) {
    assert!(matches!(content, MessageContent::Text(_)));
    if let MessageContent::Text(message_text) = content {
        message_text.web_page_id = WebPageId::default();
        message_text.force_small_media = false;
        message_text.force_large_media = false;
        message_text.skip_web_page_confirmation = false;
        message_text.web_page_url = String::new();
    }
}

/// `can_message_content_have_media_timestamp` (MessageContent.cpp:7436-7452).
pub fn can_message_content_have_media_timestamp(content: &MessageContent) -> bool {
    match content {
        MessageContent::Audio(_)
        | MessageContent::Story(_)
        | MessageContent::Video(_)
        | MessageContent::VideoNote(_)
        | MessageContent::VoiceNote(_) => true,
        MessageContent::Invoice(m) => m.input_invoice.has_media_timestamp(),
        _ => has_message_content_web_page(content),
    }
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

    // ==== M.9.2/M.9.3 tests ====

    /// The raw-id storage stand-in: every mediated call round-trips the
    /// raw id. Byte-compatible with the C++ only once the real managers
    /// back the seam (the module doc records the M.12.2 gate).
    struct RawIdStorage;

    impl MessageContentStorage for RawIdStorage {
        fn store_file(&self, file_id: FileId, out: &mut Vec<u8>) {
            store_i32(out, file_id.get());
        }

        fn parse_file(&mut self, parser: &mut Parser) -> Result<FileId, &'static str> {
            Ok(FileId::new(parser.i32_()?, 0))
        }

        fn store_animation(&self, file_id: FileId, out: &mut Vec<u8>) {
            self.store_file(file_id, out);
        }

        fn parse_animation(&mut self, parser: &mut Parser) -> Result<FileId, &'static str> {
            self.parse_file(parser)
        }

        fn store_audio(&self, file_id: FileId, out: &mut Vec<u8>) {
            self.store_file(file_id, out);
        }

        fn parse_audio(&mut self, parser: &mut Parser) -> Result<FileId, &'static str> {
            self.parse_file(parser)
        }

        fn store_document(&self, file_id: FileId, out: &mut Vec<u8>) {
            self.store_file(file_id, out);
        }

        fn parse_document(&mut self, parser: &mut Parser) -> Result<FileId, &'static str> {
            self.parse_file(parser)
        }

        fn store_video(&self, file_id: FileId, out: &mut Vec<u8>) {
            self.store_file(file_id, out);
        }

        fn parse_video(&mut self, parser: &mut Parser) -> Result<FileId, &'static str> {
            self.parse_file(parser)
        }

        fn store_video_note(&self, file_id: FileId, out: &mut Vec<u8>) {
            self.store_file(file_id, out);
        }

        fn parse_video_note(&mut self, parser: &mut Parser) -> Result<FileId, &'static str> {
            self.parse_file(parser)
        }

        fn store_voice_note(&self, file_id: FileId, out: &mut Vec<u8>) {
            self.store_file(file_id, out);
        }

        fn parse_voice_note(&mut self, parser: &mut Parser) -> Result<FileId, &'static str> {
            self.parse_file(parser)
        }

        fn store_sticker(&self, file_id: FileId, out: &mut Vec<u8>) {
            self.store_file(file_id, out);
        }

        fn parse_sticker(&mut self, parser: &mut Parser) -> Result<FileId, &'static str> {
            self.parse_file(parser)
        }

        fn store_poll(&self, poll_id: PollId, out: &mut Vec<u8>) {
            store_i64(out, poll_id.get());
        }

        fn parse_poll(&mut self, parser: &mut Parser) -> Result<PollId, &'static str> {
            Ok(PollId::new(parser.i64_()?))
        }

        fn store_background(&self, background_id: BackgroundId, out: &mut Vec<u8>) {
            store_i64(out, background_id.get());
        }

        fn parse_background(&mut self, parser: &mut Parser) -> Result<BackgroundId, &'static str> {
            Ok(BackgroundId::new(parser.i64_()?))
        }

        fn on_load_web_page_url_from_database(&mut self, _web_page_id: WebPageId, _url: &str) {}
    }

    /// The representative populated model of every modeled variant —
    /// non-default field values wherever the type carries any, so the
    /// round-trip exercises flags and conditionals, not just defaults.
    fn sample_contents() -> Vec<MessageContent> {
        let user_id = UserId::new(7);
        let dialog_id = DialogId::from_user_id(user_id);
        let message_id = MessageId::new(4242);
        let file_id = FileId::new(11, 3);
        let text = |s: &str| FormattedText {
            text: s.to_string(),
            entities: vec![MessageEntity::new(
                crate::message_entity::Type::Italic,
                0,
                2,
            )],
        };
        vec![
            MessageContent::Text(MessageText::new(
                text("hello"),
                WebPageId::new(99),
                true,
                false,
                true,
                "https://example.com".to_string(),
            )),
            MessageContent::Animation(MessageAnimation {
                file_id,
                caption: text("cap"),
                has_spoiler: true,
            }),
            MessageContent::Audio(MessageAudio {
                file_id,
                caption: text("cap"),
            }),
            MessageContent::Contact(MessageContact {
                contact: Contact::new(
                    "+15551234567".to_string(),
                    "John".to_string(),
                    "Doe".to_string(),
                    String::new(),
                    user_id,
                ),
            }),
            MessageContent::Document(MessageDocument {
                file_id,
                caption: text("cap"),
            }),
            MessageContent::Game(MessageGame {
                game: Game {
                    id: 5,
                    access_hash: -6,
                    bot_user_id: user_id,
                    short_name: "game".to_string(),
                    title: "The Game".to_string(),
                    description: "desc".to_string(),
                    photo: Photo::default(),
                    animation_file_id: file_id,
                    text: text("game text"),
                },
            }),
            MessageContent::Invoice(MessageInvoice {
                input_invoice: InputInvoice {
                    title: "t".to_string(),
                    description: "d".to_string(),
                    photo: Photo::default(),
                    start_parameter: "sp".to_string(),
                    invoice: Invoice {
                        currency: "USD".to_string(),
                        price_parts: vec![LabeledPricePart {
                            label: "part".to_string(),
                            amount: 250,
                        }],
                        subscription_period: 30,
                        max_tip_amount: 10,
                        suggested_tip_amounts: vec![5, 20],
                        recurring_payment_terms_of_service_url: "r".to_string(),
                        terms_of_service_url: "tos".to_string(),
                        is_test: true,
                        need_name: true,
                        need_phone_number: true,
                        need_email_address: true,
                        need_shipping_address: true,
                        send_phone_number_to_provider: true,
                        send_email_address_to_provider: true,
                        is_flexible: true,
                    },
                    payload: "payload".to_string(),
                    provider_token: "tok".to_string(),
                    provider_data: "pdata".to_string(),
                    extended_media: MessageExtendedMedia {
                        r#type: MessageExtendedMediaType::Preview,
                        unsupported_version: 1,
                        duration: 3,
                        dimensions: crate::photo::get_dimensions(10, 20, "test"),
                        minithumbnail: "mini".to_string(),
                        photo: Photo::default(),
                        video_file_id: FileId::default(),
                        start_timestamp: 9,
                    },
                    extended_media_caption: text("caption"),
                    total_amount: 250,
                    receipt_message_id: message_id,
                },
            }),
            MessageContent::LiveLocation(MessageLiveLocation::new(
                Location::new(1.5, -2.5, 30.0, -7),
                90,
                180,
                1500,
            )),
            MessageContent::Location(MessageLocation {
                location: Location::new(10.0, 20.0, 5.0, -8),
            }),
            MessageContent::Venue(MessageVenue {
                venue: Venue {
                    location: Location::new(30.0, 40.0, 0.0, -9),
                    title: "Title".to_string(),
                    address: "Address".to_string(),
                    provider: "provider".to_string(),
                    id: "venue-id".to_string(),
                    r#type: "venue-type".to_string(),
                },
            }),
            MessageContent::Photo(MessagePhoto {
                photo: Photo {
                    id: 123,
                    date: 1000,
                    minithumbnail: "mini".to_string(),
                    photos: vec![crate::photo::PhotoSize {
                        size_type: b'x' as i32,
                        size: 42,
                        dimensions: crate::photo::get_dimensions(100, 200, "test"),
                        file_id,
                        progressive_sizes: vec![1, 2, 3],
                    }],
                    animations: Vec::new(),
                    sticker_photo_size: None,
                    has_stickers: true,
                },
                video_file_id: file_id,
                caption: text("cap"),
                has_spoiler: true,
            }),
            MessageContent::Sticker(MessageSticker {
                file_id,
                is_premium: true,
            }),
            MessageContent::Video(MessageVideo {
                file_id,
                alternative_file_ids: vec![FileId::new(12, 0)],
                hls_file_ids: vec![FileId::new(13, 0)],
                storyboard_file_ids: vec![FileId::new(14, 0)],
                storyboard_map_file_ids: vec![FileId::new(15, 0)],
                cover: Photo::default(),
                start_timestamp: 33,
                caption: text("cap"),
                has_spoiler: true,
            }),
            MessageContent::VideoNote(MessageVideoNote {
                file_id,
                is_viewed: true,
            }),
            MessageContent::VoiceNote(MessageVoiceNote {
                file_id,
                caption: text("cap"),
                is_listened: true,
            }),
            MessageContent::ChatCreate(MessageChatCreate {
                title: "chat".to_string(),
                participant_user_ids: vec![user_id, UserId::new(9)],
            }),
            MessageContent::ChatChangeTitle(MessageChatChangeTitle {
                title: "renamed".to_string(),
            }),
            MessageContent::ChatChangePhoto(MessageChatChangePhoto {
                photo: Photo {
                    id: 77,
                    date: 1001,
                    minithumbnail: String::new(),
                    photos: vec![],
                    animations: vec![crate::photo::AnimationSize {
                        photo_size: crate::photo::PhotoSize {
                            size_type: b's' as i32,
                            size: 7,
                            dimensions: crate::photo::get_dimensions(8, 8, "test"),
                            file_id,
                            progressive_sizes: Vec::new(),
                        },
                        main_frame_timestamp: 0.5,
                    }],
                    sticker_photo_size: None,
                    has_stickers: false,
                },
            }),
            MessageContent::ChatAddUsers(MessageChatAddUsers {
                user_ids: vec![user_id],
            }),
            MessageContent::ChatJoinedByLink(MessageChatJoinedByLink { is_approved: true }),
            MessageContent::ChatDeleteUser(MessageChatDeleteUser { user_id }),
            MessageContent::ChatMigrateTo(MessageChatMigrateTo {
                migrated_to_channel_id: crate::chat_manager::ChannelId::new(1001),
            }),
            MessageContent::ChannelCreate(MessageChannelCreate {
                title: "channel".to_string(),
            }),
            MessageContent::ChannelMigrateFrom(MessageChannelMigrateFrom {
                title: "old".to_string(),
                migrated_from_chat_id: ChatId::new(42),
            }),
            MessageContent::PinMessage(MessagePinMessage { message_id }),
            MessageContent::GameScore(MessageGameScore {
                game_message_id: message_id,
                game_id: 123456789,
                score: 1000,
            }),
            MessageContent::ChatSetTtl(MessageChatSetTtl {
                ttl: 86400,
                from_user_id: user_id,
            }),
            MessageContent::Unsupported(MessageUnsupported { version: 3 }),
            MessageContent::Call(MessageCall {
                call_id: 777,
                call_access_hash: -12345,
                duration: 65,
                discard_reason: CallDiscardReason {
                    r#type: CallDiscardReasonType::HungUp,
                    slug: String::new(),
                },
                is_video: true,
            }),
            MessageContent::PaymentSuccessful(MessagePaymentSuccessful {
                invoice_dialog_id: dialog_id,
                invoice_message_id: message_id,
                currency: "USD".to_string(),
                total_amount: 999,
                invoice_payload: "pl".to_string(),
                subscription_until_date: 2000,
                is_recurring: true,
                is_first_recurring: true,
                shipping_option_id: "opt".to_string(),
                order_info: Some(OrderInfo {
                    name: "John Doe".to_string(),
                    phone_number: "+1555".to_string(),
                    email_address: "a@b.c".to_string(),
                    shipping_address: Some(Box::new(Address {
                        country_code: "US".to_string(),
                        state: "CA".to_string(),
                        city: "SF".to_string(),
                        street_line1: "1 st".to_string(),
                        street_line2: String::new(),
                        postal_code: "94100".to_string(),
                    })),
                }),
                telegram_payment_charge_id: "tc".to_string(),
                provider_payment_charge_id: "pc".to_string(),
            }),
            MessageContent::CustomServiceAction(MessageCustomServiceAction {
                message: "action".to_string(),
            }),
            MessageContent::WebsiteConnected(MessageWebsiteConnected {
                domain_name: "example.com".to_string(),
            }),
            MessageContent::PassportDataSent(MessagePassportDataSent {
                types: vec![SecureValueType::Passport, SecureValueType::EmailAddress],
            }),
            MessageContent::PassportDataReceived(MessagePassportDataReceived {
                values: vec![EncryptedSecureValue {
                    r#type: SecureValueType::Address,
                    data: EncryptedSecureData {
                        data: "d".to_string(),
                        hash: "h".to_string(),
                        encrypted_secret: "s".to_string(),
                    },
                    files: vec![EncryptedSecureFile {
                        file: DatedFile {
                            file_id,
                            date: 1000,
                        },
                        file_hash: "fh".to_string(),
                        encrypted_secret: "es".to_string(),
                    }],
                    front_side: EncryptedSecureFile::default(),
                    reverse_side: EncryptedSecureFile::default(),
                    selfie: EncryptedSecureFile::default(),
                    translations: Vec::new(),
                    hash: "vh".to_string(),
                }],
                credentials: EncryptedSecureCredentials {
                    data: "d".to_string(),
                    hash: "h".to_string(),
                    encrypted_secret: "s".to_string(),
                },
            }),
            MessageContent::Poll(MessagePoll {
                poll_id: PollId::new(555),
                caption: text("poll cap"),
                attached_media: Some(Box::new(MessageContent::Unsupported(MessageUnsupported {
                    version: 1,
                }))),
            }),
            MessageContent::Dice(MessageDice {
                emoji: "🎲".to_string(),
                dice_value: 5,
                is_stake: false,
                seed: "seed".to_string(),
                state_hash: "hash".to_string(),
                stake_ton_count: 0,
                prize_ton_count: 0,
            }),
            MessageContent::ProximityAlertTriggered(MessageProximityAlertTriggered {
                traveler_dialog_id: dialog_id,
                watcher_dialog_id: DialogId::from_user_id(UserId::new(8)),
                distance: 42,
            }),
            MessageContent::GroupCall(MessageGroupCall {
                input_group_call_id: InputGroupCallId::new(11, -22),
                duration: 30,
                schedule_date: 2000,
            }),
            MessageContent::InviteToGroupCall(MessageInviteToGroupCall {
                input_group_call_id: InputGroupCallId::new(12, -23),
                user_ids: vec![user_id],
            }),
            MessageContent::WebViewDataSent(MessageWebViewDataSent {
                button_text: "btn".to_string(),
            }),
            MessageContent::WebViewDataReceived(MessageWebViewDataReceived {
                button_text: "btn".to_string(),
                data: "data".to_string(),
            }),
            MessageContent::GiftPremium(MessageGiftPremium {
                text: text("gift"),
                currency: "USD".to_string(),
                amount: 5,
                crypto_currency: "TON".to_string(),
                crypto_amount: 1,
                days: 30,
            }),
            MessageContent::TopicCreate(MessageTopicCreate {
                title: "topic".to_string(),
                icon: ForumTopicIcon::new(0xFF0000, crate::emoji_status::CustomEmojiId::new(9)),
                title_missing: true,
            }),
            MessageContent::TopicEdit(MessageTopicEdit {
                edited_data: ForumTopicEditedData {
                    title: "new".to_string(),
                    icon_custom_emoji_id: crate::emoji_status::CustomEmojiId::new(3),
                    edit_icon_custom_emoji_id: true,
                    edit_is_closed: true,
                    is_closed: true,
                    edit_is_hidden: true,
                    is_hidden: true,
                },
            }),
            MessageContent::SuggestProfilePhoto(MessageSuggestProfilePhoto {
                photo: Photo {
                    id: 88,
                    date: 1002,
                    minithumbnail: String::new(),
                    photos: vec![crate::photo::PhotoSize {
                        size_type: b'x' as i32,
                        size: 9,
                        dimensions: crate::photo::get_dimensions(50, 50, "test"),
                        file_id: FileId::new(31, 0),
                        progressive_sizes: Vec::new(),
                    }],
                    animations: Vec::new(),
                    sticker_photo_size: None,
                    has_stickers: false,
                },
            }),
            MessageContent::RequestedDialog(MessageRequestedDialog {
                shared_dialog_ids: vec![dialog_id],
                button_id: 5,
            }),
            MessageContent::WebViewWriteAccessAllowed(MessageWebViewWriteAccessAllowed {
                web_app: WebApp {
                    id: 1,
                    access_hash: -2,
                    short_name: "app".to_string(),
                    title: "App".to_string(),
                    description: "d".to_string(),
                    photo: Photo::default(),
                    animation_file_id: FileId::default(),
                    hash: 99,
                },
            }),
            MessageContent::SetBackground(MessageSetBackground {
                old_message_id: message_id,
                background_info: BackgroundInfo {
                    background_id: BackgroundId::new(44),
                    background_type: BackgroundType::pattern(
                        true,
                        BackgroundFill::gradient(1, 2, 135),
                        50,
                    ),
                },
                for_both: true,
            }),
            MessageContent::Story(MessageStory {
                story_full_id: StoryFullId::new(dialog_id, StoryId::new(10)),
                via_mention: true,
            }),
            MessageContent::GiftCode(MessageGiftCode {
                creator_dialog_id: dialog_id,
                text: text("giftcode"),
                days: 3,
                currency: "USD".to_string(),
                amount: 10,
                crypto_currency: "TON".to_string(),
                crypto_amount: 2,
                via_giveaway: true,
                is_unclaimed: true,
                code: "CODE".to_string(),
            }),
            MessageContent::Giveaway(MessageGiveaway {
                giveaway_parameters: GiveawayParameters {
                    boosted_channel_id: crate::chat_manager::ChannelId::new(1002),
                    additional_channel_ids: vec![crate::chat_manager::ChannelId::new(1003)],
                    only_new_subscribers: true,
                    winners_are_visible: true,
                    date: 2000,
                    country_codes: vec!["US".to_string()],
                    prize_description: "prize".to_string(),
                },
                quantity: 5,
                months: 3,
                star_count: 100,
            }),
            MessageContent::GiveawayLaunch(MessageGiveawayLaunch { star_count: 50 }),
            MessageContent::GiveawayResults(MessageGiveawayResults {
                giveaway_message_id: message_id,
                winner_count: 3,
                unclaimed_count: 1,
                is_star_giveaway: true,
            }),
            MessageContent::GiveawayWinners(MessageGiveawayWinners {
                giveaway_message_id: message_id,
                boosted_channel_id: crate::chat_manager::ChannelId::new(1004),
                additional_dialog_count: 2,
                month_count: 1,
                star_count: 10,
                prize_description: "p".to_string(),
                winners_selection_date: 2100,
                only_new_subscribers: true,
                was_refunded: true,
                winner_count: 2,
                unclaimed_count: 1,
                winner_user_ids: vec![user_id],
            }),
            MessageContent::BoostApply(MessageBoostApply { boost_count: 3 }),
            MessageContent::DialogShared(MessageDialogShared {
                shared_dialogs: vec![SharedDialog {
                    dialog_id: DialogId::from_user_id(UserId::new(6)),
                    first_name: "A".to_string(),
                    ..SharedDialog::default()
                }],
                button_id: 7,
            }),
            MessageContent::PaidMedia(MessagePaidMedia {
                media: vec![MessageExtendedMedia {
                    r#type: MessageExtendedMediaType::Video,
                    unsupported_version: 0,
                    duration: 5,
                    dimensions: crate::photo::get_dimensions(0, 0, "test"),
                    minithumbnail: String::new(),
                    photo: Photo::default(),
                    video_file_id: FileId::new(21, 0),
                    start_timestamp: 2,
                }],
                caption: text("paid"),
                star_count: 15,
                payload: "pay".to_string(),
            }),
            MessageContent::PaymentRefunded(MessagePaymentRefunded {
                dialog_id,
                currency: "USD".to_string(),
                total_amount: 50,
                invoice_payload: "pl".to_string(),
                telegram_payment_charge_id: "tc".to_string(),
                provider_payment_charge_id: "pc".to_string(),
            }),
            MessageContent::GiftStars(MessageGiftStars {
                currency: "USD".to_string(),
                amount: 5,
                crypto_currency: "TON".to_string(),
                crypto_amount: 1,
                star_count: 100,
                transaction_id: "tx".to_string(),
            }),
            MessageContent::PrizeStars(MessagePrizeStars {
                star_count: 20,
                transaction_id: "tx".to_string(),
                boosted_dialog_id: DialogId::from_channel_id(crate::chat_manager::ChannelId::new(
                    1005,
                )),
                giveaway_message_id: message_id,
                is_unclaimed: true,
            }),
            MessageContent::PaidMessagesRefunded(MessagePaidMessagesRefunded {
                message_count: 4,
                star_count: 40,
            }),
            MessageContent::PaidMessagesPrice(MessagePaidMessagesPrice {
                star_count: 10,
                broadcast_messages_allowed: true,
            }),
            MessageContent::ConferenceCall(MessageConferenceCall {
                call_id: 31,
                duration: 120,
                other_participant_dialog_ids: vec![dialog_id],
                is_active: true,
                is_video: true,
                was_missed: true,
            }),
            MessageContent::ToDoList(MessageToDoList {
                list: ToDoList {
                    title: text("todo"),
                    items: vec![ToDoItem {
                        id: 1,
                        title: text("task"),
                    }],
                    others_can_append: true,
                    others_can_complete: true,
                },
                completions: vec![ToDoCompletion {
                    id: 1,
                    completed_by_dialog_id: DialogId::from_user_id(UserId::new(7)),
                    date: 1500,
                }],
            }),
            MessageContent::TodoCompletions(MessageTodoCompletions {
                to_do_message_id: message_id,
                completed_task_ids: vec![1],
                incompleted_task_ids: vec![2, 3],
            }),
            MessageContent::TodoAppendTasks(MessageTodoAppendTasks {
                to_do_message_id: message_id,
                items: vec![ToDoItem {
                    id: 2,
                    title: text("task2"),
                }],
            }),
            MessageContent::GiftTon(MessageGiftTon {
                currency: "USD".to_string(),
                amount: 5,
                crypto_currency: "TON".to_string(),
                crypto_amount: 1,
                transaction_id: "tx".to_string(),
            }),
            MessageContent::SuggestedPostSuccess(MessageSuggestedPostSuccess {
                suggested_post_message_id: message_id,
                amount: CurrencyAmount {
                    r#type: CurrencyAmountType::Star,
                    star_amount: StarAmount {
                        star_count: 5,
                        nanostar_count: 1,
                    },
                    ton_amount: TonAmount::default(),
                },
            }),
            MessageContent::SuggestedPostRefund(MessageSuggestedPostRefund {
                suggested_post_message_id: message_id,
                is_payer_initiated: true,
            }),
            MessageContent::SuggestedPostApproval(MessageSuggestedPostApproval {
                suggested_post_message_id: message_id,
                price: SuggestedPostPrice {
                    r#type: SuggestedPostPriceType::Ton,
                    amount: 7_000_000,
                },
                comment: "ok".to_string(),
                send_date: 2500,
                is_rejected: true,
                is_balance_too_low: true,
            }),
            MessageContent::SuggestBirthday(MessageSuggestBirthday {
                birthdate: crate::birthdate::Birthdate::new(29, 2, 1990),
            }),
            MessageContent::NewCreatorPending(MessageNewCreatorPending {
                new_creator_user_id: user_id,
            }),
            MessageContent::ChangeCreator(MessageChangeCreator {
                new_creator_user_id: user_id,
            }),
            MessageContent::NoForwardsToggle(MessageNoForwardsToggle {
                request_message_id: message_id,
                prev_value: true,
                new_value: false,
            }),
            MessageContent::NoForwardsRequest(MessageNoForwardsRequest {
                is_expired: true,
                prev_value: true,
                new_value: false,
            }),
            MessageContent::ManagedBotCreated(MessageManagedBotCreated {
                bot_user_id: user_id,
            }),
            MessageContent::PollAppendAnswer(MessagePollAppendAnswer {
                poll_message_id: message_id,
                text: text("answer"),
                data: "data".to_string(),
            }),
            MessageContent::PollDeleteAnswer(MessagePollDeleteAnswer {
                poll_message_id: message_id,
                text: text("answer"),
                data: "data".to_string(),
            }),
            MessageContent::ChangeCommunity(MessageChangeCommunity {
                community_id: CommunityId::new(77),
            }),
            MessageContent::ChatJoinedViaCommunity(MessageChatJoinedViaCommunity {
                community_id: CommunityId::new(78),
            }),
            // The field-less variants.
            MessageContent::ChatDeletePhoto(MessageChatDeletePhoto),
            MessageContent::ChatDeleteHistory(MessageChatDeleteHistory),
            MessageContent::ContactRegistered(MessageContactRegistered),
            MessageContent::ExpiredPhoto(MessageExpiredPhoto),
            MessageContent::ExpiredVideo(MessageExpiredVideo),
            MessageContent::ExpiredVideoNote(MessageExpiredVideoNote),
            MessageContent::ExpiredVoiceNote(MessageExpiredVoiceNote),
            MessageContent::ScreenshotTaken(MessageScreenshotTaken),
            MessageContent::WriteAccessAllowed(MessageWriteAccessAllowed),
            MessageContent::WriteAccessAllowedByRequest(MessageWriteAccessAllowedByRequest),
        ]
    }

    #[test]
    fn every_modeled_variant_round_trips_through_the_db_format() {
        let contents = sample_contents();
        assert_eq!(contents.len(), 90, "one sample per modeled variant");
        for content in contents {
            let mut out = Vec::new();
            store_message_content(&content, &RawIdStorage, &mut out);
            let mut parser = Parser::new(&out);
            let parsed = parse_message_content(&mut parser, &mut RawIdStorage).unwrap();
            parser.fetch_end().unwrap();
            assert_eq!(
                parsed.get_type(),
                content.get_type(),
                "round trip changed the content type"
            );
            assert_eq!(
                parsed,
                content,
                "round trip changed the bytes of {:?}",
                content.get_type()
            );
        }
    }

    #[test]
    fn unsupported_content_stores_the_type_int_and_version() {
        let mut out = Vec::new();
        store_message_content(
            &MessageContent::Unsupported(MessageUnsupported { version: 5 }),
            &RawIdStorage,
            &mut out,
        );
        // Type int 27 (`Unsupported`), then the version — the whole blob.
        assert_eq!(out, [0x1b, 0, 0, 0, 5, 0, 0, 0]);

        // A pre-`AddMessageUnsupportedVersion` (16) blob carries no version.
        let blob = [0x1bu8, 0, 0, 0];
        let mut parser = Parser::new_with_version(&blob, 15);
        match parse_message_content(&mut parser, &mut RawIdStorage).unwrap() {
            MessageContent::Unsupported(m) => assert_eq!(m.version, 0),
            _ => panic!("wrong type"),
        }
    }

    #[test]
    fn unknown_and_unmodeled_types_degrade_to_unsupported_without_consuming() {
        // Type int 93 is `RichText` — known to the C++ binary but unmodeled
        // here; the C++ `default:`/is_bad arm returns `MessageUnsupported(0)`
        // and leaves the remainder unconsumed.
        let blob = [93u8, 0, 0, 0, 1, 2, 3, 4];
        let mut parser = Parser::new_with_version(&blob, 61);
        let parsed = parse_message_content(&mut parser, &mut RawIdStorage).unwrap();
        assert!(matches!(parsed, MessageContent::Unsupported(m) if m.version == 0));
        assert_eq!(parser.left_len(), 4, "the trailing bytes must stay");
    }

    #[test]
    fn legacy_version_branches_match_the_cpp() {
        // Dice pre-AddDiceEmoji (25)/AddDiceFlags (57): no flags block, no
        // emoji — the default die emoji is the C++ fallback.
        let mut blob = Vec::new();
        store_i32(&mut blob, MessageContentType::Dice as i32);
        store_i32(&mut blob, 3);
        let mut parser = Parser::new_with_version(&blob, 24);
        match parse_message_content(&mut parser, &mut RawIdStorage).unwrap() {
            MessageContent::Dice(d) => {
                assert_eq!(d.emoji, MessageDice::DEFAULT_EMOJI);
                assert_eq!(d.dice_value, 3);
                assert!(!d.is_stake);
            }
            _ => panic!("wrong type"),
        }

        // GiftPremium with has_days=false runs the month-to-day conversion:
        // 12 months -> 12*30 + 12/3 + 12/12 = 365.
        let mut blob = Vec::new();
        store_i32(&mut blob, MessageContentType::GiftPremium as i32);
        store_flags(&mut blob, [false, false, false]);
        store_string(&mut blob, "USD");
        store_i64(&mut blob, 5);
        store_i32(&mut blob, 12);
        let mut parser = Parser::new_with_version(&blob, 61);
        match parse_message_content(&mut parser, &mut RawIdStorage).unwrap() {
            MessageContent::GiftPremium(g) => assert_eq!(g.days, 365),
            _ => panic!("wrong type"),
        }

        // Call pre-AddVideoCallsSupport (20): no flags, is_video=false.
        let mut blob = Vec::new();
        store_i32(&mut blob, MessageContentType::Call as i32);
        store_i64(&mut blob, 99);
        store_i32(&mut blob, 30);
        store_i32(&mut blob, CallDiscardReasonType::Missed as i32);
        let mut parser = Parser::new_with_version(&blob, 19);
        match parse_message_content(&mut parser, &mut RawIdStorage).unwrap() {
            MessageContent::Call(c) => {
                assert!(!c.is_video);
                assert_eq!(c.call_access_hash, 0);
                assert_eq!(c.discard_reason.r#type, CallDiscardReasonType::Missed);
            }
            _ => panic!("wrong type"),
        }

        // LiveLocation pre-heading (29)/proximity (30): both default to 0.
        let mut blob = Vec::new();
        store_i32(&mut blob, MessageContentType::LiveLocation as i32);
        store_flags(&mut blob, [false, false, false]);
        store_f64(&mut blob, 0.0);
        store_f64(&mut blob, 0.0);
        store_i32(&mut blob, 60);
        let mut parser = Parser::new_with_version(&blob, 28);
        match parse_message_content(&mut parser, &mut RawIdStorage).unwrap() {
            MessageContent::LiveLocation(l) => {
                assert_eq!(l.period, 60);
                assert_eq!(l.heading, 0);
                assert_eq!(l.proximity_alert_radius, 0);
            }
            _ => panic!("wrong type"),
        }

        // ChatSetTtl pre-AddMessageChatSetTtlFlags (44): no from_user_id.
        let mut blob = Vec::new();
        store_i32(&mut blob, MessageContentType::ChatSetTtl as i32);
        store_i32(&mut blob, 60);
        let mut parser = Parser::new_with_version(&blob, 43);
        match parse_message_content(&mut parser, &mut RawIdStorage).unwrap() {
            MessageContent::ChatSetTtl(m) => {
                assert_eq!(m.ttl, 60);
                assert!(!m.from_user_id.is_valid());
            }
            _ => panic!("wrong type"),
        }

        // GiveawayLaunch pre-SupportStarGiveaways (54): no flags, no stars.
        let mut blob = Vec::new();
        store_i32(&mut blob, MessageContentType::GiveawayLaunch as i32);
        let mut parser = Parser::new_with_version(&blob, 53);
        match parse_message_content(&mut parser, &mut RawIdStorage).unwrap() {
            MessageContent::GiveawayLaunch(m) => assert_eq!(m.star_count, 0),
            _ => panic!("wrong type"),
        }

        // RequestedDialog pre-SupportMultipleSharedUsers (52): one dialog,
        // no flags block.
        let mut blob = Vec::new();
        store_i32(&mut blob, MessageContentType::RequestedDialog as i32);
        store_i64(&mut blob, DialogId::from_user_id(UserId::new(7)).get());
        store_i32(&mut blob, 3);
        let mut parser = Parser::new_with_version(&blob, 51);
        match parse_message_content(&mut parser, &mut RawIdStorage).unwrap() {
            MessageContent::RequestedDialog(m) => {
                assert_eq!(m.shared_dialog_ids.len(), 1);
                assert_eq!(m.button_id, 3);
            }
            _ => panic!("wrong type"),
        }

        // Text pre-AddMessageTextFlags (49): has_web_page_id defaults true,
        // so the web page id still follows the text.
        let mut blob = Vec::new();
        store_i32(&mut blob, MessageContentType::Text as i32);
        store_string(&mut blob, "text");
        store_i32(&mut blob, 0);
        store_i64(&mut blob, 7);
        let mut parser = Parser::new_with_version(&blob, 48);
        match parse_message_content(&mut parser, &mut RawIdStorage).unwrap() {
            MessageContent::Text(m) => {
                assert_eq!(m.web_page_id, WebPageId::new(7));
                assert!(m.web_page_url.is_empty());
            }
            _ => panic!("wrong type"),
        }
    }

    #[test]
    fn caption_parse_re_derives_entities_pre_add_caption_entities() {
        // An Audio caption blob written before AddCaptionEntities (13)
        // stores only the text; find_entities re-derives the URL entity.
        let mut blob = Vec::new();
        store_i32(&mut blob, MessageContentType::Audio as i32);
        store_i32(&mut blob, 77); // the raw-id audio file id
        store_string(&mut blob, "see a.com here");
        store_i32(&mut blob, 1); // the legacy is_listened dummy
        let mut parser = Parser::new_with_version(&blob, 12);
        match parse_message_content(&mut parser, &mut RawIdStorage).unwrap() {
            MessageContent::Audio(a) => {
                assert_eq!(a.caption.text, "see a.com here");
                assert!(!a.caption.entities.is_empty());
                assert!(a.file_id.is_valid());
            }
            _ => panic!("wrong type"),
        }
    }

    #[test]
    fn invalid_content_degrades_to_unsupported() {
        // A ChatChangePhoto with an empty photo is is_bad.
        let mut out = Vec::new();
        store_message_content(
            &MessageContent::ChatChangePhoto(MessageChatChangePhoto {
                photo: Photo::default(),
            }),
            &RawIdStorage,
            &mut out,
        );
        let parsed = parse_message_content(&mut Parser::new(&out), &mut RawIdStorage).unwrap();
        assert!(matches!(parsed, MessageContent::Unsupported(m) if m.version == 0));

        // A non-server story id is is_bad.
        let mut out = Vec::new();
        store_message_content(
            &MessageContent::Story(MessageStory {
                story_full_id: StoryFullId::new(
                    DialogId::from_user_id(UserId::new(1)),
                    StoryId::new(2_000_000_000),
                ),
                via_mention: false,
            }),
            &RawIdStorage,
            &mut out,
        );
        let parsed = parse_message_content(&mut Parser::new(&out), &mut RawIdStorage).unwrap();
        assert!(matches!(parsed, MessageContent::Unsupported(_)));

        // A live location with period 0 is is_bad.
        let mut out = Vec::new();
        store_message_content(
            &MessageContent::LiveLocation(MessageLiveLocation {
                location: Location::default(),
                period: 0,
                heading: 0,
                proximity_alert_radius: 0,
            }),
            &RawIdStorage,
            &mut out,
        );
        let parsed = parse_message_content(&mut Parser::new(&out), &mut RawIdStorage).unwrap();
        assert!(matches!(parsed, MessageContent::Unsupported(_)));

        // A negative boost count is is_bad.
        let mut out = Vec::new();
        store_message_content(
            &MessageContent::BoostApply(MessageBoostApply { boost_count: -1 }),
            &RawIdStorage,
            &mut out,
        );
        let parsed = parse_message_content(&mut Parser::new(&out), &mut RawIdStorage).unwrap();
        assert!(matches!(parsed, MessageContent::Unsupported(_)));
    }

    #[test]
    fn call_bytes_pin_the_flag_layout() {
        let call = MessageContent::Call(MessageCall {
            call_id: 1,
            call_access_hash: -1,
            duration: 2,
            discard_reason: CallDiscardReason {
                r#type: CallDiscardReasonType::Declined,
                slug: String::new(),
            },
            is_video: true,
        });
        let mut out = Vec::new();
        store_message_content(&call, &RawIdStorage, &mut out);
        let expected: Vec<u8> = (MessageContentType::Call as i32)
            .to_le_bytes()
            .into_iter()
            .chain(0b11u32.to_le_bytes()) // is_video | has_call_access_hash
            .chain(1i64.to_le_bytes())
            .chain(2i32.to_le_bytes())
            .chain((CallDiscardReasonType::Declined as i32).to_le_bytes())
            .chain((-1i64).to_le_bytes())
            .collect();
        assert_eq!(out, expected);
    }

    // ------------------------------------------------------------------
    // M.9.7/M.9.8/M.9.9 — the classification, policy and accessor planes.
    // ------------------------------------------------------------------

    /// The `MessageContentContext` stand-in: a permission table over a
    /// restricted rights word, plus flags for the manager seams the
    /// policy plane reads. Manager reads without a test default
    /// `log_error!` and answer like an empty manager would.
    struct TestContext {
        rights: RestrictedRights,
        is_bot: bool,
        my_dialog_id: DialogId,
        broadcast: bool,
        monoforum: bool,
        voice_forbidden: bool,
        poll_anonymous: bool,
        sticker_type: StickerType,
        web_page_user_ids: Vec<UserId>,
        web_page_channel_ids: Vec<ChannelId>,
        poll_input_media: bool,
        checklist_max: i64,
        poll_contents: Vec<MessageContent>,
    }

    impl Default for TestContext {
        fn default() -> Self {
            TestContext {
                rights: RestrictedRights::from_bools(
                    true,
                    true,
                    true,
                    true,
                    true,
                    true,
                    true,
                    true,
                    true,
                    true,
                    true,
                    true,
                    true,
                    true,
                    true,
                    true,
                    true,
                    true,
                    true,
                    true,
                    ChannelType::Unknown,
                ),
                is_bot: false,
                my_dialog_id: DialogId::default(),
                broadcast: false,
                monoforum: false,
                voice_forbidden: false,
                poll_anonymous: true,
                sticker_type: StickerType::Regular,
                web_page_user_ids: Vec::new(),
                web_page_channel_ids: Vec::new(),
                poll_input_media: true,
                checklist_max: 100,
                poll_contents: Vec::new(),
            }
        }
    }

    impl TestContext {
        fn restricted(&mut self, flags: u64) -> &mut Self {
            self.rights = RestrictedRights::from_flags(flags);
            self
        }
    }

    impl MessageContentContext for TestContext {
        fn get_user_default_permissions(&self, _user_id: UserId) -> RestrictedRights {
            self.rights
        }

        fn get_secret_chat_default_permissions(
            &self,
            _secret_chat_id: SecretChatId,
        ) -> RestrictedRights {
            self.rights
        }

        fn is_user_bot(&self, _user_id: UserId) -> bool {
            false
        }

        fn get_user_voice_messages_forbidden(&self, _user_id: UserId) -> bool {
            self.voice_forbidden
        }

        fn get_chat_effective_restricted_rights(&self, _chat_id: ChatId) -> RestrictedRights {
            self.rights
        }

        fn get_channel_effective_restricted_rights(
            &self,
            _channel_id: ChannelId,
        ) -> RestrictedRights {
            self.rights
        }

        fn is_broadcast_channel(&self, _channel_id: ChannelId) -> bool {
            self.broadcast
        }

        fn is_monoforum_channel(&self, _channel_id: ChannelId) -> bool {
            self.monoforum
        }

        fn is_bot(&self) -> bool {
            self.is_bot
        }

        fn get_my_dialog_id(&self) -> DialogId {
            self.my_dialog_id
        }

        fn get_poll_is_anonymous(&self, _poll_id: PollId) -> bool {
            self.poll_anonymous
        }

        fn get_poll_is_closed(&self, _poll_id: PollId) -> bool {
            false
        }

        fn get_poll_can_add_option(&self, _poll_id: PollId) -> bool {
            false
        }

        fn get_poll_can_view_stats(&self, _poll_id: PollId) -> bool {
            false
        }

        fn get_poll_has_unread_votes(&self, _poll_id: PollId) -> bool {
            true
        }

        fn remove_poll_has_unread_votes(&self, _poll_id: PollId) {}

        fn has_input_media_poll(&self, _poll_id: PollId) -> bool {
            self.poll_input_media
        }

        fn get_poll_individual_message_contents(
            &self,
            _poll_id: PollId,
            _attached_media: Option<&MessageContent>,
        ) -> Vec<MessageContent> {
            self.poll_contents.clone()
        }

        fn get_poll_option_properties(
            &self,
            _poll_id: PollId,
            _option_id: &str,
            _dialog_id: DialogId,
            _message_id: MessageId,
            _can_be_replied: bool,
            _can_be_replied_in_another_chat: bool,
            _can_get_link: bool,
            _is_forward: bool,
            _is_outgoing: bool,
            _promise: Promise<crate::td_api::PollOptionProperties>,
        ) {
        }

        fn get_sticker_type(&self, _file_id: FileId) -> StickerType {
            self.sticker_type
        }

        fn get_web_page_user_ids(&self, _web_page_id: WebPageId) -> Vec<UserId> {
            self.web_page_user_ids.clone()
        }

        fn get_web_page_channel_ids(&self, _web_page_id: WebPageId) -> Vec<ChannelId> {
            self.web_page_channel_ids.clone()
        }

        fn get_option_integer(&self, name: &str, default: i64) -> i64 {
            if name == "checklist_task_count_max" {
                self.checklist_max
            } else {
                default
            }
        }
    }

    fn simple_text(text: &str) -> MessageContent {
        MessageContent::Text(MessageText::new(
            FormattedText {
                text: text.to_string(),
                entities: Vec::new(),
            },
            WebPageId::default(),
            false,
            false,
            false,
            String::new(),
        ))
    }

    /// The modeled C++ false-set of `is_service_message_content`
    /// (MessageContentType.cpp:623-731): 27 of the 28 `false` arms
    /// (`RichText` unmodeled); everything else is service.
    #[test]
    fn service_classification_partitions_like_the_cpp() {
        let non_service = [
            MessageContentType::Animation,
            MessageContentType::Audio,
            MessageContentType::Contact,
            MessageContentType::Dice,
            MessageContentType::Document,
            MessageContentType::ExpiredPhoto,
            MessageContentType::ExpiredVideo,
            MessageContentType::ExpiredVideoNote,
            MessageContentType::ExpiredVoiceNote,
            MessageContentType::Game,
            MessageContentType::Giveaway,
            MessageContentType::GiveawayWinners,
            MessageContentType::Invoice,
            MessageContentType::LiveLocation,
            MessageContentType::Location,
            MessageContentType::PaidMedia,
            MessageContentType::Photo,
            MessageContentType::Poll,
            MessageContentType::Sticker,
            MessageContentType::Story,
            MessageContentType::Text,
            MessageContentType::ToDoList,
            MessageContentType::Unsupported,
            MessageContentType::Venue,
            MessageContentType::Video,
            MessageContentType::VideoNote,
            MessageContentType::VoiceNote,
        ];
        assert_eq!(non_service.len(), 27);
        for content_type in non_service {
            assert!(!is_service_message_content(content_type));
        }
        // the expired four are non-service and expired at the same time;
        // everything else non-service is not expired
        for content_type in [
            MessageContentType::Animation,
            MessageContentType::Audio,
            MessageContentType::Contact,
            MessageContentType::Dice,
            MessageContentType::Document,
            MessageContentType::Game,
            MessageContentType::Giveaway,
            MessageContentType::GiveawayWinners,
            MessageContentType::Invoice,
            MessageContentType::LiveLocation,
            MessageContentType::Location,
            MessageContentType::PaidMedia,
            MessageContentType::Photo,
            MessageContentType::Poll,
            MessageContentType::Sticker,
            MessageContentType::Story,
            MessageContentType::Text,
            MessageContentType::ToDoList,
            MessageContentType::Unsupported,
            MessageContentType::Venue,
            MessageContentType::Video,
            MessageContentType::VideoNote,
            MessageContentType::VoiceNote,
        ] {
            assert!(!is_expired_message_content(content_type));
        }
        for content_type in [
            MessageContentType::ExpiredPhoto,
            MessageContentType::ExpiredVideo,
            MessageContentType::ExpiredVideoNote,
            MessageContentType::ExpiredVoiceNote,
        ] {
            assert!(is_expired_message_content(content_type));
        }
        assert!(is_service_message_content(MessageContentType::ChatCreate));
        assert!(is_service_message_content(MessageContentType::Call));
        assert!(is_service_message_content(
            MessageContentType::PollAppendAnswer
        ));
        // C++ UNREACHABLEs on `None` (it enumerates only real types); the
        // complement answers "service" — the can_be_secret_message_content
        // degradation precedent.
        assert!(is_service_message_content(MessageContentType::None));
    }

    /// `get_expired_message_content_type` (MessageContentType.cpp:897-909).
    #[test]
    fn expired_content_type_mapping_matches_the_cpp() {
        assert_eq!(
            get_expired_message_content_type(MessageContentType::Photo),
            MessageContentType::ExpiredPhoto
        );
        assert_eq!(
            get_expired_message_content_type(MessageContentType::Video),
            MessageContentType::ExpiredVideo
        );
        assert_eq!(
            get_expired_message_content_type(MessageContentType::VideoNote),
            MessageContentType::ExpiredVideoNote
        );
        assert_eq!(
            get_expired_message_content_type(MessageContentType::VoiceNote),
            MessageContentType::ExpiredVoiceNote
        );
        for content_type in [
            MessageContentType::Text,
            MessageContentType::ExpiredPhoto,
            MessageContentType::None,
            MessageContentType::Unsupported,
        ] {
            assert_eq!(
                get_expired_message_content_type(content_type),
                MessageContentType::None
            );
        }
    }

    /// `can_send_message_content_to_secret_chat`
    /// (MessageContentType.cpp:1019-1121): the 13 true types, the 9
    /// modeled false types (`RichText` is the tenth, unmodeled).
    #[test]
    fn secret_chat_sendable_types_match_the_cpp() {
        for content_type in [
            MessageContentType::Animation,
            MessageContentType::Audio,
            MessageContentType::Contact,
            MessageContentType::Document,
            MessageContentType::LiveLocation,
            MessageContentType::Location,
            MessageContentType::Photo,
            MessageContentType::Sticker,
            MessageContentType::Text,
            MessageContentType::Venue,
            MessageContentType::Video,
            MessageContentType::VideoNote,
            MessageContentType::VoiceNote,
        ] {
            assert!(can_send_message_content_to_secret_chat(content_type));
        }
        for content_type in [
            MessageContentType::Dice,
            MessageContentType::Game,
            MessageContentType::Giveaway,
            MessageContentType::GiveawayWinners,
            MessageContentType::Invoice,
            MessageContentType::PaidMedia,
            MessageContentType::Poll,
            MessageContentType::Story,
            MessageContentType::ToDoList,
        ] {
            assert!(!can_send_message_content_to_secret_chat(content_type));
        }
    }

    /// The index-mask families (MessageContent.cpp:6537-6691): the media
    /// bits, the missed-call extras and the Game text exemption.
    #[test]
    fn index_masks_match_the_cpp_tables() {
        let td = TestContext::default();
        let photo = MessageContent::Photo(MessagePhoto {
            photo: Photo::default(),
            video_file_id: FileId::default(),
            caption: FormattedText::default(),
            has_spoiler: false,
        });
        let expected_photo = message_search_filter_index_mask(MessageSearchFilter::Photo)
            | message_search_filter_index_mask(MessageSearchFilter::PhotoAndVideo);
        assert_eq!(
            get_message_content_index_mask(&td, &photo, true),
            expected_photo
        );

        let video = MessageContent::Video(MessageVideo {
            file_id: FileId::default(),
            alternative_file_ids: Vec::new(),
            hls_file_ids: Vec::new(),
            storyboard_file_ids: Vec::new(),
            storyboard_map_file_ids: Vec::new(),
            cover: Photo::default(),
            start_timestamp: 0,
            caption: FormattedText::default(),
            has_spoiler: false,
        });
        let expected_video = message_search_filter_index_mask(MessageSearchFilter::Video)
            | message_search_filter_index_mask(MessageSearchFilter::PhotoAndVideo);
        assert_eq!(
            get_message_content_index_mask(&td, &video, false),
            expected_video
        );

        let declined_call = MessageContent::Call(MessageCall {
            call_id: 1,
            call_access_hash: -1,
            duration: -1,
            discard_reason: CallDiscardReason {
                r#type: CallDiscardReasonType::Declined,
                slug: String::new(),
            },
            is_video: false,
        });
        let call_bit = message_search_filter_index_mask(MessageSearchFilter::Call);
        let missed_bit = message_search_filter_index_mask(MessageSearchFilter::MissedCall);
        assert_eq!(
            get_message_content_index_mask(&td, &declined_call, false),
            call_bit | missed_bit
        );
        assert_eq!(
            get_message_content_index_mask(&td, &declined_call, true),
            call_bit
        );

        let missed_conference = MessageContent::ConferenceCall(MessageConferenceCall {
            call_id: 1,
            duration: -1,
            other_participant_dialog_ids: Vec::new(),
            is_active: false,
            is_video: false,
            was_missed: true,
        });
        assert_eq!(
            get_message_content_index_mask(&td, &missed_conference, false),
            call_bit | missed_bit
        );

        let with_url = MessageContent::Text(MessageText::new(
            FormattedText {
                text: "a https://example.com b".to_string(),
                entities: vec![MessageEntity::new(Type::TextUrl, 2, 18)],
            },
            WebPageId::default(),
            false,
            false,
            false,
            String::new(),
        ));
        assert_eq!(
            get_message_content_index_mask(&td, &with_url, true),
            message_search_filter_index_mask(MessageSearchFilter::Url)
        );

        let game = MessageContent::Game(MessageGame {
            game: Game {
                id: 1,
                access_hash: 2,
                bot_user_id: UserId::default(),
                short_name: "g".to_string(),
                title: "T".to_string(),
                description: String::new(),
                photo: Photo::default(),
                animation_file_id: FileId::default(),
                text: with_url_text(),
            },
        });
        assert_eq!(get_message_content_index_mask(&td, &game, true), 0);
    }

    fn with_url_text() -> FormattedText {
        FormattedText {
            text: "https://example.com".to_string(),
            entities: vec![MessageEntity::new(Type::Url, 0, 19)],
        }
    }

    /// `can_forward_message_content` (MessageContent.cpp:6483-6512).
    #[test]
    fn forwardability_matches_the_cpp_rules() {
        let td = TestContext::default();
        assert!(!can_forward_message_content(&td, &simple_text(""), false));
        assert!(can_forward_message_content(&td, &simple_text("hi"), false));

        let with_preview = MessageContent::Text(MessageText::new(
            FormattedText::default(),
            WebPageId::new(7),
            false,
            false,
            false,
            "https://example.com".to_string(),
        ));
        assert!(can_forward_message_content(&td, &with_preview, false));

        let local_poll = MessageContent::Poll(MessagePoll {
            poll_id: PollId::new(-3),
            caption: FormattedText::default(),
            attached_media: None,
        });
        assert!(!can_forward_message_content(&td, &local_poll, false));
        let server_poll = MessageContent::Poll(MessagePoll {
            poll_id: PollId::new(3),
            caption: FormattedText::default(),
            attached_media: None,
        });
        assert!(can_forward_message_content(&td, &server_poll, false));
        // the C++ early-returns for every poll, so the trailing is_copy
        // poll arm below it is shadowed — non-local polls are forwardable
        // even without stored input media
        assert!(can_forward_message_content(&td, &server_poll, true));

        let invoice = MessageContent::Invoice(MessageInvoice {
            input_invoice: InputInvoice::default(),
        });
        assert!(!can_forward_message_content(&td, &invoice, true));
        assert!(can_forward_message_content(&td, &invoice, false));

        assert!(!can_forward_message_content(&td, &expired_photo(), false));
    }

    fn expired_photo() -> MessageContent {
        MessageContent::ExpiredPhoto(MessageExpiredPhoto {})
    }

    /// `update_opened_message_content` (MessageContent.cpp:6514-6531).
    #[test]
    fn opened_content_flips_once_like_the_cpp() {
        let mut video_note = MessageContent::VideoNote(MessageVideoNote {
            file_id: FileId::new(9, 0),
            is_viewed: false,
        });
        assert!(update_opened_message_content(&mut video_note));
        assert!(!update_opened_message_content(&mut video_note));
        match &video_note {
            MessageContent::VideoNote(m) => assert!(m.is_viewed),
            _ => unreachable!(),
        }

        let mut voice_note = MessageContent::VoiceNote(MessageVoiceNote {
            file_id: FileId::new(10, 0),
            caption: FormattedText::default(),
            is_listened: false,
        });
        assert!(update_opened_message_content(&mut voice_note));
        assert!(!update_opened_message_content(&mut voice_note));

        let mut text = simple_text("hi");
        assert!(!update_opened_message_content(&mut text));
    }

    /// The paid-media album splat (MessageContent.cpp:6693-6700) keeps
    /// the `MessageExtendedMedia` order and routes the poll arm through
    /// the seam.
    #[test]
    fn individual_contents_splat_paid_media_in_order() {
        let td = TestContext::default();
        let paid_media = MessageContent::PaidMedia(MessagePaidMedia {
            media: vec![
                MessageExtendedMedia {
                    r#type: MessageExtendedMediaType::Photo,
                    photo: Photo::default(),
                    ..Default::default()
                },
                MessageExtendedMedia {
                    r#type: MessageExtendedMediaType::Video,
                    video_file_id: FileId::new(4, 1),
                    start_timestamp: 33,
                    ..Default::default()
                },
            ],
            caption: FormattedText::default(),
            star_count: 10,
            payload: String::new(),
        });
        let individual = get_individual_message_contents(&td, &paid_media);
        assert_eq!(individual.len(), 2);
        assert!(matches!(individual[0], MessageContent::Photo(_)));
        match &individual[1] {
            MessageContent::Video(m) => {
                assert_eq!(m.file_id, FileId::new(4, 1));
                assert_eq!(m.start_timestamp, 33);
                assert!(m.cover.is_empty());
            }
            _ => panic!("expected the video splat"),
        }

        let poll_td = TestContext {
            poll_contents: vec![simple_text("poll")],
            ..Default::default()
        };
        let poll = MessageContent::Poll(MessagePoll {
            poll_id: PollId::new(5),
            caption: FormattedText::default(),
            attached_media: None,
        });
        assert_eq!(get_individual_message_contents(&poll_td, &poll).len(), 1);
    }

    /// `can_send_message_content` (MessageContent.cpp:6207-6481): the
    /// rights mapping, the secret-chat gate and the per-type extras.
    #[test]
    fn can_send_follows_the_cpp_decision_table() {
        let user_id = UserId::new(7);
        let dialog_id = DialogId::from_user_id(user_id);
        let channel_id = ChannelId::new(77);
        let channel_dialog = DialogId::from_channel_id(channel_id);

        // check_permissions = false allows everything sendable.
        let mut td = TestContext::default();
        td.restricted(0);
        assert!(can_send_message_content(&td, dialog_id, &simple_text("hi"), false, false).is_ok());

        // Text with no can_send_messages right (an empty rights word).
        let mut td = TestContext::default();
        td.restricted(0);
        let error =
            can_send_message_content(&td, dialog_id, &simple_text("hi"), false, true).unwrap_err();
        assert_eq!(error.code(), 400);
        assert_eq!(
            error.message(),
            "Not enough rights to send text messages to the chat"
        );

        // Dice rides the stickers right; animations denied separately.
        let stickers_bit = 1u64 << 18;
        let animations_bit = 1u64 << 19;
        let mut td = TestContext::default();
        td.restricted(stickers_bit);
        let dice = MessageContent::Dice(MessageDice::new(
            MessageDice::DEFAULT_EMOJI,
            0,
            false,
            String::new(),
            String::new(),
            0,
            0,
        ));
        assert!(can_send_message_content(&td, dialog_id, &dice, false, true).is_ok());
        let animation = MessageContent::Animation(MessageAnimation {
            file_id: FileId::new(1, 0),
            caption: FormattedText::default(),
            has_spoiler: false,
        });
        let error = can_send_message_content(&td, dialog_id, &animation, false, true).unwrap_err();
        assert_eq!(
            error.message(),
            "Not enough rights to send animations to the chat"
        );
        let mut td = TestContext::default();
        td.restricted(animations_bit);
        assert!(can_send_message_content(&td, dialog_id, &animation, false, true).is_ok());

        // Secret-chat gate wins over the rights table.
        let secret_chat_id = SecretChatId::new(9);
        let secret_dialog = DialogId::from_secret_chat_id(secret_chat_id);
        let poll = MessageContent::Poll(MessagePoll {
            poll_id: PollId::new(5),
            caption: FormattedText::default(),
            attached_media: None,
        });
        let td = TestContext::default();
        assert_eq!(
            can_send_message_content(&td, secret_dialog, &poll, false, true)
                .unwrap_err()
                .message(),
            "Message can't be sent to a secret chat"
        );
        // a default context answers like an existing secret chat, whose
        // default permissions allow text
        let td = TestContext::default();
        assert!(
            can_send_message_content(&td, secret_dialog, &simple_text("hi"), false, true).is_ok()
        );

        // Paid media: forward needs photo+video; direct send is
        // broadcast-channels-only for users.
        let photos_bit = 1u64 << 34;
        let paid_media = MessageContent::PaidMedia(MessagePaidMedia {
            media: Vec::new(),
            caption: FormattedText::default(),
            star_count: 5,
            payload: String::new(),
        });
        let mut td = TestContext::default();
        td.restricted(photos_bit);
        let error = can_send_message_content(&td, dialog_id, &paid_media, true, true).unwrap_err();
        assert_eq!(
            error.message(),
            "Not enough rights to send paid media to the chat"
        );
        let td = TestContext::default();
        let error = can_send_message_content(&td, dialog_id, &paid_media, false, true).unwrap_err();
        assert_eq!(
            error.message(),
            "Paid media can be sent only in channel chats"
        );
        let td = TestContext {
            broadcast: true,
            ..Default::default()
        };
        assert!(can_send_message_content(&td, channel_dialog, &paid_media, false, true).is_ok());

        // Non-anonymous polls can't go to broadcast channels; polls
        // can't go to private chats from users.
        let td = TestContext {
            broadcast: true,
            poll_anonymous: false,
            ..Default::default()
        };
        let error = can_send_message_content(&td, channel_dialog, &poll, false, true).unwrap_err();
        assert_eq!(
            error.message(),
            "Non-anonymous polls can't be sent to channel chats"
        );
        let td = TestContext::default();
        let error = can_send_message_content(&td, dialog_id, &poll, false, true).unwrap_err();
        assert_eq!(error.message(), "Polls can't be sent to the private chat");
        let td = TestContext {
            is_bot: true,
            ..Default::default()
        };
        assert!(can_send_message_content(&td, dialog_id, &poll, false, true).is_ok());

        // Video notes to a user that restricted voice messages.
        let video_note = MessageContent::VideoNote(MessageVideoNote {
            file_id: FileId::new(3, 0),
            is_viewed: false,
        });
        let td = TestContext {
            voice_forbidden: true,
            ..Default::default()
        };
        let error = can_send_message_content(&td, dialog_id, &video_note, false, true).unwrap_err();
        assert_eq!(
            error.message(),
            "User restricted receiving of video note messages"
        );

        // Custom-emoji stickers can't be sent as messages.
        let sticker = MessageContent::Sticker(MessageSticker {
            file_id: FileId::new(2, 0),
            is_premium: false,
        });
        let td = TestContext {
            sticker_type: StickerType::CustomEmoji,
            ..Default::default()
        };
        let error = can_send_message_content(&td, dialog_id, &sticker, false, true).unwrap_err();
        assert_eq!(error.message(), "Can't send emoji stickers in messages");

        // Checklists can't go to broadcast channels and ride the poll
        // right.
        let todo = MessageContent::ToDoList(MessageToDoList {
            list: ToDoList::default(),
            completions: Vec::new(),
        });
        let td = TestContext {
            broadcast: true,
            ..Default::default()
        };
        let error = can_send_message_content(&td, channel_dialog, &todo, false, true).unwrap_err();
        assert_eq!(error.message(), "Checklists can't be sent to channel chats");
        let polls_bit = 1u64 << 23;
        let mut td = TestContext::default();
        td.restricted(polls_bit);
        assert!(can_send_message_content(&td, dialog_id, &todo, false, true).is_ok());

        // Games can't go to monoforum channel direct messages chats.
        let game = MessageContent::Game(MessageGame {
            game: Game::default(),
        });
        let td = TestContext {
            monoforum: true,
            ..Default::default()
        };
        let error = can_send_message_content(&td, channel_dialog, &game, false, true).unwrap_err();
        assert_eq!(
            error.message(),
            "Games can't be sent to channel direct messages chats"
        );
    }

    /// The replied-message id extraction (MessageContent.cpp:6766-6872).
    #[test]
    fn replied_message_full_ids_match_the_cpp_arms() {
        let user_id = UserId::new(7);
        let dialog_id = DialogId::from_user_id(user_id);
        let other_dialog = DialogId::from_channel_id(ChannelId::new(50));
        let full_id = |dialog_id, message_id: MessageId| {
            MessageFullId::new(dialog_id, to_messages_manager_message_id(message_id))
        };

        let pin = MessageContent::PinMessage(MessagePinMessage {
            message_id: MessageId::new(11),
        });
        assert_eq!(
            get_message_content_replied_message_full_id(dialog_id, &pin),
            full_id(dialog_id, MessageId::new(11))
        );

        // the validity-checked arms need full-form ids: a server message
        // id has empty type bits (`MessageId::is_valid`, MessageId.cpp:104)
        let payment = MessageContent::PaymentSuccessful(MessagePaymentSuccessful {
            invoice_dialog_id: other_dialog,
            invoice_message_id: MessageId::new(12 << 20),
            ..Default::default()
        });
        assert_eq!(
            get_message_content_replied_message_full_id(dialog_id, &payment),
            full_id(other_dialog, MessageId::new(12 << 20))
        );
        let own_payment = MessageContent::PaymentSuccessful(MessagePaymentSuccessful {
            invoice_dialog_id: DialogId::default(),
            invoice_message_id: MessageId::new(13 << 20),
            ..Default::default()
        });
        assert_eq!(
            get_message_content_replied_message_full_id(dialog_id, &own_payment),
            full_id(dialog_id, MessageId::new(13 << 20))
        );
        let invalid_payment = MessageContent::PaymentSuccessful(MessagePaymentSuccessful {
            invoice_dialog_id: DialogId::default(),
            invoice_message_id: MessageId::default(),
            ..Default::default()
        });
        assert_eq!(
            get_message_content_replied_message_full_id(dialog_id, &invalid_payment),
            MessageFullId::default()
        );

        for content in [
            simple_text("hi"),
            MessageContent::ScreenshotTaken(MessageScreenshotTaken {}),
        ] {
            assert_eq!(
                get_message_content_replied_message_full_id(dialog_id, &content),
                MessageFullId::default()
            );
        }

        let background = MessageContent::SetBackground(MessageSetBackground {
            old_message_id: MessageId::new(21 << 20),
            background_info: BackgroundInfo::default(),
            for_both: false,
        });
        assert_eq!(
            get_message_content_replied_message_full_id(dialog_id, &background),
            full_id(dialog_id, MessageId::new(21 << 20))
        );
    }

    /// `MessagePaymentSuccessful::Default` — the C++ field-initializer
    /// defaults the M.10.1b plane pinned.
    #[test]
    fn min_user_ids_follow_the_cpp_arms() {
        let user_id = UserId::new(7);
        let other_user_id = UserId::new(8);
        let dialog_id = DialogId::from_user_id(user_id);
        let td = TestContext::default();

        let chat_create = MessageContent::ChatCreate(MessageChatCreate {
            title: "t".to_string(),
            participant_user_ids: vec![other_user_id, user_id],
        });
        assert_eq!(
            get_message_content_min_user_ids(&td, &chat_create),
            vec![other_user_id, user_id]
        );

        let proximity = MessageContent::ProximityAlertTriggered(MessageProximityAlertTriggered {
            traveler_dialog_id: DialogId::from_user_id(user_id),
            watcher_dialog_id: DialogId::from_channel_id(ChannelId::new(5)),
            distance: 10,
        });
        assert_eq!(
            get_message_content_min_user_ids(&td, &proximity),
            vec![user_id]
        );
        assert_eq!(
            get_message_content_min_channel_ids(&td, &proximity),
            vec![ChannelId::new(5)]
        );

        let story = MessageContent::Story(MessageStory {
            story_full_id: StoryFullId::new(dialog_id, StoryId::new(3)),
            via_mention: false,
        });
        assert_eq!(get_message_content_min_user_ids(&td, &story), vec![user_id]);
        assert_eq!(
            get_message_content_min_channel_ids(&td, &story),
            Vec::<ChannelId>::new()
        );
        let channel_story = MessageContent::Story(MessageStory {
            story_full_id: StoryFullId::new(
                DialogId::from_channel_id(ChannelId::new(6)),
                StoryId::new(3),
            ),
            via_mention: false,
        });
        assert_eq!(
            get_message_content_min_channel_ids(&td, &channel_story),
            vec![ChannelId::new(6)]
        );

        let giveaway = MessageContent::Giveaway(MessageGiveaway {
            giveaway_parameters: GiveawayParameters {
                boosted_channel_id: ChannelId::new(2),
                additional_channel_ids: vec![ChannelId::new(4), ChannelId::new(3)],
                ..Default::default()
            },
            quantity: 1,
            months: 2,
            star_count: 0,
        });
        assert_eq!(
            get_message_content_min_channel_ids(&td, &giveaway),
            vec![ChannelId::new(4), ChannelId::new(3), ChannelId::new(2)]
        );

        let contact = MessageContent::Contact(MessageContact {
            contact: Contact::new(
                "+15551234567".to_string(),
                "J".to_string(),
                "D".to_string(),
                String::new(),
                user_id,
            ),
        });
        assert_eq!(
            get_message_content_min_user_ids(&td, &contact),
            vec![user_id]
        );

        // Text min ids combine the formatted-text entities with the web
        // page ids, sorted-unique; the entity user_id is invalid here,
        // so only the web page ids survive the combination.
        let td = TestContext {
            web_page_user_ids: vec![user_id, UserId::new(100)],
            ..Default::default()
        };
        let text = MessageContent::Text(MessageText::new(
            FormattedText {
                text: "hi @a".to_string(),
                entities: vec![MessageEntity {
                    r#type: Some(Type::MentionName),
                    offset: 3,
                    length: 2,
                    ..Default::default()
                }],
            },
            WebPageId::new(9),
            false,
            false,
            false,
            String::new(),
        ));
        assert_eq!(
            get_message_content_min_user_ids(&td, &text),
            vec![user_id, UserId::new(100)]
        );
    }

    /// The named user-id accessors (MessageContent.cpp:7240-7255).
    #[test]
    fn added_and_deleted_user_ids_match_the_cpp() {
        let user_id = UserId::new(7);
        let other_user_id = UserId::new(9);
        let added = MessageContent::ChatAddUsers(MessageChatAddUsers {
            user_ids: vec![user_id, other_user_id],
        });
        assert_eq!(
            get_message_content_added_user_ids(&added),
            vec![user_id, other_user_id]
        );
        let deleted = MessageContent::ChatDeleteUser(MessageChatDeleteUser {
            user_id: other_user_id,
        });
        assert_eq!(get_message_content_deleted_user_id(&deleted), other_user_id);
        assert_eq!(
            get_message_content_deleted_user_id(&simple_text("hi")),
            UserId::default()
        );
    }

    /// The phone-call/live-location/poll/to-do accessor families
    /// (MessageContent.cpp:7257-7389).
    #[test]
    fn small_accessors_match_the_cpp_defaults() {
        let td = TestContext::default();

        let call = MessageContent::Call(MessageCall {
            call_id: 42,
            call_access_hash: -7,
            duration: 60,
            discard_reason: CallDiscardReason::default(),
            is_video: false,
        });
        match get_message_content_input_phone_call(&call) {
            Some(telegram_api::InputPhoneCall { id, access_hash }) => {
                assert_eq!(id, 42);
                assert_eq!(access_hash, -7);
            }
            _ => panic!("expected inputPhoneCall"),
        }
        let no_hash_call = MessageContent::Call(MessageCall {
            call_id: 42,
            call_access_hash: 0,
            ..Default::default()
        });
        assert!(get_message_content_input_phone_call(&no_hash_call).is_none());

        let live_location = MessageContent::LiveLocation(MessageLiveLocation {
            location: Location::default(),
            period: 900,
            heading: -1,
            proximity_alert_radius: 0,
        });
        assert_eq!(
            get_message_content_live_location_period(&live_location),
            900
        );
        assert_eq!(
            get_message_content_live_location_period(&simple_text("hi")),
            0
        );

        let poll = MessageContent::Poll(MessagePoll {
            poll_id: PollId::new(5),
            caption: FormattedText::default(),
            attached_media: None,
        });
        assert_eq!(get_message_content_poll_id(&poll), PollId::new(5));
        assert_eq!(
            get_message_content_poll_id(&simple_text("hi")),
            PollId::default()
        );
        assert!(get_message_content_poll_is_anonymous(&td, &poll));
        assert!(get_message_content_poll_is_closed(&td, &simple_text("hi")));
        assert!(get_message_content_poll_has_unread_votes(&td, &poll));

        let todo = MessageContent::ToDoList(MessageToDoList {
            list: ToDoList {
                title: FormattedText::default(),
                items: vec![ToDoItem::default(); 3],
                others_can_append: true,
                others_can_complete: false,
            },
            completions: Vec::new(),
        });
        assert!(get_message_content_to_do_list_others_can_append(&todo));
        assert!(!get_message_content_to_do_list_others_can_complete(&todo));
        assert!(get_message_content_to_do_list_can_append_items(
            &td, &todo, 97
        ));
        assert!(!get_message_content_to_do_list_can_append_items(
            &td, &todo, 98
        ));
        assert!(!get_message_content_to_do_list_can_append_items(
            &td, &todo, -1
        ));
        assert!(!get_message_content_to_do_list_can_append_items(
            &td,
            &simple_text("hi"),
            1
        ));
    }

    /// The media accessors (MessageContent.cpp:7391-7452) and the
    /// background-info gate (MessageContent.cpp:6730-6754).
    #[test]
    fn media_accessors_and_background_info_match_the_cpp() {
        let photo = MessageContent::Photo(MessagePhoto {
            photo: Photo::default(),
            video_file_id: FileId::default(),
            caption: FormattedText::default(),
            has_spoiler: false,
        });
        assert!(get_message_content_photo(&photo).is_some());
        assert!(get_message_content_photo(&simple_text("hi")).is_none());
        assert!(get_message_content_venue(&photo).is_none());

        let text_with_preview = MessageContent::Text(MessageText::new(
            FormattedText::default(),
            WebPageId::new(12),
            true,
            true,
            true,
            "https://example.com".to_string(),
        ));
        assert!(has_message_content_web_page(&text_with_preview));
        assert_eq!(
            get_message_content_web_page_id(Some(&text_with_preview)),
            WebPageId::new(12)
        );
        assert_eq!(get_message_content_web_page_id(None), WebPageId::default());
        assert!(can_message_content_have_media_timestamp(&text_with_preview));

        let mut text = text_with_preview;
        remove_message_content_web_page(&mut text);
        match &text {
            MessageContent::Text(m) => {
                assert_eq!(m.web_page_id, WebPageId::default());
                assert!(!m.force_small_media);
                assert!(!m.force_large_media);
                assert!(!m.skip_web_page_confirmation);
                assert!(m.web_page_url.is_empty());
            }
            _ => unreachable!(),
        }

        let audio = MessageContent::Audio(MessageAudio {
            file_id: FileId::new(1, 0),
            caption: FormattedText::default(),
        });
        assert!(can_message_content_have_media_timestamp(&audio));
        assert!(!can_message_content_have_media_timestamp(&simple_text(
            "hi"
        )));

        let background_info = BackgroundInfo {
            background_id: BackgroundId::new(3),
            background_type: BackgroundType::default(),
        };
        let set_background = MessageContent::SetBackground(MessageSetBackground {
            old_message_id: MessageId::default(),
            background_info: background_info.clone(),
            for_both: false,
        });
        assert_eq!(
            get_message_content_my_background_info(&set_background, true),
            background_info
        );
        assert_eq!(
            get_message_content_my_background_info(&set_background, false),
            BackgroundInfo::default()
        );
        let for_both = MessageContent::SetBackground(MessageSetBackground {
            old_message_id: MessageId::default(),
            background_info: background_info.clone(),
            for_both: true,
        });
        assert_eq!(
            get_message_content_my_background_info(&for_both, false),
            background_info
        );
    }

    /// The value helpers the accessors consume: Game/GiveawayParameters/
    /// InputInvoice/ToDoList.
    #[test]
    fn value_helpers_match_the_cpp() {
        let user_id = UserId::new(7);
        let game = Game {
            bot_user_id: user_id,
            text: with_url_text(),
            ..Default::default()
        };
        assert_eq!(game.get_bot_user_id(), user_id);
        assert_eq!(game.get_text().text, "https://example.com");

        let parameters = GiveawayParameters {
            boosted_channel_id: ChannelId::new(2),
            additional_channel_ids: vec![ChannelId::new(4), ChannelId::new(3)],
            ..Default::default()
        };
        assert_eq!(
            parameters.get_channel_ids(),
            vec![ChannelId::new(4), ChannelId::new(3), ChannelId::new(2)]
        );

        let mut invoice = InputInvoice::default();
        invoice.extended_media.r#type = MessageExtendedMediaType::Video;
        assert!(invoice.has_media_timestamp());
        invoice.extended_media.r#type = MessageExtendedMediaType::Photo;
        assert!(!invoice.has_media_timestamp());
        invoice.extended_media_caption = with_url_text();
        assert_eq!(invoice.get_caption().text, "https://example.com");

        let list = ToDoList {
            title: FormattedText::default(),
            items: vec![ToDoItem::default(); 3],
            others_can_append: false,
            others_can_complete: true,
        };
        assert!(list.get_can_append_items(100, 97));
        assert!(!list.get_can_append_items(100, 98));
        assert!(list.get_can_append_items(3, 0));
        assert!(!list.get_can_append_items(3, 1));
        assert!(!list.get_can_append_items(100, -1));
    }
}
