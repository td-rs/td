//! `td::ReactionType` (ReactionType.h/.cpp/.hpp) — the story-plane subset
//! (M.47). The C++ type is a string with a `#`-prefixed base64 custom-emoji
//! encoding and the `$` paid sentinel; the subset here ports the string
//! representation, the predicates, the ordering, the store/parse and the
//! td_api/telegram_api conversions byte-faithfully (ReactionType.cpp:30-37,
//! 90-160, 184-193; ReactionType.hpp:17-25). The manager-level halves
//! (`is_active_reaction` against the active-reaction map, the default-
//! reaction option glue, the hash) are the Reactions track and stay there.

use crate::store_parse::{self, Parser};
use crate::td_api;
use tdmtproto::telegram_api;

/// `td::ReactionType` (ReactionType.h:20-94) — the `reaction_` string form.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ReactionType {
    reaction: String,
}

impl ReactionType {
    /// `ReactionType::paid()` (ReactionType.cpp:97-99).
    pub fn paid() -> ReactionType {
        ReactionType {
            reaction: "$".to_owned(),
        }
    }

    /// `ReactionType::ReactionType(string &&)` (ReactionType.cpp:38-40).
    pub fn from_emoji(emoji: String) -> ReactionType {
        ReactionType { reaction: emoji }
    }

    /// `ReactionType::ReactionType(const telegram_api::object_ptr<Reaction>
    /// &)` (ReactionType.cpp:42-66).
    pub fn from_telegram(reaction: Option<&telegram_api::Reaction>) -> ReactionType {
        let Some(reaction) = reaction else {
            return ReactionType::default();
        };
        match reaction {
            telegram_api::Reaction::reactionEmpty(_) => ReactionType::default(),
            telegram_api::Reaction::reactionEmoji(reaction) => {
                let mut result = ReactionType {
                    reaction: reaction.emoticon.clone(),
                };
                if result.is_custom_reaction() || result.is_paid_reaction() {
                    result.reaction = String::new();
                }
                result
            }
            telegram_api::Reaction::reactionCustomEmoji(reaction) => ReactionType {
                reaction: get_custom_emoji_string(reaction.document_id),
            },
            telegram_api::Reaction::reactionPaid(_) => ReactionType::paid(),
        }
    }

    /// `ReactionType::ReactionType(const td_api::object_ptr<td_api::ReactionType>
    /// &)` (ReactionType.cpp:68-95).
    pub fn from_td_api(r#type: Option<&td_api::ReactionType>) -> ReactionType {
        let Some(r#type) = r#type else {
            return ReactionType::default();
        };
        match r#type {
            td_api::ReactionType::reactionTypeEmoji(r#type) => {
                let mut result = ReactionType {
                    reaction: r#type.emoji.clone(),
                };
                if result.is_custom_reaction() || result.is_paid_reaction() {
                    result.reaction = String::new();
                }
                result
            }
            td_api::ReactionType::reactionTypeCustomEmoji(r#type) => ReactionType {
                reaction: get_custom_emoji_string(r#type.custom_emoji_id),
            },
            td_api::ReactionType::reactionTypePaid(_) => ReactionType::paid(),
        }
    }

    /// `ReactionType::is_empty` (ReactionType.h:68-70).
    pub fn is_empty(&self) -> bool {
        self.reaction.is_empty()
    }

    /// `ReactionType::is_custom_reaction` (ReactionType.cpp:102-104) — the
    /// C++ reads `reaction_[0]`, which is `'\0'` for the empty string, so
    /// the empty reaction is not "custom".
    pub fn is_custom_reaction(&self) -> bool {
        self.reaction.starts_with('#')
    }

    /// `ReactionType::is_paid_reaction` (ReactionType.cpp:106-108).
    pub fn is_paid_reaction(&self) -> bool {
        self.reaction == "$"
    }

    /// `operator<(ReactionType, ReactionType)` (ReactionType.cpp:184-193) —
    /// paid reactions sort last, then the raw string order.
    pub fn cmp_order(&self, other: &ReactionType) -> std::cmp::Ordering {
        if self.is_paid_reaction() {
            return if other.is_paid_reaction() {
                std::cmp::Ordering::Equal
            } else {
                std::cmp::Ordering::Greater
            };
        }
        if other.is_paid_reaction() {
            return std::cmp::Ordering::Less;
        }
        self.reaction.cmp(&other.reaction)
    }

    /// `ReactionType::get_input_reaction` (ReactionType.cpp:142-155).
    pub fn get_input_reaction(&self) -> telegram_api::Reaction {
        if self.is_empty() {
            return telegram_api::Reaction::reactionEmpty(telegram_api::reactionEmpty);
        }
        if self.is_custom_reaction() {
            return telegram_api::Reaction::reactionCustomEmoji(
                telegram_api::reactionCustomEmoji {
                    document_id: get_custom_emoji_id(&self.reaction),
                },
            );
        }
        if self.is_paid_reaction() {
            return telegram_api::Reaction::reactionPaid(telegram_api::reactionPaid);
        }
        telegram_api::Reaction::reactionEmoji(telegram_api::reactionEmoji {
            emoticon: self.reaction.clone(),
        })
    }

    /// `ReactionType::get_reaction_type_object` (ReactionType.cpp:157-170);
    /// `None` for the C++ nullptr.
    pub fn get_reaction_type_object(&self) -> Option<td_api::ReactionType> {
        if self.is_empty() {
            return None;
        }
        if self.is_custom_reaction() {
            return Some(td_api::ReactionType::reactionTypeCustomEmoji(
                td_api::reactionTypeCustomEmoji {
                    custom_emoji_id: get_custom_emoji_id(&self.reaction),
                },
            ));
        }
        if self.is_paid_reaction() {
            return Some(td_api::ReactionType::reactionTypePaid(
                td_api::reactionTypePaid,
            ));
        }
        Some(td_api::ReactionType::reactionTypeEmoji(
            td_api::reactionTypeEmoji {
                emoji: self.reaction.clone(),
            },
        ))
    }

    /// `ReactionType::store` (ReactionType.hpp:17-20) — the raw string.
    pub fn store(&self, out: &mut Vec<u8>) {
        store_parse::store_string(out, &self.reaction);
    }

    /// `ReactionType::parse` (ReactionType.hpp:22-25).
    pub fn parse(parser: &mut Parser) -> std::result::Result<ReactionType, &'static str> {
        let reaction = parser.string()?;
        Ok(ReactionType { reaction })
    }
}

/// `get_custom_emoji_string` (ReactionType.cpp:30-37) — `'#' +
/// base64(8 little-endian bytes)`.
pub fn get_custom_emoji_string(custom_emoji_id: i64) -> String {
    use base64::Engine;
    let bytes = custom_emoji_id.to_le_bytes();
    format!(
        "#{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )
}

/// `get_custom_emoji_id` (ReactionType.cpp:17-28) — the inverse; a broken
/// encoding reads back as 0 like the C++ (`as<int64>` of a zero-filled
/// buffer on the failed parse).
pub fn get_custom_emoji_id(reaction: &str) -> i64 {
    use base64::Engine;
    if reaction.len() <= 1 || !reaction.starts_with('#') {
        return 0;
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(&reaction[1..])
        .unwrap_or_default();
    if decoded.len() < 8 {
        return 0;
    }
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&decoded[..8]);
    i64::from_le_bytes(bytes)
}

/// The `vector<pair<ReactionType, int32>>` sort helper
/// (StoryInteractionInfo.cpp:117): stable by the pair-first ordering of
/// [`ReactionType::cmp_order`], then the count.
pub fn sort_reaction_counts(reaction_counts: &mut [(ReactionType, i32)]) {
    reaction_counts.sort_by(|lhs, rhs| {
        lhs.0
            .cmp_order(&rhs.0)
            .then_with(|| lhs.1.cmp(&rhs.1))
            .then_with(|| lhs.0.reaction.cmp(&rhs.0.reaction))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emoji_custom_and_paid_forms() {
        // ReactionType.cpp:38-99 — the string encodings.
        let emoji = ReactionType::from_emoji("👍".to_owned());
        assert!(!emoji.is_custom_reaction());
        assert!(!emoji.is_paid_reaction());
        assert_eq!(
            emoji.get_reaction_type_object(),
            Some(td_api::ReactionType::reactionTypeEmoji(
                td_api::reactionTypeEmoji {
                    emoji: "👍".to_owned()
                }
            ))
        );

        let custom = ReactionType::from_emoji(get_custom_emoji_string(12345));
        assert!(custom.is_custom_reaction());
        assert_eq!(
            custom.get_reaction_type_object(),
            Some(td_api::ReactionType::reactionTypeCustomEmoji(
                td_api::reactionTypeCustomEmoji {
                    custom_emoji_id: 12345
                }
            ))
        );

        assert!(ReactionType::paid().is_paid_reaction());
        assert_eq!(
            ReactionType::paid().get_reaction_type_object(),
            Some(td_api::ReactionType::reactionTypePaid(
                td_api::reactionTypePaid
            ))
        );
        assert_eq!(ReactionType::default().get_reaction_type_object(), None);
    }

    #[test]
    fn paid_sorts_last() {
        // ReactionType.cpp:184-193.
        assert!(
            ReactionType::from_emoji("👍".to_owned()).cmp_order(&ReactionType::paid())
                == std::cmp::Ordering::Less
        );
        assert!(
            ReactionType::paid().cmp_order(&ReactionType::from_emoji("👍".to_owned()))
                == std::cmp::Ordering::Greater
        );
        assert!(ReactionType::paid().cmp_order(&ReactionType::paid()) == std::cmp::Ordering::Equal);
        assert!(
            ReactionType::from_emoji("a".to_owned())
                .cmp_order(&ReactionType::from_emoji("b".to_owned()))
                == std::cmp::Ordering::Less
        );
    }

    #[test]
    fn store_parse_round_trip() {
        for value in [
            ReactionType::from_emoji("👍".to_owned()),
            ReactionType::from_emoji(get_custom_emoji_string(999)),
            ReactionType::paid(),
            ReactionType::default(),
        ] {
            let mut out = Vec::new();
            value.store(&mut out);
            let mut parser = Parser::new(&out);
            let parsed = ReactionType::parse(&mut parser).expect("parse");
            assert_eq!(parsed, value);
        }
    }
}
