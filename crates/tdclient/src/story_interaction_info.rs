//! `td::StoryInteractionInfo` (StoryInteractionInfo.h/.cpp/.hpp, M.47).

use crate::reaction_type::ReactionType;
use crate::store_parse::{self, Parser};
use crate::story_manager::StoryManagerEnv;
use crate::td_api;
use crate::user_manager::UserId;
use tdmtproto::telegram_api;

/// `td::StoryInteractionInfo` (StoryInteractionInfo.h:23-93).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StoryInteractionInfo {
    recent_viewer_user_ids: Vec<UserId>,
    reaction_counts: Vec<(ReactionType, i32)>,
    view_count: i32,
    forward_count: i32,
    reaction_count: i32,
    has_viewers: bool,
}

/// `MAX_RECENT_VIEWERS` (StoryInteractionInfo.h:32).
const MAX_RECENT_VIEWERS: usize = 3;

impl StoryInteractionInfo {
    /// `StoryInteractionInfo(Td *, storyViews)` (StoryInteractionInfo.cpp:19-64);
    /// `None` models the C++ null-`story_views` empty construction.
    pub fn from_story_views(
        env: &mut impl StoryManagerEnv,
        story_views: Option<telegram_api::StoryViews>,
    ) -> StoryInteractionInfo {
        let Some(story_views) = story_views else {
            return StoryInteractionInfo::default();
        };
        let mut result = StoryInteractionInfo::default();
        for viewer_id in story_views.recent_viewers {
            let user_id = UserId::new(viewer_id);
            if user_id.is_valid() && env.have_min_user(user_id) {
                if result.recent_viewer_user_ids.len() == MAX_RECENT_VIEWERS {
                    tdutils::log_error!("Receive too many recent story viewers");
                    break;
                }
                result.recent_viewer_user_ids.push(user_id);
            } else {
                tdutils::log_error!("Receive {} as recent viewer", user_id.get());
            }
        }
        result.view_count = story_views.views_count;
        if result.view_count < 0 {
            tdutils::log_error!("Receive {} story views", result.view_count);
            result.view_count = 0;
        }
        result.forward_count = story_views.forwards_count;
        if result.forward_count < 0 {
            tdutils::log_error!("Receive {} story forwards", result.forward_count);
            result.forward_count = 0;
        }
        result.reaction_count = story_views.reactions_count;
        if result.reaction_count < 0 {
            tdutils::log_error!("Receive {} story reactions", result.reaction_count);
            result.reaction_count = 0;
        }
        result.has_viewers = story_views.has_viewers;

        let mut added_reaction_types: Vec<ReactionType> = Vec::new();
        for reaction in story_views.reactions {
            let reaction_type = ReactionType::from_telegram(reaction.reaction.as_ref());
            if reaction_type.is_empty() || reaction_type.is_paid_reaction() {
                tdutils::log_error!("Receive invalid story reaction");
                continue;
            }
            if added_reaction_types.contains(&reaction_type) {
                tdutils::log_error!("Receive again a story reaction");
                continue;
            }
            added_reaction_types.push(reaction_type.clone());
            if reaction.count == 0 {
                tdutils::log_error!("Receive zero story reaction count");
                continue;
            }
            result.reaction_counts.push((reaction_type, reaction.count));
        }
        crate::reaction_type::sort_reaction_counts(&mut result.reaction_counts);
        result
    }

    /// `StoryInteractionInfo::is_empty` (StoryInteractionInfo.h:38-40).
    pub fn is_empty(&self) -> bool {
        self.view_count < 0
    }

    /// `StoryInteractionInfo::has_hidden_viewers` (StoryInteractionInfo.h:42-44).
    pub fn has_hidden_viewers(&self) -> bool {
        self.view_count < 0 || !self.has_viewers
    }

    /// `StoryInteractionInfo::set_counts` (StoryInteractionInfo.h:50-56) —
    /// the C++ writes back into the *arguments* on change; the Rust form
    /// takes `&mut` counts.
    pub fn set_counts(&self, view_count: &mut i32, reaction_count: &mut i32) -> bool {
        if *view_count != self.view_count || *reaction_count != self.reaction_count {
            *view_count = self.view_count;
            *reaction_count = self.reaction_count;
            return true;
        }
        false
    }

    /// `StoryInteractionInfo::set_chosen_reaction_type`
    /// (StoryInteractionInfo.cpp:130-154) — the C++ `old`-loop bug is kept
    /// verbatim: the increment branch compares against `old_reaction_type`.
    pub fn set_chosen_reaction_type(
        &mut self,
        new_reaction_type: &ReactionType,
        old_reaction_type: &ReactionType,
    ) {
        if !old_reaction_type.is_empty() {
            assert!(!old_reaction_type.is_paid_reaction());
            for index in 0..self.reaction_counts.len() {
                if self.reaction_counts[index].0 == *old_reaction_type {
                    self.reaction_counts[index].1 -= 1;
                    if self.reaction_counts[index].1 == 0 {
                        self.reaction_counts.remove(index);
                    }
                    break;
                }
            }
        }
        if !new_reaction_type.is_empty() {
            assert!(!new_reaction_type.is_paid_reaction());
            let mut is_found = false;
            for pair in self.reaction_counts.iter_mut() {
                if pair.0 == *old_reaction_type {
                    pair.1 += 1;
                    is_found = true;
                }
            }
            if !is_found {
                self.reaction_counts.push((new_reaction_type.clone(), 1));
            }
        }
        crate::reaction_type::sort_reaction_counts(&mut self.reaction_counts);
    }

    /// `StoryInteractionInfo::get_view_count` (StoryInteractionInfo.h:66-68).
    pub fn get_view_count(&self) -> i32 {
        self.view_count
    }

    /// `StoryInteractionInfo::get_reaction_count`
    /// (StoryInteractionInfo.h:70-72).
    pub fn get_reaction_count(&self) -> i32 {
        self.reaction_count
    }

    /// `StoryInteractionInfo::get_reaction_counts`
    /// (StoryInteractionInfo.h:74-76).
    pub fn get_reaction_counts(&self) -> &[(ReactionType, i32)] {
        &self.reaction_counts
    }

    /// `StoryInteractionInfo::definitely_has_no_user`
    /// (StoryInteractionInfo.cpp:182-185).
    pub fn definitely_has_no_user(&self, user_id: UserId) -> bool {
        !self.is_empty()
            && self.view_count == self.recent_viewer_user_ids.len() as i32
            && !self.recent_viewer_user_ids.contains(&user_id)
    }

    /// `StoryInteractionInfo::set_recent_viewer_user_ids`
    /// (StoryInteractionInfo.cpp:159-180).
    pub fn set_recent_viewer_user_ids(&mut self, mut user_ids: Vec<UserId>) -> bool {
        if self.recent_viewer_user_ids.is_empty() && self.view_count > 0 {
            // don't update recent viewers for stories with expired viewers
            return false;
        }
        if user_ids.len() > MAX_RECENT_VIEWERS {
            user_ids.truncate(MAX_RECENT_VIEWERS);
        }
        if self.recent_viewer_user_ids.len() < user_ids.len()
            && user_ids.len() <= self.view_count as usize
        {
            return false;
        }
        if self.recent_viewer_user_ids != user_ids {
            self.recent_viewer_user_ids = user_ids;
            return true;
        }
        false
    }

    /// `StoryInteractionInfo::get_story_interaction_info_object`
    /// (StoryInteractionInfo.cpp:187-195); `None` for the C++ nullptr.
    pub fn get_story_interaction_info_object(
        &self,
        env: &mut impl StoryManagerEnv,
    ) -> Option<td_api::StoryInteractionInfo> {
        if self.is_empty() {
            return None;
        }
        Some(td_api::StoryInteractionInfo {
            view_count: self.view_count,
            forward_count: self.forward_count,
            reaction_count: self.reaction_count,
            recent_viewer_user_ids: env.get_user_ids_object(
                &self.recent_viewer_user_ids,
                "get_story_interaction_info_object",
            ),
        })
    }

    /// The dependency collapse (StoryInteractionInfo.cpp:122-128) — the
    /// recent viewers ride the manager's force-create seam.
    pub fn add_dependencies(&self, env: &mut impl StoryManagerEnv) {
        for user_id in &self.recent_viewer_user_ids {
            env.force_create_dialog(
                crate::dialog_id::DialogId::from_user_id(*user_id),
                "StoryInteractionInfo",
                true,
            );
        }
    }

    /// `StoryInteractionInfo::store` (StoryInteractionInfo.hpp:14-47).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_recent_viewer_user_ids = !self.recent_viewer_user_ids.is_empty();
        let has_reaction_count = self.reaction_count > 0;
        let know_has_viewers = true;
        let has_forward_count = self.forward_count > 0;
        let has_reaction_counts = !self.reaction_counts.is_empty();
        store_parse::store_flags(
            out,
            [
                has_recent_viewer_user_ids,
                has_reaction_count,
                know_has_viewers,
                self.has_viewers,
                has_forward_count,
                has_reaction_counts,
            ],
        );
        store_parse::store_i32(out, self.view_count);
        if has_recent_viewer_user_ids {
            // `td::store(vector<UserId>)` — the size, then the ids.
            store_parse::store_i32(out, self.recent_viewer_user_ids.len() as i32);
            for user_id in &self.recent_viewer_user_ids {
                store_parse::store_i64(out, user_id.get());
            }
        }
        if has_reaction_count {
            store_parse::store_i32(out, self.reaction_count);
        }
        if has_forward_count {
            store_parse::store_i32(out, self.forward_count);
        }
        if has_reaction_counts {
            store_parse::store_i32(out, self.reaction_counts.len() as i32);
            for (reaction_type, count) in &self.reaction_counts {
                reaction_type.store(out);
                store_parse::store_i32(out, *count);
            }
        }
    }

    /// `StoryInteractionInfo::parse` (StoryInteractionInfo.hpp:49-82).
    pub fn parse(parser: &mut Parser) -> std::result::Result<StoryInteractionInfo, &'static str> {
        let [
            has_recent_viewer_user_ids,
            has_reaction_count,
            know_has_viewers,
            has_viewers,
            has_forward_count,
            has_reaction_counts,
        ] = parser.parse_flags()?;
        let mut result = StoryInteractionInfo {
            has_viewers,
            ..StoryInteractionInfo::default()
        };
        result.view_count = parser.i32_()?;
        if has_recent_viewer_user_ids {
            let size = parser.i32_()?;
            for _ in 0..size {
                result
                    .recent_viewer_user_ids
                    .push(UserId::new(parser.i64_()?));
            }
        }
        if has_reaction_count {
            result.reaction_count = parser.i32_()?;
        }
        if has_forward_count {
            result.forward_count = parser.i32_()?;
        }
        if has_reaction_counts {
            let size = parser.i32_()?;
            for _ in 0..size {
                let reaction_type = ReactionType::parse(parser)?;
                let count = parser.i32_()?;
                result.reaction_counts.push((reaction_type, count));
            }
        }
        if !know_has_viewers {
            result.has_viewers =
                (result.view_count > 0 && !has_recent_viewer_user_ids) || result.reaction_count > 0;
        }
        Ok(result)
    }
}

impl std::fmt::Display for StoryInteractionInfo {
    /// `operator<<(StringBuilder, StoryInteractionInfo)`
    /// (StoryInteractionInfo.cpp:197-200).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} views and {} forwards with {} reactions by {:?}",
            self.view_count,
            self.forward_count,
            self.reaction_count,
            self.recent_viewer_user_ids
                .iter()
                .map(|user_id| user_id.get())
                .collect::<Vec<i64>>()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_parse_round_trip_covers_all_shapes() {
        // StoryInteractionInfo.hpp:14-82 — the six-flag layout with the
        // conditional viewer/reaction blocks.
        let value = StoryInteractionInfo {
            recent_viewer_user_ids: vec![UserId::new(1), UserId::new(2)],
            reaction_counts: vec![
                (ReactionType::from_emoji("👍".to_owned()), 3),
                (ReactionType::paid(), 1),
            ],
            view_count: 42,
            forward_count: 7,
            reaction_count: 4,
            has_viewers: true,
        };
        let mut out = Vec::new();
        value.store(&mut out);
        let mut parser = Parser::new(&out);
        let parsed = StoryInteractionInfo::parse(&mut parser).expect("parse");
        assert_eq!(parsed, value);

        let value = StoryInteractionInfo::default();
        let mut out = Vec::new();
        value.store(&mut out);
        let mut parser = Parser::new(&out);
        let parsed = StoryInteractionInfo::parse(&mut parser).expect("parse");
        assert_eq!(parsed, value);
        assert_eq!(parsed.view_count, 0);
        assert!(!parsed.has_viewers);
    }
}
