//! Port of `td/telegram/InlineMessageManager.{h,cpp}` (M.21) — the inline
//! message id decode plane plus the edit-inline-message request family.
//!
//! The manager is the collapsed actor: `Td` owns the struct, the
//! [`InlineMessageManagerEnv`] trait carries the `Td *td` surfaces, and
//! the three edit/score query families ride the net-drain 61
//! `InlineMessageNetKind` plane — the env-carried handle dispatches the
//! wire queries (each on the inline message's own DC) and the answers
//! drain through `crate::td::Td::on_inline_message_net_answer`; the
//! C++ promise lands in the query and parks in the Td-side family lots
//! (§5's FIFO shape). The id-decode helpers mirror
//! the C++ file split: the encode half
//! ([`crate::inline_queries_manager::get_inline_message_id`]) lives in the
//! InlineQueriesManager port, this module owns the decode pair — the byte
//! format is the bare TL store (fields, no constructor id) dispatched on
//! the 20-vs-24-byte length, URL-safe base64 without padding.
//!
//! The edit arms keep their C++ validation order; the content conversions
//! ride the landed planes where they exist (the text parse and the live
//! location record) and park the request at the `[blocked: M.9/M.10/M.17]`
//! seams otherwise (`edit_inline_message_media`'s
//! `get_input_message_content`/`get_message_content_input_media` pair, the
//! rich-message arm). The `get_game_high_scores_object` response build is
//! the Wave-3 GameManager's — unreachable offline, the net seam drops the
//! promise.

use crate::inline_queries_manager::{InlineQueriesManagerEnv, process_input_message_text};
use crate::td_api;
use crate::user_manager::UserId;
use tdmtproto::telegram_api;
use tdutils::promise::Promise;
use tdutils::status::Status;

/// The in-flight query kinds of the net-drain 61 inline-message
/// families — the C++ `Td::ResultHandler` query classes
/// (InlineMessageManager.cpp:87-210) collapsed onto dispatcher tags; the
/// answers drain through
/// [`crate::td::Td::on_inline_message_net_answer`] over the
/// env-carried [`crate::td::InlineMessageNetHandle`]. The handlers
/// carry only their `promise_` members (cpp:88/144/181), so the lots
/// park the promises alone (§5's FIFO shape); the send legs route each
/// query to `DcId::internal(get_inline_message_dc_id(...))`
/// (cpp:121/168/206).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InlineMessageNetKind {
    /// `EditInlineMessageQuery` (InlineMessageManager.cpp:87-140) —
    /// `messages.editInlineBotMessage`.
    EditInlineMessage,
    /// `SetInlineGameScoreQuery` (InlineMessageManager.cpp:143-177) —
    /// `messages.setInlineGameScore`.
    SetInlineGameScore,
    /// `GetInlineGameHighScoresQuery` (InlineMessageManager.cpp:179-210)
    /// — `messages.getInlineGameHighScores`.
    GetInlineGameHighScores,
    /// `EditInlineMessageWidgetDataQuery` (InlineMessageManager.cpp:
    /// 131-172, the 1.8.70 widgets family) —
    /// `messages.editInlineBotMessageWidgetData`.
    EditInlineMessageWidgetData,
}

/// The parked inline-message queries (the net-drain 61 families): the
/// C++ handlers own their promises; the port parks them at the Td-side
/// lots until the matching `InlineMessageNetKind` answer drains.
#[derive(Default)]
pub(crate) struct PendingInlineMessageQueries {
    /// The parked `messages.editInlineBotMessage` promises.
    pub(crate) edits: Vec<Promise<()>>,
    /// The parked `messages.setInlineGameScore` promises.
    pub(crate) scores: Vec<Promise<()>>,
    /// The parked `messages.getInlineGameHighScores` promises.
    pub(crate) high_scores: Vec<Promise<td_api::GameHighScores>>,
    /// The parked `messages.editInlineBotMessageWidgetData` promises
    /// (the 1.8.70 widgets family).
    pub(crate) widget_edits: Vec<Promise<()>>,
}

/// `DcId::MAX_RAW_DC_ID` (net/DcId.h:30).
const MAX_RAW_DC_ID: i32 = 1000;

/// `DcId::is_valid` (net/DcId.h:31-33).
fn is_valid_dc_id(dc_id: i32) -> bool {
    (1..=MAX_RAW_DC_ID).contains(&dc_id)
}

/// `get_inline_message_dc_id` (InlineMessageManager.cpp:36-48).
pub fn get_inline_message_dc_id(inline_message_id: &telegram_api::InputBotInlineMessageID) -> i32 {
    match inline_message_id {
        telegram_api::InputBotInlineMessageID::inputBotInlineMessageID(id) => id.dc_id,
        telegram_api::InputBotInlineMessageID::inputBotInlineMessageID64(id) => id.dc_id,
    }
}

/// `parse_input_bot_inline_message_id` (InlineMessageManager.cpp:50-69) —
/// `base64url_decode`, then the 20-vs-24-byte constructor dispatch over
/// the bare field store; `parser.fetch_end()` is the consumed-bytes check.
pub fn parse_input_bot_inline_message_id(
    inline_message_id: &str,
) -> Option<telegram_api::InputBotInlineMessageID> {
    let binary = match tdmtproto::utils::base64url_decode(inline_message_id.as_bytes()) {
        tdutils::status::Result::Ok(binary) => binary,
        tdutils::status::Result::Err(_) => return None,
    };
    let mut reader = telegram_api::tl::Reader::new(&binary);
    let result = if binary.len() == 20 {
        match telegram_api::inputBotInlineMessageID::read_bare(&mut reader) {
            Ok(id) => telegram_api::InputBotInlineMessageID::inputBotInlineMessageID(id),
            Err(_) => return None,
        }
    } else {
        match telegram_api::inputBotInlineMessageID64::read_bare(&mut reader) {
            Ok(id) => telegram_api::InputBotInlineMessageID::inputBotInlineMessageID64(id),
            Err(_) => return None,
        }
    };
    if reader.remaining() != 0 {
        // the C++ parser.fetch_end() error band
        return None;
    }
    if !is_valid_dc_id(get_inline_message_dc_id(&result)) {
        return None;
    }
    Some(result)
}

/// `get_input_bot_inline_message_id` (InlineMessageManager.cpp:71-78).
pub fn get_input_bot_inline_message_id(
    inline_message_id: &str,
) -> Result<telegram_api::InputBotInlineMessageID, Status> {
    let Some(result) = parse_input_bot_inline_message_id(inline_message_id) else {
        return Err(Status::error(
            400,
            "Invalid inline message identifier specified",
        ));
    };
    Ok(result)
}

/// The `Td *td` slice of the collapsed InlineMessageManager (over the
/// shared InlineQueriesManager surfaces).
pub trait InlineMessageManagerEnv: InlineQueriesManagerEnv {
    // -- the net-drain 61 query send arms (the td.rs overrides
    // -- dispatch through the InlineMessageNetKind plane; the defaults
    // -- keep the offline unit-test posture — each one drops the
    // -- promise, which keeps the C++ request unanswered offline)
    /// `EditInlineMessageQuery::send` (InlineMessageManager.cpp:87-124) —
    /// the manager builds the full TL query (flags fold included); the
    /// `CHECK(!FileManager::extract_was_uploaded(input_media))` and the
    /// `DcId::internal(get_inline_message_dc_id(...))` routing are the
    /// net-step's business.
    fn send_edit_inline_message_query(
        &mut self,
        query: telegram_api::messages_editInlineBotMessage,
        promise: Promise<()>,
    );
    /// `SetInlineGameScoreQuery` (InlineMessageManager.cpp:143-177).
    fn send_set_inline_game_score_query(
        &mut self,
        query: telegram_api::messages_setInlineGameScore,
        promise: Promise<()>,
    );
    /// `GetInlineGameHighScoresQuery` (InlineMessageManager.cpp:179-210);
    /// the response lands in the (Wave-3) GameManager object builder.
    fn send_get_inline_game_high_scores_query(
        &mut self,
        query: telegram_api::messages_getInlineGameHighScores,
        promise: Promise<td_api::GameHighScores>,
    );
    /// `EditInlineMessageWidgetDataQuery` (InlineMessageManager.cpp:
    /// 131-172, the 1.8.70 widgets family) — the data-only edit rides
    /// the same per-DC routing.
    fn send_edit_inline_message_widget_data_query(
        &mut self,
        query: telegram_api::messages_editInlineBotMessageWidgetData,
        promise: Promise<()>,
    );
    /// `get_input_message_content` + `get_message_content_input_media` +
    /// `get_message_content_caption` (MessageContent.{h,cpp}) —
    /// `[blocked: M.9/M.10]`; `Ok(None)` parks the caller's request.
    fn get_input_media_for_edit(
        &mut self,
        input_message_content: Box<td_api::InputMessageContent>,
    ) -> Result<Option<MediaEditPiece>, Status>;
}

/// The `content`/`ttl`/`invert_media`/`media`/`caption` fold of
/// `edit_inline_message_media` (InlineMessageManager.cpp:297-306) — the
/// `[blocked: M.9/M.10]` seam output record.
pub struct MediaEditPiece {
    /// `is_editable_media_message_content(content.content->get_type())`.
    pub is_editable_media: bool,
    /// `!content.ttl.is_empty()`.
    pub has_ttl: bool,
    /// `get_message_content_input_media(...).media_`.
    pub media: Option<telegram_api::InputMedia>,
    /// `get_message_content_caption(content.content.get())`.
    pub caption: Option<String>,
    /// `content.invert_media`.
    pub invert_media: bool,
    /// the caption entity list (`get_input_message_entities` runs at the
    /// seam, over the user-manager surface).
    pub caption_entities: Vec<telegram_api::MessageEntity>,
}

/// `InlineMessageManager` (InlineMessageManager.h:21-59) — the collapsed
/// actor; the C++ manager carries no fields beyond `td_`/`parent_`.
#[derive(Default)]
pub struct InlineMessageManager {}

impl std::fmt::Debug for InlineMessageManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InlineMessageManager").finish()
    }
}

/// The `EditInlineMessageQuery::send` flags fold
/// (InlineMessageManager.cpp:87-124), shared by the edit arms.
#[allow(clippy::too_many_arguments)]
fn build_edit_inline_bot_message(
    env: &mut impl InlineMessageManagerEnv,
    input_bot_inline_message_id: telegram_api::InputBotInlineMessageID,
    force_edit_text: bool,
    text: Option<&crate::message_entity::FormattedText>,
    disable_web_page_preview: bool,
    media: Option<telegram_api::InputMedia>,
    invert_media: bool,
    reply_markup: Option<telegram_api::ReplyMarkup>,
    rich_message: Option<telegram_api::InputRichMessage>,
) -> telegram_api::messages_editInlineBotMessage {
    let mut flags = 0;
    let input_reply_markup = reply_markup;
    if input_reply_markup.is_some() {
        flags |= 1 << 2; // REPLY_MARKUP_MASK (flags.2)
    }
    let mut entities = Vec::new();
    if force_edit_text || text.is_some_and(|text| !text.text.is_empty()) {
        flags |= 1 << 11; // MESSAGE_MASK (flags.11)

        if let Some(text) = text {
            entities = env.get_input_message_entities(&text.entities, "EditInlineMessageQuery");
        }
        if !entities.is_empty() {
            flags |= 1 << 3; // ENTITIES_MASK (flags.3)
        }
    }
    if media.is_some() {
        flags |= 1 << 14; // MEDIA_MASK (flags.14)
    }
    if rich_message.is_some() {
        flags |= 1 << 22; // RICH_MESSAGE_MASK (flags.23)
    }

    telegram_api::messages_editInlineBotMessage {
        flags,
        no_webpage: disable_web_page_preview,
        invert_media,
        id: Some(input_bot_inline_message_id),
        message: text.map(|text| text.text.clone()).unwrap_or_default(),
        media: media.map(Box::new),
        reply_markup: input_reply_markup,
        entities,
        rich_message,
    }
}

impl InlineMessageManager {
    pub fn new() -> InlineMessageManager {
        InlineMessageManager::default()
    }

    /// `edit_inline_message_text` (InlineMessageManager.cpp:219-261).
    pub fn edit_inline_message_text(
        &mut self,
        env: &mut impl InlineMessageManagerEnv,
        inline_message_id: &str,
        reply_markup: Option<td_api::ReplyMarkup>,
        input_message_content: Option<Box<td_api::InputMessageContent>>,
        mut promise: Promise<()>,
    ) {
        let is_bot = env.auth_is_bot();
        debug_assert!(is_bot, "edit_inline_message_text is bot-only");

        let Some(input_message_content) = input_message_content else {
            promise.set_error(Status::error(400, "Can't edit message without new content"));
            return;
        };
        if !matches!(
            input_message_content.as_ref(),
            td_api::InputMessageContent::inputMessageText(_)
                | td_api::InputMessageContent::inputMessageRichMessage(_)
        ) {
            promise.set_error(Status::error(
                400,
                "Input message content type must be inputMessageText or inputMessageRichMessage",
            ));
            return;
        }

        let is_bot = env.auth_is_bot();
        let new_reply_markup = match env.get_input_reply_markup(reply_markup, is_bot, true) {
            Ok(reply_markup) => reply_markup,
            Err(status) => {
                promise.set_error(status);
                return;
            }
        };
        let input_bot_inline_message_id = match get_input_bot_inline_message_id(inline_message_id) {
            Ok(id) => id,
            Err(status) => {
                promise.set_error(status);
                return;
            }
        };

        if let td_api::InputMessageContent::inputMessageRichMessage(rich_message_input) =
            input_message_content.as_ref()
        {
            let input_rich_message = match env.get_input_rich_message(rich_message_input) {
                Ok(Some(input_rich_message)) => input_rich_message,
                Ok(None) => return, // the [blocked: M.17] seam parked the request
                Err(status) => {
                    promise.set_error(status);
                    return;
                }
            };
            let query = build_edit_inline_bot_message(
                env,
                input_bot_inline_message_id,
                false,
                None,
                false,
                None,
                false,
                new_reply_markup,
                Some(input_rich_message),
            );
            env.send_edit_inline_message_query(query, promise);
            return;
        }

        let input_message_text = match process_input_message_text(
            env,
            env.get_my_dialog_id(),
            &input_message_content,
            is_bot,
            false,
        ) {
            Ok(text) => text,
            Err(status) => {
                promise.set_error(status);
                return;
            }
        };
        let input_media_web_page = input_message_text.get_input_media_web_page();
        let query = build_edit_inline_bot_message(
            env,
            input_bot_inline_message_id,
            true,
            Some(&input_message_text.text),
            input_message_text.disable_web_page_preview,
            input_media_web_page.map(telegram_api::InputMedia::inputMediaWebPage),
            input_message_text.show_above_text,
            new_reply_markup,
            None,
        );
        env.send_edit_inline_message_query(query, promise);
    }

    /// `edit_inline_message_live_location` (InlineMessageManager.cpp:263-277).
    pub fn edit_inline_message_live_location(
        &mut self,
        env: &mut impl InlineMessageManagerEnv,
        inline_message_id: &str,
        reply_markup: Option<td_api::ReplyMarkup>,
        input_location: Option<td_api::LiveLocation>,
        mut promise: Promise<()>,
    ) {
        debug_assert!(
            env.auth_is_bot(),
            "edit_inline_message_live_location is bot-only"
        );

        let is_bot = env.auth_is_bot();
        let new_reply_markup = match env.get_input_reply_markup(reply_markup, is_bot, true) {
            Ok(reply_markup) => reply_markup,
            Err(status) => {
                promise.set_error(status);
                return;
            }
        };
        let input_bot_inline_message_id = match get_input_bot_inline_message_id(inline_message_id) {
            Ok(id) => id,
            Err(status) => {
                promise.set_error(status);
                return;
            }
        };
        let location =
            match crate::inline_queries_manager::process_live_location(input_location, true) {
                Ok(location) => location,
                Err(status) => {
                    promise.set_error(status);
                    return;
                }
            };

        // `InputMedia(location.get_input_media_geo_live())`
        // (Location.cpp:146-156)
        let mut flags = 1 << 3; // PROXIMITY_NOTIFICATION_RADIUS_MASK (flags.3), always
        if location.live_period != 0 {
            flags |= 1 << 2; // PERIOD_MASK (flags.2)
        }
        if location.heading != 0 {
            flags |= 1 << 1; // HEADING_MASK (flags.1)
        }
        let media = telegram_api::InputMedia::inputMediaGeoLive(telegram_api::inputMediaGeoLive {
            flags,
            stopped: false,
            geo_point: Some(location.location.get_input_geo_point()),
            heading: location.heading,
            period: location.live_period,
            proximity_notification_radius: location.proximity_alert_radius,
        });

        let query = build_edit_inline_bot_message(
            env,
            input_bot_inline_message_id,
            false,
            None,
            false,
            Some(media),
            false, // ignored, like the C++
            new_reply_markup,
            None,
        );
        env.send_edit_inline_message_query(query, promise);
    }

    /// `edit_inline_message_media` (InlineMessageManager.cpp:279-307) —
    /// the content conversion rides the `[blocked: M.9/M.10]` seam; the
    /// C++ validation order is kept.
    pub fn edit_inline_message_media(
        &mut self,
        env: &mut impl InlineMessageManagerEnv,
        inline_message_id: &str,
        reply_markup: Option<td_api::ReplyMarkup>,
        input_message_content: Option<Box<td_api::InputMessageContent>>,
        mut promise: Promise<()>,
    ) {
        debug_assert!(env.auth_is_bot(), "edit_inline_message_media is bot-only");

        let Some(input_message_content) = input_message_content else {
            // the C++ get_input_message_content(nullptr) band
            promise.set_error(Status::error(400, "Message content must be non-empty"));
            return;
        };
        let piece = match env.get_input_media_for_edit(input_message_content) {
            Ok(Some(piece)) => piece,
            Ok(None) => return, // the seam parked the request
            Err(status) => {
                promise.set_error(status);
                return;
            }
        };
        if !piece.is_editable_media {
            // 2d108dde5 renamed the error
            promise.set_error(Status::error(400, "Unallowed message content specified"));
            return;
        }
        if piece.has_ttl {
            promise.set_error(Status::error(
                400,
                "Can't enable self-destruction for media",
            ));
            return;
        }

        let is_bot = env.auth_is_bot();
        let new_reply_markup = match env.get_input_reply_markup(reply_markup, is_bot, true) {
            Ok(reply_markup) => reply_markup,
            Err(status) => {
                promise.set_error(status);
                return;
            }
        };
        let input_bot_inline_message_id = match get_input_bot_inline_message_id(inline_message_id) {
            Ok(id) => id,
            Err(status) => {
                promise.set_error(status);
                return;
            }
        };

        let Some(media) = piece.media else {
            promise.set_error(Status::error(400, "Invalid message content specified"));
            return;
        };

        let caption_text = piece
            .caption
            .map(|text| crate::message_entity::FormattedText {
                text,
                entities: Vec::new(),
            });
        let query = build_edit_inline_bot_message(
            env,
            input_bot_inline_message_id,
            true,
            caption_text.as_ref(),
            false,
            Some(media),
            piece.invert_media,
            new_reply_markup,
            None,
        );
        env.send_edit_inline_message_query(query, promise);
    }

    /// `edit_inline_message_caption` (InlineMessageManager.cpp:309-325).
    pub fn edit_inline_message_caption(
        &mut self,
        env: &mut impl InlineMessageManagerEnv,
        inline_message_id: &str,
        reply_markup: Option<td_api::ReplyMarkup>,
        input_caption: Option<td_api::FormattedText>,
        invert_media: bool,
        mut promise: Promise<()>,
    ) {
        debug_assert!(env.auth_is_bot(), "edit_inline_message_caption is bot-only");

        let caption = match env.parse_formatted_text(
            env.get_my_dialog_id(),
            input_caption,
            env.auth_is_bot(),
            true,
            false,
            false,
        ) {
            Ok(caption) => caption,
            Err(status) => {
                promise.set_error(status);
                return;
            }
        };
        let is_bot = env.auth_is_bot();
        let new_reply_markup = match env.get_input_reply_markup(reply_markup, is_bot, true) {
            Ok(reply_markup) => reply_markup,
            Err(status) => {
                promise.set_error(status);
                return;
            }
        };
        let input_bot_inline_message_id = match get_input_bot_inline_message_id(inline_message_id) {
            Ok(id) => id,
            Err(status) => {
                promise.set_error(status);
                return;
            }
        };

        let query = build_edit_inline_bot_message(
            env,
            input_bot_inline_message_id,
            true,
            Some(&caption),
            false,
            None,
            invert_media,
            new_reply_markup,
            None,
        );
        env.send_edit_inline_message_query(query, promise);
    }

    /// `edit_inline_message_reply_markup` (InlineMessageManager.cpp:327-339).
    pub fn edit_inline_message_reply_markup(
        &mut self,
        env: &mut impl InlineMessageManagerEnv,
        inline_message_id: &str,
        reply_markup: Option<td_api::ReplyMarkup>,
        mut promise: Promise<()>,
    ) {
        debug_assert!(
            env.auth_is_bot(),
            "edit_inline_message_reply_markup is bot-only"
        );

        let is_bot = env.auth_is_bot();
        let new_reply_markup = match env.get_input_reply_markup(reply_markup, is_bot, true) {
            Ok(reply_markup) => reply_markup,
            Err(status) => {
                promise.set_error(status);
                return;
            }
        };
        let input_bot_inline_message_id = match get_input_bot_inline_message_id(inline_message_id) {
            Ok(id) => id,
            Err(status) => {
                promise.set_error(status);
                return;
            }
        };

        let query = build_edit_inline_bot_message(
            env,
            input_bot_inline_message_id,
            false,
            None,
            false,
            None,
            false, // ignored, like the C++
            new_reply_markup,
            None,
        );
        env.send_edit_inline_message_query(query, promise);
    }

    /// `InlineMessageManager::edit_inline_message_widget_data`
    /// (InlineMessageManager.cpp:372-384) — the 1.8.70 widgets family:
    /// the data-only edit, the both-flags-false band answers immediately.
    #[allow(clippy::too_many_arguments)] // the C++ signature has the same shape
    pub fn edit_inline_message_widget_data(
        &mut self,
        env: &mut impl InlineMessageManagerEnv,
        inline_message_id: &str,
        edit_public_data: bool,
        public_data: &str,
        edit_chat_data: bool,
        chat_data: &str,
        mut promise: Promise<()>,
    ) {
        debug_assert!(
            env.auth_is_bot(),
            "edit_inline_message_widget_data is bot-only"
        );

        let input_bot_inline_message_id = match get_input_bot_inline_message_id(inline_message_id) {
            Ok(id) => id,
            Err(status) => {
                promise.set_error(status);
                return;
            }
        };
        if !edit_public_data && !edit_chat_data {
            return promise.set_value(());
        }

        let mut flags = 0;
        if edit_public_data {
            flags |= 1 << 0; // PUBLIC_DATA_MASK
        }
        if edit_chat_data {
            flags |= 1 << 1; // CHAT_DATA_MASK
        }
        let query = telegram_api::messages_editInlineBotMessageWidgetData {
            flags,
            id: Some(input_bot_inline_message_id),
            public_data: if edit_public_data {
                public_data.to_owned()
            } else {
                String::new()
            },
            chat_data: if edit_chat_data {
                chat_data.to_owned()
            } else {
                String::new()
            },
        };
        env.send_edit_inline_message_widget_data_query(query, promise);
    }

    /// `set_inline_game_score` (InlineMessageManager.cpp:341-350).
    #[allow(clippy::too_many_arguments)]
    pub fn set_inline_game_score(
        &mut self,
        env: &mut impl InlineMessageManagerEnv,
        inline_message_id: &str,
        edit_message: bool,
        user_id: UserId,
        score: i32,
        force: bool,
        mut promise: Promise<()>,
    ) {
        debug_assert!(env.auth_is_bot(), "set_inline_game_score is bot-only");

        let input_bot_inline_message_id = match get_input_bot_inline_message_id(inline_message_id) {
            Ok(id) => id,
            Err(status) => {
                promise.set_error(status);
                return;
            }
        };
        let input_user = match env.get_input_user(user_id) {
            Ok(input_user) => input_user,
            Err(status) => {
                promise.set_error(status);
                return;
            }
        };

        let query = telegram_api::messages_setInlineGameScore {
            flags: 0,
            edit_message,
            force,
            id: Some(input_bot_inline_message_id),
            user_id: Some(input_user),
            score,
        };
        env.send_set_inline_game_score_query(query, promise);
    }

    /// `get_inline_game_high_scores` (InlineMessageManager.cpp:352-361).
    pub fn get_inline_game_high_scores(
        &mut self,
        env: &mut impl InlineMessageManagerEnv,
        inline_message_id: &str,
        user_id: UserId,
        mut promise: Promise<td_api::GameHighScores>,
    ) {
        debug_assert!(env.auth_is_bot(), "get_inline_game_high_scores is bot-only");

        let input_bot_inline_message_id = match get_input_bot_inline_message_id(inline_message_id) {
            Ok(id) => id,
            Err(status) => {
                promise.set_error(status);
                return;
            }
        };
        let input_user = match env.get_input_user(user_id) {
            Ok(input_user) => input_user,
            Err(status) => {
                promise.set_error(status);
                return;
            }
        };

        let query = telegram_api::messages_getInlineGameHighScores {
            id: Some(input_bot_inline_message_id),
            user_id: Some(input_user),
        };
        env.send_get_inline_game_high_scores_query(query, promise);
    }
}
