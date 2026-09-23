//! Port of `td/telegram/ReactionListType.{h,cpp}` (M.16.1): the three
//! server-synced reaction lists ReactionManager caches.

/// `ReactionListType` (ReactionListType.h) — the binlog-pmc index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReactionListType {
    Recent,
    Top,
    DefaultTag,
}

/// `MAX_REACTION_LIST_TYPE` (ReactionListType.h:16).
pub const MAX_REACTION_LIST_TYPE: usize = 3;

impl ReactionListType {
    /// The `reaction_lists_` array index (`static_cast<int32>`).
    pub fn as_index(self) -> usize {
        match self {
            ReactionListType::Recent => 0,
            ReactionListType::Top => 1,
            ReactionListType::DefaultTag => 2,
        }
    }
}

/// `get_reaction_list_type_database_key` (ReactionListType.cpp:11-23).
pub fn get_reaction_list_type_database_key(reaction_list_type: ReactionListType) -> &'static str {
    match reaction_list_type {
        ReactionListType::Recent => "recent_reactions",
        ReactionListType::Top => "top_reactions",
        ReactionListType::DefaultTag => "default_tag_reactions",
    }
}

impl std::fmt::Display for ReactionListType {
    /// `operator<<` (ReactionListType.cpp:25-37).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReactionListType::Recent => write!(f, "recent reactions"),
            ReactionListType::Top => write!(f, "top reactions"),
            ReactionListType::DefaultTag => write!(f, "default tag reactions"),
        }
    }
}

#[cfg(test)]
mod reaction_list_type_tests {
    use super::*;

    #[test]
    fn database_keys_match_the_cpp() {
        assert_eq!(
            get_reaction_list_type_database_key(ReactionListType::Recent),
            "recent_reactions"
        );
        assert_eq!(
            get_reaction_list_type_database_key(ReactionListType::Top),
            "top_reactions"
        );
        assert_eq!(
            get_reaction_list_type_database_key(ReactionListType::DefaultTag),
            "default_tag_reactions"
        );
        assert_eq!(MAX_REACTION_LIST_TYPE, 3);
        assert_eq!(ReactionListType::DefaultTag.as_index(), 2);
    }
}
