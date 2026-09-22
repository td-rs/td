//! Port of `td/telegram/PollManager.{h,cpp,hpp}`, `PollOption.{h,cpp,hpp}`
//! and `PollId.h` (M.19): the poll id plane, the `PollOption`/`Poll` records
//! with their byte-exact db store/parse, the quiz-correct-options checker,
//! the vote-percentage rounding machine, and (slice M.19.2) the collapsed
//! `PollManager` — the poll state map, the voter lists, the
//! register/unregister and reload/close/unload timeout machinery, the
//! `on_get_poll` wire intake and the td_api object builders.
//!
//! The manager is the collapsed actor: `Td` owns the struct, the
//! [`PollManagerEnv`] trait carries the `G()`/option/pm/cross-manager
//! surfaces, updates buffer into a `Vec<td_api::Update>` drained by
//! `Td::flush_user_updates`, and the member `MultiTimeout`s are deadline
//! maps driven by [`PollManager::tick_timeouts`].
//!
//! The message-plane probes (`MessagesManager::get_message_poll_id`, the
//! notify-on-update hops) are the `[blocked: M.10 integration]` env seams;
//! the net query tails (`messages.sendVote`, `messages.getPollResults`,
//! `messages.getPollVotes`, `messages.editMessage` close,
//! `messages.addPollAnswer`, `messages.deletePollAnswer`) are the
//! `[blocked: net]` env seams.
//!
//! Deferred to the M.9/M.10 integration (the C++ bodies read the message
//! content plane): the option/explanation `MessageContent` media values —
//! the record carries the content-type tag only and the db bytes ride the
//! [`PollStoreEnv::store_poll_media`]/[`PollStoreEnv::parse_poll_media`]
//! seam — plus `add_poll_option`'s upload half, `dup_poll`,
//! `get_poll_file_ids`, `get_input_media` and the individual
//! message-content accessors.

use crate::chat_manager::{ChannelId, MinChannel};
use crate::dialog_id::{DialogId, MessageFullId};
use crate::message_entity::FormattedText;
use crate::store_parse::{Parser, store_flags, store_i32, store_i64, store_string};
use crate::user_manager::UserId;

// ---------------------------------------------------------------------------
// PollId (PollId.h)
// ---------------------------------------------------------------------------

/// `td::PollId` (PollId.h) — the poll id newtype. Local (client-created)
/// polls take negative ids; server polls are positive.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PollId(i64);

impl PollId {
    pub fn new(id: i64) -> PollId {
        PollId(id)
    }

    pub fn get(self) -> i64 {
        self.0
    }

    /// `PollId::is_valid` (PollId.h:44-46).
    pub fn is_valid(self) -> bool {
        self.0 != 0
    }

    /// `PollId::store` (PollId.hpp:23-25) — the manager-mediated form;
    /// the bare-id framing used inside the message-content poll record.
    pub fn store(self, out: &mut Vec<u8>) {
        store_i64(out, self.0);
    }

    /// `PollId::parse` (PollId.hpp:27-29) — the bare-id framing.
    pub fn parse_from(parser: &mut Parser<'_>) -> Result<PollId, &'static str> {
        Ok(PollId::new(parser.i64_()?))
    }
}

impl std::fmt::Display for PollId {
    /// `operator<<(StringBuilder, PollId)` (PollId.h:57-59).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "poll {}", self.0)
    }
}

// ---------------------------------------------------------------------------
// Value helpers (PollManager.cpp)
// ---------------------------------------------------------------------------

/// `PollManager::is_local_poll_id` (PollManager.cpp:449-451) — local ids
/// are negative, but not `INT32_MIN` (that range stays reserved).
pub fn is_local_poll_id(poll_id: PollId) -> bool {
    poll_id.get() < 0 && poll_id.get() > i32::MIN as i64
}

/// `PollManager::check_quiz_correct_option_ids` (PollManager.cpp:453-469).
pub fn check_quiz_correct_option_ids(
    correct_option_ids: &[i32],
    option_count: usize,
    allow_empty: bool,
) -> std::result::Result<(), tdutils::status::Status> {
    if !allow_empty && correct_option_ids.is_empty() {
        return Err(tdutils::status::Status::error(
            400,
            "Correct quiz option list must be non-empty",
        ));
    }
    for i in 0..correct_option_ids.len().saturating_sub(1) {
        if correct_option_ids[i] >= correct_option_ids[i + 1] {
            return Err(tdutils::status::Status::error(
                400,
                "Correct quiz option list must be increasing",
            ));
        }
    }
    for correct_option_id in correct_option_ids {
        if *correct_option_id < 0 || *correct_option_id >= option_count as i32 {
            return Err(tdutils::status::Status::error(
                400,
                "Wrong quiz correct_option_id",
            ));
        }
    }
    Ok(())
}

/// `PollManager::get_vote_percentage` (PollManager.cpp:620-722) — the
/// percentages that sum below 100 with the gap-greedy top-up: options with
/// equal votes move together, halves round to 50%, more popular option
/// groups win ties, the first-encountered group wins the last tie.
pub fn get_vote_percentage(voter_counts: &[i32], total_voter_count: i32) -> Vec<i32> {
    let mut total_voter_count = total_voter_count;
    let mut sum: i64 = 0;
    for voter_count in voter_counts {
        sum += *voter_count as i64;
    }

    if total_voter_count as i64 > sum {
        total_voter_count = sum as i32;
    }

    let mut result = vec![0i32; voter_counts.len()];
    if total_voter_count == 0 {
        return result;
    }
    if total_voter_count as i64 != sum {
        // just round to the nearest
        for i in 0..result.len() {
            result[i] = ((voter_counts[i] as i64 * 200 + total_voter_count as i64)
                / total_voter_count as i64
                / 2) as i32;
        }
        return result;
    }

    // make sure that options with equal votes have equal percent and total sum is less than 100%
    let mut percent_sum: i32 = 0;
    let mut gap = vec![0i64; voter_counts.len()];
    for i in 0..result.len() {
        let multiplied_voter_count = voter_counts[i] as i64 * 100;
        result[i] = (multiplied_voter_count / total_voter_count as i64) as i32;
        gap[i] = (result[i] as i64 + 1) * total_voter_count as i64 - multiplied_voter_count;
        percent_sum += result[i];
    }
    if percent_sum == 100 {
        return result;
    }

    // now we need to choose up to (100 - percent_sum) options with a minimum total gap, such that
    // any two options with the same voter_count are chosen or not chosen simultaneously
    struct Option {
        pos: i32,
        count: i32,
    }
    // the C++ keys the map with voter_count + 1 so the zero-vote bucket
    // stays addressable; the iteration order only feeds a greedy pass.
    let mut buckets: std::collections::HashMap<i64, Option> = std::collections::HashMap::new();
    for (i, voter_count) in voter_counts.iter().enumerate() {
        let option = buckets
            .entry(*voter_count as i64 + 1)
            .or_insert(Option { pos: -1, count: 0 });
        if option.pos == -1 {
            option.pos = i as i32;
        }
        option.count += 1;
    }
    let mut sorted_options: Vec<Option> = Vec::new();
    for option in buckets.values() {
        let pos = option.pos as usize;
        if gap[pos] > total_voter_count as i64 / 2 {
            // do not round to wrong direction
            continue;
        }
        if total_voter_count % 2 == 0
            && gap[pos] == total_voter_count as i64 / 2
            && result[pos] >= 50
        {
            // round halves to the 50%
            continue;
        }
        sorted_options.push(Option {
            pos: option.pos,
            count: option.count,
        });
    }
    sorted_options.sort_by(|lhs, rhs| {
        if gap[lhs.pos as usize] != gap[rhs.pos as usize] {
            // prefer options with smallest gap
            return gap[lhs.pos as usize].cmp(&gap[rhs.pos as usize]);
        }
        if lhs.count != rhs.count {
            // prefer more popular options
            return rhs.count.cmp(&lhs.count);
        }
        lhs.pos.cmp(&rhs.pos) // prefer the first encountered option
    });

    // dynamic programming or brute force can give perfect result, but for now we use simple gready approach
    let mut left_percent = 100 - percent_sum;
    for option in sorted_options {
        if option.count <= left_percent {
            left_percent -= option.count;

            let pos = option.pos as usize;
            for i in 0..result.len() {
                if voter_counts[i] == voter_counts[pos] {
                    result[i] += 1;
                }
            }
            if left_percent == 0 {
                break;
            }
        }
    }
    result
}

// ---------------------------------------------------------------------------
// The store/parse env (PollId.hpp/PollOption.hpp/PollManager.hpp storer
// contexts)
// ---------------------------------------------------------------------------

/// The `storer.context()->td()` slice the poll db records read: the
/// DialogPhoto codec of the min-channel records and the message-content
/// codec of the option/explanation media.
pub trait PollStoreEnv: crate::photo::DialogPhotoStoreEnv {
    /// `store_message_content` (MessageContent.cpp) — the option/explanation
    /// media bytes. [blocked: M.9 integration]: the codec lands with the
    /// message-content line; the C++ byte layout around it is preserved
    /// (the `has_media` flag + the inline content blob).
    fn store_poll_media(&mut self, media_type: i32, out: &mut Vec<u8>);

    /// `parse_message_content` (MessageContent.cpp) — the same seam on the
    /// read side; returns the content-type tag.
    fn parse_poll_media(&mut self, parser: &mut Parser<'_>) -> Result<i32, &'static str>;
}

/// `store(const MinChannel &, StorerT &)` (MinChannel.hpp:8-32).
/// (The M.16 line carries the twin in message_reactor.rs; dedupe at merge.)
pub fn store_min_channel(min_channel: &MinChannel, env: &mut impl PollStoreEnv, out: &mut Vec<u8>) {
    let has_title = !min_channel.title_.is_empty();
    let has_photo = min_channel.photo_.small_file_id.is_valid();
    let has_accent_color_id = min_channel.accent_color_id_.is_valid();
    store_flags(
        out,
        [
            has_title,
            has_photo,
            min_channel.is_megagroup_,
            has_accent_color_id,
        ],
    );
    if has_title {
        store_string(out, &min_channel.title_);
    }
    if has_photo {
        crate::photo::store_dialog_photo(&min_channel.photo_, env, out);
    }
    if has_accent_color_id {
        store_i32(out, min_channel.accent_color_id_.get());
    }
}

/// `parse(MinChannel &, ParserT &)` (MinChannel.hpp:34-57).
pub fn parse_min_channel(
    env: &mut impl PollStoreEnv,
    parser: &mut Parser<'_>,
) -> Result<MinChannel, &'static str> {
    let mut min_channel = MinChannel::default();
    let [has_title, has_photo, is_megagroup, has_accent_color_id] = parser.parse_flags()?;
    min_channel.is_megagroup_ = is_megagroup;
    if has_title {
        min_channel.title_ = parser.string()?;
    }
    if has_photo {
        min_channel.photo_ = crate::photo::parse_dialog_photo(env, parser)?;
    }
    if has_accent_color_id {
        min_channel.accent_color_id_ = crate::peer_color::AccentColorId::new(parser.i32_()?);
    }
    Ok(min_channel)
}

/// `store(vector<pair<ChannelId, MinChannel>>)` — the tl_helpers vector
/// framing over `ChannelId::store` (store_long) + `MinChannel::store`.
pub fn store_min_channels(
    min_channels: &[(ChannelId, MinChannel)],
    env: &mut impl PollStoreEnv,
    out: &mut Vec<u8>,
) {
    store_i32(out, min_channels.len() as i32);
    for (channel_id, min_channel) in min_channels {
        store_i64(out, channel_id.get());
        store_min_channel(min_channel, env, out);
    }
}

/// `parse(vector<pair<ChannelId, MinChannel>>)` — the same framing on the
/// read side.
pub fn parse_min_channels(
    env: &mut impl PollStoreEnv,
    parser: &mut Parser<'_>,
) -> Result<Vec<(ChannelId, MinChannel)>, &'static str> {
    let len = parser.i32_()?;
    if len < 0 {
        return Err("Wrong vector length");
    }
    let mut result = Vec::with_capacity(len as usize);
    for _ in 0..len {
        let channel_id = ChannelId::new(parser.i64_()?);
        let min_channel = parse_min_channel(env, parser)?;
        result.push((channel_id, min_channel));
    }
    Ok(result)
}

/// `store(vector<DialogId>)` — the tl_helpers vector framing.
pub fn store_dialog_ids(dialog_ids: &[DialogId], out: &mut Vec<u8>) {
    store_i32(out, dialog_ids.len() as i32);
    for dialog_id in dialog_ids {
        dialog_id.store(out);
    }
}

/// `parse(vector<DialogId>)` — the same framing on the read side.
pub fn parse_dialog_ids(parser: &mut Parser<'_>) -> Result<Vec<DialogId>, &'static str> {
    let len = parser.i32_()?;
    if len < 0 {
        return Err("Wrong vector length");
    }
    let mut result = Vec::with_capacity(len as usize);
    for _ in 0..len {
        result.push(DialogId::parse_from(parser)?);
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// PollOption (PollOption.{h,cpp,hpp})
// ---------------------------------------------------------------------------

/// `td::PollOption` (PollOption.h:30-131). The derived `PartialEq` is the
/// full-field record equality; the C++ `operator==` (which ignores the
/// voter machine fields) is [`PollOption::options_equal`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PollOption {
    pub(crate) text: FormattedText,
    pub(crate) media_type: Option<i32>,
    pub(crate) added_by_dialog_id: DialogId,
    pub(crate) recent_voter_dialog_ids: Vec<DialogId>,
    pub(crate) added_date: i32,
    pub(crate) voter_count: i32,
    pub(crate) is_chosen: bool,
    pub(crate) data: String,
}

impl PollOption {
    /// `PollOption::get_text` (PollOption.h:54-56).
    pub fn get_text(&self) -> &FormattedText {
        &self.text
    }

    /// `PollOption::get_data` (PollOption.h:58-60) — the raw option bytes
    /// (the vote payload) as a string.
    pub fn get_data(&self) -> &str {
        &self.data
    }

    /// `PollOption::get_added_by_dialog_id` (PollOption.h:62-64).
    pub fn get_added_by_dialog_id(&self) -> DialogId {
        self.added_by_dialog_id
    }

    /// `PollOption::get_added_date` (PollOption.h:66-68).
    pub fn get_added_date(&self) -> i32 {
        self.added_date
    }

    /// `PollOption::is_chosen` (PollOption.h:70-72).
    pub fn is_chosen(&self) -> bool {
        self.is_chosen
    }

    /// `PollOption::set_is_chosen` (PollOption.h:74-80) — true on change.
    pub fn set_is_chosen(&mut self, is_chosen: bool) -> bool {
        if self.is_chosen != is_chosen {
            self.is_chosen = is_chosen;
            return true;
        }
        false
    }

    /// `PollOption::get_voter_count` (PollOption.h:82-84).
    pub fn get_voter_count(&self) -> i32 {
        self.voter_count
    }

    /// `PollOption::set_voter_count` (PollOption.h:86-92) — true on change.
    pub fn set_voter_count(&mut self, voter_count: i32) -> bool {
        if self.voter_count != voter_count {
            self.voter_count = voter_count;
            return true;
        }
        false
    }

    /// `PollOption::set_recent_voter_dialog_ids` (PollOption.h:94-100) —
    /// true on change.
    pub fn set_recent_voter_dialog_ids(&mut self, recent_voter_dialog_ids: Vec<DialogId>) -> bool {
        if self.recent_voter_dialog_ids != recent_voter_dialog_ids {
            self.recent_voter_dialog_ids = recent_voter_dialog_ids;
            return true;
        }
        false
    }

    /// `PollOption::get_recent_voter_dialog_ids` — the direct accessor the
    /// C++ reaches through the public field.
    pub fn get_recent_voter_dialog_ids(&self) -> &[DialogId] {
        &self.recent_voter_dialog_ids
    }

    /// `PollOption::get_search_text` (PollOption.cpp:101-103).
    pub fn get_search_text(&self) -> &str {
        &self.text.text
    }

    /// `operator==(const PollOption &, const PollOption &)`
    /// (PollOption.cpp:162-173) — voter_count, recent_voter_dialog_ids and
    /// is_chosen don't compare; the media compares by content type only.
    pub fn options_equal(lhs: &PollOption, rhs: &PollOption) -> bool {
        if lhs.media_type.is_none() {
            if rhs.media_type.is_some() {
                return false;
            }
        } else if lhs.media_type != rhs.media_type {
            return false;
        }
        lhs.text == rhs.text
            && lhs.data == rhs.data
            && lhs.added_by_dialog_id == rhs.added_by_dialog_id
            && lhs.added_date == rhs.added_date
    }

    /// `PollOption::store` (PollOption.hpp:17-60) — the byte-exact record.
    pub fn store(&self, env: &mut impl PollStoreEnv, out: &mut Vec<u8>) {
        let has_entities = !self.text.entities.is_empty();
        let has_added_by_dialog_id = self.added_by_dialog_id.is_valid();
        let has_added_date = self.added_date != 0;
        let has_recent_voter_dialog_ids = !self.recent_voter_dialog_ids.is_empty();
        let has_media = self.media_type.is_some();
        let has_no_voter_count = self.voter_count == 0;
        let has_no_data = self.data.is_empty();
        store_flags(
            out,
            [
                self.is_chosen,
                has_entities,
                has_added_by_dialog_id,
                has_added_date,
                has_recent_voter_dialog_ids,
                has_media,
                has_no_voter_count,
                has_no_data,
            ],
        );

        store_string(out, &self.text.text);
        if !has_no_data {
            store_string(out, &self.data);
        }
        if !has_no_voter_count {
            store_i32(out, self.voter_count);
        }
        if has_entities {
            store_i32(out, self.text.entities.len() as i32);
            for entity in &self.text.entities {
                entity.store(out);
            }
        }
        if has_added_by_dialog_id {
            self.added_by_dialog_id.store(out);
        }
        if has_added_date {
            store_i32(out, self.added_date);
        }
        if has_recent_voter_dialog_ids {
            store_dialog_ids(&self.recent_voter_dialog_ids, out);
        }
        if has_media {
            // store_message_content — the [blocked: M.9 integration] seam
            env.store_poll_media(self.media_type.unwrap_or_default(), out);
        }
    }

    /// `PollOption::parse` (PollOption.hpp:62-105) — the byte-exact record.
    pub fn parse(
        env: &mut impl PollStoreEnv,
        parser: &mut Parser<'_>,
    ) -> Result<PollOption, &'static str> {
        let mut result = PollOption::default();
        let [
            is_chosen,
            has_entities,
            has_added_by_dialog_id,
            has_added_date,
            has_recent_voter_dialog_ids,
            has_media,
            has_no_voter_count,
            has_no_data,
        ] = parser.parse_flags()?;
        result.is_chosen = is_chosen;

        result.text.text = parser.string()?;
        if !has_no_data {
            result.data = parser.string()?;
        }
        if !has_no_voter_count {
            result.voter_count = parser.i32_()?;
        }
        if has_entities {
            let len = parser.i32_()?;
            if len < 0 {
                return Err("Wrong vector length");
            }
            result.text.entities.reserve(len as usize);
            for _ in 0..len {
                result
                    .text
                    .entities
                    .push(crate::message_entity::MessageEntity::parse(parser)?);
            }
        }
        if has_added_by_dialog_id {
            result.added_by_dialog_id = DialogId::parse_from(parser)?;
        }
        if has_added_date {
            result.added_date = parser.i32_()?;
        }
        if has_recent_voter_dialog_ids {
            result.recent_voter_dialog_ids = parse_dialog_ids(parser)?;
        }
        if has_media {
            // parse_message_content — the [blocked: M.9 integration] seam
            result.media_type = Some(env.parse_poll_media(parser)?);
        }
        Ok(result)
    }
}

// ---------------------------------------------------------------------------
// Poll (PollManager.h:161-195 + PollManager.hpp:22-223)
// ---------------------------------------------------------------------------

/// `PollManager::Poll` (PollManager.h:161-195) — the poll record. The
/// `was_saved_` latch stays in the manager (it rides the mutable half of
/// the C++ struct).
#[derive(Clone, Debug, Default)]
pub struct Poll {
    pub(crate) question: FormattedText,
    pub(crate) options: Vec<PollOption>,
    pub(crate) option_min_channels: Vec<(ChannelId, MinChannel)>,
    pub(crate) recent_option_voter_min_channels: Vec<(ChannelId, MinChannel)>,
    pub(crate) recent_voter_dialog_ids: Vec<DialogId>,
    pub(crate) recent_voter_min_channels: Vec<(ChannelId, MinChannel)>,
    pub(crate) explanation: FormattedText,
    pub(crate) explanation_media_type: Option<i32>,
    pub(crate) correct_option_ids: Vec<i32>,
    pub(crate) total_voter_count: i32,
    pub(crate) open_period: i32,
    pub(crate) close_date: i32,
    pub(crate) hash: i64,
    pub(crate) country_codes: Vec<String>,
    pub(crate) is_anonymous: bool,
    pub(crate) allow_multiple_answers: bool,
    pub(crate) has_open_answers: bool,
    pub(crate) has_revoting_disabled: bool,
    pub(crate) subscribers_only: bool,
    pub(crate) shuffle_answers: bool,
    pub(crate) hide_results_until_close: bool,
    pub(crate) is_quiz: bool,
    pub(crate) is_closed: bool,
    pub(crate) is_updated_after_close: bool,
    pub(crate) is_creator: bool,
    pub(crate) has_unread_votes: bool,
    pub(crate) can_view_stats: bool,
}

impl Poll {
    /// `PollManager::Poll::store` (PollManager.hpp:22-112) — the byte-exact
    /// pmc record.
    pub fn store(&self, env: &mut impl PollStoreEnv, out: &mut Vec<u8>) {
        let is_public = !self.is_anonymous;
        let has_open_period = self.open_period != 0;
        let has_close_date = self.close_date != 0;
        let has_explanation = !self.explanation.text.is_empty();
        let has_recent_voter_dialog_ids = !self.recent_voter_dialog_ids.is_empty();
        let has_recent_voter_min_channels = !self.recent_voter_min_channels.is_empty();
        let has_question_entities = !self.question.entities.is_empty();
        let has_multiple_correct_option_ids = self.correct_option_ids.len() > 1;
        let know_revoting_disabled = true;
        let has_hash = self.hash != 0;
        let has_explanation_media = self.explanation_media_type.is_some();
        let has_option_min_channels = !self.option_min_channels.is_empty();
        let has_recent_option_voter_min_channels =
            !self.recent_option_voter_min_channels.is_empty();
        let has_country_codes = !self.country_codes.is_empty();
        store_flags(
            out,
            [
                self.is_closed,                       // 0
                is_public,                            // 1
                self.allow_multiple_answers,          // 2
                self.is_quiz,                         // 3
                false,                                // 4
                has_open_period,                      // 5
                has_close_date,                       // 6
                has_explanation,                      // 7
                self.is_updated_after_close,          // 8
                has_recent_voter_dialog_ids,          // 9
                has_recent_voter_min_channels,        // 10
                has_question_entities,                // 11
                has_multiple_correct_option_ids,      // 12
                self.has_open_answers,                // 13
                know_revoting_disabled,               // 14
                self.has_revoting_disabled,           // 15
                self.shuffle_answers,                 // 16
                self.hide_results_until_close,        // 17
                self.is_creator,                      // 18
                has_hash,                             // 19
                self.has_unread_votes,                // 20
                has_explanation_media,                // 21
                has_option_min_channels,              // 22
                has_recent_option_voter_min_channels, // 23
                self.subscribers_only,                // 24
                has_country_codes,                    // 25
                self.can_view_stats,                  // 26
            ],
        );

        store_string(out, &self.question.text);
        store_i32(out, self.options.len() as i32);
        for option in &self.options {
            option.store(env, out);
        }
        store_i32(out, self.total_voter_count);
        if self.is_quiz {
            if has_multiple_correct_option_ids {
                store_i32(out, self.correct_option_ids.len() as i32);
                for correct_option_id in &self.correct_option_ids {
                    store_i32(out, *correct_option_id);
                }
            } else {
                store_i32(
                    out,
                    if self.correct_option_ids.is_empty() {
                        -1
                    } else {
                        self.correct_option_ids[0]
                    },
                );
            }
        }
        if has_open_period {
            store_i32(out, self.open_period);
        }
        if has_close_date {
            store_i32(out, self.close_date);
        }
        if has_explanation {
            self.explanation.store(out);
        }
        if has_recent_voter_dialog_ids {
            store_dialog_ids(&self.recent_voter_dialog_ids, out);
        }
        if has_recent_voter_min_channels {
            store_min_channels(&self.recent_voter_min_channels, env, out);
        }
        if has_question_entities {
            store_i32(out, self.question.entities.len() as i32);
            for entity in &self.question.entities {
                entity.store(out);
            }
        }
        if has_hash {
            store_i64(out, self.hash);
        }
        if has_explanation_media {
            // store_message_content — the [blocked: M.9 integration] seam
            env.store_poll_media(self.explanation_media_type.unwrap_or_default(), out);
        }
        if has_option_min_channels {
            store_min_channels(&self.option_min_channels, env, out);
        }
        if has_recent_option_voter_min_channels {
            store_min_channels(&self.recent_option_voter_min_channels, env, out);
        }
        if has_country_codes {
            store_string_vec(out, &self.country_codes);
        }
    }

    /// `PollManager::Poll::parse` (PollManager.hpp:114-223) — the byte-exact
    /// pmc record, including the legacy recent-voter-user-ids path.
    pub fn parse(
        env: &mut impl PollStoreEnv,
        parser: &mut Parser<'_>,
    ) -> Result<Poll, &'static str> {
        let mut result = Poll::default();
        let [
            is_closed,
            is_public,
            allow_multiple_answers,
            is_quiz,
            has_recent_voter_user_ids,
            has_open_period,
            has_close_date,
            has_explanation,
            is_updated_after_close,
            has_recent_voter_dialog_ids,
            has_recent_voter_min_channels,
            has_question_entities,
            has_multiple_correct_option_ids,
            has_open_answers,
            know_revoting_disabled,
            has_revoting_disabled,
            shuffle_answers,
            hide_results_until_close,
            is_creator,
            has_hash,
            has_unread_votes,
            has_explanation_media,
            has_option_min_channels,
            has_recent_option_voter_min_channels,
            subscribers_only,
            has_country_codes,
            can_view_stats,
        ] = parser.parse_flags()?;
        result.is_closed = is_closed;
        result.is_anonymous = !is_public;
        result.allow_multiple_answers = allow_multiple_answers;
        result.is_quiz = is_quiz;
        result.is_updated_after_close = is_updated_after_close;
        result.has_open_answers = has_open_answers;
        result.has_revoting_disabled = has_revoting_disabled;
        result.shuffle_answers = shuffle_answers;
        result.hide_results_until_close = hide_results_until_close;
        result.is_creator = is_creator;
        result.has_unread_votes = has_unread_votes;
        result.subscribers_only = subscribers_only;
        result.can_view_stats = can_view_stats;

        result.question.text = parser.string()?;
        let options_len = parser.i32_()?;
        if options_len < 0 {
            return Err("Wrong vector length");
        }
        result.options.reserve(options_len as usize);
        for _ in 0..options_len {
            result.options.push(PollOption::parse(env, parser)?);
        }
        result.total_voter_count = parser.i32_()?;
        if result.is_quiz {
            if has_multiple_correct_option_ids {
                let len = parser.i32_()?;
                if len < 0 {
                    return Err("Wrong vector length");
                }
                result.correct_option_ids.reserve(len as usize);
                for _ in 0..len {
                    result.correct_option_ids.push(parser.i32_()?);
                }
            } else {
                let correct_option_id = parser.i32_()?;
                if correct_option_id != -1 {
                    result.correct_option_ids.push(correct_option_id);
                }
            }
            check_quiz_correct_option_ids(&result.correct_option_ids, result.options.len(), true)
                .map_err(|status| {
                // parser.set_error(status.message())
                // — a Box<str> can't be a &'static str; the callers see
                // the generic corruption band.
                let _ = status.message();
                "Invalid quiz correct_option_ids"
            })?;
        }
        if has_recent_voter_user_ids {
            // the pre-Support64BitDialogIds legacy form: user ids, widened
            // to user dialog ids
            let len = parser.i32_()?;
            if len < 0 {
                return Err("Wrong vector length");
            }
            let mut recent_voter_user_ids = Vec::with_capacity(len as usize);
            for _ in 0..len {
                let user_id = if parser.version() >= version_ids::SUPPORT_64_BIT_IDS {
                    parser.i64_()?
                } else {
                    parser.i32_()? as i64
                };
                recent_voter_user_ids.push(UserId::new(user_id));
            }
            result.recent_voter_dialog_ids = recent_voter_user_ids
                .iter()
                .map(|user_id| DialogId::from_user_id(*user_id))
                .collect();
        }
        if has_open_period {
            result.open_period = parser.i32_()?;
        }
        if has_close_date {
            result.close_date = parser.i32_()?;
        }
        if has_explanation {
            result.explanation = FormattedText::parse(parser)?;
        }
        if has_recent_voter_dialog_ids {
            result.recent_voter_dialog_ids = parse_dialog_ids(parser)?;
        }
        if has_recent_voter_min_channels {
            result.recent_voter_min_channels = parse_min_channels(env, parser)?;
        }
        if has_question_entities {
            let len = parser.i32_()?;
            if len < 0 {
                return Err("Wrong vector length");
            }
            result.question.entities.reserve(len as usize);
            for _ in 0..len {
                result
                    .question
                    .entities
                    .push(crate::message_entity::MessageEntity::parse(parser)?);
            }
        }
        if !know_revoting_disabled {
            result.has_revoting_disabled = result.is_quiz;
        }
        if has_hash {
            result.hash = parser.i64_()?;
        }
        if has_explanation_media {
            // parse_message_content — the [blocked: M.9 integration] seam
            result.explanation_media_type = Some(env.parse_poll_media(parser)?);
        }
        if has_option_min_channels {
            result.option_min_channels = parse_min_channels(env, parser)?;
        }
        if has_recent_option_voter_min_channels {
            result.recent_option_voter_min_channels = parse_min_channels(env, parser)?;
        }
        if has_country_codes {
            result.country_codes = parser.string_vec()?;
        }
        Ok(result)
    }
}

/// The db version constants the legacy parse paths gate on
/// (`Version::Support64BitIds`; `SupportPolls2_0` arrives with the
/// manager-level `parse_poll`).
mod version_ids {
    pub const SUPPORT_64_BIT_IDS: i32 = 33;
}

fn store_string_vec(out: &mut Vec<u8>, values: &[String]) {
    crate::store_parse::store_string_vec(out, values);
}

// ---------------------------------------------------------------------------
// The manager-level store_poll/parse_poll (PollManager.hpp:225-421) — the
// form `PollId::store/parse` writes inside other records (the message
// content poll record). They live on PollManager in slice M.19.2; the
// value-plane helpers they share are above.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// The manager log events (PollManager.cpp:1289-1308, 1748-1764)
// ---------------------------------------------------------------------------

/// `PollManager::SetPollAnswerLogEvent` (PollManager.cpp:1289-1308).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SetPollAnswerLogEvent {
    pub poll_id: PollId,
    pub message_full_id: MessageFullId,
    pub options: Vec<String>,
}

impl SetPollAnswerLogEvent {
    /// `SetPollAnswerLogEvent::store` (PollManager.cpp:1295-1300).
    pub fn store(&self, out: &mut Vec<u8>) {
        self.poll_id.store(out);
        self.message_full_id.store(out);
        crate::store_parse::store_string_vec(out, &self.options);
    }

    /// `SetPollAnswerLogEvent::parse` (PollManager.cpp:1302-1307).
    pub fn parse(parser: &mut Parser<'_>) -> Result<SetPollAnswerLogEvent, &'static str> {
        Ok(SetPollAnswerLogEvent {
            poll_id: PollId::parse_from(parser)?,
            message_full_id: MessageFullId::parse_from(parser)?,
            options: parser.string_vec()?,
        })
    }
}

/// `PollManager::StopPollLogEvent` (PollManager.cpp:1748-1764).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StopPollLogEvent {
    pub poll_id: PollId,
    pub message_full_id: MessageFullId,
}

impl StopPollLogEvent {
    /// `StopPollLogEvent::store` (PollManager.cpp:1753-1757).
    pub fn store(&self, out: &mut Vec<u8>) {
        self.poll_id.store(out);
        self.message_full_id.store(out);
    }

    /// `StopPollLogEvent::parse` (PollManager.cpp:1759-1763).
    pub fn parse(parser: &mut Parser<'_>) -> Result<StopPollLogEvent, &'static str> {
        Ok(StopPollLogEvent {
            poll_id: PollId::parse_from(parser)?,
            message_full_id: MessageFullId::parse_from(parser)?,
        })
    }
}

// The PollManager struct + machinery land in slice M.19.2 (the records
// above are everything the store/parse plane needs).

#[cfg(test)]
#[path = "poll_manager_tests.rs"]
mod poll_manager_tests;
