//! `td::ActiveStoryState` (ActiveStoryState.h/.cpp, M.47).

use crate::story_ids::StoryId;

/// `td::ActiveStoryState` (ActiveStoryState.h:17-39).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ActiveStoryState {
    max_active_story_id: StoryId,
    max_read_story_id: StoryId,
    has_live_story: bool,
}

impl ActiveStoryState {
    pub fn new(max_active_story_id: StoryId, max_read_story_id: StoryId, has_live_story: bool) -> Self {
        ActiveStoryState {
            max_active_story_id,
            max_read_story_id,
            has_live_story,
        }
    }

    /// `ActiveStoryState::has_unread_stories` (ActiveStoryState.cpp:13-17).
    fn has_unread_stories(&self) -> bool {
        assert!(!self.has_live_story);
        self.max_active_story_id.get() > self.max_read_story_id.get()
    }

    /// `ActiveStoryState::get_active_story_state_object`
    /// (ActiveStoryState.cpp:19-32); `None` for the C++ nullptr.
    pub fn get_active_story_state_object(&self) -> Option<td_api::ActiveStoryState> {
        if !self.max_active_story_id.is_server() {
            return None;
        }
        if self.has_live_story {
            return Some(td_api::ActiveStoryState::activeStoryStateLive(
                td_api::activeStoryStateLive {
                    story_id: self.max_active_story_id.get(),
                },
            ));
        }
        if self.has_unread_stories() {
            return Some(td_api::ActiveStoryState::activeStoryStateUnread(
                td_api::activeStoryStateUnread,
            ));
        }
        Some(td_api::ActiveStoryState::activeStoryStateRead(
            td_api::activeStoryStateRead,
        ))
    }
}

/// `operator==(ActiveStoryState, ActiveStoryState)` (ActiveStoryState.cpp:34-45)
/// — the collapsed comparison keeps the live-story id check and the
/// unread-flag comparison.
pub fn active_story_states_equal(lhs: &ActiveStoryState, rhs: &ActiveStoryState) -> bool {
    if lhs.has_live_story != rhs.has_live_story {
        return false;
    }
    if lhs.has_live_story {
        return lhs.max_active_story_id == rhs.max_active_story_id;
    }
    lhs.has_unread_stories() == rhs.has_unread_stories()
}
