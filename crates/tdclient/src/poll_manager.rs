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
//! notify-on-update hops) are the `[blocked: M.10 integration]` env seams.
//! The five wire tails (`messages.sendVote`, `messages.getPollResults`,
//! `messages.getPollVotes`, `messages.editMessage` close,
//! `messages.deletePollAnswer`) ride the `PollNetKind` plane
//! (net-drain 39); `messages.addPollAnswer` stays the M.9/M.10
//! media-upload plane (the `add_poll_option` upload half).
//!
//! # The net-drain 39 audit table
//!
//! | C++ handler (PollManager.cpp) | wire | kind | send leg | answer arm | bands kept |
//! |---|---|---|---|---|---|
//! | `SendVoteQuery` cpp:236-306 | `messages.sendVote` | `PollNetKind::SendVote` | the `send_vote_query` env arm at `do_set_poll_answer` cpp:1386 | `on_send_vote_answer` → `on_set_poll_answer` cpp:1390-1429 → `on_set_poll_answer_finished` cpp:1431-1469 | close_flag drop; the None-peer 400 routes the dialog-error head + the full ladder |
//! | `GetPollResultsQuery` cpp:67-110 | `messages.getPollResults` | `PollNetKind::GetPollResults` | the `send_get_poll_results_query` env arm at `on_update_poll_timeout` cpp:1866 | `on_get_poll_results_answer` → `on_get_poll_results` cpp:1940-1975 | Ok(null) drop at the send leg; error reget ladder; stale-generation reget 0.0 |
//! | `GetPollVotersQuery` cpp:109-162 | `messages.getPollVotes` | `PollNetKind::GetPollVoters` | the `send_get_poll_voters_query` env arm at `get_poll_voters` cpp:1614 | `on_get_poll_voters_answer` → `on_get_poll_voters` cpp:1619-1710 | anonymous band; offset mismatch; empty-promise lot; count-mismatch reget; MESSAGE_ID_INVALID carve-out |
//! | `StopPollQuery` cpp:278-332 | `messages.editMessage` | `PollNetKind::StopPoll` | the `send_stop_poll_query` env arm at `do_stop_poll` cpp:1787 | `on_stop_poll_answer` → the engine tail → `on_stop_poll_finished` cpp:1790-1806 | `MESSAGE_NOT_MODIFIED` Ok band (non-bot); being_closed erase on every band |
//! | `DeletePollAnswerQuery` cpp:199-234 | `messages.deletePollAnswer` | `PollNetKind::DeletePollAnswer` | the `send_delete_poll_answer_query` env arm at `delete_poll_option` cpp:1235 | `on_delete_poll_answer_answer` → the updates tail with the popped promise | dialog error head |
//!
//! Divergences: `NetQueryRef query_ref_` cancellation stays unmodeled —
//! the kind-carried `generation` identity pairs a replaced query's late
//! answer with its own lot entry, and the generation check inside
//! `on_set_poll_answer` drops it before any state touch; the C++
//! `server_poll_messages_.get_random()` spread pick collapses to an
//! arbitrary element; `G()->ignore_result_if_closing` (Global.h:706-709,
//! Ok → 500 "Request aborted" on close_flag) guards the
//! voters/results answers; the [`PollManagerEnv`] send arms carry the
//! query construction (the peers resolve at the send leg, the §0.2
//! recipe) and their `Err`s are the C++ `return on_error(...)`
//! send-leg bands.
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
use std::collections::{HashMap, HashSet};
use tdutils::promise::Promise;

use crate::dialog_id::DialogType;
use crate::td_api;

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
pub trait PollStoreEnv: crate::photo::PhotoStoreEnv {
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
        crate::photo::store_dialog_photo(env, out, &min_channel.photo_);
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

/// `LogEvent::HandlerType::SetPollAnswer` (LogEvent.h:71).
const SET_POLL_ANSWER_HANDLER_TYPE: i32 = 0x20;
/// `LogEvent::HandlerType::StopPoll` (LogEvent.h:72).
const STOP_POLL_HANDLER_TYPE: i32 = 0x21;

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

// ---------------------------------------------------------------------------
// The manager env (the `Td *td_` / `G()` slice of the collapsed manager)
// ---------------------------------------------------------------------------

/// The `Td *td_` surface the collapsed PollManager reads. The five wire
/// tails (`SendVoteQuery`/`GetPollResultsQuery`/`GetPollVotersQuery`/
/// `StopPollQuery`/`DeletePollAnswerQuery`) ride the net-drain 39 plane:
/// the [`send_vote_query`]/[`send_get_poll_results_query`]/
/// [`send_get_poll_voters_query`]/[`send_stop_poll_query`]/
/// [`send_delete_poll_answer_query`] arms dispatch (the net-inert log
/// in the `None` band parks the promises, exactly the C++ request
/// without a network) and the answers re-enter through the
/// `PollNetKind` router. The `messages_manager_` lookups are
/// **[blocked: M.10 integration]** seams; the web-page registration rides
/// the M.17 line (the `WebPageId` newtype lands there — the seam carries
/// the raw id).
///
/// [`send_vote_query`]: PollManagerEnv::send_vote_query
/// [`send_get_poll_results_query`]: PollManagerEnv::send_get_poll_results_query
/// [`send_get_poll_voters_query`]: PollManagerEnv::send_get_poll_voters_query
/// [`send_stop_poll_query`]: PollManagerEnv::send_stop_poll_query
/// [`send_delete_poll_answer_query`]: PollManagerEnv::send_delete_poll_answer_query
pub trait PollManagerEnv: PollStoreEnv {
    // -- G() --
    fn auth_is_bot(&self) -> bool;
    fn close_flag(&self) -> bool;
    fn use_message_database(&self) -> bool;
    fn unix_time(&self) -> i32;
    fn server_time(&self) -> f64;
    /// The MultiTimeout clock (the scheduler's steady now).
    fn now(&self) -> f64;
    fn is_test_dc(&self) -> bool;

    // -- the sqlite sync pmc (the poll records) --
    fn pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8>;
    fn pmc_set_raw(&mut self, key: &[u8], value: &[u8]);

    // -- the binlog (the SetPollAnswer/StopPoll log events) --
    fn binlog_add(&mut self, handler_type: i32, data: &[u8]) -> u64;
    fn binlog_rewrite(&mut self, log_event_id: u64, handler_type: i32, data: &[u8]) -> u64;
    fn binlog_erase(&mut self, log_event_id: u64);

    // -- ChatManager probes --
    fn add_min_channel(&mut self, channel_id: ChannelId, min_channel: MinChannel);
    fn get_min_channel(&self, channel_id: ChannelId) -> Option<MinChannel>;
    fn channel_is_member(&self, channel_id: ChannelId) -> bool;
    fn channel_date(&self, channel_id: ChannelId) -> i32;

    // -- UserManager / DialogManager probes --
    fn have_dialog_info(&self, dialog_id: DialogId) -> bool;
    fn have_min_user(&self, user_id: crate::user_manager::UserId) -> bool;
    fn get_my_id(&self) -> crate::user_manager::UserId;
    fn get_my_dialog_id(&self) -> DialogId;

    // -- OptionManager probes --
    fn get_option_integer(&self, name: &str, default_value: i64) -> i64;
    fn get_option_string(&self, name: &str) -> String;

    // -- MessagesManager [blocked: M.10 integration] seams --
    /// `messages_manager_->get_message_poll_id` — the message-content poll
    /// lookup. The seam answers the not-found band until the M.10 line.
    fn get_message_poll_id(
        &mut self,
        message_full_id: &MessageFullId,
        ignore_has_poll: bool,
    ) -> Result<PollId, tdutils::status::Status>;
    /// `messages_manager_->has_message_sender_user_id` — the inline-reply
    /// markup fold of `stop_poll`.
    fn has_message_sender_user_id(&mut self, message_full_id: &MessageFullId) -> bool;
    /// `messages_manager_->on_external_update_message_content` — the
    /// notify_on_poll_update fan-out.
    fn on_external_update_message_content(&mut self, message_full_id: MessageFullId);
    /// `messages_manager_->on_update_poll_has_unread_votes`.
    fn on_update_poll_has_unread_votes(
        &mut self,
        message_full_id: MessageFullId,
        has_unread_votes: bool,
    );

    // -- WebPagesManager [M.17] seam (the WebPageId newtype lands there) --
    fn register_poll_web_pages(&mut self, poll_id: PollId, web_page_ids: Vec<i64>);

    // -- the message-sender folds of the object builders --
    /// `get_min_message_sender_object` — the recent-voter/author objects.
    fn get_min_message_sender_object(
        &mut self,
        dialog_id: DialogId,
    ) -> Option<td_api::MessageSender>;

    // -- the poll-media object builder [blocked: M.9 integration] --
    /// `get_poll_media_object` — the option/explanation media objects;
    /// `None` media answers the empty object.
    fn get_poll_media_object(&mut self, media_type: Option<i32>) -> Option<td_api::PollMedia>;

    // -- the bot update sends (updatePoll / updatePollAnswer) --
    fn send_update(&mut self, update: td_api::Update);

    // -- the get_formatted_text halves over the user manager --
    /// `get_formatted_text(user_manager, ...)` with the manager's own
    /// user manager (the solution/entities fixups).
    fn get_formatted_text_with_users(
        &mut self,
        text: String,
        entities: Vec<tdmtproto::telegram_api::MessageEntity>,
        skip_media_timestamps: bool,
        skip_trim: bool,
        source: &str,
    ) -> FormattedText;

    // -- the net-drain 39 plane (the five wire tails; the send arms
    //    resolve the peers at the send leg, the §0.2 recipe) --

    /// `SendVoteQuery::send` (PollManager.cpp:247-265) — the input peer
    /// resolves at the send leg; `Err` is the None-peer 400 band (the
    /// manager runs the handler's on_error head + the full answer ladder
    /// with it); `Ok(())` covers the dispatched query and the net-inert
    /// log alike (the promise parks in `pending_answers`, the C++
    /// request without a network). The generation rides the wire kind
    /// (the query identity, §5); the C++ `{{poll_id}, {dialog_id_}}`
    /// invoke-after chains stay unmodeled (the collapsed dispatcher
    /// carries no chains).
    fn send_vote_query(
        &mut self,
        message_full_id: &MessageFullId,
        options: Vec<String>,
        generation: u64,
    ) -> Result<(), tdutils::status::Status>;

    /// `GetPollResultsQuery::send` (PollManager.cpp:79-90) — `false` is
    /// the None-peer `promise.set_value(nullptr)` band (the Ok(null)
    /// answer drops through the intake without touching state);
    /// `true` covers the dispatched query and the net-inert log.
    fn send_get_poll_results_query(
        &mut self,
        message_full_id: &MessageFullId,
        poll_hash: i64,
        generation: u64,
    ) -> bool;

    /// `GetPollVotersQuery::send` (PollManager.cpp:120-140) — the input
    /// peer resolves at the send leg; `Err` is the None-peer 400 band
    /// (the handler settles the promise directly — no dialog-error head
    /// — and the manager routes it through the full answer intake). The
    /// (poll_id, option_id) pair rides the wire kind (the query
    /// identity, §5).
    fn send_get_poll_voters_query(
        &mut self,
        message_full_id: &MessageFullId,
        poll_id: PollId,
        option_id: i32,
        option: Vec<u8>,
        offset: &str,
        limit: i32,
    ) -> Result<(), tdutils::status::Status>;

    /// `StopPollQuery::send` (PollManager.cpp:287-322) — the input peer
    /// (Edit rights) resolves at the send leg; `Err` is the None-peer
    /// 400 band (the manager routes it through on_stop_poll_finished,
    /// so the being_closed/binlog erase + the promise error all run).
    fn send_stop_poll_query(
        &mut self,
        message_full_id: &MessageFullId,
        reply_markup: Option<&crate::reply_markup::ReplyMarkup>,
        poll_id: PollId,
    ) -> Result<(), tdutils::status::Status>;

    /// `DeletePollAnswerQuery::send` (PollManager.cpp:205-217) — the
    /// input peer resolves at the send leg; `Err` is the None-peer 400
    /// band (the manager runs the dialog-error head + the promise
    /// error).
    fn send_delete_poll_answer_query(
        &mut self,
        message_full_id: &MessageFullId,
        option_id: Vec<u8>,
    ) -> Result<(), tdutils::status::Status>;

    /// `MessagesManager::on_get_message_error(dialog_id, message_id,
    /// status, source)` — [blocked: M.10]; the default answers `false`
    /// (the error not consumed → the caller logs).
    fn on_get_message_error(
        &mut self,
        _dialog_id: DialogId,
        _message_id: crate::messages_manager::MessageId,
        _status: &tdutils::status::Status,
        _source: &str,
    ) -> bool {
        tdutils::log_info!("MessagesManager::on_get_message_error — [blocked: M.10 Messages]");
        false
    }

    /// `DialogManager::on_get_dialog_error(dialog_id, status, source)`
    /// — `true` when the error was consumed.
    fn on_get_dialog_error(
        &mut self,
        _dialog_id: DialogId,
        _status: &tdutils::status::Status,
        _source: &str,
    ) -> bool {
        false
    }

    /// `UserManager::on_get_users(users, source)` — the GetPollVoters
    /// answer feed (cpp:1659).
    fn on_get_users(&mut self, _users: Vec<tdmtproto::telegram_api::User>, _source: &str) {}

    /// `ChatManager::on_get_chats(chats, source)` — the GetPollVoters
    /// answer feed (cpp:1660).
    fn on_get_chats(&mut self, _chats: Vec<tdmtproto::telegram_api::Chat>, _source: &str) {}
}

/// The concrete env over the live `Td` fields — built by `poll_env_split`
/// in td.rs; the trait impls live there (the M.16 `ReactionEnv` shape).
pub(crate) struct PollManagerEnvImpl<'a> {
    pub(crate) global: &'a mut crate::global::Global,
    /// `None` once the option manager is torn down; the getters answer
    /// their defaults on the destroy path.
    pub(crate) options: Option<&'a mut crate::option_manager::OptionManager>,
    pub(crate) auth_is_bot: bool,
    pub(crate) user_manager: &'a mut crate::user_manager::UserManager,
    pub(crate) chat_manager: &'a mut crate::chat_manager::ChatManager,
    pub(crate) file_manager: std::sync::Arc<parking_lot::Mutex<crate::files::FileManager>>,
    pub(crate) updates: &'a mut Vec<td_api::Update>,
    /// The net-drain 39 dispatch surface (`None` in the net-inert tests).
    pub(crate) net: Option<crate::td::PollNetHandle>,
}

/// `PollManager::PendingPollAnswer` (PollManager.h:312-319).
#[derive(Default)]
pub struct PendingPollAnswer {
    pub(crate) options: Vec<String>,
    pub(crate) promises: Vec<Promise<()>>,
    pub(crate) generation: u64,
    pub(crate) log_event_id: u64,
    /// `NetQueryRef query_ref_` — the in-flight cancel stays unmodeled;
    /// the answer's `generation` check subsumes the stale query (the
    /// finished flag is what the state machine reads).
    pub(crate) is_finished: bool,
}

/// `PollManager::PollOptionVoters` (PollManager.h:197-202).
#[derive(Default)]
pub struct PollOptionVoters {
    pub(crate) voters: Vec<(DialogId, i32)>,
    pub(crate) next_offset: String,
    pub(crate) pending_queries: Vec<Promise<td_api::PollVoters>>,
    pub(crate) was_invalidated: bool,
}

/// The PollManager in-flight query kinds — the C++ ResultHandler classes
/// of the net-drain 39 families (PollManager.cpp:67-332). The variants
/// carry the identity the C++ promise lambdas capture ([`PollId`] pairs
/// with a globally-unique `generation` where the C++ captures one), so an
/// out-of-order answer pairs with its own query (§5 — the query-carried
/// promise identity); the rest of the handler state rides the
/// [`PendingPollNetQueries`] lots. The kinds stay `Copy`.
#[derive(Clone, Copy)]
pub(crate) enum PollNetKind {
    /// `SendVoteQuery` (cpp:236) — `messages.sendVote`; the answer
    /// re-enters `on_set_poll_answer` with the captured generation
    /// (cpp:1381-1386). The generation is globally unique
    /// (`++current_generation_`), so it identifies the query even when a
    /// replaced vote's stale answer lands late.
    SendVote { generation: u64 },
    /// `GetPollResultsQuery` (cpp:67) — `messages.getPollResults`; the
    /// answer re-enters `on_get_poll_results` with the captured
    /// generation (cpp:1859-1866).
    GetPollResults { generation: u64 },
    /// `GetPollVotersQuery` (cpp:109) — `messages.getPollVotes`; the
    /// (poll_id, option_id) pair is unique among the in-flight queries
    /// (the pending_queries guard allows one per option).
    GetPollVoters { poll_id: PollId, option_id: i32 },
    /// `StopPollQuery` (cpp:278) — `messages.editMessage` with the
    /// closed-`inputMediaPoll`; one in-flight query per poll (the
    /// `being_closed_polls` guard).
    StopPoll { poll_id: PollId },
    /// `DeletePollAnswerQuery` (cpp:199) — `messages.deletePollAnswer`;
    /// the C++ handler carries only the promise, so the lot is a plain
    /// positional FIFO.
    DeletePollAnswer,
}

/// The parked query state of the net-drain 39 families — the
/// ResultHandler members the [`PollNetKind`] identities can't carry.
/// Entries pair with the wire answers by the kind-carried identity (a
/// positional FIFO for DeletePollAnswer, §5); the SendVote promise
/// parks in `pending_answers` and the GetPollVoters promises park in
/// `poll_voters[..].pending_queries` exactly like the C++.
#[derive(Default)]
pub(crate) struct PendingPollNetQueries {
    /// `SendVoteQuery`'s `dialog_id_` keyed by its generation — (poll_id,
    /// generation, dialog_id).
    pub(crate) send_vote: Vec<(PollId, u64, DialogId)>,
    /// `GetPollResultsQuery`'s `poll_id_`/`dialog_id_`/`message_id_`
    /// keyed by its generation.
    pub(crate) get_poll_results: Vec<(PollId, u64, DialogId, crate::messages_manager::MessageId)>,
    /// `GetPollVotersQuery`'s lambda captures — (offset at send, the
    /// un-maxed limit, dialog_id) keyed by (poll_id, option_id).
    pub(crate) get_poll_voters: Vec<(PollId, i32, String, i32, DialogId)>,
    /// `StopPollQuery` — (message_full_id, log_event_id, promise): the
    /// C++ `new_promise` lambda tuple (PollManager.cpp:1779-1784), keyed
    /// by the poll id.
    pub(crate) stop_poll: Vec<(PollId, MessageFullId, u64, Promise<()>)>,
    /// `DeletePollAnswerQuery::promise_` with the dialog id the error
    /// head reads — positional FIFO.
    pub(crate) delete_poll_answer: Vec<(DialogId, Promise<()>)>,
}

/// `PollManager` (PollManager.h:47) — the collapsed actor. The three
/// MultiTimeouts (`update_poll_timeout_`/`close_poll_timeout_`/
/// `unload_poll_timeout_`) are deadline maps drained by
/// [`PollManager::tick_timeouts`].
#[derive(Default)]
pub struct PollManager {
    pub(crate) polls: HashMap<PollId, Poll>,
    pub(crate) poll_was_saved: HashMap<PollId, bool>,
    pub(crate) server_poll_messages: HashMap<PollId, HashSet<MessageFullId>>,
    pub(crate) other_poll_messages: HashMap<PollId, HashSet<MessageFullId>>,
    pub(crate) reply_poll_counts: HashMap<PollId, i32>,
    pub(crate) pending_answers: HashMap<PollId, PendingPollAnswer>,
    pub(crate) poll_voters: HashMap<PollId, Vec<PollOptionVoters>>,
    /// The parked query state of the net-drain 39 families.
    pub(crate) pending_net: PendingPollNetQueries,
    pub(crate) loaded_from_database_polls: HashSet<PollId>,
    pub(crate) being_closed_polls: HashSet<PollId>,
    pub(crate) current_local_poll_id: i64,
    pub(crate) current_generation: u64,
    /// `update_poll_timeout_` — `add_timeout_in` keeps the earlier
    /// deadline, `set_timeout_in` replaces it (the MultiTimeout contract).
    pub(crate) update_poll_timeout: HashMap<i64, f64>,
    close_poll_timeout: HashMap<i64, f64>,
    unload_poll_timeout: HashMap<i64, f64>,
    /// The learned MultiTimeout clock — the last `env.now()` an
    /// env-carrying entry point saw. `schedule_poll_unload` deadlines
    /// against it; 0.0 = the clock is not known yet and no unload is
    /// armed (nothing fires without a `tick_timeouts`, which always
    /// learns the clock first).
    last_now: f64,
}

/// `MultiTimeout::add_timeout_in` — arm only when not already armed.
fn timeout_add(map: &mut HashMap<i64, f64>, key: i64, timeout: f64, now: f64) {
    map.entry(key).or_insert(now + timeout);
}

/// `MultiTimeout::set_timeout_in` — replace the deadline.
fn timeout_set(map: &mut HashMap<i64, f64>, key: i64, timeout: f64, now: f64) {
    map.insert(key, now + timeout);
}

impl PollManager {
    pub fn new() -> PollManager {
        PollManager::default()
    }

    /// `PollManager::get_poll` const (PollManager.cpp:471-473).
    pub fn get_poll(&self, poll_id: PollId) -> Option<&Poll> {
        self.polls.get(&poll_id)
    }

    /// `PollManager::get_poll` non-const (PollManager.cpp:475-481) — the
    /// mutable lookup schedules the unload.
    pub fn get_poll_and_schedule_unload(&mut self, poll_id: PollId) -> Option<&mut Poll> {
        if !self.polls.contains_key(&poll_id) {
            return None;
        }
        // the unload scheduling reads only the manager maps
        self.schedule_poll_unload(poll_id);
        self.polls.get_mut(&poll_id)
    }

    /// `PollManager::get_poll_editable` (PollManager.cpp:483-489).
    pub fn get_poll_editable(&mut self, poll_id: PollId) -> Option<&mut Poll> {
        if !self.polls.contains_key(&poll_id) {
            return None;
        }
        self.schedule_poll_unload(poll_id);
        self.polls.get_mut(&poll_id)
    }

    /// `PollManager::have_poll` (PollManager.cpp:491-493).
    pub fn have_poll(&self, poll_id: PollId) -> bool {
        self.polls.contains_key(&poll_id)
    }

    fn was_saved(&self, poll_id: PollId) -> bool {
        self.poll_was_saved.get(&poll_id).copied().unwrap_or(false)
    }

    fn set_was_saved(&mut self, poll_id: PollId, value: bool) {
        self.poll_was_saved.insert(poll_id, value);
    }

    /// `PollManager::notify_on_poll_update` (PollManager.cpp:495-511).
    pub fn notify_on_poll_update(&mut self, env: &mut impl PollManagerEnv, poll_id: PollId) {
        if env.auth_is_bot() {
            return;
        }
        let message_full_ids: Vec<MessageFullId> = self
            .server_poll_messages
            .get(&poll_id)
            .into_iter()
            .flatten()
            .copied()
            .chain(
                self.other_poll_messages
                    .get(&poll_id)
                    .into_iter()
                    .flatten()
                    .copied(),
            )
            .collect();
        for message_full_id in message_full_ids {
            env.on_external_update_message_content(message_full_id);
        }
    }

    /// `PollManager::notify_on_poll_has_unread_votes_update`
    /// (PollManager.cpp:513-523).
    fn notify_on_poll_has_unread_votes_update(
        &mut self,
        env: &mut impl PollManagerEnv,
        poll_id: PollId,
        has_unread_votes: bool,
    ) {
        if env.auth_is_bot() {
            return;
        }
        let message_full_ids: Vec<MessageFullId> = self
            .server_poll_messages
            .get(&poll_id)
            .into_iter()
            .flatten()
            .copied()
            .collect();
        for message_full_id in message_full_ids {
            env.on_update_poll_has_unread_votes(message_full_id, has_unread_votes);
        }
    }

    /// `PollManager::get_poll_database_key` (PollManager.cpp:525-527).
    fn get_poll_database_key(poll_id: PollId) -> Vec<u8> {
        format!("poll{}", poll_id.get()).into_bytes()
    }

    /// `PollManager::save_poll` (PollManager.cpp:529-540).
    fn save_poll(&mut self, env: &mut impl PollManagerEnv, poll_id: PollId) {
        debug_assert!(!is_local_poll_id(poll_id));
        self.set_was_saved(poll_id, true);
        if !env.use_message_database() {
            return;
        }
        let mut out = Vec::new();
        if let Some(poll) = self.polls.get(&poll_id) {
            poll.store(env, &mut out);
        }
        env.pmc_set_raw(&Self::get_poll_database_key(poll_id), &out);
    }

    /// `PollManager::on_load_poll_from_database` (PollManager.cpp:542-597).
    fn on_load_poll_from_database(
        &mut self,
        env: &mut impl PollManagerEnv,
        poll_id: PollId,
        value: Vec<u8>,
    ) {
        debug_assert!(poll_id.is_valid());
        self.loaded_from_database_polls.insert(poll_id);
        debug_assert!(!self.have_poll(poll_id));
        if value.is_empty() {
            return;
        }
        let mut parser = Parser::new(&value);
        let poll = match Poll::parse(env, &mut parser) {
            Ok(poll) => poll,
            Err(_) => return,
        };
        for (channel_id, min_channel) in poll
            .option_min_channels
            .iter()
            .chain(poll.recent_option_voter_min_channels.iter())
            .chain(poll.recent_voter_min_channels.iter())
        {
            env.add_min_channel(*channel_id, min_channel.clone());
        }
        // The Dependencies resolve_force half and the
        // register_poll_web_pages call ride the M.10/M.17 integrations;
        // the close-date arming below is the local half.
        let mut poll = poll;
        if !poll.is_closed && poll.close_date != 0 {
            if poll.close_date as f64 <= env.server_time() {
                poll.is_closed = true;
            } else if !env.close_flag() {
                timeout_set(
                    &mut self.close_poll_timeout,
                    poll_id.get(),
                    poll.close_date as f64 - env.server_time() + 1e-3,
                    env.now(),
                );
            }
        }
        self.polls.insert(poll_id, poll);
    }

    /// `PollManager::have_poll_force` (PollManager.cpp:599-601).
    pub fn have_poll_force(&mut self, env: &mut impl PollManagerEnv, poll_id: PollId) -> bool {
        self.get_poll_force(env, poll_id).is_some()
    }

    /// `PollManager::get_poll_force` (PollManager.cpp:603-618) — the
    /// mutable variant (the C++ const overload rides the same db half).
    pub fn get_poll_force(
        &mut self,
        env: &mut impl PollManagerEnv,
        poll_id: PollId,
    ) -> Option<&mut Poll> {
        if self.polls.contains_key(&poll_id) {
            return self.get_poll_editable(poll_id);
        }
        if !env.use_message_database() {
            return None;
        }
        if !poll_id.is_valid() || self.loaded_from_database_polls.contains(&poll_id) {
            return None;
        }
        let value = env.pmc_get_raw(&Self::get_poll_database_key(poll_id));
        self.on_load_poll_from_database(env, poll_id, value);
        self.get_poll_editable(poll_id)
    }

    /// `PollManager::is_poll_vote_subscribers_only_limited`
    /// (PollManager.cpp:724-736).
    fn is_poll_vote_subscribers_only_limited(
        &self,
        env: &impl PollManagerEnv,
        poll: &Poll,
        initial_dialog_id: DialogId,
        initial_date: i32,
    ) -> bool {
        if !poll.subscribers_only || initial_dialog_id.get_type() != DialogType::Channel {
            return false;
        }
        let channel_id = initial_dialog_id.get_channel_id();
        if !env.channel_is_member(channel_id) {
            return true;
        }
        let membership_delay = if env.is_test_dc() { 300 } else { 86400 };
        env.channel_date(channel_id)
            >= if initial_date > 0 {
                initial_date
            } else {
                env.unix_time()
            } - membership_delay
    }

    /// `PollManager::get_poll_vote_restriction_reason_object`
    /// (PollManager.cpp:738-773).
    #[allow(clippy::too_many_arguments)]
    fn get_poll_vote_restriction_reason_object(
        &self,
        env: &impl PollManagerEnv,
        poll_id: PollId,
        poll: &Poll,
        message_id: crate::chat_manager::MessageId,
        initial_dialog_id: DialogId,
        initial_date: i32,
        is_real_message_content: bool,
    ) -> Option<td_api::PollVoteRestrictionReason> {
        use td_api::PollVoteRestrictionReason as Reason;
        if env.auth_is_bot() {
            return None;
        }
        if poll.is_closed {
            return Some(Reason::pollVoteRestrictionReasonClosed(
                td_api::pollVoteRestrictionReasonClosed {},
            ));
        }
        if message_id.is_valid() || message_id.is_scheduled() {
            if message_id.is_local() {
                return Some(Reason::pollVoteRestrictionReasonClosed(
                    td_api::pollVoteRestrictionReasonClosed {},
                ));
            }
            if message_id.is_yet_unsent() {
                return Some(Reason::pollVoteRestrictionReasonYetUnsent(
                    td_api::pollVoteRestrictionReasonYetUnsent {},
                ));
            }
            if message_id.is_scheduled() {
                return Some(Reason::pollVoteRestrictionReasonScheduled(
                    td_api::pollVoteRestrictionReasonScheduled {},
                ));
            }
        }
        if !is_real_message_content {
            return Some(Reason::pollVoteRestrictionReasonOther(
                td_api::pollVoteRestrictionReasonOther {},
            ));
        }
        debug_assert!(!is_local_poll_id(poll_id));
        if !poll.country_codes.is_empty() {
            let country_code = env.get_option_string("phone_country_iso2");
            if !poll.country_codes.contains(&country_code) {
                return Some(Reason::pollVoteRestrictionReasonCountryRestricted(
                    td_api::pollVoteRestrictionReasonCountryRestricted { country_code },
                ));
            }
        }
        if self.is_poll_vote_subscribers_only_limited(env, poll, initial_dialog_id, initial_date) {
            return Some(Reason::pollVoteRestrictionReasonMembershipRequired(
                td_api::pollVoteRestrictionReasonMembershipRequired {
                    // The M.6 dialog plane carries `get_chat_id_object`;
                    // the raw chat id rides it until that integration.
                    chat_id: initial_dialog_id.get_chat_id().get(),
                },
            ));
        }
        None
    }

    /// `PollManager::get_poll_object` (PollManager.cpp:775-921) — the
    /// object builder over the record + the pending-answer overlay.
    pub fn get_poll_object(
        &mut self,
        env: &mut impl PollManagerEnv,
        poll_id: PollId,
        message_id: crate::chat_manager::MessageId,
        initial_dialog_id: DialogId,
        initial_date: i32,
        is_real_message_content: bool,
    ) -> td_api::Poll {
        self.last_now = env.now();
        debug_assert!(self.have_poll(poll_id), "poll {poll_id:?} not found");
        let poll = self.get_poll(poll_id).expect("poll not found").clone();
        let is_bot = env.auth_is_bot();
        let my_id = env.get_my_id().get();

        // pollOption objects (PollOption::get_poll_option_object,
        // PollOption.cpp:118-130) over the record + the pending overlay.
        let mut poll_options: Vec<td_api::PollOption> = Vec::with_capacity(poll.options.len());
        for option in &poll.options {
            let author = if option.added_by_dialog_id.is_valid() {
                env.get_min_message_sender_object(option.added_by_dialog_id)
            } else {
                None
            };
            let is_added = author.is_some();
            let recent_voter_ids: Vec<td_api::MessageSender> = option
                .recent_voter_dialog_ids
                .iter()
                .filter_map(|&dialog_id| env.get_min_message_sender_object(dialog_id))
                .collect();
            poll_options.push(td_api::PollOption {
                id: option.data.clone(),
                text: Some(crate::message_entity::get_formatted_text_object::<()>(
                    None,
                    &option.text,
                    true,
                    -1,
                )),
                media: env.get_poll_media_object(option.media_type),
                voter_count: option.voter_count,
                vote_percentage: 0,
                recent_voter_ids,
                is_chosen: option.is_chosen,
                is_being_chosen: false,
                author,
                addition_date: if is_added { option.added_date } else { 0 },
            });
        }
        let mut recent_voters: Vec<td_api::MessageSender> = poll
            .recent_voter_dialog_ids
            .iter()
            .filter_map(|&dialog_id| env.get_min_message_sender_object(dialog_id))
            .collect();

        // The pending-answer overlay (PollManager.cpp:793-823)
        let mut hide_correct_options = false;
        let mut voter_count_diff = 0;
        let pending_is_live = self
            .pending_answers
            .get(&poll_id)
            .map(|pending| !(pending.is_finished && self.was_saved(poll_id)))
            .unwrap_or(false);
        if pending_is_live {
            let chosen_options = self.pending_answers.get(&poll_id).unwrap().options.clone();
            let remove_self = |voters: &mut Vec<td_api::MessageSender>| {
                if let Some(pos) = voters.iter().position(|voter| match voter {
                    td_api::MessageSender::messageSenderUser(user) => user.user_id == my_id,
                    _ => false,
                }) {
                    voters.remove(pos);
                }
            };
            for poll_option in &mut poll_options {
                poll_option.is_being_chosen = chosen_options.contains(&poll_option.id);
                if poll_option.is_chosen && !poll_option.is_being_chosen {
                    voter_count_diff = -1;
                    poll_option.voter_count -= 1;
                    poll_option.is_chosen = false;
                    remove_self(&mut poll_option.recent_voter_ids);
                }
            }
            if chosen_options.is_empty() {
                remove_self(&mut recent_voters);
                if !is_bot && !poll.is_creator {
                    hide_correct_options = true;
                }
            }
        }
        // The non-UTF-8 option id base64 fallback (PollManager.cpp:824-831)
        if poll_options
            .iter()
            .any(|option| !tdutils::utf8::check_utf8(option.id.as_bytes()))
        {
            for poll_option in &mut poll_options {
                poll_option.id = base64_encode(poll_option.id.as_bytes());
            }
        }

        let mut total_voter_count = poll.total_voter_count + voter_count_diff;
        let can_get_voters =
            self.can_get_poll_voters(env, poll_id, &poll, initial_dialog_id, initial_date)
                && is_real_message_content;
        let can_see_results = can_get_voters || is_bot;
        if !can_see_results {
            // hide the voter counts
            for poll_option in &mut poll_options {
                poll_option.voter_count = 0;
                poll_option.recent_voter_ids.clear();
            }
        } else {
            // calculate vote percentage and fix total_voter_count
            let voter_counts: Vec<i32> = poll_options
                .iter()
                .map(|option| option.voter_count)
                .collect();
            let mut voter_count_sum = 0;
            for &voter_count in &voter_counts {
                if total_voter_count < voter_count {
                    total_voter_count = voter_count;
                }
                voter_count_sum += voter_count;
            }
            if voter_count_sum < total_voter_count && voter_count_sum != 0 {
                total_voter_count = voter_count_sum;
            }
            let vote_percentage = get_vote_percentage(&voter_counts, total_voter_count);
            debug_assert_eq!(poll_options.len(), vote_percentage.len());
            for (poll_option, vote_percentage) in poll_options.iter_mut().zip(vote_percentage) {
                poll_option.vote_percentage = vote_percentage;
            }
        }

        let poll_type = if poll.is_quiz {
            if is_local_poll_id(poll_id) || hide_correct_options {
                td_api::PollType::pollTypeQuiz(td_api::pollTypeQuiz {
                    correct_option_ids: Vec::new(),
                    explanation: Some(crate::message_entity::get_formatted_text_object::<()>(
                        None,
                        &FormattedText::default(),
                        true,
                        -1,
                    )),
                    explanation_media: None,
                })
            } else {
                td_api::PollType::pollTypeQuiz(td_api::pollTypeQuiz {
                    correct_option_ids: poll.correct_option_ids.clone(),
                    explanation: Some(crate::message_entity::get_formatted_text_object::<()>(
                        None,
                        &poll.explanation,
                        true,
                        -1,
                    )),
                    explanation_media: env.get_poll_media_object(poll.explanation_media_type),
                })
            }
        } else {
            td_api::PollType::pollTypeRegular(td_api::pollTypeRegular {})
        };

        let mut open_period = poll.open_period;
        let mut close_date = poll.close_date;
        if open_period != 0 && close_date == 0 {
            close_date = env.unix_time() + open_period;
        }
        if open_period == 0 && close_date != 0 {
            let now = env.unix_time();
            if close_date < now + 5 {
                close_date = 0;
            } else {
                open_period = close_date - now;
            }
        }
        if poll.is_closed {
            open_period = 0;
            close_date = 0;
        }

        // The per-user shuffle order (PollManager.cpp:899-910) —
        // `crc32(PSLICE() << my_user_id << option_id << poll_id)`: the
        // i64 big-endian bytes of the store_long framing, then the raw
        // option id bytes, then the poll id.
        let mut option_order: Vec<i32> = Vec::new();
        if poll.shuffle_answers && !poll.is_creator && !is_bot {
            let mut sort_pairs: Vec<(u32, i32)> = Vec::with_capacity(poll_options.len());
            for (i, poll_option) in poll_options.iter().enumerate() {
                let mut data = Vec::new();
                store_i64(&mut data, my_id);
                data.extend_from_slice(poll_option.id.as_bytes());
                store_i64(&mut data, poll_id.get());
                sort_pairs.push((tdutils::crc32::crc32(&data), i as i32));
            }
            sort_pairs.sort();
            option_order = sort_pairs.into_iter().map(|(_, i)| i).collect();
        }

        let vote_restriction_reason = self.get_poll_vote_restriction_reason_object(
            env,
            poll_id,
            &poll,
            message_id,
            initial_dialog_id,
            initial_date,
            is_real_message_content,
        );
        td_api::Poll {
            id: poll_id.get(),
            question: Some(crate::message_entity::get_formatted_text_object::<()>(
                None,
                &poll.question,
                true,
                -1,
            )),
            options: poll_options,
            total_voter_count,
            recent_voter_ids: recent_voters,
            can_get_voters: can_get_voters
                && !poll.is_anonymous
                && message_id.is_server()
                && !is_local_poll_id(poll_id),
            can_see_results,
            is_anonymous: poll.is_anonymous,
            allows_multiple_answers: poll.allow_multiple_answers,
            allows_revoting: !poll.has_revoting_disabled,
            members_only: poll.subscribers_only,
            country_codes: poll.country_codes.clone(),
            option_order,
            r#type: Some(poll_type),
            open_period,
            close_date,
            is_closed: poll.is_closed,
            vote_restriction_reason,
        }
    }

    /// `PollManager::create_poll` (PollManager.cpp:923-959).
    #[allow(clippy::too_many_arguments)]
    pub fn create_poll(
        &mut self,
        mut question: FormattedText,
        options: Vec<PollOption>,
        is_anonymous: bool,
        allow_multiple_answers: bool,
        has_open_answers: bool,
        has_revoting_disabled: bool,
        subscribers_only: bool,
        country_codes: Vec<String>,
        shuffle_answers: bool,
        hide_results_until_close: bool,
        is_quiz: bool,
        correct_option_ids: Vec<i32>,
        explanation: FormattedText,
        explanation_media_type: Option<i32>,
        open_period: i32,
        close_date: i32,
        is_closed: bool,
    ) -> PollId {
        let has_open_answers = if is_quiz && has_open_answers {
            false
        } else {
            has_open_answers
        };
        crate::message_entity::keep_only_custom_emoji(&mut question);
        let poll = Poll {
            question,
            options,
            is_anonymous,
            allow_multiple_answers,
            has_open_answers,
            has_revoting_disabled,
            subscribers_only,
            country_codes,
            shuffle_answers,
            hide_results_until_close,
            is_quiz,
            correct_option_ids,
            explanation,
            explanation_media_type,
            open_period,
            close_date,
            is_closed,
            is_creator: true,
            can_view_stats: false,
            ..Poll::default()
        };
        self.current_local_poll_id -= 1;
        let poll_id = PollId::new(self.current_local_poll_id);
        debug_assert!(is_local_poll_id(poll_id));
        self.polls.insert(poll_id, poll);
        poll_id
    }

    /// `PollManager::register_poll` (PollManager.cpp:961-981).
    pub fn register_poll(
        &mut self,
        env: &mut impl PollManagerEnv,
        poll_id: PollId,
        message_full_id: MessageFullId,
    ) {
        self.last_now = env.now();
        debug_assert!(self.have_poll(poll_id));
        let message_id = message_full_id.get_message_id();
        if message_id.is_scheduled() || !message_id.is_server() {
            self.other_poll_messages
                .entry(poll_id)
                .or_default()
                .insert(message_full_id);
            if !env.close_flag() {
                self.unload_poll_timeout.remove(&poll_id.get());
            }
            return;
        }
        self.server_poll_messages
            .entry(poll_id)
            .or_default()
            .insert(message_full_id);
        let (is_closed, is_updated_after_close) = match self.polls.get(&poll_id) {
            Some(poll) => (poll.is_closed, poll.is_updated_after_close),
            None => (false, false),
        };
        if !(env.auth_is_bot()
            || is_local_poll_id(poll_id)
            || env.close_flag()
            || (is_closed && is_updated_after_close))
        {
            timeout_add(&mut self.update_poll_timeout, poll_id.get(), 0.0, env.now());
        }
        if !env.close_flag() {
            self.unload_poll_timeout.remove(&poll_id.get());
        }
    }

    /// `PollManager::unregister_poll` (PollManager.cpp:983-1016).
    pub fn unregister_poll(
        &mut self,
        env: &mut impl PollManagerEnv,
        poll_id: PollId,
        message_full_id: MessageFullId,
    ) {
        self.last_now = env.now();
        debug_assert!(self.have_poll(poll_id));
        let message_id = message_full_id.get_message_id();
        if message_id.is_scheduled() || !message_id.is_server() {
            let is_local = is_local_poll_id(poll_id);
            if let Some(message_ids) = self.other_poll_messages.get_mut(&poll_id) {
                message_ids.remove(&message_full_id);
                if is_local {
                    debug_assert!(message_ids.is_empty());
                }
                if message_ids.is_empty() {
                    self.other_poll_messages.remove(&poll_id);
                    self.schedule_poll_unload(poll_id);
                }
            }
            if is_local {
                self.forget_local_poll(env, poll_id);
            }
            return;
        }
        let is_local = is_local_poll_id(poll_id);
        if let Some(message_ids) = self.server_poll_messages.get_mut(&poll_id) {
            message_ids.remove(&message_full_id);
            if is_local {
                debug_assert!(message_ids.is_empty());
            }
            if message_ids.is_empty() {
                self.server_poll_messages.remove(&poll_id);
                if !env.close_flag() {
                    self.update_poll_timeout.remove(&poll_id.get());
                }
                self.schedule_poll_unload(poll_id);
            }
        }
        if is_local {
            self.forget_local_poll(env, poll_id);
        }
    }

    /// `PollManager::register_reply_poll` (PollManager.cpp:1018-1025).
    pub fn register_reply_poll(&mut self, env: &mut impl PollManagerEnv, poll_id: PollId) {
        debug_assert!(self.have_poll(poll_id));
        *self.reply_poll_counts.entry(poll_id).or_default() += 1;
        if !env.close_flag() {
            self.unload_poll_timeout.remove(&poll_id.get());
        }
    }

    /// `PollManager::unregister_reply_poll` (PollManager.cpp:1027-1041).
    pub fn unregister_reply_poll(&mut self, env: &mut impl PollManagerEnv, poll_id: PollId) {
        debug_assert!(self.have_poll(poll_id));
        let count = self.reply_poll_counts.get_mut(&poll_id).expect("counted");
        debug_assert!(*count > 0);
        *count -= 1;
        let is_local = is_local_poll_id(poll_id);
        if is_local {
            debug_assert!(*count == 0);
        }
        if *count == 0 {
            self.reply_poll_counts.remove(&poll_id);
            self.schedule_poll_unload(poll_id);
        }
        if is_local {
            self.forget_local_poll(env, poll_id);
        }
    }

    /// `PollManager::can_unload_poll` (PollManager.cpp:1043-1063).
    fn can_unload_poll(&self, poll_id: PollId) -> bool {
        if is_local_poll_id(poll_id)
            || self.server_poll_messages.contains_key(&poll_id)
            || self.other_poll_messages.contains_key(&poll_id)
            || self.reply_poll_counts.contains_key(&poll_id)
            || self.pending_answers.contains_key(&poll_id)
            || self.being_closed_polls.contains(&poll_id)
        {
            return false;
        }
        if let Some(poll_voters) = self.poll_voters.get(&poll_id) {
            for voters in poll_voters {
                if !voters.pending_queries.is_empty() {
                    return false;
                }
            }
        }
        true
    }

    /// `PollManager::schedule_poll_unload` (PollManager.cpp:1065-1069).
    fn schedule_poll_unload(&mut self, poll_id: PollId) {
        if self.can_unload_poll(poll_id) && self.last_now != 0.0 {
            timeout_set(
                &mut self.unload_poll_timeout,
                poll_id.get(),
                Self::UNLOAD_POLL_DELAY as f64,
                self.last_now,
            );
        }
    }

    /// `PollManager::get_poll_is_closed` (PollManager.cpp:1071-1075).
    pub fn get_poll_is_closed(&self, poll_id: PollId) -> bool {
        self.get_poll(poll_id).expect("poll not found").is_closed
    }

    /// `PollManager::get_poll_is_anonymous` (PollManager.cpp:1077-1081).
    pub fn get_poll_is_anonymous(&self, poll_id: PollId) -> bool {
        self.get_poll(poll_id).expect("poll not found").is_anonymous
    }

    /// `PollManager::get_poll_can_add_option` (PollManager.cpp:1083-1091).
    pub fn get_poll_can_add_option(&self, env: &impl PollManagerEnv, poll_id: PollId) -> bool {
        if is_local_poll_id(poll_id) {
            return false;
        }
        let poll = self.get_poll(poll_id).expect("poll not found");
        poll.has_open_answers
            && !poll.is_closed
            && (poll.options.len() as i64) < env.get_option_integer("poll_answer_count_max", 0)
    }

    /// `PollManager::get_poll_can_view_stats` (PollManager.cpp:1093-1097).
    pub fn get_poll_can_view_stats(&self, poll_id: PollId) -> bool {
        self.get_poll(poll_id)
            .expect("poll not found")
            .can_view_stats
    }

    /// `PollManager::get_poll_has_unread_votes` (PollManager.cpp:1099-1103).
    pub fn get_poll_has_unread_votes(&self, poll_id: PollId) -> bool {
        self.get_poll(poll_id)
            .expect("poll not found")
            .has_unread_votes
    }

    /// `PollManager::remove_poll_has_unread_votes`
    /// (PollManager.cpp:1105-1115).
    pub fn remove_poll_has_unread_votes(&mut self, env: &mut impl PollManagerEnv, poll_id: PollId) {
        let poll = self.get_poll_editable(poll_id).expect("poll not found");
        if !poll.has_unread_votes {
            return;
        }
        poll.has_unread_votes = false;
        self.save_poll(env, poll_id);
    }

    /// `PollManager::can_delete_poll_option`
    /// (PollManager.cpp:1117-1141).
    fn can_delete_poll_option(
        &self,
        env: &impl PollManagerEnv,
        poll: &Poll,
        option: &PollOption,
        message_id: crate::chat_manager::MessageId,
        is_forward: bool,
        is_outgoing: bool,
    ) -> bool {
        if poll.is_closed || !poll.has_open_answers {
            return false;
        }
        if !message_id.is_server() || is_forward {
            return false;
        }
        let added_by_dialog_id = option.added_by_dialog_id;
        if !added_by_dialog_id.is_valid() {
            return false;
        }
        if is_outgoing {
            return true;
        }
        added_by_dialog_id == env.get_my_dialog_id()
            && env.unix_time()
                < option.added_date + env.get_option_integer("poll_answer_delete_period", 0) as i32
    }

    /// `PollManager::get_poll_option_properties`
    /// (PollManager.cpp:1143-1170).
    #[allow(clippy::too_many_arguments)]
    pub fn get_poll_option_properties(
        &mut self,
        env: &mut impl PollManagerEnv,
        poll_id: PollId,
        option_id: &str,
        dialog_id: DialogId,
        message_id: crate::chat_manager::MessageId,
        mut can_be_replied: bool,
        mut can_be_replied_in_another_chat: bool,
        mut can_get_link: bool,
        is_forward: bool,
        is_outgoing: bool,
    ) -> Result<td_api::PollOptionProperties, tdutils::status::Status> {
        let poll = self.get_poll(poll_id).expect("poll not found").clone();
        let option = poll.options.iter().find(|option| option.data == option_id);
        let Some(option) = option else {
            if option_id.is_empty() {
                return Ok(td_api::PollOptionProperties {
                    can_be_deleted: false,
                    can_be_replied: false,
                    can_be_replied_in_another_chat: false,
                    can_get_link: false,
                });
            }
            return Err(tdutils::status::Status::error(400, "Poll option not found"));
        };
        let can_be_deleted =
            self.can_delete_poll_option(env, &poll, option, message_id, is_forward, is_outgoing);
        if dialog_id.get_type() == DialogType::SecretChat || !message_id.is_server() {
            can_be_replied = false;
            can_be_replied_in_another_chat = false;
            can_get_link = false;
        }
        Ok(td_api::PollOptionProperties {
            can_be_deleted,
            can_be_replied,
            can_be_replied_in_another_chat,
            can_get_link,
        })
    }

    /// `PollManager::get_poll_search_text` (PollManager.cpp:1172-1182).
    pub fn get_poll_search_text(&self, poll_id: PollId) -> String {
        let poll = self.get_poll(poll_id).expect("poll not found");
        let mut result = poll.question.text.clone();
        for option in &poll.options {
            result.push(' ');
            result.push_str(option.get_search_text());
        }
        result
    }

    /// `PollManager::get_poll_web_page_ids` (PollManager.cpp:2630-2640) —
    /// the option-media web pages (the media rides the M.9 seam; the
    /// record plane carries no web pages on this line).
    fn get_poll_web_page_ids(_poll: &Poll) -> Vec<i64> {
        Vec::new()
    }

    /// `PollManager::add_poll_option` (PollManager.cpp:1198-1215) — the
    /// poll-id seam first; the upload-message-content machinery
    /// (`message_query_manager_->create_upload_message_content_query`) is
    /// [blocked: M.10 integration] and the input-poll media validation is
    /// [blocked: M.9]: the promise parks unanswered past the seam.
    pub fn add_poll_option(
        &mut self,
        env: &mut impl PollManagerEnv,
        message_full_id: MessageFullId,
        _option: Option<td_api::InputPollOption>,
        mut promise: Promise<()>,
    ) {
        if let Err(status) = env.get_message_poll_id(&message_full_id, false) {
            promise.set_error(status);
        }
        // [blocked: M.10 integration] + [blocked: M.9]: the option text
        // validation half + the upload tail.
    }

    /// `PollManager::delete_poll_option` (PollManager.cpp:1233-1236) —
    /// the `DeletePollAnswerQuery` send: the C++ handler holds only the
    /// promise, so the lot is a positional FIFO; the send-leg None-peer
    /// band routes the handler's on_error head (dialog error + the
    /// promise error).
    pub fn delete_poll_option(
        &mut self,
        env: &mut impl PollManagerEnv,
        message_full_id: MessageFullId,
        option_id: String,
        mut promise: Promise<()>,
    ) {
        // TRY_STATUS_PROMISE(promise, get_message_poll_id(…)) (cpp:1234)
        if let Err(status) = env.get_message_poll_id(&message_full_id, false) {
            promise.set_error(status);
            return;
        }
        let dialog_id = message_full_id.get_dialog_id();
        self.pending_net
            .delete_poll_answer
            .push((dialog_id, promise));
        if let Err(status) =
            env.send_delete_poll_answer_query(&message_full_id, option_id.into_bytes())
        {
            // `DeletePollAnswerQuery::send`'s None-peer band (cpp:206-209)
            // runs the on_error head with the parked promise
            let (_, mut promise) = self.pending_net.delete_poll_answer.pop().unwrap();
            env.on_get_dialog_error(dialog_id, &status, "DeletePollAnswerQuery");
            promise.set_error(status);
        }
    }

    /// `PollManager::set_poll_answer` (PollManager.cpp:1238-1287).
    pub fn set_poll_answer(
        &mut self,
        env: &mut impl PollManagerEnv,
        message_full_id: MessageFullId,
        option_ids: Vec<i32>,
        mut promise: Promise<()>,
    ) {
        let poll_id = match env.get_message_poll_id(&message_full_id, false) {
            Ok(poll_id) => poll_id,
            Err(status) => return promise.set_error(status),
        };
        let mut option_ids = option_ids;
        option_ids.sort_unstable();
        option_ids.dedup();

        if is_local_poll_id(poll_id) {
            return promise.set_error(tdutils::status::Status::error(
                400,
                "Poll can't be answered",
            ));
        }
        let poll = match self.get_poll(poll_id) {
            Some(poll) => poll.clone(),
            None => return,
        };
        if poll.is_closed {
            return promise.set_error(tdutils::status::Status::error(
                400,
                "Can't answer closed poll",
            ));
        }
        if !poll.allow_multiple_answers && option_ids.len() > 1 {
            return promise.set_error(tdutils::status::Status::error(
                400,
                "Can't choose more than 1 option in the poll",
            ));
        }
        if poll.has_revoting_disabled && option_ids.is_empty() {
            return promise.set_error(tdutils::status::Status::error(
                400,
                "Can't retract vote in the poll",
            ));
        }
        if poll.has_revoting_disabled && self.pending_answers.contains_key(&poll_id) {
            return promise.set_error(tdutils::status::Status::error(
                400,
                "Can't revote in the poll",
            ));
        }

        let mut affected_option_ids: HashMap<u64, i32> = HashMap::new();
        let mut options: Vec<String> = Vec::with_capacity(option_ids.len());
        for option_id in option_ids {
            let index = option_id as usize;
            if index >= poll.options.len() {
                return promise.set_error(tdutils::status::Status::error(
                    400,
                    "Invalid option identifier specified",
                ));
            }
            options.push(poll.options[index].data.clone());
            *affected_option_ids.entry(index as u64 + 1).or_default() += 1;
        }
        for option_index in 0..poll.options.len() {
            if poll.options[option_index].is_chosen {
                if poll.has_revoting_disabled {
                    return promise.set_error(tdutils::status::Status::error(
                        400,
                        "Can't revote in a quiz",
                    ));
                }
                *affected_option_ids
                    .entry(option_index as u64 + 1)
                    .or_default() += 1;
            }
        }
        for (option_index, count) in affected_option_ids {
            if count == 1 {
                self.invalidate_poll_option_voters(&poll, poll_id, (option_index - 1) as usize);
            }
        }

        self.do_set_poll_answer(env, poll_id, message_full_id, options, 0, promise);
    }

    /// `PollManager::do_set_poll_answer` (PollManager.cpp:1310-1388).
    pub(crate) fn do_set_poll_answer(
        &mut self,
        env: &mut impl PollManagerEnv,
        poll_id: PollId,
        message_full_id: MessageFullId,
        options: Vec<String>,
        log_event_id: u64,
        promise: Promise<()>,
    ) {
        if !poll_id.is_valid()
            || !message_full_id.get_dialog_id().is_valid()
            || !message_full_id.get_message_id().is_valid()
        {
            debug_assert!(log_event_id != 0);
            env.binlog_erase(log_event_id);
            return;
        }
        if !env.close_flag() {
            self.unload_poll_timeout.remove(&poll_id.get());
        }

        let pending_answer = self.pending_answers.entry(poll_id).or_default();
        if !pending_answer.promises.is_empty() && pending_answer.options == options {
            pending_answer.promises.push(promise);
            return;
        }
        let had_promises = !pending_answer.promises.is_empty();
        let old_generation = pending_answer.generation;
        let old_is_finished = pending_answer.is_finished;
        let old_log_event_id = pending_answer.log_event_id;

        let mut log_event_id = log_event_id;
        if pending_answer.log_event_id != 0 && log_event_id != 0 {
            env.binlog_erase(log_event_id);
            return;
        }
        if log_event_id == 0 && env.use_message_database() {
            let log_event = SetPollAnswerLogEvent {
                poll_id,
                message_full_id,
                options: options.clone(),
            };
            let mut data = Vec::new();
            log_event.store(&mut data);
            if old_generation == 0 || old_is_finished {
                debug_assert!(old_log_event_id == 0);
                log_event_id = env.binlog_add(SET_POLL_ANSWER_HANDLER_TYPE, &data);
                debug_assert!(log_event_id != 0);
            } else {
                debug_assert!(old_log_event_id != 0);
                log_event_id = old_log_event_id;
                env.binlog_rewrite(log_event_id, SET_POLL_ANSWER_HANDLER_TYPE, &data);
            }
        }

        // The in-flight query cancel + promise release of the C++ happens
        // when a DIFFERENT answer replaces it; the wire answer of the
        // replaced query rides a stale generation and returns before any
        // state touch — the generation check subsumes the cancel. Only
        // the promises release here.
        if had_promises {
            let promises =
                std::mem::take(&mut self.pending_answers.get_mut(&poll_id).unwrap().promises);
            for mut old_promise in promises {
                old_promise.set_value(());
            }
        }

        // the C++ `sent_options` copy (cpp:1360-1364) — the option data
        // rides the wire before `options` moves into the pending record
        let sent_options: Vec<String> = options.clone();

        self.current_generation += 1;
        let generation = self.current_generation;

        let pending_answer = self.pending_answers.get_mut(&poll_id).unwrap();
        pending_answer.options = options;
        pending_answer.promises.push(promise);
        pending_answer.generation = generation;
        pending_answer.log_event_id = log_event_id;
        pending_answer.is_finished = false;

        self.notify_on_poll_update(env, poll_id);

        // `SendVoteQuery::send` (PollManager.cpp:247-265) — the lot parks
        // the handler's `dialog_id_` keyed by the generation; the
        // send-leg None-peer band runs the handler's on_error head
        // (dialog error) and routes the error through the full answer
        // ladder (`on_set_poll_answer` with the 400), exactly the C++
        // `return on_error(Status)` tail. The promise stays in
        // pending_answers either way.
        let dialog_id = message_full_id.get_dialog_id();
        self.pending_net
            .send_vote
            .push((poll_id, generation, dialog_id));
        if let Err(status) = env.send_vote_query(&message_full_id, sent_options, generation) {
            // no query was dispatched — the handler dies at the send leg
            // (the C++ `return on_error(...)`); its lot entry goes with it
            let index = self
                .pending_net
                .send_vote
                .iter()
                .position(|&(_, generation_, _)| generation_ == generation)
                .unwrap();
            self.pending_net.send_vote.remove(index);
            tdutils::log_info!("Can't set poll answer, because have no read access to {dialog_id}");
            env.on_get_dialog_error(dialog_id, &status, "SendVoteQuery");
            self.on_set_poll_answer_wire(env, poll_id, generation, Err(status));
        }
    }

    /// The `SendVoteQuery` answer — the C++ handler's `on_result`/
    /// `on_error` (PollManager.cpp:266-306): the lot pairs the answer
    /// with its query by the kind-carried generation; the error band
    /// runs the dialog-error head, then the promise tail re-enters
    /// `on_set_poll_answer`. Returns the parsed updates + the
    /// (poll_id, generation) pair for the router's engine feed (the
    /// `Unit()` lambda tail finishes the ladder); `None` when the
    /// ladder settled (or dropped) without one.
    pub(crate) fn on_send_vote_answer(
        &mut self,
        env: &mut impl PollManagerEnv,
        generation: u64,
        wire: std::result::Result<tdmtproto::telegram_api::Updates, tdutils::status::Status>,
    ) -> Option<(tdmtproto::telegram_api::Updates, PollId, u64)> {
        let Some(index) = self
            .pending_net
            .send_vote
            .iter()
            .position(|&(_, generation_, _)| generation_ == generation)
        else {
            // an answer for a query this manager never sent (or already
            // answered) — the C++ handler instance is gone either way
            return None;
        };
        let (poll_id, _, dialog_id) = self.pending_net.send_vote.remove(index);
        if let Err(status) = &wire {
            // `SendVoteQuery::on_error` (cpp:299-304)
            env.on_get_dialog_error(dialog_id, status, "SendVoteQuery");
        }
        self.on_set_poll_answer_wire(env, poll_id, generation, wire)
            .map(|updates| (updates, poll_id, generation))
    }

    /// `PollManager::on_set_poll_answer` (PollManager.cpp:1390-1429) —
    /// the query-result half: the close_flag drop, the pending-entry +
    /// generation guards, the binlog erase + `is_finished` pre-section,
    /// then either the updates feed (the router passes the parsed
    /// answer; the `Unit()` lambda tail finishes the ladder) or the
    /// direct error finish. Returns the parsed updates for the router's
    /// engine feed; `None` when the ladder settled (or dropped) without
    /// one. The binlog replay reaches `do_set_poll_answer` directly.
    pub(crate) fn on_set_poll_answer_wire(
        &mut self,
        env: &mut impl PollManagerEnv,
        poll_id: PollId,
        generation: u64,
        wire: std::result::Result<tdmtproto::telegram_api::Updates, tdutils::status::Status>,
    ) -> Option<tdmtproto::telegram_api::Updates> {
        if env.close_flag() && wire.is_err() {
            // the request will be re-sent after restart (cpp:1393-1395)
            return None;
        }
        // can happen if this is an answer with mismatched generation
        // and server has ignored invoke-after (cpp:1396-1399)
        self.pending_answers.get(&poll_id)?;
        let pending_generation = self.pending_answers.get(&poll_id).unwrap().generation;
        if pending_generation != generation {
            return None;
        }
        debug_assert!(
            !self
                .pending_answers
                .get(&poll_id)
                .unwrap()
                .promises
                .is_empty()
        );
        let log_event_id = self.pending_answers.get(&poll_id).unwrap().log_event_id;
        if log_event_id != 0 {
            env.binlog_erase(log_event_id);
            self.pending_answers.get_mut(&poll_id).unwrap().log_event_id = 0;
        }
        self.pending_answers.get_mut(&poll_id).unwrap().is_finished = true;
        if self.polls.contains_key(&poll_id) {
            self.set_was_saved(poll_id, false);
        }
        match wire {
            // the router feeds the engine and the `Unit()` lambda tail
            // (cpp:1417-1422) finishes the ladder with Ok(())
            Ok(updates) => Some(updates),
            Err(status) => {
                self.on_set_poll_answer_finished(env, poll_id, Err(status), generation);
                None
            }
        }
    }

    /// `PollManager::on_set_poll_answer_finished`
    /// (PollManager.cpp:1431-1469).
    pub(crate) fn on_set_poll_answer_finished(
        &mut self,
        env: &mut impl PollManagerEnv,
        poll_id: PollId,
        result: std::result::Result<(), tdutils::status::Status>,
        generation: u64,
    ) {
        let Some(pending_answer) = self.pending_answers.get(&poll_id) else {
            return;
        };
        debug_assert!(!pending_answer.promises.is_empty());
        if pending_answer.generation != generation {
            return;
        }
        debug_assert!(pending_answer.is_finished);
        let promises = self.pending_answers.remove(&poll_id).unwrap().promises;

        if !env.close_flag() {
            let poll = self
                .polls
                .get(&poll_id)
                .map(|poll| (poll.is_closed, poll.is_updated_after_close));
            let was_saved = self.was_saved(poll_id);
            if let Some((is_closed, is_updated_after_close)) = poll
                && !was_saved
            {
                if !(is_closed && is_updated_after_close) {
                    timeout_set(&mut self.update_poll_timeout, poll_id.get(), 0.0, env.now());
                }
                self.notify_on_poll_update(env, poll_id);
                self.set_was_saved(poll_id, true);
            }
        }

        match result {
            Ok(()) => {
                for mut promise in promises {
                    promise.set_value(());
                }
            }
            Err(status) => {
                for mut promise in promises {
                    promise.set_error(tdutils::status::Status::error(
                        status.code(),
                        status.message(),
                    ));
                }
            }
        }
    }

    /// `PollManager::invalidate_poll_voters` (PollManager.cpp:1471-1484).
    fn invalidate_poll_voters(&mut self, poll: &Poll, poll_id: PollId) {
        if poll.is_anonymous {
            return;
        }
        if let Some(poll_voters) = self.poll_voters.get_mut(&poll_id) {
            for voters in poll_voters {
                voters.was_invalidated = true;
            }
        }
    }

    /// `PollManager::invalidate_poll_option_voters`
    /// (PollManager.cpp:1486-1500).
    fn invalidate_poll_option_voters(&mut self, poll: &Poll, poll_id: PollId, option_index: usize) {
        if poll.is_anonymous {
            return;
        }
        if let Some(poll_voters) = self.poll_voters.get_mut(&poll_id) {
            debug_assert_eq!(poll_voters.len(), poll.options.len());
            debug_assert!(option_index < poll_voters.len());
            poll_voters[option_index].was_invalidated = true;
        }
    }

    /// `PollManager::get_poll_option_voters` (PollManager.cpp:1502-1510).
    fn get_poll_option_voters(
        &mut self,
        poll: &Poll,
        poll_id: PollId,
        option_id: i32,
    ) -> &mut PollOptionVoters {
        let poll_voters = self.poll_voters.entry(poll_id).or_default();
        if poll_voters.is_empty() {
            poll_voters.resize_with(poll.options.len(), PollOptionVoters::default);
        }
        let index = option_id as usize;
        debug_assert!(index < poll_voters.len());
        &mut poll_voters[index]
    }

    /// `PollManager::get_poll_voters_object` (PollManager.cpp:1512-1523).
    fn get_poll_voters_object(
        &mut self,
        env: &mut impl PollManagerEnv,
        total_count: i32,
        voters: &[(DialogId, i32)],
    ) -> td_api::PollVoters {
        let mut result = td_api::PollVoters {
            total_count,
            voters: Vec::with_capacity(voters.len()),
        };
        for &(dialog_id, date) in voters {
            if let Some(voter_id) = env.get_min_message_sender_object(dialog_id) {
                result.voters.push(td_api::PollVoter {
                    voter_id: Some(voter_id),
                    date,
                });
            }
        }
        result
    }

    /// `PollManager::can_get_poll_voters` (PollManager.cpp:1525-1552).
    fn can_get_poll_voters(
        &self,
        env: &impl PollManagerEnv,
        poll_id: PollId,
        poll: &Poll,
        initial_dialog_id: DialogId,
        initial_date: i32,
    ) -> bool {
        if env.auth_is_bot() {
            return false;
        }
        if poll.is_closed || poll.is_creator {
            return true;
        }
        if !poll.hide_results_until_close {
            let pending = self.pending_answers.get(&poll_id);
            let not_being_answered = pending
                .map(|pending| pending.is_finished && self.was_saved(poll_id))
                .unwrap_or(true);
            if not_being_answered {
                for poll_option in &poll.options {
                    if poll_option.is_chosen {
                        return true;
                    }
                }
            }
        }
        if !poll.country_codes.is_empty()
            && !poll
                .country_codes
                .contains(&env.get_option_string("phone_country_iso2"))
        {
            return true;
        }
        if self.is_poll_vote_subscribers_only_limited(env, poll, initial_dialog_id, initial_date) {
            return true;
        }
        false
    }

    /// `PollManager::get_poll_voters` (PollManager.cpp:1554-1617) — the
    /// local cache halves answer; the server tail parks the promise.
    pub fn get_poll_voters(
        &mut self,
        env: &mut impl PollManagerEnv,
        message_full_id: MessageFullId,
        option_id: i32,
        offset: i32,
        limit: i32,
        mut promise: Promise<td_api::PollVoters>,
    ) {
        let poll_id = match env.get_message_poll_id(&message_full_id, false) {
            Ok(poll_id) => poll_id,
            Err(status) => return promise.set_error(status),
        };
        if offset < 0 {
            return promise.set_error(tdutils::status::Status::error(
                400,
                "Invalid offset specified",
            ));
        }
        let mut limit = limit;
        if limit <= 0 {
            return promise.set_error(tdutils::status::Status::error(
                400,
                "Parameter limit must be positive",
            ));
        }
        if limit > Self::MAX_GET_POLL_VOTERS {
            limit = Self::MAX_GET_POLL_VOTERS;
        }

        let poll = match self.get_poll(poll_id) {
            Some(poll) => poll.clone(),
            None => return,
        };
        if (!poll.subscribers_only
            && !self.can_get_poll_voters(env, poll_id, &poll, DialogId::default(), 0))
            || poll.is_anonymous
            || is_local_poll_id(poll_id)
        {
            return promise.set_error(tdutils::status::Status::error(
                400,
                "Poll results can't be received",
            ));
        }
        if option_id < 0 || option_id as usize >= poll.options.len() {
            return promise.set_error(tdutils::status::Status::error(
                400,
                "Invalid option ID specified",
            ));
        }

        {
            let voters = self.get_poll_option_voters(&poll, poll_id, option_id);
            if voters.pending_queries.is_empty() && voters.was_invalidated && offset == 0 {
                voters.voters.clear();
                voters.next_offset.clear();
                voters.was_invalidated = false;
            }
        }
        let cur_offset = self
            .poll_voters
            .get(&poll_id)
            .map(|poll_voters| poll_voters[option_id as usize].voters.len() as i32)
            .unwrap_or(0);

        if offset > cur_offset {
            return promise.set_error(tdutils::status::Status::error(
                400,
                "Too big offset specified; voters can be received only consequently",
            ));
        }
        if offset < cur_offset {
            let voters = &self.poll_voters.get(&poll_id).unwrap()[option_id as usize];
            let mut result: Vec<(DialogId, i32)> = Vec::new();
            let mut i = offset;
            while i != cur_offset && i - offset < limit {
                result.push(voters.voters[i as usize]);
                i += 1;
            }
            let total = std::cmp::max(poll.options[option_id as usize].voter_count, cur_offset);
            let object = self.get_poll_voters_object(env, total, &result);
            return promise.set_value(object);
        }

        let voter_count = poll.options[option_id as usize].voter_count;
        let next_offset_empty = self
            .poll_voters
            .get(&poll_id)
            .map(|poll_voters| poll_voters[option_id as usize].next_offset.is_empty())
            .unwrap_or(true);
        if voter_count == 0 || (next_offset_empty && cur_offset > 0) {
            let object = self.get_poll_voters_object(env, 0, &[]);
            return promise.set_value(object);
        }

        let voters = self.get_poll_option_voters(&poll, poll_id, option_id);
        voters.pending_queries.push(promise);
        if voters.pending_queries.len() > 1 {
            return;
        }

        if !env.close_flag() {
            self.unload_poll_timeout.remove(&poll_id.get());
        }

        // `GetPollVotersQuery::send` (PollManager.cpp:1609-1617) — the
        // lot parks the lambda captures ((poll_id, option_id) identity,
        // the offset captured at send, the un-maxed limit, the handler's
        // dialog_id_); the promise stays in pending_queries like the
        // C++. The send-leg None-peer band settles the promise directly
        // (no dialog-error head — cpp:122-125) by routing the 400
        // through the full answer intake (the parked promises fail
        // there).
        let offset = self.poll_voters.get(&poll_id).unwrap()[option_id as usize]
            .next_offset
            .clone();
        let dialog_id = message_full_id.get_dialog_id();
        self.pending_net.get_poll_voters.push((
            poll_id,
            option_id,
            offset.clone(),
            limit,
            dialog_id,
        ));
        let option = poll.options[option_id as usize]
            .get_data()
            .as_bytes()
            .to_vec();
        if let Err(status) = env.send_get_poll_voters_query(
            &message_full_id,
            poll_id,
            option_id,
            option,
            &offset,
            limit,
        ) {
            // no query was dispatched — the handler dies at the send leg
            // (cpp:123-125)
            tdutils::log_info!("Can't get poll, because have no read access to {dialog_id}");
            let index = self
                .pending_net
                .get_poll_voters
                .iter()
                .position(|&(poll_id_, option_id_, ..)| {
                    poll_id_ == poll_id && option_id_ == option_id
                })
                .unwrap();
            self.pending_net.get_poll_voters.remove(index);
            self.on_get_poll_voters_result(env, poll_id, option_id, offset, limit, Err(status));
        }
    }

    /// The `GetPollVotersQuery` answer — the C++ handler's
    /// `on_result`/`on_error` (PollManager.cpp:134-162): the lot pairs
    /// the answer with its query by the (poll_id, option_id) identity;
    /// the error band runs the dialog-error head with the
    /// MESSAGE_ID_INVALID log carve-out, then the promise tail re-enters
    /// the intake.
    pub(crate) fn on_get_poll_voters_answer(
        &mut self,
        env: &mut impl PollManagerEnv,
        poll_id: PollId,
        option_id: i32,
        wire: std::result::Result<
            tdmtproto::telegram_api::messages_VotesList,
            tdutils::status::Status,
        >,
    ) {
        let Some(index) = self
            .pending_net
            .get_poll_voters
            .iter()
            .position(|&(poll_id_, option_id_, ..)| poll_id_ == poll_id && option_id_ == option_id)
        else {
            // an answer for a query this manager never sent (or already
            // answered) — the C++ handler instance is gone either way
            return;
        };
        let (_, _, offset, limit, dialog_id) = self.pending_net.get_poll_voters.remove(index);
        if let Err(status) = &wire {
            // `GetPollVotersQuery::on_error` (cpp:155-161)
            if !env.on_get_dialog_error(dialog_id, status, "GetPollVotersQuery")
                && status.message() != "MESSAGE_ID_INVALID"
            {
                tdutils::log_error!("Receive {status}, while trying to get voters of {poll_id}");
            }
        }
        self.on_get_poll_voters_result(env, poll_id, option_id, offset, limit, wire);
    }

    /// `PollManager::on_get_poll_voters` (PollManager.cpp:1619-1710) —
    /// the answer intake; the parked `pending_queries` promises settle
    /// here (the None-peer band routes its 400 through the same
    /// ladder).
    pub(crate) fn on_get_poll_voters_result(
        &mut self,
        env: &mut impl PollManagerEnv,
        poll_id: PollId,
        option_id: i32,
        offset: String,
        limit: i32,
        wire: std::result::Result<
            tdmtproto::telegram_api::messages_VotesList,
            tdutils::status::Status,
        >,
    ) {
        // `G()->ignore_result_if_closing` (Global.h:706-709) — a closing
        // Td converts an Ok answer into the 500 request-aborted error;
        // errors pass through.
        let wire = if env.close_flag() && wire.is_ok() {
            Err(tdutils::status::Status::error(500, "Request aborted"))
        } else {
            wire
        };
        let Some(poll) = self.get_poll(poll_id).cloned() else {
            // the C++ CHECK(poll != nullptr)
            panic!("poll {poll_id} not found in on_get_poll_voters");
        };
        if option_id < 0 || option_id as usize >= poll.options.len() {
            tdutils::log_error!(
                "Can't process voters for option {option_id} in {poll_id}, because it has only {} options",
                poll.options.len()
            );
            return;
        }
        let wire = if poll.is_anonymous {
            // just in case (cpp:1634-1636)
            Err(tdutils::status::Status::error(400, "Poll is anonymous"))
        } else {
            wire
        };

        {
            let voters = self.get_poll_option_voters(&poll, poll_id, option_id);
            if voters.next_offset != offset {
                tdutils::log_error!(
                    "Expected results for option {option_id} in {poll_id} with offset {}, but received with {offset}",
                    voters.next_offset
                );
                return;
            }
        }
        let promises = {
            let voters = self.get_poll_option_voters(&poll, poll_id, option_id);
            std::mem::take(&mut voters.pending_queries)
        };
        if promises.is_empty() {
            tdutils::log_error!("Have no waiting promises for option {option_id} in {poll_id}");
            return;
        }
        let vote_list = match wire {
            Err(status) => {
                for mut promise in promises {
                    promise.set_error(tdutils::status::Status::error(
                        status.code(),
                        status.message(),
                    ));
                }
                return;
            }
            Ok(vote_list) => vote_list,
        };

        env.on_get_users(vote_list.users, "on_get_poll_voters");
        env.on_get_chats(vote_list.chats, "on_get_poll_voters");

        {
            let voters = self.get_poll_option_voters(&poll, poll_id, option_id);
            voters.next_offset = vote_list.next_offset.clone();
        }
        if poll.options[option_id as usize].voter_count != vote_list.count {
            self.current_generation += 1;
            timeout_set(&mut self.update_poll_timeout, poll_id.get(), 0.0, env.now());
        }

        let option_data = poll.options[option_id as usize]
            .get_data()
            .as_bytes()
            .to_vec();
        let mut voter_ids: Vec<(DialogId, i32)> = Vec::new();
        for peer_vote in vote_list.votes {
            let (peer, date) = match peer_vote {
                tdmtproto::telegram_api::MessagePeerVote::messagePeerVote(voter) => {
                    if voter.option != option_data {
                        continue;
                    }
                    (voter.peer, voter.date)
                }
                tdmtproto::telegram_api::MessagePeerVote::messagePeerVoteInputOption(voter) => {
                    (voter.peer, voter.date)
                }
                tdmtproto::telegram_api::MessagePeerVote::messagePeerVoteMultiple(voter) => {
                    if !voter.options.contains(&option_data) {
                        continue;
                    }
                    (voter.peer, voter.date)
                }
            };
            let dialog_id = peer.as_ref().map(DialogId::from_peer).unwrap_or_default();
            if dialog_id.is_valid() {
                voter_ids.push((dialog_id, date));
            } else {
                tdutils::log_error!("Receive {dialog_id} as voter in {poll_id}");
            }
        }
        {
            let voters = self.get_poll_option_voters(&poll, poll_id, option_id);
            voters.voters.extend_from_slice(&voter_ids);
        }
        // the C++ truncation rides the object copy — the map keeps every
        // voter, the object carries the first `limit` (cpp:1694-1697)
        if voter_ids.len() as i32 > limit {
            voter_ids.truncate(limit as usize);
        }
        let known_voter_count = self
            .poll_voters
            .get(&poll_id)
            .map(|poll_voters| poll_voters[option_id as usize].voters.len() as i32)
            .unwrap_or(0);
        if vote_list.next_offset.is_empty() && known_voter_count != vote_list.count {
            let voters = self.get_poll_option_voters(&poll, poll_id, option_id);
            voters.was_invalidated = true;
        }

        let total = std::cmp::max(vote_list.count, known_voter_count);
        for mut promise in promises {
            let object = self.get_poll_voters_object(env, total, &voter_ids);
            promise.set_value(object);
        }
    }

    /// `PollManager::stop_poll` (PollManager.cpp:1717-1746).
    pub fn stop_poll(
        &mut self,
        env: &mut impl PollManagerEnv,
        message_full_id: MessageFullId,
        reply_markup: Option<td_api::ReplyMarkup>,
        mut promise: Promise<()>,
    ) {
        let poll_id = match env.get_message_poll_id(&message_full_id, true) {
            Ok(poll_id) => poll_id,
            Err(status) => return promise.set_error(status),
        };
        if self.get_poll_is_closed(poll_id) {
            return promise.set_error(tdutils::status::Status::error(
                400,
                "Poll has already been closed",
            ));
        }
        // `get_inline_reply_markup(reply_markup, is_bot,
        // has_message_sender_user_id(message_full_id))` (PollManager.cpp:
        // 1723-1725; ReplyMarkup.cpp:434-445) — the inline-only fold; the
        // switch-inline allowance keys on the message sender being a user.
        let switch_inline_buttons_allowed = env.has_message_sender_user_id(&message_full_id);
        let new_reply_markup = match crate::reply_markup::get_inline_reply_markup(
            reply_markup,
            env.auth_is_bot(),
            switch_inline_buttons_allowed,
            env.is_test_dc(),
        ) {
            Ok(markup) => markup,
            Err(status) => return promise.set_error(status),
        };
        if is_local_poll_id(poll_id) {
            self.stop_local_poll(env, poll_id);
            promise.set_value(());
            return;
        }
        let is_closed = self
            .get_poll_editable(poll_id)
            .expect("poll not found")
            .is_closed;
        if is_closed {
            promise.set_value(());
            return;
        }

        self.current_generation += 1;

        let poll = self.get_poll_editable(poll_id).expect("poll not found");
        poll.is_closed = true;
        self.save_poll(env, poll_id);
        self.notify_on_poll_update(env, poll_id);

        self.do_stop_poll(env, poll_id, message_full_id, new_reply_markup, 0, promise);
    }

    /// `PollManager::do_stop_poll` (PollManager.cpp:1766-1788).
    pub(crate) fn do_stop_poll(
        &mut self,
        env: &mut impl PollManagerEnv,
        poll_id: PollId,
        message_full_id: MessageFullId,
        reply_markup: Option<crate::reply_markup::ReplyMarkup>,
        log_event_id: u64,
        promise: Promise<()>,
    ) {
        debug_assert!(poll_id.is_valid());

        let mut log_event_id = log_event_id;
        if log_event_id == 0 && env.use_message_database() && reply_markup.is_none() {
            let log_event = StopPollLogEvent {
                poll_id,
                message_full_id,
            };
            let mut data = Vec::new();
            log_event.store(&mut data);
            log_event_id = env.binlog_add(STOP_POLL_HANDLER_TYPE, &data);
        }

        if !env.close_flag() {
            self.unload_poll_timeout.remove(&poll_id.get());
        }

        let is_inserted = self.being_closed_polls.insert(poll_id);
        debug_assert!(is_inserted);

        // `StopPollQuery::send` (PollManager.cpp:287-322) — the lot
        // parks the C++ `new_promise` lambda tuple (message_full_id,
        // log_event_id, promise) keyed by the poll id; the send-leg
        // None-peer band routes the handler's on_error head + the
        // error through `on_stop_poll_finished`, so the being_closed
        // erase + the binlog erase run on every band.
        let dialog_id = message_full_id.get_dialog_id();
        self.pending_net
            .stop_poll
            .push((poll_id, message_full_id, log_event_id, promise));
        if let Err(status) =
            env.send_stop_poll_query(&message_full_id, reply_markup.as_ref(), poll_id)
        {
            let index = self
                .pending_net
                .stop_poll
                .iter()
                .position(|&(poll_id_, ..)| poll_id_ == poll_id)
                .unwrap();
            let (_, message_full_id, log_event_id, promise) =
                self.pending_net.stop_poll.remove(index);
            tdutils::log_info!("Can't close poll, because have no edit access to {dialog_id}");
            env.on_get_dialog_error(dialog_id, &status, "StopPollQuery");
            self.on_stop_poll_finished(
                env,
                poll_id,
                message_full_id,
                log_event_id,
                Err(status),
                promise,
            );
        }
    }

    /// The `StopPollQuery` answer — the C++ handler's
    /// `on_result`/`on_error` (PollManager.cpp:294-332): the lot pairs
    /// the answer with its query by the poll id; the error band runs
    /// the MESSAGE_NOT_MODIFIED Ok settle (non-bot) or the
    /// dialog-error head + the error finish; the Ok updates feed the
    /// engine with the popped lambda tuple (the router's
    /// `OnGetUpdatesStopPoll` hop runs `on_stop_poll_finished` after
    /// the engine).
    pub(crate) fn on_stop_poll_answer(
        &mut self,
        env: &mut impl PollManagerEnv,
        poll_id: PollId,
        wire: std::result::Result<tdmtproto::telegram_api::Updates, tdutils::status::Status>,
    ) -> Option<(
        tdmtproto::telegram_api::Updates,
        MessageFullId,
        u64,
        Promise<()>,
    )> {
        let Some(index) = self
            .pending_net
            .stop_poll
            .iter()
            .position(|&(poll_id_, ..)| poll_id_ == poll_id)
        else {
            // an answer for a query this manager never sent (or already
            // answered) — the C++ handler instance is gone either way
            return None;
        };
        let (_, message_full_id, log_event_id, promise) = self.pending_net.stop_poll.remove(index);
        match wire {
            Err(status) => {
                // `StopPollQuery::on_error` (cpp:323-332)
                if !env.auth_is_bot() && status.message() == "MESSAGE_NOT_MODIFIED" {
                    self.on_stop_poll_finished(
                        env,
                        poll_id,
                        message_full_id,
                        log_event_id,
                        Ok(()),
                        promise,
                    );
                    return None;
                }
                env.on_get_dialog_error(message_full_id.get_dialog_id(), &status, "StopPollQuery");
                self.on_stop_poll_finished(
                    env,
                    poll_id,
                    message_full_id,
                    log_event_id,
                    Err(status),
                    promise,
                );
                None
            }
            // the router feeds the engine, then the C++ promise-lambda
            // tail runs `on_stop_poll_finished` with the engine's
            // result (cpp:1778-1783)
            Ok(updates) => Some((updates, message_full_id, log_event_id, promise)),
        }
    }

    /// `PollManager::on_stop_poll_finished` (PollManager.cpp:1790-1806)
    /// — the query-result landing: the being_closed erase, the binlog
    /// erase, the bot re-register band and the promise settle (the
    /// MESSAGE_NOT_MODIFIED band routes its Ok through here too — the
    /// C++ promise-lambda always runs).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn on_stop_poll_finished(
        &mut self,
        env: &mut impl PollManagerEnv,
        poll_id: PollId,
        message_full_id: MessageFullId,
        log_event_id: u64,
        result: std::result::Result<(), tdutils::status::Status>,
        mut promise: Promise<()>,
    ) {
        self.being_closed_polls.remove(&poll_id);
        if log_event_id != 0 && !env.close_flag() {
            env.binlog_erase(log_event_id);
        }
        if env.auth_is_bot() {
            let registered = self
                .server_poll_messages
                .get(&poll_id)
                .map(|ids| ids.contains(&message_full_id))
                .unwrap_or(false)
                || self
                    .other_poll_messages
                    .get(&poll_id)
                    .map(|ids| ids.contains(&message_full_id))
                    .unwrap_or(false);
            if registered {
                env.on_external_update_message_content(message_full_id);
            }
        }
        match result {
            Ok(()) => promise.set_value(()),
            Err(status) => promise.set_error(status),
        }
    }

    /// `PollManager::stop_local_poll` (PollManager.cpp:1808-1818).
    pub fn stop_local_poll(&mut self, env: &mut impl PollManagerEnv, poll_id: PollId) {
        debug_assert!(is_local_poll_id(poll_id));
        let poll = self.get_poll_editable(poll_id).expect("poll not found");
        if poll.is_closed {
            return;
        }
        poll.is_closed = true;
        self.notify_on_poll_update(env, poll_id);
    }

    /// `PollManager::delete_pending_web_page` (PollManager.cpp:1820-1833).
    pub fn delete_pending_web_page(
        &mut self,
        env: &mut impl PollManagerEnv,
        poll_id: PollId,
        _web_page_id: i64,
    ) {
        let poll = self
            .get_poll_editable(poll_id)
            .expect("poll not found")
            .clone();
        let web_page_ids = Self::get_poll_web_page_ids(&poll);
        env.register_poll_web_pages(poll_id, web_page_ids);
        self.save_poll(env, poll_id);
    }

    /// `PollManager::get_polling_timeout` (PollManager.cpp:1835-1838).
    fn get_polling_timeout(&self, _env: &impl PollManagerEnv) -> f64 {
        let result = if env_is_online() { 60.0 } else { 30.0 * 60.0 };
        result * tdutils::random::fast(70, 100) as f64 * 0.01
    }

    /// `PollManager::on_update_poll_timeout` (PollManager.cpp:1840-1867) —
    /// the local checks, then the `GetPollResultsQuery` send: the
    /// captured generation rides the kind, the promise is empty (the
    /// C++ `Promise<Unit>()`; the answer re-enters
    /// `on_get_poll_results` through the router).
    fn on_update_poll_timeout(&mut self, env: &mut impl PollManagerEnv, poll_id: PollId) {
        if env.close_flag() {
            return;
        }
        debug_assert!(!env.auth_is_bot());
        debug_assert!(!is_local_poll_id(poll_id));

        let Some(poll) = self.get_poll(poll_id).cloned() else {
            return;
        };
        if poll.is_closed && poll.is_updated_after_close {
            return;
        }
        if self.pending_answers.contains_key(&poll_id) {
            tdutils::log_info!("Skip fetching results of {poll_id}, because it is being voted now");
            return;
        }
        let Some(message_full_id) = self.server_poll_messages.get(&poll_id).and_then(|ids| {
            // the C++ `get_random()` spread pick collapses to an
            // arbitrary element
            ids.iter().next().copied()
        }) else {
            return;
        };
        tdutils::log_info!("Fetching results of {poll_id} from {message_full_id:?}");
        let generation = self.current_generation;

        // `GetPollResultsQuery::send` (PollManager.cpp:79-90) — the lot
        // parks the handler's `poll_id_`/`dialog_id_`/`message_id_`
        // keyed by the generation; the send-leg None-peer band is
        // `promise.set_value(nullptr)`: the Ok(null) answer drops
        // through the intake without touching state, so no query parks
        // at all.
        let dialog_id = message_full_id.get_dialog_id();
        let message_id = message_full_id.get_message_id();
        let dispatched = env.send_get_poll_results_query(&message_full_id, poll.hash, generation);
        if !dispatched {
            tdutils::log_info!("Can't reget poll, because have no read access to {dialog_id}");
            return;
        }
        self.pending_net
            .get_poll_results
            .push((poll_id, generation, dialog_id, message_id));
    }

    /// The `GetPollResultsQuery` answer — the C++ handler's
    /// `on_result`/`on_error` (PollManager.cpp:91-110): the lot pairs
    /// the answer with its query by the kind-carried generation; the
    /// error band runs the messages-error head + the log, then the
    /// promise tail re-enters the intake.
    pub(crate) fn on_get_poll_results_answer(
        &mut self,
        env: &mut impl PollManagerEnv,
        generation: u64,
        wire: std::result::Result<
            Option<tdmtproto::telegram_api::Updates>,
            tdutils::status::Status,
        >,
    ) -> Option<tdmtproto::telegram_api::Updates> {
        let Some(index) = self
            .pending_net
            .get_poll_results
            .iter()
            .position(|&(_, generation_, _, _)| generation_ == generation)
        else {
            // an answer for a query this manager never sent (or already
            // answered) — the C++ handler instance is gone either way
            return None;
        };
        let (poll_id, _, dialog_id, message_id) = self.pending_net.get_poll_results.remove(index);
        if let Err(status) = &wire {
            // `GetPollResultsQuery::on_error` (cpp:100-107)
            if !env.on_get_message_error(dialog_id, message_id, status, "GetPollResultsQuery") {
                tdutils::log_error!("Receive {status}, while trying to get results of {poll_id}");
            }
        }
        self.on_get_poll_results_result(env, poll_id, generation, wire)
    }

    /// The `DeletePollAnswerQuery` answer — the C++ handler's
    /// `on_result`/`on_error` (PollManager.cpp:216-234): the promise
    /// pops FIFO (the C++ handler carries only the promise); the error
    /// band runs the dialog-error head + the promise error, the Ok band
    /// hands the promise to the updates tail.
    pub(crate) fn on_delete_poll_answer_answer(
        &mut self,
        env: &mut impl PollManagerEnv,
        wire: std::result::Result<tdmtproto::telegram_api::Updates, tdutils::status::Status>,
    ) -> Option<(tdmtproto::telegram_api::Updates, Promise<()>)> {
        let Some((dialog_id, mut promise)) = (!self.pending_net.delete_poll_answer.is_empty())
            .then(|| self.pending_net.delete_poll_answer.remove(0))
        else {
            // an answer for a query this manager never sent (or already
            // answered) — the C++ handler instance is gone either way
            return None;
        };
        match wire {
            Err(status) => {
                // `DeletePollAnswerQuery::on_error` (cpp:228-233)
                env.on_get_dialog_error(dialog_id, &status, "DeletePollAnswerQuery");
                promise.set_error(status);
                None
            }
            // `td_->updates_manager_->on_get_updates(result,
            // std::move(promise_))` (cpp:222-226) — the router feeds the
            // engine with the popped promise
            Ok(updates) => Some((updates, promise)),
        }
    }

    /// `PollManager::on_get_poll_results` (PollManager.cpp:1940-1975) —
    /// the answer intake; returns the parsed updates for the router's
    /// engine feed (the C++ passes an empty promise, `Promise<Unit>()`).
    pub(crate) fn on_get_poll_results_result(
        &mut self,
        env: &mut impl PollManagerEnv,
        poll_id: PollId,
        generation: u64,
        wire: std::result::Result<
            Option<tdmtproto::telegram_api::Updates>,
            tdutils::status::Status,
        >,
    ) -> Option<tdmtproto::telegram_api::Updates> {
        // `G()->ignore_result_if_closing` (Global.h:706-709).
        let wire = if env.close_flag() && wire.is_ok() {
            Err(tdutils::status::Status::error(500, "Request aborted"))
        } else {
            wire
        };
        let poll = self.get_poll(poll_id).cloned()?;
        match wire {
            Err(_) => {
                if !(env.close_flag()
                    || env.auth_is_bot()
                    || (poll.is_closed && poll.is_updated_after_close))
                {
                    let timeout = self.get_polling_timeout(env);
                    tdutils::log_info!("Schedule updating of {poll_id} in {timeout}");
                    timeout_add(
                        &mut self.update_poll_timeout,
                        poll_id.get(),
                        timeout,
                        env.now(),
                    );
                }
                None
            }
            Ok(None) => None,
            Ok(Some(updates)) => {
                if generation != self.current_generation {
                    tdutils::log_info!("Receive possibly outdated result of {poll_id}, reget it");
                    if !(env.close_flag()
                        || env.auth_is_bot()
                        || (poll.is_closed && poll.is_updated_after_close))
                    {
                        timeout_set(&mut self.update_poll_timeout, poll_id.get(), 0.0, env.now());
                    }
                    return None;
                }
                Some(updates)
            }
        }
    }

    /// `PollManager::on_close_poll_timeout` (PollManager.cpp:1869-1894).
    fn on_close_poll_timeout(&mut self, env: &mut impl PollManagerEnv, poll_id: PollId) {
        if env.close_flag() {
            return;
        }
        debug_assert!(!is_local_poll_id(poll_id));

        let close_date = match self.get_poll_editable(poll_id) {
            Some(poll) => {
                if poll.is_closed || poll.close_date == 0 {
                    return;
                }
                poll.close_date
            }
            None => return,
        };
        if close_date as f64 <= env.server_time() {
            let poll = self.get_poll_editable(poll_id).expect("poll not found");
            poll.is_closed = true;
            self.save_poll(env, poll_id);
            self.notify_on_poll_update(env, poll_id);
            if !env.auth_is_bot() {
                timeout_set(&mut self.update_poll_timeout, poll_id.get(), 1.0, env.now());
            }
        } else {
            timeout_set(
                &mut self.close_poll_timeout,
                poll_id.get(),
                close_date as f64 - env.server_time() + 1e-3,
                env.now(),
            );
        }
    }

    /// `PollManager::on_unload_poll_timeout` (PollManager.cpp:1896-1930).
    fn on_unload_poll_timeout(&mut self, env: &mut impl PollManagerEnv, poll_id: PollId) {
        if env.close_flag() {
            return;
        }
        if is_local_poll_id(poll_id) {
            let is_deleted = self.polls.remove(&poll_id).is_some();
            debug_assert!(is_deleted);
            self.poll_was_saved.remove(&poll_id);
            debug_assert!(!self.poll_voters.contains_key(&poll_id));
            debug_assert!(!self.loaded_from_database_polls.contains(&poll_id));
            return;
        }
        if !self.can_unload_poll(poll_id) {
            return;
        }
        if !self.have_poll(poll_id) {
            return;
        }

        self.update_poll_timeout.remove(&poll_id.get());
        self.close_poll_timeout.remove(&poll_id.get());

        let is_deleted = self.polls.remove(&poll_id).is_some();
        debug_assert!(is_deleted);
        self.poll_was_saved.remove(&poll_id);
        self.poll_voters.remove(&poll_id);
        self.loaded_from_database_polls.remove(&poll_id);
        self.unload_poll_timeout.remove(&poll_id.get());
        env.register_poll_web_pages(poll_id, Vec::new());
    }

    /// `PollManager::forget_local_poll` (PollManager.cpp:1932-1938).
    fn forget_local_poll(&mut self, env: &mut impl PollManagerEnv, poll_id: PollId) {
        if env.close_flag() {
            return;
        }
        debug_assert!(is_local_poll_id(poll_id));
        timeout_set(
            &mut self.unload_poll_timeout,
            poll_id.get(),
            Self::UNLOAD_POLL_DELAY as f64,
            env.now(),
        );
    }

    /// `PollManager::on_online` (PollManager.cpp:1970-1982).
    pub fn on_online(&mut self, env: &mut impl PollManagerEnv) {
        if env.auth_is_bot() {
            return;
        }
        let poll_ids: Vec<i64> = self
            .server_poll_messages
            .keys()
            .map(|poll_id| poll_id.get())
            .filter(|poll_id| self.update_poll_timeout.contains_key(poll_id))
            .collect();
        for poll_id in poll_ids {
            timeout_set(
                &mut self.update_poll_timeout,
                poll_id,
                tdutils::random::fast(3, 30) as f64,
                env.now(),
            );
        }
    }

    /// The collapsed MultiTimeout pump: fires every due update/close/unload
    /// deadline once (the `on_update_poll_timeout_callback` family,
    /// PollManager.cpp:422-447).
    pub fn tick_timeouts(&mut self, env: &mut impl PollManagerEnv) {
        let now = env.now();
        self.last_now = now;
        let due_update: Vec<i64> = self
            .update_poll_timeout
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(&key, _)| key)
            .collect();
        let due_close: Vec<i64> = self
            .close_poll_timeout
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(&key, _)| key)
            .collect();
        let due_unload: Vec<i64> = self
            .unload_poll_timeout
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(&key, _)| key)
            .collect();

        for poll_id in due_update {
            self.update_poll_timeout.remove(&poll_id);
            if env.close_flag() {
                return;
            }
            self.on_update_poll_timeout(env, PollId::new(poll_id));
        }
        for poll_id in due_close {
            self.close_poll_timeout.remove(&poll_id);
            if env.close_flag() {
                return;
            }
            self.on_close_poll_timeout(env, PollId::new(poll_id));
        }
        for poll_id in due_unload {
            self.unload_poll_timeout.remove(&poll_id);
            if env.close_flag() {
                return;
            }
            self.on_unload_poll_timeout(env, PollId::new(poll_id));
        }
    }

    /// `PollManager::dup_poll` (PollManager.cpp:2040-2063) — the local
    /// halves; the entity/media dup folds ride the M.8/M.9 manager seams
    /// (the value plane keeps the records as-is).
    pub fn dup_poll(
        &mut self,
        env: &mut impl PollManagerEnv,
        _dialog_id: DialogId,
        poll_id: PollId,
    ) -> PollId {
        let poll = self.get_poll(poll_id).expect("poll not found").clone();
        let is_broadcast = false; // [blocked: M.6 integration] is_broadcast_channel
        self.create_poll(
            poll.question,
            poll.options,
            poll.is_anonymous,
            poll.allow_multiple_answers,
            poll.has_open_answers,
            poll.has_revoting_disabled,
            if is_broadcast {
                poll.subscribers_only
            } else {
                false
            },
            if is_broadcast {
                poll.country_codes.clone()
            } else {
                Vec::new()
            },
            poll.shuffle_answers,
            poll.hide_results_until_close,
            poll.is_quiz,
            poll.correct_option_ids,
            poll.explanation,
            poll.explanation_media_type,
            poll.open_period,
            if poll.open_period == 0 {
                0
            } else {
                env.unix_time() + poll.open_period
            },
            false,
        )
    }

    /// `PollManager::has_input_media` (PollManager.cpp:2065-2072).
    pub fn has_input_media(&self, poll_id: PollId) -> bool {
        let poll = self.get_poll(poll_id).expect("poll not found");
        !(poll.is_quiz && poll.correct_option_ids.is_empty())
    }

    /// `PollManager::on_get_poll_vote` (PollManager.cpp:2587-2628).
    pub fn on_get_poll_vote(
        &mut self,
        env: &mut impl PollManagerEnv,
        poll_id: PollId,
        dialog_id: DialogId,
        options: Vec<Vec<u8>>,
        positions: Vec<i32>,
    ) {
        if !poll_id.is_valid() {
            return;
        }
        if !dialog_id.is_valid() {
            return;
        }
        if options.len() != positions.len() {
            return;
        }
        debug_assert!(env.auth_is_bot());

        let mut option_ids: Vec<String> = Vec::with_capacity(options.len());
        for option in options {
            let Ok(option_id) = String::from_utf8(option) else {
                return;
            };
            if option_id.is_empty() {
                return;
            }
            option_ids.push(option_id);
        }
        for &position in &positions {
            if !(0..=100).contains(&position) {
                return;
            }
        }

        let voter_id = env.get_min_message_sender_object(dialog_id);
        env.send_update(td_api::Update::updatePollAnswer(td_api::updatePollAnswer {
            poll_id: poll_id.get(),
            voter_id,
            option_ids,
            option_positions: positions,
        }));
    }

    /// `PollManager::on_binlog_events` (PollManager.cpp:2642-2690) — the
    /// SetPollAnswer/StopPoll replays.
    pub fn on_binlog_events(
        &mut self,
        env: &mut impl PollManagerEnv,
        events: Vec<(i32, u64, Vec<u8>)>,
    ) {
        if env.close_flag() {
            return;
        }
        let have_old_message_database = env.use_message_database();
        for (event_type, event_id, data) in events {
            match event_type {
                SET_POLL_ANSWER_HANDLER_TYPE => {
                    if !have_old_message_database {
                        env.binlog_erase(event_id);
                        continue;
                    }
                    let mut parser = Parser::new(&data);
                    let Ok(log_event) = SetPollAnswerLogEvent::parse(&mut parser) else {
                        continue;
                    };
                    // The Dependencies resolve_force half rides the M.10
                    // integration.
                    self.do_set_poll_answer(
                        env,
                        log_event.poll_id,
                        log_event.message_full_id,
                        log_event.options,
                        event_id,
                        Promise::default(),
                    );
                }
                STOP_POLL_HANDLER_TYPE => {
                    if !have_old_message_database {
                        env.binlog_erase(event_id);
                        continue;
                    }
                    let mut parser = Parser::new(&data);
                    let Ok(log_event) = StopPollLogEvent::parse(&mut parser) else {
                        continue;
                    };
                    self.do_stop_poll(
                        env,
                        log_event.poll_id,
                        log_event.message_full_id,
                        None,
                        event_id,
                        Promise::default(),
                    );
                }
                _ => {
                    tdutils::log_error!("Unsupported poll log event type {}", event_type);
                }
            }
        }
    }
}

/// `env_is_online` — the `online_manager_->is_online()` probe of
/// `get_polling_timeout`; the collapsed line answers the offline band.
fn env_is_online() -> bool {
    false
}

/// The base64 fallback of non-UTF-8 option ids (PollManager.cpp:824-831;
/// `base64_encode` — the tdutils standard alphabet with padding).
fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let triple = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(ALPHABET[(triple >> 18) as usize & 0x3f] as char);
        out.push(ALPHABET[(triple >> 12) as usize & 0x3f] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(triple >> 6) as usize & 0x3f] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[triple as usize & 0x3f] as char
        } else {
            '='
        });
    }
    out
}

impl PollManager {
    /// `PollManager::MAX_GET_POLL_VOTERS` (PollManager.h:204).
    pub const MAX_GET_POLL_VOTERS: i32 = 50;
    /// `PollManager::UNLOAD_POLL_DELAY` (PollManager.h:205).
    pub const UNLOAD_POLL_DELAY: i32 = 600;
}

/// The pairwise `operator==` over option lists (PollOption.cpp:161-173 via
/// `poll->options_ != options`, PollManager.cpp:2198).
fn options_equal_vec(lhs: &[PollOption], rhs: &[PollOption]) -> bool {
    lhs.len() == rhs.len()
        && lhs
            .iter()
            .zip(rhs)
            .all(|(l, r)| PollOption::options_equal(l, r))
}

/// `check_min_message_sender` (MessageSender.cpp:101-131) — the min-sender
/// validity check; channel senders record their min-channel pair.
fn check_min_message_sender(
    env: &impl PollManagerEnv,
    dialog_id: DialogId,
    min_channels: &mut Vec<(ChannelId, MinChannel)>,
) -> bool {
    if env.have_dialog_info(dialog_id) {
        return true;
    }
    match dialog_id.get_type() {
        DialogType::User => {
            let user_id = dialog_id.get_user_id();
            if !env.have_min_user(user_id) {
                return false;
            }
            true
        }
        DialogType::Channel => {
            let channel_id = dialog_id.get_channel_id();
            let Some(min_channel) = env.get_min_channel(channel_id) else {
                return false;
            };
            for (known_channel_id, _) in min_channels.iter() {
                if *known_channel_id == channel_id {
                    return true;
                }
            }
            min_channels.push((channel_id, min_channel));
            true
        }
        _ => false,
    }
}

/// `PollManager::on_get_poll` (PollManager.cpp:2129-2567) — the wire
/// intake. `poll_server`/`poll_results` are the telegram_api records.
pub fn on_get_poll(
    manager: &mut PollManager,
    env: &mut impl PollManagerEnv,
    poll_id: PollId,
    poll_server: Option<tdmtproto::telegram_api::Poll>,
    poll_results: Option<tdmtproto::telegram_api::PollResults>,
    source: &str,
) -> PollId {
    let _ = source;
    let is_bot = env.auth_is_bot();
    let need_update_poll = poll_id.is_valid() && is_bot;
    let mut poll_id = poll_id;
    if !poll_id.is_valid()
        && let Some(poll_server) = &poll_server
    {
        poll_id = PollId::new(poll_server.id);
    }
    if !poll_id.is_valid() || is_local_poll_id(poll_id) {
        return PollId::new(0);
    }
    if let Some(poll_server) = &poll_server
        && poll_server.id != poll_id.get()
    {
        return PollId::new(0);
    }
    let max_poll_options = env
        .get_option_integer("poll_answer_count_max", 12)
        .clamp(12, 1000);
    if let Some(poll_server) = &poll_server {
        if poll_server.answers.is_empty()
            || poll_server.answers.len() as i64 > 10 * max_poll_options
        {
            return PollId::new(0);
        }
        let mut option_data: HashSet<&[u8]> = HashSet::new();
        for answer in &poll_server.answers {
            let tdmtproto::telegram_api::PollAnswer::pollAnswer(answer) = &**answer else {
                return PollId::new(0);
            };
            if answer.option.is_empty() {
                return PollId::new(0);
            }
            option_data.insert(&answer.option);
        }
        if option_data.len() != poll_server.answers.len() {
            return PollId::new(0);
        }
    }

    // The force-load db half
    if manager.get_poll_force(env, poll_id).is_none() {
        if poll_server.is_none() {
            return PollId::new(0);
        }
        manager.polls.insert(poll_id, Poll::default());
    } else if let Some(results) = &poll_results
        && results.min
        && manager.pending_answers.contains_key(&poll_id)
    {
        return poll_id;
    }

    let mut is_changed = false;
    let mut need_save_to_database = false;
    let mut poll_server_is_closed = false;

    if let Some(poll_server) = poll_server {
        // PollOption::get_poll_options(td, answers, min_channels)
        let mut option_min_channels: Vec<(ChannelId, MinChannel)> = Vec::new();
        let mut options: Vec<PollOption> = Vec::with_capacity(poll_server.answers.len());
        for answer in poll_server.answers {
            let tdmtproto::telegram_api::PollAnswer::pollAnswer(answer) = *answer else {
                continue;
            };
            let mut text = crate::message_entity::get_formatted_text_of_text_with_entities::<()>(
                None,
                answer.text,
                true,
                true,
                "PollOption",
            );
            crate::message_entity::keep_only_custom_emoji(&mut text);
            // The pollAnswer media rides the M.9 message-content seam;
            // the answer record keeps media_type None.
            let mut option = PollOption::default();
            option.text = text;
            option.data = String::from_utf8_lossy(&answer.option).into_owned();
            if let Some(added_by) = answer.added_by {
                let added_by_dialog_id = DialogId::from_peer(&added_by);
                let added_date = answer.date;
                if check_min_message_sender(env, added_by_dialog_id, &mut option_min_channels)
                    && added_date > 0
                {
                    option.added_by_dialog_id = added_by_dialog_id;
                    option.added_date = added_date;
                }
            }
            options.push(option);
        }
        {
            let poll = manager.polls.get_mut(&poll_id).unwrap();
            if !options_equal_vec(&poll.options, &options) {
                let correct_option_datas: Vec<String> = poll
                    .correct_option_ids
                    .iter()
                    .filter_map(|&correct_option_id| {
                        poll.options
                            .get(correct_option_id as usize)
                            .map(|option| option.data.clone())
                    })
                    .collect();
                poll.options = options;
                poll.option_min_channels = option_min_channels;
                if !correct_option_datas.is_empty() {
                    poll.correct_option_ids.clear();
                    for (i, option) in poll.options.iter().enumerate() {
                        if correct_option_datas.contains(&option.data) {
                            poll.correct_option_ids.push(i as i32);
                        }
                    }
                }
                if let Some(poll_voters) = manager.poll_voters.remove(&poll_id) {
                    for voters in poll_voters {
                        for mut promise in voters.pending_queries {
                            promise.set_error(tdutils::status::Status::error(
                                500,
                                "The poll was changed",
                            ));
                        }
                    }
                }
                is_changed = true;
            }
        }
        let mut question = crate::message_entity::get_formatted_text_of_text_with_entities::<()>(
            None,
            poll_server.question,
            true,
            true,
            "on_get_poll",
        );
        crate::message_entity::keep_only_custom_emoji(&mut question);
        {
            let poll = manager.polls.get_mut(&poll_id).unwrap();
            if poll.question != question {
                poll.question = question;
                is_changed = true;
            }
        }
        poll_server_is_closed = poll_server.closed;
        let mut arm_close_timeout = false;
        #[allow(unused_assignments)]
        let mut close_date = 0;
        {
            let poll = manager.polls.get_mut(&poll_id).unwrap();
            if poll_server_is_closed && !poll.is_closed {
                poll.is_closed = poll_server_is_closed;
                is_changed = true;
            }
            if poll_server_is_closed && !poll.is_updated_after_close {
                poll.is_updated_after_close = true;
                is_changed = true;
            }
            let mut open_period = poll_server.close_period;
            close_date = poll_server.close_date;
            if close_date == 0 || open_period == 0 {
                close_date = 0;
                open_period = 0;
            }
            if open_period != poll.open_period {
                poll.open_period = open_period;
                if !poll.is_closed {
                    is_changed = true;
                } else {
                    need_save_to_database = true;
                }
            }
            if close_date != poll.close_date {
                poll.close_date = close_date;
                if !poll.is_closed {
                    is_changed = true;
                    if close_date != 0 {
                        if close_date as f64 <= env.server_time() {
                            poll.is_closed = true;
                        } else if !env.close_flag() {
                            arm_close_timeout = true;
                        }
                    } else if !env.close_flag() {
                        arm_close_timeout = false;
                    }
                } else {
                    need_save_to_database = true;
                }
            }
            let is_anonymous = !poll_server.public_voters;
            if is_anonymous != poll.is_anonymous {
                poll.is_anonymous = is_anonymous;
                is_changed = true;
            }
            let allow_multiple_answers = poll_server.multiple_choice;
            let mut has_open_answers = poll_server.open_answers;
            let has_revoting_disabled = poll_server.revoting_disabled;
            let subscribers_only = poll_server.subscribers_only;
            let shuffle_answers = poll_server.shuffle_answers;
            let hide_results_until_close = poll_server.hide_results_until_close;
            let is_quiz = poll_server.quiz;
            let country_codes = poll_server.countries_iso2;
            if is_quiz && has_open_answers {
                has_open_answers = false;
            }
            if allow_multiple_answers != poll.allow_multiple_answers {
                poll.allow_multiple_answers = allow_multiple_answers;
                is_changed = true;
            }
            if has_open_answers != poll.has_open_answers {
                poll.has_open_answers = has_open_answers;
                is_changed = true;
            }
            if has_revoting_disabled != poll.has_revoting_disabled {
                poll.has_revoting_disabled = has_revoting_disabled;
                is_changed = true;
            }
            if shuffle_answers != poll.shuffle_answers {
                poll.shuffle_answers = shuffle_answers;
                is_changed = true;
            }
            if subscribers_only != poll.subscribers_only {
                poll.subscribers_only = subscribers_only;
                is_changed = true;
            }
            if country_codes != poll.country_codes {
                poll.country_codes = country_codes;
                is_changed = true;
            }
            if hide_results_until_close != poll.hide_results_until_close {
                poll.hide_results_until_close = hide_results_until_close;
                is_changed = true;
            }
            if is_quiz != poll.is_quiz {
                poll.is_quiz = is_quiz;
                is_changed = true;
            }
            if poll_server.creator && !poll.is_creator {
                poll.is_creator = true;
                is_changed = true;
            }
            if poll_server.hash != poll.hash {
                poll.hash = poll_server.hash;
                is_changed = true;
            }
        }
        if arm_close_timeout {
            timeout_set(
                &mut manager.close_poll_timeout,
                poll_id.get(),
                close_date as f64 - env.server_time() + 1e-3,
                env.now(),
            );
        }
    }

    let Some(poll_results) = poll_results else {
        return poll_id;
    };
    let is_min = poll_results.min;
    // telegram_api pollResults::TOTAL_VOTERS_MASK — flag bit 2.
    let has_total_voters = poll_results.flags & (1 << 2) != 0;
    let total_voters = poll_results.total_voters;
    if has_total_voters {
        let poll = manager.polls.get_mut(&poll_id).unwrap();
        if total_voters != poll.total_voter_count {
            poll.total_voter_count = total_voters.max(0);
            is_changed = true;
        }
    }
    // The results intake mutates the poll record across every matched
    // option; the record is taken out of the map for the whole section
    // and reinserted at the end (the C++ `poll` pointer equivalent).
    let mut poll = manager.polls.remove(&poll_id).unwrap();
    let mut recent_option_voter_min_channels: Vec<(ChannelId, MinChannel)> = Vec::new();
    let mut need_update_recent_option_voter_min_channels = false;
    let mut correct_option_ids: Vec<i32> = Vec::new();
    for poll_result in &poll_results.results {
        let data = &poll_result.option;
        let options_len = poll.options.len();
        for option_index in 0..options_len {
            let is_match = poll.options[option_index].data.as_bytes() == data.as_slice();
            if !is_match {
                continue;
            }
            if !is_min && poll.options[option_index].set_is_chosen(poll_result.chosen) {
                is_changed = true;
            }
            if poll_result.correct {
                correct_option_ids.push(option_index as i32);
            }
            let mut voters_count = poll_result.voters.max(0);
            {
                if poll.options[option_index].is_chosen
                    && voters_count == 0
                    && !poll.hide_results_until_close
                {
                    voters_count = 1;
                }
                if voters_count > poll.total_voter_count {
                    poll.total_voter_count = voters_count;
                }
                let max_voter_count = i32::MAX / poll.options.len() as i32 - 2;
                if voters_count > max_voter_count {
                    voters_count = max_voter_count;
                }
            }
            if poll.options[option_index].set_voter_count(voters_count) {
                manager.invalidate_poll_option_voters(&poll, poll_id, option_index);
                is_changed = true;
            }
            let keep_recent =
                !(is_bot || (is_min && poll.is_creator && poll.hide_results_until_close));
            if keep_recent {
                let mut recent_option_voter_dialog_ids: Vec<DialogId> = Vec::new();
                for recent_voter in &poll_result.recent_voters {
                    let dialog_id = DialogId::from_peer(recent_voter);
                    if recent_option_voter_dialog_ids.contains(&dialog_id) {
                        continue;
                    }
                    if check_min_message_sender(
                        env,
                        dialog_id,
                        &mut recent_option_voter_min_channels,
                    ) {
                        recent_option_voter_dialog_ids.push(dialog_id);
                    }
                }
                let voter_count = poll.options[option_index].voter_count;
                if recent_option_voter_dialog_ids.len() as i32 > voter_count {
                    recent_option_voter_dialog_ids.truncate(voter_count.max(0) as usize);
                }
                if poll.options[option_index]
                    .set_recent_voter_dialog_ids(recent_option_voter_dialog_ids)
                {
                    is_changed = true;
                    need_update_recent_option_voter_min_channels = true;
                }
            }
        }
    }
    if !is_min && poll_results.results.is_empty() && poll.hide_results_until_close {
        let options_len = poll.options.len();
        for option_index in 0..options_len {
            if poll.options[option_index].set_is_chosen(false) {
                is_changed = true;
            }
            if poll.options[option_index].set_voter_count(0) {
                is_changed = true;
            }
            if poll.options[option_index].set_recent_voter_dialog_ids(Vec::new()) {
                is_changed = true;
                need_update_recent_option_voter_min_channels = true;
            }
        }
    }
    if need_update_recent_option_voter_min_channels {
        poll.recent_option_voter_min_channels = recent_option_voter_min_channels;
    }
    if !poll_results.results.is_empty() && has_total_voters {
        let mut max_total_voter_count = 0;
        for option in &poll.options {
            max_total_voter_count += option.voter_count;
        }
        if poll.total_voter_count > max_total_voter_count && max_total_voter_count != 0 {
            poll.total_voter_count = max_total_voter_count;
        }
    }
    if !is_min && !env.auth_is_bot() {
        let has_unread_votes = poll_results.has_unread_votes;
        if has_unread_votes != poll.has_unread_votes {
            // The pending-read-query check rides the M.10 seam; the
            // collapsed line applies the update.
            poll.has_unread_votes = has_unread_votes;
            need_save_to_database = true;
            manager.notify_on_poll_has_unread_votes_update(env, poll_id, has_unread_votes);
        }
    }
    if !is_min {
        let can_view_stats = poll_results.can_view_stats;
        if can_view_stats != poll.can_view_stats {
            poll.can_view_stats = can_view_stats;
            is_changed = true;
        }
    }

    let explanation = env.get_formatted_text_with_users(
        poll_results.solution,
        poll_results.solution_entities,
        true,
        false,
        "on_get_poll",
    );
    if poll.is_quiz && (!is_min || poll_server_is_closed) {
        if poll.correct_option_ids != correct_option_ids {
            if correct_option_ids.is_empty()
                && !poll.correct_option_ids.is_empty()
                && poll.has_revoting_disabled
            {
                // Can't change correct options of a known quiz
            } else {
                poll.correct_option_ids = correct_option_ids;
                is_changed = true;
            }
        }
        if poll.explanation != explanation {
            if explanation.text.is_empty()
                && !poll.explanation.text.is_empty()
                && poll.has_revoting_disabled
            {
                // Can't change known explanation to empty
            } else {
                poll.explanation = explanation;
                is_changed = true;
            }
        }
        // The solution media rides the M.9 message-content seam;
        // the record keeps explanation_media_type None.
    }

    let mut recent_voter_dialog_ids: Vec<DialogId> = Vec::new();
    let mut recent_voter_min_channels: Vec<(ChannelId, MinChannel)> = Vec::new();
    if !is_bot {
        for peer_id in &poll_results.recent_voters {
            let dialog_id = DialogId::from_peer(peer_id);
            if recent_voter_dialog_ids.contains(&dialog_id) {
                continue;
            }
            if check_min_message_sender(env, dialog_id, &mut recent_voter_min_channels) {
                recent_voter_dialog_ids.push(dialog_id);
            }
        }
    }
    if poll.is_anonymous && !recent_voter_dialog_ids.is_empty() {
        recent_voter_dialog_ids.clear();
        recent_voter_min_channels.clear();
    }
    if recent_voter_dialog_ids != poll.recent_voter_dialog_ids {
        let poll_before = poll.clone();
        poll.recent_voter_dialog_ids = recent_voter_dialog_ids;
        poll.recent_voter_min_channels = recent_voter_min_channels;
        manager.invalidate_poll_voters(&poll_before, poll_id);
        is_changed = true;
    }
    manager.polls.insert(poll_id, poll);

    if !is_bot && !manager.polls.get(&poll_id).unwrap().is_closed && !env.close_flag() {
        let timeout = manager.get_polling_timeout(env);
        timeout_add(
            &mut manager.update_poll_timeout,
            poll_id.get(),
            timeout,
            env.now(),
        );
    }
    if is_changed || need_save_to_database {
        manager.save_poll(env, poll_id);
    }
    if is_changed {
        manager.notify_on_poll_update(env, poll_id);
    }
    if need_update_poll
        && (is_changed
            || (manager.polls.get(&poll_id).unwrap().is_closed
                && manager.being_closed_polls.remove(&poll_id)))
    {
        debug_assert!(env.auth_is_bot());
        let poll_object = manager.get_poll_object(
            env,
            poll_id,
            crate::chat_manager::MessageId::new(0),
            DialogId::default(),
            0,
            false,
        );
        env.send_update(td_api::Update::updatePoll(td_api::updatePoll {
            poll: Some(poll_object),
        }));
        manager.schedule_poll_unload(poll_id);
    }
    poll_id
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat_manager::{ChannelId, MinChannel};
    use crate::dialog_id::MessageFullId;
    use crate::emoji_status::CustomEmojiId;
    use crate::files::FileManager;
    use crate::message_entity::{MessageEntity, Type};
    use crate::messages_manager::MessageId;
    use crate::peer_color::AccentColorId;
    use crate::store_parse::{Parser, store_i32, store_log_event_version, store_string};
    use crate::user_manager::{FileId, UserId};
    use tdutils::status::Status;

    /// The store/parse env of the fixture: the real FileId codec, an empty
    /// media seam (no poll record on this line carries media — the intake
    /// paths are the M.9/M.10 blocked band).
    struct TestStoreEnv {
        files: FileManager,
        stored_media: Vec<i32>,
    }

    impl Default for TestStoreEnv {
        fn default() -> TestStoreEnv {
            TestStoreEnv {
                files: FileManager::new(),
                stored_media: Vec::new(),
            }
        }
    }

    impl crate::photo::PhotoStoreEnv for TestStoreEnv {
        fn store_file_id(&mut self, file_id: FileId, out: &mut Vec<u8>) {
            self.files.store_file(file_id, out);
        }

        fn parse_file_id(&mut self, parser: &mut Parser<'_>) -> Result<FileId, &'static str> {
            self.files.parse_file(parser)
        }
    }

    impl PollStoreEnv for TestStoreEnv {
        fn store_poll_media(&mut self, media_type: i32, _out: &mut Vec<u8>) {
            self.stored_media.push(media_type);
        }

        fn parse_poll_media(&mut self, _parser: &mut Parser<'_>) -> Result<i32, &'static str> {
            Err("Poll media parse — [blocked: M.9 integration]")
        }
    }

    fn custom_emoji_entity(offset: i32, length: i32, document_id: i64) -> MessageEntity {
        MessageEntity::custom_emoji(offset, length, CustomEmojiId::new(document_id))
    }

    fn italic_entity(offset: i32, length: i32) -> MessageEntity {
        MessageEntity::new(Type::Italic, offset, length)
    }

    // ---------------------------------------------------------------------------
    // PollId + value helpers
    // ---------------------------------------------------------------------------

    #[test]
    fn local_poll_id_bands() {
        // PollManager.cpp:449-451 — negative but not INT32_MIN.
        assert!(!is_local_poll_id(PollId::new(0)));
        assert!(!is_local_poll_id(PollId::new(1)));
        assert!(!is_local_poll_id(PollId::new(-1i64 << 62)));
        assert!(is_local_poll_id(PollId::new(-1)));
        assert!(is_local_poll_id(PollId::new(i32::MIN as i64 + 1)));
        assert!(!is_local_poll_id(PollId::new(i32::MIN as i64)));
        assert!(!is_local_poll_id(PollId::new(i32::MIN as i64 - 1)));
    }

    #[test]
    fn quiz_correct_option_ids_checks() {
        // PollManager.cpp:453-469.
        assert_eq!(
            check_quiz_correct_option_ids(&[], 3, false)
                .unwrap_err()
                .code(),
            400
        );
        assert_eq!(
            check_quiz_correct_option_ids(&[], 3, true)
                .err()
                .map(|s| s.message().to_owned()),
            None
        );
        assert!(check_quiz_correct_option_ids(&[0, 1, 2], 3, false).is_ok());
        assert_eq!(
            check_quiz_correct_option_ids(&[2, 1], 3, true)
                .unwrap_err()
                .message(),
            "Correct quiz option list must be increasing"
        );
        assert_eq!(
            check_quiz_correct_option_ids(&[1, 1], 3, true)
                .unwrap_err()
                .message(),
            "Correct quiz option list must be increasing"
        );
        assert_eq!(
            check_quiz_correct_option_ids(&[-1], 3, true)
                .unwrap_err()
                .message(),
            "Wrong quiz correct_option_id"
        );
        assert_eq!(
            check_quiz_correct_option_ids(&[3], 3, true)
                .unwrap_err()
                .message(),
            "Wrong quiz correct_option_id"
        );
        assert!(check_quiz_correct_option_ids(&[0, 2], 3, true).is_ok());
    }

    #[test]
    fn vote_percentage_nearest_rounding() {
        // The "just round to the nearest" band: the option counts don't sum to
        // the total (PollManager.cpp:627-646). The total clamps DOWN to the
        // sum first.
        assert_eq!(get_vote_percentage(&[1, 1, 2], 5), vec![25, 25, 50]);
        assert_eq!(get_vote_percentage(&[1, 1, 2], 6), vec![25, 25, 50]);
        assert_eq!(get_vote_percentage(&[0, 0], 5), vec![0, 0]);
    }

    #[test]
    fn vote_percentage_equal_options_stay_equal() {
        // The exact-sum band (PollManager.cpp:648-721).
        // 1/3 tie: 25/75, nothing to top up.
        assert_eq!(get_vote_percentage(&[1, 3], 4), vec![25, 75]);
        // two-way 1/1: exactly 50/50.
        assert_eq!(get_vote_percentage(&[1, 1], 2), vec![50, 50]);
        // 1/1/2 (four voters): 25/25/50, no gap.
        assert_eq!(get_vote_percentage(&[1, 1, 2], 4), vec![25, 25, 50]);
        // 2/2/6 (ten voters): 20/20/60, no gap.
        assert_eq!(get_vote_percentage(&[2, 2, 6], 10), vec![20, 20, 60]);
        // 1/2/4 (seven voters): 14/28/57 sums 99; the 2-vote option has the
        // only gap ≤ total/2, so its whole equal-count group takes the point.
        assert_eq!(get_vote_percentage(&[1, 2, 4], 7), vec![14, 29, 57]);
    }

    #[test]
    fn vote_percentage_halves_round_to_fifty() {
        // The half rule: on an even total, an option sitting on gap == total/2
        // with ≥ 50% never rounds up (PollManager.cpp:686-689); smaller groups
        // do.
        // 1/2/4 (seven voters): the single top-up lands on the 2-vote group.
        assert_eq!(get_vote_percentage(&[1, 2, 4], 7), vec![14, 29, 57]);
        // 1/1 (three voters): the total clamps to the sum 2 → 50/50.
        assert_eq!(get_vote_percentage(&[1, 1], 3), vec![50, 50]);
        // 0/1/1 (two voters): exact 0/50/50 — the halves keep 50.
        assert_eq!(get_vote_percentage(&[0, 1, 1], 2), vec![0, 50, 50]);
        // 1/1/1/2 (five voters): exact 20/20/20/40.
        assert_eq!(get_vote_percentage(&[1, 1, 1, 2], 5), vec![20, 20, 20, 40]);
        // 3/3/2 (eight voters): 37/37/25 = 99; both 3-groups have gap == 4 ==
        // total/2 but 37 < 50, the count 2 exceeds the leftover 1, so nothing
        // moves.
        assert_eq!(get_vote_percentage(&[3, 3, 2], 8), vec![37, 37, 25]);
        // 1/1/1 (three voters): every gap is 2 > total/2 — no wrong-direction
        // rounding, the sum stays 99.
        assert_eq!(get_vote_percentage(&[1, 1, 1], 3), vec![33, 33, 33]);
        // 5/2/1 (eight voters): the 62% option sits on gap == total/2 — the
        // half rule keeps it there while the 12% option takes the point.
        assert_eq!(get_vote_percentage(&[5, 2, 1], 8), vec![62, 25, 13]);
    }

    // ---------------------------------------------------------------------------
    // PollOption store/parse
    // ---------------------------------------------------------------------------

    fn sample_poll_option() -> PollOption {
        PollOption {
            text: FormattedText {
                text: "Option \u{1F389}".to_owned(),
                entities: vec![italic_entity(0, 6)],
            },
            media_type: None,
            added_by_dialog_id: DialogId::from_user_id(UserId::new(123)),
            recent_voter_dialog_ids: vec![
                DialogId::from_user_id(UserId::new(1)),
                DialogId::from_user_id(UserId::new(2)),
            ],
            added_date: 1700000000,
            voter_count: 7,
            is_chosen: true,
            data: "vote-payload-bytes".to_owned(),
        }
    }

    #[test]
    fn poll_option_store_parse_round_trips_every_field() {
        let option = sample_poll_option();
        let mut out = Vec::new();
        let mut env = TestStoreEnv::default();
        option.store(&mut env, &mut out);

        let mut parser = Parser::new(&out);
        let parsed = PollOption::parse(&mut env, &mut parser).unwrap();
        parser.fetch_end().unwrap();

        assert_eq!(parsed.text, option.text);
        assert_eq!(parsed.data, option.data);
        assert_eq!(parsed.added_by_dialog_id, option.added_by_dialog_id);
        assert_eq!(parsed.added_date, option.added_date);
        assert_eq!(
            parsed.recent_voter_dialog_ids,
            option.recent_voter_dialog_ids
        );
        assert_eq!(parsed.voter_count, option.voter_count);
        assert!(parsed.is_chosen);
        assert_eq!(parsed.media_type, None);
    }

    #[test]
    fn poll_option_store_omits_default_optionals() {
        let option = PollOption {
            text: FormattedText {
                text: "Plain".to_owned(),
                entities: Vec::new(),
            },
            ..PollOption::default()
        };
        let mut out = Vec::new();
        let mut env = TestStoreEnv::default();
        option.store(&mut env, &mut out);

        // flags block + "Plain" (1 length byte + 5 bytes padded to 8) —
        // nothing else.
        assert_eq!(out.len(), 4 + 8);
        let mut parser = Parser::new(&out);
        let parsed = PollOption::parse(&mut env, &mut parser).unwrap();
        parser.fetch_end().unwrap();
        assert_eq!(parsed.text.text, "Plain");
        assert!(!parsed.is_chosen);
        assert_eq!(parsed.voter_count, 0);
        assert_eq!(parsed.data, "");
    }

    #[test]
    fn poll_option_media_flag_rides_the_seam() {
        // The has_media path stores through the seam; the record keeps the
        // content-type tag (the M.9 integration owns the bytes).
        let mut option = sample_poll_option();
        option.media_type = Some(9);
        let mut out = Vec::new();
        let mut env = TestStoreEnv::default();
        option.store(&mut env, &mut out);
        assert_eq!(env.stored_media, vec![9], "the seam saw the content tag");

        // parse fails honestly on the seam (the bytes belong to the M.9 codec)
        let mut parser = Parser::new(&out);
        assert!(PollOption::parse(&mut env, &mut parser).is_err());
    }

    #[test]
    fn poll_option_equality_ignores_voter_machine_fields() {
        // PollOption.cpp:162-173 — voter_count/recent voters/is_chosen don't
        // compare; the media compares by type tag only.
        let lhs = sample_poll_option();
        let mut rhs = sample_poll_option();
        rhs.voter_count += 1;
        rhs.is_chosen = !rhs.is_chosen;
        rhs.recent_voter_dialog_ids.clear();
        assert!(PollOption::options_equal(&lhs, &rhs));

        let mut with_media = sample_poll_option();
        with_media.media_type = Some(3);
        assert!(!PollOption::options_equal(&lhs, &with_media));

        let mut different_text = sample_poll_option();
        different_text.text.text = "Other".to_owned();
        assert!(!PollOption::options_equal(&lhs, &different_text));
    }

    // ---------------------------------------------------------------------------
    // Poll store/parse
    // ---------------------------------------------------------------------------

    fn sample_poll() -> Poll {
        Poll {
            question: FormattedText {
                text: "Question?".to_owned(),
                entities: vec![custom_emoji_entity(0, 8, 12345)],
            },
            options: vec![sample_poll_option(), {
                let mut option = sample_poll_option();
                option.is_chosen = false;
                option.voter_count = 3;
                option.data = "second".to_owned();
                option
            }],
            option_min_channels: vec![(
                ChannelId::new(1001),
                MinChannel {
                    title_: "Min chat".to_owned(),
                    photo_: crate::photo::DialogPhoto::default(),
                    accent_color_id_: AccentColorId::new(7),
                    is_megagroup_: true,
                },
            )],
            recent_option_voter_min_channels: Vec::new(),
            recent_voter_dialog_ids: vec![DialogId::from_user_id(UserId::new(42))],
            recent_voter_min_channels: vec![(
                ChannelId::new(1002),
                MinChannel {
                    title_: String::new(),
                    photo_: crate::photo::DialogPhoto::default(),
                    accent_color_id_: AccentColorId::new(0),
                    is_megagroup_: false,
                },
            )],
            explanation: FormattedText {
                text: "Because".to_owned(),
                entities: vec![italic_entity(0, 7)],
            },
            explanation_media_type: None,
            correct_option_ids: vec![0, 1],
            total_voter_count: 10,
            open_period: 300,
            close_date: 1700000600,
            hash: 987654321,
            country_codes: vec!["RU".to_owned(), "US".to_owned()],
            is_anonymous: false,
            allow_multiple_answers: true,
            has_open_answers: true,
            has_revoting_disabled: false,
            subscribers_only: true,
            shuffle_answers: true,
            hide_results_until_close: true,
            is_quiz: true,
            is_closed: false,
            is_updated_after_close: false,
            is_creator: true,
            has_unread_votes: true,
            can_view_stats: true,
        }
    }

    #[test]
    fn poll_store_parse_round_trips_every_field() {
        let poll = sample_poll();
        let mut out = Vec::new();
        let mut env = TestStoreEnv::default();
        poll.store(&mut env, &mut out);

        let mut parser = Parser::new(&out);
        let parsed = Poll::parse(&mut env, &mut parser).unwrap();
        parser.fetch_end().unwrap();

        assert_eq!(parsed.question, poll.question);
        assert_eq!(parsed.options, poll.options);
        assert_eq!(parsed.total_voter_count, 10);
        assert_eq!(parsed.correct_option_ids, vec![0, 1]);
        assert_eq!(parsed.open_period, 300);
        assert_eq!(parsed.close_date, 1700000600);
        assert_eq!(parsed.explanation, poll.explanation);
        assert_eq!(parsed.recent_voter_dialog_ids, poll.recent_voter_dialog_ids);
        assert_eq!(
            parsed.recent_voter_min_channels,
            poll.recent_voter_min_channels
        );
        assert_eq!(parsed.option_min_channels, poll.option_min_channels);
        assert_eq!(parsed.hash, 987654321);
        assert_eq!(parsed.country_codes, poll.country_codes);
        assert!(!parsed.is_anonymous);
        assert!(parsed.allow_multiple_answers);
        assert!(parsed.has_open_answers);
        assert!(!parsed.has_revoting_disabled);
        assert!(parsed.subscribers_only);
        assert!(parsed.shuffle_answers);
        assert!(parsed.hide_results_until_close);
        assert!(parsed.is_quiz);
        assert!(!parsed.is_closed);
        assert!(!parsed.is_updated_after_close);
        assert!(parsed.is_creator);
        assert!(parsed.has_unread_votes);
        assert!(parsed.can_view_stats);
    }

    #[test]
    fn poll_store_parse_regular_poll_minimal_form() {
        // The non-quiz minimal record: the single correct-option form and the
        // optional blocks are all absent.
        let poll = Poll {
            question: FormattedText {
                text: "Yes/No?".to_owned(),
                entities: Vec::new(),
            },
            options: vec![PollOption {
                text: FormattedText {
                    text: "Yes".to_owned(),
                    entities: Vec::new(),
                },
                data: "opt1".to_owned(),
                voter_count: 0,
                ..PollOption::default()
            }],
            is_anonymous: true,
            ..Poll::default()
        };
        let mut out = Vec::new();
        let mut env = TestStoreEnv::default();
        poll.store(&mut env, &mut out);

        let mut parser = Parser::new(&out);
        let parsed = Poll::parse(&mut env, &mut parser).unwrap();
        parser.fetch_end().unwrap();
        assert_eq!(parsed.question.text, "Yes/No?");
        assert_eq!(parsed.options.len(), 1);
        assert_eq!(parsed.options[0].data, "opt1");
        assert!(parsed.is_anonymous);
        assert!(!parsed.is_quiz);
        assert_eq!(parsed.correct_option_ids, Vec::<i32>::new());
    }

    #[test]
    fn poll_store_parse_quiz_single_correct_option() {
        // The non-multiple correct ids are stored as the single int form
        // (-1 encodes "empty").
        let mut poll = sample_poll();
        poll.correct_option_ids = vec![1];
        poll.option_min_channels.clear();
        poll.recent_voter_min_channels.clear();
        let mut out = Vec::new();
        let mut env = TestStoreEnv::default();
        poll.store(&mut env, &mut out);

        let mut parser = Parser::new(&out);
        let parsed = Poll::parse(&mut env, &mut parser).unwrap();
        parser.fetch_end().unwrap();
        assert_eq!(parsed.correct_option_ids, vec![1]);

        // The -1 encoding parses as an empty list.
        poll.correct_option_ids.clear();
        let mut out = Vec::new();
        poll.store(&mut env, &mut out);
        let mut parser = Parser::new(&out);
        let parsed = Poll::parse(&mut env, &mut parser).unwrap();
        parser.fetch_end().unwrap();
        assert_eq!(parsed.correct_option_ids, Vec::<i32>::new());

        // An out-of-range correct id corrupts the record (the C++ sets the
        // parser error; the Rust band is the generic corruption error).
        let mut out = Vec::new();
        store_flags(
            &mut out,
            [
                false, false, false, true, false, false, false, false, false, false, false, false,
                false, false, true, false, false, false, false, false, false, false, false, false,
                false, false, false,
            ],
        );
        store_string(&mut out, "Q");
        store_i32(&mut out, 1); // one option
        {
            let option = PollOption {
                text: FormattedText {
                    text: "Yes".to_owned(),
                    entities: Vec::new(),
                },
                ..PollOption::default()
            };
            option.store(&mut env, &mut out);
        }
        store_i32(&mut out, 0);
        store_i32(&mut out, 5); // the out-of-range correct option id
        let mut parser = Parser::new(&out);
        assert!(Poll::parse(&mut env, &mut parser).is_err());
    }

    #[test]
    fn poll_parse_legacy_recent_voter_user_ids() {
        // The pre-flags-era bit 4 form: recent voters stored as user ids
        // (PollManager.hpp:137, 182-186).
        let mut out = Vec::new();
        store_flags(
            &mut out,
            [
                false, // is_closed
                false, // is_public (anonymous)
                false, // allow_multiple_answers
                false, // is_quiz
                true,  // has_recent_voter_user_ids — the legacy bit 4 form
                false, // has_open_period
                false, // has_close_date
                false, // has_explanation
                false, // is_updated_after_close
                false, // has_recent_voter_dialog_ids
                false, // has_recent_voter_min_channels
                false, // has_question_entities
                false, // has_multiple_correct_option_ids
                false, // has_open_answers
                true,  // know_revoting_disabled
                false, // has_revoting_disabled
                false, // shuffle_answers
                false, // hide_results_until_close
                false, // is_creator
                false, // has_hash
                false, // has_unread_votes
                false, // has_explanation_media
                false, // has_option_min_channels
                false, // has_recent_option_voter_min_channels
                false, // subscribers_only
                false, // has_country_codes
                false, // can_view_stats
            ],
        );
        store_string(&mut out, "Legacy"); // question text
        store_i32(&mut out, 0); // no options
        store_i32(&mut out, 3); // total_voter_count
        store_i32(&mut out, 2); // two legacy recent-voter user ids
        store_i64(&mut out, 77); // UserId (Support64BitIds: store_long)
        store_i64(&mut out, 78);
        let mut env = TestStoreEnv::default();
        let mut parser = Parser::new(&out);
        let parsed = Poll::parse(&mut env, &mut parser).unwrap();
        parser.fetch_end().unwrap();
        assert_eq!(parsed.question.text, "Legacy");
        assert_eq!(
            parsed.recent_voter_dialog_ids,
            vec![
                DialogId::from_user_id(UserId::new(77)),
                DialogId::from_user_id(UserId::new(78)),
            ]
        );
    }

    #[test]
    fn poll_parse_revoting_disabled_fallback() {
        // The know_revoting_disabled=false band: has_revoting_disabled rides
        // the quiz flag (PollManager.hpp:205-207) — a quiz becomes
        // revoting-disabled, a regular poll doesn't.
        // Quiz: the fallback sets the flag true regardless of the stored bit.
        let mut out = Vec::new();
        store_flags(
            &mut out,
            [
                false, false, false, true, false, false, false, false, false, false, false, false,
                false, false, false, // know_revoting_disabled = false
                false, false, false, false, false, false, false, false, false, false, false, false,
            ],
        );
        store_string(&mut out, "Q");
        store_i32(&mut out, 0);
        store_i32(&mut out, 0);
        store_i32(&mut out, -1); // the quiz single-correct-id empty form
        let mut env = TestStoreEnv::default();
        let mut parser = Parser::new(&out);
        let parsed = Poll::parse(&mut env, &mut parser).unwrap();
        parser.fetch_end().unwrap();
        assert!(parsed.is_quiz);
        assert!(parsed.has_revoting_disabled, "the fallback rides is_quiz");

        // Regular poll with the stored flag true: the fallback clears it.
        let mut out = Vec::new();
        store_flags(
            &mut out,
            [
                false, false, false, false, false, false, false, false, false, false, false, false,
                false, false, false,
                true, // has_revoting_disabled despite the unknown-flags era
                false, false, false, false, false, false, false, false, false, false, false,
            ],
        );
        store_string(&mut out, "Q");
        store_i32(&mut out, 0);
        store_i32(&mut out, 0);
        let mut parser = Parser::new(&out);
        let parsed = Poll::parse(&mut env, &mut parser).unwrap();
        parser.fetch_end().unwrap();
        assert!(!parsed.is_quiz);
        assert!(!parsed.has_revoting_disabled);
    }

    #[test]
    fn min_channel_store_parse_round_trips() {
        // The photo block rides the has_photo flag — small_file_id.is_valid()
        // — so the invalid-placeholder photo drops wholesale, the exact
        // framing the C++ produces for a photo-less MinChannel.
        let min_channels = vec![
            (
                ChannelId::new(1001),
                MinChannel {
                    title_: "Chat A".to_owned(),
                    photo_: crate::photo::DialogPhoto::default(),
                    accent_color_id_: AccentColorId::new(3),
                    is_megagroup_: true,
                },
            ),
            (ChannelId::new(1002), MinChannel::default()),
        ];
        let mut out = Vec::new();
        let mut env = TestStoreEnv::default();
        store_min_channels(&min_channels, &mut env, &mut out);

        let mut parser = Parser::new(&out);
        let parsed = parse_min_channels(&mut env, &mut parser).unwrap();
        parser.fetch_end().unwrap();
        assert_eq!(parsed, min_channels);
    }

    // ---------------------------------------------------------------------------
    // The manager log events
    // ---------------------------------------------------------------------------

    #[test]
    fn set_poll_answer_log_event_round_trips() {
        let event = SetPollAnswerLogEvent {
            poll_id: PollId::new(-12345),
            message_full_id: MessageFullId::new(
                DialogId::from_user_id(UserId::new(9)),
                MessageId::new(1 << 20),
            ),
            options: vec!["opt-a".to_owned(), "opt-b".to_owned()],
        };
        let mut out = Vec::new();
        store_log_event_version(&mut out);
        event.store(&mut out);

        let mut parser = Parser::new_log_event(&out).unwrap();
        let parsed = SetPollAnswerLogEvent::parse(&mut parser).unwrap();
        parser.fetch_end().unwrap();
        assert_eq!(parsed, event);
    }

    #[test]
    fn stop_poll_log_event_round_trips() {
        let event = StopPollLogEvent {
            poll_id: PollId::new(777),
            message_full_id: MessageFullId::new(
                DialogId::from_channel_id(ChannelId::new(1003)),
                MessageId::new(2 << 20),
            ),
        };
        let mut out = Vec::new();
        store_log_event_version(&mut out);
        event.store(&mut out);

        let mut parser = Parser::new_log_event(&out).unwrap();
        let parsed = StopPollLogEvent::parse(&mut parser).unwrap();
        parser.fetch_end().unwrap();
        assert_eq!(parsed, event);
    }

    #[test]
    fn status_error_bands_match_cpp_strings() {
        // the Status::Error(400, ...) texts the td_api layer surfaces
        let error = check_quiz_correct_option_ids(&[], 3, false).unwrap_err();
        assert_eq!(error.code(), 400);
        assert_eq!(
            error.message(),
            "Correct quiz option list must be non-empty"
        );
        let _: Result<(), Status> = check_quiz_correct_option_ids(&[0], 1, false);
    }

    // ---------------------------------------------------------------------------
    // The net-drain 39 band tests (the five wire families)
    // ---------------------------------------------------------------------------

    /// The band-test env: the full [`PollManagerEnv`] surface over
    /// recording fields (the callback-queries TestEnv convention). The
    /// send arms record their parameters; `peer = None` answers the
    /// send-leg None bands.
    struct TestEnv {
        store: TestStoreEnv,
        bot: bool,
        closing: bool,
        use_message_database: bool,
        /// `get_message_poll_id` answers `Ok(known_poll)` unless
        /// `poll_not_found` flips.
        known_poll: PollId,
        poll_not_found: bool,
        peer: Option<tdmtproto::telegram_api::InputPeer>,
        queries: Vec<String>,
        dialog_errors: Vec<(String, String)>,
        message_errors: Vec<(String, String)>,
        intake_users: usize,
        intake_chats: usize,
        binlog_adds: Vec<(i32, u64)>,
        binlog_erases: Vec<u64>,
    }

    impl Default for TestEnv {
        fn default() -> TestEnv {
            TestEnv {
                store: TestStoreEnv::default(),
                bot: false,
                closing: false,
                use_message_database: false,
                known_poll: PollId::new(31),
                poll_not_found: false,
                peer: Some(tdmtproto::telegram_api::InputPeer::inputPeerSelf(
                    tdmtproto::telegram_api::inputPeerSelf {},
                )),
                queries: Vec::new(),
                dialog_errors: Vec::new(),
                message_errors: Vec::new(),
                intake_users: 0,
                intake_chats: 0,
                binlog_adds: Vec::new(),
                binlog_erases: Vec::new(),
            }
        }
    }

    impl crate::photo::PhotoStoreEnv for TestEnv {
        fn store_file_id(&mut self, file_id: FileId, out: &mut Vec<u8>) {
            self.store.store_file_id(file_id, out);
        }

        fn parse_file_id(&mut self, parser: &mut Parser<'_>) -> Result<FileId, &'static str> {
            self.store.parse_file_id(parser)
        }
    }

    impl PollStoreEnv for TestEnv {
        fn store_poll_media(&mut self, media_type: i32, out: &mut Vec<u8>) {
            self.store.store_poll_media(media_type, out);
        }

        fn parse_poll_media(&mut self, parser: &mut Parser<'_>) -> Result<i32, &'static str> {
            self.store.parse_poll_media(parser)
        }
    }

    impl PollManagerEnv for TestEnv {
        fn auth_is_bot(&self) -> bool {
            self.bot
        }

        fn close_flag(&self) -> bool {
            self.closing
        }

        fn use_message_database(&self) -> bool {
            self.use_message_database
        }

        fn unix_time(&self) -> i32 {
            1700000000
        }

        fn server_time(&self) -> f64 {
            1700000000.0
        }

        fn now(&self) -> f64 {
            1700000000.0
        }

        fn is_test_dc(&self) -> bool {
            false
        }

        fn pmc_get_raw(&mut self, _key: &[u8]) -> Vec<u8> {
            Vec::new()
        }

        fn pmc_set_raw(&mut self, _key: &[u8], _value: &[u8]) {}

        fn binlog_add(&mut self, handler_type: i32, _data: &[u8]) -> u64 {
            let id = self.binlog_adds.len() as u64 + 1;
            self.binlog_adds.push((handler_type, id));
            id
        }

        fn binlog_rewrite(&mut self, _log_event_id: u64, _handler_type: i32, _data: &[u8]) -> u64 {
            0
        }

        fn binlog_erase(&mut self, log_event_id: u64) {
            self.binlog_erases.push(log_event_id);
        }

        fn add_min_channel(&mut self, _channel_id: ChannelId, _min_channel: MinChannel) {}

        fn get_min_channel(&self, _channel_id: ChannelId) -> Option<MinChannel> {
            None
        }

        fn channel_is_member(&self, _channel_id: ChannelId) -> bool {
            false
        }

        fn channel_date(&self, _channel_id: ChannelId) -> i32 {
            0
        }

        fn have_dialog_info(&self, _dialog_id: DialogId) -> bool {
            false
        }

        fn have_min_user(&self, _user_id: UserId) -> bool {
            false
        }

        fn get_my_id(&self) -> UserId {
            UserId::new(7)
        }

        fn get_my_dialog_id(&self) -> DialogId {
            DialogId::from_user_id(UserId::new(7))
        }

        fn get_option_integer(&self, _name: &str, default_value: i64) -> i64 {
            default_value
        }

        fn get_option_string(&self, _name: &str) -> String {
            String::new()
        }

        fn get_message_poll_id(
            &mut self,
            _message_full_id: &MessageFullId,
            _ignore_has_poll: bool,
        ) -> Result<PollId, Status> {
            if self.poll_not_found {
                Err(Status::error(400, "Chat not found"))
            } else {
                Ok(self.known_poll)
            }
        }

        fn has_message_sender_user_id(&mut self, _message_full_id: &MessageFullId) -> bool {
            false
        }

        fn on_external_update_message_content(&mut self, _message_full_id: MessageFullId) {}

        fn on_update_poll_has_unread_votes(
            &mut self,
            _message_full_id: MessageFullId,
            _has_unread_votes: bool,
        ) {
        }

        fn register_poll_web_pages(&mut self, _poll_id: PollId, _web_page_ids: Vec<i64>) {}

        fn get_min_message_sender_object(
            &mut self,
            dialog_id: DialogId,
        ) -> Option<td_api::MessageSender> {
            Some(td_api::MessageSender::messageSenderUser(
                td_api::messageSenderUser {
                    user_id: dialog_id.get(),
                },
            ))
        }

        fn get_poll_media_object(&mut self, _media_type: Option<i32>) -> Option<td_api::PollMedia> {
            None
        }

        fn send_update(&mut self, _update: td_api::Update) {}

        fn get_formatted_text_with_users(
            &mut self,
            text: String,
            _entities: Vec<tdmtproto::telegram_api::MessageEntity>,
            _skip_media_timestamps: bool,
            _skip_trim: bool,
            _source: &str,
        ) -> FormattedText {
            FormattedText {
                text,
                entities: Vec::new(),
            }
        }

        fn send_vote_query(
            &mut self,
            _message_full_id: &MessageFullId,
            options: Vec<String>,
            _generation: u64,
        ) -> Result<(), Status> {
            let Some(_) = self.peer.clone() else {
                return Err(Status::error(400, "Can't access the chat"));
            };
            self.queries.push(format!("sendVote:{}", options.join(",")));
            Ok(())
        }

        fn send_get_poll_results_query(
            &mut self,
            _message_full_id: &MessageFullId,
            _poll_hash: i64,
            _generation: u64,
        ) -> bool {
            if self.peer.is_none() {
                return false;
            }
            self.queries.push("getPollResults".to_owned());
            true
        }

        fn send_get_poll_voters_query(
            &mut self,
            _message_full_id: &MessageFullId,
            _poll_id: PollId,
            option_id: i32,
            option: Vec<u8>,
            offset: &str,
            limit: i32,
        ) -> Result<(), Status> {
            if self.peer.is_none() {
                return Err(Status::error(400, "Chat is not accessible"));
            }
            self.queries.push(format!(
                "getPollVotes:{}:{}:{offset}:{limit}",
                String::from_utf8_lossy(&option),
                option_id
            ));
            Ok(())
        }

        fn send_stop_poll_query(
            &mut self,
            _message_full_id: &MessageFullId,
            _reply_markup: Option<&crate::reply_markup::ReplyMarkup>,
            _poll_id: PollId,
        ) -> Result<(), Status> {
            if self.peer.is_none() {
                return Err(Status::error(400, "Can't access the chat"));
            }
            self.queries.push("stopPoll".to_owned());
            Ok(())
        }

        fn send_delete_poll_answer_query(
            &mut self,
            _message_full_id: &MessageFullId,
            option_id: Vec<u8>,
        ) -> Result<(), Status> {
            if self.peer.is_none() {
                return Err(Status::error(400, "Can't access the chat"));
            }
            self.queries.push(format!(
                "deletePollAnswer:{}",
                String::from_utf8_lossy(&option_id)
            ));
            Ok(())
        }

        fn on_get_message_error(
            &mut self,
            _dialog_id: DialogId,
            _message_id: MessageId,
            status: &Status,
            _source: &str,
        ) -> bool {
            self.message_errors.push((
                "GetPollResultsQuery".to_owned(),
                status.message().to_owned(),
            ));
            false
        }

        fn on_get_dialog_error(
            &mut self,
            _dialog_id: DialogId,
            status: &Status,
            source: &str,
        ) -> bool {
            self.dialog_errors
                .push((source.to_owned(), status.message().to_owned()));
            false
        }

        fn on_get_users(&mut self, _users: Vec<tdmtproto::telegram_api::User>, _source: &str) {
            self.intake_users += 1;
        }

        fn on_get_chats(&mut self, _chats: Vec<tdmtproto::telegram_api::Chat>, _source: &str) {
            self.intake_chats += 1;
        }
    }

    // the test promise helpers (the M.45 RecordingPromise convention)

    type Recorded<T> = std::sync::Arc<std::sync::Mutex<Vec<std::result::Result<T, String>>>>;

    struct RecordingPromise<T: Send + 'static> {
        outcomes: Recorded<T>,
    }

    impl<T: Send + 'static> tdutils::promise::PromiseInterface<T> for RecordingPromise<T> {
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

    fn band_updates() -> tdmtproto::telegram_api::Updates {
        tdmtproto::telegram_api::Updates::updatesCombined(
            tdmtproto::telegram_api::updatesCombined {
                updates: Vec::new(),
                users: Vec::new(),
                chats: Vec::new(),
                date: 1700000000,
                seq_start: 0,
                seq: 0,
            },
        )
    }

    fn seed_poll(bot: bool) -> Poll {
        Poll {
            options: vec![
                PollOption {
                    voter_count: 1,
                    data: "a".to_owned(),
                    ..PollOption::default()
                },
                PollOption {
                    voter_count: 0,
                    data: "b".to_owned(),
                    ..PollOption::default()
                },
            ],
            hash: 77,
            is_anonymous: false,
            allow_multiple_answers: true,
            is_creator: !bot,
            ..Poll::default()
        }
    }

    fn band_message_full_id() -> MessageFullId {
        MessageFullId::new(
            DialogId::from_user_id(UserId::new(7)),
            MessageId::new(1 << 20),
        )
    }

    #[test]
    fn set_poll_answer_parks_dispatches_and_settles_the_ladder() {
        let mut env = TestEnv::default();
        let mut manager = PollManager::new();
        let poll_id = env.known_poll;
        manager.polls.insert(poll_id, seed_poll(false));

        // the vote parks the promise + dispatches the wire query
        let (promise, outcomes) = recording_promise::<()>();
        manager.set_poll_answer(&mut env, band_message_full_id(), vec![0], promise);
        assert_eq!(env.queries, vec!["sendVote:a".to_owned()]);
        assert!(outcomes.lock().unwrap().is_empty());
        assert!(manager.pending_answers.contains_key(&poll_id));
        let generation = manager.pending_answers.get(&poll_id).unwrap().generation;

        // the Ok answer re-enters the ladder: the bookkeeping runs, the
        // updates hand back for the engine tail, then the finish settles
        // the promise (the router order collapses here)
        let (updates, tail_poll_id, tail_generation) = manager
            .on_send_vote_answer(&mut env, generation, Ok(band_updates()))
            .expect("the Ok updates hand back");
        let _ = updates;
        assert_eq!(tail_poll_id, poll_id);
        assert_eq!(tail_generation, generation);
        manager.on_set_poll_answer_finished(&mut env, poll_id, Ok(()), generation);
        assert_eq!(*outcomes.lock().unwrap(), vec![Ok(())]);
        assert!(!manager.pending_answers.contains_key(&poll_id));
        assert!(manager.pending_net.send_vote.is_empty());
    }

    #[test]
    fn set_poll_answer_send_leg_band_runs_the_error_head_and_ladder() {
        let mut env = TestEnv {
            peer: None,
            ..TestEnv::default()
        };
        let mut manager = PollManager::new();
        let poll_id = env.known_poll;
        manager.polls.insert(poll_id, seed_poll(false));

        let (promise, outcomes) = recording_promise::<()>();
        manager.set_poll_answer(&mut env, band_message_full_id(), vec![0], promise);
        // the None-peer band: the handler's on_error head runs, then the
        // ladder settles the parked promise with the 400
        assert_eq!(
            env.dialog_errors,
            vec![(
                "SendVoteQuery".to_owned(),
                "Can't access the chat".to_owned()
            )]
        );
        assert_eq!(
            *outcomes.lock().unwrap(),
            vec![Err("400 Can't access the chat".to_owned())]
        );
        // no query was dispatched — the lot entry died with the handler
        assert!(manager.pending_net.send_vote.is_empty());
        // the second vote on the same poll re-enters do_set_poll_answer
        // cleanly (the pending entry stays coherent)
        let (promise2, outcomes2) = recording_promise::<()>();
        manager.set_poll_answer(&mut env, band_message_full_id(), vec![1], promise2);
        assert_eq!(outcomes2.lock().unwrap().len(), 1);
    }

    #[test]
    fn replaced_vote_releases_and_the_late_answer_pairs_by_generation() {
        let mut env = TestEnv::default();
        let mut manager = PollManager::new();
        let poll_id = env.known_poll;
        manager.polls.insert(poll_id, seed_poll(false));

        let (promise1, outcomes1) = recording_promise::<()>();
        manager.set_poll_answer(&mut env, band_message_full_id(), vec![0], promise1);
        let generation1 = manager.pending_answers.get(&poll_id).unwrap().generation;

        // a DIFFERENT answer replaces it: the old promises release Ok,
        // the new generation parks
        let (promise2, outcomes2) = recording_promise::<()>();
        manager.set_poll_answer(&mut env, band_message_full_id(), vec![1], promise2);
        assert_eq!(*outcomes1.lock().unwrap(), vec![Ok(())]);
        let generation2 = manager.pending_answers.get(&poll_id).unwrap().generation;
        assert_ne!(generation1, generation2);
        assert_eq!(manager.pending_net.send_vote.len(), 2);

        // the replaced query's late answer pairs with ITS lot entry and
        // drops at the generation check (no state touch, no settle)
        let tail = manager.on_send_vote_answer(&mut env, generation1, Ok(band_updates()));
        assert!(tail.is_none());
        assert_eq!(manager.pending_net.send_vote.len(), 1);
        assert!(outcomes2.lock().unwrap().is_empty());
        assert!(manager.pending_answers.contains_key(&poll_id));

        // the current generation's answer settles
        let (updates, _, generation) = manager
            .on_send_vote_answer(&mut env, generation2, Ok(band_updates()))
            .unwrap();
        let _ = updates;
        assert_eq!(generation, generation2);
        manager.on_set_poll_answer_finished(&mut env, poll_id, Ok(()), generation);
        assert_eq!(*outcomes2.lock().unwrap(), vec![Ok(())]);
    }

    #[test]
    fn get_poll_voters_cache_bands_and_wire_send() {
        let mut env = TestEnv::default();
        let mut manager = PollManager::new();
        let poll_id = env.known_poll;
        manager.polls.insert(poll_id, seed_poll(false));

        // a cached page answers from the map (offset < cur_offset)
        manager
            .poll_voters
            .entry(poll_id)
            .or_default()
            .resize_with(2, Default::default);
        let voter = DialogId::from_user_id(UserId::new(9));
        manager.poll_voters.get_mut(&poll_id).unwrap()[0].voters =
            vec![(voter, 5), (voter, 6), (voter, 7)];
        let (promise, outcomes) = recording_promise::<td_api::PollVoters>();
        manager.get_poll_voters(&mut env, band_message_full_id(), 0, 1, 2, promise);
        let outcomes = outcomes.lock().unwrap();
        let object = outcomes[0].as_ref().expect("the cached page answers");
        assert_eq!(object.total_count, 3);
        assert_eq!(object.voters.len(), 2);
        assert_eq!(object.voters[0].date, 6);
        drop(outcomes);

        // too big offset errors
        let (promise, outcomes) = recording_promise::<td_api::PollVoters>();
        manager.get_poll_voters(&mut env, band_message_full_id(), 0, 4, 2, promise);
        assert!(
            matches!(&outcomes.lock().unwrap()[0], Err(message) if message == "400 Too big offset specified; voters can be received only consequently")
        );

        // the exhausted page (next_offset empty, cur_offset > 0) answers
        // the empty object
        let (promise, outcomes) = recording_promise::<td_api::PollVoters>();
        manager.get_poll_voters(&mut env, band_message_full_id(), 0, 3, 2, promise);
        let outcomes = outcomes.lock().unwrap();
        let object = outcomes[0].as_ref().expect("the exhausted page answers");
        assert_eq!(object.total_count, 0);
        assert!(object.voters.is_empty());
        drop(outcomes);

        // the voter_count == 0 option answers the empty object without a
        // query
        let (promise, outcomes) = recording_promise::<td_api::PollVoters>();
        manager.get_poll_voters(&mut env, band_message_full_id(), 1, 0, 2, promise);
        let outcomes = outcomes.lock().unwrap();
        assert!(
            outcomes[0]
                .as_ref()
                .expect("the empty option answers")
                .voters
                .is_empty()
        );
        drop(outcomes);
        assert!(env.queries.is_empty());

        // a fresh send parks the promise + fires the query (option 0 has
        // voter_count 1 > 0)
        manager.poll_voters.get_mut(&poll_id).unwrap().clear();
        let (promise, outcomes) = recording_promise::<td_api::PollVoters>();
        manager.get_poll_voters(&mut env, band_message_full_id(), 0, 0, 2, promise);
        assert_eq!(env.queries, vec!["getPollVotes:a:0::2".to_owned()]);
        assert!(outcomes.lock().unwrap().is_empty());
        assert_eq!(manager.pending_net.get_poll_voters.len(), 1);
    }

    #[test]
    fn get_poll_voters_intake_bands() {
        let mut env = TestEnv::default();
        let mut manager = PollManager::new();
        let poll_id = env.known_poll;
        manager.polls.insert(poll_id, seed_poll(false));
        manager
            .poll_voters
            .entry(poll_id)
            .or_default()
            .resize_with(2, Default::default);

        // the send-leg 400 (no read access) settles the parked promises
        // directly — no dialog-error head on this family
        let mut no_peer_env = TestEnv {
            peer: None,
            ..TestEnv::default()
        };
        let (promise, outcomes) = recording_promise::<td_api::PollVoters>();
        manager.get_poll_voters(&mut no_peer_env, band_message_full_id(), 0, 0, 2, promise);
        assert_eq!(
            *outcomes.lock().unwrap(),
            vec![Err("400 Chat is not accessible".to_owned())]
        );
        assert!(manager.pending_net.get_poll_voters.is_empty());

        // the happy intake: the three vote variants filter by the option
        // data, the users/chats intakes run, the count mismatch re-arms
        // the update timeout (the seeded voter_count 1 != the wire
        // count 3)
        let (promise, outcomes) = recording_promise::<td_api::PollVoters>();
        manager.get_poll_voters(&mut env, band_message_full_id(), 0, 0, 2, promise);
        let answer = tdmtproto::telegram_api::messages_VotesList {
            flags: 0,
            count: 3,
            votes: vec![
                // the wrong-option vote drops (messagePeerVote)
                tdmtproto::telegram_api::MessagePeerVote::messagePeerVote(
                    tdmtproto::telegram_api::messagePeerVote {
                        peer: Some(tdmtproto::telegram_api::Peer::peerUser(
                            tdmtproto::telegram_api::peerUser { user_id: 11 },
                        )),
                        option: b"b".to_vec(),
                        date: 1,
                    },
                ),
                // the right-option vote lands (messagePeerVote)
                tdmtproto::telegram_api::MessagePeerVote::messagePeerVote(
                    tdmtproto::telegram_api::messagePeerVote {
                        peer: Some(tdmtproto::telegram_api::Peer::peerUser(
                            tdmtproto::telegram_api::peerUser { user_id: 9 },
                        )),
                        option: b"a".to_vec(),
                        date: 5,
                    },
                ),
                // the multi-vote lands when the option rides its list
                tdmtproto::telegram_api::MessagePeerVote::messagePeerVoteMultiple(
                    tdmtproto::telegram_api::messagePeerVoteMultiple {
                        peer: Some(tdmtproto::telegram_api::Peer::peerUser(
                            tdmtproto::telegram_api::peerUser { user_id: 12 },
                        )),
                        options: vec![b"a".to_vec()],
                        date: 7,
                    },
                ),
            ],
            chats: Vec::new(),
            users: Vec::new(),
            next_offset: "next".to_owned(),
        };
        manager.on_get_poll_voters_answer(&mut env, poll_id, 0, Ok(answer));
        assert_eq!(env.intake_users, 1);
        assert_eq!(env.intake_chats, 1);
        let outcomes = outcomes.lock().unwrap();
        let object = outcomes[0].as_ref().expect("the page answers");
        // the wire count (3) vs the known voters (2) — max wins
        assert_eq!(object.total_count, 3);
        assert_eq!(object.voters.len(), 2);
        drop(outcomes);
        assert_eq!(
            manager.poll_voters.get(&poll_id).unwrap()[0].next_offset,
            "next"
        );
        // the voter count mismatch re-armed the update timeout at 0.0
        assert!(manager.update_poll_timeout.contains_key(&poll_id.get()));
        manager.update_poll_timeout.clear();

        // the offset-mismatch drop: a fresh page request parks at the
        // current offset (== cur_offset), then an intervening update
        // resets the cached next_offset — the answer's captured offset
        // mismatches and the intake drops it without touching the
        // promises (cpp:1638-1647)
        let (promise, outcomes) = recording_promise::<td_api::PollVoters>();
        manager.get_poll_voters(&mut env, band_message_full_id(), 0, 2, 2, promise);
        assert!(outcomes.lock().unwrap().is_empty());
        manager.poll_voters.get_mut(&poll_id).unwrap()[0].next_offset = String::new();
        let stale = tdmtproto::telegram_api::messages_VotesList {
            flags: 0,
            count: 3,
            votes: Vec::new(),
            chats: Vec::new(),
            users: Vec::new(),
            next_offset: String::new(),
        };
        manager.on_get_poll_voters_answer(&mut env, poll_id, 0, Ok(stale));
        assert!(outcomes.lock().unwrap().is_empty());

        // the wire error band: the dialog-error head + the MESSAGE_ID_INVALID
        // carve-out log, then the parked promises fail (the dropped
        // mismatch promise is cleared — the C++ would have it fail on
        // its own answer; here the fresh park drives the band)
        manager.poll_voters.get_mut(&poll_id).unwrap()[0]
            .pending_queries
            .clear();
        manager.poll_voters.get_mut(&poll_id).unwrap()[0].next_offset = "next".to_owned();
        let (promise, outcomes) = recording_promise::<td_api::PollVoters>();
        manager.get_poll_voters(&mut env, band_message_full_id(), 0, 2, 2, promise);
        assert!(outcomes.lock().unwrap().is_empty());
        manager.on_get_poll_voters_answer(
            &mut env,
            poll_id,
            0,
            Err(Status::error(420, "SLOWMODE_WAIT")),
        );
        assert_eq!(
            env.dialog_errors,
            vec![("GetPollVotersQuery".to_owned(), "SLOWMODE_WAIT".to_owned())]
        );
        assert_eq!(
            *outcomes.lock().unwrap(),
            vec![Err("420 SLOWMODE_WAIT".to_owned())]
        );
    }

    #[test]
    fn get_poll_voters_entry_guards_and_anonymous_band() {
        let mut env = TestEnv::default();
        let mut manager = PollManager::new();
        let poll_id = env.known_poll;
        let mut poll = seed_poll(false);
        poll.is_anonymous = true;
        manager.polls.insert(poll_id, poll);
        manager
            .poll_voters
            .entry(poll_id)
            .or_default()
            .resize_with(2, Default::default);

        // the entry guard rejects the anonymous poll outright
        // (cpp:1571-1573)
        let (promise, outcomes) = recording_promise::<td_api::PollVoters>();
        manager.get_poll_voters(&mut env, band_message_full_id(), 0, 0, 2, promise);
        assert!(
            matches!(&outcomes.lock().unwrap()[0], Err(message) if message == "400 Poll results can't be received")
        );

        // the in-intake anonymous band (the "just in case" cpp:1634-1636)
        // converts the Ok answer into the 400 — reachable when the poll
        // turned anonymous after the send (the white-box park)
        let (promise, outcomes) = recording_promise::<td_api::PollVoters>();
        manager.poll_voters.get_mut(&poll_id).unwrap()[0]
            .pending_queries
            .push(promise);
        manager.pending_net.get_poll_voters.push((
            poll_id,
            0,
            String::new(),
            2,
            DialogId::from_user_id(UserId::new(7)),
        ));
        manager.on_get_poll_voters_answer(
            &mut env,
            poll_id,
            0,
            Ok(tdmtproto::telegram_api::messages_VotesList::default()),
        );
        assert_eq!(
            *outcomes.lock().unwrap(),
            vec![Err("400 Poll is anonymous".to_owned())]
        );
    }

    #[test]
    fn stop_poll_fold_send_and_finish_bands() {
        // the markup fold only runs for bots (get_inline_reply_markup
        // drops the markup for users — ReplyMarkup.cpp:319-322)
        let mut env = TestEnv {
            bot: true,
            ..TestEnv::default()
        };
        let mut manager = PollManager::new();
        let poll_id = env.known_poll;
        manager.polls.insert(poll_id, seed_poll(true));

        // the non-inline markup fails the fold at the C++
        // TRY_RESULT_PROMISE position (cpp:1723-1725)
        let (promise, outcomes) = recording_promise::<()>();
        manager.stop_poll(
            &mut env,
            band_message_full_id(),
            Some(td_api::ReplyMarkup::replyMarkupShowKeyboard(
                td_api::replyMarkupShowKeyboard::default(),
            )),
            promise,
        );
        assert!(
            matches!(&outcomes.lock().unwrap()[0], Err(message) if message == "400 Inline keyboard expected")
        );
        assert!(manager.pending_net.stop_poll.is_empty());

        // the finish bands want the non-bot posture (the
        // MESSAGE_NOT_MODIFIED Ok settle is the !is_bot band, cpp:324)
        env.bot = false;

        // the happy send parks the C++ lambda tuple and closes locally
        let (promise, outcomes) = recording_promise::<()>();
        manager.stop_poll(&mut env, band_message_full_id(), None, promise);
        assert_eq!(env.queries, vec!["stopPoll".to_owned()]);
        assert!(manager.being_closed_polls.contains(&poll_id));
        assert!(manager.get_poll(poll_id).unwrap().is_closed);
        assert!(outcomes.lock().unwrap().is_empty());

        // re-open the poll between the finish bands (the entry guard
        // rejects a second stop of a closed poll)
        let reopen = |manager: &mut PollManager| {
            manager.get_poll_editable(poll_id).unwrap().is_closed = false;
        };

        // the MESSAGE_NOT_MODIFIED band settles Ok through the finish
        // (the being_closed erase + the promise)
        reopen(&mut manager);
        manager.on_stop_poll_answer(
            &mut env,
            poll_id,
            Err(Status::error(400, "MESSAGE_NOT_MODIFIED")),
        );
        assert_eq!(*outcomes.lock().unwrap(), vec![Ok(())]);
        assert!(!manager.being_closed_polls.contains(&poll_id));

        // the error band runs the dialog-error head + the error finish
        let (promise, outcomes) = recording_promise::<()>();
        manager.stop_poll(&mut env, band_message_full_id(), None, promise);
        manager.on_stop_poll_answer(
            &mut env,
            poll_id,
            Err(Status::error(500, "CHAT_WRITE_FORBIDDEN")),
        );
        assert_eq!(
            env.dialog_errors,
            vec![(
                "StopPollQuery".to_owned(),
                "CHAT_WRITE_FORBIDDEN".to_owned()
            )]
        );
        assert_eq!(
            *outcomes.lock().unwrap(),
            vec![Err("500 CHAT_WRITE_FORBIDDEN".to_owned())]
        );

        // the happy answer hands the updates + the tuple back for the
        // engine tail; the finish then erases the log event and settles
        env.use_message_database = true;
        reopen(&mut manager);
        let (promise, outcomes) = recording_promise::<()>();
        manager.stop_poll(&mut env, band_message_full_id(), None, promise);
        let log_event_id = manager.pending_net.stop_poll[0].2;
        assert_ne!(log_event_id, 0);
        let (updates, tail_mfi, tail_log_event_id, tail_promise) = manager
            .on_stop_poll_answer(&mut env, poll_id, Ok(band_updates()))
            .expect("the Ok updates hand back");
        let _ = updates;
        assert_eq!(tail_mfi, band_message_full_id());
        assert_eq!(tail_log_event_id, log_event_id);
        manager.on_stop_poll_finished(
            &mut env,
            poll_id,
            tail_mfi,
            tail_log_event_id,
            Ok(()),
            tail_promise,
        );
        assert_eq!(*outcomes.lock().unwrap(), vec![Ok(())]);
        assert!(env.binlog_erases.contains(&log_event_id));
    }

    #[test]
    fn stop_poll_send_leg_band_routes_the_full_finish() {
        let mut env = TestEnv {
            peer: None,
            ..TestEnv::default()
        };
        let mut manager = PollManager::new();
        let poll_id = env.known_poll;
        manager.polls.insert(poll_id, seed_poll(false));

        let (promise, outcomes) = recording_promise::<()>();
        manager.stop_poll(&mut env, band_message_full_id(), None, promise);
        // the None-peer band runs the dialog-error head, then the finish
        // (the being_closed erase + the promise error)
        assert_eq!(
            env.dialog_errors,
            vec![(
                "StopPollQuery".to_owned(),
                "Can't access the chat".to_owned()
            )]
        );
        assert_eq!(
            *outcomes.lock().unwrap(),
            vec![Err("400 Can't access the chat".to_owned())]
        );
        assert!(!manager.being_closed_polls.contains(&poll_id));
        assert!(manager.pending_net.stop_poll.is_empty());
    }

    #[test]
    fn on_update_poll_timeout_guards_and_results_ladder() {
        let mut env = TestEnv::default();
        let mut manager = PollManager::new();
        let poll_id = env.known_poll;

        // no poll → no send
        manager.on_update_poll_timeout(&mut env, poll_id);
        assert!(env.queries.is_empty());

        // closed + updated-after-close → no send
        let mut closed = seed_poll(false);
        closed.is_closed = true;
        closed.is_updated_after_close = true;
        manager.polls.insert(poll_id, closed);
        manager.on_update_poll_timeout(&mut env, poll_id);
        assert!(env.queries.is_empty());

        // an open poll without server messages → no send
        manager.polls.insert(poll_id, seed_poll(false));
        manager.on_update_poll_timeout(&mut env, poll_id);
        assert!(env.queries.is_empty());

        // a pending vote skips the fetch (cpp:1853-1856)
        manager
            .server_poll_messages
            .entry(poll_id)
            .or_default()
            .insert(band_message_full_id());
        manager
            .pending_answers
            .insert(poll_id, crate::poll_manager::PendingPollAnswer::default());
        manager.on_update_poll_timeout(&mut env, poll_id);
        assert!(env.queries.is_empty());
        manager.pending_answers.remove(&poll_id);

        // the happy send parks the handler members keyed by the
        // generation
        manager.on_update_poll_timeout(&mut env, poll_id);
        assert_eq!(env.queries, vec!["getPollResults".to_owned()]);
        assert_eq!(manager.pending_net.get_poll_results.len(), 1);
        let generation = manager.pending_net.get_poll_results[0].1;

        // the error band runs the messages-error head, then re-arms the
        // update timeout with the polling timeout
        assert!(manager.update_poll_timeout.is_empty());
        let updates = manager.on_get_poll_results_answer(
            &mut env,
            generation,
            Err(Status::error(500, "TIMEOUT")),
        );
        assert!(updates.is_none());
        assert_eq!(env.message_errors.len(), 1);
        assert!(manager.update_poll_timeout.contains_key(&poll_id.get()));

        // the stale-generation band hands nothing back and re-arms at 0.0
        // (the re-parked lot entry pairs with the answer)
        manager.update_poll_timeout.clear();
        manager.current_generation += 1;
        manager.pending_net.get_poll_results.push((
            poll_id,
            generation,
            DialogId::from_user_id(UserId::new(7)),
            MessageId::new(1 << 20),
        ));
        let updates =
            manager.on_get_poll_results_answer(&mut env, generation, Ok(Some(band_updates())));
        assert!(updates.is_none());
        assert!(manager.update_poll_timeout.contains_key(&poll_id.get()));
        manager.update_poll_timeout.clear();

        // the current-generation answer hands the updates back
        let generation = manager.current_generation;
        manager.pending_net.get_poll_results.push((
            poll_id,
            generation,
            DialogId::from_user_id(UserId::new(7)),
            MessageId::new(1 << 20),
        ));
        let updates =
            manager.on_get_poll_results_answer(&mut env, generation, Ok(Some(band_updates())));
        assert!(updates.is_some());
        assert!(manager.pending_net.get_poll_results.is_empty());
    }

    #[test]
    fn on_update_poll_timeout_null_band_parks_nothing() {
        let mut env = TestEnv {
            peer: None,
            ..TestEnv::default()
        };
        let mut manager = PollManager::new();
        let poll_id = env.known_poll;
        manager.polls.insert(poll_id, seed_poll(false));
        manager
            .server_poll_messages
            .entry(poll_id)
            .or_default()
            .insert(band_message_full_id());

        // the Ok(null) send band: no query, no lot entry, no state touch
        manager.on_update_poll_timeout(&mut env, poll_id);
        assert!(env.queries.is_empty());
        assert!(manager.pending_net.get_poll_results.is_empty());
    }

    #[test]
    fn delete_poll_option_seam_send_and_answer_bands() {
        let mut env = TestEnv::default();
        let mut manager = PollManager::new();

        // the M.10 seam error settles the promise directly (the C++
        // TRY_STATUS_PROMISE)
        env.poll_not_found = true;
        let (promise, outcomes) = recording_promise::<()>();
        manager.delete_poll_option(&mut env, band_message_full_id(), "a".to_owned(), promise);
        assert_eq!(
            *outcomes.lock().unwrap(),
            vec![Err("400 Chat not found".to_owned())]
        );
        assert!(manager.pending_net.delete_poll_answer.is_empty());
        env.poll_not_found = false;

        // the send-leg band runs the dialog-error head + the promise error
        env.peer = None;
        let (promise, outcomes) = recording_promise::<()>();
        manager.delete_poll_option(&mut env, band_message_full_id(), "a".to_owned(), promise);
        assert_eq!(
            env.dialog_errors,
            vec![(
                "DeletePollAnswerQuery".to_owned(),
                "Can't access the chat".to_owned()
            )]
        );
        assert_eq!(
            *outcomes.lock().unwrap(),
            vec![Err("400 Can't access the chat".to_owned())]
        );
        env.peer = Some(tdmtproto::telegram_api::InputPeer::inputPeerSelf(
            tdmtproto::telegram_api::inputPeerSelf {},
        ));

        // the happy send parks the promise FIFO
        let (promise, outcomes) = recording_promise::<()>();
        manager.delete_poll_option(&mut env, band_message_full_id(), "a".to_owned(), promise);
        assert_eq!(env.queries, vec!["deletePollAnswer:a".to_owned()]);
        assert_eq!(manager.pending_net.delete_poll_answer.len(), 1);
        assert!(outcomes.lock().unwrap().is_empty());

        // the Ok answer hands the updates + the popped promise back (the
        // router feeds the engine with them)
        let (updates, tail_promise) = manager
            .on_delete_poll_answer_answer(&mut env, Ok(band_updates()))
            .expect("the Ok updates hand back");
        let _ = updates;
        drop(tail_promise);
        assert!(manager.pending_net.delete_poll_answer.is_empty());

        // the error band settles the popped promise with the head
        let (promise, outcomes) = recording_promise::<()>();
        manager.delete_poll_option(&mut env, band_message_full_id(), "b".to_owned(), promise);
        manager.on_delete_poll_answer_answer(&mut env, Err(Status::error(400, "OPTION_INVALID")));
        assert_eq!(
            env.dialog_errors,
            vec![
                (
                    "DeletePollAnswerQuery".to_owned(),
                    "Can't access the chat".to_owned()
                ),
                (
                    "DeletePollAnswerQuery".to_owned(),
                    "OPTION_INVALID".to_owned()
                )
            ]
        );
        assert_eq!(
            *outcomes.lock().unwrap(),
            vec![Err("400 OPTION_INVALID".to_owned())]
        );
    }
}
