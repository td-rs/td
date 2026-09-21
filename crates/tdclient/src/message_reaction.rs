//! Port of `td/telegram/MessageReaction.{h,cpp,hpp}` (M.16.2): the
//! per-message reaction set — the aggregated `MessageReaction` rows with
//! their recent choosers, the unread-reaction list, the chosen-reaction
//! order and the pending paid-reaction machine — plus the
//! `check_min_message_sender` helper (MessageSender.cpp:42-71) the wire
//! intake uses. The byte format is the MessageReaction.hpp blocks; the
//! pmc blobs stay readable by both implementations.
//!
//! Deferred to the M.10 integration (the C++ bodies read
//! `td_->messages_manager_`/message senders): `get_message_reactions`
//! (the wire intake), the object builders (`get_message_reaction(s)_object`,
//! `get_unread_reaction_object`, `get_paid_reactor_object`),
//! `add_min_channels`/`add_dependencies`, `send_paid_message_reaction` and
//! `set_paid_message_reaction_type` (their net tails are
//! `[blocked: net]`). Everything locally observable — the mutation
//! machine, the consistency checks and the db record — is here.

use crate::chat_manager::{ChannelId, MinChannel};
use crate::dialog_id::DialogId;
use crate::message_reactor::MessageReactor;
use crate::paid_reaction_type::PaidReactionType;
use crate::reaction_type::ReactionType;
use crate::store_parse::{Parser, store_flags, store_i32};
use crate::td_api;
use std::collections::HashMap;

/// `MessageReaction::MAX_CHOOSE_COUNT` (MessageReaction.h:34).
pub const MAX_CHOOSE_COUNT: i32 = 2147483640;

/// `MessageReaction::MAX_RECENT_CHOOSERS` (MessageReaction.h:36).
pub const MAX_RECENT_CHOOSERS: usize = 3;

/// The `Td *td` slice the local mutation machine reads — the premium
/// options of `get_max_reaction_count`.
pub trait MessageReactionEnv {
    fn get_option_boolean(&self, name: &str, default_value: bool) -> bool;
    fn get_option_integer(&self, name: &str, default_value: i64) -> i64;
}

/// The extra surfaces of the paid-reaction pending machine:
/// StarManager's pending-count ledger and ReactionManager's default type.
pub trait MessageReactionPaidEnv:
    MessageReactionEnv + crate::paid_reaction_type::PaidReactionTypeEnv
{
    /// `dialog_manager_->get_my_dialog_id`.
    fn get_my_dialog_id(&self) -> DialogId;
    /// `star_manager_->add_pending_owned_star_count(count, is_final)`.
    fn add_pending_owned_star_count(&mut self, count: i32, is_final: bool);
    /// `reaction_manager_->get_default_paid_reaction_type()`.
    fn get_default_paid_reaction_type(&self) -> PaidReactionType;
    /// `reaction_manager_->on_update_default_paid_reaction_type(type)`.
    fn on_update_default_paid_reaction_type(&mut self, type_: &PaidReactionType);
}

/// `get_max_reaction_count` (MessageReaction.cpp:38-43).
fn get_max_reaction_count(env: &impl MessageReactionEnv) -> usize {
    let is_premium = env.get_option_boolean("is_premium", false);
    let default_value: i64 = if is_premium { 3 } else { 1 };
    let count = std::cmp::max(
        1,
        env.get_option_integer(
            if is_premium {
                "reactions_user_max_premium"
            } else {
                "reactions_user_max_default"
            },
            default_value,
        ) as i32,
    );
    count as usize
}

/// `td::add_to_top` (tdutils/td/utils/algorithm.h:115-137) — move the
/// value to the front, growing up to `max_size` and dropping the last
/// element beyond it.
fn add_to_top(v: &mut Vec<DialogId>, max_size: usize, value: DialogId) {
    if let Some(pos) = v.iter().position(|&x| x == value) {
        v.remove(pos);
    } else if v.len() >= max_size && !v.is_empty() {
        v.pop();
    }
    v.insert(0, value);
}

/// `td::remove` — the first-occurrence removal flag.
fn remove_first<T: PartialEq>(v: &mut Vec<T>, value: &T) -> bool {
    match v.iter().position(|x| x == value) {
        Some(index) => {
            v.remove(index);
            true
        }
        None => false,
    }
}

/// `MessageReaction` (MessageReaction.h).
#[derive(Clone, Debug)]
pub struct MessageReaction {
    reaction_type: ReactionType,
    choose_count: i32,
    is_chosen: bool,
    my_recent_chooser_dialog_id: DialogId,
    recent_chooser_dialog_ids: Vec<DialogId>,
    recent_chooser_min_channels: Vec<(ChannelId, MinChannel)>,
}

impl MessageReaction {
    /// The (private in the C++) full constructor
    /// (MessageReaction.cpp:343-356) — the C++ CHECKs that a valid
    /// `my_recent_chooser_dialog_id` is present among the choosers.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        reaction_type: ReactionType,
        choose_count: i32,
        is_chosen: bool,
        my_recent_chooser_dialog_id: DialogId,
        recent_chooser_dialog_ids: Vec<DialogId>,
        recent_chooser_min_channels: Vec<(ChannelId, MinChannel)>,
    ) -> MessageReaction {
        let mut result = MessageReaction {
            reaction_type,
            choose_count,
            is_chosen,
            my_recent_chooser_dialog_id,
            recent_chooser_dialog_ids,
            recent_chooser_min_channels,
        };
        debug_assert!(
            !result.my_recent_chooser_dialog_id.is_valid()
                || result
                    .recent_chooser_dialog_ids
                    .contains(&result.my_recent_chooser_dialog_id),
            "the C++ CHECKs the my chooser membership"
        );
        result.fix_choose_count();
        result
    }

    /// The paid-reaction row the pending insertions build
    /// (MessageReaction.cpp:973/1059 — `MessageReaction(paid, count, true,
    /// DialogId(), Auto(), Auto())`).
    pub fn new_paid(star_count: i32) -> MessageReaction {
        MessageReaction::new(
            ReactionType::paid(),
            star_count,
            true,
            DialogId::new(0),
            Vec::new(),
            Vec::new(),
        )
    }

    pub fn get_reaction_type(&self) -> &ReactionType {
        &self.reaction_type
    }

    pub fn get_choose_count(&self) -> i32 {
        self.choose_count
    }

    pub fn is_chosen(&self) -> bool {
        self.is_chosen
    }

    pub fn is_empty(&self) -> bool {
        self.choose_count <= 0
    }

    pub fn get_my_recent_chooser_dialog_id(&self) -> DialogId {
        self.my_recent_chooser_dialog_id
    }

    pub fn get_recent_chooser_dialog_ids(&self) -> &[DialogId] {
        &self.recent_chooser_dialog_ids
    }

    pub fn get_recent_chooser_min_channels(&self) -> &[(ChannelId, MinChannel)] {
        &self.recent_chooser_min_channels
    }

    /// `add_my_recent_chooser_dialog_id` (MessageReaction.cpp:358-363).
    pub fn add_my_recent_chooser_dialog_id(&mut self, dialog_id: DialogId) {
        debug_assert!(
            !self.my_recent_chooser_dialog_id.is_valid(),
            "the C++ CHECKs the chooser was unset"
        );
        self.my_recent_chooser_dialog_id = dialog_id;
        add_to_top(
            &mut self.recent_chooser_dialog_ids,
            MAX_RECENT_CHOOSERS + 1,
            dialog_id,
        );
        self.fix_choose_count();
    }

    /// `remove_my_recent_chooser_dialog_id` (MessageReaction.cpp:365-373).
    pub fn remove_my_recent_chooser_dialog_id(&mut self) -> bool {
        if self.my_recent_chooser_dialog_id.is_valid() {
            let is_removed = remove_first(
                &mut self.recent_chooser_dialog_ids,
                &self.my_recent_chooser_dialog_id,
            );
            debug_assert!(is_removed, "the C++ CHECKs the removal");
            self.my_recent_chooser_dialog_id = DialogId::new(0);
            return true;
        }
        false
    }

    /// `update_from` (MessageReaction.cpp:375-383).
    pub fn update_from(&mut self, old_reaction: &MessageReaction) {
        debug_assert!(old_reaction.is_chosen(), "the C++ CHECKs is_chosen");
        self.is_chosen = true;

        let my_dialog_id = old_reaction.get_my_recent_chooser_dialog_id();
        if my_dialog_id.is_valid() && self.recent_chooser_dialog_ids.contains(&my_dialog_id) {
            self.my_recent_chooser_dialog_id = my_dialog_id;
        }
    }

    /// `update_recent_chooser_dialog_ids` (MessageReaction.cpp:385-401).
    pub fn update_recent_chooser_dialog_ids(&mut self, old_reaction: &MessageReaction) {
        if self.recent_chooser_dialog_ids.len() != MAX_RECENT_CHOOSERS {
            return;
        }
        debug_assert!(
            self.is_chosen
                && old_reaction.is_chosen
                && self.reaction_type == old_reaction.reaction_type,
            "the C++ CHECKs the chosen-state/type pair"
        );
        debug_assert_eq!(
            old_reaction.recent_chooser_dialog_ids.len(),
            MAX_RECENT_CHOOSERS + 1,
            "the C++ CHECKs the old chooser count"
        );
        for i in 0..MAX_RECENT_CHOOSERS {
            if self.recent_chooser_dialog_ids[i] != old_reaction.recent_chooser_dialog_ids[i] {
                return;
            }
        }
        self.my_recent_chooser_dialog_id = old_reaction.my_recent_chooser_dialog_id;
        self.recent_chooser_dialog_ids = old_reaction.recent_chooser_dialog_ids.clone();
        self.recent_chooser_min_channels = old_reaction.recent_chooser_min_channels.clone();
        self.fix_choose_count();
    }

    /// `set_as_chosen` (MessageReaction.cpp:403-412).
    pub fn set_as_chosen(&mut self, my_dialog_id: DialogId, have_recent_choosers: bool) {
        debug_assert!(!self.is_chosen, "the C++ CHECKs !is_chosen");

        self.is_chosen = true;
        self.choose_count += 1;
        if have_recent_choosers {
            self.remove_my_recent_chooser_dialog_id();
            self.add_my_recent_chooser_dialog_id(my_dialog_id);
        }
    }

    /// `unset_as_chosen` (MessageReaction.cpp:414-421).
    pub fn unset_as_chosen(&mut self) {
        debug_assert!(self.is_chosen, "the C++ CHECKs is_chosen");

        self.is_chosen = false;
        self.choose_count -= 1;
        self.remove_my_recent_chooser_dialog_id();
        self.fix_choose_count();
    }

    /// `add_paid_reaction` (MessageReaction.cpp:423-427).
    pub fn add_paid_reaction(&mut self, star_count: i32) {
        self.is_chosen = true;
        debug_assert!(
            star_count <= i32::MAX - self.choose_count,
            "the C++ CHECKs the count headroom"
        );
        self.choose_count += star_count;
    }

    /// `fix_choose_count` (MessageReaction.cpp:429-431).
    pub fn fix_choose_count(&mut self) {
        self.choose_count = std::cmp::max(
            self.choose_count,
            self.recent_chooser_dialog_ids.len() as i32,
        );
    }

    /// `set_my_recent_chooser_dialog_id` (MessageReaction.cpp:433-445).
    pub fn set_my_recent_chooser_dialog_id(&mut self, my_dialog_id: DialogId) {
        if !self.my_recent_chooser_dialog_id.is_valid()
            || self.my_recent_chooser_dialog_id == my_dialog_id
        {
            return;
        }
        remove_first(&mut self.recent_chooser_dialog_ids, &my_dialog_id);
        for dialog_id in &mut self.recent_chooser_dialog_ids {
            if *dialog_id == self.my_recent_chooser_dialog_id {
                *dialog_id = my_dialog_id;
            }
        }
        debug_assert!(
            self.recent_chooser_dialog_ids.contains(&my_dialog_id),
            "the C++ CHECKs the membership"
        );
        self.my_recent_chooser_dialog_id = my_dialog_id;
    }

    /// `MessageReaction::store` (MessageReaction.hpp:21-44).
    pub fn store(&self, env: &mut impl crate::photo::DialogPhotoStoreEnv, out: &mut Vec<u8>) {
        debug_assert!(!self.is_empty(), "the C++ store CHECKs !is_empty");
        let has_recent_chooser_dialog_ids = !self.recent_chooser_dialog_ids.is_empty();
        let has_recent_chooser_min_channels = !self.recent_chooser_min_channels.is_empty();
        let has_my_recent_chooser_dialog_id = self.my_recent_chooser_dialog_id.is_valid();
        store_flags(
            out,
            [
                self.is_chosen,
                has_recent_chooser_dialog_ids,
                has_recent_chooser_min_channels,
                has_my_recent_chooser_dialog_id,
            ],
        );
        self.reaction_type.store(out);
        store_i32(out, self.choose_count);
        if has_recent_chooser_dialog_ids {
            store_i32(out, self.recent_chooser_dialog_ids.len() as i32);
            for dialog_id in &self.recent_chooser_dialog_ids {
                dialog_id.store(out);
            }
        }
        if has_recent_chooser_min_channels {
            store_i32(out, self.recent_chooser_min_channels.len() as i32);
            for (channel_id, min_channel) in &self.recent_chooser_min_channels {
                // ChannelId::store is store_long (ChannelId.h:56-58)
                crate::store_parse::store_i64(out, channel_id.get());
                crate::message_reactor::store_min_channel(min_channel, env, out);
            }
        }
        if has_my_recent_chooser_dialog_id {
            self.my_recent_chooser_dialog_id.store(out);
        }
    }

    /// `MessageReaction::parse` (MessageReaction.hpp:46-77).
    pub fn parse(
        env: &mut impl crate::photo::DialogPhotoStoreEnv,
        parser: &mut Parser<'_>,
    ) -> Result<MessageReaction, &'static str> {
        let mut result = MessageReaction::default();
        let [
            is_chosen,
            has_recent_chooser_dialog_ids,
            has_recent_chooser_min_channels,
            has_my_recent_chooser_dialog_id,
        ] = parser.parse_flags()?;
        result.is_chosen = is_chosen;
        result.reaction_type = ReactionType::parse(parser)?;
        result.choose_count = parser.i32_()?;
        if has_recent_chooser_dialog_ids {
            let len = parser.i32_()?;
            if len < 0 {
                return Err("Invalid vector size");
            }
            result.recent_chooser_dialog_ids.reserve(len as usize);
            for _ in 0..len {
                result
                    .recent_chooser_dialog_ids
                    .push(DialogId::parse_from(parser)?);
            }
        }
        if has_recent_chooser_min_channels {
            let len = parser.i32_()?;
            if len < 0 {
                return Err("Invalid vector size");
            }
            result.recent_chooser_min_channels.reserve(len as usize);
            for _ in 0..len {
                let channel_id = ChannelId::new(parser.i64_()?);
                let min_channel = crate::message_reactor::parse_min_channel(env, parser)?;
                result
                    .recent_chooser_min_channels
                    .push((channel_id, min_channel));
            }
        }
        if has_my_recent_chooser_dialog_id {
            result.my_recent_chooser_dialog_id = DialogId::parse_from(parser)?;
            if !result.my_recent_chooser_dialog_id.is_valid()
                || !result
                    .recent_chooser_dialog_ids
                    .contains(&result.my_recent_chooser_dialog_id)
            {
                return Err("Invalid recent reaction chooser");
            }
        }
        result.fix_choose_count();

        if result.is_empty() || result.reaction_type.is_empty() {
            return Err("Invalid message reaction");
        }
        Ok(result)
    }
}

impl Default for MessageReaction {
    fn default() -> MessageReaction {
        MessageReaction {
            reaction_type: ReactionType::default(),
            choose_count: 0,
            is_chosen: false,
            my_recent_chooser_dialog_id: DialogId::new(0),
            recent_chooser_dialog_ids: Vec::new(),
            recent_chooser_min_channels: Vec::new(),
        }
    }
}

/// `operator==` (MessageReaction.cpp:486-490) — `min_channels` is
/// deliberately not compared.
impl PartialEq for MessageReaction {
    fn eq(&self, other: &Self) -> bool {
        self.reaction_type == other.reaction_type
            && self.choose_count == other.choose_count
            && self.is_chosen == other.is_chosen
            && self.my_recent_chooser_dialog_id == other.my_recent_chooser_dialog_id
            && self.recent_chooser_dialog_ids == other.recent_chooser_dialog_ids
    }
}

impl std::fmt::Display for MessageReaction {
    /// `operator<<` (MessageReaction.cpp:492-501).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[{} {} {}",
            self.reaction_type,
            if self.is_chosen { "X" } else { "x" },
            self.choose_count
        )?;
        if !self.recent_chooser_dialog_ids.is_empty() {
            write!(f, " by {:?}", self.recent_chooser_dialog_ids)?;
            if self.my_recent_chooser_dialog_id.is_valid() {
                write!(f, " and my {}", self.my_recent_chooser_dialog_id.get())?;
            }
        }
        write!(f, "]")
    }
}

/// `UnreadMessageReaction` (MessageReaction.h).
#[derive(Clone, Debug)]
pub struct UnreadMessageReaction {
    reaction_type: ReactionType,
    sender_dialog_id: DialogId,
    is_big: bool,
}

impl UnreadMessageReaction {
    pub fn new(
        reaction_type: ReactionType,
        sender_dialog_id: DialogId,
        is_big: bool,
    ) -> UnreadMessageReaction {
        UnreadMessageReaction {
            reaction_type,
            sender_dialog_id,
            is_big,
        }
    }

    pub fn get_reaction_type(&self) -> &ReactionType {
        &self.reaction_type
    }

    pub fn get_sender_dialog_id(&self) -> DialogId {
        self.sender_dialog_id
    }

    pub fn is_big(&self) -> bool {
        self.is_big
    }

    /// `UnreadMessageReaction::store` (MessageReaction.hpp:79-86).
    pub fn store(&self, out: &mut Vec<u8>) {
        store_flags(out, [self.is_big]);
        self.reaction_type.store(out);
        self.sender_dialog_id.store(out);
    }

    /// `UnreadMessageReaction::parse` (MessageReaction.hpp:88-98).
    pub fn parse(parser: &mut Parser<'_>) -> Result<UnreadMessageReaction, &'static str> {
        let mut result = UnreadMessageReaction {
            reaction_type: ReactionType::default(),
            sender_dialog_id: DialogId::new(0),
            is_big: false,
        };
        let [is_big] = parser.parse_flags()?;
        result.is_big = is_big;
        result.reaction_type = ReactionType::parse(parser)?;
        result.sender_dialog_id = DialogId::parse_from(parser)?;
        if result.reaction_type.is_empty() {
            return Err("Invalid unread message reaction");
        }
        Ok(result)
    }
}

impl PartialEq for UnreadMessageReaction {
    /// `operator==` (MessageReaction.cpp:512-515).
    fn eq(&self, other: &Self) -> bool {
        self.reaction_type == other.reaction_type
            && self.sender_dialog_id == other.sender_dialog_id
            && self.is_big == other.is_big
    }
}

impl std::fmt::Display for UnreadMessageReaction {
    /// `operator<<` (MessageReaction.cpp:517-520).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[{} {} {}]",
            self.reaction_type,
            if self.is_big { "BY" } else { "by" },
            self.sender_dialog_id.get()
        )
    }
}

/// `MessageReactions` (MessageReaction.h).
#[derive(Clone, Debug)]
pub struct MessageReactions {
    pub(crate) reactions: Vec<MessageReaction>,
    pub(crate) unread_reactions: Vec<UnreadMessageReaction>,
    pub(crate) chosen_reaction_order: Vec<ReactionType>,
    pub(crate) top_reactors: Vec<MessageReactor>,
    pub(crate) pending_paid_reactions: i32,
    pub(crate) pending_paid_reaction_type: PaidReactionType,
    pub(crate) pending_use_default_paid_reaction_type: bool,
    pub(crate) is_min: bool,
    pub(crate) need_polling: bool,
    pub(crate) can_get_added_reactions: bool,
    pub(crate) are_tags: bool,
}

impl Default for MessageReactions {
    /// `MessageReactions() = default` with `need_polling_ = true`
    /// (MessageReaction.h:164).
    fn default() -> MessageReactions {
        MessageReactions {
            reactions: Vec::new(),
            unread_reactions: Vec::new(),
            chosen_reaction_order: Vec::new(),
            top_reactors: Vec::new(),
            pending_paid_reactions: 0,
            pending_paid_reaction_type: PaidReactionType::default(),
            pending_use_default_paid_reaction_type: false,
            is_min: false,
            need_polling: true,
            can_get_added_reactions: false,
            are_tags: false,
        }
    }
}

impl MessageReactions {
    /// `are_empty` (MessageReaction.cpp:522-524).
    pub fn are_empty(&self) -> bool {
        self.reactions.is_empty() && self.pending_paid_reactions == 0
    }

    /// `get_reaction` (MessageReaction.cpp:642-649).
    pub fn get_reaction(&self, reaction_type: &ReactionType) -> Option<&MessageReaction> {
        self.reactions
            .iter()
            .find(|added_reaction| added_reaction.get_reaction_type() == reaction_type)
    }

    /// The mutable overload (MessageReaction.cpp:633-640).
    pub fn get_reaction_mut(
        &mut self,
        reaction_type: &ReactionType,
    ) -> Option<&mut MessageReaction> {
        self.reactions
            .iter_mut()
            .find(|added_reaction| added_reaction.get_reaction_type() == reaction_type)
    }

    /// `update_from` (MessageReaction.cpp:651-700).
    pub fn update_from(&mut self, old_reactions: &MessageReactions, my_dialog_id: DialogId) {
        if self.is_min && !old_reactions.is_min {
            // chosen reactions were known, keep them
            self.is_min = false;
            self.chosen_reaction_order = old_reactions.chosen_reaction_order.clone();
            for old_reaction in &old_reactions.reactions {
                if old_reaction.is_chosen() {
                    if let Some(reaction) = self.get_reaction_mut(old_reaction.get_reaction_type())
                    {
                        reaction.update_from(old_reaction);
                    }
                } else {
                    remove_first(
                        &mut self.chosen_reaction_order,
                        old_reaction.get_reaction_type(),
                    );
                }
            }
            self.unread_reactions = old_reactions.unread_reactions.clone();
            if self.chosen_reaction_order.len() == 1 {
                // `reset_to_empty` — a single-entry order is implicit
                self.chosen_reaction_order.clear();
            }

            let mut was_me = false;
            for reactor in &mut self.top_reactors {
                if reactor.fix_is_me(my_dialog_id) {
                    was_me = true;
                    break;
                }
            }
            if !was_me {
                for reactor in &old_reactions.top_reactors {
                    if reactor.is_me() {
                        // self paid reaction was known, keep it
                        self.top_reactors.push(reactor.clone());
                        MessageReactor::fix_message_reactors(&mut self.top_reactors, false, false);
                    }
                }
            }
        }
        for old_reaction in &old_reactions.reactions {
            if old_reaction.is_chosen()
                && old_reaction.get_recent_chooser_dialog_ids().len() == MAX_RECENT_CHOOSERS + 1
                && self
                    .get_reaction(old_reaction.get_reaction_type())
                    .is_some_and(MessageReaction::is_chosen)
            {
                self.get_reaction_mut(old_reaction.get_reaction_type())
                    .unwrap()
                    .update_recent_chooser_dialog_ids(old_reaction);
            }
        }
        self.pending_paid_reactions = old_reactions.pending_paid_reactions;
        self.pending_use_default_paid_reaction_type =
            old_reactions.pending_use_default_paid_reaction_type;
        self.pending_paid_reaction_type = old_reactions.pending_paid_reaction_type.clone();
    }

    /// `add_my_reaction` (MessageReaction.cpp:702-748).
    pub fn add_my_reaction(
        &mut self,
        env: &impl MessageReactionEnv,
        reaction_type: &ReactionType,
        is_big: bool,
        my_dialog_id: DialogId,
        have_recent_choosers: bool,
        is_tag: bool,
    ) -> bool {
        let mut new_chosen_reaction_order = self.get_chosen_reaction_types();

        match self.get_reaction_mut(reaction_type) {
            None => {
                let mut recent_chooser_dialog_ids = Vec::new();
                let mut my_recent_chooser_dialog_id = DialogId::new(0);
                if have_recent_choosers {
                    recent_chooser_dialog_ids.push(my_dialog_id);
                    my_recent_chooser_dialog_id = my_dialog_id;
                }
                self.reactions.push(MessageReaction::new(
                    reaction_type.clone(),
                    1,
                    true,
                    my_recent_chooser_dialog_id,
                    recent_chooser_dialog_ids,
                    Vec::new(),
                ));
                new_chosen_reaction_order.push(reaction_type.clone());
            }
            Some(added_reaction) if !added_reaction.is_chosen() => {
                added_reaction.set_as_chosen(my_dialog_id, have_recent_choosers);
                new_chosen_reaction_order.push(reaction_type.clone());
            }
            Some(_) if !is_big => return false,
            Some(_) => {}
        }
        if !is_tag {
            assert!(!self.are_tags, "the C++ CHECKs !are_tags_");
        } else {
            self.are_tags = true;
        }

        let max_reaction_count = get_max_reaction_count(env);
        while new_chosen_reaction_order.len() > max_reaction_count {
            let index = if new_chosen_reaction_order[0] == *reaction_type {
                1
            } else {
                0
            };
            debug_assert!(
                index < new_chosen_reaction_order.len(),
                "the C++ CHECKs the index"
            );
            let is_removed = self.do_remove_my_reaction(&new_chosen_reaction_order[index]);
            debug_assert!(is_removed, "the C++ CHECKs the removal");
            new_chosen_reaction_order.remove(index);
        }

        if new_chosen_reaction_order.len() == 1 {
            new_chosen_reaction_order.clear();
        }
        self.chosen_reaction_order = new_chosen_reaction_order;

        for message_reaction in &mut self.reactions {
            message_reaction.set_my_recent_chooser_dialog_id(my_dialog_id);
        }

        true
    }

    /// `remove_my_reaction` (MessageReaction.cpp:750-776).
    pub fn remove_my_reaction(
        &mut self,
        env: &impl MessageReactionEnv,
        reaction_type: &ReactionType,
        my_dialog_id: DialogId,
    ) -> bool {
        if self.do_remove_my_reaction(reaction_type) {
            if !self.chosen_reaction_order.is_empty() {
                let is_removed = remove_first(&mut self.chosen_reaction_order, reaction_type);
                debug_assert!(is_removed, "the C++ CHECKs the removal");

                // if the user isn't a Premium user, then max_reaction_count
                // could be reduced from 3 to 1
                let max_reaction_count = get_max_reaction_count(env);
                while self.chosen_reaction_order.len() > max_reaction_count {
                    let first = self.chosen_reaction_order[0].clone();
                    let is_removed = self.do_remove_my_reaction(&first);
                    debug_assert!(is_removed, "the C++ CHECKs the removal");
                    self.chosen_reaction_order.remove(0);
                }

                if self.chosen_reaction_order.len() <= 1 {
                    self.chosen_reaction_order.clear();
                }
            }

            for message_reaction in &mut self.reactions {
                message_reaction.set_my_recent_chooser_dialog_id(my_dialog_id);
            }

            return true;
        }
        false
    }

    /// `do_remove_my_reaction` (MessageReaction.cpp:778-793).
    fn do_remove_my_reaction(&mut self, reaction_type: &ReactionType) -> bool {
        if let Some(index) = self.reactions.iter().position(|message_reaction| {
            message_reaction.get_reaction_type() == reaction_type && message_reaction.is_chosen()
        }) {
            self.reactions[index].unset_as_chosen();
            if self.reactions[index].is_empty() {
                self.reactions.remove(index);
            }
            return true;
        }
        false
    }

    /// `add_my_paid_reaction` (MessageReaction.cpp:795-827).
    pub fn add_my_paid_reaction(
        &mut self,
        env: &mut impl MessageReactionPaidEnv,
        star_count: i32,
        type_: Option<td_api::PaidReactionType>,
    ) {
        if self.pending_paid_reactions > 1_000_000_000 || star_count > 1_000_000_000 {
            tdutils::log_error!("Pending paid reactions overflown");
            return;
        }
        let use_default_paid_reaction_type = type_.is_none();
        let paid_reaction_type = PaidReactionType::from_td_api(env, type_);
        env.add_pending_owned_star_count(-star_count, false);
        if use_default_paid_reaction_type {
            if self.pending_paid_reactions == 0 {
                self.pending_use_default_paid_reaction_type = true;
            }
            if self.pending_use_default_paid_reaction_type {
                let mut was_me = false;
                let my_dialog_id = env.get_my_dialog_id();
                for reactor in &self.top_reactors {
                    if reactor.is_me() {
                        was_me = true;
                        self.pending_paid_reaction_type =
                            reactor.get_paid_reaction_type(my_dialog_id);
                    }
                }
                if !was_me {
                    self.pending_paid_reaction_type = env.get_default_paid_reaction_type();
                }
            }
        } else {
            env.on_update_default_paid_reaction_type(&paid_reaction_type);

            self.pending_use_default_paid_reaction_type = false;
            self.pending_paid_reaction_type = paid_reaction_type;
        }
        self.pending_paid_reactions += star_count;
    }

    /// `has_pending_paid_reactions` (MessageReaction.cpp:829-831).
    pub fn has_pending_paid_reactions(&self) -> bool {
        self.pending_paid_reactions != 0
    }

    /// `drop_pending_paid_reactions` (MessageReaction.cpp:833-839).
    pub fn drop_pending_paid_reactions(&mut self, env: &mut impl MessageReactionPaidEnv) {
        debug_assert!(
            self.has_pending_paid_reactions(),
            "the C++ CHECKs the pending reactions"
        );
        env.add_pending_owned_star_count(self.pending_paid_reactions, false);
        self.pending_paid_reactions = 0;
        self.pending_use_default_paid_reaction_type = false;
        self.pending_paid_reaction_type = PaidReactionType::default();
    }

    /// `sort_reactions` (MessageReaction.cpp:841-860).
    pub fn sort_reactions(&mut self, active_reaction_pos: &HashMap<ReactionType, usize>) {
        let missing_pos = active_reaction_pos.len();
        self.reactions.sort_by(|lhs, rhs| {
            use std::cmp::Ordering;
            if lhs.get_reaction_type().is_paid_reaction()
                != rhs.get_reaction_type().is_paid_reaction()
            {
                // the paid reaction sorts last, like the C++ "less" arm
                return if lhs.get_reaction_type().is_paid_reaction() {
                    Ordering::Greater
                } else {
                    Ordering::Less
                };
            }
            if lhs.get_choose_count() != rhs.get_choose_count() {
                return rhs.get_choose_count().cmp(&lhs.get_choose_count());
            }
            let lhs_pos = active_reaction_pos
                .get(lhs.get_reaction_type())
                .copied()
                .unwrap_or(missing_pos);
            let rhs_pos = active_reaction_pos
                .get(rhs.get_reaction_type())
                .copied()
                .unwrap_or(missing_pos);
            if lhs_pos != rhs_pos {
                return lhs_pos.cmp(&rhs_pos);
            }

            lhs.get_reaction_type().cmp(rhs.get_reaction_type())
        });
    }

    /// `fix_chosen_reaction` (MessageReaction.cpp:862-880).
    pub fn fix_chosen_reaction(&mut self) {
        let mut my_dialog_id = DialogId::new(0);
        for reaction in &mut self.reactions {
            if !reaction.is_chosen() && reaction.get_my_recent_chooser_dialog_id().is_valid() {
                my_dialog_id = reaction.get_my_recent_chooser_dialog_id();
                tdutils::log_warning!("Fix recent chosen reaction");
                reaction.remove_my_recent_chooser_dialog_id();
            }
        }
        if !my_dialog_id.is_valid() {
            return;
        }
        for reaction in &mut self.reactions {
            if !reaction.get_reaction_type().is_paid_reaction()
                && reaction.is_chosen()
                && !reaction.get_my_recent_chooser_dialog_id().is_valid()
            {
                reaction.add_my_recent_chooser_dialog_id(my_dialog_id);
            }
        }
    }

    /// `fix_my_recent_chooser_dialog_id` (MessageReaction.cpp:882-890).
    pub fn fix_my_recent_chooser_dialog_id(&mut self, my_dialog_id: DialogId) {
        for reaction in &mut self.reactions {
            if !reaction.get_reaction_type().is_paid_reaction()
                && reaction.is_chosen()
                && !reaction.get_my_recent_chooser_dialog_id().is_valid()
                && reaction
                    .get_recent_chooser_dialog_ids()
                    .contains(&my_dialog_id)
            {
                reaction.my_recent_chooser_dialog_id = my_dialog_id;
            }
        }
    }

    /// `get_chosen_reaction_types` (MessageReaction.cpp:892-904).
    pub fn get_chosen_reaction_types(&self) -> Vec<ReactionType> {
        if !self.chosen_reaction_order.is_empty() {
            return self.chosen_reaction_order.clone();
        }

        let mut reaction_order = Vec::new();
        for reaction in &self.reactions {
            if !reaction.get_reaction_type().is_paid_reaction() && reaction.is_chosen() {
                reaction_order.push(reaction.get_reaction_type().clone());
            }
        }
        reaction_order
    }

    /// `are_consistent_with_list` (MessageReaction.cpp:906-941) — the
    /// C++ `FlatHashMap operator[]` default-constructs missing entries;
    /// reads of a missing key answer an empty list here.
    pub fn are_consistent_with_list(
        &self,
        reaction_type: &ReactionType,
        mut reaction_types: HashMap<ReactionType, Vec<DialogId>>,
        total_count: i32,
    ) -> bool {
        let are_consistent = |lhs: &[DialogId], rhs: &[DialogId]| -> bool {
            let max_i = std::cmp::min(lhs.len(), rhs.len());
            let mut i = 0;
            while i < max_i && lhs[i] == rhs[i] {
                i += 1;
            }
            i == max_i
        };

        if reaction_type.is_empty() {
            // received list and total_count for all reactions
            let mut old_total_count = 0;
            for message_reaction in &self.reactions {
                debug_assert!(
                    !message_reaction.get_reaction_type().is_empty(),
                    "the C++ CHECKs the type"
                );
                let empty = Vec::new();
                let choosers = reaction_types
                    .get(message_reaction.get_reaction_type())
                    .unwrap_or(&empty);
                if !are_consistent(choosers, message_reaction.get_recent_chooser_dialog_ids()) {
                    return false;
                }
                old_total_count += message_reaction.get_choose_count();
                reaction_types.remove(message_reaction.get_reaction_type());
            }
            return old_total_count == total_count && reaction_types.is_empty();
        }

        // received list and total_count for a single reaction
        match self.get_reaction(reaction_type) {
            None => !reaction_types.contains_key(reaction_type) && total_count == 0,
            Some(message_reaction) => {
                let empty = Vec::new();
                let choosers = reaction_types.get(reaction_type).unwrap_or(&empty);
                are_consistent(choosers, message_reaction.get_recent_chooser_dialog_ids())
                    && message_reaction.get_choose_count() == total_count
            }
        }
    }

    /// `apply_reactor_pending_paid_reactions`
    /// (MessageReaction.cpp:943-961).
    pub fn apply_reactor_pending_paid_reactions(
        &self,
        my_dialog_id: DialogId,
    ) -> Vec<MessageReactor> {
        let mut top_reactors = Vec::new();
        let mut was_me = false;
        let reactor_dialog_id = self.pending_paid_reaction_type.get_dialog_id(my_dialog_id);
        for reactor in &self.top_reactors {
            let mut copy = reactor.clone();
            if reactor.is_me() {
                was_me = true;
                copy.add_count(self.pending_paid_reactions, reactor_dialog_id, my_dialog_id);
            }
            top_reactors.push(copy);
        }
        if !was_me {
            let is_anonymous = reactor_dialog_id == DialogId::new(0);
            top_reactors.push(MessageReactor::new(
                if is_anonymous {
                    my_dialog_id
                } else {
                    reactor_dialog_id
                },
                self.pending_paid_reactions,
                true,
                is_anonymous,
            ));
        }
        MessageReactor::fix_message_reactors(&mut top_reactors, false, false);
        top_reactors
    }

    /// `get_non_paid_reaction_count` (MessageReaction.cpp:989-997).
    pub fn get_non_paid_reaction_count(&self) -> i32 {
        let mut result = 0;
        for reaction in &self.reactions {
            if !reaction.reaction_type.is_paid_reaction() {
                result += 1;
            }
        }
        result
    }

    /// `need_update_message_reactions` (MessageReaction.cpp:1023-1040).
    pub fn need_update_message_reactions(
        old_reactions: Option<&MessageReactions>,
        new_reactions: Option<&MessageReactions>,
    ) -> bool {
        let Some(old_reactions) = old_reactions else {
            // add reactions
            return new_reactions.is_some();
        };
        let Some(new_reactions) = new_reactions else {
            // remove reactions when they are disabled
            return true;
        };

        // unread_reactions/chosen_reaction_order update independently;
        // compare all other fields
        old_reactions.reactions != new_reactions.reactions
            || old_reactions.is_min != new_reactions.is_min
            || old_reactions.can_get_added_reactions != new_reactions.can_get_added_reactions
            || old_reactions.need_polling != new_reactions.need_polling
            || old_reactions.are_tags != new_reactions.are_tags
            || old_reactions.top_reactors != new_reactions.top_reactors
    }

    /// `need_update_unread_reactions` (MessageReaction.cpp:1042-1048).
    pub fn need_update_unread_reactions(
        old_reactions: Option<&MessageReactions>,
        new_reactions: Option<&MessageReactions>,
    ) -> bool {
        match old_reactions {
            None => new_reactions
                .is_some_and(|new_reactions| !new_reactions.unread_reactions.is_empty()),
            Some(old_reactions) if old_reactions.unread_reactions.is_empty() => new_reactions
                .is_some_and(|new_reactions| !new_reactions.unread_reactions.is_empty()),
            Some(old_reactions) => match new_reactions {
                None => true,
                Some(new_reactions) => {
                    old_reactions.unread_reactions != new_reactions.unread_reactions
                }
            },
        }
    }

    /// `MessageReactions::store` (MessageReaction.hpp:100-128).
    pub fn store(&self, env: &mut impl crate::photo::DialogPhotoStoreEnv, out: &mut Vec<u8>) {
        let has_reactions = !self.reactions.is_empty();
        let has_unread_reactions = !self.unread_reactions.is_empty();
        let has_chosen_reaction_order = !self.chosen_reaction_order.is_empty();
        let has_top_reactors = !self.top_reactors.is_empty();
        store_flags(
            out,
            [
                self.is_min,
                self.need_polling,
                self.can_get_added_reactions,
                has_unread_reactions,
                has_reactions,
                has_chosen_reaction_order,
                self.are_tags,
                has_top_reactors,
            ],
        );
        if has_reactions {
            store_i32(out, self.reactions.len() as i32);
            for reaction in &self.reactions {
                reaction.store(env, out);
            }
        }
        if has_unread_reactions {
            store_i32(out, self.unread_reactions.len() as i32);
            for unread_reaction in &self.unread_reactions {
                unread_reaction.store(out);
            }
        }
        if has_chosen_reaction_order {
            store_i32(out, self.chosen_reaction_order.len() as i32);
            for reaction_type in &self.chosen_reaction_order {
                reaction_type.store(out);
            }
        }
        if has_top_reactors {
            store_i32(out, self.top_reactors.len() as i32);
            for reactor in &self.top_reactors {
                reactor.store(env, out);
            }
        }
    }

    /// `MessageReactions::parse` (MessageReaction.hpp:130-158).
    pub fn parse(
        env: &mut impl crate::photo::DialogPhotoStoreEnv,
        parser: &mut Parser<'_>,
    ) -> Result<MessageReactions, &'static str> {
        let mut result = MessageReactions::default();
        let [
            is_min,
            need_polling,
            can_get_added_reactions,
            has_unread_reactions,
            has_reactions,
            has_chosen_reaction_order,
            are_tags,
            has_top_reactors,
        ] = parser.parse_flags()?;
        result.is_min = is_min;
        result.need_polling = need_polling;
        result.can_get_added_reactions = can_get_added_reactions;
        result.are_tags = are_tags;
        if has_reactions {
            let len = parser.i32_()?;
            if len < 0 {
                return Err("Invalid vector size");
            }
            result.reactions.reserve(len as usize);
            for _ in 0..len {
                result.reactions.push(MessageReaction::parse(env, parser)?);
            }
        }
        if has_unread_reactions {
            let len = parser.i32_()?;
            if len < 0 {
                return Err("Invalid vector size");
            }
            result.unread_reactions.reserve(len as usize);
            for _ in 0..len {
                result
                    .unread_reactions
                    .push(UnreadMessageReaction::parse(parser)?);
            }
        }
        if has_chosen_reaction_order {
            let len = parser.i32_()?;
            if len < 0 {
                return Err("Invalid vector size");
            }
            result.chosen_reaction_order.reserve(len as usize);
            for _ in 0..len {
                result
                    .chosen_reaction_order
                    .push(ReactionType::parse(parser)?);
            }
        }
        if has_top_reactors {
            let len = parser.i32_()?;
            if len < 0 {
                return Err("Invalid vector size");
            }
            result.top_reactors.reserve(len as usize);
            for _ in 0..len {
                result
                    .top_reactors
                    .push(MessageReactor::parse(env, parser)?);
            }
        }
        Ok(result)
    }
}

/// `operator==` is memberwise over the compared-in-need-update fields;
/// the C++ default `operator==` (MessageReaction.h:168) compares
/// everything — used by the intake diffs.
impl PartialEq for MessageReactions {
    fn eq(&self, other: &Self) -> bool {
        self.reactions == other.reactions
            && self.unread_reactions == other.unread_reactions
            && self.chosen_reaction_order == other.chosen_reaction_order
            && self.top_reactors == other.top_reactors
            && self.pending_paid_reactions == other.pending_paid_reactions
            && self.pending_paid_reaction_type == other.pending_paid_reaction_type
            && self.pending_use_default_paid_reaction_type
                == other.pending_use_default_paid_reaction_type
            && self.is_min == other.is_min
            && self.need_polling == other.need_polling
            && self.can_get_added_reactions == other.can_get_added_reactions
            && self.are_tags == other.are_tags
    }
}

impl std::fmt::Display for MessageReactions {
    /// `operator<<` (MessageReaction.cpp:1097-1108).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.are_tags {
            return write!(f, "MessageTags{{{:?}}}", self.reactions);
        }
        write!(
            f,
            "{}MessageReactions{{{:?} with unread {:?}, reaction order {:?} and can_get_added_reactions = {} with paid reactions by {:?} and {} pending {}}}",
            if self.is_min { "Min" } else { "" },
            self.reactions,
            self.unread_reactions,
            self.chosen_reaction_order,
            self.can_get_added_reactions,
            self.top_reactors,
            self.pending_paid_reactions,
            self.pending_paid_reaction_type,
        )
    }
}

/// `get_chosen_tags` (MessageReaction.cpp:1190-1195) — the C++ unique_ptr
/// argument is the `None` case.
pub fn get_chosen_tags(message_reactions: Option<&MessageReactions>) -> Vec<ReactionType> {
    match message_reactions {
        None => Vec::new(),
        Some(message_reactions) => {
            if !message_reactions.are_tags {
                return Vec::new();
            }
            message_reactions.get_chosen_reaction_types()
        }
    }
}

#[cfg(test)]
mod message_reaction_tests {
    use super::*;
    use crate::store_parse::store_log_event_version;

    /// the FileId codec stub: the fixtures carry no min-channel photos,
    /// so the codec never fires
    struct TestFileEnv;
    impl crate::photo::DialogPhotoStoreEnv for TestFileEnv {
        fn store_file_id(&mut self, _file_id: crate::files::FileId, _out: &mut Vec<u8>) {
            panic!("no photos in the fixtures");
        }
        fn parse_file_id(
            &mut self,
            _parser: &mut Parser<'_>,
        ) -> Result<crate::files::FileId, &'static str> {
            panic!("no photos in the fixtures");
        }
    }

    #[derive(Default)]
    struct Env {
        is_premium: bool,
    }

    impl MessageReactionEnv for Env {
        fn get_option_boolean(&self, name: &str, _default_value: bool) -> bool {
            name == "is_premium" && self.is_premium
        }

        fn get_option_integer(&self, _name: &str, default_value: i64) -> i64 {
            default_value
        }
    }

    fn emoji(reaction: &str) -> ReactionType {
        ReactionType::new(reaction.to_owned())
    }

    #[test]
    fn chooser_machine_follows_the_cpp_clamps() {
        let mut reaction = MessageReaction::new(
            emoji("👍"),
            2,
            false,
            DialogId::new(0),
            Vec::new(),
            Vec::new(),
        );
        assert!(!reaction.is_empty());
        assert_eq!(reaction.get_choose_count(), 2);

        let me = DialogId::new(-99);
        reaction.set_as_chosen(me, true);
        assert!(reaction.is_chosen());
        assert_eq!(reaction.get_choose_count(), 3);
        assert_eq!(reaction.get_recent_chooser_dialog_ids(), &[me]);
        assert_eq!(reaction.get_my_recent_chooser_dialog_id(), me);

        reaction.unset_as_chosen();
        assert!(!reaction.is_chosen());
        assert_eq!(reaction.get_choose_count(), 2);
        assert!(!reaction.get_my_recent_chooser_dialog_id().is_valid());

        // the paid add keeps the is_chosen flag and sums the stars
        let mut paid = MessageReaction::new_paid(10);
        paid.add_paid_reaction(5);
        assert_eq!(paid.get_choose_count(), 15);
        assert!(paid.is_chosen());
    }

    #[test]
    fn add_and_remove_my_reaction_clamp_the_order() {
        let mut env = Env::default();
        let me = DialogId::new(-99);

        // a default user keeps at most 1 chosen reaction
        let mut reactions = MessageReactions::default();
        assert!(reactions.add_my_reaction(&env, &emoji("👍"), false, me, true, false));
        // the explicit order stays empty at one entry; the derived list answers
        assert_eq!(reactions.get_chosen_reaction_types(), vec![emoji("👍")]);
        assert!(reactions.get_reaction(&emoji("👍")).unwrap().is_chosen());

        // re-adding the same reaction without is_big is a no-op
        assert!(!reactions.add_my_reaction(&env, &emoji("👍"), false, me, true, false));

        // a second chosen reaction evicts the first (max 1)
        assert!(reactions.add_my_reaction(&env, &emoji("🔥"), false, me, true, false));
        assert_eq!(reactions.get_chosen_reaction_types(), vec![emoji("🔥")]);
        assert!(reactions.get_reaction(&emoji("👍")).is_none()); // dropped at count 0

        // a premium user keeps up to 3, tracked in the explicit order
        env.is_premium = true;
        assert!(reactions.add_my_reaction(&env, &emoji("👍"), false, me, true, false));
        assert!(reactions.add_my_reaction(&env, &emoji("😂"), false, me, true, false));
        assert_eq!(
            reactions.get_chosen_reaction_types(),
            vec![emoji("🔥"), emoji("👍"), emoji("😂")]
        );

        assert!(reactions.remove_my_reaction(&env, &emoji("🔥"), me));
        assert_eq!(
            reactions.get_chosen_reaction_types(),
            vec![emoji("👍"), emoji("😂")]
        );
        assert!(!reactions.remove_my_reaction(&env, &emoji("❤"), me));
    }

    #[test]
    fn update_from_keeps_the_chosen_state_of_a_min_intake() {
        let mut old = MessageReactions::default();
        old.reactions.push(MessageReaction::new(
            emoji("👍"),
            3,
            true,
            DialogId::new(0),
            Vec::new(),
            Vec::new(),
        ));
        old.chosen_reaction_order.clear();
        old.unread_reactions.push(UnreadMessageReaction::new(
            emoji("👍"),
            DialogId::new(-7),
            true,
        ));

        let mut new_reactions = MessageReactions {
            is_min: true,
            ..Default::default()
        };
        new_reactions.reactions.push(MessageReaction::new(
            emoji("👍"),
            3,
            false,
            DialogId::new(0),
            Vec::new(),
            Vec::new(),
        ));
        // the fresh min intake differs from the stored state ...
        assert!(MessageReactions::need_update_message_reactions(
            Some(&old),
            Some(&new_reactions)
        ));
        new_reactions.update_from(&old, DialogId::new(-99));
        // ... and the chosen state + unread list survive the min upgrade
        assert!(
            new_reactions
                .get_reaction(&emoji("👍"))
                .unwrap()
                .is_chosen()
        );
        assert_eq!(new_reactions.unread_reactions, old.unread_reactions);
    }

    #[test]
    fn sort_reactions_rides_the_active_positions() {
        let mut reactions = MessageReactions::default();
        reactions.reactions.push(MessageReaction::new(
            emoji("👍"),
            1,
            false,
            DialogId::new(0),
            Vec::new(),
            Vec::new(),
        ));
        reactions.reactions.push(MessageReaction::new(
            emoji("🔥"),
            5,
            false,
            DialogId::new(0),
            Vec::new(),
            Vec::new(),
        ));
        reactions.reactions.push(MessageReaction::new_paid(10));
        let mut active_pos = HashMap::new();
        active_pos.insert(emoji("🔥"), 0);
        active_pos.insert(emoji("👍"), 1);
        reactions.sort_reactions(&active_pos);
        // count first, paid last
        assert_eq!(reactions.reactions[0].get_reaction_type(), &emoji("🔥"));
        assert_eq!(reactions.reactions[1].get_reaction_type(), &emoji("👍"));
        assert!(
            reactions.reactions[2]
                .get_reaction_type()
                .is_paid_reaction()
        );
        assert_eq!(reactions.get_non_paid_reaction_count(), 2);
    }

    #[test]
    fn paid_reaction_pending_machine_matches_the_cpp() {
        struct PaidEnv {
            base: Env,
            my_dialog_id: DialogId,
            default_type: PaidReactionType,
            updated: Vec<PaidReactionType>,
            pending_star: i32,
        }
        impl MessageReactionEnv for PaidEnv {
            fn get_option_boolean(&self, name: &str, default_value: bool) -> bool {
                self.base.get_option_boolean(name, default_value)
            }
            fn get_option_integer(&self, name: &str, default_value: i64) -> i64 {
                self.base.get_option_integer(name, default_value)
            }
        }
        impl crate::paid_reaction_type::PaidReactionTypeEnv for PaidEnv {
            // the fixtures never exercise the Dialog paid-reaction type,
            // so the dialog probes stay unreachable
            fn have_dialog_info(&self, _dialog_id: DialogId) -> bool {
                panic!("no dialog plane in the fixtures");
            }
            fn force_create_dialog(&mut self, _dialog_id: DialogId) {
                panic!("no dialog plane in the fixtures");
            }
            fn check_dialog_access(
                &self,
                _dialog_id: DialogId,
                _source: &str,
            ) -> std::result::Result<(), tdutils::status::Status> {
                panic!("no dialog plane in the fixtures");
            }
            fn is_broadcast_channel(&self, _dialog_id: DialogId) -> bool {
                panic!("no dialog plane in the fixtures");
            }
            fn get_input_peer(
                &self,
                _dialog_id: DialogId,
            ) -> Option<tdmtproto::telegram_api::InputPeer> {
                panic!("no dialog plane in the fixtures");
            }
            fn get_chat_id_object(&self, _dialog_id: DialogId, _source: &str) -> i64 {
                panic!("no dialog plane in the fixtures");
            }
        }
        impl MessageReactionPaidEnv for PaidEnv {
            fn get_my_dialog_id(&self) -> DialogId {
                self.my_dialog_id
            }
            fn add_pending_owned_star_count(&mut self, count: i32, _is_final: bool) {
                self.pending_star += count;
            }
            fn get_default_paid_reaction_type(&self) -> PaidReactionType {
                self.default_type.clone()
            }
            fn on_update_default_paid_reaction_type(&mut self, type_: &PaidReactionType) {
                self.updated.push(type_.clone());
            }
        }

        let mut env = PaidEnv {
            base: Env::default(),
            my_dialog_id: DialogId::new(-99),
            default_type: PaidReactionType::legacy(true),
            updated: Vec::new(),
            pending_star: 0,
        };
        let mut reactions = MessageReactions::default();

        // the default-type path parks the default until the first commit
        reactions.add_my_paid_reaction(&mut env, 10, None);
        assert_eq!(reactions.pending_paid_reactions, 10);
        assert!(reactions.pending_use_default_paid_reaction_type);
        assert_eq!(
            reactions.pending_paid_reaction_type,
            PaidReactionType::legacy(true)
        );
        assert_eq!(env.pending_star, -10);

        // an explicit type updates the default and switches the flag off
        reactions.add_my_paid_reaction(
            &mut env,
            5,
            Some(td_api::PaidReactionType::paidReactionTypeAnonymous(
                td_api::paidReactionTypeAnonymous {},
            )),
        );
        assert!(!reactions.pending_use_default_paid_reaction_type);
        assert_eq!(
            reactions.pending_paid_reaction_type,
            PaidReactionType::legacy(true)
        );
        assert_eq!(env.updated, vec![PaidReactionType::legacy(true)]);
        assert_eq!(reactions.pending_paid_reactions, 15);

        reactions.drop_pending_paid_reactions(&mut env);
        assert!(!reactions.has_pending_paid_reactions());
        // -10 - 5 while pending, +15 back on the drop
        assert_eq!(env.pending_star, 0);
    }

    #[test]
    fn store_parse_round_trips_every_block() {
        let mut reactions = MessageReactions::default();
        reactions.reactions.push(MessageReaction::new(
            emoji("👍"),
            3,
            true,
            DialogId::new(-99),
            vec![DialogId::new(-99), DialogId::new(-7)],
            Vec::new(),
        ));
        reactions.unread_reactions.push(UnreadMessageReaction::new(
            emoji("👍"),
            DialogId::new(-7),
            true,
        ));
        reactions.chosen_reaction_order.push(emoji("👍"));
        reactions.are_tags = true;
        reactions.can_get_added_reactions = true;

        let mut out = Vec::new();
        store_log_event_version(&mut out);
        reactions.store(&mut TestFileEnv, &mut out);
        let mut parser = Parser::new_log_event(&out).unwrap();
        let parsed = MessageReactions::parse(&mut TestFileEnv, &mut parser).unwrap();
        parser.fetch_end().unwrap();
        assert_eq!(parsed, reactions);

        // the default (all-blocks-off) shape round trips too
        let empty_row = MessageReactions::default();
        let mut out = Vec::new();
        store_log_event_version(&mut out);
        empty_row.store(&mut TestFileEnv, &mut out);
        let mut parser = Parser::new_log_event(&out).unwrap();
        let parsed = MessageReactions::parse(&mut TestFileEnv, &mut parser).unwrap();
        parser.fetch_end().unwrap();
        assert_eq!(parsed, empty_row);
    }

    #[test]
    fn chosen_tags_and_consistency_follow_the_cpp() {
        let mut reactions = MessageReactions {
            are_tags: true,
            ..Default::default()
        };
        reactions.chosen_reaction_order.push(emoji("👍"));
        assert_eq!(get_chosen_tags(Some(&reactions)), vec![emoji("👍")]);
        reactions.are_tags = false;
        assert!(get_chosen_tags(Some(&reactions)).is_empty());
        assert!(get_chosen_tags(None).is_empty());

        // a single-reaction list matches the row's choosers and count
        let mut reactions = MessageReactions::default();
        reactions.reactions.push(MessageReaction::new(
            emoji("👍"),
            2,
            false,
            DialogId::new(0),
            vec![DialogId::new(-7), DialogId::new(-8)],
            Vec::new(),
        ));
        let mut list = HashMap::new();
        list.insert(emoji("👍"), vec![DialogId::new(-7), DialogId::new(-8)]);
        assert!(reactions.are_consistent_with_list(&emoji("👍"), list.clone(), 2));
        assert!(!reactions.are_consistent_with_list(&emoji("👍"), list, 3));
    }
}
