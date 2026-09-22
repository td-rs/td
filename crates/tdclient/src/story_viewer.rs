//! `td::StoryViewer`/`td::StoryViewers` (StoryViewer.h/.cpp, M.47). The
//! MessagesManager halves of the public-forward branches
//! (`on_get_message`, `get_dialog_message_sender`, `get_message_object`)
//! ride the [`crate::story_manager::StoryManagerEnv`] seams — [blocked:
//! MessagesManager track]; the repost branches resolve through the
//! manager's own `on_get_story`/`get_story_date`/`get_story_object`.

use crate::dialog_id::DialogId;
use crate::message_id::MessageId;
use crate::reaction_type::ReactionType;
use crate::story_ids::{StoryFullId, StoryId, dialog_id_from_peer};
use crate::story_manager::StoryManager;
use crate::td_api;
use crate::user_manager::UserId;
use tdmtproto::telegram_api;

/// The `StoryViewer::Type` discriminator (StoryViewer.h:25).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum StoryViewerType {
    #[default]
    None,
    View,
    Forward,
    Repost,
}

/// The `BlockListId` slice (BlockListId.h:17-72) the story viewers need.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StoryBlockListId {
    #[default]
    None,
    Main,
    Stories,
}

impl StoryBlockListId {
    /// `BlockListId(bool, bool)` (BlockListId.h:29-31).
    pub fn from_flags(is_blocked: bool, is_blocked_for_stories: bool) -> StoryBlockListId {
        if is_blocked {
            StoryBlockListId::Main
        } else if is_blocked_for_stories {
            StoryBlockListId::Stories
        } else {
            StoryBlockListId::None
        }
    }

    /// `BlockListId::get_block_list_object` (BlockListId.h:57-68).
    pub fn get_block_list_object(self) -> Option<td_api::BlockList> {
        match self {
            StoryBlockListId::None => None,
            StoryBlockListId::Main => Some(td_api::BlockList::blockListMain(td_api::blockListMain)),
            StoryBlockListId::Stories => Some(td_api::BlockList::blockListStories(
                td_api::blockListStories,
            )),
        }
    }
}

/// `td::StoryViewer` (StoryViewer.h:23-91).
#[derive(Clone, Debug, Default)]
pub struct StoryViewer {
    r#type: StoryViewerType,
    actor_dialog_id: DialogId,
    date: i32,
    is_blocked: bool,
    is_blocked_for_stories: bool,
    reaction_type: ReactionType,
    message_full_id: Option<(DialogId, MessageId)>, // for Forward
    story_id: StoryId,                              // for Repost
}

impl StoryViewer {
    /// `StoryViewer(Td *, StoryView)` (StoryViewer.cpp:15-79).
    pub fn from_story_view(
        manager: &mut StoryManager,
        env: &mut impl crate::story_manager::StoryManagerEnv,
        story_view_ptr: telegram_api::StoryView,
    ) -> StoryViewer {
        let mut viewer = StoryViewer::default();
        match story_view_ptr {
            telegram_api::StoryView::storyView(story_view) => {
                let user_id = UserId::new(story_view.user_id);
                if !user_id.is_valid() || story_view.date <= 0 {
                    return viewer;
                }
                viewer.r#type = StoryViewerType::View;
                viewer.actor_dialog_id = DialogId::from_user_id(user_id);
                viewer.date = story_view.date;
                viewer.is_blocked = story_view.blocked;
                viewer.is_blocked_for_stories = story_view.blocked_my_stories_from;
                viewer.reaction_type = ReactionType::from_telegram(story_view.reaction.as_ref());
            }
            telegram_api::StoryView::storyViewPublicForward(story_view) => {
                // `MessagesManager::get_message_date` +
                // `MessagesManager::on_get_message` — [blocked:
                // MessagesManager track]; the unresolvable message keeps the
                // viewer invalid like the C++ bail.
                let Some(message_full_id) = env.on_get_public_forward_message(story_view.message)
                else {
                    return viewer;
                };
                if !message_full_id.1.is_valid() || message_full_id.2 <= 0 {
                    return viewer;
                }
                viewer.r#type = StoryViewerType::Forward;
                viewer.actor_dialog_id =
                    env.get_dialog_message_sender(message_full_id.0, message_full_id.1.get());
                viewer.date = message_full_id.2;
                viewer.is_blocked = story_view.blocked;
                viewer.is_blocked_for_stories = story_view.blocked_my_stories_from;
                viewer.message_full_id = Some((message_full_id.0, message_full_id.1));
            }
            telegram_api::StoryView::storyViewPublicRepost(story_view) => {
                let owner_dialog_id = dialog_id_from_peer(story_view.peer_id.as_ref());
                if !owner_dialog_id.is_valid() {
                    return viewer;
                }
                let story_item = story_view.story.map(|boxed| *boxed);
                let story_id = manager.on_get_story(env, owner_dialog_id, story_item);
                let date = manager.get_story_date(env, StoryFullId::new(owner_dialog_id, story_id));
                if date <= 0 {
                    return viewer;
                }
                viewer.r#type = StoryViewerType::Repost;
                viewer.actor_dialog_id = owner_dialog_id;
                viewer.date = date;
                viewer.is_blocked = story_view.blocked;
                viewer.is_blocked_for_stories = story_view.blocked_my_stories_from;
                viewer.story_id = story_id;
            }
        }
        if viewer.is_valid() {
            env.on_update_dialog_is_blocked(
                viewer.actor_dialog_id,
                viewer.is_blocked,
                viewer.is_blocked_for_stories,
            );
        }
        viewer
    }

    /// `StoryViewer(Td *, StoryReaction)` (StoryViewer.cpp:81-131).
    pub fn from_story_reaction(
        manager: &mut StoryManager,
        env: &mut impl crate::story_manager::StoryManagerEnv,
        story_reaction_ptr: telegram_api::StoryReaction,
    ) -> StoryViewer {
        let mut viewer = StoryViewer::default();
        match story_reaction_ptr {
            telegram_api::StoryReaction::storyReaction(story_reaction) => {
                let actor_dialog_id = dialog_id_from_peer(story_reaction.peer_id.as_ref());
                if !actor_dialog_id.is_valid() || story_reaction.date <= 0 {
                    return viewer;
                }
                viewer.r#type = StoryViewerType::View;
                viewer.actor_dialog_id = actor_dialog_id;
                viewer.date = story_reaction.date;
                viewer.reaction_type =
                    ReactionType::from_telegram(story_reaction.reaction.as_ref());
            }
            telegram_api::StoryReaction::storyReactionPublicForward(story_reaction) => {
                let Some(message_full_id) =
                    env.on_get_public_forward_message(story_reaction.message)
                else {
                    return viewer;
                };
                if !message_full_id.1.is_valid() || message_full_id.2 <= 0 {
                    return viewer;
                }
                viewer.r#type = StoryViewerType::Forward;
                viewer.actor_dialog_id =
                    env.get_dialog_message_sender(message_full_id.0, message_full_id.1.get());
                viewer.date = message_full_id.2;
                viewer.message_full_id = Some((message_full_id.0, message_full_id.1));
            }
            telegram_api::StoryReaction::storyReactionPublicRepost(story_reaction) => {
                let owner_dialog_id = dialog_id_from_peer(story_reaction.peer_id.as_ref());
                if !owner_dialog_id.is_valid() {
                    return viewer;
                }
                let story_item = story_reaction.story.map(|boxed| *boxed);
                let story_id = manager.on_get_story(env, owner_dialog_id, story_item);
                let date = manager.get_story_date(env, StoryFullId::new(owner_dialog_id, story_id));
                if date <= 0 {
                    return viewer;
                }
                viewer.r#type = StoryViewerType::Repost;
                viewer.actor_dialog_id = owner_dialog_id;
                viewer.date = date;
                viewer.story_id = story_id;
            }
        }
        viewer
    }

    /// `StoryViewer::get_viewer_user_id` (StoryViewer.h:49-51).
    pub fn get_viewer_user_id(&self) -> UserId {
        if self.r#type == StoryViewerType::View
            && self.actor_dialog_id.get_type() == crate::dialog_id::DialogType::User
        {
            self.actor_dialog_id.get_user_id()
        } else {
            UserId::default()
        }
    }

    /// `StoryViewer::get_actor_dialog_id` (StoryViewer.h:53-55).
    pub fn get_actor_dialog_id(&self) -> DialogId {
        self.actor_dialog_id
    }

    /// `StoryViewer::is_valid` (StoryViewer.cpp:225-229).
    pub fn is_valid(&self) -> bool {
        self.r#type != StoryViewerType::None && self.actor_dialog_id.is_valid() && self.date > 0
    }

    /// `StoryViewer::get_story_interaction_object` (StoryViewer.cpp:133-160).
    pub fn get_story_interaction_object(
        &self,
        manager: &mut StoryManager,
        env: &mut impl crate::story_manager::StoryManagerEnv,
    ) -> td_api::StoryInteraction {
        let r#type = match self.r#type {
            StoryViewerType::View => td_api::StoryInteractionType::storyInteractionTypeView(
                td_api::storyInteractionTypeView {
                    chosen_reaction_type: self.reaction_type.get_reaction_type_object(),
                },
            ),
            StoryViewerType::Forward => {
                // `MessagesManager::get_message_object` — [blocked:
                // MessagesManager track]; the seam never resolves a message,
                // so the interaction type carries the codegen-forced
                // unsupported message placeholder.
                let message = env
                    .get_message_object(self.message_full_id, "storyInteractionTypeForward")
                    .unwrap_or_default();
                td_api::StoryInteractionType::storyInteractionTypeForward(
                    td_api::storyInteractionTypeForward {
                        message: Some(message),
                    },
                )
            }
            StoryViewerType::Repost => {
                let story_object = manager
                    .get_story_object(env, StoryFullId::new(self.actor_dialog_id, self.story_id));
                td_api::StoryInteractionType::storyInteractionTypeRepost(
                    td_api::storyInteractionTypeRepost {
                        story: story_object,
                    },
                )
            }
            StoryViewerType::None => unreachable!("invalid story viewer"),
        };
        td_api::StoryInteraction {
            actor_id: env.get_message_sender_object(self.actor_dialog_id, "storyInteraction"),
            interaction_date: self.date,
            block_list: StoryBlockListId::from_flags(self.is_blocked, self.is_blocked_for_stories)
                .get_block_list_object(),
            r#type: Some(r#type),
        }
    }
}

impl PartialEq for StoryViewer {
    fn eq(&self, other: &Self) -> bool {
        self.r#type == other.r#type
            && self.actor_dialog_id == other.actor_dialog_id
            && self.date == other.date
    }
}

/// `td::StoryViewers` (StoryViewer.h:97-142).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StoryViewers {
    total_count: i32,
    total_forward_count: i32,
    total_reaction_count: i32,
    story_viewers: Vec<StoryViewer>,
    next_offset: String,
}

impl StoryViewers {
    /// `StoryViewers(Td *, int32, int32, int32, vector<StoryView>, string)`
    /// (StoryViewer.cpp:162-181).
    #[allow(clippy::too_many_arguments)]
    pub fn from_story_views(
        manager: &mut StoryManager,
        env: &mut impl crate::story_manager::StoryManagerEnv,
        total_count: i32,
        total_forward_count: i32,
        total_reaction_count: i32,
        story_views: Vec<telegram_api::StoryView>,
        next_offset: String,
    ) -> StoryViewers {
        let mut result = StoryViewers {
            total_count,
            total_forward_count,
            total_reaction_count,
            next_offset,
            ..StoryViewers::default()
        };
        for story_view in story_views {
            let story_viewer = StoryViewer::from_story_view(manager, env, story_view);
            if !story_viewer.is_valid() {
                tdutils::log_error!("Receive invalid story interaction");
                continue;
            }
            result.story_viewers.push(story_viewer);
        }
        result
    }

    /// `StoryViewers(Td *, int32, vector<StoryReaction>, string)`
    /// (StoryViewer.cpp:183-199).
    pub fn from_story_reactions(
        manager: &mut StoryManager,
        env: &mut impl crate::story_manager::StoryManagerEnv,
        total_count: i32,
        story_reactions: Vec<telegram_api::StoryReaction>,
        next_offset: String,
    ) -> StoryViewers {
        let mut result = StoryViewers {
            total_count,
            next_offset,
            ..StoryViewers::default()
        };
        for story_reaction in story_reactions {
            let story_viewer = StoryViewer::from_story_reaction(manager, env, story_reaction);
            if !story_viewer.is_valid() {
                tdutils::log_error!("Receive invalid story interaction");
                continue;
            }
            result.story_viewers.push(story_viewer);
        }
        result
    }

    /// `StoryViewers::get_viewer_user_ids` (StoryViewer.cpp:201-212).
    pub fn get_viewer_user_ids(&self) -> Vec<UserId> {
        let mut result = Vec::new();
        for story_viewer in &self.story_viewers {
            let user_id = story_viewer.get_viewer_user_id();
            if user_id.is_valid() {
                result.push(user_id);
            }
        }
        result
    }

    /// `StoryViewers::get_actor_dialog_ids` (StoryViewer.cpp:214-216).
    pub fn get_actor_dialog_ids(&self) -> Vec<DialogId> {
        self.story_viewers
            .iter()
            .map(|viewer| viewer.get_actor_dialog_id())
            .collect()
    }

    /// `StoryViewers::get_story_interactions_object`
    /// (StoryViewer.cpp:218-224).
    pub fn get_story_interactions_object(
        &self,
        manager: &mut StoryManager,
        env: &mut impl crate::story_manager::StoryManagerEnv,
    ) -> td_api::StoryInteractions {
        td_api::StoryInteractions {
            total_count: self.total_count,
            total_forward_count: self.total_forward_count,
            total_reaction_count: self.total_reaction_count,
            interactions: self
                .story_viewers
                .iter()
                .map(|viewer| viewer.get_story_interaction_object(manager, env))
                .collect(),
            next_offset: self.next_offset.clone(),
        }
    }
}
