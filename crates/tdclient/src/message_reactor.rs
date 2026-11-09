//! Port of `td/telegram/MessageReactor.{h,cpp,hpp}` (M.16.2): the paid
//! reactor rows of a message, plus the `MinChannel` store/parse
//! (MinChannel.h/hpp — the struct itself is already on master in
//! chat_manager.rs, the M.5.21 groundwork) and the `DialogPhoto`
//! store/parse slice MinChannel needs (DialogPhoto.hpp, over the
//! [`crate::photo::PhotoStoreEnv`] file-id seams).
//!
//! The Td*-carrying object builder (`get_paid_reactor_object`) and
//! `add_dependencies` ride the message-sender/Dependencies planes and land
//! at the M.10 integration; every local mutation, the wire constructor,
//! `add_min_channel`, `fix_message_reactors` and the db record are here.

use crate::chat_manager::MinChannel;
use crate::dialog_id::DialogId;
use crate::paid_reaction_type::PaidReactionType;
use crate::store_parse::{Parser, store_flags, store_i32};
use tdmtproto::telegram_api;

/// The `Td *td`/`storer.context()->td()` slice the ported methods read:
/// the dialog/user info probes of the wire intake, the min-channel cache
/// of `add_min_channel`, and the FileManager FileId codec of the db
/// record (`store_file`/`parse_file`, the M.40 icon-plane encoding).
pub trait MessageReactorEnv: crate::photo::PhotoStoreEnv {
    /// `dialog_manager_->have_dialog_info`.
    fn have_dialog_info(&self, dialog_id: DialogId) -> bool;
    /// `chat_manager_->get_min_channel(channel_id)` — cloned, the C++
    /// copies the struct into the reactor.
    fn get_min_channel(&self, channel_id: crate::chat_manager::ChannelId) -> Option<MinChannel>;
    /// `chat_manager_->add_min_channel(channel_id, min_channel)`.
    fn add_min_channel(
        &mut self,
        channel_id: crate::chat_manager::ChannelId,
        min_channel: &MinChannel,
    );
}

/// `MessageReactor` (MessageReactor.h).
#[derive(Clone, Debug, Default)]
pub struct MessageReactor {
    dialog_id: DialogId, // self for anonymous reactions by the current user
    min_channel: Option<Box<MinChannel>>,
    count: i32,
    is_top: bool,
    is_me: bool,
    is_anonymous: bool,
}

impl MessageReactor {
    /// The plain constructor (MessageReactor.h:57-61).
    pub fn new(dialog_id: DialogId, count: i32, is_me: bool, is_anonymous: bool) -> MessageReactor {
        MessageReactor {
            dialog_id,
            min_channel: None,
            count,
            is_top: false,
            is_me,
            is_anonymous,
        }
    }

    /// `store_min_channel` (MessageReactor.cpp:24-34) — an unknown
    /// reacted channel snapshots its min-channel record.
    fn store_min_channel(&mut self, env: &mut impl MessageReactorEnv) {
        if dialog_type_is_channel(self.dialog_id) && !env.have_dialog_info(self.dialog_id) {
            let channel_id = self.dialog_id.get_channel_id();
            match env.get_min_channel(channel_id) {
                None => {
                    tdutils::log_error!("Receive unknown reacted channel {}", channel_id.get());
                }
                Some(min_channel) => {
                    self.min_channel = Some(Box::new(min_channel));
                }
            }
        }
    }

    /// `MessageReactor(Td *, telegram_api::object_ptr<messageReactor>)`
    /// (MessageReactor.cpp:36-43). The generated `peer_id` cannot be
    /// null; the empty-peer default decodes as the invalid DialogId like
    /// the C++ null check.
    pub fn from_telegram(
        env: &mut impl MessageReactorEnv,
        reactor: &telegram_api::MessageReactor,
    ) -> MessageReactor {
        let mut result = MessageReactor {
            dialog_id: reactor
                .peer_id
                .as_ref()
                .map(DialogId::from_peer)
                .unwrap_or_default(),
            min_channel: None,
            count: reactor.count,
            is_top: reactor.top,
            is_me: reactor.my,
            is_anonymous: reactor.anonymous,
        };
        result.store_min_channel(env);
        result
    }

    /// `is_valid` (MessageReactor.h:64-66).
    pub fn is_valid(&self) -> bool {
        self.count > 0
            && if self.is_me {
                self.dialog_id.is_valid()
            } else {
                (self.dialog_id.is_valid() || self.is_anonymous) && self.is_top
            }
    }

    /// `is_me` (MessageReactor.h:68-70).
    pub fn is_me(&self) -> bool {
        self.is_me
    }

    /// `is_same` (MessageReactor.h:72-74).
    pub fn is_same(&self, dialog_id: DialogId) -> bool {
        self.dialog_id == dialog_id
    }

    /// `is_anonymous` (MessageReactor.h:76-78).
    pub fn is_anonymous(&self) -> bool {
        self.is_anonymous
    }

    pub fn is_top(&self) -> bool {
        self.is_top
    }

    pub fn get_count(&self) -> i32 {
        self.count
    }

    pub fn get_dialog_id(&self) -> DialogId {
        self.dialog_id
    }

    /// `get_paid_reaction_type` (MessageReactor.cpp:71-79).
    pub fn get_paid_reaction_type(&self, my_dialog_id: DialogId) -> PaidReactionType {
        if self.is_anonymous || !self.dialog_id.is_valid() {
            return PaidReactionType::legacy(true);
        }
        if self.dialog_id == my_dialog_id {
            return PaidReactionType::legacy(false);
        }
        PaidReactionType::dialog(self.dialog_id)
    }

    /// `fix_is_me` (MessageReactor.cpp:81-87).
    pub fn fix_is_me(&mut self, my_dialog_id: DialogId) -> bool {
        if self.dialog_id == my_dialog_id {
            self.is_me = true;
            return true;
        }
        false
    }

    /// `add_count` (MessageReactor.h:80-91).
    pub fn add_count(&mut self, count: i32, reactor_dialog_id: DialogId, my_dialog_id: DialogId) {
        self.count += count;
        if reactor_dialog_id == DialogId::new(0) {
            self.dialog_id = my_dialog_id;
            self.is_anonymous = true;
        } else {
            self.dialog_id = reactor_dialog_id;
            self.is_anonymous = false;
        }
    }

    /// `remove_count` (MessageReactor.h:93-96).
    pub fn remove_count(&mut self, count: i32) {
        self.count = std::cmp::max(self.count - count, 0);
    }

    /// `add_min_channel` (MessageReactor.cpp:61-65).
    pub fn add_min_channel_to_chat_manager(&self, env: &mut impl MessageReactorEnv) {
        if let (Some(min_channel), true) =
            (&self.min_channel, dialog_type_is_channel(self.dialog_id))
        {
            env.add_min_channel(self.dialog_id.get_channel_id(), min_channel);
        }
    }

    /// `fix_message_reactors` (MessageReactor.cpp:89-134) — the clamp to
    /// the top rows + self, the validity filter and the re-sort.
    pub fn fix_message_reactors(
        reactors: &mut Vec<MessageReactor>,
        need_warning: bool,
        for_group_call: bool,
    ) {
        let top_reactor_count: usize = if for_group_call { 100 } else { 3 };
        if reactors.len() > top_reactor_count + 1 {
            tdutils::log_error!("Have too many reactors");
            reactors.truncate(top_reactor_count + 1);
        }
        if reactors.len() > top_reactor_count && !reactors[top_reactor_count].is_me() {
            tdutils::log_error!("Receive unexpected reactors");
            reactors.truncate(top_reactor_count);
        }
        if need_warning {
            for (i, reactor) in reactors.iter().enumerate() {
                if reactor.is_top != (i < top_reactor_count) {
                    tdutils::log_error!("Receive incorrect top reactors");
                    break;
                }
            }
            for i in 0..reactors.len().saturating_sub(1) {
                if reactors[i].count < reactors[i + 1].count {
                    tdutils::log_error!("Receive unordered reactors");
                    break;
                }
            }
        }
        // `td::remove_if` — the invalid rows and the duplicate selves
        let mut was_me = false;
        reactors.retain(|reactor| {
            if !reactor.is_valid() {
                return false;
            }
            if reactor.is_me() {
                if was_me {
                    tdutils::log_error!("Receive self multiple times");
                    return false;
                }
                was_me = true;
            }
            true
        });
        reactors.sort();
        if reactors.len() > top_reactor_count && !reactors[top_reactor_count].is_me() {
            reactors.truncate(top_reactor_count);
        }
        for (i, reactor) in reactors.iter_mut().enumerate() {
            reactor.is_top = i < top_reactor_count;
        }
    }

    /// `MessageReactor::store` (MessageReactor.hpp).
    pub fn store(&self, env: &mut impl crate::photo::PhotoStoreEnv, out: &mut Vec<u8>) {
        let has_dialog_id = self.dialog_id.is_valid();
        let has_min_channel = self.min_channel.is_some();
        store_flags(
            out,
            [
                self.is_top,
                self.is_me,
                self.is_anonymous,
                has_dialog_id,
                has_min_channel,
            ],
        );
        if has_dialog_id {
            self.dialog_id.store(out);
        }
        store_i32(out, self.count);
        if let Some(min_channel) = &self.min_channel {
            store_min_channel(min_channel, env, out);
        }
    }

    /// `MessageReactor::parse` (MessageReactor.hpp).
    pub fn parse(
        env: &mut impl crate::photo::PhotoStoreEnv,
        parser: &mut Parser<'_>,
    ) -> Result<MessageReactor, &'static str> {
        let mut result = MessageReactor::default();
        let [is_top, is_me, is_anonymous, has_dialog_id, has_min_channel] = parser.parse_flags()?;
        result.is_top = is_top;
        result.is_me = is_me;
        result.is_anonymous = is_anonymous;
        if has_dialog_id {
            result.dialog_id = DialogId::parse_from(parser)?;
        }
        result.count = parser.i32_()?;
        if has_min_channel {
            result.min_channel = Some(Box::new(parse_min_channel(env, parser)?));
        }
        Ok(result)
    }
}

/// `dialog_id.get_type() == DialogType::Channel` — the local probe the
/// C++ spreads over two calls.
fn dialog_type_is_channel(dialog_id: DialogId) -> bool {
    use crate::dialog_id::DialogType;
    dialog_id.get_type() == DialogType::Channel
}

/// `operator<` (MessageReactor.cpp:136-141).
impl Ord for MessageReactor {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        if self.count != other.count {
            return other.count.cmp(&self.count); // the higher count first
        }
        self.dialog_id.get().cmp(&other.dialog_id.get())
    }
}

impl PartialOrd for MessageReactor {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// `operator==` (MessageReactor.cpp:143-146) — 7c9c98cd2 added
/// `min_channel_` to the comparison.
impl PartialEq for MessageReactor {
    fn eq(&self, other: &Self) -> bool {
        self.dialog_id == other.dialog_id
            && self.min_channel == other.min_channel
            && self.count == other.count
            && self.is_top == other.is_top
            && self.is_me == other.is_me
            && self.is_anonymous == other.is_anonymous
    }
}

impl Eq for MessageReactor {}

impl std::fmt::Display for MessageReactor {
    /// `operator<<` (MessageReactor.cpp:148-151).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "PaidReactor[{} - {}{}]",
            self.dialog_id.get(),
            self.count,
            if self.is_me { " by me" } else { "" }
        )
    }
}

/// `store(const MinChannel &, StorerT &)` (MinChannel.hpp:8-32).
pub fn store_min_channel(
    min_channel: &MinChannel,
    env: &mut impl crate::photo::PhotoStoreEnv,
    out: &mut Vec<u8>,
) {
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
        crate::store_parse::store_string(out, &min_channel.title_);
    }
    if has_photo {
        crate::photo::store_dialog_photo(env, out, &min_channel.photo_);
    }
    if has_accent_color_id {
        crate::store_parse::store_i32(out, min_channel.accent_color_id_.get());
    }
}

/// `parse(MinChannel &, ParserT &)` (MinChannel.hpp:34-57).
pub fn parse_min_channel(
    env: &mut impl crate::photo::PhotoStoreEnv,
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
