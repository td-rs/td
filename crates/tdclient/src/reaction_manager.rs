//! Port of `td/telegram/ReactionManager.{h,cpp,hpp}` (M.16.3) plus the
//! model headers that only serve this manager: `MessageEffectId.h`,
//! `SavedMessagesTopicId.h` (the minimal value slice; the topic lists
//! stay with SavedMessagesManager, Wave 3) and
//! `ReactionUnavailabilityReason.h`.
//!
//! The manager is the collapsed actor: `Td` owns the struct, the
//! [`ReactionManagerEnv`] trait carries the `G()`/option/pm/cross-manager
//! surfaces, and every `create_handler<...>` network tail is an env seam
//! marked `[blocked: net]` — the C++ promise lands in the query and stays
//! unanswered offline, exactly like the collapsed-line convention.
//!
//! FileIds of the available-reaction animations ride the
//! [`crate::photo::DialogPhotoStoreEnv`] codec (`FileManager::store_file`/
//! `parse_file`), the M.40 icon-plane encoding — a documented divergence
//! from the C++ `StickersManager::store_sticker` framing; every
//! non-FileId byte of the pmc records stays C++-compatible.
//!
//! Deferred intakes (need the StickersManager document plane, M.15):
//! `on_get_available_reactions`/`on_get_message_effects` — the wire
//! result handlers that build the animation FileIds. Everything locally
//! observable (the pmc record planes, the reaction lists, the saved-tags
//! machine, the synchronous available-reactions builders, the default
//! paid-reaction type) is here, including the net-independent
//! `on_get_reaction_list`/`on_get_saved_messages_tags` intakes, which are
//! fully portable and unit-tested against the wire shapes.

use crate::paid_reaction_type::PaidReactionType;
use crate::photo::DialogPhotoStoreEnv;
use crate::reaction_list_type::{
    MAX_REACTION_LIST_TYPE, ReactionListType, get_reaction_list_type_database_key,
};
use crate::reaction_type::{ReactionType, get_reaction_types_hash};
use crate::store_parse::{Parser, store_flags, store_i32, store_i64, store_log_event_version};
use crate::td_api;
use std::collections::HashMap;
use tdmtproto::telegram_api;
use tdutils::promise::Promise;

/// `MessageEffectId` (MessageEffectId.h) — the i64 wrapper with the
/// `store_long` db form.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct MessageEffectId(i64);

impl MessageEffectId {
    pub fn new(id: i64) -> MessageEffectId {
        MessageEffectId(id)
    }

    /// `is_valid` (MessageEffectId.h:31-33).
    pub fn is_valid(self) -> bool {
        self.0 != 0
    }

    /// `get` (MessageEffectId.h:35-37).
    pub fn get(self) -> i64 {
        self.0
    }

    /// `store` — `storer.store_long(id)`.
    pub fn store(self, out: &mut Vec<u8>) {
        store_i64(out, self.0);
    }

    /// `parse` — `parser.fetch_long()`.
    pub fn parse(parser: &mut Parser<'_>) -> Result<MessageEffectId, &'static str> {
        Ok(MessageEffectId(parser.i64_()?))
    }
}

impl std::fmt::Display for MessageEffectId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MessageEffectId {}", self.0)
    }
}

/// `SavedMessagesTopicId` (SavedMessagesTopicId.h) — the value slice this
/// manager needs: a `DialogId` wrapper with `get_unique_id`. The topic
/// lists, the input-peer machinery and the object builders stay with
/// SavedMessagesManager (Wave 3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct SavedMessagesTopicId {
    dialog_id: crate::dialog_id::DialogId,
}

impl SavedMessagesTopicId {
    /// `SavedMessagesTopicId(DialogId)`.
    pub fn from_dialog_id(dialog_id: crate::dialog_id::DialogId) -> SavedMessagesTopicId {
        SavedMessagesTopicId { dialog_id }
    }

    /// `is_valid` — `dialog_id_.is_valid()`.
    pub fn is_valid(self) -> bool {
        self.dialog_id.is_valid()
    }

    /// `get_unique_id` — `dialog_id_.get()`.
    pub fn get_unique_id(self) -> i64 {
        self.dialog_id.get()
    }

    /// The raw dialog id (the local accessor of the private C++ field).
    pub fn get_dialog_id(self) -> crate::dialog_id::DialogId {
        self.dialog_id
    }
}

impl std::fmt::Display for SavedMessagesTopicId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SavedMessagesTopicId {}", self.dialog_id.get())
    }
}

/// `ReactionUnavailabilityReason` (ReactionUnavailabilityReason.h).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReactionUnavailabilityReason {
    None,
    AnonymousAdministrator,
    Guest,
    Restricted,
}

/// The `Td *td` slice of the collapsed ReactionManager.
pub trait ReactionManagerEnv:
    DialogPhotoStoreEnv + crate::paid_reaction_type::PaidReactionTypeEnv
{
    // -- the G()/auth slice
    fn auth_is_authorized(&self) -> bool;
    fn auth_is_bot(&self) -> bool;
    fn close_flag(&self) -> bool;

    // -- options
    fn get_option_boolean(&self, name: &str, default_value: bool) -> bool;
    fn get_option_integer(&self, name: &str, default_value: i64) -> i64;
    fn get_option_string(&self, name: &str, default_value: &str) -> String;
    fn have_option(&self, name: &str) -> bool;
    fn set_option_string(&mut self, name: &str, value: &str);
    fn set_option_boolean(&mut self, name: &str, value: bool);
    fn set_option_empty(&mut self, name: &str);

    // -- the pmc slice
    /// `G()->td_db()->get_binlog_pmc()->get/set` — the raw form, the
    /// reaction records are binary (FileId payloads).
    fn binlog_pmc_get_raw(&mut self, key: &str) -> Vec<u8>;
    fn binlog_pmc_set_raw(&mut self, key: &str, value: &[u8]);
    /// `G()->td_db()->get_sqlite_sync_pmc()->get` /
    /// `get_sqlite_pmc()->set`.
    fn sqlite_pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8>;
    fn sqlite_pmc_set_raw(&mut self, key: &[u8], value: &[u8]);

    // -- the update plane
    /// `send_closure(G()->td(), &Td::send_update, ...)`.
    fn send_update(&mut self, update: td_api::Update);

    // -- the cross-manager surfaces
    /// `messages_manager_->set_active_reactions` (the M.10 consumer).
    fn set_active_reactions(&mut self, reactions: &[ReactionType]);
    /// `stickers_manager_->get_sticker_object(file_id)` — the M.15 seam;
    /// the collapsed line answers `None` and the object builders carry
    /// null stickers.
    fn get_sticker_object(&self, file_id: crate::files::FileId) -> Option<td_api::Sticker>;

    // -- the `[blocked: net]` query tails; each one drops the promise,
    // -- which keeps the C++ request unanswered offline
    /// `GetAvailableReactionsQuery` (ReactionManager.cpp:46-67).
    fn send_get_available_reactions_query(&mut self, hash: i32);
    /// `GetReactionListQuery` (ReactionManager.cpp:69-111).
    fn send_get_reaction_list_query(&mut self, reaction_list_type: ReactionListType, hash: i64);
    /// `ClearRecentReactionsQuery` (ReactionManager.cpp:113-141).
    fn send_clear_recent_reactions_query(&mut self, promise: Promise<()>);
    /// `SetDefaultReactionQuery` (ReactionManager.cpp:143-180).
    fn send_set_default_reaction_query(&mut self, reaction_type: &ReactionType);
    /// `GetSavedReactionTagsQuery` (ReactionManager.cpp:182-218).
    fn send_get_saved_reaction_tags_query(
        &mut self,
        saved_messages_topic_id: SavedMessagesTopicId,
        hash: i64,
    );
    /// `UpdateSavedReactionTagQuery` (ReactionManager.cpp:220-248).
    fn send_update_saved_reaction_tag_query(&mut self, reaction_type: &ReactionType, title: &str);
    /// `GetMessageAvailableEffectsQuery` (ReactionManager.cpp:250-277).
    fn send_get_available_effects_query(&mut self, hash: i32);
}

/// `ReactionManager::Reaction` (ReactionManager.h:97-120).
#[derive(Clone, Debug, Default)]
pub struct Reaction {
    pub(crate) reaction_type: ReactionType,
    pub(crate) title: String,
    pub(crate) is_active: bool,
    pub(crate) is_premium: bool,
    pub(crate) static_icon: crate::files::FileId,
    pub(crate) appear_animation: crate::files::FileId,
    pub(crate) select_animation: crate::files::FileId,
    pub(crate) activate_animation: crate::files::FileId,
    pub(crate) effect_animation: crate::files::FileId,
    pub(crate) around_animation: crate::files::FileId,
    pub(crate) center_animation: crate::files::FileId,
}

impl Reaction {
    /// `is_valid` (ReactionManager.h:110-113) — the center/around rows are
    /// optional, the rest are mandatory.
    pub fn is_valid(&self) -> bool {
        self.static_icon.is_valid()
            && self.appear_animation.is_valid()
            && self.select_animation.is_valid()
            && self.activate_animation.is_valid()
            && self.effect_animation.is_valid()
            && !self.reaction_type.is_empty()
    }

    /// `Reaction::store` (ReactionManager.hpp:20-44) — the sticker
    /// FileIds ride the FileManager codec.
    pub fn store(&self, env: &mut impl DialogPhotoStoreEnv, out: &mut Vec<u8>) {
        let has_around_animation = self.around_animation.is_valid();
        let has_center_animation = self.center_animation.is_valid();
        store_flags(
            out,
            [
                self.is_active,
                has_around_animation,
                has_center_animation,
                self.is_premium,
            ],
        );
        self.reaction_type.store(out);
        crate::store_parse::store_string(out, &self.title);
        env.store_file_id(self.static_icon, out);
        env.store_file_id(self.appear_animation, out);
        env.store_file_id(self.select_animation, out);
        env.store_file_id(self.activate_animation, out);
        env.store_file_id(self.effect_animation, out);
        if has_around_animation {
            env.store_file_id(self.around_animation, out);
        }
        if has_center_animation {
            env.store_file_id(self.center_animation, out);
        }
    }

    /// `Reaction::parse` (ReactionManager.hpp:46-72) — the premium flag is
    /// reset after the load, like the C++.
    pub fn parse(
        env: &mut impl DialogPhotoStoreEnv,
        parser: &mut Parser<'_>,
    ) -> Result<Reaction, &'static str> {
        let mut result = Reaction::default();
        let [
            is_active,
            has_around_animation,
            has_center_animation,
            is_premium,
        ] = parser.parse_flags()?;
        result.is_active = is_active;
        result.is_premium = is_premium;
        result.reaction_type = ReactionType::parse(parser)?;
        result.title = parser.string()?;
        result.static_icon = env.parse_file_id(parser)?;
        result.appear_animation = env.parse_file_id(parser)?;
        result.select_animation = env.parse_file_id(parser)?;
        result.activate_animation = env.parse_file_id(parser)?;
        result.effect_animation = env.parse_file_id(parser)?;
        if has_around_animation {
            result.around_animation = env.parse_file_id(parser)?;
        }
        if has_center_animation {
            result.center_animation = env.parse_file_id(parser)?;
        }

        result.is_premium = false;
        Ok(result)
    }
}

/// `ReactionManager::Reactions` (ReactionManager.h:122-132).
#[derive(Clone, Debug, Default)]
pub struct Reactions {
    pub(crate) hash: i32,
    pub(crate) are_being_reloaded: bool,
    pub(crate) reactions: Vec<Reaction>,
}

impl Reactions {
    /// `Reactions::store` (ReactionManager.hpp:74-84).
    pub fn store(&self, env: &mut impl DialogPhotoStoreEnv, out: &mut Vec<u8>) {
        let has_reactions = !self.reactions.is_empty();
        store_flags(out, [has_reactions]);
        if has_reactions {
            store_i32(out, self.reactions.len() as i32);
            for reaction in &self.reactions {
                reaction.store(env, out);
            }
            store_i32(out, self.hash);
        }
    }

    /// `Reactions::parse` (ReactionManager.hpp:86-96).
    pub fn parse(
        env: &mut impl DialogPhotoStoreEnv,
        parser: &mut Parser<'_>,
    ) -> Result<Reactions, &'static str> {
        let mut result = Reactions::default();
        let [has_reactions] = parser.parse_flags()?;
        if has_reactions {
            let len = parser.i32_()?;
            if len < 0 {
                return Err("Invalid vector size");
            }
            result.reactions.reserve(len as usize);
            for _ in 0..len {
                result.reactions.push(Reaction::parse(env, parser)?);
            }
            result.hash = parser.i32_()?;
        }
        Ok(result)
    }
}

/// `ReactionManager::ReactionList` (ReactionManager.h:134-145).
#[derive(Clone, Debug, Default)]
pub struct ReactionList {
    pub(crate) hash: i64,
    pub(crate) is_loaded_from_database: bool,
    pub(crate) is_being_reloaded: bool,
    pub(crate) reaction_types: Vec<ReactionType>,
}

impl ReactionList {
    /// `ReactionList::store` (ReactionManager.hpp:98-108).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_reaction_types = !self.reaction_types.is_empty();
        store_flags(out, [has_reaction_types]);
        if has_reaction_types {
            store_i32(out, self.reaction_types.len() as i32);
            for reaction_type in &self.reaction_types {
                reaction_type.store(out);
            }
            store_i64(out, self.hash);
        }
    }

    /// `ReactionList::parse` (ReactionManager.hpp:110-120).
    pub fn parse(parser: &mut Parser<'_>) -> Result<ReactionList, &'static str> {
        let mut result = ReactionList::default();
        let [has_reaction_types] = parser.parse_flags()?;
        if has_reaction_types {
            let len = parser.i32_()?;
            if len < 0 {
                return Err("Invalid vector size");
            }
            result.reaction_types.reserve(len as usize);
            for _ in 0..len {
                result.reaction_types.push(ReactionType::parse(parser)?);
            }
            result.hash = parser.i64_()?;
        }
        Ok(result)
    }
}

/// `ReactionManager::Effect` (ReactionManager.h:201-222).
#[derive(Clone, Debug, Default)]
pub struct Effect {
    pub(crate) id: MessageEffectId,
    pub(crate) emoji: String,
    pub(crate) static_icon_id: crate::files::FileId,
    pub(crate) effect_sticker_id: crate::files::FileId,
    pub(crate) effect_animation_id: crate::files::FileId,
    pub(crate) is_premium: bool,
}

impl Effect {
    /// `is_valid` (ReactionManager.h:209-211).
    pub fn is_valid(&self) -> bool {
        self.id.is_valid() && self.effect_sticker_id.is_valid()
    }

    /// `is_sticker` (ReactionManager.h:213-215).
    pub fn is_sticker(&self) -> bool {
        !self.effect_animation_id.is_valid()
    }

    /// `Effect::store` (ReactionManager.hpp:122-141).
    pub fn store(&self, env: &mut impl DialogPhotoStoreEnv, out: &mut Vec<u8>) {
        let has_static_icon = self.static_icon_id.is_valid();
        let has_effect_animation = self.effect_animation_id.is_valid();
        store_flags(
            out,
            [self.is_premium, has_static_icon, has_effect_animation],
        );
        self.id.store(out);
        crate::store_parse::store_string(out, &self.emoji);
        if has_static_icon {
            env.store_file_id(self.static_icon_id, out);
        }
        env.store_file_id(self.effect_sticker_id, out);
        if has_effect_animation {
            env.store_file_id(self.effect_animation_id, out);
        }
    }

    /// `Effect::parse` (ReactionManager.hpp:143-162).
    pub fn parse(
        env: &mut impl DialogPhotoStoreEnv,
        parser: &mut Parser<'_>,
    ) -> Result<Effect, &'static str> {
        let mut result = Effect::default();
        let [is_premium, has_static_icon, has_effect_animation] = parser.parse_flags()?;
        result.is_premium = is_premium;
        result.id = MessageEffectId::parse(parser)?;
        result.emoji = parser.string()?;
        if has_static_icon {
            result.static_icon_id = env.parse_file_id(parser)?;
        }
        result.effect_sticker_id = env.parse_file_id(parser)?;
        if has_effect_animation {
            result.effect_animation_id = env.parse_file_id(parser)?;
        }
        Ok(result)
    }
}

/// `ReactionManager::Effects` (ReactionManager.h:224-234).
#[derive(Clone, Debug, Default)]
pub struct Effects {
    pub(crate) hash: i32,
    pub(crate) are_being_reloaded: bool,
    pub(crate) effects: Vec<Effect>,
}

impl Effects {
    /// `Effects::store` (ReactionManager.hpp:164-174).
    pub fn store(&self, env: &mut impl DialogPhotoStoreEnv, out: &mut Vec<u8>) {
        let has_effects = !self.effects.is_empty();
        store_flags(out, [has_effects]);
        if has_effects {
            store_i32(out, self.effects.len() as i32);
            for effect in &self.effects {
                effect.store(env, out);
            }
            store_i32(out, self.hash);
        }
    }

    /// `Effects::parse` (ReactionManager.hpp:176-186).
    pub fn parse(
        env: &mut impl DialogPhotoStoreEnv,
        parser: &mut Parser<'_>,
    ) -> Result<Effects, &'static str> {
        let mut result = Effects::default();
        let [has_effects] = parser.parse_flags()?;
        if has_effects {
            let len = parser.i32_()?;
            if len < 0 {
                return Err("Invalid vector size");
            }
            result.effects.reserve(len as usize);
            for _ in 0..len {
                result.effects.push(Effect::parse(env, parser)?);
            }
            result.hash = parser.i32_()?;
        }
        Ok(result)
    }
}

/// `ReactionManager::ActiveEffects` (ReactionManager.h:236-249).
#[derive(Clone, Debug, Default)]
pub struct ActiveEffects {
    pub(crate) reaction_effects: Vec<i64>,
    pub(crate) sticker_effects: Vec<i64>,
}

impl ActiveEffects {
    /// `is_empty` (ReactionManager.h:240-242).
    pub fn is_empty(&self) -> bool {
        self.reaction_effects.is_empty() && self.sticker_effects.is_empty()
    }

    /// `ActiveEffects::store` (ReactionManager.hpp:188-202).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_reaction_effects = !self.reaction_effects.is_empty();
        let has_sticker_effects = !self.sticker_effects.is_empty();
        store_flags(out, [has_reaction_effects, has_sticker_effects]);
        if has_reaction_effects {
            crate::store_parse::store_i64_vec(out, &self.reaction_effects);
        }
        if has_sticker_effects {
            crate::store_parse::store_i64_vec(out, &self.sticker_effects);
        }
    }

    /// `ActiveEffects::parse` (ReactionManager.hpp:204-218).
    pub fn parse(parser: &mut Parser<'_>) -> Result<ActiveEffects, &'static str> {
        let mut result = ActiveEffects::default();
        let [has_reaction_effects, has_sticker_effects] = parser.parse_flags()?;
        if has_reaction_effects {
            result.reaction_effects = parse_i64_vec(parser)?;
        }
        if has_sticker_effects {
            result.sticker_effects = parse_i64_vec(parser)?;
        }
        Ok(result)
    }
}

/// `td::store(vector<int64>)` inline.
fn parse_i64_vec(parser: &mut Parser<'_>) -> Result<Vec<i64>, &'static str> {
    let len = parser.i32_()?;
    if len < 0 {
        return Err("Invalid vector size");
    }
    let mut result = Vec::with_capacity(len as usize);
    for _ in 0..len {
        result.push(parser.i64_()?);
    }
    Ok(result)
}

/// `ReactionManager::MAX_RECENT_REACTIONS` (ReactionManager.h:43).
pub const MAX_RECENT_REACTIONS: usize = 100;

/// `ReactionManager::MAX_TAG_TITLE_LENGTH` (ReactionManager.h:147).
const MAX_TAG_TITLE_LENGTH: usize = 12;

/// `ReactionManager::SavedReactionTag` (ReactionManager.h:149-172).
#[derive(Clone, Debug, Default)]
pub struct SavedReactionTag {
    pub(crate) reaction_type: ReactionType,
    pub(crate) hash: u64,
    pub(crate) title: String,
    pub(crate) count: i32,
}

impl SavedReactionTag {
    /// `SavedReactionTag(telegram_api::object_ptr<savedReactionTag>)`
    /// (ReactionManager.cpp:279-284).
    pub fn from_telegram(tag: &telegram_api::SavedReactionTag) -> SavedReactionTag {
        let reaction_type = tag
            .reaction
            .as_ref()
            .map(|reaction| ReactionType::from_telegram(Some(reaction)))
            .unwrap_or_default();
        SavedReactionTag {
            hash: reaction_type.get_hash(),
            reaction_type,
            title: tag.title.clone(),
            count: tag.count,
        }
    }

    /// `SavedReactionTag(reaction_type, title, count)`
    /// (ReactionManager.cpp:286-288).
    pub fn new(reaction_type: &ReactionType, title: &str, count: i32) -> SavedReactionTag {
        SavedReactionTag {
            hash: reaction_type.get_hash(),
            reaction_type: reaction_type.clone(),
            title: title.to_owned(),
            count,
        }
    }

    /// `is_valid` (ReactionManager.h:161-163).
    pub fn is_valid(&self) -> bool {
        !self.reaction_type.is_empty()
            && self.count >= 0
            && (self.count > 0 || !self.title.is_empty())
    }

    /// `get_saved_messages_tag_object` (ReactionManager.cpp:290-292).
    pub fn get_saved_messages_tag_object(&self) -> td_api::SavedMessagesTag {
        td_api::SavedMessagesTag {
            tag: self.reaction_type.get_reaction_type_object(),
            label: self.title.clone(),
            count: self.count,
        }
    }

    /// `SavedReactionTag::store` (ReactionManager.cpp:294-309).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_title = !self.title.is_empty();
        let has_count = self.count != 0;
        store_flags(out, [has_title, has_count]);
        self.reaction_type.store(out);
        if has_title {
            crate::store_parse::store_string(out, &self.title);
        }
        if has_count {
            store_i32(out, self.count);
        }
    }

    /// `SavedReactionTag::parse` (ReactionManager.cpp:311-327).
    pub fn parse(parser: &mut Parser<'_>) -> Result<SavedReactionTag, &'static str> {
        let mut result = SavedReactionTag::default();
        let [has_title, has_count] = parser.parse_flags()?;
        result.reaction_type = ReactionType::parse(parser)?;
        result.hash = result.reaction_type.get_hash();
        if has_title {
            result.title = parser.string()?;
        }
        if has_count {
            result.count = parser.i32_()?;
        }
        Ok(result)
    }
}

impl PartialEq for SavedReactionTag {
    /// `operator==` (ReactionManager.cpp:329-331).
    fn eq(&self, other: &Self) -> bool {
        self.reaction_type == other.reaction_type
            && self.title == other.title
            && self.count == other.count
    }
}

impl Eq for SavedReactionTag {}

impl Ord for SavedReactionTag {
    /// `operator<` (ReactionManager.cpp:337-342) — the higher count first,
    /// then the higher hash.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        if self.count != other.count {
            return other.count.cmp(&self.count);
        }
        other.hash.cmp(&self.hash)
    }
}

impl PartialOrd for SavedReactionTag {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl std::fmt::Display for SavedReactionTag {
    /// `operator<<` (ReactionManager.cpp:344-347).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "SavedMessagesTag{{{}({}) X {}}}",
            self.reaction_type, self.title, self.count
        )
    }
}

/// `ReactionManager::SavedReactionTags` (ReactionManager.h:181-199).
#[derive(Clone, Debug, Default)]
pub struct SavedReactionTags {
    pub(crate) tags: Vec<SavedReactionTag>,
    pub(crate) hash: i64,
    pub(crate) is_inited: bool,
}

impl SavedReactionTags {
    /// `get_saved_messages_tags_object` (ReactionManager.cpp:349-353).
    pub fn get_saved_messages_tags_object(&self) -> td_api::SavedMessagesTags {
        td_api::SavedMessagesTags {
            tags: self
                .tags
                .iter()
                .map(SavedReactionTag::get_saved_messages_tag_object)
                .collect(),
        }
    }

    /// `update_saved_messages_tags` (ReactionManager.cpp:355-399).
    pub fn update_saved_messages_tags(
        &mut self,
        old_tags: &[ReactionType],
        new_tags: &[ReactionType],
    ) -> bool {
        if !self.is_inited {
            return false;
        }
        let mut is_changed = false;
        for old_tag in old_tags {
            if !new_tags.contains(old_tag) {
                debug_assert!(!old_tag.is_empty(), "the C++ CHECKs the old tag");
                if let Some(index) = self
                    .tags
                    .iter()
                    .position(|tag| &tag.reaction_type == old_tag)
                {
                    self.tags[index].count -= 1;
                    if !self.tags[index].is_valid() {
                        self.tags.remove(index);
                    }
                    is_changed = true;
                }
            }
        }
        for new_tag in new_tags {
            if !old_tags.contains(new_tag) {
                debug_assert!(!new_tag.is_empty(), "the C++ CHECKs the new tag");
                is_changed = true;
                let mut is_found = false;
                for tag in &mut self.tags {
                    if &tag.reaction_type == new_tag {
                        tag.count += 1;
                        is_found = true;
                        break;
                    }
                }
                if !is_found {
                    self.tags.push(SavedReactionTag::new(new_tag, "", 1));
                }
            }
        }
        if is_changed {
            self.tags.sort();
            self.hash = self.calc_hash();
        }
        is_changed
    }

    /// `set_tag_title` (ReactionManager.cpp:401-423).
    pub fn set_tag_title(&mut self, reaction_type: &ReactionType, title: &str) -> bool {
        if !self.is_inited {
            return false;
        }
        if let Some(index) = self
            .tags
            .iter()
            .position(|tag| &tag.reaction_type == reaction_type)
        {
            if self.tags[index].title == title {
                return false;
            }
            self.tags[index].title = title.to_owned();
            if !self.tags[index].is_valid() {
                self.tags.remove(index);
            }
            self.hash = self.calc_hash();
            return true;
        }
        self.tags
            .push(SavedReactionTag::new(reaction_type, title, 0));
        self.tags.sort();
        self.hash = self.calc_hash();
        true
    }

    /// `calc_hash` (ReactionManager.cpp:425-435).
    pub fn calc_hash(&self) -> i64 {
        let mut numbers = Vec::with_capacity(self.tags.len() * 3);
        for tag in &self.tags {
            numbers.push(tag.hash);
            if !tag.title.is_empty() {
                numbers.push(crate::misc::get_md5_string_hash(&tag.title));
            }
            numbers.push(tag.count as i64 as u64);
        }
        crate::misc::get_vector_hash(&numbers)
    }

    /// `SavedReactionTags::store` (ReactionManager.cpp:437-443).
    pub fn store(&self, out: &mut Vec<u8>) {
        debug_assert!(self.is_inited, "the C++ store CHECKs is_inited");
        store_flags::<0>(out, []);
        store_i32(out, self.tags.len() as i32);
        for tag in &self.tags {
            tag.store(out);
        }
    }

    /// `SavedReactionTags::parse` (ReactionManager.cpp:445-452).
    pub fn parse(parser: &mut Parser<'_>) -> Result<SavedReactionTags, &'static str> {
        let mut result = SavedReactionTags::default();
        let [] = parser.parse_flags()?;
        let len = parser.i32_()?;
        if len < 0 {
            return Err("Invalid vector size");
        }
        result.tags.reserve(len as usize);
        for _ in 0..len {
            result.tags.push(SavedReactionTag::parse(parser)?);
        }
        result.hash = result.calc_hash();
        result.is_inited = true;
        Ok(result)
    }
}

/// `ReactionManager` (ReactionManager.h:34-349) — the collapsed actor.
#[derive(Default)]
pub struct ReactionManager {
    is_inited: bool,
    are_reactions_loaded_from_database: bool,
    are_all_tags_loaded_from_database: bool,
    are_message_effects_loaded_from_database: bool,

    pending_get_emoji_reaction_queries: Vec<(String, Promise<Option<td_api::EmojiReaction>>)>,

    reactions: Reactions,
    active_reaction_types: Vec<ReactionType>,

    reaction_lists: [ReactionList; MAX_REACTION_LIST_TYPE],

    all_tags: SavedReactionTags,
    topic_tags: HashMap<SavedMessagesTopicId, SavedReactionTags>,

    pending_get_all_saved_reaction_tags_queries: Vec<Promise<td_api::SavedMessagesTags>>,
    pending_get_topic_saved_reaction_tags_queries:
        HashMap<SavedMessagesTopicId, Vec<Promise<td_api::SavedMessagesTags>>>,

    message_effects: Effects,
    active_message_effects: ActiveEffects,

    pending_get_message_effect_queries:
        Vec<(MessageEffectId, Promise<Option<td_api::MessageEffect>>)>,

    default_paid_reaction_type: PaidReactionType,
}

impl std::fmt::Debug for ReactionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReactionManager")
            .field("is_inited", &self.is_inited)
            .field("reactions_loaded", &self.are_reactions_loaded_from_database)
            .field("reaction_count", &self.reactions.reactions.len())
            .field("active_reaction_count", &self.active_reaction_types.len())
            .field("recent_count", &self.reaction_lists[0].reaction_types.len())
            .field("top_count", &self.reaction_lists[1].reaction_types.len())
            .field(
                "default_tag_count",
                &self.reaction_lists[2].reaction_types.len(),
            )
            .field("tag_count", &self.all_tags.tags.len())
            .field("effect_count", &self.message_effects.effects.len())
            .field(
                "default_paid_reaction_type",
                &self.default_paid_reaction_type,
            )
            .finish()
    }
}

impl ReactionManager {
    pub fn new() -> ReactionManager {
        ReactionManager::default()
    }

    /// `init` (ReactionManager.cpp:467-485). The
    /// `stickers_manager_->init()` call rides the M.15 integration.
    pub fn init(&mut self, env: &mut impl ReactionManagerEnv) {
        if env.close_flag() {
            return;
        }
        if self.is_inited || !env.auth_is_authorized() || env.auth_is_bot() {
            return;
        }
        self.is_inited = true;

        self.load_active_reactions(env);
        self.load_active_message_effects(env);
        self.load_default_paid_reaction_type(env);

        if env.get_option_boolean("default_reaction_needs_sync", false) {
            self.send_set_default_reaction_query(env);
        }
    }

    /// `get_emoji_reaction_object` (ReactionManager.cpp:487-502) — the
    /// sticker objects come from the M.15 seam; a null sticker is the
    /// collapsed-line degraded object.
    pub fn get_emoji_reaction_object(
        &self,
        env: &impl ReactionManagerEnv,
        emoji: &str,
    ) -> Option<td_api::EmojiReaction> {
        for reaction in &self.reactions.reactions {
            if reaction.reaction_type.get_string() == emoji {
                let sticker = |file_id: crate::files::FileId| env.get_sticker_object(file_id);
                return Some(td_api::EmojiReaction {
                    emoji: reaction.reaction_type.get_string().to_owned(),
                    title: reaction.title.clone(),
                    is_active: reaction.is_active,
                    static_icon: sticker(reaction.static_icon),
                    appear_animation: sticker(reaction.appear_animation),
                    select_animation: sticker(reaction.select_animation),
                    activate_animation: sticker(reaction.activate_animation),
                    effect_animation: sticker(reaction.effect_animation),
                    around_animation: sticker(reaction.around_animation),
                    center_animation: sticker(reaction.center_animation),
                });
            }
        }
        None
    }

    /// `get_emoji_reaction` (ReactionManager.cpp:504-512).
    pub fn get_emoji_reaction(
        &mut self,
        env: &mut impl ReactionManagerEnv,
        emoji: &str,
        mut promise: Promise<Option<td_api::EmojiReaction>>,
    ) {
        self.load_reactions(env);
        if self.reactions.reactions.is_empty() && self.reactions.are_being_reloaded {
            self.pending_get_emoji_reaction_queries
                .push((emoji.to_owned(), promise));
            return;
        }
        promise.set_value(self.get_emoji_reaction_object(env, emoji));
    }

    /// `get_sorted_available_reactions` (ReactionManager.cpp:514-660).
    pub fn get_sorted_available_reactions(
        &mut self,
        env: &mut impl ReactionManagerEnv,
        available_reactions: crate::chat_reactions::ChatReactions,
        active_reactions: crate::chat_reactions::ChatReactions,
        mut row_size: i32,
        is_tag: bool,
        unavailability_reason: ReactionUnavailabilityReason,
    ) -> td_api::AvailableReactions {
        if is_tag {
            self.load_reaction_list(env, ReactionListType::DefaultTag);
        } else {
            self.load_reaction_list(env, ReactionListType::Recent);
        }
        self.load_reaction_list(env, ReactionListType::Top);

        if !(5..=25).contains(&row_size) {
            row_size = 8;
        }

        let is_premium = env.get_option_boolean("is_premium", false);
        let mut show_premium = is_premium || is_tag;
        let mut recent_reactions = Vec::new();
        let mut top_reactions = Vec::new();
        if is_tag {
            if is_premium {
                for tag in &self
                    .get_saved_reaction_tags(env, SavedMessagesTopicId::default())
                    .tags
                {
                    top_reactions.push(tag.reaction_type.clone());
                }
                for reaction_type in &self
                    .get_reaction_list(ReactionListType::DefaultTag)
                    .reaction_types
                {
                    if !top_reactions.contains(reaction_type) {
                        top_reactions.push(reaction_type.clone());
                    }
                }
                for reaction_type in &self.get_reaction_list(ReactionListType::Top).reaction_types {
                    if !top_reactions.contains(reaction_type) {
                        top_reactions.push(reaction_type.clone());
                    }
                }
            } else {
                top_reactions = self
                    .get_reaction_list(ReactionListType::DefaultTag)
                    .reaction_types
                    .clone();
            }
        } else {
            recent_reactions = self
                .get_reaction_list(ReactionListType::Recent)
                .reaction_types
                .clone();
            top_reactions = self
                .get_reaction_list(ReactionListType::Top)
                .reaction_types
                .clone();
        }
        if active_reactions.allow_all_custom() && active_reactions.allow_all_regular() {
            for reaction_type in &recent_reactions {
                if reaction_type.is_custom_reaction() {
                    show_premium = true;
                }
            }
            for reaction_type in &top_reactions {
                if reaction_type.is_custom_reaction() {
                    show_premium = true;
                }
            }
        }

        let mut all_available_reaction_types: std::collections::HashSet<ReactionType> =
            std::collections::HashSet::new();
        for reaction_type in available_reactions.reaction_types() {
            debug_assert!(!reaction_type.is_empty(), "the C++ CHECKs the type");
            all_available_reaction_types.insert(reaction_type.clone());
        }
        if available_reactions.paid_reactions_available()
            || available_reactions
                .reaction_types()
                .first()
                .is_some_and(ReactionType::is_paid_reaction)
        {
            all_available_reaction_types.insert(ReactionType::paid());
            top_reactions.insert(0, ReactionType::paid());
        }

        let mut top_reaction_objects: Vec<td_api::AvailableReaction> = Vec::new();
        let mut recent_reaction_objects: Vec<td_api::AvailableReaction> = Vec::new();
        let mut popular_reaction_objects: Vec<td_api::AvailableReaction> = Vec::new();
        let mut last_reaction_objects: Vec<td_api::AvailableReaction> = Vec::new();

        let mut added_custom_reaction_types: std::collections::HashSet<ReactionType> =
            std::collections::HashSet::new();
        let add_reactions = |reaction_objects: &mut Vec<td_api::AvailableReaction>,
                             reaction_types: &[ReactionType],
                             all_available: &mut std::collections::HashSet<ReactionType>,
                             added_custom: &mut std::collections::HashSet<ReactionType>,
                             available_all_custom: bool,
                             is_premium: bool| {
            for reaction_type in reaction_types {
                if all_available.remove(reaction_type) {
                    // add available reaction
                    if reaction_type.is_custom_reaction() {
                        added_custom.insert(reaction_type.clone());
                    }
                    reaction_objects.push(td_api::AvailableReaction {
                        r#type: reaction_type.get_reaction_type_object(),
                        needs_premium: is_tag && !is_premium,
                    });
                } else if reaction_type.is_custom_reaction()
                    && available_all_custom
                    && added_custom.insert(reaction_type.clone())
                {
                    // add implicitly available custom reaction
                    reaction_objects.push(td_api::AvailableReaction {
                        r#type: reaction_type.get_reaction_type_object(),
                        needs_premium: !is_premium,
                    });
                } else {
                    // skip the reaction
                }
            }
        };
        if show_premium {
            if !is_tag && top_reactions.len() > 2 * row_size as usize {
                top_reactions.truncate(2 * row_size as usize);
            }
            add_reactions(
                &mut top_reaction_objects,
                &top_reactions,
                &mut all_available_reaction_types,
                &mut added_custom_reaction_types,
                available_reactions.allow_all_custom(),
                is_premium,
            );
            add_reactions(
                &mut recent_reaction_objects,
                &recent_reactions,
                &mut all_available_reaction_types,
                &mut added_custom_reaction_types,
                available_reactions.allow_all_custom(),
                is_premium,
            );
        } else {
            add_reactions(
                &mut top_reaction_objects,
                &top_reactions,
                &mut all_available_reaction_types,
                &mut added_custom_reaction_types,
                available_reactions.allow_all_custom(),
                is_premium,
            );
        }
        add_reactions(
            &mut last_reaction_objects,
            &self.active_reaction_types,
            &mut all_available_reaction_types,
            &mut added_custom_reaction_types,
            available_reactions.allow_all_custom(),
            is_premium,
        );
        add_reactions(
            &mut last_reaction_objects,
            available_reactions.reaction_types(),
            &mut all_available_reaction_types,
            &mut added_custom_reaction_types,
            available_reactions.allow_all_custom(),
            is_premium,
        );

        if show_premium {
            if recent_reactions.is_empty() {
                popular_reaction_objects = last_reaction_objects;
            } else {
                let max_objects = 10 * row_size as usize;
                if recent_reaction_objects.len() + last_reaction_objects.len() > max_objects {
                    if last_reaction_objects.len() < max_objects {
                        recent_reaction_objects.truncate(max_objects - last_reaction_objects.len());
                    } else {
                        recent_reaction_objects.clear();
                    }
                }
                recent_reaction_objects.append(&mut last_reaction_objects);
            }
        } else {
            top_reaction_objects.append(&mut last_reaction_objects);
        }

        debug_assert!(
            all_available_reaction_types.is_empty(),
            "the C++ CHECKs every available reaction was emitted"
        );

        let reason = match unavailability_reason {
            ReactionUnavailabilityReason::None => None,
            ReactionUnavailabilityReason::AnonymousAdministrator => {
                Some(td_api::ReactionUnavailabilityReason::reactionUnavailabilityReasonAnonymousAdministrator(
                    td_api::reactionUnavailabilityReasonAnonymousAdministrator {},
                ))
            }
            ReactionUnavailabilityReason::Guest => {
                Some(td_api::ReactionUnavailabilityReason::reactionUnavailabilityReasonGuest(
                    td_api::reactionUnavailabilityReasonGuest {},
                ))
            }
            ReactionUnavailabilityReason::Restricted => {
                Some(td_api::ReactionUnavailabilityReason::reactionUnavailabilityReasonRestricted(
                    td_api::reactionUnavailabilityReasonRestricted {},
                ))
            }
        };

        td_api::AvailableReactions {
            top_reactions: top_reaction_objects,
            recent_reactions: recent_reaction_objects,
            popular_reactions: popular_reaction_objects,
            allow_custom_emoji: available_reactions.allow_all_custom(),
            are_tags: is_tag,
            unavailability_reason: reason,
        }
    }

    /// `get_available_reactions` (ReactionManager.cpp:662-668) — the
    /// synchronous builder behind the getStoryAvailableReactions arm.
    pub fn get_available_reactions(
        &mut self,
        env: &mut impl ReactionManagerEnv,
        row_size: i32,
    ) -> td_api::AvailableReactions {
        let available_reactions = crate::chat_reactions::ChatReactions {
            reaction_types: self.active_reaction_types.clone(),
            allow_all_custom: true,
            ..crate::chat_reactions::ChatReactions::default()
        };
        self.get_sorted_available_reactions(
            env,
            available_reactions,
            crate::chat_reactions::ChatReactions::all(true, true),
            row_size,
            false,
            ReactionUnavailabilityReason::None,
        )
    }

    /// `add_recent_reaction` (ReactionManager.cpp:670-683).
    pub fn add_recent_reaction(
        &mut self,
        env: &mut impl ReactionManagerEnv,
        reaction_type: &ReactionType,
    ) {
        self.load_reaction_list(env, ReactionListType::Recent);

        let recent_reactions = self.get_reaction_list_mut(ReactionListType::Recent);
        let reactions = &mut recent_reactions.reaction_types;
        if reactions.first() == Some(reaction_type) {
            return;
        }
        debug_assert!(
            !reaction_type.is_paid_reaction(),
            "the C++ CHECKs !is_paid_reaction"
        );

        add_to_top(reactions, MAX_RECENT_REACTIONS, reaction_type.clone());

        recent_reactions.hash = get_reaction_types_hash(reactions);
    }

    /// `clear_recent_reactions` (ReactionManager.cpp:685-697).
    pub fn clear_recent_reactions(
        &mut self,
        env: &mut impl ReactionManagerEnv,
        promise: Promise<()>,
    ) {
        self.load_reaction_list(env, ReactionListType::Recent);

        let recent_reactions = self.get_reaction_list_mut(ReactionListType::Recent);
        if recent_reactions.reaction_types.is_empty() {
            let mut promise = promise;
            promise.set_value(());
            return;
        }

        recent_reactions.hash = 0;
        recent_reactions.reaction_types.clear();

        env.send_clear_recent_reactions_query(promise);
    }

    /// `get_default_tag_reactions` (ReactionManager.cpp:699-703).
    pub fn get_default_tag_reactions(
        &mut self,
        env: &mut impl ReactionManagerEnv,
    ) -> Vec<ReactionType> {
        self.load_reaction_list(env, ReactionListType::DefaultTag);

        self.get_reaction_list(ReactionListType::DefaultTag)
            .reaction_types
            .clone()
    }

    /// `reload_reactions` (ReactionManager.cpp:705-713).
    fn reload_reactions(&mut self, env: &mut impl ReactionManagerEnv) {
        if env.close_flag() || self.reactions.are_being_reloaded {
            return;
        }
        debug_assert!(!env.auth_is_bot(), "the C++ CHECKs !is_bot");
        self.reactions.are_being_reloaded = true;
        // the load must run after the flag is set to avoid recursion
        self.load_reactions(env);
        env.send_get_available_reactions_query(self.reactions.hash);
    }

    /// `reload_reaction_list` (ReactionManager.cpp:715-728).
    fn reload_reaction_list(
        &mut self,
        env: &mut impl ReactionManagerEnv,
        reaction_list_type: ReactionListType,
    ) {
        if env.close_flag() {
            return;
        }
        let reaction_list = self.get_reaction_list_mut(reaction_list_type);
        if reaction_list.is_being_reloaded {
            return;
        }
        debug_assert!(!env.auth_is_bot(), "the C++ CHECKs !is_bot");
        reaction_list.is_being_reloaded = true;
        // the load must run after the flag is set to avoid recursion
        self.load_reaction_list(env, reaction_list_type);
        let hash = self.get_reaction_list(reaction_list_type).hash;
        env.send_get_reaction_list_query(reaction_list_type, hash);
    }

    /// `get_update_active_emoji_reactions_object`
    /// (ReactionManager.cpp:730-734).
    pub fn get_update_active_emoji_reactions_object(&self) -> td_api::Update {
        td_api::Update::updateActiveEmojiReactions(td_api::updateActiveEmojiReactions {
            emojis: self
                .active_reaction_types
                .iter()
                .map(|reaction_type| reaction_type.get_string().to_owned())
                .collect(),
        })
    }

    /// `get_reaction_list` (ReactionManager.cpp:736-738).
    fn get_reaction_list(&self, reaction_list_type: ReactionListType) -> &ReactionList {
        &self.reaction_lists[reaction_list_type.as_index()]
    }

    fn get_reaction_list_mut(&mut self, reaction_list_type: ReactionListType) -> &mut ReactionList {
        &mut self.reaction_lists[reaction_list_type.as_index()]
    }

    /// `save_active_reactions` (ReactionManager.cpp:740-743) — the
    /// `log_event_store(vector<ReactionType>)` framing: version, length,
    /// elements.
    fn save_active_reactions(&self, env: &mut impl ReactionManagerEnv) {
        let mut out = Vec::new();
        store_log_event_version(&mut out);
        store_i32(&mut out, self.active_reaction_types.len() as i32);
        for reaction_type in &self.active_reaction_types {
            reaction_type.store(&mut out);
        }
        env.binlog_pmc_set_raw("active_reactions", &out);
    }

    /// `save_reactions` (ReactionManager.cpp:745-749) — the caller is
    /// `on_get_available_reactions`, the M.15-gated wire intake; the
    /// database-load path sets the flag inline.
    #[allow(dead_code)]
    fn save_reactions(&mut self, env: &mut impl ReactionManagerEnv) {
        let mut out = Vec::new();
        store_log_event_version(&mut out);
        self.reactions.store(env, &mut out);
        env.binlog_pmc_set_raw("reactions", &out);
        self.are_reactions_loaded_from_database = true;
    }

    /// `save_reaction_list` (ReactionManager.cpp:751-757).
    fn save_reaction_list(
        &mut self,
        env: &mut impl ReactionManagerEnv,
        reaction_list_type: ReactionListType,
    ) {
        let mut out = Vec::new();
        store_log_event_version(&mut out);
        let reaction_list = self.get_reaction_list_mut(reaction_list_type);
        reaction_list.store(&mut out);
        env.binlog_pmc_set_raw(
            get_reaction_list_type_database_key(reaction_list_type),
            &out,
        );
        reaction_list.is_loaded_from_database = true;
    }

    /// `load_active_reactions` (ReactionManager.cpp:759-778).
    fn load_active_reactions(&mut self, env: &mut impl ReactionManagerEnv) {
        let active_reaction_types = env.binlog_pmc_get_raw("active_reactions");
        if active_reaction_types.is_empty() {
            return self.reload_reactions(env);
        }

        let parse = || -> Result<Vec<ReactionType>, &'static str> {
            let mut parser = Parser::new_log_event(&active_reaction_types)?;
            let len = parser.i32_()?;
            if len < 0 {
                return Err("Invalid vector size");
            }
            let mut reaction_types = Vec::with_capacity(len as usize);
            for _ in 0..len {
                reaction_types.push(ReactionType::parse(&mut parser)?);
            }
            parser.fetch_end()?;
            Ok(reaction_types)
        };
        match parse() {
            Ok(reaction_types) => {
                self.active_reaction_types = reaction_types;
            }
            Err(error) => {
                tdutils::log_error!("Can't load active reactions: {error}");
                self.active_reaction_types.clear();
                return self.reload_reactions(env);
            }
        }

        env.set_active_reactions(&self.active_reaction_types);

        let update = self.get_update_active_emoji_reactions_object();
        env.send_update(update);
    }

    /// `load_reactions` (ReactionManager.cpp:780-810).
    fn load_reactions(&mut self, env: &mut impl ReactionManagerEnv) {
        if self.are_reactions_loaded_from_database {
            return;
        }
        self.are_reactions_loaded_from_database = true;

        let reactions = env.binlog_pmc_get_raw("reactions");
        if reactions.is_empty() {
            return self.reload_reactions(env);
        }

        let new_reactions = match parse_reactions_record(env, &reactions) {
            Ok(new_reactions) => new_reactions,
            Err(error) => {
                tdutils::log_error!("Can't load available reactions: {error}");
                return self.reload_reactions(env);
            }
        };
        let mut new_reactions = new_reactions;
        new_reactions.are_being_reloaded = self.reactions.are_being_reloaded;
        for reaction in &new_reactions.reactions {
            if !reaction.is_valid() {
                tdutils::log_error!("Loaded invalid reaction");
                return self.reload_reactions(env);
            }
        }
        self.reactions = new_reactions;

        self.update_active_reactions(env);
    }

    /// `load_reaction_list` (ReactionManager.cpp:812-833).
    fn load_reaction_list(
        &mut self,
        env: &mut impl ReactionManagerEnv,
        reaction_list_type: ReactionListType,
    ) {
        if self
            .get_reaction_list(reaction_list_type)
            .is_loaded_from_database
        {
            return;
        }
        self.get_reaction_list_mut(reaction_list_type)
            .is_loaded_from_database = true;

        let reactions_str =
            env.binlog_pmc_get_raw(get_reaction_list_type_database_key(reaction_list_type));
        if reactions_str.is_empty() {
            return self.reload_reaction_list(env, reaction_list_type);
        }

        let parse = || -> Result<ReactionList, &'static str> {
            let mut parser = Parser::new_log_event(&reactions_str)?;
            let reaction_list = ReactionList::parse(&mut parser)?;
            parser.fetch_end()?;
            Ok(reaction_list)
        };
        match parse() {
            Ok(reaction_list) => {
                *self.get_reaction_list_mut(reaction_list_type) = reaction_list;
            }
            Err(error) => {
                tdutils::log_error!("Can't load the reaction list: {error}");
                *self.get_reaction_list_mut(reaction_list_type) = ReactionList {
                    is_loaded_from_database: true,
                    is_being_reloaded: false,
                    hash: 0,
                    reaction_types: Vec::new(),
                };
                self.reload_reaction_list(env, reaction_list_type)
            }
        }
    }

    /// `update_active_reactions` (ReactionManager.cpp:835-852).
    fn update_active_reactions(&mut self, env: &mut impl ReactionManagerEnv) {
        let active_reaction_types: Vec<ReactionType> = self
            .reactions
            .reactions
            .iter()
            .filter(|reaction| reaction.is_active)
            .map(|reaction| reaction.reaction_type.clone())
            .collect();
        if active_reaction_types == self.active_reaction_types {
            return;
        }
        self.active_reaction_types = active_reaction_types.clone();

        self.save_active_reactions(env);

        let update = self.get_update_active_emoji_reactions_object();
        env.send_update(update);

        env.set_active_reactions(&active_reaction_types);
    }

    /// `on_get_reaction_list` (ReactionManager.cpp:935-969) — the wire
    /// intake is net-independent and fully ported.
    pub fn on_get_reaction_list(
        &mut self,
        env: &mut impl ReactionManagerEnv,
        reaction_list_type: ReactionListType,
        reactions: Option<telegram_api::messages_Reactions>,
    ) {
        let reaction_list = self.get_reaction_list_mut(reaction_list_type);
        debug_assert!(
            reaction_list.is_being_reloaded,
            "the C++ CHECKs is_being_reloaded"
        );
        reaction_list.is_being_reloaded = false;

        let Some(telegram_api::messages_Reactions::messages_reactions(reactions)) = reactions
        else {
            // failed to get reactions (or the NotModified constructor)
            return;
        };

        let new_reaction_types = ReactionType::get_reaction_types(&reactions.reactions);
        let reaction_list = self.get_reaction_list(reaction_list_type);
        if new_reaction_types == reaction_list.reaction_types
            && reaction_list.hash == reactions.hash
        {
            return;
        }
        let reaction_list = self.get_reaction_list_mut(reaction_list_type);
        reaction_list.reaction_types = new_reaction_types;
        reaction_list.hash = reactions.hash;

        let expected_hash = get_reaction_types_hash(&reaction_list.reaction_types);
        if reaction_list.hash != expected_hash {
            tdutils::log_error!("Receive unexpected reaction list hash");
        }

        self.save_reaction_list(env, reaction_list_type);
    }

    /// `is_active_reaction` (ReactionManager.cpp:971-973).
    pub fn is_active_reaction(&self, reaction_type: &ReactionType) -> bool {
        self.active_reaction_types.contains(reaction_type)
    }

    /// `set_default_reaction` (ReactionManager.cpp:975-994).
    pub fn set_default_reaction(
        &mut self,
        env: &mut impl ReactionManagerEnv,
        reaction_type: ReactionType,
        mut promise: Promise<()>,
    ) {
        if reaction_type.is_empty() {
            promise.set_error(tdutils::status::Status::error(
                400,
                "Default reaction must be non-empty",
            ));
            return;
        }
        if reaction_type.is_paid_reaction() {
            promise.set_error(tdutils::status::Status::error(
                400,
                "Can't set paid reaction as default",
            ));
            return;
        }
        if !reaction_type.is_custom_reaction() && !self.is_active_reaction(&reaction_type) {
            promise.set_error(tdutils::status::Status::error(
                400,
                "Can't set inactive reaction as default",
            ));
            return;
        }

        if env.get_option_string("default_reaction", "-") != reaction_type.get_string() {
            env.set_option_string("default_reaction", reaction_type.get_string());
            if !env.get_option_boolean("default_reaction_needs_sync", false) {
                env.set_option_boolean("default_reaction_needs_sync", true);
                self.send_set_default_reaction_query(env);
            }
        }
        promise.set_value(());
    }

    /// `send_set_default_reaction_query` (ReactionManager.cpp:996-999).
    fn send_set_default_reaction_query(&mut self, env: &mut impl ReactionManagerEnv) {
        let reaction_type = ReactionType::new(env.get_option_string("default_reaction", "-"));
        env.send_set_default_reaction_query(&reaction_type);
    }

    /// `load_all_saved_reaction_tags_from_database`
    /// (ReactionManager.cpp:1001-1017).
    fn load_all_saved_reaction_tags_from_database(&mut self, env: &mut impl ReactionManagerEnv) {
        if self.are_all_tags_loaded_from_database || self.all_tags.is_inited {
            return;
        }
        self.are_all_tags_loaded_from_database = true;

        let value = env.sqlite_pmc_get_raw(
            &self.get_saved_messages_tags_database_key(SavedMessagesTopicId::default()),
        );
        if !value.is_empty() {
            let parse = || -> Result<SavedReactionTags, &'static str> {
                let mut parser = Parser::new_log_event(&value)?;
                let tags = SavedReactionTags::parse(&mut parser)?;
                parser.fetch_end()?;
                Ok(tags)
            };
            match parse() {
                Ok(tags) => {
                    self.all_tags = tags;
                    self.send_update_saved_messages_tags(
                        env,
                        SavedMessagesTopicId::default(),
                        true,
                    );
                }
                Err(_) => {
                    tdutils::log_error!("Failed to load all tags from database");
                    self.all_tags = SavedReactionTags::default();
                }
            }
        }
        self.reload_saved_messages_tags(env, SavedMessagesTopicId::default());
    }

    /// `load_saved_reaction_tags_from_database`
    /// (ReactionManager.cpp:1019-1037).
    fn load_saved_reaction_tags_from_database(
        &mut self,
        env: &mut impl ReactionManagerEnv,
        saved_messages_topic_id: SavedMessagesTopicId,
        tags: &mut SavedReactionTags,
    ) {
        let value = env.sqlite_pmc_get_raw(
            &self.get_saved_messages_tags_database_key(saved_messages_topic_id),
        );
        if value.is_empty() {
            return;
        }
        let parse = || -> Result<SavedReactionTags, &'static str> {
            let mut parser = Parser::new_log_event(&value)?;
            let parsed = SavedReactionTags::parse(&mut parser)?;
            parser.fetch_end()?;
            Ok(parsed)
        };
        match parse() {
            Ok(parsed) => {
                *tags = parsed;
            }
            Err(_) => {
                tdutils::log_error!("Failed to load all tags from database");
                *tags = SavedReactionTags::default();
                return;
            }
        }

        self.send_update_saved_messages_tags(env, saved_messages_topic_id, true);
        self.reload_saved_messages_tags(env, saved_messages_topic_id);
    }

    /// `get_saved_reaction_tags` (ReactionManager.cpp:1039-1051) — the
    /// C++ `unique_ptr == nullptr` check is the absent map entry.
    fn get_saved_reaction_tags(
        &mut self,
        env: &mut impl ReactionManagerEnv,
        saved_messages_topic_id: SavedMessagesTopicId,
    ) -> &SavedReactionTags {
        if saved_messages_topic_id == SavedMessagesTopicId::default() {
            self.load_all_saved_reaction_tags_from_database(env);
            return &self.all_tags;
        }
        if !self.topic_tags.contains_key(&saved_messages_topic_id) {
            let mut tags = SavedReactionTags::default();
            self.load_saved_reaction_tags_from_database(env, saved_messages_topic_id, &mut tags);
            self.topic_tags.insert(saved_messages_topic_id, tags);
        }
        self.topic_tags.get(&saved_messages_topic_id).unwrap()
    }

    /// `get_saved_messages_tags` (ReactionManager.cpp:1053-1063).
    pub fn get_saved_messages_tags(
        &mut self,
        env: &mut impl ReactionManagerEnv,
        saved_messages_topic_id: SavedMessagesTopicId,
        mut promise: Promise<td_api::SavedMessagesTags>,
    ) {
        if !saved_messages_topic_id.is_valid()
            && saved_messages_topic_id != SavedMessagesTopicId::default()
        {
            promise.set_error(tdutils::status::Status::error(
                400,
                "Invalid Saved Messages topic specified",
            ));
            return;
        }
        {
            let tags = self.get_saved_reaction_tags(env, saved_messages_topic_id);
            if tags.is_inited {
                promise.set_value(tags.get_saved_messages_tags_object());
                return;
            }
        }
        self.reload_saved_messages_tags_with_promise(env, saved_messages_topic_id, promise);
    }

    /// `reload_saved_messages_tags` (ReactionManager.cpp:1065-1082) — the
    /// first promise parks and sends the (blocked) query, the rest park
    /// behind it.
    fn reload_saved_messages_tags_with_promise(
        &mut self,
        env: &mut impl ReactionManagerEnv,
        saved_messages_topic_id: SavedMessagesTopicId,
        promise: Promise<td_api::SavedMessagesTags>,
    ) {
        let is_first = if saved_messages_topic_id == SavedMessagesTopicId::default() {
            let is_first = self.pending_get_all_saved_reaction_tags_queries.is_empty();
            self.pending_get_all_saved_reaction_tags_queries
                .push(promise);
            is_first
        } else {
            let pending = self
                .pending_get_topic_saved_reaction_tags_queries
                .entry(saved_messages_topic_id)
                .or_default();
            let is_first = pending.is_empty();
            pending.push(promise);
            is_first
        };
        if !is_first {
            return;
        }
        let hash = self
            .get_saved_reaction_tags(env, saved_messages_topic_id)
            .hash;
        env.send_get_saved_reaction_tags_query(saved_messages_topic_id, hash);
    }

    /// `reload_saved_messages_tags` without a request promise — the
    /// implicit reload of the database-load paths.
    fn reload_saved_messages_tags(
        &mut self,
        env: &mut impl ReactionManagerEnv,
        saved_messages_topic_id: SavedMessagesTopicId,
    ) {
        self.reload_saved_messages_tags_with_promise(
            env,
            saved_messages_topic_id,
            tdutils::promise::PromiseCreator::lambda_result(|_| {}),
        );
    }

    /// `on_get_saved_messages_tags` (ReactionManager.cpp:1084-1150) — the
    /// wire intake is net-independent and fully ported.
    pub fn on_get_saved_messages_tags(
        &mut self,
        env: &mut impl ReactionManagerEnv,
        saved_messages_topic_id: SavedMessagesTopicId,
        r_tags: Result<telegram_api::messages_SavedReactionTags, tdutils::status::Status>,
    ) {
        let mut promises = if saved_messages_topic_id == SavedMessagesTopicId::default() {
            std::mem::take(&mut self.pending_get_all_saved_reaction_tags_queries)
        } else {
            self.pending_get_topic_saved_reaction_tags_queries
                .remove(&saved_messages_topic_id)
                .unwrap_or_default()
        };
        debug_assert!(!promises.is_empty(), "the C++ CHECKs the pending set");

        match r_tags {
            Err(status) => {
                for mut promise in promises {
                    promise.set_error(status.clone());
                }
            }
            Ok(tags) => {
                let (need_send_update, object) = {
                    let reaction_tags =
                        self.get_saved_reaction_tags_mut(env, saved_messages_topic_id);
                    let mut need_send_update = false;
                    match tags {
                        telegram_api::messages_SavedReactionTags::messages_savedReactionTagsNotModified(_) => {
                            if !reaction_tags.is_inited {
                                // the list of tags is empty
                                debug_assert!(reaction_tags.tags.is_empty());
                                reaction_tags.is_inited = true;
                                need_send_update = true;
                            }
                        }
                        telegram_api::messages_SavedReactionTags::messages_savedReactionTags(tags) => {
                            let mut saved_reaction_tags: Vec<SavedReactionTag> = tags
                                .tags
                                .iter()
                                .map(SavedReactionTag::from_telegram)
                                .filter(|tag| {
                                    let is_valid = tag.is_valid();
                                    if !is_valid {
                                        tdutils::log_error!("Receive an invalid Saved Messages tag");
                                    }
                                    is_valid
                                })
                                .collect();
                            saved_reaction_tags.sort();
                            reaction_tags.hash = tags.hash;
                            if saved_reaction_tags != reaction_tags.tags {
                                reaction_tags.tags = saved_reaction_tags;
                                need_send_update = true;
                            }
                            if reaction_tags.hash != reaction_tags.calc_hash() {
                                tdutils::log_error!("Receive unexpected Saved Messages tag hash");
                            }
                            reaction_tags.is_inited = true;
                        }
                    }
                    (
                        need_send_update,
                        reaction_tags.get_saved_messages_tags_object(),
                    )
                };

                if need_send_update {
                    self.send_update_saved_messages_tags(env, saved_messages_topic_id, false);
                }
                for promise in promises.iter_mut() {
                    promise.set_value(object.clone());
                }
            }
        }
    }

    /// The mutable view [`ReactionManager::get_saved_reaction_tags`]
    /// hands out for the intake.
    fn get_saved_reaction_tags_mut(
        &mut self,
        env: &mut impl ReactionManagerEnv,
        saved_messages_topic_id: SavedMessagesTopicId,
    ) -> &mut SavedReactionTags {
        if saved_messages_topic_id == SavedMessagesTopicId::default() {
            self.load_all_saved_reaction_tags_from_database(env);
            &mut self.all_tags
        } else {
            self.load_topic_tags_if_needed(env, saved_messages_topic_id);
            self.topic_tags.get_mut(&saved_messages_topic_id).unwrap()
        }
    }

    /// The `topic_tags_[id] = make_unique<SavedReactionTags>()` +
    /// database-load path of `get_saved_reaction_tags`.
    fn load_topic_tags_if_needed(
        &mut self,
        env: &mut impl ReactionManagerEnv,
        saved_messages_topic_id: SavedMessagesTopicId,
    ) {
        let is_new = !self.topic_tags.contains_key(&saved_messages_topic_id);
        if is_new {
            let mut tags = SavedReactionTags::default();
            self.load_saved_reaction_tags_from_database(env, saved_messages_topic_id, &mut tags);
            self.topic_tags.insert(saved_messages_topic_id, tags);
        }
    }

    /// `get_saved_messages_tags_database_key` (ReactionManager.cpp:1152-1154).
    fn get_saved_messages_tags_database_key(
        &self,
        saved_messages_topic_id: SavedMessagesTopicId,
    ) -> Vec<u8> {
        format!(
            "saved_messages_tags{}",
            saved_messages_topic_id.get_unique_id()
        )
        .into_bytes()
    }

    /// `get_update_saved_messages_tags_object`
    /// (ReactionManager.cpp:1156-1162) — the topic id field is the
    /// SavedMessagesManager object collapsed to the raw unique id (0 for
    /// the default topic, the C++ non-bot answer without topic lists).
    fn get_update_saved_messages_tags_object(
        &self,
        saved_messages_topic_id: SavedMessagesTopicId,
        tags: &SavedReactionTags,
    ) -> td_api::Update {
        td_api::Update::updateSavedMessagesTags(td_api::updateSavedMessagesTags {
            saved_messages_topic_id: if saved_messages_topic_id == SavedMessagesTopicId::default() {
                0
            } else {
                saved_messages_topic_id.get_unique_id()
            },
            tags: Some(tags.get_saved_messages_tags_object()),
        })
    }

    /// `send_update_saved_messages_tags` (ReactionManager.cpp:1164-1171).
    fn send_update_saved_messages_tags(
        &mut self,
        env: &mut impl ReactionManagerEnv,
        saved_messages_topic_id: SavedMessagesTopicId,
        from_database: bool,
    ) {
        let tags = if saved_messages_topic_id == SavedMessagesTopicId::default() {
            &self.all_tags
        } else {
            self.topic_tags.get(&saved_messages_topic_id).unwrap()
        };
        let update = self.get_update_saved_messages_tags_object(saved_messages_topic_id, tags);
        env.send_update(update);
        if !from_database {
            env.sqlite_pmc_set_raw(
                &self.get_saved_messages_tags_database_key(saved_messages_topic_id),
                {
                    let mut out = Vec::new();
                    store_log_event_version(&mut out);
                    tags.store(&mut out);
                    out
                }
                .as_slice(),
            );
        }
    }

    /// `set_saved_messages_tag_title` (ReactionManager.cpp:1200-1223).
    pub fn set_saved_messages_tag_title(
        &mut self,
        env: &mut impl ReactionManagerEnv,
        reaction_type: ReactionType,
        title: &str,
        mut promise: Promise<()>,
    ) {
        if reaction_type.is_empty() {
            promise.set_error(tdutils::status::Status::error(
                400,
                "Reaction type must be non-empty",
            ));
            return;
        }
        if reaction_type.is_paid_reaction() {
            promise.set_error(tdutils::status::Status::error(
                400,
                "Invalid reaction specified",
            ));
            return;
        }
        let title = crate::misc::clean_name(title, MAX_TAG_TITLE_LENGTH);

        let is_changed = {
            let all_tags = self.get_saved_reaction_tags_mut(env, SavedMessagesTopicId::default());
            all_tags.set_tag_title(&reaction_type, &title)
        };
        if is_changed {
            self.send_update_saved_messages_tags(env, SavedMessagesTopicId::default(), false);
        }

        // the UpdateSavedReactionTagQuery promise reloads the tags on
        // success (on_update_saved_reaction_tags); the query itself is
        // [blocked: net]
        env.send_update_saved_reaction_tag_query(&reaction_type, &title);
    }

    /// `on_update_saved_reaction_tags` (ReactionManager.cpp:1173-1180) —
    /// the reload tail of the tag update query: the request promise
    /// completes once the reload lands.
    pub fn on_update_saved_reaction_tags(
        &mut self,
        env: &mut impl ReactionManagerEnv,
        mut promise: Promise<()>,
    ) {
        self.reload_saved_messages_tags_with_promise(
            env,
            SavedMessagesTopicId::default(),
            tdutils::promise::PromiseCreator::lambda_result(
                move |result: tdutils::status::Result<td_api::SavedMessagesTags>| {
                    // the C++ lambda drops the tags and answers ok; an
                    // error forwards
                    match result {
                        tdutils::status::Result::Ok(_) => promise.set_value(()),
                        tdutils::status::Result::Err(status) => promise.set_error(status),
                    }
                },
            ),
        );
    }

    /// `get_message_effect_object(Effect)` (ReactionManager.cpp:1225-1238)
    /// — the sticker objects come from the M.15 seam.
    pub fn get_message_effect_object(
        &self,
        env: &impl ReactionManagerEnv,
        effect: &Effect,
    ) -> td_api::MessageEffect {
        // the pinned scheme's emoji-reaction effect type carries the two
        // animations (the newer local tree swaps the first for a sticker;
        // the effect_sticker_id row fills the select slot here)
        let r#type = if effect.is_sticker() {
            td_api::MessageEffectType::messageEffectTypePremiumSticker(
                td_api::messageEffectTypePremiumSticker {
                    sticker: env.get_sticker_object(effect.effect_sticker_id),
                },
            )
        } else {
            td_api::MessageEffectType::messageEffectTypeEmojiReaction(
                td_api::messageEffectTypeEmojiReaction {
                    select_animation: env.get_sticker_object(effect.effect_sticker_id),
                    effect_animation: env.get_sticker_object(effect.effect_animation_id),
                },
            )
        };
        td_api::MessageEffect {
            id: effect.id.get(),
            static_icon: env.get_sticker_object(effect.static_icon_id),
            emoji: effect.emoji.clone(),
            is_premium: effect.is_premium,
            r#type: Some(r#type),
        }
    }

    /// `get_message_effect_object(MessageEffectId)`
    /// (ReactionManager.cpp:1240-1247).
    pub fn get_message_effect_by_id(
        &self,
        env: &impl ReactionManagerEnv,
        effect_id: MessageEffectId,
    ) -> Option<td_api::MessageEffect> {
        for effect in &self.message_effects.effects {
            if effect.id == effect_id {
                return Some(self.get_message_effect_object(env, effect));
            }
        }
        None
    }

    /// `get_update_available_message_effects_object`
    /// (ReactionManager.cpp:1249-1257).
    pub fn get_update_available_message_effects_object(&self) -> td_api::Update {
        td_api::Update::updateAvailableMessageEffects(td_api::updateAvailableMessageEffects {
            reaction_effect_ids: self.active_message_effects.reaction_effects.clone(),
            sticker_effect_ids: self.active_message_effects.sticker_effects.clone(),
        })
    }

    /// `load_message_effects` (ReactionManager.cpp:1274-1304) — the
    /// `on_get_message_effects` intake is the M.15-gated wire half.
    fn load_message_effects(&mut self, env: &mut impl ReactionManagerEnv) {
        if self.are_message_effects_loaded_from_database {
            return;
        }
        self.are_message_effects_loaded_from_database = true;

        let message_effects = env.binlog_pmc_get_raw("message_effects");
        if message_effects.is_empty() {
            return self.reload_message_effects(env);
        }

        let new_effects = match parse_effects_record(env, &message_effects) {
            Ok(new_effects) => new_effects,
            Err(error) => {
                tdutils::log_error!("Can't load message effects: {error}");
                return self.reload_message_effects(env);
            }
        };
        let mut new_effects = new_effects;
        new_effects.are_being_reloaded = self.message_effects.are_being_reloaded;
        for effect in &new_effects.effects {
            if !effect.is_valid() {
                tdutils::log_error!("Loaded invalid message effect");
                return self.reload_message_effects(env);
            }
        }
        self.message_effects = new_effects;

        self.update_active_message_effects(env);
    }

    /// `reload_message_effects` (ReactionManager.cpp:1259-1272).
    fn reload_message_effects(&mut self, env: &mut impl ReactionManagerEnv) {
        if env.close_flag() || self.message_effects.are_being_reloaded {
            return;
        }
        debug_assert!(!env.auth_is_bot(), "the C++ CHECKs !is_bot");
        self.message_effects.are_being_reloaded = true;
        self.load_message_effects(env);
        env.send_get_available_effects_query(self.message_effects.hash);
    }

    /// `save_message_effects` (ReactionManager.cpp:1306-1310) — the
    /// caller is `on_get_message_effects`, the M.15-gated wire intake.
    #[allow(dead_code)]
    fn save_message_effects(&mut self, env: &mut impl ReactionManagerEnv) {
        let mut out = Vec::new();
        store_log_event_version(&mut out);
        self.message_effects.store(env, &mut out);
        env.binlog_pmc_set_raw("message_effects", &out);
        self.are_message_effects_loaded_from_database = true;
    }

    /// `save_active_message_effects` (ReactionManager.cpp:1421-1426).
    fn save_active_message_effects(&mut self, env: &mut impl ReactionManagerEnv) {
        let mut out = Vec::new();
        store_log_event_version(&mut out);
        self.active_message_effects.store(&mut out);
        env.binlog_pmc_set_raw("active_message_effects", &out);
    }

    /// `load_active_message_effects` (ReactionManager.cpp:1428-1446).
    fn load_active_message_effects(&mut self, env: &mut impl ReactionManagerEnv) {
        let active_message_effects = env.binlog_pmc_get_raw("active_message_effects");
        if active_message_effects.is_empty() {
            return self.reload_message_effects(env);
        }

        let parse = || -> Result<ActiveEffects, &'static str> {
            let mut parser = Parser::new_log_event(&active_message_effects)?;
            let active = ActiveEffects::parse(&mut parser)?;
            parser.fetch_end()?;
            Ok(active)
        };
        match parse() {
            Ok(active) => {
                self.active_message_effects = active;
            }
            Err(error) => {
                tdutils::log_error!("Can't load active message effects: {error}");
                self.active_message_effects = ActiveEffects::default();
                return self.reload_message_effects(env);
            }
        }

        let update = self.get_update_available_message_effects_object();
        env.send_update(update);
    }

    /// `update_active_message_effects` (ReactionManager.cpp:1448-1466).
    fn update_active_message_effects(&mut self, env: &mut impl ReactionManagerEnv) {
        let mut active_message_effects = ActiveEffects::default();
        for effect in &self.message_effects.effects {
            if effect.is_sticker() {
                active_message_effects.sticker_effects.push(effect.id.get());
            } else {
                active_message_effects
                    .reaction_effects
                    .push(effect.id.get());
            }
        }
        if active_message_effects.reaction_effects == self.active_message_effects.reaction_effects
            && active_message_effects.sticker_effects == self.active_message_effects.sticker_effects
        {
            return;
        }
        self.active_message_effects = active_message_effects;

        self.save_active_message_effects(env);

        let update = self.get_update_available_message_effects_object();
        env.send_update(update);
    }

    /// `get_message_effect` (ReactionManager.cpp:1468-1476).
    pub fn get_message_effect(
        &mut self,
        env: &mut impl ReactionManagerEnv,
        effect_id: MessageEffectId,
        mut promise: Promise<Option<td_api::MessageEffect>>,
    ) {
        self.load_message_effects(env);
        if self.message_effects.effects.is_empty() && self.message_effects.are_being_reloaded {
            self.pending_get_message_effect_queries
                .push((effect_id, promise));
            return;
        }
        promise.set_value(self.get_message_effect_by_id(env, effect_id));
    }

    /// `send_update_default_paid_reaction_type`
    /// (ReactionManager.cpp:1478-1480).
    fn send_update_default_paid_reaction_type(&self, env: &mut impl ReactionManagerEnv) {
        let update = self
            .default_paid_reaction_type
            .get_update_default_paid_reaction_type(env);
        env.send_update(update);
    }

    /// `on_update_default_paid_reaction_type`
    /// (ReactionManager.cpp:1482-1488).
    pub fn on_update_default_paid_reaction_type(
        &mut self,
        env: &mut impl ReactionManagerEnv,
        type_: &PaidReactionType,
    ) {
        if type_.is_valid() && *type_ != self.default_paid_reaction_type {
            self.default_paid_reaction_type = type_.clone();
            self.save_default_paid_reaction_type(env);
            self.send_update_default_paid_reaction_type(env);
        }
    }

    /// `load_default_paid_reaction_type` (ReactionManager.cpp:1490-1514).
    /// The C++ `Dependencies` revalidation of the Dialog type collapses to
    /// the local `is_valid` check (the dependency resolution is the M.6/M.10
    /// dialog plane).
    fn load_default_paid_reaction_type(&mut self, env: &mut impl ReactionManagerEnv) {
        let type_ = env.binlog_pmc_get_raw("default_paid_reaction_type");
        if !type_.is_empty() {
            let parse = || -> Result<PaidReactionType, &'static str> {
                let mut parser = Parser::new_log_event(&type_)?;
                let parsed = PaidReactionType::parse(&mut parser)?;
                parser.fetch_end()?;
                Ok(parsed)
            };
            match parse() {
                Ok(parsed) => {
                    if parsed.is_valid() {
                        self.default_paid_reaction_type = parsed;
                    } else {
                        self.default_paid_reaction_type = PaidReactionType::default();
                        self.save_default_paid_reaction_type(env);
                    }
                }
                Err(error) => {
                    tdutils::log_error!("Can't load default paid reaction type: {error}");
                    self.default_paid_reaction_type = PaidReactionType::default();
                    self.save_default_paid_reaction_type(env);
                }
            }
        } else if env.have_option("is_paid_reaction_anonymous") {
            let is_anonymous = env.get_option_boolean("is_paid_reaction_anonymous", false);
            self.default_paid_reaction_type = PaidReactionType::legacy(is_anonymous);
            self.save_default_paid_reaction_type(env);
            env.set_option_empty("is_paid_reaction_anonymous");
        }
        self.send_update_default_paid_reaction_type(env);
    }

    /// `save_default_paid_reaction_type` (ReactionManager.cpp:1516-1520).
    fn save_default_paid_reaction_type(&self, env: &mut impl ReactionManagerEnv) {
        let mut out = Vec::new();
        store_log_event_version(&mut out);
        self.default_paid_reaction_type.store(&mut out);
        env.binlog_pmc_set_raw("default_paid_reaction_type", &out);
    }

    /// `get_default_paid_reaction_type` (ReactionManager.cpp:1522-1524).
    pub fn get_default_paid_reaction_type(&self) -> PaidReactionType {
        self.default_paid_reaction_type.clone()
    }

    /// `get_current_state` (ReactionManager.cpp:1526-1544).
    pub fn get_current_state(
        &self,
        env: &impl ReactionManagerEnv,
        updates: &mut Vec<td_api::Update>,
    ) {
        if env.auth_is_bot() {
            return;
        }

        if !self.active_reaction_types.is_empty() {
            updates.push(self.get_update_active_emoji_reactions_object());
        }
        if self.all_tags.is_inited {
            updates.push(self.get_update_saved_messages_tags_object(
                SavedMessagesTopicId::default(),
                &self.all_tags,
            ));
        }
        for (topic_id, tags) in &self.topic_tags {
            updates.push(self.get_update_saved_messages_tags_object(*topic_id, tags));
        }
        if !self.active_message_effects.is_empty() {
            updates.push(self.get_update_available_message_effects_object());
        }
        let update = self
            .default_paid_reaction_type
            .get_update_default_paid_reaction_type(env);
        updates.push(update);
    }
}

/// The `log_event_parse` of the `reactions` pmc record.
fn parse_reactions_record(
    env: &mut impl ReactionManagerEnv,
    data: &[u8],
) -> Result<Reactions, &'static str> {
    let mut parser = Parser::new_log_event(data)?;
    let parsed = Reactions::parse(env, &mut parser)?;
    parser.fetch_end()?;
    Ok(parsed)
}

/// The `log_event_parse` of the `message_effects` pmc record.
fn parse_effects_record(
    env: &mut impl ReactionManagerEnv,
    data: &[u8],
) -> Result<Effects, &'static str> {
    let mut parser = Parser::new_log_event(data)?;
    let parsed = Effects::parse(env, &mut parser)?;
    parser.fetch_end()?;
    Ok(parsed)
}

/// `td::add_to_top` over `ReactionType` (algorithm.h:115-137).
fn add_to_top(v: &mut Vec<ReactionType>, max_size: usize, value: ReactionType) {
    if let Some(pos) = v.iter().position(|x| *x == value) {
        v.remove(pos);
    } else if v.len() >= max_size && !v.is_empty() {
        v.pop();
    }
    v.insert(0, value);
}

#[cfg(test)]
#[path = "reaction_manager_tests.rs"]
mod reaction_manager_tests;
