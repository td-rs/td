//! Port of `td/telegram/ChatReactions.{h,cpp,hpp}` (M.16.1): the
//! available-reactions container of a chat — the explicit reaction list,
//! the "all reactions" wildcards, the reactions limit and the paid
//! availability flag. Consumed by ChatManager (the dialog full update
//! intake, at integration) and by ReactionManager's available-reactions
//! builders.

use crate::reaction_type::ReactionType;
use crate::store_parse::{Parser, store_flags, store_i32};
use crate::td_api;
use std::collections::HashMap;
use tdmtproto::telegram_api;

/// `ChatReactions` (ChatReactions.h) — `allow_all_regular` implies an
/// empty `reaction_types`, `allow_all_custom` implies `allow_all_regular`.
#[derive(Clone, Debug, Default)]
pub struct ChatReactions {
    reaction_types: Vec<ReactionType>,
    allow_all_regular: bool,
    allow_all_custom: bool,
    reactions_limit: i32,
    paid_reactions_available: bool,
}

/// The `Td *td` slice [`ChatReactions::get_chat_available_reactions_object`]
/// reads — the `reactions_uniq_max` option.
pub trait ChatReactionsEnv {
    fn get_option_integer(&self, name: &str, default_value: i64) -> i64;
}

impl ChatReactions {
    /// `ChatReactions::legacy` (ChatReactions.h:30-34).
    pub fn legacy(reactions: Vec<ReactionType>) -> ChatReactions {
        ChatReactions {
            reaction_types: reactions,
            ..ChatReactions::default()
        }
    }

    /// The `(allow_all_regular, allow_all_custom)` constructor
    /// (ChatReactions.h:41-43).
    pub fn all(allow_all_regular: bool, allow_all_custom: bool) -> ChatReactions {
        ChatReactions {
            reaction_types: Vec::new(),
            allow_all_regular,
            allow_all_custom,
            reactions_limit: 0,
            paid_reactions_available: false,
        }
    }

    /// `ChatReactions(telegram_api::object_ptr<ChatReactions>, int32,
    /// bool)` (ChatReactions.cpp:22-49).
    pub fn from_telegram(
        chat_reactions: Option<telegram_api::ChatReactions>,
        reactions_limit: i32,
        paid_reactions_available: bool,
    ) -> ChatReactions {
        let mut result = ChatReactions {
            paid_reactions_available,
            ..ChatReactions::default()
        };
        match chat_reactions {
            None => {}
            Some(telegram_api::ChatReactions::chatReactionsNone(_)) => {}
            Some(telegram_api::ChatReactions::chatReactionsAll(chat_reactions)) => {
                result.allow_all_regular = true;
                result.allow_all_custom = chat_reactions.allow_custom;
            }
            Some(telegram_api::ChatReactions::chatReactionsSome(chat_reactions)) => {
                result.reaction_types = ReactionType::get_reaction_types(&chat_reactions.reactions);
                if result.remove_paid_reactions() {
                    tdutils::log_error!("Receive paid reaction allowed");
                }
            }
        }
        result.reactions_limit = reactions_limit;
        result
    }

    /// `ChatReactions(td_api::object_ptr<ChatAvailableReactions>, bool)`
    /// (ChatReactions.cpp:51-74).
    pub fn from_td_api(
        chat_reactions: Option<td_api::ChatAvailableReactions>,
        allow_all_custom: bool,
    ) -> ChatReactions {
        let mut result = ChatReactions::default();
        match chat_reactions {
            None => {}
            Some(td_api::ChatAvailableReactions::chatAvailableReactionsAll(chat_reactions)) => {
                result.allow_all_regular = true;
                result.allow_all_custom = allow_all_custom;
                result.reactions_limit = chat_reactions.max_reaction_count;
            }
            Some(td_api::ChatAvailableReactions::chatAvailableReactionsSome(chat_reactions)) => {
                result.reaction_types =
                    ReactionType::get_reaction_types_from_td_api(chat_reactions.reactions);
                result.reactions_limit = chat_reactions.max_reaction_count;
                result.paid_reactions_available = result.remove_paid_reactions();
            }
        }
        result
    }

    /// `remove_paid_reactions` (ChatReactions.cpp:17-20).
    pub fn remove_paid_reactions(&mut self) -> bool {
        let old_len = self.reaction_types.len();
        self.reaction_types
            .retain(|type_| !type_.is_paid_reaction());
        old_len != self.reaction_types.len()
    }

    /// `get_active_reactions` (ChatReactions.cpp:76-87).
    pub fn get_active_reactions(
        &self,
        active_reaction_pos: &HashMap<ReactionType, usize>,
    ) -> ChatReactions {
        let mut result = self.clone();
        if !result.reaction_types.is_empty() {
            debug_assert!(!result.allow_all_regular);
            debug_assert!(!result.allow_all_custom);
            result
                .reaction_types
                .retain(|type_| type_.is_active_reaction(active_reaction_pos));
        }
        result
    }

    /// `fix_broadcast_reactions` (ChatReactions.cpp:89-95).
    pub fn fix_broadcast_reactions(&mut self, active_reaction_types: &[ReactionType]) {
        if self.allow_all_regular {
            self.reaction_types = active_reaction_types.to_vec();
            self.allow_all_regular = false;
            self.allow_all_custom = false;
        }
    }

    /// `is_allowed_reaction_type` (ChatReactions.cpp:97-106) — the C++
    /// CHECKs `!allow_all_regular`; the callers only ask on the explicit
    /// lists.
    pub fn is_allowed_reaction_type(&self, reaction_type: &ReactionType) -> bool {
        debug_assert!(!self.allow_all_regular);
        if self.allow_all_custom && reaction_type.is_custom_reaction() {
            return true;
        }
        if reaction_type.is_paid_reaction() && self.paid_reactions_available {
            return true;
        }
        self.reaction_types.contains(reaction_type)
    }

    /// `get_chat_available_reactions_object` (ChatReactions.cpp:108-119).
    pub fn get_chat_available_reactions_object(
        &self,
        env: &impl ChatReactionsEnv,
    ) -> td_api::ChatAvailableReactions {
        let mut reactions_uniq_max = env.get_option_integer("reactions_uniq_max", 11) as i32;
        if 0 < self.reactions_limit && self.reactions_limit < reactions_uniq_max {
            reactions_uniq_max = self.reactions_limit;
        }
        if self.allow_all_regular {
            if self.paid_reactions_available {
                tdutils::log_error!("Have paid reaction in a non-channel chat");
            }
            return td_api::ChatAvailableReactions::chatAvailableReactionsAll(
                td_api::chatAvailableReactionsAll {
                    max_reaction_count: reactions_uniq_max,
                },
            );
        }
        td_api::ChatAvailableReactions::chatAvailableReactionsSome(
            td_api::chatAvailableReactionsSome {
                reactions: ReactionType::get_reaction_types_object(
                    &self.reaction_types,
                    self.paid_reactions_available,
                ),
                max_reaction_count: reactions_uniq_max,
            },
        )
    }

    /// `get_input_chat_reactions` (ChatReactions.cpp:121-130).
    pub fn get_input_chat_reactions(&self) -> telegram_api::ChatReactions {
        if self.allow_all_regular {
            return telegram_api::ChatReactions::chatReactionsAll(telegram_api::chatReactionsAll {
                flags: 0,
                allow_custom: self.allow_all_custom,
            });
        }
        if !self.reaction_types.is_empty() {
            return telegram_api::ChatReactions::chatReactionsSome(
                telegram_api::chatReactionsSome {
                    reactions: ReactionType::get_input_reactions(&self.reaction_types),
                },
            );
        }
        telegram_api::ChatReactions::chatReactionsNone(telegram_api::chatReactionsNone {})
    }

    /// `empty` (ChatReactions.h:56-58).
    pub fn is_empty(&self) -> bool {
        self.reaction_types.is_empty() && !self.allow_all_regular && !self.paid_reactions_available
    }

    /// `ignore_non_paid_reaction_types` (ChatReactions.cpp:132-136).
    pub fn ignore_non_paid_reaction_types(&mut self) {
        self.reaction_types.clear();
        self.allow_all_regular = false;
        self.allow_all_custom = false;
    }

    pub fn reaction_types(&self) -> &[ReactionType] {
        &self.reaction_types
    }

    pub fn allow_all_regular(&self) -> bool {
        self.allow_all_regular
    }

    pub fn allow_all_custom(&self) -> bool {
        self.allow_all_custom
    }

    pub fn paid_reactions_available(&self) -> bool {
        self.paid_reactions_available
    }

    pub fn reactions_limit(&self) -> i32 {
        self.reactions_limit
    }

    /// `ChatReactions::store` (ChatReactions.hpp:17-34).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_reactions = !self.reaction_types.is_empty();
        let has_reactions_limit = self.reactions_limit != 0;
        store_flags(
            out,
            [
                self.allow_all_regular,
                self.allow_all_custom,
                has_reactions,
                has_reactions_limit,
                self.paid_reactions_available,
            ],
        );
        if has_reactions {
            store_i32(out, self.reaction_types.len() as i32);
            for reaction_type in &self.reaction_types {
                reaction_type.store(out);
            }
        }
        if has_reactions_limit {
            store_i32(out, self.reactions_limit);
        }
    }

    /// `ChatReactions::parse` (ChatReactions.hpp:36-53).
    pub fn parse(parser: &mut Parser<'_>) -> Result<ChatReactions, &'static str> {
        let mut result = ChatReactions::default();
        let [
            allow_all_regular,
            allow_all_custom,
            has_reactions,
            has_reactions_limit,
            paid_reactions_available,
        ] = parser.parse_flags()?;
        result.allow_all_regular = allow_all_regular;
        result.allow_all_custom = allow_all_custom;
        result.paid_reactions_available = paid_reactions_available;
        if has_reactions {
            let len = parser.i32_()?;
            if len < 0 {
                return Err("Invalid vector size");
            }
            result.reaction_types.reserve(len as usize);
            for _ in 0..len {
                result.reaction_types.push(ReactionType::parse(parser)?);
            }
        }
        if has_reactions_limit {
            result.reactions_limit = parser.i32_()?;
        }
        Ok(result)
    }
}

/// `operator==` (ChatReactions.cpp:138-142) — `allow_all_custom` is
/// deliberately not compared.
impl PartialEq for ChatReactions {
    fn eq(&self, other: &Self) -> bool {
        self.reaction_types == other.reaction_types
            && self.allow_all_regular == other.allow_all_regular
            && self.reactions_limit == other.reactions_limit
            && self.paid_reactions_available == other.paid_reactions_available
    }
}

impl std::fmt::Display for ChatReactions {
    /// `operator<<` (ChatReactions.cpp:144-158).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.reactions_limit != 0 {
            write!(f, "[{}] ", self.reactions_limit)?;
        }
        if self.paid_reactions_available {
            write!(f, "Paid")?;
        }
        if self.allow_all_regular {
            if self.allow_all_custom {
                return write!(f, "AllReactions");
            }
            return write!(f, "AllRegularReactions");
        }
        write!(f, "[{:?}]", self.reaction_types)
    }
}

#[cfg(test)]
mod chat_reactions_tests {
    use super::*;
    use crate::store_parse::store_log_event_version;

    fn emoji(reaction: &str) -> ReactionType {
        ReactionType::new(reaction.to_owned())
    }

    struct Env;
    impl ChatReactionsEnv for Env {
        fn get_option_integer(&self, _name: &str, default_value: i64) -> i64 {
            default_value
        }
    }

    #[test]
    fn wire_conversions_match_the_cpp_switches() {
        // the paid availability flag alone makes the container non-empty,
        // like the C++ empty() body
        let reactions = ChatReactions::from_telegram(None, 7, true);
        assert!(!reactions.is_empty());
        assert_eq!(reactions.reactions_limit(), 7);
        assert!(reactions.paid_reactions_available());
        assert!(ChatReactions::from_telegram(None, 7, false).is_empty());

        let reactions = ChatReactions::from_telegram(
            Some(telegram_api::ChatReactions::chatReactionsAll(
                telegram_api::chatReactionsAll {
                    flags: 0,
                    allow_custom: true,
                },
            )),
            0,
            false,
        );
        assert_eq!(reactions, ChatReactions::all(true, true));

        // a paid reaction inside chatReactionsSome is dropped with the
        // C++ error log
        let reactions = ChatReactions::from_telegram(
            Some(telegram_api::ChatReactions::chatReactionsSome(
                telegram_api::chatReactionsSome {
                    reactions: vec![
                        telegram_api::Reaction::reactionEmoji(telegram_api::reactionEmoji {
                            emoticon: "👍".to_owned(),
                        }),
                        telegram_api::Reaction::reactionPaid(telegram_api::reactionPaid {}),
                    ],
                },
            )),
            3,
            false,
        );
        assert_eq!(reactions.reaction_types(), &[emoji("👍")]);
        assert!(!reactions.paid_reactions_available());

        // round trip through the input builder
        let input = reactions.get_input_chat_reactions();
        let parsed = ChatReactions::from_telegram(Some(input), 3, false);
        assert_eq!(parsed, reactions);
    }

    #[test]
    fn td_api_conversions_keep_the_limit_and_the_paid_flag() {
        let reactions = ChatReactions::from_td_api(
            Some(td_api::ChatAvailableReactions::chatAvailableReactionsAll(
                td_api::chatAvailableReactionsAll {
                    max_reaction_count: 11,
                },
            )),
            true,
        );
        assert_eq!(reactions.reaction_types(), &[]);
        assert!(reactions.allow_all_regular());
        assert!(reactions.allow_all_custom());
        // the td_api All carries the limit like the C++ ctor does
        assert_eq!(reactions.reactions_limit(), 11);

        let reactions = ChatReactions::from_td_api(
            Some(td_api::ChatAvailableReactions::chatAvailableReactionsSome(
                td_api::chatAvailableReactionsSome {
                    reactions: vec![
                        td_api::ReactionType::reactionTypeEmoji(td_api::reactionTypeEmoji {
                            emoji: "👍".to_owned(),
                        }),
                        td_api::ReactionType::reactionTypePaid(td_api::reactionTypePaid {}),
                    ],
                    max_reaction_count: 3,
                },
            )),
            false,
        );
        // the td_api paid reaction records the availability flag
        assert_eq!(reactions.reaction_types(), &[emoji("👍")]);
        assert!(reactions.paid_reactions_available());

        let object = reactions.get_chat_available_reactions_object(&Env);
        let td_api::ChatAvailableReactions::chatAvailableReactionsSome(object) = object else {
            panic!("wrong object kind");
        };
        assert_eq!(object.max_reaction_count, 3);
        assert_eq!(object.reactions.len(), 2); // the paid type rides along
    }

    #[test]
    fn active_and_broadcast_fixes_follow_the_cpp() {
        let mut active_pos = HashMap::new();
        active_pos.insert(emoji("👍"), 0);
        let reactions = ChatReactions::legacy(vec![emoji("👍"), emoji("🔥")]);
        let active = reactions.get_active_reactions(&active_pos);
        assert_eq!(active.reaction_types(), &[emoji("👍")]);

        let mut reactions = ChatReactions::all(true, true);
        reactions.fix_broadcast_reactions(&[emoji("👍")]);
        assert_eq!(reactions.reaction_types(), &[emoji("👍")]);
        assert!(!reactions.allow_all_regular());

        assert!(reactions.is_allowed_reaction_type(&emoji("👍")));
        assert!(!reactions.is_allowed_reaction_type(&emoji("🔥")));
    }

    #[test]
    fn store_parse_round_trips() {
        let mut reactions = ChatReactions::from_telegram(
            Some(telegram_api::ChatReactions::chatReactionsSome(
                telegram_api::chatReactionsSome {
                    reactions: vec![telegram_api::Reaction::reactionEmoji(
                        telegram_api::reactionEmoji {
                            emoticon: "👍".to_owned(),
                        },
                    )],
                },
            )),
            3,
            true,
        );
        let mut out = Vec::new();
        store_log_event_version(&mut out);
        reactions.store(&mut out);
        let mut parser = Parser::new_log_event(&out).unwrap();
        let parsed = ChatReactions::parse(&mut parser).unwrap();
        parser.fetch_end().unwrap();
        assert_eq!(parsed, reactions);

        // the all-reactions flag block round trips too
        reactions = ChatReactions::all(true, true);
        let mut out = Vec::new();
        store_log_event_version(&mut out);
        reactions.store(&mut out);
        let mut parser = Parser::new_log_event(&out).unwrap();
        let parsed = ChatReactions::parse(&mut parser).unwrap();
        parser.fetch_end().unwrap();
        assert_eq!(parsed, reactions);
        // allow_all_custom is not part of the equality
        let mut different_custom = reactions.clone();
        different_custom.allow_all_custom = false;
        assert_eq!(different_custom, reactions);
    }
}
