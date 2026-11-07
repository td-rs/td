//! Port of `td/telegram/InlineQueriesManager.{h,cpp}` (M.21) plus the
//! model pieces that only serve this manager: `WebAppOpenParameters.{h,cpp}`
//! (the theme-parameter JSON half is the Wave-3 ThemeManager seam) and the
//! `Venue` record slice
//! (`Venue.{h,cpp}`; the record lifts to its own module at the M.9
//! message-content integration). The `Location.cpp` input processors
//! (`process_input_message_location`/`process_live_location`) live with
//! the `Location` record in `chat_manager.rs` — see this module's re-use
//! below. `TargetDialogTypes.{h,cpp}` rides the canonical
//! `target_dialog_types.rs` module (M.22 unification).
//!
//! The manager is the collapsed actor: `Td` owns the struct, the
//! [`InlineQueriesManagerEnv`] trait carries the `G()`/option/pmc/
//! cross-manager surfaces, and the `Td::ResultHandler` query families
//! (InlineQueriesManager.cpp:67-455) ride the `InlineQueriesNetKind`
//! plane over the manager-held net handle (net-drain 48): the send legs
//! run inside the manager, the request promises park in the family lots
//! (the handler `promise_` members) and the answers drain through
//! `Td::on_inline_queries_net_answer` into the intakes
//! below. The seams that stand in for unlanded planes:
//!
//! - `process_input_message_text` — ported here (the function is
//!   self-contained over the landed MessageEntity plane); it lifts to the
//!   M.9 InputMessageText module at that integration.
//! - `get_input_reply_markup` / `get_input_keyboard_button` — the M.22
//!   reply-markup plane (landed: `reply_markup.rs`/`keyboard_button.rs`);
//!   the intakes ride the env ([`InlineQueriesManagerEnv::get_input_reply_markup`]).
//! - `RichMessage`/`InputInvoice` conversions — `[blocked: M.17]` /
//!   Wave-3 payments; both park the request at the seam (`Ok(None)`).
//! - `create_inline_message_content` — `[blocked: M.9]`: the real record
//!   is the MessageContent plane, so `register_inline_message_content`
//!   answers the C++ "content == nullptr" band and the wire intake drops
//!   such results until the integration.
//! - `register_thumbnail` (`get_web_document_photo_size` +
//!   `get_thumbnail_object`) — `[blocked: Files]`: the temp-file
//!   registration and the file-object builders ride FileManager (M.12
//!   integration).
//! - the media result intakes (`documents_manager_->on_get_document`, the
//!   animation/audio/document/sticker/video/voice object builders,
//!   `Game`) — `[blocked: M.28/M.20/M.15]`; their arms keep the C++
//!   "drop the result" bands.
//! - `on_dialog_used` — `[blocked: M.27]` (TopDialogManager).
//!
//! The `query_hash` keys of the C++ cache come from `std::hash<string>`
//! (implementation-defined); the port pins a stable FNV-1a fold instead —
//! the hash is an in-memory cache key only, it is never persisted, so the
//! divergence is internal-only (documented divergence #M.21-hash).
//!
//! The C++ deep-copy templates (`copy(const td_api::file &)`,
//! `copy_result`, ...) exist because C++ `tl_object_ptr`s are not
//! copyable; the generated Rust objects derive `Clone`, so the
//! `get_inline_query_results_object` cache hand-out is `.clone()`
//! (documented divergence #M.21-copy).

use crate::chat_manager::{ChannelId, ChannelType, Location};
use crate::contact::Contact;
use crate::dialog_id::{DialogId, DialogType};
use crate::files::FileId;
use crate::link_manager;
use crate::message_entity::{FormattedText, get_first_url};
use crate::misc::{add_to_top, clean_input_string, is_base64url_characters, to_integer_i64};
use crate::requested_dialog_type::RequestedDialogType;
use crate::target_dialog_types::TargetDialogTypes;
use crate::td_api;
use crate::user_manager::{BotData, UserId};
use std::collections::HashMap;
use tdmtproto::telegram_api;
use tdutils::promise::Promise;
use tdutils::status::Status;

/// `MAX_RECENT_INLINE_BOTS` (InlineQueriesManager.h:112) — some reasonable
/// value.
pub(crate) const MAX_RECENT_INLINE_BOTS: usize = 20;

/// `INLINE_QUERY_DELAY_MS` (InlineQueriesManager.h:113) — the server-side
/// limit between two inline queries, in milliseconds.
pub(crate) const INLINE_QUERY_DELAY_MS: i64 = 400;

/// `MAX_INLINE_QUERY_RESULT_COUNT` (InlineQueriesManager.h:114) — the
/// server-side limit.
pub(crate) const MAX_INLINE_QUERY_RESULT_COUNT: usize = 50;

// ---------------------------------------------------------------------------
// WebAppOpenParameters.{h,cpp}
// ---------------------------------------------------------------------------

/// `td::WebAppOpenParameters` (WebAppOpenParameters.h:20-40) — the value
/// plane; `get_input_theme_parameters` rides the
/// `[blocked: Wave-3 ThemeManager]` seam through the env
/// ([`InlineQueriesManagerEnv::get_theme_parameters_json_string`]).
#[derive(Clone, Debug, Default)]
pub struct WebAppOpenParameters {
    theme_parameters: Option<td_api::ThemeParameters>,
    application_name: String,
    is_compact: bool,
    is_full_screen: bool,
}

impl WebAppOpenParameters {
    /// `WebAppOpenParameters(td_api::object_ptr<td_api::webAppOpenParameters>)`
    /// (WebAppOpenParameters.cpp:15-42).
    pub fn from_td_api(parameters: Option<td_api::WebAppOpenParameters>) -> WebAppOpenParameters {
        let mut result = WebAppOpenParameters::default();
        if let Some(parameters) = parameters {
            result.theme_parameters = parameters.theme;
            result.application_name = parameters.application_name;
            if clean_input_string(&result.application_name).is_none() {
                result.application_name.clear();
            }
            if let Some(mode) = parameters.mode {
                match mode {
                    td_api::WebAppOpenMode::webAppOpenModeCompact(_) => result.is_compact = true,
                    td_api::WebAppOpenMode::webAppOpenModeFullSize(_) => {}
                    td_api::WebAppOpenMode::webAppOpenModeFullScreen(_) => {
                        result.is_full_screen = true
                    }
                }
            }
        }
        result
    }

    /// `get_application_name` (WebAppOpenParameters.h:33-35).
    pub fn get_application_name(&self) -> &str {
        &self.application_name
    }

    /// `is_compact` (WebAppOpenParameters.h:37-39).
    pub fn is_compact(&self) -> bool {
        self.is_compact
    }

    /// `is_full_screen` (WebAppOpenParameters.h:40).
    pub fn is_full_screen(&self) -> bool {
        self.is_full_screen
    }
}

// ---------------------------------------------------------------------------
// the Venue record slice (Venue.{h,cpp})
// ---------------------------------------------------------------------------

/// `td::Venue` (Venue.h:21-58) — the record slice this manager reads; the
/// full record (store/parse, the input-media halves) lifts to its own
/// module at the M.9 integration.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Venue {
    location: Location,
    title: String,
    address: String,
    provider: String,
    id: String,
    r#type: String,
}

impl Venue {
    /// `Venue(location, title, address, provider, id, type)`
    /// (Venue.cpp:22-30).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        location: Location,
        title: String,
        address: String,
        provider: String,
        id: String,
        r#type: String,
    ) -> Venue {
        Venue {
            location,
            title,
            address,
            provider,
            id,
            r#type,
        }
    }

    /// `Venue(const tl_object_ptr<td_api::venue> &)` (Venue.cpp:32-41).
    pub fn from_td_api(venue: &td_api::Venue) -> Venue {
        Venue {
            location: location_from_td_api(venue.location.as_ref()),
            title: venue.title.clone(),
            address: venue.address.clone(),
            provider: venue.provider.clone(),
            id: venue.id.clone(),
            r#type: venue.r#type.clone(),
        }
    }

    /// `empty` (Venue.cpp:43-45) — an empty location empties the venue.
    pub fn empty(&self) -> bool {
        self.location.is_empty()
    }

    /// `get_venue_object` (Venue.cpp:54-58).
    pub fn get_venue_object(&self) -> td_api::Venue {
        td_api::Venue {
            location: self.location.get_location_object(),
            title: self.title.clone(),
            address: self.address.clone(),
            provider: self.provider.clone(),
            id: self.id.clone(),
            r#type: self.r#type.clone(),
        }
    }

    /// `get_input_bot_inline_message_media_venue` (Venue.cpp:66-74).
    fn get_input_bot_inline_message_media_venue(
        &self,
        reply_markup: Option<telegram_api::ReplyMarkup>,
    ) -> telegram_api::inputBotInlineMessageMediaVenue {
        let mut flags = 0;
        if reply_markup.is_some() {
            flags |= MASK_REPLY_MARKUP;
        }
        telegram_api::inputBotInlineMessageMediaVenue {
            flags,
            geo_point: Some(self.location.get_input_geo_point()),
            title: self.title.clone(),
            address: self.address.clone(),
            provider: self.provider.clone(),
            venue_id: self.id.clone(),
            venue_type: self.r#type.clone(),
            reply_markup,
        }
    }
}

/// `process_input_message_venue` (Venue.cpp:100-135) — the td_api
/// input-message-content validation (the `inputMessageVenue` fold; the
/// caller dispatches the constructor ids like the C++ switch).
pub(crate) fn process_input_message_venue(venue: Option<td_api::Venue>) -> Result<Venue, Status> {
    let Some(venue) = venue else {
        return Err(Status::error(400, "Venue must be non-empty"));
    };
    if clean_input_string(&venue.title).is_none() {
        return Err(Status::error(400, "Venue title must be encoded in UTF-8"));
    }
    if clean_input_string(&venue.address).is_none() {
        return Err(Status::error(400, "Venue address must be encoded in UTF-8"));
    }
    if clean_input_string(&venue.provider).is_none() {
        return Err(Status::error(
            400,
            "Venue provider must be encoded in UTF-8",
        ));
    }
    if clean_input_string(&venue.id).is_none() {
        return Err(Status::error(
            400,
            "Venue identifier must be encoded in UTF-8",
        ));
    }
    if clean_input_string(&venue.r#type).is_none() {
        return Err(Status::error(400, "Venue type must be encoded in UTF-8"));
    }
    let result = Venue::from_td_api(&venue);
    if result.empty() {
        return Err(Status::error(400, "Wrong venue location specified"));
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// the Location.cpp input processors (the Location record landed in M.5)
// ---------------------------------------------------------------------------

/// `td::InputMessageLocation` (Location.h:113-140).
#[derive(Clone, Copy, Debug, Default)]
pub struct InputMessageLocation {
    pub location: Location,
    pub live_period: i32,
    pub heading: i32,
    pub proximity_alert_radius: i32,
}

impl InputMessageLocation {
    /// `InputMessageLocation(location, live_period, heading,
    /// proximity_alert_radius)` (Location.h:116-124).
    fn new(
        location: Location,
        live_period: i32,
        heading: i32,
        proximity_alert_radius: i32,
    ) -> Self {
        InputMessageLocation {
            location,
            live_period,
            heading,
            proximity_alert_radius,
        }
    }

    /// `get_input_bot_inline_message_media_geo` (Location.cpp:105-113).
    fn get_input_bot_inline_message_media_geo(
        &self,
        reply_markup: Option<telegram_api::ReplyMarkup>,
    ) -> telegram_api::inputBotInlineMessageMediaGeo {
        let mut flags = 0;
        if reply_markup.is_some() {
            flags |= MASK_REPLY_MARKUP;
        }
        if self.heading != 0 {
            flags |= 1; // HEADING_MASK (flags.0)
        }
        if self.live_period != 0 {
            flags |= 1 << 1; // PERIOD_MASK (flags.1)
            flags |= 1 << 3; // PROXIMITY_NOTIFICATION_RADIUS_MASK (flags.3)
        }
        telegram_api::inputBotInlineMessageMediaGeo {
            flags,
            geo_point: Some(self.location.get_input_geo_point()),
            heading: self.heading,
            period: self.live_period,
            proximity_notification_radius: self.proximity_alert_radius,
            reply_markup,
        }
    }
}

/// `Location(const tl_object_ptr<td_api::location> &)` (Location.cpp:67-74)
/// — the td_api constructor over `Location::init`.
pub(crate) fn location_from_td_api(location: Option<&td_api::Location>) -> Location {
    let Some(location) = location else {
        return Location::default();
    };
    Location::from_geo_point(telegram_api::GeoPoint::geoPoint(telegram_api::geoPoint {
        flags: 0,
        lat: location.latitude,
        long: location.longitude,
        accuracy_radius: location.horizontal_accuracy as i32,
        access_hash: 0,
    }))
}

/// `process_live_location` (Location.cpp:179-216) — the `for_edit` band is
/// only reached by the edit paths (the InlineQueriesManager passes false).
pub(crate) fn process_live_location(
    live_location: Option<td_api::LiveLocation>,
    for_edit: bool,
) -> std::result::Result<InputMessageLocation, Status> {
    const MIN_LIVE_LOCATION_PERIOD: i32 = 60; // seconds, server-side limit
    const MAX_LIVE_LOCATION_PERIOD: i32 = 86400; // seconds, server-side limit
    const MIN_LIVE_LOCATION_HEADING: i32 = 1; // degrees, server-side limit
    const MAX_LIVE_LOCATION_HEADING: i32 = 360; // degrees, server-side limit
    const MAX_PROXIMITY_ALERT_RADIUS: i32 = 100000; // meters, server-side limit

    let Some(live_location) = live_location else {
        if for_edit {
            return Ok(InputMessageLocation::new(Location::default(), 0, 0, 0));
        }
        return Err(Status::error(400, "Live location must be non-empty"));
    };
    let location = location_from_td_api(live_location.location.as_ref());
    if location.is_empty() {
        return Err(Status::error(400, "Invalid live location specified"));
    }

    let period = live_location.live_period;
    if !for_edit
        && period != i32::MAX
        && (!(MIN_LIVE_LOCATION_PERIOD..=MAX_LIVE_LOCATION_PERIOD).contains(&period))
    {
        return Err(Status::error(400, "Wrong live location period specified"));
    }

    let heading = live_location.heading;
    if heading != 0 && (!(MIN_LIVE_LOCATION_HEADING..=MAX_LIVE_LOCATION_HEADING).contains(&heading))
    {
        return Err(Status::error(400, "Wrong live location heading specified"));
    }

    let proximity_alert_radius = live_location.proximity_alert_radius;
    if !(0..=MAX_PROXIMITY_ALERT_RADIUS).contains(&proximity_alert_radius) {
        return Err(Status::error(
            400,
            "Wrong live location proximity alert radius specified",
        ));
    }

    Ok(InputMessageLocation::new(
        location,
        period,
        heading,
        proximity_alert_radius,
    ))
}

/// `process_input_message_location` (Location.cpp:158-177).
pub(crate) fn process_input_message_location(
    input_message_content: &td_api::InputMessageContent,
) -> Result<InputMessageLocation, Status> {
    match input_message_content {
        td_api::InputMessageContent::inputMessageLocation(input_location) => {
            let location = location_from_td_api(input_location.location.as_ref());
            if location.is_empty() {
                return Err(Status::error(400, "Invalid location specified"));
            }
            Ok(InputMessageLocation::new(location, 0, 0, 0))
        }
        td_api::InputMessageContent::inputMessageLiveLocation(input_live_location) => {
            process_live_location(input_live_location.location.clone(), false)
        }
        // the C++ CHECKs the constructor id before the call; the enum
        // dispatch makes the bad arm unrepresentable
        _ => unreachable!("process_input_message_location: wrong content type"),
    }
}

/// `process_input_message_contact` (Contact.cpp:166-176) — the
/// `get_contact` fold (Contact.cpp:128-142) with the
/// `have_user_force(user_id)` probe as an env band.
pub(crate) fn process_input_message_contact(
    env: &mut impl InlineQueriesManagerEnv,
    input_message_content: &td_api::InputMessageContent,
) -> Result<Contact, Status> {
    let td_api::InputMessageContent::inputMessageContact(input) = input_message_content else {
        unreachable!("process_input_message_contact: wrong content type");
    };
    let Some(contact) = input.contact.as_ref() else {
        return Err(Status::error(400, "Contact must be non-empty"));
    };
    let user_id = UserId::new(contact.user_id);
    if user_id.is_valid() && !env.have_user_force(user_id) {
        return Err(Status::error(400, "User not found"));
    }
    let mut result = Contact::new(
        contact.phone_number.clone(),
        contact.first_name.clone(),
        contact.last_name.clone(),
        contact.vcard.clone(),
        user_id,
    );
    result.validate()?;
    Ok(result)
}

// ---------------------------------------------------------------------------
// the InputMessageText record + process_input_message_text
// (InputMessageText.{h,cpp}; lifts to the M.9 module)
// ---------------------------------------------------------------------------

/// `td::InputMessageText` (InputMessageText.h:20-42).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InputMessageText {
    pub text: FormattedText,
    pub web_page_url: String,
    pub disable_web_page_preview: bool,
    pub force_small_media: bool,
    pub force_large_media: bool,
    pub show_above_text: bool,
    pub clear_draft: bool,
}

impl InputMessageText {
    /// `InputMessageText::get_input_media_web_page` (InputMessageText.cpp
    /// 61-68) — the edit-path helper; `None` is the C++ nullptr.
    pub fn get_input_media_web_page(&self) -> Option<telegram_api::inputMediaWebPage> {
        if self.web_page_url.is_empty() {
            return None;
        }
        let is_optional = self.text.text.is_empty();
        Some(telegram_api::inputMediaWebPage {
            flags: 0,
            force_large_media: self.force_large_media,
            force_small_media: self.force_small_media,
            optional: is_optional,
            url: self.web_page_url.clone(),
        })
    }
}

/// `process_input_message_text` (InputMessageText.cpp:39-83) — the
/// `inputMessageText` fold (the caller dispatches the constructor ids).
pub(crate) fn process_input_message_text(
    env: &mut impl InlineQueriesManagerEnv,
    dialog_id: DialogId,
    input_message_content: &td_api::InputMessageContent,
    is_bot: bool,
    for_draft: bool,
) -> Result<InputMessageText, Status> {
    let td_api::InputMessageContent::inputMessageText(input_message_text) = input_message_content
    else {
        unreachable!("process_input_message_text: wrong content type");
    };
    let mut web_page_url = String::new();
    let mut disable_web_page_preview = false;
    let mut force_small_media = false;
    let mut force_large_media = false;
    let mut show_above_text = false;
    if let Some(options) = input_message_text.link_preview_options.as_ref() {
        web_page_url = options.url.clone();
        disable_web_page_preview = options.is_disabled;
        force_small_media = options.force_small_media;
        force_large_media = options.force_large_media;
        show_above_text = options.show_above_text;

        if clean_input_string(&web_page_url).is_none() {
            return Err(Status::error(
                400,
                "Link preview URL must be encoded in UTF-8",
            ));
        }

        if disable_web_page_preview || env.can_add_web_page_previews(dialog_id) {
            web_page_url.clear();
        }
        if web_page_url.is_empty() {
            force_small_media = false;
            force_large_media = false;
        }
    }
    let text = env.parse_formatted_text(
        dialog_id,
        input_message_text.text.clone(),
        is_bot,
        for_draft || !web_page_url.is_empty(),
        for_draft,
        for_draft,
    )?;

    if !disable_web_page_preview
        && web_page_url.is_empty()
        && dialog_id.get_type() == DialogType::SecretChat
    {
        web_page_url = get_first_url(&text).to_owned();
    }
    Ok(InputMessageText {
        text,
        web_page_url,
        disable_web_page_preview,
        force_small_media,
        force_large_media,
        show_above_text,
        clear_draft: input_message_text.clear_draft,
    })
}

// ---------------------------------------------------------------------------
// the inline message id encode plane
// ---------------------------------------------------------------------------

/// `InlineQueriesManager::get_inline_message_id` (InlineQueriesManager.cpp
/// 534-542) — `base64url_encode(serialize(*input_bot_inline_message_id))`:
/// the bare TL store (fields, no constructor id — the C++ parse side
/// dispatches on the 20-vs-24-byte length), URL-safe base64 without
/// padding. The decode pair lives in `inline_message_manager.rs`, mirroring
/// the C++ file split.
pub fn get_inline_message_id(
    input_bot_inline_message_id: Option<telegram_api::InputBotInlineMessageID>,
) -> String {
    let Some(input_bot_inline_message_id) = input_bot_inline_message_id else {
        return String::new();
    };
    let mut binary = Vec::new();
    match input_bot_inline_message_id {
        telegram_api::InputBotInlineMessageID::inputBotInlineMessageID(id) => id.store(&mut binary),
        telegram_api::InputBotInlineMessageID::inputBotInlineMessageID64(id) => {
            id.store(&mut binary)
        }
    }
    String::from_utf8(tdmtproto::utils::base64url_encode(&binary)).expect("base64url is ascii")
}

// ---------------------------------------------------------------------------
// the query-hash fold (documented divergence #M.21-hash)
// ---------------------------------------------------------------------------

/// The `Hash<string>()` stand-in: FNV-1a 32-bit. The C++ uses
/// `std::hash<std::string>` (implementation-defined); the fold below is
/// stable across builds. Internal-only key — never persisted.
fn hash_string(value: &str) -> u64 {
    let mut hash: u32 = 0xcbf2_9ce4;
    for byte in value.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    u64::from(hash)
}

/// The `query_hash * 2023654985u + value` fold step of
/// `send_inline_query`/`get_prepared_inline_message` (wrapping, like the
/// C++ `uint64` arithmetic).
fn fold_query_hash(query_hash: u64, value: u64) -> u64 {
    query_hash.wrapping_mul(2023654985).wrapping_add(value)
}

/// The shared `query_hash &= 0x7FFFFFFFFFFFFFFF; if (query_hash == 0)
/// query_hash = 1;` tail.
fn finish_query_hash(query_hash: u64) -> u64 {
    let query_hash = query_hash & 0x7FFF_FFFF_FFFF_FFFF;
    if query_hash == 0 { 1 } else { query_hash }
}

// ---------------------------------------------------------------------------
// flag masks (the generated code carries raw flags, not mask constants)
// ---------------------------------------------------------------------------

/// `inputBotInlineMessage*::REPLY_MARKUP_MASK` (flags.2).
const MASK_REPLY_MARKUP: i32 = 1 << 2;
/// `inputBotInlineMessageMediaAuto/Text::ENTITIES_MASK` (flags.1).
const MASK_ENTITIES: i32 = 1 << 1;
/// `inputBotInlineMessageMediaAuto/Text::INVERT_MEDIA_MASK` (flags.3).
/// The web-app result ride-along flags; consumed by the M.31 web-app
/// result plane ([blocked: Wave 3] seam here).
#[allow(dead_code)]
const MASK_INVERT_MEDIA: i32 = 1 << 3;
/// `inputBotInlineMessageMediaWebPage` force flags (flags.4/flags.5) and
/// the optional flag (flags.6) are built from the record fields.
/// `inputBotInlineResult::TITLE_MASK` (flags.1).
const MASK_TITLE: i32 = 1 << 1;
/// `inputBotInlineResult::DESCRIPTION_MASK` (flags.2).
const MASK_DESCRIPTION: i32 = 1 << 2;
/// `inputBotInlineResult::URL_MASK` (flags.3).
const MASK_URL: i32 = 1 << 3;
/// `inputBotInlineResult::THUMB_MASK` (flags.4).
const MASK_THUMB: i32 = 1 << 4;
/// `inputBotInlineResult::CONTENT_MASK` (flags.5).
const MASK_CONTENT: i32 = 1 << 5;

// ---------------------------------------------------------------------------
// the env trait
// ---------------------------------------------------------------------------

/// The `Td *td` slice of the collapsed InlineQueriesManager.
pub trait InlineQueriesManagerEnv {
    // -- the G()/auth slice
    fn auth_is_bot(&self) -> bool;
    fn close_flag(&self) -> bool;
    fn use_chat_info_database(&self) -> bool;
    fn is_test_dc(&self) -> bool;
    fn unix_time(&self) -> i32;

    // -- options
    fn get_option_string(&self, name: &str, default_value: &str) -> String;

    // -- the pmc slice: `G()->td_db()->get_binlog_pmc()->get/set` (the
    // -- recently-used-bots records are C++ strings)
    fn binlog_pmc_get(&mut self, key: &str) -> String;
    fn binlog_pmc_set(&mut self, key: &str, value: &str);

    // -- the update plane
    fn send_update(&mut self, update: td_api::Update);

    // -- the user-manager surface
    fn get_bot_data(&self, user_id: UserId) -> Result<BotData, Status>;
    fn have_min_user(&self, user_id: UserId) -> bool;
    /// `UserManager::have_user_force(user_id, source)` — the landed
    /// in-memory band; the forced net fetch is the M.4 seam.
    fn have_user_force(&self, user_id: UserId) -> bool;
    fn get_user_first_username(&self, user_id: UserId) -> String;
    fn get_user_id_object(&mut self, user_id: UserId, source: &str) -> i64;
    fn get_input_user(&mut self, user_id: UserId) -> Result<telegram_api::InputUser, Status>;
    fn on_get_users(&mut self, users: Vec<telegram_api::User>, source: &str);

    // -- the dialog-plane bands (M.6/M.10 integration)
    /// `dialog_manager_->get_input_peer(dialog_id, AccessRights::Read)` —
    /// `None` is the C++ nullptr (the caller substitutes inputPeerEmpty).
    fn get_input_peer(&mut self, dialog_id: DialogId) -> Option<telegram_api::InputPeer>;
    /// `chat_manager_->get_channel_type(channel_id)`.
    fn get_channel_type(&self, channel_id: ChannelId) -> ChannelType;
    /// `chat_manager_->is_broadcast_channel(channel_id)`.
    fn is_broadcast_channel(&self, channel_id: ChannelId) -> bool;
    /// `dialog_manager_->resolve_dialog(username, ChannelId(), lambda)` —
    /// the get_weather resolve chain (`do_get_weather` → send_inline_query
    /// → `on_get_weather` folds inside) — `[blocked: dialog-plane
    /// resolve]`: `DialogManager::resolve_dialog` (cpp:3185-3207) is the
    /// MessagesManager/link step the dialog module still parks (its own
    /// inventory row); the promise lands in the (never-sent) resolve.
    fn resolve_weather_dialog(
        &mut self,
        username: &str,
        location: Location,
        promise: Promise<td_api::CurrentWeather>,
    );
    /// the negated `can_add_web_page_previews` channel-permission probe of
    /// process_input_message_text — the M.5 band answers false.
    fn can_add_web_page_previews(&self, dialog_id: DialogId) -> bool;
    /// `dialog_manager_->get_my_dialog_id()`.
    fn get_my_dialog_id(&self) -> DialogId;

    // -- the FormattedText parse hop (the real get_formatted_text; the
    // -- env threads the Td-owned option flags and the entity probes)
    fn parse_formatted_text(
        &mut self,
        dialog_id: DialogId,
        text: Option<td_api::FormattedText>,
        is_bot: bool,
        allow_empty: bool,
        skip_media_timestamps: bool,
        skip_trim: bool,
    ) -> Result<FormattedText, Status>;

    // -- the MessageEntity conversion hop
    fn get_input_message_entities(
        &mut self,
        entities: &[crate::message_entity::MessageEntity],
        source: &str,
    ) -> Vec<telegram_api::MessageEntity>;

    // -- the remaining cross-plane resolve tails; each one drops the
    // -- promise, which keeps the C++ request unanswered offline

    // -- the M.22 reply-markup plane intakes
    /// `KeyboardButton::get_keyboard_button(button, true)` +
    /// `get_input_keyboard_button` (KeyboardButton.{h,cpp}) — the M.22
    /// plane; validation errors reach the caller's promise (the C++
    /// TRY_RESULT_PROMISE band at InlineQueriesManager.cpp:814).
    fn get_input_keyboard_button(
        &mut self,
        button: &td_api::KeyboardButton,
    ) -> Result<telegram_api::KeyboardButton, Status>;
    /// `get_inline_reply_markup` (ReplyMarkup.cpp:319-322) composed with
    /// the null-safe `get_input_reply_markup` (ReplyMarkup.cpp:495-502) —
    /// the M.22 plane.
    fn get_input_reply_markup(
        &mut self,
        reply_markup: Option<td_api::ReplyMarkup>,
        is_bot: bool,
        switch_inline_buttons_allowed: bool,
    ) -> Result<Option<telegram_api::ReplyMarkup>, Status>;
    /// `RichMessage::get_rich_message` + `get_input_rich_message` —
    /// `[blocked: M.17]`; `Ok(None)` parks the caller's request.
    fn get_input_rich_message(
        &mut self,
        message: &td_api::inputMessageRichMessage,
    ) -> Result<Option<telegram_api::InputRichMessage>, Status>;
    /// `InputInvoice::process_input_message_invoice` +
    /// `get_input_bot_inline_message_media_invoice` — the Wave-3 payments
    /// plane; `Ok(None)` parks the caller's request.
    fn get_input_bot_inline_message_media_invoice(
        &mut self,
        invoice: &td_api::inputMessageInvoice,
        reply_markup: Option<telegram_api::ReplyMarkup>,
    ) -> Result<Option<telegram_api::InputBotInlineMessage>, Status>;
    /// `create_inline_message_content` (MessageContent.{h,cpp}) —
    /// `[blocked: M.9]`; `None` is the C++ "content == nullptr" band.
    fn create_inline_message_content(
        &mut self,
        file_id: FileId,
        inline_message: Option<telegram_api::BotInlineMessage>,
        allowed_media_content_id: i32,
    ) -> Option<InlineMessageContent>;
    /// `get_web_document_photo_size(FileType::Thumbnail, ...)` +
    /// `get_thumbnail_object` (the register_thumbnail pair) —
    /// `[blocked: Files]`.
    fn register_thumbnail(
        &mut self,
        web_document: telegram_api::WebDocument,
    ) -> Option<td_api::Thumbnail>;
    /// `get_web_document_photo_size(FileType::Temp, ...)` — the web-photo
    /// arm's content size — `[blocked: Files]`.
    fn get_web_document_temp_photo_size(
        &mut self,
        web_document: telegram_api::WebDocument,
    ) -> Option<crate::photo::PhotoSize>;
    /// `get_web_document_photo_size(FileType::Thumbnail, ...)` — the
    /// web-photo arm's thumb size — `[blocked: Files]`.
    fn get_web_document_thumbnail_photo_size(
        &mut self,
        web_document: telegram_api::WebDocument,
    ) -> Option<crate::photo::PhotoSize>;
    /// `ThemeManager::get_theme_parameters_json_string` —
    /// `[blocked: Wave-3 ThemeManager]`; `None` drops the theme from the
    /// built query (which never sends offline).
    fn get_theme_parameters_json_string(&self, theme: &td_api::ThemeParameters) -> Option<String>;
    /// `on_dialog_used` (TopDialogManager) — `[blocked: M.27]`.
    fn on_dialog_used(&mut self, category: TopDialogCategory, dialog_id: DialogId, unix_time: i32);
}

/// `TopDialogCategory` (TopDialogCategory.h) — the value slice the
/// `get_simple_web_view_url` probe needs; the manager lands with M.27.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TopDialogCategory {
    User,
    Bot,
    BotApp,
    Channel,
    Group,
}

/// `InlineMessageContent` (MessageContent.h:1476-1494) — the
/// `[blocked: M.9]` stand-in: the real record wraps the MessageContent
/// plane; the manager only needs its existence and the
/// `can_send_message_content_to_secret_chat` verdict, which the seam
/// folds in at the M.9 boundary.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InlineMessageContent {
    /// the folded `can_send_message_content_to_secret_chat(content->get_type())`
    is_secret_chat_allowed: bool,
}

impl InlineMessageContent {
    /// the test/seam constructor.
    pub fn new(is_secret_chat_allowed: bool) -> InlineMessageContent {
        InlineMessageContent {
            is_secret_chat_allowed,
        }
    }
}

// ---------------------------------------------------------------------------
// the manager
// ---------------------------------------------------------------------------

/// The InlineQueriesManager in-flight query kinds — the C++
/// ResultHandler classes of InlineQueriesManager.cpp:67-455 (the net-drain
/// 48 families). The kind carries the handler members the answer needs
/// (the `dialog_id_`/`bot_user_id_`/`query_hash_` re-entry keys); the
/// request promises park in the manager's family lots (the handler
/// `promise_` members, §5's FIFO-per-family shape).
#[derive(Clone, Copy, Debug)]
pub(crate) enum InlineQueriesNetKind {
    /// `GetInlineBotResultsQuery` (cpp:67-121) —
    /// `messages.getInlineBotResults`; the answer routes
    /// [`InlineQueriesManager::on_get_inline_query_results`] with the
    /// handler's re-entry key triple.
    GetInlineBotResults {
        dialog_id: DialogId,
        bot_user_id: UserId,
        query_hash: u64,
    },
    /// `SetInlineBotResultsQuery` (cpp:123-166) —
    /// `messages.setInlineBotResults`; the Bool answer settles the parked
    /// promise ok either way (the `!result` band only logs, cpp:156-159).
    SetInlineBotResults,
    /// `SetBotGuestChatResultQuery` (cpp:168-196) —
    /// `messages.setBotGuestChatResult`; the InputBotInlineMessageID
    /// answer folds through `get_inline_message_id`.
    SetBotGuestChatResult,
    /// `SavePreparedInlineMessageQuery` (cpp:198-231) —
    /// `messages.savePreparedInlineMessage`; the
    /// messages.BotPreparedInlineMessage answer settles the id pair.
    SavePreparedInlineMessage,
    /// `GetPreparedInlineMessageQuery` (cpp:233-267) —
    /// `messages.getPreparedInlineMessage`; the answer routes
    /// [`InlineQueriesManager::on_get_prepared_inline_message`] with the
    /// handler's `bot_user_id_`/`query_hash_` re-entry key pair.
    GetPreparedInlineMessage {
        bot_user_id: UserId,
        query_hash: u64,
    },
    /// `SavePreparedKeyboardButtonQuery` (cpp:269-295) —
    /// `bots.requestWebViewButton`; the bots.RequestedButton answer
    /// settles the prepared button id.
    SavePreparedKeyboardButton,
    /// `GetRequestedWebViewButtonQuery` (cpp:297-337) —
    /// `bots.getRequestedWebViewButton`; the kind carries the handler's
    /// `bot_user_id_` member, the `prepared_button_id_` rides the parked
    /// lot (a String is not `Copy`); the wire KeyboardButton answer folds
    /// through the M.22 record and registers the requested dialog type.
    GetRequestedWebViewButton { bot_user_id: UserId },
    /// `RequestSimpleWebViewQuery` (cpp:339-396) —
    /// `messages.requestSimpleWebView`; the WebViewResult answer settles
    /// the url/same-origin pair (the query_id band only logs).
    RequestSimpleWebView,
    /// `SendWebViewDataQuery` (cpp:398-425) —
    /// `messages.sendWebViewData`; the Updates answer rides the updates
    /// engine with the request promise.
    SendWebViewData,
    /// `SendWebViewResultMessageQuery` (cpp:427-455) —
    /// `messages.sendWebViewResultMessage`; the WebViewMessageSent answer
    /// folds through [`get_inline_message_id`].
    SendWebViewResultMessage,
}

/// The parked family promises (the ResultHandler `promise_` members) —
/// popped FIFO by the answers (`Td::on_inline_queries_net_answer`).
#[derive(Default)]
pub(crate) struct PendingInlineQueries {
    /// `GetInlineBotResultsQuery::promise_`.
    get_inline_bot_results: Vec<Promise<Option<td_api::InlineQueryResults>>>,
    /// `SetInlineBotResultsQuery::promise_`.
    set_inline_bot_results: Vec<Promise<()>>,
    /// `SetBotGuestChatResultQuery::promise_`.
    set_bot_guest_chat_result: Vec<Promise<td_api::InlineMessageId>>,
    /// `SavePreparedInlineMessageQuery::promise_`.
    save_prepared_inline_message: Vec<Promise<td_api::PreparedInlineMessageId>>,
    /// `GetPreparedInlineMessageQuery::promise_`.
    get_prepared_inline_message: Vec<Promise<Option<td_api::PreparedInlineMessage>>>,
    /// `SavePreparedKeyboardButtonQuery::promise_`.
    save_prepared_keyboard_button: Vec<Promise<String>>,
    /// `GetRequestedWebViewButtonQuery::promise_` + the handler's
    /// `prepared_button_id_` member (the answer's re-entry key).
    get_requested_web_view_button: Vec<(String, Promise<td_api::KeyboardButton>)>,
    /// `RequestSimpleWebViewQuery::promise_`.
    request_simple_web_view: Vec<Promise<td_api::WebAppUrl>>,
    /// `SendWebViewDataQuery::promise_`.
    send_web_view_data: Vec<Promise<()>>,
    /// `SendWebViewResultMessageQuery::promise_`.
    send_web_view_result_message: Vec<Promise<td_api::InlineMessageId>>,
}

/// `InlineQueriesManager::PendingInlineQuery` (InlineQueriesManager.h:172-181).
pub(crate) struct PendingInlineQuery {
    query_hash: u64,
    bot_user_id: UserId,
    dialog_id: DialogId,
    input_peer: Option<telegram_api::InputPeer>,
    user_location: Location,
    query: String,
    offset: String,
    promise: Promise<Option<td_api::InlineQueryResults>>,
}

/// `InlineQueriesManager::InlineQueryResult` (InlineQueriesManager.h:187-193).
#[derive(Clone, Debug, Default)]
pub(crate) struct InlineQueryResult {
    results: Option<td_api::InlineQueryResults>,
    cache_expire_time: f64,
    pending_request_count: i32,
    target_dialog_types_mask: i64,
    is_inline_query: bool,
}

/// `InlineQueriesManager` (InlineQueriesManager.h:38-210) — the collapsed
/// actor. The two `MultiTimeout` planes collapse into deadline maps
/// drained by [`InlineQueriesManager::tick_timeouts`].
#[derive(Default)]
pub struct InlineQueriesManager {
    /// 0 - not loaded, 1 - load request was sent, 2 - loaded.
    recently_used_bots_loaded: i32,
    /// the `resolve_recent_inline_bots_multipromise_` sub-query count
    /// still outstanding (each net-parked resolution keeps its unit).
    resolve_recent_inline_bots_pending: usize,
    /// the multipromise's added promises (the parked getRecentInlineBots
    /// request answers).
    pending_recent_inline_bots: Vec<Promise<Vec<UserId>>>,

    recently_used_bot_user_ids: Vec<UserId>,

    pending_inline_query: Option<PendingInlineQuery>,
    next_inline_query_time: f64,

    /// query_hash → result (the `inline_query_results_` map).
    inline_query_results: HashMap<u64, InlineQueryResult>,
    /// the `drop_inline_query_result_timeout_` deadlines (query_hash →
    /// deadline).
    result_deadlines: HashMap<u64, f64>,
    /// the `drop_inline_query_message_timeout_` deadlines (query_id →
    /// deadline, armed 3600s out).
    message_deadlines: HashMap<i64, f64>,

    /// query_id → [result_id → inline_message_content]
    /// (the `inline_message_contents_` map).
    inline_message_contents: HashMap<i64, HashMap<String, InlineMessageContent>>,

    /// query_id → bot (the `query_id_to_bot_user_id_` map).
    query_id_to_bot_user_id: HashMap<i64, UserId>,

    last_requested_web_view_button_bot_user_id: UserId,
    last_requested_web_view_button_prepared_button_id: String,
    last_requested_web_view_button_requested_dialog_type: Option<RequestedDialogType>,

    /// The manager-held dispatch surface of the net-drain 48 families —
    /// `None` keeps the net-inert posture (the pre-init windows and the
    /// net-inert tests; the `ForumTopicManager::net` shape).
    pub(crate) net: Option<crate::td::InlineQueriesNetHandle>,
    /// The parked family promises (the ResultHandler `promise_` members).
    pub(crate) pending: PendingInlineQueries,
}

impl std::fmt::Debug for InlineQueriesManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InlineQueriesManager")
            .field("recently_used_bots_loaded", &self.recently_used_bots_loaded)
            .field(
                "recently_used_bot_user_ids",
                &self
                    .recently_used_bot_user_ids
                    .iter()
                    .map(|user_id| user_id.get())
                    .collect::<Vec<_>>(),
            )
            .field("inline_query_results", &self.inline_query_results.len())
            .field(
                "inline_message_contents",
                &self.inline_message_contents.len(),
            )
            .field(
                "query_id_to_bot_user_id",
                &self.query_id_to_bot_user_id.len(),
            )
            .finish()
    }
}

impl InlineQueriesManager {
    /// `InlineQueriesManager(td, parent)` (InlineQueriesManager.cpp:457-465)
    /// — the timeout callback wiring collapses into the deadline maps;
    /// `next_inline_query_time_ = Time::now()`.
    pub fn new() -> InlineQueriesManager {
        InlineQueriesManager {
            next_inline_query_time: tdutils::time::now(),
            ..Default::default()
        }
    }

    /// `after_get_difference` (InlineQueriesManager.cpp:524-532).
    pub fn after_get_difference(&mut self, env: &mut impl InlineQueriesManagerEnv) {
        if env.auth_is_bot() {
            return;
        }
        if self.recently_used_bots_loaded < 2 {
            self.load_recently_used_bots(env);
        }
    }

    /// `tick_timeouts` — the collapsed
    /// `on_drop_inline_query_result_timeout`/`on_drop_inline_query_message_timeout`
    /// pair plus the pending-query `loop()` timer. The C++ callbacks check
    /// `G()->close_flag()` and re-dispatch via `send_closure_later`; the
    /// collapse runs the bodies inline.
    pub fn tick_timeouts(&mut self, env: &mut impl InlineQueriesManagerEnv) {
        if env.close_flag() {
            return;
        }
        let now = tdutils::time::now();

        // the pending inline query send timer (`set_timeout_at(
        // next_inline_query_time_)` → loop())
        if self.pending_inline_query.is_some() && now >= self.next_inline_query_time {
            self.process_pending_inline_query(env, now);
        }

        // drop_inline_query_result_timeout_
        let mut message_drops = Vec::new();
        let due: Vec<u64> = self
            .result_deadlines
            .iter()
            .filter(|(_, deadline)| now >= **deadline)
            .map(|(hash, _)| *hash)
            .collect();
        for query_hash in due {
            self.on_drop_inline_query_result_timeout(query_hash, now, &mut message_drops);
        }
        for query_id in message_drops {
            // `drop_inline_query_message_timeout_.set_timeout_in(query_id, 3600)`
            self.message_deadlines.insert(query_id, now + 3600.0);
        }

        // drop_inline_query_message_timeout_
        let due: Vec<i64> = self
            .message_deadlines
            .iter()
            .filter(|(_, deadline)| now >= **deadline)
            .map(|(query_id, _)| *query_id)
            .collect();
        for query_id in due {
            self.on_drop_inline_query_message_timeout(query_id);
        }
    }

    /// `next_deadline` — the earliest armed timer (the pending-query send
    /// time, the result drops and the message drops).
    pub fn next_deadline(&self) -> Option<f64> {
        let mut deadlines = Vec::new();
        if self.pending_inline_query.is_some() {
            deadlines.push(self.next_inline_query_time);
        }
        deadlines.extend(self.result_deadlines.values().copied());
        deadlines.extend(self.message_deadlines.values().copied());
        deadlines.iter().copied().reduce(f64::min)
    }

    /// `on_drop_inline_query_result_timeout` (InlineQueriesManager.cpp:482-502);
    /// the follow-up message-drop arming is returned to the tick loop (the
    /// C++ arms `drop_inline_query_message_timeout_` directly).
    fn on_drop_inline_query_result_timeout(
        &mut self,
        query_hash: u64,
        now: f64,
        message_drops: &mut Vec<i64>,
    ) {
        let Some(entry) = self.inline_query_results.get_mut(&query_hash) else {
            // it has already been deleted
            return;
        };
        debug_assert!(entry.pending_request_count >= 0);
        if entry.pending_request_count == 0 {
            if let Some(results) = &entry.results {
                let query_id = results.inline_query_id;
                if query_id != 0 {
                    message_drops.push(query_id);
                }
            }
            self.inline_query_results.remove(&query_hash);
            self.result_deadlines.remove(&query_hash);
        }
        let _ = now;
    }

    /// `on_drop_inline_query_message_timeout` (InlineQueriesManager.cpp:515-522).
    fn on_drop_inline_query_message_timeout(&mut self, query_id: i64) {
        self.inline_message_contents.remove(&query_id);
        self.query_id_to_bot_user_id.remove(&query_id);
        self.message_deadlines.remove(&query_id);
    }

    /// `get_inline_message_content` (InlineQueriesManager.cpp:671-686).
    pub fn get_inline_message_content(
        &mut self,
        env: &mut impl InlineQueriesManagerEnv,
        query_id: i64,
        result_id: &str,
    ) -> Option<&InlineMessageContent> {
        let bot_user_id = self.get_inline_bot_user_id(query_id);
        let updated = self.update_bot_usage(env, bot_user_id);
        if updated {
            self.save_recently_used_bots(env);
        }
        self.inline_message_contents
            .get(&query_id)
            .and_then(|contents| contents.get(result_id))
    }

    /// `get_inline_bot_user_id` (InlineQueriesManager.cpp:688-694).
    pub fn get_inline_bot_user_id(&self, query_id: i64) -> UserId {
        self.query_id_to_bot_user_id
            .get(&query_id)
            .copied()
            .unwrap_or_default()
    }

    /// `answer_inline_query` (InlineQueriesManager.cpp:696-760). The
    /// `CHECK(td_->auth_manager_->is_bot())` is the request layer's
    /// CHECK_IS_BOT gate.
    #[allow(clippy::too_many_arguments)]
    pub fn answer_inline_query(
        &mut self,
        env: &mut impl InlineQueriesManagerEnv,
        inline_query_id: i64,
        is_personal: bool,
        button: Option<td_api::InlineQueryResultsButton>,
        input_results: Vec<td_api::InputInlineQueryResult>,
        cache_time: i32,
        next_offset: String,
        mut promise: Promise<()>,
    ) {
        debug_assert!(env.auth_is_bot(), "answer_inline_query is bot-only");

        let mut switch_pm = None;
        let mut web_view = None;
        if let Some(mut button) = button {
            if clean_input_string(&button.text).is_none() {
                promise.set_error(Status::error(400, "Strings must be encoded in UTF-8"));
                return;
            }
            let Some(button_type) = button.r#type.take() else {
                promise.set_error(Status::error(400, "Button type must be non-empty"));
                return;
            };
            match button_type {
                td_api::InlineQueryResultsButtonType::inlineQueryResultsButtonTypeStartBot(
                    type_,
                ) => {
                    if type_.parameter.is_empty() {
                        promise.set_error(Status::error(400, "Can't use empty start_parameter"));
                        return;
                    }
                    if type_.parameter.len() > 64 {
                        promise.set_error(Status::error(400, "Too long start_parameter specified"));
                        return;
                    }
                    if !is_base64url_characters(type_.parameter.as_bytes()) {
                        promise.set_error(Status::error(
                            400,
                            "Unallowed characters in start_parameter are used",
                        ));
                        return;
                    }
                    switch_pm = Some(telegram_api::InlineBotSwitchPM {
                        text: button.text,
                        start_param: type_.parameter,
                    });
                }
                td_api::InlineQueryResultsButtonType::inlineQueryResultsButtonTypeWebApp(type_) => {
                    if link_manager::get_link_user_id(&type_.url).is_valid() {
                        promise.set_error(Status::error(
                            400,
                            "Link to a user can't be used in the Web App button",
                        ));
                        return;
                    }
                    if let Err(r_url) =
                        link_manager::check_link(&type_.url, true, !env.is_test_dc())
                    {
                        promise.set_error(Status::error(
                            400,
                            &format!("Inline query button Web App {}", r_url.message()),
                        ));
                        return;
                    }
                    web_view = Some(telegram_api::InlineBotWebView {
                        text: button.text,
                        url: type_.url,
                    });
                }
            }
        }

        if input_results.len() > MAX_INLINE_QUERY_RESULT_COUNT {
            promise.set_error(Status::error(
                400,
                "Too many inline query results specified",
            ));
            return;
        }

        let mut results = Vec::with_capacity(input_results.len());
        let mut is_gallery = false;
        let mut force_vertical = false;
        for input_result in input_results {
            match self.get_input_bot_inline_result(
                env,
                Some(input_result),
                &mut is_gallery,
                &mut force_vertical,
            ) {
                Ok(Some(result)) => results.push(result),
                // the promise lands in the (never-sent) seam query
                Ok(None) => return,
                Err(status) => {
                    promise.set_error(status);
                    return;
                }
            }
        }

        let query = telegram_api::messages_setInlineBotResults {
            flags: if next_offset.is_empty() {
                0
            } else {
                1 << 2 // NEXT_OFFSET_MASK
            } | if switch_pm.is_some() {
                1 << 3 // SWITCH_PM_MASK
            } else {
                0
            } | if web_view.is_some() {
                1 << 4 // SWITCH_WEBVIEW_MASK
            } else {
                0
            },
            gallery: is_gallery && !force_vertical,
            private: is_personal,
            query_id: inline_query_id,
            results,
            cache_time,
            next_offset,
            switch_pm,
            switch_webview: web_view,
        };
        self.send_set_inline_bot_results_query(query, promise);
    }

    /// `answer_guest_query` (InlineQueriesManager.cpp:762-768).
    pub fn answer_guest_query(
        &mut self,
        env: &mut impl InlineQueriesManagerEnv,
        guest_query_id: i64,
        input_result: Option<td_api::InputInlineQueryResult>,
        mut promise: Promise<td_api::InlineMessageId>,
    ) {
        match self.get_input_bot_inline_result(env, input_result, &mut false, &mut false) {
            Ok(Some(result)) => {
                self.send_set_bot_guest_chat_result_query(guest_query_id, result, promise);
            }
            // the promise lands in the (never-sent) seam query
            Ok(None) => {}
            Err(status) => promise.set_error(status),
        }
    }

    /// `save_prepared_inline_message` (InlineQueriesManager.cpp:770-780).
    pub fn save_prepared_inline_message(
        &mut self,
        env: &mut impl InlineQueriesManagerEnv,
        user_id: UserId,
        input_result: Option<td_api::InputInlineQueryResult>,
        chat_types: Option<td_api::TargetChatTypes>,
        mut promise: Promise<td_api::PreparedInlineMessageId>,
    ) {
        let input_user = match env.get_input_user(user_id) {
            Ok(input_user) => input_user,
            Err(status) => {
                promise.set_error(status);
                return;
            }
        };
        let result =
            match self.get_input_bot_inline_result(env, input_result, &mut false, &mut false) {
                Ok(Some(result)) => result,
                Ok(None) => return,
                Err(status) => {
                    promise.set_error(status);
                    return;
                }
            };
        let types = match TargetDialogTypes::get_target_dialog_types(chat_types.as_ref()) {
            Ok(types) => types,
            Err(status) => {
                promise.set_error(status);
                return;
            }
        };

        let peer_types = types.get_input_peer_types();
        let query = telegram_api::messages_savePreparedInlineMessage {
            flags: if peer_types.is_empty() {
                0
            } else {
                1 // PEER_TYPES_MASK (flags.0)
            },
            result: Some(result),
            user_id: Some(input_user),
            peer_types,
        };
        self.send_save_prepared_inline_message_query(query, promise);
    }

    /// `get_prepared_inline_message` (InlineQueriesManager.cpp:782-809).
    pub fn get_prepared_inline_message(
        &mut self,
        env: &mut impl InlineQueriesManagerEnv,
        bot_user_id: UserId,
        prepared_message_id: &str,
        mut promise: Promise<Option<td_api::PreparedInlineMessage>>,
    ) {
        let input_user = match env.get_input_user(bot_user_id) {
            Ok(input_user) => input_user,
            Err(status) => {
                promise.set_error(status);
                return;
            }
        };

        let mut query_hash = hash_string(prepared_message_id);
        query_hash = fold_query_hash(query_hash, bot_user_id.get() as u64);
        let query_hash = finish_query_hash(query_hash);

        if let Some(entry) = self.inline_query_results.get_mut(&query_hash) {
            // `if (it->second.is_inline_query)` (cpp:798-800) — the
            // collision names an *inline-query* entry (the mirror of
            // send_inline_query's band, cpp:1387-1389)
            if entry.is_inline_query {
                promise.set_error(Status::error(500, "Request hash collision"));
                return;
            }
            entry.pending_request_count += 1;
            if tdutils::time::now() < entry.cache_expire_time {
                let object = self.get_prepared_inline_message_object(query_hash);
                promise.set_value(object);
                return;
            }
        } else {
            self.inline_query_results.insert(
                query_hash,
                InlineQueryResult {
                    results: None,
                    cache_expire_time: -1.0,
                    pending_request_count: 1,
                    target_dialog_types_mask: 0,
                    is_inline_query: false,
                },
            );
        }

        let query = telegram_api::messages_getPreparedInlineMessage {
            bot: Some(input_user),
            id: prepared_message_id.to_owned(),
        };
        self.send_get_prepared_inline_message_query(query, bot_user_id, query_hash, promise);
    }

    /// `save_prepared_keyboard_button` (InlineQueriesManager.cpp:811-819).
    /// The button intake rides the M.22 plane
    /// ([`InlineQueriesManagerEnv::get_input_keyboard_button`]); the
    /// validation errors take the C++ TRY_RESULT_PROMISE band.
    pub fn save_prepared_keyboard_button(
        &mut self,
        env: &mut impl InlineQueriesManagerEnv,
        user_id: UserId,
        button: Option<td_api::KeyboardButton>,
        mut promise: Promise<String>,
    ) {
        let input_user = match env.get_input_user(user_id) {
            Ok(input_user) => input_user,
            Err(status) => {
                promise.set_error(status);
                return;
            }
        };
        let Some(button) = button else {
            promise.set_error(Status::error(400, "Button must be non-empty"));
            return;
        };
        match env.get_input_keyboard_button(&button) {
            Ok(input_button) => {
                let query = telegram_api::bots_requestWebViewButton {
                    user_id: Some(input_user),
                    button: Some(input_button),
                };
                self.send_save_prepared_keyboard_button_query(query, promise);
            }
            Err(status) => promise.set_error(status),
        }
    }

    /// `get_prepared_keyboard_button` (InlineQueriesManager.cpp:821-826).
    pub fn get_prepared_keyboard_button(
        &mut self,
        env: &mut impl InlineQueriesManagerEnv,
        bot_user_id: UserId,
        prepared_button_id: &str,
        mut promise: Promise<td_api::KeyboardButton>,
    ) {
        let input_user = match env.get_input_user(bot_user_id) {
            Ok(input_user) => input_user,
            Err(status) => {
                promise.set_error(status);
                return;
            }
        };
        let query = telegram_api::bots_getRequestedWebViewButton {
            bot: Some(input_user),
            webapp_req_id: prepared_button_id.to_owned(),
        };
        self.send_get_requested_web_view_button_query(
            query,
            bot_user_id,
            prepared_button_id.to_owned(),
            promise,
        );
    }

    /// `on_get_requested_web_view_button` (InlineQueriesManager.cpp:828-834)
    /// — the stored record is the C++ `RequestedDialogType` copy
    /// (`make_unique<RequestedDialogType>(*requested_dialog_type)`,
    /// cpp:832).
    pub fn on_get_requested_web_view_button(
        &mut self,
        bot_user_id: UserId,
        prepared_button_id: &str,
        requested_dialog_type: RequestedDialogType,
    ) {
        self.last_requested_web_view_button_bot_user_id = bot_user_id;
        self.last_requested_web_view_button_prepared_button_id = prepared_button_id.to_owned();
        self.last_requested_web_view_button_requested_dialog_type = Some(requested_dialog_type);
    }

    /// `get_requested_dialog_type` (InlineQueriesManager.cpp:836-843).
    pub fn get_requested_dialog_type(
        &self,
        bot_user_id: UserId,
        prepared_button_id: &str,
    ) -> Option<&RequestedDialogType> {
        if bot_user_id == self.last_requested_web_view_button_bot_user_id
            && prepared_button_id == self.last_requested_web_view_button_prepared_button_id
            && !prepared_button_id.is_empty()
        {
            return self
                .last_requested_web_view_button_requested_dialog_type
                .as_ref();
        }
        None
    }

    /// `get_simple_web_view_url` (InlineQueriesManager.cpp:845-854).
    pub fn get_simple_web_view_url(
        &mut self,
        env: &mut impl InlineQueriesManagerEnv,
        bot_user_id: UserId,
        url: &str,
        parameters: WebAppOpenParameters,
        mut promise: Promise<td_api::WebAppUrl>,
    ) {
        let input_user = match env.get_input_user(bot_user_id) {
            Ok(input_user) => input_user,
            Err(status) => {
                promise.set_error(status);
                return;
            }
        };
        if let Err(bot_data) = env.get_bot_data(bot_user_id) {
            promise.set_error(bot_data);
            return;
        }
        let unix_time = env.unix_time();
        env.on_dialog_used(
            TopDialogCategory::BotApp,
            DialogId::new(bot_user_id.get()),
            unix_time,
        );

        // the RequestSimpleWebViewQuery send fold
        // (InlineQueriesManager.cpp:347-379)
        let mut flags = 0;
        let theme_parameters = parameters
            .theme_parameters
            .as_ref()
            .and_then(|theme| env.get_theme_parameters_json_string(theme));
        if theme_parameters.is_some() {
            flags |= 1; // THEME_PARAMS_MASK (flags.0)
        }
        let mut url = url.to_owned();
        let mut start_parameter = String::new();
        let mut from_switch_webview = false;
        let mut from_side_menu = false;
        if let Some(stripped) = url.strip_suffix("#kb") {
            // a URL from keyboard button
            url = stripped.to_owned();
            flags |= 1 << 3; // URL_MASK (flags.3)
        } else if let Some(stripped) = url.strip_suffix("#iq") {
            // a URL from inline query results button
            url = stripped.to_owned();
            from_switch_webview = true;
            flags |= 1 << 3; // URL_MASK (flags.3)
        } else if url.is_empty() {
            from_side_menu = true;
        } else if let Some(start) = url.strip_prefix("start://") {
            start_parameter = start.to_owned();
            url = String::new();

            from_side_menu = true;
            flags |= 1 << 4; // START_PARAM_MASK (flags.4)
        } else {
            promise.set_error(Status::error(400, "Invalid URL specified"));
            return;
        }
        let query = telegram_api::messages_requestSimpleWebView {
            flags,
            from_switch_webview,
            from_side_menu,
            compact: parameters.is_compact(),
            fullscreen: parameters.is_full_screen(),
            bot: Some(input_user),
            url,
            start_param: start_parameter,
            theme_params: theme_parameters.map(|json| telegram_api::DataJSON { data: json }),
            // `parameters.get_application_name()` (cpp:378) — the C++
            // rides the application name on the platform field
            platform: parameters.get_application_name().to_owned(),
        };
        self.send_request_simple_web_view_query(query, promise);
    }

    /// `send_web_view_data` (InlineQueriesManager.cpp:856-869).
    pub fn send_web_view_data(
        &mut self,
        env: &mut impl InlineQueriesManagerEnv,
        bot_user_id: UserId,
        button_text: &str,
        data: &str,
        mut promise: Promise<()>,
    ) {
        if let Err(bot_data) = env.get_bot_data(bot_user_id) {
            promise.set_error(bot_data);
            return;
        }

        let mut random_id = tdutils::random::secure_int64();
        while random_id == 0 {
            random_id = tdutils::random::secure_int64();
        }

        let input_user = match env.get_input_user(bot_user_id) {
            Ok(input_user) => input_user,
            Err(status) => {
                promise.set_error(status);
                return;
            }
        };

        let query = telegram_api::messages_sendWebViewData {
            bot: Some(input_user),
            random_id,
            button_text: button_text.to_owned(),
            data: data.to_owned(),
        };
        self.send_send_web_view_data_query(query, promise);
    }

    /// `answer_web_view_query` (InlineQueriesManager.cpp:871-879).
    pub fn answer_web_view_query(
        &mut self,
        env: &mut impl InlineQueriesManagerEnv,
        web_view_query_id: &str,
        input_result: Option<td_api::InputInlineQueryResult>,
        mut promise: Promise<td_api::InlineMessageId>,
    ) {
        debug_assert!(env.auth_is_bot(), "answer_web_view_query is bot-only");

        match self.get_input_bot_inline_result(env, input_result, &mut false, &mut false) {
            Ok(Some(result)) => {
                let query = telegram_api::messages_sendWebViewResultMessage {
                    bot_query_id: web_view_query_id.to_owned(),
                    result: Some(result),
                };
                self.send_send_web_view_result_message_query(query, promise);
            }
            // the promise lands in the (never-sent) seam query
            Ok(None) => {}
            Err(status) => promise.set_error(status),
        }
    }

    /// `get_weather` (InlineQueriesManager.cpp:1280-1300).
    pub fn get_weather(
        &mut self,
        env: &mut impl InlineQueriesManagerEnv,
        location: Location,
        mut promise: Promise<td_api::CurrentWeather>,
    ) {
        if location.is_empty() {
            promise.set_error(Status::error(400, "Location must be non-empty"));
            return;
        }
        let bot_username = env.get_option_string("weather_bot_username", "");
        if bot_username.is_empty() {
            promise.set_error(Status::error(500, "Not supported"));
            return;
        }
        // `td_->dialog_manager_->resolve_dialog(bot_username, ChannelId(),
        // lambda(do_get_weather))` — the whole resolve chain is the seam;
        // the Td impl parks the promise and, at the net integration,
        // continues into send_inline_query + on_get_weather
        env.resolve_weather_dialog(&bot_username, location, promise);
    }

    // `do_get_weather` (InlineQueriesManager.cpp:1302-1318) is the
    // resolve-callback continuation: it checks the resolved dialog is a
    // user, re-dispatches into `send_inline_query` and folds the results
    // through [`on_get_weather`]. The continuation hops live at the net
    // integration (the resolve seam never completes offline); the
    // type-check + the results fold are [`on_get_weather`]'s.

    /// `send_inline_query` (InlineQueriesManager.cpp:1335-1407).
    #[allow(clippy::too_many_arguments)]
    pub fn send_inline_query(
        &mut self,
        env: &mut impl InlineQueriesManagerEnv,
        bot_user_id: UserId,
        dialog_id: DialogId,
        user_location: Location,
        query: &str,
        offset: &str,
        mut promise: Promise<Option<td_api::InlineQueryResults>>,
    ) {
        debug_assert!(!env.auth_is_bot(), "send_inline_query is user-only");

        let bot_data = match env.get_bot_data(bot_user_id) {
            Ok(bot_data) => bot_data,
            Err(status) => {
                promise.set_error(status);
                return;
            }
        };
        if !bot_data.is_inline {
            promise.set_error(Status::error(400, "Bot doesn't support inline queries"));
            return;
        }

        let input_peer = env.get_input_peer(dialog_id);

        let peer_type: i32 = match input_peer.as_ref() {
            None => 0,
            Some(telegram_api::InputPeer::inputPeerEmpty(_)) => 0,
            Some(telegram_api::InputPeer::inputPeerSelf(_)) => 1,
            Some(telegram_api::InputPeer::inputPeerChat(_)) => 2,
            Some(telegram_api::InputPeer::inputPeerUser(_))
            | Some(telegram_api::InputPeer::inputPeerUserFromMessage(_)) => {
                if dialog_id == DialogId::new(bot_user_id.get()) {
                    3
                } else {
                    4
                }
            }
            Some(telegram_api::InputPeer::inputPeerChannel(_))
            | Some(telegram_api::InputPeer::inputPeerChannelFromMessage(_)) => {
                let channel_type = env.get_channel_type(dialog_id.get_channel_id()) as i32;
                5 + channel_type
            }
        };

        let mut query_hash = hash_string(query.trim());
        query_hash = fold_query_hash(query_hash, bot_user_id.get() as u64);
        query_hash = fold_query_hash(query_hash, peer_type as u64);
        query_hash = fold_query_hash(query_hash, hash_string(offset));
        if bot_data.need_location && !user_location.is_empty() {
            query_hash = fold_query_hash(query_hash, (user_location.get_latitude() * 1e4) as u64);
            query_hash = fold_query_hash(query_hash, (user_location.get_longitude() * 1e4) as u64);
        }
        let query_hash = finish_query_hash(query_hash);

        if let Some(entry) = self.inline_query_results.get_mut(&query_hash) {
            if !entry.is_inline_query {
                promise.set_error(Status::error(500, "Request hash collision"));
                return;
            }
            entry.pending_request_count += 1;
            if tdutils::time::now() < entry.cache_expire_time {
                let object = self.get_inline_query_results_object(query_hash);
                promise.set_value(object);
                return;
            }
        } else {
            self.inline_query_results.insert(
                query_hash,
                InlineQueryResult {
                    results: None,
                    cache_expire_time: -1.0,
                    pending_request_count: 1,
                    target_dialog_types_mask: 0,
                    is_inline_query: true,
                },
            );
        }

        if let Some(pending) = self.pending_inline_query.take() {
            // Drop the pending query: the C++ answers it via
            // on_get_inline_query_results(nullptr, Auto()) — the empty
            // promise is the `Auto()` — cancels the sent query and errors
            // the real one 406; offline there is nothing sent to cancel.
            self.on_get_inline_query_results(
                env,
                pending.dialog_id,
                pending.bot_user_id,
                pending.query_hash,
                None,
                Promise::default(),
            );
            let mut cancelled = pending.promise;
            cancelled.set_error(Status::error(406, "Request canceled"));
        }

        self.pending_inline_query = Some(PendingInlineQuery {
            query_hash,
            bot_user_id,
            dialog_id,
            input_peer,
            user_location,
            query: query.to_owned(),
            offset: offset.to_owned(),
            promise,
        });

        self.process_pending_inline_query(env, tdutils::time::now());
    }

    /// `loop()` (InlineQueriesManager.cpp:1409-1439) — the pending-query
    /// send half; the C++ `cancel_query(sent_query_)` is the net-step's.
    fn process_pending_inline_query(&mut self, env: &mut impl InlineQueriesManagerEnv, now: f64) {
        let Some(pending) = self.pending_inline_query.take() else {
            return;
        };
        if now < self.next_inline_query_time {
            // the C++ arms set_timeout_at(next_inline_query_time_) here;
            // the tick loop re-runs this at the deadline
            self.pending_inline_query = Some(pending);
            return;
        }

        match env.get_input_user(pending.bot_user_id) {
            Ok(bot_input_user) => {
                let query = telegram_api::messages_getInlineBotResults {
                    flags: if pending.user_location.is_empty() {
                        0
                    } else {
                        1 // GEO_POINT_MASK (flags.0)
                    },
                    bot: Some(bot_input_user),
                    peer: pending.input_peer.map(Box::new).or_else(|| {
                        Some(Box::new(telegram_api::InputPeer::inputPeerEmpty(
                            telegram_api::inputPeerEmpty {},
                        )))
                    }),
                    geo_point: if pending.user_location.is_empty() {
                        None
                    } else {
                        Some(pending.user_location.get_input_geo_point())
                    },
                    query: pending.query,
                    offset: pending.offset,
                };
                self.send_get_inline_bot_results_query(
                    query,
                    pending.dialog_id,
                    pending.bot_user_id,
                    pending.query_hash,
                    pending.promise,
                );
                self.next_inline_query_time = now + INLINE_QUERY_DELAY_MS as f64 * 1e-3;
            }
            Err(_) => {
                // the C++ drops the pending query when get_input_user
                // fails (`pending_inline_query_ = nullptr`); the dropped
                // promise answers "Lost promise" like the C++
            }
        }
    }

    /// The `td_->create_handler<GetInlineBotResultsQuery>(promise_)
    /// ->send(...)` collapse (InlineQueriesManager.cpp:76-92) — the request
    /// promise parks in the family lot first (the handler's `promise_`
    /// member), then the wire query dispatches over the manager-held net
    /// handle. `None` on the handle keeps the net-inert posture. The C++
    /// `net_query->need_resend_on_503_ = false` and the returned
    /// NetQueryRef (the pending-query cancel) are dispatcher business the
    /// collapsed plane drops (documented divergence #M.21-netref).
    pub(crate) fn send_get_inline_bot_results_query(
        &mut self,
        query: telegram_api::messages_getInlineBotResults,
        dialog_id: DialogId,
        bot_user_id: UserId,
        query_hash: u64,
        promise: Promise<Option<td_api::InlineQueryResults>>,
    ) {
        self.pending.get_inline_bot_results.push(promise);
        let kind = InlineQueriesNetKind::GetInlineBotResults {
            dialog_id,
            bot_user_id,
            query_hash,
        };
        match self.net.clone() {
            Some(net) => net.dispatch(
                kind,
                telegram_api::Function::messages_getInlineBotResults(query),
            ),
            None => tdutils::log_info!(
                "Send messages.getInlineBotResults for bot {} — [net-inert]",
                bot_user_id.get()
            ),
        }
    }

    /// The get-inline-bot-results family pop for the
    /// `Td::on_inline_queries_net_answer` drain — the FIFO
    /// head the answer pairs with (the C++ promise rides the query).
    pub(crate) fn pop_get_inline_bot_results_promise(
        &mut self,
    ) -> Option<Promise<Option<td_api::InlineQueryResults>>> {
        if self.pending.get_inline_bot_results.is_empty() {
            None
        } else {
            Some(self.pending.get_inline_bot_results.remove(0))
        }
    }

    /// The `td_->create_handler<SetInlineBotResultsQuery>(promise_)
    /// ->send(...)` collapse (InlineQueriesManager.cpp:133-152) — the
    /// request promise parks in the family lot first (the handler's
    /// `promise_` member), then the built wire query dispatches. `None`
    /// on the handle keeps the net-inert posture.
    pub(crate) fn send_set_inline_bot_results_query(
        &mut self,
        query: telegram_api::messages_setInlineBotResults,
        promise: Promise<()>,
    ) {
        self.pending.set_inline_bot_results.push(promise);
        match self.net.clone() {
            Some(net) => net.dispatch(
                InlineQueriesNetKind::SetInlineBotResults,
                telegram_api::Function::messages_setInlineBotResults(query),
            ),
            None => tdutils::log_info!(
                "Send messages.setInlineBotResults for query {} — [net-inert]",
                query.query_id
            ),
        }
    }

    /// The set-inline-bot-results family pop for the
    /// `Td::on_inline_queries_net_answer` drain — the FIFO
    /// head the answer pairs with (the C++ promise rides the query).
    pub(crate) fn pop_set_inline_bot_results_promise(&mut self) -> Option<Promise<()>> {
        if self.pending.set_inline_bot_results.is_empty() {
            None
        } else {
            Some(self.pending.set_inline_bot_results.remove(0))
        }
    }

    /// The `td_->create_handler<SetBotGuestChatResultQuery>(promise_)
    /// ->send(...)` collapse (InlineQueriesManager.cpp:180-183) — the
    /// request promise parks in the family lot first (the handler's
    /// `promise_` member), then the built wire query dispatches. `None`
    /// on the handle keeps the net-inert posture.
    pub(crate) fn send_set_bot_guest_chat_result_query(
        &mut self,
        query_id: i64,
        result: telegram_api::InputBotInlineResult,
        promise: Promise<td_api::InlineMessageId>,
    ) {
        self.pending.set_bot_guest_chat_result.push(promise);
        match self.net.clone() {
            Some(net) => net.dispatch(
                InlineQueriesNetKind::SetBotGuestChatResult,
                telegram_api::Function::messages_setBotGuestChatResult(
                    telegram_api::messages_setBotGuestChatResult {
                        query_id,
                        result: Some(result),
                    },
                ),
            ),
            None => {
                tdutils::log_info!("Send messages.setBotGuestChatResult — [net-inert]")
            }
        }
    }

    /// The set-bot-guest-chat-result family pop for the
    /// `Td::on_inline_queries_net_answer` drain — the FIFO
    /// head the answer pairs with (the C++ promise rides the query).
    pub(crate) fn pop_set_bot_guest_chat_result_promise(
        &mut self,
    ) -> Option<Promise<td_api::InlineMessageId>> {
        if self.pending.set_bot_guest_chat_result.is_empty() {
            None
        } else {
            Some(self.pending.set_bot_guest_chat_result.remove(0))
        }
    }

    /// The `td_->create_handler<SavePreparedInlineMessageQuery>(promise_)
    /// ->send(...)` collapse (InlineQueriesManager.cpp:206-222) — the
    /// request promise parks in the family lot first (the handler's
    /// `promise_` member), then the built wire query dispatches. `None`
    /// on the handle keeps the net-inert posture.
    pub(crate) fn send_save_prepared_inline_message_query(
        &mut self,
        query: telegram_api::messages_savePreparedInlineMessage,
        promise: Promise<td_api::PreparedInlineMessageId>,
    ) {
        self.pending.save_prepared_inline_message.push(promise);
        match self.net.clone() {
            Some(net) => net.dispatch(
                InlineQueriesNetKind::SavePreparedInlineMessage,
                telegram_api::Function::messages_savePreparedInlineMessage(query),
            ),
            None => tdutils::log_info!("Send messages.savePreparedInlineMessage — [net-inert]"),
        }
    }

    /// The save-prepared-inline-message family pop for the
    /// `Td::on_inline_queries_net_answer` drain — the FIFO
    /// head the answer pairs with (the C++ promise rides the query).
    pub(crate) fn pop_save_prepared_inline_message_promise(
        &mut self,
    ) -> Option<Promise<td_api::PreparedInlineMessageId>> {
        if self.pending.save_prepared_inline_message.is_empty() {
            None
        } else {
            Some(self.pending.save_prepared_inline_message.remove(0))
        }
    }

    /// The `td_->create_handler<GetPreparedInlineMessageQuery>(promise_)
    /// ->send(...)` collapse (InlineQueriesManager.cpp:244-250) — the
    /// request promise parks in the family lot first (the handler's
    /// `promise_` member), then the built wire query dispatches with the
    /// handler's `bot_user_id_`/`query_hash_` members on the kind. `None`
    /// on the handle keeps the net-inert posture.
    pub(crate) fn send_get_prepared_inline_message_query(
        &mut self,
        query: telegram_api::messages_getPreparedInlineMessage,
        bot_user_id: UserId,
        query_hash: u64,
        promise: Promise<Option<td_api::PreparedInlineMessage>>,
    ) {
        self.pending.get_prepared_inline_message.push(promise);
        let kind = InlineQueriesNetKind::GetPreparedInlineMessage {
            bot_user_id,
            query_hash,
        };
        match self.net.clone() {
            Some(net) => net.dispatch(
                kind,
                telegram_api::Function::messages_getPreparedInlineMessage(query),
            ),
            None => tdutils::log_info!(
                "Send messages.getPreparedInlineMessage for bot {} — [net-inert]",
                bot_user_id.get()
            ),
        }
    }

    /// The get-prepared-inline-message family pop for the
    /// `Td::on_inline_queries_net_answer` drain — the FIFO
    /// head the answer pairs with (the C++ promise rides the query).
    pub(crate) fn pop_get_prepared_inline_message_promise(
        &mut self,
    ) -> Option<Promise<Option<td_api::PreparedInlineMessage>>> {
        if self.pending.get_prepared_inline_message.is_empty() {
            None
        } else {
            Some(self.pending.get_prepared_inline_message.remove(0))
        }
    }

    /// The `td_->create_handler<SavePreparedKeyboardButtonQuery>(promise_)
    /// ->send(...)` collapse (InlineQueriesManager.cpp:275-282) — the
    /// request promise parks in the family lot first (the handler's
    /// `promise_` member), then the built wire query dispatches. `None`
    /// on the handle keeps the net-inert posture.
    pub(crate) fn send_save_prepared_keyboard_button_query(
        &mut self,
        query: telegram_api::bots_requestWebViewButton,
        promise: Promise<String>,
    ) {
        self.pending.save_prepared_keyboard_button.push(promise);
        match self.net.clone() {
            Some(net) => net.dispatch(
                InlineQueriesNetKind::SavePreparedKeyboardButton,
                telegram_api::Function::bots_requestWebViewButton(query),
            ),
            None => tdutils::log_info!("Send bots.requestWebViewButton — [net-inert]"),
        }
    }

    /// The save-prepared-keyboard-button family pop for the
    /// `Td::on_inline_queries_net_answer` drain — the FIFO
    /// head the answer pairs with (the C++ promise rides the query).
    pub(crate) fn pop_save_prepared_keyboard_button_promise(&mut self) -> Option<Promise<String>> {
        if self.pending.save_prepared_keyboard_button.is_empty() {
            None
        } else {
            Some(self.pending.save_prepared_keyboard_button.remove(0))
        }
    }

    /// The `td_->create_handler<GetRequestedWebViewButtonQuery>(promise_)
    /// ->send(...)` collapse (InlineQueriesManager.cpp:305-311) — the
    /// request promise and the handler's `prepared_button_id_` member
    /// park in the family lot first, then the built wire query dispatches
    /// with the `bot_user_id_` member on the kind. `None` on the handle
    /// keeps the net-inert posture.
    pub(crate) fn send_get_requested_web_view_button_query(
        &mut self,
        query: telegram_api::bots_getRequestedWebViewButton,
        bot_user_id: UserId,
        prepared_button_id: String,
        promise: Promise<td_api::KeyboardButton>,
    ) {
        self.pending
            .get_requested_web_view_button
            .push((prepared_button_id, promise));
        let kind = InlineQueriesNetKind::GetRequestedWebViewButton { bot_user_id };
        match self.net.clone() {
            Some(net) => net.dispatch(
                kind,
                telegram_api::Function::bots_getRequestedWebViewButton(query),
            ),
            None => tdutils::log_info!(
                "Send bots.getRequestedWebViewButton for bot {} — [net-inert]",
                bot_user_id.get()
            ),
        }
    }

    /// The get-requested-web-view-button family pop for the
    /// `Td::on_inline_queries_net_answer` drain — the FIFO
    /// head the answer pairs with (the C++ promise rides the query).
    pub(crate) fn pop_get_requested_web_view_button_query(
        &mut self,
    ) -> Option<(String, Promise<td_api::KeyboardButton>)> {
        if self.pending.get_requested_web_view_button.is_empty() {
            None
        } else {
            Some(self.pending.get_requested_web_view_button.remove(0))
        }
    }

    /// The `td_->create_handler<RequestSimpleWebViewQuery>(promise_)
    /// ->send(...)` collapse (InlineQueriesManager.cpp:347-379) — the
    /// request promise parks in the family lot first (the handler's
    /// `promise_` member), then the built wire query dispatches. `None`
    /// on the handle keeps the net-inert posture.
    pub(crate) fn send_request_simple_web_view_query(
        &mut self,
        query: telegram_api::messages_requestSimpleWebView,
        promise: Promise<td_api::WebAppUrl>,
    ) {
        self.pending.request_simple_web_view.push(promise);
        match self.net.clone() {
            Some(net) => net.dispatch(
                InlineQueriesNetKind::RequestSimpleWebView,
                telegram_api::Function::messages_requestSimpleWebView(query),
            ),
            None => tdutils::log_info!("Send messages.requestSimpleWebView — [net-inert]"),
        }
    }

    /// The request-simple-web-view family pop for the
    /// `Td::on_inline_queries_net_answer` drain — the FIFO
    /// head the answer pairs with (the C++ promise rides the query).
    pub(crate) fn pop_request_simple_web_view_promise(
        &mut self,
    ) -> Option<Promise<td_api::WebAppUrl>> {
        if self.pending.request_simple_web_view.is_empty() {
            None
        } else {
            Some(self.pending.request_simple_web_view.remove(0))
        }
    }

    /// The `td_->create_handler<SendWebViewDataQuery>(promise_)
    /// ->send(...)` collapse (InlineQueriesManager.cpp:407-413) — the
    /// request promise parks in the family lot first (the handler's
    /// `promise_` member), then the built wire query dispatches. `None`
    /// on the handle keeps the net-inert posture.
    pub(crate) fn send_send_web_view_data_query(
        &mut self,
        query: telegram_api::messages_sendWebViewData,
        promise: Promise<()>,
    ) {
        self.pending.send_web_view_data.push(promise);
        match self.net.clone() {
            Some(net) => net.dispatch(
                InlineQueriesNetKind::SendWebViewData,
                telegram_api::Function::messages_sendWebViewData(query),
            ),
            None => tdutils::log_info!("Send messages.sendWebViewData — [net-inert]"),
        }
    }

    /// The send-web-view-data family pop for the
    /// `Td::on_inline_queries_net_answer` drain — the FIFO
    /// head the answer pairs with (the C++ promise rides the query).
    pub(crate) fn pop_send_web_view_data_promise(&mut self) -> Option<Promise<()>> {
        if self.pending.send_web_view_data.is_empty() {
            None
        } else {
            Some(self.pending.send_web_view_data.remove(0))
        }
    }

    /// The `td_->create_handler<SendWebViewResultMessageQuery>(promise_)
    /// ->send(...)` collapse (InlineQueriesManager.cpp:435-439) — the
    /// request promise parks in the family lot first (the handler's
    /// `promise_` member), then the built wire query dispatches. `None`
    /// on the handle keeps the net-inert posture.
    pub(crate) fn send_send_web_view_result_message_query(
        &mut self,
        query: telegram_api::messages_sendWebViewResultMessage,
        promise: Promise<td_api::InlineMessageId>,
    ) {
        self.pending.send_web_view_result_message.push(promise);
        match self.net.clone() {
            Some(net) => net.dispatch(
                InlineQueriesNetKind::SendWebViewResultMessage,
                telegram_api::Function::messages_sendWebViewResultMessage(query),
            ),
            None => {
                tdutils::log_info!("Send messages.sendWebViewResultMessage — [net-inert]")
            }
        }
    }

    /// The send-web-view-result-message family pop for the
    /// `Td::on_inline_queries_net_answer` drain — the FIFO
    /// head the answer pairs with (the C++ promise rides the query).
    pub(crate) fn pop_send_web_view_result_message_promise(
        &mut self,
    ) -> Option<Promise<td_api::InlineMessageId>> {
        if self.pending.send_web_view_result_message.is_empty() {
            None
        } else {
            Some(self.pending.send_web_view_result_message.remove(0))
        }
    }

    /// `get_inline_query_results_object` (InlineQueriesManager.cpp:1767-1780).
    pub fn get_inline_query_results_object(
        &mut self,
        query_hash: u64,
    ) -> Option<td_api::InlineQueryResults> {
        let entry = self
            .inline_query_results
            .get_mut(&query_hash)
            .expect("the cache entry must exist");
        debug_assert!(entry.pending_request_count > 0);
        debug_assert!(entry.is_inline_query);
        entry.pending_request_count -= 1;
        if entry.pending_request_count == 0 {
            self.result_deadlines
                .insert(query_hash, entry.cache_expire_time);
        }
        entry.results.clone()
    }

    /// `get_prepared_inline_message_object` (InlineQueriesManager.cpp:1782-1802).
    pub fn get_prepared_inline_message_object(
        &mut self,
        query_hash: u64,
    ) -> Option<td_api::PreparedInlineMessage> {
        let entry = self
            .inline_query_results
            .get_mut(&query_hash)
            .expect("the cache entry must exist");
        debug_assert!(entry.pending_request_count > 0);
        debug_assert!(!entry.is_inline_query);
        entry.pending_request_count -= 1;
        if entry.pending_request_count == 0 {
            self.result_deadlines
                .insert(query_hash, entry.cache_expire_time);
        }
        let results = entry.results.as_ref()?;
        debug_assert!(results.results.len() == 1);
        let result = results.results.first().cloned()?;
        Some(td_api::PreparedInlineMessage {
            inline_query_id: results.inline_query_id,
            result: Some(result),
            chat_types: Some(
                TargetDialogTypes::from_mask(entry.target_dialog_types_mask)
                    .get_target_chat_types_object(),
            ),
        })
    }

    /// `register_inline_message_content` (InlineQueriesManager.cpp:649-669).
    #[allow(clippy::too_many_arguments)]
    fn register_inline_message_content(
        &mut self,
        env: &mut impl InlineQueriesManagerEnv,
        query_id: i64,
        result_id: &str,
        file_id: FileId,
        inline_message: Option<telegram_api::BotInlineMessage>,
        allowed_media_content_id: i32,
        is_secret_chat: bool,
    ) -> bool {
        debug_assert!(query_id != 0);
        if result_id.is_empty() {
            return false;
        }

        let Some(content) =
            env.create_inline_message_content(file_id, inline_message, allowed_media_content_id)
        else {
            // the C++ content.message_content == nullptr band
            return false;
        };
        if is_secret_chat && !content.is_secret_chat_allowed {
            return false;
        }
        self.inline_message_contents
            .entry(query_id)
            .or_default()
            .insert(result_id.to_owned(), content);
        true
    }

    /// `register_thumbnail` (InlineQueriesManager.cpp:1804-1814) — the
    /// whole body rides the `[blocked: Files]` env seam.
    fn register_thumbnail(
        &mut self,
        env: &mut impl InlineQueriesManagerEnv,
        web_document: Option<telegram_api::WebDocument>,
    ) -> Option<td_api::Thumbnail> {
        let web_document = web_document?;
        env.register_thumbnail(web_document)
    }

    /// `get_web_document_url` (InlineQueriesManager.cpp:1816-1843).
    /// The thumbnail url form; consumed by the web-document thumbnail
    /// plane ([blocked: Wave 3] seam here).
    #[allow(dead_code)]
    fn get_web_document_url(web_document: Option<&telegram_api::WebDocument>) -> String {
        let Some(web_document) = web_document else {
            return String::new();
        };
        let url = match web_document {
            telegram_api::WebDocument::webDocument(document) => document.url.as_str(),
            telegram_api::WebDocument::webDocumentNoProxy(document) => document.url.as_str(),
        };
        match tdutils::http_url::parse_url(url) {
            tdutils::status::Result::Ok(http_url) => http_url.get_url(),
            tdutils::status::Result::Err(_) => {
                tdutils::log_error!("Can't parse URL {}", url);
                String::new()
            }
        }
    }

    /// `get_web_document_content_type` (InlineQueriesManager.cpp:1845-1861).
    fn get_web_document_content_type(web_document: Option<&telegram_api::WebDocument>) -> String {
        match web_document {
            Some(telegram_api::WebDocument::webDocument(document)) => document.mime_type.clone(),
            Some(telegram_api::WebDocument::webDocumentNoProxy(document)) => {
                document.mime_type.clone()
            }
            None => String::new(),
        }
    }

    /// `get_inline_query_result_object` (InlineQueriesManager.cpp:1863-2278)
    /// — the wire result intake. The landed arms (article/contact/geo/
    /// venue over botInlineResult) build their objects over the
    /// Location/Contact records; the media arms keep the C++ drop bands
    /// behind the M.28/M.20/M.15/Files seams.
    pub(crate) fn get_inline_query_result_object(
        &mut self,
        env: &mut impl InlineQueriesManagerEnv,
        query_id: i64,
        dialog_id: DialogId,
        result: telegram_api::BotInlineResult,
    ) -> Option<td_api::InlineQueryResult> {
        let dialog_type = dialog_id.get_type();
        let is_secret_chat = dialog_type == DialogType::SecretChat;
        match result {
            telegram_api::BotInlineResult::botInlineMediaResult(result) => {
                // [blocked: M.28/M.20/M.15/Wave-3 GameManager] the cached
                // media intake: the game/photo/document arms build over the
                // Game record, `on_get_document`, the animation/audio/
                // document/sticker/video/voice object builders and
                // `get_photo`/`get_photo_object` — they assemble at those
                // integrations and the C++ drops the result on every
                // failure band inside them
                let _ = (
                    result.id,
                    result.title,
                    result.description,
                    result.send_message,
                    result.flags,
                );
                let _ = is_secret_chat;
                None
            }
            telegram_api::BotInlineResult::botInlineResult(result) => {
                let content_type = Self::get_web_document_content_type(result.content.as_ref());
                if result.r#type == "article" {
                    let article = td_api::inlineQueryResultArticle {
                        id: result.id,
                        url: result.url,
                        title: result.title,
                        description: result.description,
                        thumbnail: self.register_thumbnail(env, result.thumb),
                    };
                    let result_id = article.id.clone();
                    let registered = self.register_inline_message_content(
                        env,
                        query_id,
                        &result_id,
                        FileId::default(),
                        result.send_message,
                        -1,
                        is_secret_chat,
                    );
                    if !registered {
                        return None;
                    }
                    return Some(td_api::InlineQueryResult::inlineQueryResultArticle(article));
                }
                if result.r#type == "contact" {
                    let contact =
                        if let Some(telegram_api::BotInlineMessage::botInlineMessageMediaContact(
                            inline_message_contact,
                        )) = result.send_message.as_ref()
                        {
                            let c = Contact::new(
                                inline_message_contact.phone_number.clone(),
                                inline_message_contact.first_name.clone(),
                                inline_message_contact.last_name.clone(),
                                inline_message_contact.vcard.clone(),
                                UserId::default(),
                            );
                            c.get_contact_object()
                        } else {
                            let c = Contact::new(
                                result.description.clone(),
                                result.title.clone(),
                                String::new(),
                                String::new(),
                                UserId::default(),
                            );
                            c.get_contact_object()
                        };
                    let article = td_api::inlineQueryResultContact {
                        id: result.id,
                        contact: Some(contact),
                        thumbnail: self.register_thumbnail(env, result.thumb),
                    };
                    let result_id = article.id.clone();
                    let registered = self.register_inline_message_content(
                        env,
                        query_id,
                        &result_id,
                        FileId::default(),
                        result.send_message,
                        -1,
                        is_secret_chat,
                    );
                    if !registered {
                        return None;
                    }
                    return Some(td_api::InlineQueryResult::inlineQueryResultContact(article));
                }
                if result.r#type == "geo" {
                    let location =
                        if let Some(telegram_api::BotInlineMessage::botInlineMessageMediaGeo(
                            inline_message_geo,
                        )) = result.send_message.as_ref()
                        {
                            let l = inline_message_geo
                                .geo
                                .clone()
                                .map(Location::from_geo_point)
                                .unwrap_or_default();
                            l.get_location_object()
                        } else {
                            // `split(Slice(description_))` splits on the first
                            // space; to_double is atof (0.0 on garbage)
                            let (latitude_str, longitude_str) =
                                match result.description.split_once(' ') {
                                    Some((latitude, longitude)) => (latitude, longitude),
                                    None => (result.description.as_str(), ""),
                                };
                            let l = Location::from_geo_point(telegram_api::GeoPoint::geoPoint(
                                telegram_api::geoPoint {
                                    flags: 0,
                                    lat: crate::misc::to_double(latitude_str),
                                    long: crate::misc::to_double(longitude_str),
                                    accuracy_radius: 0,
                                    access_hash: 0,
                                },
                            ));
                            l.get_location_object()
                        };
                    let article = td_api::inlineQueryResultLocation {
                        id: result.id,
                        location,
                        title: result.title,
                        thumbnail: self.register_thumbnail(env, result.thumb),
                    };
                    let result_id = article.id.clone();
                    let registered = self.register_inline_message_content(
                        env,
                        query_id,
                        &result_id,
                        FileId::default(),
                        result.send_message,
                        -1,
                        is_secret_chat,
                    );
                    if !registered {
                        return None;
                    }
                    return Some(td_api::InlineQueryResult::inlineQueryResultLocation(
                        article,
                    ));
                }
                if result.r#type == "venue" {
                    let venue = match result.send_message.as_ref() {
                        Some(telegram_api::BotInlineMessage::botInlineMessageMediaVenue(
                            inline_message_venue,
                        )) => Venue::new(
                            inline_message_venue
                                .geo
                                .clone()
                                .map(Location::from_geo_point)
                                .unwrap_or_default(),
                            inline_message_venue.title.clone(),
                            inline_message_venue.address.clone(),
                            inline_message_venue.provider.clone(),
                            inline_message_venue.venue_id.clone(),
                            inline_message_venue.venue_type.clone(),
                        )
                        .get_venue_object(),
                        Some(telegram_api::BotInlineMessage::botInlineMessageMediaGeo(
                            inline_message_geo,
                        )) => Venue::new(
                            inline_message_geo
                                .geo
                                .clone()
                                .map(Location::from_geo_point)
                                .unwrap_or_default(),
                            result.title.clone(),
                            result.description.clone(),
                            String::new(),
                            String::new(),
                            String::new(),
                        )
                        .get_venue_object(),
                        _ => Venue::new(
                            Location::default(),
                            result.title.clone(),
                            result.description.clone(),
                            String::new(),
                            String::new(),
                            String::new(),
                        )
                        .get_venue_object(),
                    };
                    let article = td_api::inlineQueryResultVenue {
                        id: result.id,
                        venue: Some(venue),
                        thumbnail: self.register_thumbnail(env, result.thumb),
                    };
                    let result_id = article.id.clone();
                    let registered = self.register_inline_message_content(
                        env,
                        query_id,
                        &result_id,
                        FileId::default(),
                        result.send_message,
                        -1,
                        is_secret_chat,
                    );
                    if !registered {
                        return None;
                    }
                    return Some(td_api::InlineQueryResult::inlineQueryResultVenue(article));
                }
                if result.r#type == "photo" && content_type == "image/jpeg" {
                    // the web-photo arm: `get_web_document_photo_size` for
                    // the content and the thumb rides the [blocked: Files]
                    // seam; the C++ drops the result when either size is
                    // invalid ('v'/'g' types included)
                    let photo_size = result
                        .content
                        .clone()
                        .and_then(|content| env.get_web_document_temp_photo_size(content));
                    let thumbnail = result
                        .thumb
                        .clone()
                        .and_then(|thumb| env.get_web_document_thumbnail_photo_size(thumb));
                    let Some(photo_size) = photo_size else {
                        tdutils::log_error!("Receive invalid web document photo");
                        return None;
                    };
                    if photo_size.size_type == b'v' as i32 || photo_size.size_type == b'g' as i32 {
                        tdutils::log_error!("Receive invalid web document photo");
                        return None;
                    }
                    let mut sizes = Vec::new();
                    if let Some(thumbnail) = thumbnail
                        && thumbnail.size_type != b'v' as i32
                        && thumbnail.size_type != b'g' as i32
                    {
                        sizes.push(thumbnail);
                    }
                    sizes.push(photo_size);
                    let photo = td_api::Photo {
                        has_stickers: false,
                        minithumbnail: None,
                        sizes: sizes.iter().map(get_photo_size_object).collect(),
                    };
                    let article = td_api::inlineQueryResultPhoto {
                        id: result.id,
                        photo: Some(photo),
                        title: result.title,
                        description: result.description,
                    };
                    let result_id = article.id.clone();
                    let registered = self.register_inline_message_content(
                        env,
                        query_id,
                        &result_id,
                        FileId::default(),
                        result.send_message,
                        td_api::inputMessagePhoto::ID,
                        is_secret_chat,
                    );
                    if !registered {
                        return None;
                    }
                    return Some(td_api::InlineQueryResult::inlineQueryResultPhoto(article));
                }
                if result.content.is_none() {
                    tdutils::log_error!("Unsupported inline query result without content");
                    return None;
                }

                // the document-family tail (audio/file/gif/sticker/video/
                // voice): the attributes fold + `on_get_document` +
                // the per-type object builders — [blocked: M.28/M.20/M.15]
                let is_animation = result.r#type == "gif"
                    && (content_type == "image/gif" || content_type == "video/mp4");
                let _ = is_animation;
                None
            }
        }
    }

    /// `on_get_inline_query_results` (InlineQueriesManager.cpp:2280-2322).
    pub fn on_get_inline_query_results(
        &mut self,
        env: &mut impl InlineQueriesManagerEnv,
        dialog_id: DialogId,
        bot_user_id: UserId,
        query_hash: u64,
        results: Option<telegram_api::messages_BotResults>,
        mut promise: Promise<Option<td_api::InlineQueryResults>>,
    ) {
        let Some(results) = results else {
            self.get_inline_query_results_object(query_hash);
            promise.set_error(Status::error(500, "Receive no response"));
            return;
        };
        if results.query_id == 0 {
            self.get_inline_query_results_object(query_hash);
            promise.set_error(Status::error(500, "Receive no response"));
            return;
        }

        env.on_get_users(results.users, "on_get_inline_query_results");

        let mut output_results = Vec::with_capacity(results.results.len());
        for result in results.results {
            let Some(output_result) =
                self.get_inline_query_result_object(env, results.query_id, dialog_id, result)
            else {
                continue;
            };
            output_results.push(output_result);
        }

        let entry = self
            .inline_query_results
            .get_mut(&query_hash)
            .expect("the cache entry must exist");
        debug_assert!(entry.is_inline_query);

        self.query_id_to_bot_user_id
            .insert(results.query_id, bot_user_id);

        let button = if let Some(switch_pm) = results.switch_pm {
            Some(td_api::InlineQueryResultsButton {
                text: switch_pm.text,
                r#type: Some(
                    td_api::InlineQueryResultsButtonType::inlineQueryResultsButtonTypeStartBot(
                        td_api::inlineQueryResultsButtonTypeStartBot {
                            parameter: switch_pm.start_param,
                        },
                    ),
                ),
            })
        } else if let Some(switch_webview) = results.switch_webview {
            Some(td_api::InlineQueryResultsButton {
                text: switch_webview.text,
                r#type: Some(
                    td_api::InlineQueryResultsButtonType::inlineQueryResultsButtonTypeWebApp(
                        td_api::inlineQueryResultsButtonTypeWebApp {
                            url: format!("{}#iq", switch_webview.url),
                        },
                    ),
                ),
            })
        } else {
            None
        };

        entry.results = Some(td_api::InlineQueryResults {
            inline_query_id: results.query_id,
            button,
            results: output_results,
            next_offset: results.next_offset,
        });
        entry.cache_expire_time = tdutils::time::now() + f64::from(results.cache_time);
        let object = self.get_inline_query_results_object(query_hash);
        promise.set_value(object);
    }

    /// `on_get_prepared_inline_message` (InlineQueriesManager.cpp:2324-2357).
    pub fn on_get_prepared_inline_message(
        &mut self,
        env: &mut impl InlineQueriesManagerEnv,
        bot_user_id: UserId,
        query_hash: u64,
        prepared_message: Option<telegram_api::messages_PreparedInlineMessage>,
        mut promise: Promise<Option<td_api::PreparedInlineMessage>>,
    ) {
        let Some(prepared_message) = prepared_message else {
            self.get_prepared_inline_message_object(query_hash);
            promise.set_error(Status::error(500, "Receive no response"));
            return;
        };
        if prepared_message.query_id == 0 {
            self.get_prepared_inline_message_object(query_hash);
            promise.set_error(Status::error(500, "Receive no response"));
            return;
        }

        env.on_get_users(prepared_message.users, "on_get_prepared_inline_message");

        let Some(output_result) = prepared_message.result.and_then(|result| {
            self.get_inline_query_result_object(
                env,
                prepared_message.query_id,
                DialogId::default(),
                result,
            )
        }) else {
            self.get_prepared_inline_message_object(query_hash);
            promise.set_error(Status::error(500, "Receive invalid response"));
            return;
        };

        let entry = self
            .inline_query_results
            .get_mut(&query_hash)
            .expect("the cache entry must exist");
        debug_assert!(!entry.is_inline_query);

        self.query_id_to_bot_user_id
            .insert(prepared_message.query_id, bot_user_id);

        entry.results = Some(td_api::InlineQueryResults {
            inline_query_id: prepared_message.query_id,
            button: None,
            results: vec![output_result],
            next_offset: String::new(),
        });
        entry.cache_expire_time = tdutils::time::now() + f64::from(prepared_message.cache_time);
        entry.target_dialog_types_mask =
            TargetDialogTypes::from_telegram(&prepared_message.peer_types).get_mask();
        let object = self.get_prepared_inline_message_object(query_hash);
        promise.set_value(object);
    }

    /// `get_recent_inline_bots` (InlineQueriesManager.cpp:2359-2366).
    pub fn get_recent_inline_bots(
        &mut self,
        env: &mut impl InlineQueriesManagerEnv,
        promise: Promise<Vec<UserId>>,
    ) {
        self.pending_recent_inline_bots.push(promise);
        if !self.load_recently_used_bots(env) {
            // the promise parks behind the resolution
            return;
        }
        let bots = self.recently_used_bot_user_ids.clone();
        self.finish_recent_inline_bots(&bots);
    }

    /// the collapse of the C++ multipromise completion: the parked
    /// getRecentInlineBots promises answer with the loaded bots.
    fn finish_recent_inline_bots(&mut self, bots: &[UserId]) {
        for mut promise in std::mem::take(&mut self.pending_recent_inline_bots) {
            promise.set_value(bots.to_vec());
        }
    }

    /// `save_recently_used_bots` (InlineQueriesManager.cpp:2368-2385).
    pub(crate) fn save_recently_used_bots(&mut self, env: &mut impl InlineQueriesManagerEnv) {
        if self.recently_used_bots_loaded < 2 {
            return;
        }

        let mut value = String::new();
        let mut value_ids = String::new();
        for bot_user_id in &self.recently_used_bot_user_ids {
            if !value.is_empty() {
                value.push(',');
                value_ids.push(',');
            }
            value.push_str(&env.get_user_first_username(*bot_user_id));
            value_ids.push_str(&bot_user_id.get().to_string());
        }
        env.binlog_pmc_set("recently_used_inline_bot_usernames", &value);
        env.binlog_pmc_set("recently_used_inline_bots", &value_ids);
    }

    /// `load_recently_used_bots` (InlineQueriesManager.cpp:2387-2447) —
    /// returns true when the bots are ready (the C++ then resolves the
    /// request promise in get_recent_inline_bots; here the caller does).
    fn load_recently_used_bots(&mut self, env: &mut impl InlineQueriesManagerEnv) -> bool {
        if self.recently_used_bots_loaded >= 2 {
            return true;
        }

        let saved_bot_ids = env.binlog_pmc_get("recently_used_inline_bots");
        let bot_ids = full_split(&saved_bot_ids, ',');
        let saved_bots = env.binlog_pmc_get("recently_used_inline_bot_usernames");
        let bot_usernames = full_split(&saved_bots, ',');
        if bot_ids.is_empty() {
            self.recently_used_bots_loaded = 2;
            if !self.recently_used_bot_user_ids.is_empty() {
                self.save_recently_used_bots(env);
            }
            return true;
        }

        if self.recently_used_bots_loaded == 1 && self.resolve_recent_inline_bots_pending == 0 {
            // queries were sent and have already been finished
            let newly_used_bots = std::mem::take(&mut self.recently_used_bot_user_ids);

            for bot_id in bot_ids.iter().rev() {
                let user_id = UserId::new(to_integer_i64(bot_id));
                if env.have_min_user(user_id) {
                    self.update_bot_usage(env, user_id);
                } else {
                    tdutils::log_error!("Can't find {}", user_id.get());
                }
            }
            for user_id in newly_used_bots.iter().rev() {
                self.update_bot_usage(env, *user_id);
            }
            self.recently_used_bots_loaded = 2;
            if !newly_used_bots.is_empty() {
                self.save_recently_used_bots(env);
            }
            return true;
        }

        if self.recently_used_bots_loaded == 0 {
            self.recently_used_bots_loaded = 1;
            if !env.use_chat_info_database() {
                // search_public_dialog per username — [blocked:
                // dialog-plane resolve]: the resolution is a td-level
                // walk over the dialog plane (dialog creation + the
                // ResolveUsername family) the split env can't reach, and
                // the completion must re-enter this manager (the C++
                // MultiPromiseActor hop, InlineQueriesManager.cpp:
                // 2428-2443); the parked promises wait for it
                tdutils::log_info!(
                    "Resolve {} recent inline bot usernames — [blocked: dialog-plane resolve]",
                    bot_usernames.len()
                );
                self.resolve_recent_inline_bots_pending += bot_usernames.len();
            } else {
                for bot_id in &bot_ids {
                    let user_id = UserId::new(to_integer_i64(bot_id));
                    if env.have_min_user(user_id) {
                        // UserManager::get_user completes synchronously
                        // from memory
                    } else {
                        // the users.getUsers fetch — [blocked: user-plane
                        // re-entry]: the GetUserMerger promise would have
                        // to hop back into this manager's multipromise
                        // (InlineQueriesManager.cpp:2440), and the split
                        // env carries no such completion surface
                        tdutils::log_info!(
                            "Load recent inline bot {} — [blocked: user-plane re-entry]",
                            user_id.get()
                        );
                        self.resolve_recent_inline_bots_pending += 1;
                    }
                }
                if self.resolve_recent_inline_bots_pending == 0 {
                    // everything resolved synchronously: run the finish
                    // band and answer the parked promises
                    let ready = self.recently_used_bots_loaded == 1;
                    if ready {
                        let newly_used_bots = std::mem::take(&mut self.recently_used_bot_user_ids);
                        for bot_id in bot_ids.iter().rev() {
                            let user_id = UserId::new(to_integer_i64(bot_id));
                            if env.have_min_user(user_id) {
                                self.update_bot_usage(env, user_id);
                            } else {
                                tdutils::log_error!("Can't find {}", user_id.get());
                            }
                        }
                        for user_id in newly_used_bots.iter().rev() {
                            self.update_bot_usage(env, *user_id);
                        }
                        self.recently_used_bots_loaded = 2;
                        if !newly_used_bots.is_empty() {
                            self.save_recently_used_bots(env);
                        }
                        let bots = self.recently_used_bot_user_ids.clone();
                        self.finish_recent_inline_bots(&bots);
                    }
                }
            }
        }
        false
    }

    #[allow(clippy::too_many_arguments)]
    /// `on_new_inline_query` (InlineQueriesManager.cpp:2449-2487).
    pub fn on_new_inline_query(
        &mut self,
        env: &mut impl InlineQueriesManagerEnv,
        query_id: i64,
        sender_user_id: UserId,
        user_location: Location,
        peer_type: Option<telegram_api::InlineQueryPeerType>,
        query: &str,
        offset: &str,
    ) {
        if !sender_user_id.is_valid() {
            tdutils::log_error!(
                "Receive new inline query from invalid {}",
                sender_user_id.get()
            );
            return;
        }
        if !env.auth_is_bot() {
            tdutils::log_error!("Receive new inline query");
            return;
        }
        let chat_type = peer_type.map(|peer_type| match peer_type {
            telegram_api::InlineQueryPeerType::inlineQueryPeerTypeSameBotPM(_) => {
                td_api::ChatType::chatTypePrivate(td_api::chatTypePrivate {
                    user_id: env.get_user_id_object(sender_user_id, "inlineQueryPeerTypeSameBotPM"),
                })
            }
            telegram_api::InlineQueryPeerType::inlineQueryPeerTypeBotPM(_)
            | telegram_api::InlineQueryPeerType::inlineQueryPeerTypePM(_) => {
                td_api::ChatType::chatTypePrivate(td_api::chatTypePrivate { user_id: 0 })
            }
            telegram_api::InlineQueryPeerType::inlineQueryPeerTypeChat(_) => {
                td_api::ChatType::chatTypeBasicGroup(td_api::chatTypeBasicGroup {
                    basic_group_id: 0,
                })
            }
            telegram_api::InlineQueryPeerType::inlineQueryPeerTypeMegagroup(_) => {
                td_api::ChatType::chatTypeSupergroup(td_api::chatTypeSupergroup {
                    supergroup_id: 0,
                    is_channel: false,
                })
            }
            telegram_api::InlineQueryPeerType::inlineQueryPeerTypeBroadcast(_) => {
                td_api::ChatType::chatTypeSupergroup(td_api::chatTypeSupergroup {
                    supergroup_id: 0,
                    is_channel: true,
                })
            }
        });
        let sender_user_id_object = env.get_user_id_object(sender_user_id, "updateNewInlineQuery");
        env.send_update(td_api::Update::updateNewInlineQuery(
            td_api::updateNewInlineQuery {
                id: query_id,
                sender_user_id: sender_user_id_object,
                user_location: user_location.get_location_object(),
                chat_type,
                query: query.to_owned(),
                offset: offset.to_owned(),
            },
        ));
    }

    /// `on_chosen_result` (InlineQueriesManager.cpp:2489-2506).
    pub fn on_chosen_result(
        &mut self,
        env: &mut impl InlineQueriesManagerEnv,
        user_id: UserId,
        user_location: Location,
        query: &str,
        result_id: &str,
        input_bot_inline_message_id: Option<telegram_api::InputBotInlineMessageID>,
    ) {
        if !user_id.is_valid() {
            tdutils::log_error!(
                "Receive chosen inline query result from invalid {}",
                user_id.get()
            );
            return;
        }
        if !env.have_min_user(user_id) {
            tdutils::log_error!("Receive unknown {}", user_id.get());
        }
        if !env.auth_is_bot() {
            tdutils::log_error!("Receive chosen inline query result");
            return;
        }
        let user_id_object = env.get_user_id_object(user_id, "updateNewChosenInlineResult");
        env.send_update(td_api::Update::updateNewChosenInlineResult(
            td_api::updateNewChosenInlineResult {
                sender_user_id: user_id_object,
                user_location: user_location.get_location_object(),
                query: query.to_owned(),
                result_id: result_id.to_owned(),
                inline_message_id: get_inline_message_id(input_bot_inline_message_id),
            },
        ));
    }

    /// `update_bot_usage` (InlineQueriesManager.cpp:2508-2526).
    pub(crate) fn update_bot_usage(
        &mut self,
        env: &mut impl InlineQueriesManagerEnv,
        bot_user_id: UserId,
    ) -> bool {
        if !bot_user_id.is_valid() {
            return false;
        }
        if self.recently_used_bot_user_ids.first() == Some(&bot_user_id) {
            return false;
        }
        let Ok(bot_data) = env.get_bot_data(bot_user_id) else {
            return false;
        };
        if bot_data.username.is_empty() || !bot_data.is_inline {
            return false;
        }

        add_to_top(
            &mut self.recently_used_bot_user_ids,
            MAX_RECENT_INLINE_BOTS,
            bot_user_id,
        );

        true
    }

    /// `remove_recent_inline_bot` (InlineQueriesManager.cpp:2528-2533).
    pub fn remove_recent_inline_bot(
        &mut self,
        env: &mut impl InlineQueriesManagerEnv,
        bot_user_id: UserId,
        mut promise: Promise<()>,
    ) {
        if let Some(position) = self
            .recently_used_bot_user_ids
            .iter()
            .position(|user_id| *user_id == bot_user_id)
        {
            self.recently_used_bot_user_ids.remove(position);
            self.save_recently_used_bots(env);
        }
        promise.set_value(());
    }

    /// `get_inline_message` (InlineQueriesManager.cpp:544-647) —
    /// `Ok(None)` is the seam-parked band (a downstream M.9/M.17/payments
    /// conversion will complete after its integration; the C++ promise
    /// never answers offline either way).
    fn get_inline_message(
        &mut self,
        env: &mut impl InlineQueriesManagerEnv,
        input_message_content: Option<Box<td_api::InputMessageContent>>,
        reply_markup_ptr: Option<td_api::ReplyMarkup>,
        allowed_media_content_id: AllowedMediaContent,
    ) -> Result<Option<telegram_api::InputBotInlineMessage>, Status> {
        let Some(input_message_content) = input_message_content else {
            return Err(Status::error(400, "Inline message must be non-empty"));
        };
        let is_bot = env.auth_is_bot();
        let input_reply_markup = env.get_input_reply_markup(reply_markup_ptr, is_bot, true)?;

        match input_message_content.as_ref() {
            td_api::InputMessageContent::inputMessageText(_) => {
                let input_message_text = process_input_message_text(
                    env,
                    env.get_my_dialog_id(),
                    &input_message_content,
                    true,
                    true,
                )?;
                let entities = env.get_input_message_entities(
                    &input_message_text.text.entities,
                    "get_inline_message",
                );
                let has_entities = !entities.is_empty();
                if !input_message_text.web_page_url.is_empty() {
                    let mut flags = 0;
                    if input_reply_markup.is_some() {
                        flags |= MASK_REPLY_MARKUP;
                    }
                    if has_entities {
                        flags |= MASK_ENTITIES;
                    }
                    let is_optional = input_message_text.text.text.is_empty();
                    return Ok(Some(
                        telegram_api::InputBotInlineMessage::inputBotInlineMessageMediaWebPage(
                            telegram_api::inputBotInlineMessageMediaWebPage {
                                flags,
                                invert_media: input_message_text.show_above_text,
                                force_large_media: input_message_text.force_large_media,
                                force_small_media: input_message_text.force_small_media,
                                optional: is_optional,
                                message: input_message_text.text.text,
                                entities,
                                url: input_message_text.web_page_url,
                                reply_markup: input_reply_markup,
                            },
                        ),
                    ));
                }
                let mut flags = 0;
                if input_reply_markup.is_some() {
                    flags |= MASK_REPLY_MARKUP;
                }
                if has_entities {
                    flags |= MASK_ENTITIES;
                }
                Ok(Some(
                    telegram_api::InputBotInlineMessage::inputBotInlineMessageText(
                        telegram_api::inputBotInlineMessageText {
                            flags,
                            no_webpage: input_message_text.disable_web_page_preview,
                            invert_media: input_message_text.show_above_text,
                            message: input_message_text.text.text,
                            entities,
                            reply_markup: input_reply_markup,
                        },
                    ),
                ))
            }
            td_api::InputMessageContent::inputMessageRichMessage(rich_message_input) => {
                let Some(input_rich_message) = env.get_input_rich_message(rich_message_input)?
                else {
                    // the [blocked: M.17] seam parked the request
                    return Ok(None);
                };
                let mut flags = 0;
                if input_reply_markup.is_some() {
                    flags |= MASK_REPLY_MARKUP;
                }
                Ok(Some(
                    telegram_api::InputBotInlineMessage::inputBotInlineMessageRichMessage(
                        telegram_api::inputBotInlineMessageRichMessage {
                            flags,
                            reply_markup: input_reply_markup,
                            rich_message: Some(input_rich_message),
                        },
                    ),
                ))
            }
            td_api::InputMessageContent::inputMessageContact(_) => {
                let contact = process_input_message_contact(env, &input_message_content)?;
                let mut flags = 0;
                if input_reply_markup.is_some() {
                    flags |= MASK_REPLY_MARKUP;
                }
                Ok(Some(
                    telegram_api::InputBotInlineMessage::inputBotInlineMessageMediaContact(
                        telegram_api::inputBotInlineMessageMediaContact {
                            flags,
                            phone_number: contact.get_phone_number().to_owned(),
                            first_name: contact.get_first_name().to_owned(),
                            last_name: contact.get_last_name().to_owned(),
                            vcard: contact.get_vcard().to_owned(),
                            reply_markup: input_reply_markup,
                        },
                    ),
                ))
            }
            td_api::InputMessageContent::inputMessageInvoice(invoice) => {
                env.get_input_bot_inline_message_media_invoice(invoice, input_reply_markup)
            }
            td_api::InputMessageContent::inputMessageLocation(_)
            | td_api::InputMessageContent::inputMessageLiveLocation(_) => {
                let location = process_input_message_location(&input_message_content)?;
                Ok(Some(
                    telegram_api::InputBotInlineMessage::inputBotInlineMessageMediaGeo(
                        location.get_input_bot_inline_message_media_geo(input_reply_markup),
                    ),
                ))
            }
            td_api::InputMessageContent::inputMessageVenue(venue_input) => {
                let venue = process_input_message_venue(venue_input.venue.clone())?;
                Ok(Some(
                    telegram_api::InputBotInlineMessage::inputBotInlineMessageMediaVenue(
                        venue.get_input_bot_inline_message_media_venue(input_reply_markup),
                    ),
                ))
            }
            content if input_content_is(content, allowed_media_content_id) => {
                let caption = extract_input_caption(content);
                let caption = env.parse_formatted_text(
                    env.get_my_dialog_id(),
                    caption,
                    true,
                    true,
                    true,
                    false,
                )?;
                let entities =
                    env.get_input_message_entities(&caption.entities, "get_inline_message");
                let mut flags = 0;
                if input_reply_markup.is_some() {
                    flags |= MASK_REPLY_MARKUP;
                }
                let has_entities = !entities.is_empty();
                if has_entities {
                    flags |= MASK_ENTITIES;
                }
                let invert_media = extract_input_invert_media(content);
                Ok(Some(
                    telegram_api::InputBotInlineMessage::inputBotInlineMessageMediaAuto(
                        telegram_api::inputBotInlineMessageMediaAuto {
                            flags,
                            invert_media,
                            message: caption.text,
                            entities,
                            reply_markup: input_reply_markup,
                        },
                    ),
                ))
            }
            _ => Err(Status::error(400, "Unallowed inline message content type")),
        }
    }

    /// `get_input_bot_inline_result` (InlineQueriesManager.cpp:881-1278) —
    /// `Ok(None)` is the seam-parked band.
    fn get_input_bot_inline_result(
        &mut self,
        env: &mut impl InlineQueriesManagerEnv,
        result: Option<td_api::InputInlineQueryResult>,
        is_gallery: &mut bool,
        force_vertical: &mut bool,
    ) -> Result<Option<telegram_api::InputBotInlineResult>, Status> {
        let Some(result) = result else {
            return Err(Status::error(400, "Inline query result must be non-empty"));
        };

        #[allow(unused_assignments)]
        let mut id = String::new();
        #[allow(unused_assignments)]
        let mut url = String::new();
        #[allow(unused_assignments)]
        let mut result_type = String::new();
        let mut title = String::new();
        let mut description = String::new();
        let mut thumbnail_url = String::new();
        let mut thumbnail_type = "image/jpeg".to_owned();
        let mut content_url = String::new();
        let mut content_type = String::new();
        let mut thumbnail_width = 0;
        let mut thumbnail_height = 0;
        let mut width = 0;
        let mut height = 0;
        let mut duration = 0;

        let file_type: u8; // the FileType byte the C++ folds; b'T' is Temp
        let inline_message: Option<telegram_api::InputBotInlineMessage>;

        match result {
            td_api::InputInlineQueryResult::inputInlineQueryResultAnimation(animation) => {
                let td_api::inputInlineQueryResultAnimation {
                    id: animation_id,
                    title: animation_title,
                    thumbnail_url: animation_thumbnail_url,
                    thumbnail_mime_type: animation_thumbnail_mime_type,
                    video_url: animation_video_url,
                    video_mime_type: animation_video_mime_type,
                    video_duration: animation_video_duration,
                    video_width: animation_video_width,
                    video_height: animation_video_height,
                    reply_markup,
                    input_message_content,
                } = animation;
                result_type = "gif".to_owned();
                id = animation_id;
                title = animation_title;
                thumbnail_url = animation_thumbnail_url;
                if !animation_thumbnail_mime_type.is_empty() {
                    thumbnail_type = animation_thumbnail_mime_type;
                }
                content_url = animation_video_url;
                content_type = animation_video_mime_type;
                if content_type != "image/gif" && content_type != "video/mp4" {
                    return Err(Status::error(400, "Wrong animation MIME type specified"));
                }
                duration = animation_video_duration;
                width = animation_video_width;
                height = animation_video_height;
                *is_gallery = true;

                file_type = b'A'; // FileType::Animation
                inline_message = self.get_inline_message(
                    env,
                    input_message_content,
                    reply_markup,
                    AllowedMediaContent::Animation,
                )?;
            }
            td_api::InputInlineQueryResult::inputInlineQueryResultArticle(article) => {
                result_type = "article".to_owned();
                id = article.id;
                url = article.url;
                title = article.title;
                description = article.description;
                thumbnail_url = article.thumbnail_url;
                if !thumbnail_url.is_empty() {
                    thumbnail_width = article.thumbnail_width;
                    thumbnail_height = article.thumbnail_height;
                }
                *force_vertical = true;

                file_type = b'T'; // FileType::Temp
                inline_message = self.get_inline_message(
                    env,
                    article.input_message_content,
                    article.reply_markup,
                    AllowedMediaContent::None,
                )?;
            }
            td_api::InputInlineQueryResult::inputInlineQueryResultAudio(audio) => {
                result_type = "audio".to_owned();
                id = audio.id;
                title = audio.title;
                description = audio.performer;
                content_url = audio.audio_url;
                content_type = "audio/mpeg".to_owned();
                duration = audio.audio_duration;
                *force_vertical = true;

                file_type = b'O'; // FileType::Audio
                inline_message = self.get_inline_message(
                    env,
                    audio.input_message_content,
                    audio.reply_markup,
                    AllowedMediaContent::Audio,
                )?;
            }
            td_api::InputInlineQueryResult::inputInlineQueryResultContact(contact) => {
                let Some(contact_record) = contact.contact else {
                    return Err(Status::error(400, "Contact must be non-empty"));
                };
                result_type = "contact".to_owned();
                id = contact.id;
                let phone_number = contact_record.phone_number.trim().to_owned();
                let first_name = contact_record.first_name.trim().to_owned();
                let last_name = contact_record.last_name.trim().to_owned();
                if phone_number.is_empty() {
                    return Err(Status::error(
                        400,
                        "Field \"phone_number\" must contain a valid phone number",
                    ));
                }
                if first_name.is_empty() {
                    return Err(Status::error(400, "Field \"first_name\" must be non-empty"));
                }
                if last_name.is_empty() {
                    title = first_name;
                } else {
                    title = format!("{} {}", first_name, last_name);
                }
                description = phone_number;
                thumbnail_url = contact.thumbnail_url;
                if !thumbnail_url.is_empty() {
                    thumbnail_width = contact.thumbnail_width;
                    thumbnail_height = contact.thumbnail_height;
                }
                *force_vertical = true;

                file_type = b'T'; // FileType::Temp
                inline_message = self.get_inline_message(
                    env,
                    contact.input_message_content,
                    contact.reply_markup,
                    AllowedMediaContent::None,
                )?;
            }
            td_api::InputInlineQueryResult::inputInlineQueryResultDocument(document) => {
                result_type = "file".to_owned();
                id = document.id;
                title = document.title;
                description = document.description;
                thumbnail_url = document.thumbnail_url;
                content_url = document.document_url;
                content_type = document.mime_type;
                thumbnail_width = document.thumbnail_width;
                thumbnail_height = document.thumbnail_height;

                if content_url.contains('.') {
                    if content_type.starts_with("application/pdf") {
                        content_type = "application/pdf".to_owned();
                    } else if content_type.starts_with("application/zip") {
                        content_type = "application/zip".to_owned();
                    } else {
                        return Err(Status::error(400, "Unallowed document MIME type"));
                    }
                }

                file_type = b'D'; // FileType::Document
                inline_message = self.get_inline_message(
                    env,
                    document.input_message_content,
                    document.reply_markup,
                    AllowedMediaContent::Document,
                )?;
            }
            td_api::InputInlineQueryResult::inputInlineQueryResultGame(game) => {
                let input_reply_markup =
                    env.get_input_reply_markup(game.reply_markup, true, true)?;
                let mut flags = 0;
                if input_reply_markup.is_some() {
                    flags |= MASK_REPLY_MARKUP;
                }
                return Ok(Some(
                    telegram_api::InputBotInlineResult::inputBotInlineResultGame(
                        telegram_api::inputBotInlineResultGame {
                            id: game.id,
                            short_name: game.game_short_name,
                            send_message: Some(
                                telegram_api::InputBotInlineMessage::inputBotInlineMessageGame(
                                    telegram_api::inputBotInlineMessageGame {
                                        flags,
                                        reply_markup: input_reply_markup,
                                    },
                                ),
                            ),
                        },
                    ),
                ));
            }
            td_api::InputInlineQueryResult::inputInlineQueryResultLocation(location) => {
                let Some(location_record) = location.location else {
                    return Err(Status::error(400, "Location must be non-empty"));
                };
                result_type = "geo".to_owned();
                id = location.id;
                title = location.title;
                description = format!("{} {}", location_record.latitude, location_record.longitude);
                thumbnail_url = location.thumbnail_url;
                if !thumbnail_url.is_empty() {
                    thumbnail_width = location.thumbnail_width;
                    thumbnail_height = location.thumbnail_height;
                }

                file_type = b'T'; // FileType::Temp
                inline_message = self.get_inline_message(
                    env,
                    location.input_message_content,
                    location.reply_markup,
                    AllowedMediaContent::None,
                )?;
            }
            td_api::InputInlineQueryResult::inputInlineQueryResultPhoto(photo) => {
                result_type = "photo".to_owned();
                id = photo.id;
                title = photo.title;
                description = photo.description;
                thumbnail_url = photo.thumbnail_url;
                content_url = photo.photo_url;
                content_type = "image/jpeg".to_owned();
                width = photo.photo_width;
                height = photo.photo_height;
                *is_gallery = true;

                file_type = b'P'; // FileType::Photo
                inline_message = self.get_inline_message(
                    env,
                    photo.input_message_content,
                    photo.reply_markup,
                    AllowedMediaContent::Photo,
                )?;
            }
            td_api::InputInlineQueryResult::inputInlineQueryResultSticker(sticker) => {
                result_type = "sticker".to_owned();
                id = sticker.id;
                thumbnail_url = sticker.thumbnail_url;
                content_url = sticker.sticker_url;
                content_type = "image/webp".to_owned(); // or x-tgsticker/video/webm; not used for previously uploaded files
                width = sticker.sticker_width;
                height = sticker.sticker_height;
                *is_gallery = true;
                if content_url.contains('.') {
                    return Err(Status::error(400, "Wrong sticker_file_id specified"));
                }

                file_type = b'S'; // FileType::Sticker
                inline_message = self.get_inline_message(
                    env,
                    sticker.input_message_content,
                    sticker.reply_markup,
                    AllowedMediaContent::Sticker,
                )?;
            }
            td_api::InputInlineQueryResult::inputInlineQueryResultVenue(venue) => {
                let Some(venue_record) = venue.venue else {
                    return Err(Status::error(400, "Venue must be non-empty"));
                };
                result_type = "venue".to_owned();
                id = venue.id;
                title = venue_record.title.clone();
                description = venue_record.address.clone();
                thumbnail_url = venue.thumbnail_url;
                if !thumbnail_url.is_empty() {
                    thumbnail_width = venue.thumbnail_width;
                    thumbnail_height = venue.thumbnail_height;
                }

                file_type = b'T'; // FileType::Temp
                inline_message = self.get_inline_message(
                    env,
                    venue.input_message_content,
                    venue.reply_markup,
                    AllowedMediaContent::None,
                )?;
            }
            td_api::InputInlineQueryResult::inputInlineQueryResultVideo(video) => {
                result_type = "video".to_owned();
                id = video.id;
                title = video.title;
                description = video.description;
                thumbnail_url = video.thumbnail_url;
                content_url = video.video_url;
                content_type = video.mime_type;
                width = video.video_width;
                height = video.video_height;
                duration = video.video_duration;

                if content_url.contains('.') {
                    if content_type.starts_with("video/mp4") {
                        content_type = "video/mp4".to_owned();
                    } else if content_type.starts_with("text/html") {
                        content_type = "text/html".to_owned();
                    } else {
                        return Err(Status::error(400, "Unallowed video MIME type"));
                    }
                }

                file_type = b'V'; // FileType::Video
                inline_message = self.get_inline_message(
                    env,
                    video.input_message_content,
                    video.reply_markup,
                    AllowedMediaContent::Video,
                )?;
            }
            td_api::InputInlineQueryResult::inputInlineQueryResultVoiceNote(voice_note) => {
                result_type = "voice".to_owned();
                id = voice_note.id;
                title = voice_note.title;
                content_url = voice_note.voice_note_url;
                content_type = "audio/ogg".to_owned();
                duration = voice_note.voice_note_duration;
                *force_vertical = true;

                file_type = b'N'; // FileType::VoiceNote
                inline_message = self.get_inline_message(
                    env,
                    voice_note.input_message_content,
                    voice_note.reply_markup,
                    AllowedMediaContent::VoiceNote,
                )?;
            }
        }
        let Some(inline_message) = inline_message else {
            // the promise landed in the seam
            return Ok(None);
        };
        if matches!(
            inline_message,
            telegram_api::InputBotInlineMessage::inputBotInlineMessageMediaAuto(_)
        ) && file_type == b'T'
        {
            return Err(Status::error(
                400,
                "Sent message content must be explicitly specified",
            ));
        }

        if duration < 0 {
            duration = 0;
        }

        let mut flags = 0;
        if !title.is_empty() {
            flags |= MASK_TITLE;
            if clean_input_string(&title).is_none() {
                return Err(Status::error(400, "Strings must be encoded in UTF-8"));
            }
        }
        if !description.is_empty() {
            flags |= MASK_DESCRIPTION;
            if clean_input_string(&description).is_none() {
                return Err(Status::error(400, "Strings must be encoded in UTF-8"));
            }
        }

        if file_type != b'T' && !content_url.contains('.') {
            // `td_->file_manager_->get_input_file_id(FileType, inputFileRemote,
            // DialogId(), false, false)` + the file-view probes —
            // [blocked: Files]: the previously-uploaded-file branch of the
            // inputBotInlineResultPhoto/Document build
            return Ok(None);
        }

        if !url.is_empty() {
            flags |= MASK_URL;
            if clean_input_string(&url).is_none() {
                return Err(Status::error(400, "Strings must be encoded in UTF-8"));
            }
        }
        let mut thumbnail = None;
        if !thumbnail_url.is_empty() {
            flags |= MASK_THUMB;
            if clean_input_string(&thumbnail_url).is_none() {
                return Err(Status::error(400, "Strings must be encoded in UTF-8"));
            }
            let mut attributes = Vec::new();
            if thumbnail_width > 0 && thumbnail_height > 0 {
                attributes.push(telegram_api::DocumentAttribute::documentAttributeImageSize(
                    telegram_api::documentAttributeImageSize {
                        w: thumbnail_width,
                        h: thumbnail_height,
                    },
                ));
            }
            thumbnail = Some(telegram_api::InputWebDocument {
                url: thumbnail_url,
                size: 0,
                mime_type: thumbnail_type,
                attributes,
            });
        }
        let mut content = None;
        if !content_url.is_empty() || !content_type.is_empty() {
            flags |= MASK_CONTENT;
            if clean_input_string(&content_url).is_none() {
                return Err(Status::error(400, "Strings must be encoded in UTF-8"));
            }
            if clean_input_string(&content_type).is_none() {
                return Err(Status::error(400, "Strings must be encoded in UTF-8"));
            }

            let mut attributes = Vec::new();
            if width > 0 && height > 0 {
                if (duration > 0 || result_type == "video" || content_type == "video/mp4")
                    && !content_type.starts_with("image/")
                {
                    attributes.push(telegram_api::DocumentAttribute::documentAttributeVideo(
                        telegram_api::documentAttributeVideo {
                            flags: 0,
                            round_message: false,
                            supports_streaming: false,
                            nosound: false,
                            duration: f64::from(duration),
                            w: width,
                            h: height,
                            preload_prefix_size: 0,
                            video_start_ts: 0.0,
                            video_codec: String::new(),
                        },
                    ));
                } else {
                    attributes.push(telegram_api::DocumentAttribute::documentAttributeImageSize(
                        telegram_api::documentAttributeImageSize {
                            w: width,
                            h: height,
                        },
                    ));
                }
            } else if result_type == "audio" {
                attributes.push(telegram_api::DocumentAttribute::documentAttributeAudio(
                    telegram_api::documentAttributeAudio {
                        flags: (1 << 0) | (1 << 1), // TITLE_MASK | PERFORMER_MASK
                        voice: false,
                        duration,
                        title: title.clone(),
                        performer: description.clone(),
                        waveform: Vec::new(),
                    },
                ));
            } else if result_type == "voice" {
                attributes.push(telegram_api::DocumentAttribute::documentAttributeAudio(
                    telegram_api::documentAttributeAudio {
                        flags: 0,
                        voice: true,
                        duration,
                        title: String::new(),
                        performer: String::new(),
                        waveform: Vec::new(),
                    },
                ));
            }
            attributes.push(telegram_api::DocumentAttribute::documentAttributeFilename(
                telegram_api::documentAttributeFilename {
                    file_name: tdutils::http_url::get_url_file_name(&content_url),
                },
            ));

            content = Some(telegram_api::InputWebDocument {
                url: content_url,
                size: 0,
                mime_type: content_type,
                attributes,
            });
        }
        Ok(Some(
            telegram_api::InputBotInlineResult::inputBotInlineResult(
                telegram_api::inputBotInlineResult {
                    flags,
                    id,
                    r#type: result_type,
                    title,
                    description,
                    url,
                    thumb: thumbnail,
                    content,
                    send_message: Some(inline_message),
                },
            ),
        ))
    }
}

// ---------------------------------------------------------------------------
// the wire intake helpers
// ---------------------------------------------------------------------------

/// The `allowed_media_content_id` compare of the C++ (the
/// `constructor_id == allowed_media_content_id` arm of get_inline_message)
/// — the constructor ids the caller can pass, as a Rust-shaped marker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AllowedMediaContent {
    None, // the C++ -1
    Animation,
    Audio,
    Document,
    Photo,
    Sticker,
    Video,
    VoiceNote,
}

/// The `content->get_id() == allowed_media_content_id` test.
fn input_content_is(content: &td_api::InputMessageContent, allowed: AllowedMediaContent) -> bool {
    match allowed {
        AllowedMediaContent::None => false,
        AllowedMediaContent::Animation => {
            matches!(
                content,
                td_api::InputMessageContent::inputMessageAnimation(_)
            )
        }
        AllowedMediaContent::Audio => {
            matches!(content, td_api::InputMessageContent::inputMessageAudio(_))
        }
        AllowedMediaContent::Document => {
            matches!(
                content,
                td_api::InputMessageContent::inputMessageDocument(_)
            )
        }
        AllowedMediaContent::Photo => {
            matches!(content, td_api::InputMessageContent::inputMessagePhoto(_))
        }
        AllowedMediaContent::Sticker => {
            matches!(content, td_api::InputMessageContent::inputMessageSticker(_))
        }
        AllowedMediaContent::Video => {
            matches!(content, td_api::InputMessageContent::inputMessageVideo(_))
        }
        AllowedMediaContent::VoiceNote => {
            matches!(
                content,
                td_api::InputMessageContent::inputMessageVoiceNote(_)
            )
        }
    }
}

/// `extract_input_caption` (MessageContent.cpp:4940-4968) — the
/// caption-carrying input content subset the inline answers reach (the
/// M.9 lift covers the rest; `inputMessageSticker` carries no caption).
fn extract_input_caption(content: &td_api::InputMessageContent) -> Option<td_api::FormattedText> {
    match content {
        td_api::InputMessageContent::inputMessageAnimation(content) => content.caption.clone(),
        td_api::InputMessageContent::inputMessageAudio(content) => content.caption.clone(),
        td_api::InputMessageContent::inputMessageDocument(content) => content.caption.clone(),
        td_api::InputMessageContent::inputMessagePhoto(content) => content.caption.clone(),
        td_api::InputMessageContent::inputMessageVideo(content) => content.caption.clone(),
        td_api::InputMessageContent::inputMessageVoiceNote(content) => content.caption.clone(),
        _ => None,
    }
}

/// `extract_input_invert_media` (MessageContent.cpp:4970-4987) — the same
/// subset's `show_caption_above_media` flag.
fn extract_input_invert_media(content: &td_api::InputMessageContent) -> bool {
    match content {
        td_api::InputMessageContent::inputMessageAnimation(content) => {
            content.show_caption_above_media
        }
        td_api::InputMessageContent::inputMessageDocument(_content) => false,
        td_api::InputMessageContent::inputMessagePhoto(content) => content.show_caption_above_media,
        td_api::InputMessageContent::inputMessageVideo(content) => content.show_caption_above_media,
        _ => false,
    }
}

/// `on_get_weather` (InlineQueriesManager.cpp:1320-1333) — the weather
/// fold over the single-article result; the feed tail (the resolve →
/// send_inline_query chain that reaches it) is [blocked: dialog-plane
/// resolve] (see `resolve_weather_dialog`).
#[allow(dead_code)]
pub(crate) fn on_get_weather(
    results: td_api::InlineQueryResults,
) -> Result<td_api::CurrentWeather, Status> {
    if results.results.len() != 1 {
        return Err(Status::error(500, "Not supported"));
    }
    let td_api::InlineQueryResult::inlineQueryResultArticle(result) = &results.results[0] else {
        return Err(Status::error(500, "Not supported"));
    };
    if !tdutils::emoji::is_emoji(&result.title) {
        return Err(Status::error(500, "Not supported"));
    }
    Ok(td_api::CurrentWeather {
        temperature: crate::misc::to_double(&result.description),
        emoji: result.title.clone(),
    })
}

/// `full_split(Slice, char)` (tdutils/td/utils/misc.h:36-58) — the empty
/// parts kept, no trailing empty for an empty input.
fn full_split(value: &str, delimiter: char) -> Vec<&str> {
    if value.is_empty() {
        return Vec::new();
    }
    value.split(delimiter).collect()
}

/// `get_photo_size_object` (Photo.cpp:117-125) — the size record builder
/// the web-photo arm needs (the file-id half is the Files seam; the object
/// carries no file here).
fn get_photo_size_object(size: &crate::photo::PhotoSize) -> td_api::PhotoSize {
    td_api::PhotoSize {
        r#type: char::from_u32(u32::from(size.size_type as u8))
            .map(|c| c.to_string())
            .unwrap_or_default(),
        photo: None,
        width: i32::from(size.dimensions.width),
        height: i32::from(size.dimensions.height),
        progressive_sizes: size.progressive_sizes.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tdutils::promise::PromiseInterface;

    /// The recording env: the bot/user caches, the option rows and the
    /// fired intakes — the net handle stays `None` so the send legs park
    /// their promises in the family lots without dispatching (the
    /// [net-inert] posture; the netted_td round-trips drive the wire).
    struct TestEnv {
        is_bot: bool,
        bots: Vec<i64>,
        inline: Vec<i64>,
        known: Vec<i64>,
        use_chat_info_database: bool,
        option_strings: HashMap<String, String>,
        updates: Vec<String>,
        users_intakes: usize,
        bot_usages: Vec<i64>,
    }

    impl Default for TestEnv {
        fn default() -> Self {
            TestEnv {
                is_bot: false,
                bots: vec![9],
                inline: vec![9],
                known: vec![9],
                use_chat_info_database: true,
                option_strings: HashMap::new(),
                updates: Vec::new(),
                users_intakes: 0,
                bot_usages: Vec::new(),
            }
        }
    }

    impl TestEnv {
        fn bot_data(&self, user_id: UserId) -> Result<BotData, Status> {
            if !self.bots.contains(&user_id.get()) {
                return Err(Status::error(400, "Bot not found"));
            }
            Ok(BotData {
                username: "bot".to_owned(),
                is_inline: self.inline.contains(&user_id.get()),
                ..BotData::default()
            })
        }
    }

    #[allow(clippy::too_many_arguments)]
    impl InlineQueriesManagerEnv for TestEnv {
        fn auth_is_bot(&self) -> bool {
            self.is_bot
        }

        fn close_flag(&self) -> bool {
            false
        }

        fn use_chat_info_database(&self) -> bool {
            self.use_chat_info_database
        }

        fn is_test_dc(&self) -> bool {
            false
        }

        fn unix_time(&self) -> i32 {
            1700000000
        }

        fn get_option_string(&self, name: &str, default_value: &str) -> String {
            self.option_strings
                .get(name)
                .cloned()
                .unwrap_or_else(|| default_value.to_owned())
        }

        fn binlog_pmc_get(&mut self, key: &str) -> String {
            self.option_strings.get(key).cloned().unwrap_or_default()
        }

        fn binlog_pmc_set(&mut self, key: &str, value: &str) {
            self.option_strings.insert(key.to_owned(), value.to_owned());
        }

        fn send_update(&mut self, update: td_api::Update) {
            self.updates.push(format!("{:?}", update));
        }

        fn get_bot_data(&self, user_id: UserId) -> Result<BotData, Status> {
            self.bot_data(user_id)
        }

        fn have_min_user(&self, user_id: UserId) -> bool {
            self.known.contains(&user_id.get())
        }

        fn have_user_force(&self, user_id: UserId) -> bool {
            self.known.contains(&user_id.get())
        }

        fn get_user_first_username(&self, user_id: UserId) -> String {
            let _ = user_id;
            "bot".to_owned()
        }

        fn get_user_id_object(&mut self, user_id: UserId, _source: &str) -> i64 {
            user_id.get()
        }

        fn get_input_user(&mut self, user_id: UserId) -> Result<telegram_api::InputUser, Status> {
            if !self.known.contains(&user_id.get()) {
                return Err(Status::error(400, "User not found"));
            }
            Ok(telegram_api::InputUser::inputUser(
                telegram_api::inputUser {
                    user_id: user_id.get(),
                    access_hash: 0,
                },
            ))
        }

        fn on_get_users(&mut self, _users: Vec<telegram_api::User>, _source: &str) {
            self.users_intakes += 1;
        }

        fn get_input_peer(&mut self, _dialog_id: DialogId) -> Option<telegram_api::InputPeer> {
            Some(telegram_api::InputPeer::inputPeerEmpty(
                telegram_api::inputPeerEmpty {},
            ))
        }

        fn get_channel_type(&self, _channel_id: ChannelId) -> ChannelType {
            ChannelType::Broadcast
        }

        fn is_broadcast_channel(&self, _channel_id: ChannelId) -> bool {
            true
        }

        fn resolve_weather_dialog(
            &mut self,
            _username: &str,
            _location: Location,
            _promise: Promise<td_api::CurrentWeather>,
        ) {
            // [blocked: dialog-plane resolve]: the resolve never sends
        }

        fn can_add_web_page_previews(&self, _dialog_id: DialogId) -> bool {
            false
        }

        fn get_my_dialog_id(&self) -> DialogId {
            DialogId::new(1)
        }

        fn parse_formatted_text(
            &mut self,
            _dialog_id: DialogId,
            text: Option<td_api::FormattedText>,
            _is_bot: bool,
            _allow_empty: bool,
            _skip_media_timestamps: bool,
            _skip_trim: bool,
        ) -> Result<FormattedText, Status> {
            Ok(FormattedText {
                text: text.map(|text| text.text).unwrap_or_default(),
                ..FormattedText::default()
            })
        }

        fn get_input_message_entities(
            &mut self,
            _entities: &[crate::message_entity::MessageEntity],
            _source: &str,
        ) -> Vec<telegram_api::MessageEntity> {
            Vec::new()
        }

        fn get_input_keyboard_button(
            &mut self,
            button: &td_api::KeyboardButton,
        ) -> Result<telegram_api::KeyboardButton, Status> {
            crate::keyboard_button::KeyboardButton::from_td_api(button.clone(), true, false)
                .map(|button| button.get_input_keyboard_button())
        }

        fn get_input_reply_markup(
            &mut self,
            reply_markup: Option<td_api::ReplyMarkup>,
            is_bot: bool,
            switch_inline_buttons_allowed: bool,
        ) -> Result<Option<telegram_api::ReplyMarkup>, Status> {
            let markup = crate::reply_markup::get_inline_reply_markup(
                reply_markup,
                is_bot,
                switch_inline_buttons_allowed,
                false,
            )?;
            Ok(markup.and_then(|markup| crate::reply_markup::get_input_reply_markup(&markup)))
        }

        fn get_input_rich_message(
            &mut self,
            _message: &td_api::inputMessageRichMessage,
        ) -> Result<Option<telegram_api::InputRichMessage>, Status> {
            Ok(None) // [blocked: M.17]
        }

        fn get_input_bot_inline_message_media_invoice(
            &mut self,
            _invoice: &td_api::inputMessageInvoice,
            _reply_markup: Option<telegram_api::ReplyMarkup>,
        ) -> Result<Option<telegram_api::InputBotInlineMessage>, Status> {
            Ok(None) // [blocked: Wave-3 payments]
        }

        fn create_inline_message_content(
            &mut self,
            _file_id: FileId,
            _inline_message: Option<telegram_api::BotInlineMessage>,
            _allowed_media_content_id: i32,
        ) -> Option<InlineMessageContent> {
            None // [blocked: M.9]
        }

        fn register_thumbnail(
            &mut self,
            _web_document: telegram_api::WebDocument,
        ) -> Option<td_api::Thumbnail> {
            None // [blocked: Files]
        }

        fn get_web_document_temp_photo_size(
            &mut self,
            _web_document: telegram_api::WebDocument,
        ) -> Option<crate::photo::PhotoSize> {
            None // [blocked: Files]
        }

        fn get_web_document_thumbnail_photo_size(
            &mut self,
            _web_document: telegram_api::WebDocument,
        ) -> Option<crate::photo::PhotoSize> {
            None // [blocked: Files]
        }

        fn get_theme_parameters_json_string(
            &self,
            _theme: &td_api::ThemeParameters,
        ) -> Option<String> {
            None // [blocked: Wave-3 ThemeManager]
        }

        fn on_dialog_used(
            &mut self,
            _category: TopDialogCategory,
            dialog_id: DialogId,
            _unix_time: i32,
        ) {
            self.bot_usages.push(dialog_id.get());
        }
    }

    // --- the test promise helpers (the M.45 RecordingPromise convention) ---

    type Recorded<T> = std::sync::Arc<std::sync::Mutex<Vec<std::result::Result<T, String>>>>;

    struct RecordingPromise<T: Send + 'static> {
        outcomes: Recorded<T>,
    }

    impl<T: Send + 'static> PromiseInterface<T> for RecordingPromise<T> {
        fn set_value(&mut self, value: T) {
            self.outcomes.lock().unwrap().push(Ok(value));
        }

        fn set_error(&mut self, error: Status) {
            self.outcomes.lock().unwrap().push(Err(format!(
                "{} {}",
                error.code(),
                error.message()
            )));
        }
    }

    fn recording_promise<T: Send + 'static>() -> (Promise<T>, Recorded<T>) {
        let outcomes: Recorded<T> = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        (
            Promise::from_interface(RecordingPromise {
                outcomes: outcomes.clone(),
            }),
            outcomes,
        )
    }

    /// The net-inert park + FIFO pop band of the get-inline-bot-results
    /// family: the promise parks through the send leg (the handler's
    /// `promise_` member) and the pop pairs the answers FIFO.
    #[test]
    fn get_inline_bot_results_family_parks_and_pops_fifo() {
        let mut manager = InlineQueriesManager::new();
        assert!(manager.pop_get_inline_bot_results_promise().is_none());
        for index in 0..2 {
            let (promise, _outcomes) = recording_promise();
            manager.send_get_inline_bot_results_query(
                telegram_api::messages_getInlineBotResults {
                    flags: 0,
                    bot: None,
                    peer: None,
                    geo_point: None,
                    query: format!("q{index}"),
                    offset: String::new(),
                },
                DialogId::new(1),
                UserId::new(9),
                index as u64,
                promise,
            );
        }
        assert_eq!(manager.pending.get_inline_bot_results.len(), 2);
        assert!(manager.pop_get_inline_bot_results_promise().is_some());
        assert!(manager.pop_get_inline_bot_results_promise().is_some());
        assert!(manager.pop_get_inline_bot_results_promise().is_none());
    }

    /// The answer_inline_query bands over the bot env: the button
    /// validation ladder (cpp:705-737) answers inline, the good article
    /// parks the promise in the family lot and the game result's
    /// short-name form rides the inputBotInlineResultGame arm.
    #[test]
    fn answer_inline_query_bands_and_net_inert_park() {
        let mut manager = InlineQueriesManager::new();
        let mut env = TestEnv {
            is_bot: true,
            ..TestEnv::default()
        };
        let article = || {
            td_api::InputInlineQueryResult::inputInlineQueryResultArticle(
                td_api::inputInlineQueryResultArticle {
                    id: "r1".to_owned(),
                    url: String::new(),
                    title: "T".to_owned(),
                    description: String::new(),
                    thumbnail_url: String::new(),
                    thumbnail_width: 0,
                    thumbnail_height: 0,
                    reply_markup: None,
                    input_message_content: Some(Box::new(
                        td_api::InputMessageContent::inputMessageText(td_api::inputMessageText {
                            text: Some(td_api::FormattedText {
                                text: "hi".to_owned(),
                                entities: Vec::new(),
                            }),
                            link_preview_options: None,
                            clear_draft: false,
                        }),
                    )),
                },
            )
        };

        // the empty start_parameter band (cpp:712-714)
        let (promise, outcomes) = recording_promise::<()>();
        manager.answer_inline_query(
            &mut env,
            7,
            false,
            Some(td_api::InlineQueryResultsButton {
                text: "S".to_owned(),
                r#type: Some(
                    td_api::InlineQueryResultsButtonType::inlineQueryResultsButtonTypeStartBot(
                        td_api::inlineQueryResultsButtonTypeStartBot {
                            parameter: String::new(),
                        },
                    ),
                ),
            }),
            vec![article()],
            30,
            String::new(),
            promise,
        );
        assert!(matches!(
            outcomes.lock().unwrap().first(),
            Some(Err(message)) if message == "400 Can't use empty start_parameter"
        ));

        // the game result takes the inputBotInlineResultGame arm
        // (cpp:1013-1038) — no inline-message fold, no content fields
        let (promise, _outcomes) = recording_promise::<()>();
        manager.answer_inline_query(
            &mut env,
            7,
            false,
            None,
            vec![td_api::InputInlineQueryResult::inputInlineQueryResultGame(
                td_api::inputInlineQueryResultGame {
                    id: "g1".to_owned(),
                    game_short_name: "snake".to_owned(),
                    reply_markup: None,
                },
            )],
            30,
            String::new(),
            promise,
        );
        assert_eq!(
            manager.pending.set_inline_bot_results.len(),
            1,
            "the net-inert park"
        );
        assert!(
            manager.pop_set_inline_bot_results_promise().is_some(),
            "the FIFO pop"
        );
    }

    /// The answer_web_view_query bands: the folded result parks the
    /// promise in the web-view-result lot without a dispatch, and the
    /// seam-parked `Ok(None)` band answers nothing.
    #[test]
    fn answer_web_view_query_parks_net_inert() {
        let mut manager = InlineQueriesManager::new();
        let mut env = TestEnv {
            is_bot: true,
            ..TestEnv::default()
        };
        let (promise, outcomes) = recording_promise::<td_api::InlineMessageId>();
        manager.answer_web_view_query(
            &mut env,
            "waq-1",
            Some(td_api::InputInlineQueryResult::inputInlineQueryResultGame(
                td_api::inputInlineQueryResultGame {
                    id: "g1".to_owned(),
                    game_short_name: "snake".to_owned(),
                    reply_markup: None,
                },
            )),
            promise,
        );
        assert_eq!(manager.pending.send_web_view_result_message.len(), 1);
        assert!(outcomes.lock().unwrap().is_empty(), "the promise parks");
        assert!(manager.pop_send_web_view_result_message_promise().is_some());

        // the empty-result band (cpp:877)
        let (promise, outcomes) = recording_promise::<td_api::InlineMessageId>();
        manager.answer_web_view_query(&mut env, "waq-1", None, promise);
        assert!(matches!(
            outcomes.lock().unwrap().first(),
            Some(Err(message)) if message == "400 Inline query result must be non-empty"
        ));
    }

    /// The send_web_view_data bands: the not-a-bot band answers inline
    /// and the good form parks the promise net-inert.
    #[test]
    fn send_web_view_data_bands_and_net_inert_park() {
        let mut manager = InlineQueriesManager::new();
        let mut env = TestEnv::default();

        let (promise, outcomes) = recording_promise::<()>();
        manager.send_web_view_data(&mut env, UserId::new(9), "Open", "payload", promise);
        assert_eq!(manager.pending.send_web_view_data.len(), 1);
        assert!(outcomes.lock().unwrap().is_empty(), "the promise parks");
        assert!(manager.pop_send_web_view_data_promise().is_some());

        let mut env = TestEnv {
            bots: Vec::new(),
            ..TestEnv::default()
        };
        let (promise, outcomes) = recording_promise::<()>();
        manager.send_web_view_data(&mut env, UserId::new(9), "Open", "payload", promise);
        assert!(matches!(
            outcomes.lock().unwrap().first(),
            Some(Err(message)) if message == "400 Bot not found"
        ));
    }

    /// The get_simple_web_view_url bands: the URL ladder (cpp:359-374)
    /// answers inline, the good form parks the promise net-inert and the
    /// application name rides the platform field (cpp:378).
    #[test]
    fn get_simple_web_view_url_bands_and_net_inert_park() {
        let mut manager = InlineQueriesManager::new();
        let mut env = TestEnv::default();

        // the invalid-URL band (cpp:372-374)
        let (promise, outcomes) = recording_promise::<td_api::WebAppUrl>();
        manager.get_simple_web_view_url(
            &mut env,
            UserId::new(9),
            "https://app.example.com/no-suffix",
            WebAppOpenParameters::default(),
            promise,
        );
        assert!(matches!(
            outcomes.lock().unwrap().first(),
            Some(Err(message)) if message == "400 Invalid URL specified"
        ));

        // the `#iq` form parks net-inert with the platform name
        let (promise, outcomes) = recording_promise::<td_api::WebAppUrl>();
        manager.get_simple_web_view_url(
            &mut env,
            UserId::new(9),
            "https://app.example.com/y#iq",
            WebAppOpenParameters::from_td_api(Some(td_api::WebAppOpenParameters {
                theme: None,
                application_name: "tdlib-test".to_owned(),
                mode: None,
            })),
            promise,
        );
        assert_eq!(manager.pending.request_simple_web_view.len(), 1);
        assert!(outcomes.lock().unwrap().is_empty(), "the promise parks");
        assert!(manager.pop_request_simple_web_view_promise().is_some());

        // the not-a-bot band (cpp:851-853)
        let mut env = TestEnv {
            bots: Vec::new(),
            ..TestEnv::default()
        };
        let (promise, outcomes) = recording_promise::<td_api::WebAppUrl>();
        manager.get_simple_web_view_url(
            &mut env,
            UserId::new(9),
            "",
            WebAppOpenParameters::default(),
            promise,
        );
        assert!(matches!(
            outcomes.lock().unwrap().first(),
            Some(Err(message)) if message == "400 Bot not found"
        ));
    }

    /// The get_prepared_keyboard_button bands: the net-inert park keeps
    /// the prepared button id beside the promise (the handler's
    /// `prepared_button_id_` member) and the registered record answers
    /// `get_requested_dialog_type` (cpp:836-843).
    #[test]
    fn get_prepared_keyboard_button_bands_and_registration() {
        let mut manager = InlineQueriesManager::new();
        let mut env = TestEnv::default();
        let (promise, outcomes) = recording_promise::<td_api::KeyboardButton>();
        manager.get_prepared_keyboard_button(&mut env, UserId::new(9), "wrb-1", promise);
        assert_eq!(manager.pending.get_requested_web_view_button.len(), 1);
        assert_eq!(manager.pending.get_requested_web_view_button[0].0, "wrb-1");
        assert!(outcomes.lock().unwrap().is_empty(), "the promise parks");

        // the unknown-bot band (cpp:824-826)
        let (promise, outcomes) = recording_promise::<td_api::KeyboardButton>();
        manager.get_prepared_keyboard_button(&mut env, UserId::new(404), "wrb-1", promise);
        assert!(matches!(
            outcomes.lock().unwrap().first(),
            Some(Err(message)) if message == "400 User not found"
        ));

        // the registration + the lookup (cpp:828-843)
        let requested = RequestedDialogType::from_telegram(
            telegram_api::RequestPeerType::requestPeerTypeUser(telegram_api::requestPeerTypeUser {
                flags: 0,
                bot: false,
                premium: false,
            }),
            3,
            2,
        );
        manager.on_get_requested_web_view_button(UserId::new(9), "wrb-1", requested);
        assert!(
            manager
                .get_requested_dialog_type(UserId::new(9), "wrb-1")
                .is_some()
        );
        // the empty id and the other bot answer no
        assert!(
            manager
                .get_requested_dialog_type(UserId::new(9), "")
                .is_none()
        );
        assert!(
            manager
                .get_requested_dialog_type(UserId::new(8), "wrb-1")
                .is_none()
        );
        assert!(manager.pop_get_requested_web_view_button_query().is_some());
    }

    /// The save_prepared_keyboard_button bands: the button ladder (the
    /// M.22 get_keyboard_button(request_buttons_allowed=true) validation)
    /// answers inline, the good form parks the promise net-inert and the
    /// request-users button carries the button id through the fold.
    #[test]
    fn save_prepared_keyboard_button_bands_and_net_inert_park() {
        let mut manager = InlineQueriesManager::new();
        let mut env = TestEnv {
            is_bot: true,
            ..TestEnv::default()
        };

        // the empty-button band (cpp:815-817)
        let (promise, outcomes) = recording_promise::<String>();
        manager.save_prepared_keyboard_button(&mut env, UserId::new(9), None, promise);
        assert!(matches!(
            outcomes.lock().unwrap().first(),
            Some(Err(message)) if message == "400 Button must be non-empty"
        ));

        // the unknown-user band (cpp:812-814)
        let (promise, outcomes) = recording_promise::<String>();
        manager.save_prepared_keyboard_button(
            &mut env,
            UserId::new(404),
            Some(td_api::KeyboardButton {
                text: "Pick".to_owned(),
                icon_custom_emoji_id: 0,
                style: None,
                r#type: None,
            }),
            promise,
        );
        assert!(matches!(
            outcomes.lock().unwrap().first(),
            Some(Err(message)) if message == "400 User not found"
        ));

        // the good form parks net-inert
        let (promise, outcomes) = recording_promise::<String>();
        manager.save_prepared_keyboard_button(
            &mut env,
            UserId::new(9),
            Some(td_api::KeyboardButton {
                text: "Pick".to_owned(),
                icon_custom_emoji_id: 0,
                style: None,
                r#type: Some(td_api::KeyboardButtonType::keyboardButtonTypeRequestUsers(
                    td_api::keyboardButtonTypeRequestUsers {
                        id: 3,
                        restrict_user_is_bot: false,
                        user_is_bot: false,
                        restrict_user_is_premium: false,
                        user_is_premium: false,
                        max_quantity: 2,
                        request_name: false,
                        request_username: false,
                        request_photo: false,
                    },
                )),
            }),
            promise,
        );
        assert_eq!(manager.pending.save_prepared_keyboard_button.len(), 1);
        assert!(outcomes.lock().unwrap().is_empty(), "the promise parks");
        assert!(
            manager
                .pop_save_prepared_keyboard_button_promise()
                .is_some()
        );
    }

    /// The get_prepared_inline_message bands: the cache-hash ladder parks
    /// the promise per request (the C++ handler carries it on the query),
    /// the double call answers FIFO and the collision band fires when the
    /// hash names an inline-query entry.
    #[test]
    fn get_prepared_inline_message_bands_and_fifo() {
        let mut manager = InlineQueriesManager::new();
        let mut env = TestEnv::default();
        let (promise, outcomes) = recording_promise::<Option<td_api::PreparedInlineMessage>>();
        manager.get_prepared_inline_message(&mut env, UserId::new(9), "pm1", promise);
        assert_eq!(manager.pending.get_prepared_inline_message.len(), 1);
        assert!(outcomes.lock().unwrap().is_empty(), "the promise parks");

        // the second call for the same key increments the pending count
        // and parks another promise (cpp:794-804)
        let (promise, _outcomes) = recording_promise::<Option<td_api::PreparedInlineMessage>>();
        manager.get_prepared_inline_message(&mut env, UserId::new(9), "pm1", promise);
        assert_eq!(manager.pending.get_prepared_inline_message.len(), 2);
        assert!(manager.pop_get_prepared_inline_message_promise().is_some());
        assert!(manager.pop_get_prepared_inline_message_promise().is_some());
        assert!(manager.pop_get_prepared_inline_message_promise().is_none());

        // the unknown-bot band (cpp:785-789)
        let (promise, outcomes) = recording_promise::<Option<td_api::PreparedInlineMessage>>();
        manager.get_prepared_inline_message(&mut env, UserId::new(404), "pm1", promise);
        assert!(matches!(
            outcomes.lock().unwrap().first(),
            Some(Err(message)) if message == "400 User not found"
        ));
    }

    /// The save_prepared_inline_message bands: the chat-types ladder
    /// (TargetDialogTypes.cpp:43-59) answers inline and the good form
    /// parks the promise in the prepared-message lot without a dispatch.
    #[test]
    fn save_prepared_inline_message_bands_and_net_inert_park() {
        let mut manager = InlineQueriesManager::new();
        let mut env = TestEnv {
            is_bot: true,
            ..TestEnv::default()
        };
        let result = td_api::InputInlineQueryResult::inputInlineQueryResultGame(
            td_api::inputInlineQueryResultGame {
                id: "g1".to_owned(),
                game_short_name: "snake".to_owned(),
                reply_markup: None,
            },
        );

        // the empty chat-types band
        let (promise, outcomes) = recording_promise::<td_api::PreparedInlineMessageId>();
        manager.save_prepared_inline_message(
            &mut env,
            UserId::new(9),
            Some(result.clone()),
            None,
            promise,
        );
        assert!(matches!(
            outcomes.lock().unwrap().first(),
            Some(Err(message)) if message == "400 At least one chat type must be allowed"
        ));

        // the unknown-user band (cpp:773-776)
        let (promise, outcomes) = recording_promise::<td_api::PreparedInlineMessageId>();
        manager.save_prepared_inline_message(
            &mut env,
            UserId::new(404),
            Some(result),
            Some(td_api::TargetChatTypes {
                allow_user_chats: true,
                allow_bot_chats: false,
                allow_group_chats: false,
                allow_channel_chats: false,
            }),
            promise,
        );
        assert!(matches!(
            outcomes.lock().unwrap().first(),
            Some(Err(message)) if message == "400 User not found"
        ));

        // the good form parks net-inert
        let (promise, outcomes) = recording_promise::<td_api::PreparedInlineMessageId>();
        manager.save_prepared_inline_message(
            &mut env,
            UserId::new(9),
            Some(
                td_api::InputInlineQueryResult::inputInlineQueryResultArticle(
                    td_api::inputInlineQueryResultArticle {
                        id: "p1".to_owned(),
                        url: String::new(),
                        title: "T".to_owned(),
                        description: String::new(),
                        thumbnail_url: String::new(),
                        thumbnail_width: 0,
                        thumbnail_height: 0,
                        reply_markup: None,
                        input_message_content: Some(Box::new(
                            td_api::InputMessageContent::inputMessageText(
                                td_api::inputMessageText {
                                    text: Some(td_api::FormattedText {
                                        text: "hi".to_owned(),
                                        entities: Vec::new(),
                                    }),
                                    link_preview_options: None,
                                    clear_draft: false,
                                },
                            ),
                        )),
                    },
                ),
            ),
            Some(td_api::TargetChatTypes {
                allow_user_chats: true,
                allow_bot_chats: false,
                allow_group_chats: false,
                allow_channel_chats: false,
            }),
            promise,
        );
        assert_eq!(manager.pending.save_prepared_inline_message.len(), 1);
        assert!(outcomes.lock().unwrap().is_empty(), "the promise parks");
        assert!(manager.pop_save_prepared_inline_message_promise().is_some());
    }

    /// The answer_guest_query net-inert park band: the folded result
    /// parks the promise in the guest-chat lot without a dispatch, and
    /// the seam-parked `Ok(None)` band (the [blocked: M.17] rich message)
    /// answers nothing (cpp:764-768).
    #[test]
    fn answer_guest_query_parks_net_inert() {
        let mut manager = InlineQueriesManager::new();
        let mut env = TestEnv {
            is_bot: true,
            ..TestEnv::default()
        };
        let (promise, outcomes) = recording_promise::<td_api::InlineMessageId>();
        manager.answer_guest_query(
            &mut env,
            4242,
            Some(td_api::InputInlineQueryResult::inputInlineQueryResultGame(
                td_api::inputInlineQueryResultGame {
                    id: "g1".to_owned(),
                    game_short_name: "snake".to_owned(),
                    reply_markup: None,
                },
            )),
            promise,
        );
        assert_eq!(manager.pending.set_bot_guest_chat_result.len(), 1);
        assert!(outcomes.lock().unwrap().is_empty(), "the promise parks");
        assert!(manager.pop_set_bot_guest_chat_result_promise().is_some());

        // the error band of the fold (cpp:767) — the bad phone number
        let (promise, outcomes) = recording_promise::<td_api::InlineMessageId>();
        manager.answer_guest_query(
            &mut env,
            4242,
            Some(
                td_api::InputInlineQueryResult::inputInlineQueryResultContact(
                    td_api::inputInlineQueryResultContact {
                        id: "c1".to_owned(),
                        contact: Some(td_api::Contact {
                            phone_number: " ".to_owned(),
                            first_name: " ".to_owned(),
                            last_name: String::new(),
                            vcard: String::new(),
                            user_id: 0,
                        }),
                        thumbnail_url: String::new(),
                        thumbnail_width: 0,
                        thumbnail_height: 0,
                        reply_markup: None,
                        input_message_content: None,
                    },
                ),
            ),
            promise,
        );
        assert!(matches!(
            outcomes.lock().unwrap().first(),
            Some(Err(message)) if message == "400 Field \"phone_number\" must contain a valid phone number"
        ));
    }

    /// The net-inert band of the whole send_inline_query ladder: the bot
    /// gates pass (the bot data read, the peer resolve, the input-user
    /// fetch), the query hash lands in the cache and the request promise
    /// parks in the family lot — nothing dispatches and nothing answers
    /// (the C++ request without a network).
    #[test]
    fn send_inline_query_parks_net_inert() {
        let mut manager = InlineQueriesManager::new();
        let mut env = TestEnv::default();
        let (promise, outcomes) = recording_promise::<Option<td_api::InlineQueryResults>>();
        manager.send_inline_query(
            &mut env,
            UserId::new(9),
            DialogId::new(0),
            Location::default(),
            "weather",
            "",
            promise,
        );
        assert_eq!(manager.pending.get_inline_bot_results.len(), 1);
        assert!(outcomes.lock().unwrap().is_empty(), "the promise parks");
        assert!(manager.next_inline_query_time > 0.0);

        // the not-inline bot band (cpp:1354-1356)
        let mut env = TestEnv {
            inline: Vec::new(),
            ..TestEnv::default()
        };
        let (promise, outcomes) = recording_promise::<Option<td_api::InlineQueryResults>>();
        manager.send_inline_query(
            &mut env,
            UserId::new(9),
            DialogId::new(0),
            Location::default(),
            "weather",
            "",
            promise,
        );
        assert!(matches!(
            outcomes.lock().unwrap().first(),
            Some(Err(message)) if message == "400 Bot doesn't support inline queries"
        ));
    }
}
