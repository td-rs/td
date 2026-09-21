//! Port of `td/telegram/ReactionType.{h,cpp,hpp}` (M.16.1): the reaction
//! identity. The C++ class is a `string` wrapper whose byte encoding is
//! the whole model — `""` is the empty (invalid) reaction, a leading `'#'`
//! marks a custom emoji reaction (`#` + base64 of the little-endian
//! document id), `"\"$"` is the paid reaction and anything else is a raw
//! emoji. Every map/set keyed by reactions and every stored list rides
//! that encoding, so the comparisons and the hash are byte-exact ports.

use crate::misc::{get_md5_string_hash, get_vector_hash};
use crate::store_parse::{Parser, store_string};
use crate::td_api;
use tdmtproto::telegram_api;
use tdutils::emoji::remove_emoji_selectors;

/// `base64_encode` (td/utils/base64.cpp) over exactly 8 bytes — 12 output
/// characters with the standard `=` padding.
fn base64_encode_8(bytes: &[u8; 8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(12);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(triple >> 18) as usize & 63] as char);
        out.push(ALPHABET[(triple >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(triple >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[triple as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// `base64_decode` (td/utils/base64.cpp) — the strict padded form; an
/// invalid or unpadded input is an error (the C++ CHECKs the decode of a
/// custom reaction string, so only corrupted stores reach the error).
fn base64_decode_8(input: &str) -> Option<[u8; 8]> {
    fn value(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a' + 26) as u32),
            b'0'..=b'9' => Some((c - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes = input.as_bytes();
    // 8 bytes → 12 characters with the single trailing '=' pad
    if bytes.len() != 12 || bytes[11] != b'=' {
        return None;
    }
    let mut out = [0u8; 8];
    for (group, out_chunk) in out.chunks_mut(3).enumerate() {
        let in_chunk = &bytes[group * 4..group * 4 + 4];
        // the final group's fourth character is the '=' pad
        let data_chars = if group == 2 { 3 } else { 4 };
        let mut triple = 0u32;
        for (index, c) in in_chunk[..data_chars].iter().enumerate() {
            triple |= value(*c)? << (18 - 6 * index);
        }
        out_chunk[0] = (triple >> 16) as u8;
        if out_chunk.len() > 1 {
            out_chunk[1] = (triple >> 8) as u8;
        }
        if out_chunk.len() > 2 {
            out_chunk[2] = triple as u8;
        }
    }
    Some(out)
}

/// `get_custom_emoji_id` (ReactionType.cpp:23-28) — the C++ CHECKs the
/// decode; a decode failure can only come from a corrupted store, so the
/// port logs and answers 0 instead of crashing.
fn get_custom_emoji_id(reaction: &str) -> i64 {
    let Some(bytes) = reaction.get(1..).and_then(base64_decode_8) else {
        tdutils::log_error!("Failed to decode custom reaction {reaction}");
        return 0;
    };
    i64::from_le_bytes(bytes)
}

/// `get_custom_emoji_string` (ReactionType.cpp:30-34).
fn get_custom_emoji_string(custom_emoji_id: i64) -> String {
    let mut string = String::from("#");
    string.push_str(&base64_encode_8(&custom_emoji_id.to_le_bytes()));
    string
}

/// `ReactionType` (ReactionType.h) — the encoding is the model; see the
/// module docs.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct ReactionType(String);

impl ReactionType {
    /// `ReactionType(string &&emoji)` (ReactionType.cpp:36) — the raw
    /// encoding; the `#`/`$`/empty classification happens inside.
    pub fn new(emoji: String) -> ReactionType {
        ReactionType(emoji)
    }

    /// `ReactionType::paid` (ReactionType.cpp:95-99).
    pub fn paid() -> ReactionType {
        ReactionType("$".to_owned())
    }

    /// `ReactionType(const telegram_api::object_ptr<Reaction> &)` — the
    /// null pointer is the `None` argument; `reactionEmpty` keeps the
    /// reaction empty (ReactionType.cpp:39-63).
    pub fn from_telegram(reaction: Option<&telegram_api::Reaction>) -> ReactionType {
        let Some(reaction) = reaction else {
            return ReactionType::default();
        };
        match reaction {
            telegram_api::Reaction::reactionEmpty(_) => ReactionType::default(),
            telegram_api::Reaction::reactionEmoji(reaction) => {
                let reaction_type = ReactionType(reaction.emoticon.clone());
                if reaction_type.is_custom_reaction() || reaction_type.is_paid_reaction() {
                    // a server emoji that encodes our internal forms is
                    // dropped, exactly like the C++
                    ReactionType::default()
                } else {
                    reaction_type
                }
            }
            telegram_api::Reaction::reactionCustomEmoji(reaction) => {
                ReactionType(get_custom_emoji_string(reaction.document_id))
            }
            telegram_api::Reaction::reactionPaid(_) => ReactionType::paid(),
        }
    }

    /// `ReactionType(const td_api::object_ptr<td_api::ReactionType> &)`
    /// (ReactionType.cpp:65-93). The C++ `check_utf8` break cannot fire —
    /// a Rust `String` is always valid UTF-8.
    pub fn from_td_api(type_: Option<td_api::ReactionType>) -> ReactionType {
        let Some(type_) = type_ else {
            return ReactionType::default();
        };
        match type_ {
            td_api::ReactionType::reactionTypeEmoji(type_) => {
                let reaction_type = ReactionType(type_.emoji);
                if reaction_type.is_custom_reaction() || reaction_type.is_paid_reaction() {
                    ReactionType::default()
                } else {
                    reaction_type
                }
            }
            td_api::ReactionType::reactionTypeCustomEmoji(type_) => {
                ReactionType(get_custom_emoji_string(type_.custom_emoji_id))
            }
            td_api::ReactionType::reactionTypePaid(_) => ReactionType::paid(),
        }
    }

    /// `ReactionType::get_reaction_types` over the wire objects.
    pub fn get_reaction_types(reactions: &[telegram_api::Reaction]) -> Vec<ReactionType> {
        reactions
            .iter()
            .map(|reaction| ReactionType::from_telegram(Some(reaction)))
            .collect()
    }

    /// The td_api overload of `get_reaction_types`.
    pub fn get_reaction_types_from_td_api(
        reactions: Vec<td_api::ReactionType>,
    ) -> Vec<ReactionType> {
        reactions
            .into_iter()
            .map(|reaction| ReactionType::from_td_api(Some(reaction)))
            .collect()
    }

    /// `get_input_reactions` (ReactionType.cpp:111-115).
    pub fn get_input_reactions(reaction_types: &[ReactionType]) -> Vec<telegram_api::Reaction> {
        reaction_types
            .iter()
            .map(ReactionType::get_input_reaction)
            .collect()
    }

    /// `get_reaction_types_object` (ReactionType.cpp:117-128) — the paid
    /// reaction is prepended when available. The C++ pushes `nullptr` for
    /// empty reactions (never stored, wire-broken only); the generated
    /// `Vec<ReactionType>` cannot carry nulls, so the port skips them with
    /// the C++ "unexpected" log.
    pub fn get_reaction_types_object(
        reaction_types: &[ReactionType],
        paid_reactions_available: bool,
    ) -> Vec<td_api::ReactionType> {
        let mut result =
            Vec::with_capacity(reaction_types.len() + usize::from(paid_reactions_available));
        if paid_reactions_available {
            result.push(ReactionType::paid().get_reaction_type_object().unwrap());
        }
        for reaction_type in reaction_types {
            match reaction_type.get_reaction_type_object() {
                Some(object) => result.push(object),
                None => {
                    tdutils::log_error!("Have empty reaction in a types object list");
                }
            }
        }
        result
    }

    /// `get_input_reaction` (ReactionType.cpp:130-141).
    pub fn get_input_reaction(&self) -> telegram_api::Reaction {
        if self.is_empty() {
            return telegram_api::Reaction::reactionEmpty(telegram_api::reactionEmpty {});
        }
        if self.is_custom_reaction() {
            return telegram_api::Reaction::reactionCustomEmoji(
                telegram_api::reactionCustomEmoji {
                    document_id: get_custom_emoji_id(&self.0),
                },
            );
        }
        if self.is_paid_reaction() {
            return telegram_api::Reaction::reactionPaid(telegram_api::reactionPaid {});
        }
        telegram_api::Reaction::reactionEmoji(telegram_api::reactionEmoji {
            emoticon: self.0.clone(),
        })
    }

    /// `get_reaction_type_object` (ReactionType.cpp:143-154) — the C++
    /// returns the null pointer for an empty reaction.
    pub fn get_reaction_type_object(&self) -> Option<td_api::ReactionType> {
        if self.is_empty() {
            return None;
        }
        if self.is_custom_reaction() {
            return Some(td_api::ReactionType::reactionTypeCustomEmoji(
                td_api::reactionTypeCustomEmoji {
                    custom_emoji_id: get_custom_emoji_id(&self.0),
                },
            ));
        }
        if self.is_paid_reaction() {
            return Some(td_api::ReactionType::reactionTypePaid(
                td_api::reactionTypePaid {},
            ));
        }
        Some(td_api::ReactionType::reactionTypeEmoji(
            td_api::reactionTypeEmoji {
                emoji: self.0.clone(),
            },
        ))
    }

    /// `get_update_default_reaction_type` (ReactionType.cpp:156-161).
    pub fn get_update_default_reaction_type(&self) -> Option<td_api::Update> {
        self.get_reaction_type_object().map(|reaction_type| {
            td_api::Update::updateDefaultReactionType(td_api::updateDefaultReactionType {
                reaction_type: Some(reaction_type),
            })
        })
    }

    /// `get_hash` (ReactionType.cpp:163-169) — a custom reaction hashes as
    /// its document id, an emoji through the md5 fold of the
    /// selector-stripped string.
    pub fn get_hash(&self) -> u64 {
        if self.is_custom_reaction() {
            get_custom_emoji_id(&self.0) as u64
        } else {
            get_md5_string_hash(&remove_emoji_selectors(&self.0))
        }
    }

    /// `is_custom_reaction` (ReactionType.cpp:171-173).
    pub fn is_custom_reaction(&self) -> bool {
        self.0.as_bytes().first() == Some(&b'#')
    }

    /// `is_paid_reaction` (ReactionType.cpp:175-177).
    pub fn is_paid_reaction(&self) -> bool {
        self.0 == "$"
    }

    /// `is_active_reaction` (ReactionType.cpp:179-182) over the
    /// reaction → position map (`FlatHashMap<ReactionType, size_t>`).
    pub fn is_active_reaction(
        &self,
        active_reaction_pos: &std::collections::HashMap<ReactionType, usize>,
    ) -> bool {
        !self.is_empty()
            && (self.is_custom_reaction()
                || self.is_paid_reaction()
                || active_reaction_pos.contains_key(self))
    }

    /// `is_empty` (ReactionType.h:68-70).
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// `get_string` (ReactionType.h:72-74).
    pub fn get_string(&self) -> &str {
        &self.0
    }

    /// `ReactionType::store` (ReactionType.hpp) — CHECKs non-empty.
    pub fn store(&self, out: &mut Vec<u8>) {
        assert!(!self.is_empty(), "the C++ store CHECKs !is_empty");
        store_string(out, &self.0);
    }

    /// `ReactionType::parse` (ReactionType.hpp).
    pub fn parse(parser: &mut Parser<'_>) -> Result<ReactionType, &'static str> {
        Ok(ReactionType(parser.string()?))
    }
}

/// `operator<` (ReactionType.cpp:184-192) — the paid reaction sorts
/// before every other reaction, then the raw byte encoding.
impl Ord for ReactionType {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        if self.is_paid_reaction() {
            return if other.is_paid_reaction() {
                Ordering::Equal
            } else {
                Ordering::Less
            };
        }
        if other.is_paid_reaction() {
            return Ordering::Greater;
        }
        self.0.cmp(&other.0)
    }
}

impl PartialOrd for ReactionType {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl std::fmt::Display for ReactionType {
    /// `operator<<` (ReactionType.cpp:198-209).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_empty() {
            return write!(f, "empty reaction");
        }
        if self.is_custom_reaction() {
            return write!(f, "custom reaction {}", get_custom_emoji_id(&self.0));
        }
        if self.is_paid_reaction() {
            return write!(f, "paid reaction");
        }
        write!(f, "reaction {}", self.0)
    }
}

/// `get_reaction_types_hash` (ReactionType.cpp:211-234) — the list hash
/// the wire hash checks ride on. Custom reactions contribute their
/// document id split into the high/low words, emojis a zero word plus the
/// big-endian first md5 word sign-extended like the C++ `static_cast<int32>`.
pub fn get_reaction_types_hash(reaction_types: &[ReactionType]) -> i64 {
    let mut numbers = Vec::with_capacity(reaction_types.len() * 2);
    for reaction_type in reaction_types {
        if reaction_type.is_custom_reaction() {
            let custom_emoji_id = get_custom_emoji_id(reaction_type.get_string()) as u64;
            numbers.push(custom_emoji_id >> 32);
            numbers.push(custom_emoji_id & 0xFFFF_FFFF);
        } else {
            if reaction_type.is_paid_reaction() {
                tdutils::log_error!("Have paid reaction");
            }
            let emoji = remove_emoji_selectors(reaction_type.get_string());
            let hash = tdutils::crypto::md5(emoji.as_bytes());
            let word = u32::from_be_bytes([hash[0], hash[1], hash[2], hash[3]]);
            numbers.push(0);
            numbers.push((word as i32) as i64 as u64);
        }
    }
    get_vector_hash(&numbers)
}

#[cfg(test)]
mod reaction_type_tests {
    use super::*;

    #[test]
    fn wire_and_td_api_conversions_match_the_cpp_encoding() {
        // reactionEmoji → raw string
        let reaction = ReactionType::from_telegram(Some(&telegram_api::Reaction::reactionEmoji(
            telegram_api::reactionEmoji {
                emoticon: "👍".to_owned(),
            },
        )));
        assert_eq!(reaction.get_string(), "👍");
        assert!(!reaction.is_custom_reaction());
        assert!(!reaction.is_paid_reaction());

        // a server emoji carrying our internal '#'/'$' forms is emptied
        for emoticon in ["#abc", "$"] {
            let reaction = ReactionType::from_telegram(Some(
                &telegram_api::Reaction::reactionEmoji(telegram_api::reactionEmoji {
                    emoticon: emoticon.to_owned(),
                }),
            ));
            assert!(reaction.is_empty(), "{emoticon}");
        }

        // reactionCustomEmoji → '#' + base64(le bytes)
        let reaction = ReactionType::from_telegram(Some(
            &telegram_api::Reaction::reactionCustomEmoji(telegram_api::reactionCustomEmoji {
                document_id: 543210987654321,
            }),
        ));
        assert_eq!(reaction.get_string(), "#sQz1KQzuAQA=");
        assert!(reaction.is_custom_reaction());

        // reactionPaid → "$"
        let reaction = ReactionType::from_telegram(Some(&telegram_api::Reaction::reactionPaid(
            telegram_api::reactionPaid {},
        )));
        assert!(reaction.is_paid_reaction());

        // reactionEmpty and the null pointer stay empty
        assert!(
            ReactionType::from_telegram(Some(&telegram_api::Reaction::reactionEmpty(
                telegram_api::reactionEmpty {}
            )))
            .is_empty()
        );
        assert!(ReactionType::from_telegram(None).is_empty());

        // td_api forms
        let reaction = ReactionType::from_td_api(Some(td_api::ReactionType::reactionTypeEmoji(
            td_api::reactionTypeEmoji {
                emoji: "👍".to_owned(),
            },
        )));
        assert_eq!(reaction.get_string(), "👍");
        let reaction = ReactionType::from_td_api(Some(
            td_api::ReactionType::reactionTypeCustomEmoji(td_api::reactionTypeCustomEmoji {
                custom_emoji_id: 543210987654321,
            }),
        ));
        assert_eq!(reaction.get_string(), "#sQz1KQzuAQA=");
        let reaction = ReactionType::from_td_api(Some(td_api::ReactionType::reactionTypePaid(
            td_api::reactionTypePaid {},
        )));
        assert!(reaction.is_paid_reaction());
        assert!(ReactionType::from_td_api(None).is_empty());
        // the '#'/'$' td_api emojis are emptied like on the wire
        let reaction = ReactionType::from_td_api(Some(td_api::ReactionType::reactionTypeEmoji(
            td_api::reactionTypeEmoji {
                emoji: "$".to_owned(),
            },
        )));
        assert!(reaction.is_empty());
    }

    #[test]
    fn object_builders_round_trip_through_the_encodings() {
        let types = vec![
            ReactionType::new("👍".to_owned()),
            ReactionType::from_telegram(Some(&telegram_api::Reaction::reactionCustomEmoji(
                telegram_api::reactionCustomEmoji {
                    document_id: 543210987654321,
                },
            ))),
            ReactionType::paid(),
        ];
        for reaction_type in &types {
            let object = reaction_type.get_reaction_type_object().unwrap();
            assert_eq!(&ReactionType::from_td_api(Some(object)), reaction_type);
            let input = reaction_type.get_input_reaction();
            assert_eq!(&ReactionType::from_telegram(Some(&input)), reaction_type);
        }

        // empty → null objects
        let empty = ReactionType::default();
        assert!(empty.get_reaction_type_object().is_none());
        assert!(empty.get_update_default_reaction_type().is_none());
        assert!(matches!(
            empty.get_input_reaction(),
            telegram_api::Reaction::reactionEmpty(_)
        ));

        // the paid reaction is prepended when available
        let objects = ReactionType::get_reaction_types_object(&types, true);
        assert_eq!(objects.len(), 4);
        assert!(matches!(
            &objects[0],
            td_api::ReactionType::reactionTypePaid(_)
        ));
        let objects = ReactionType::get_reaction_types_object(&types, false);
        assert_eq!(objects.len(), 3);

        // get_update_default_reaction_type wraps the object
        let update = ReactionType::new("👍".to_owned())
            .get_update_default_reaction_type()
            .unwrap();
        let td_api::Update::updateDefaultReactionType(update) = update else {
            panic!("wrong update kind");
        };
        assert!(matches!(
            update.reaction_type,
            Some(td_api::ReactionType::reactionTypeEmoji(_))
        ));
    }

    #[test]
    fn hashes_match_the_cpp_folds() {
        // get_md5_string_hash/remove_emoji_selectors vectors computed from
        // the C++ algorithm definitions
        assert_eq!(
            ReactionType::new("👍".to_owned()).get_hash(),
            150215612170160887
        );
        let custom = ReactionType::new("#sQz1KQzuAQA=".to_owned());
        assert_eq!(custom.get_hash(), 543210987654321);
        assert_eq!(
            get_reaction_types_hash(&[ReactionType::new("👍".to_owned())]),
            34974797
        );
        assert_eq!(
            get_reaction_types_hash(std::slice::from_ref(&custom)),
            4370997511126429
        );
        assert_eq!(
            get_reaction_types_hash(&[ReactionType::new("👍".to_owned()), custom]),
            -2021233334811694481
        );
    }

    #[test]
    fn ordering_puts_paid_first_then_byte_compares() {
        let paid = ReactionType::paid();
        let emoji = ReactionType::new("👍".to_owned());
        let custom = ReactionType::new("#sQz1KQzuAQA=".to_owned());
        assert!(paid < emoji);
        assert!(paid < custom);
        assert!(custom < emoji); // '#' (0x23) < the emoji's leading 0xf0 byte
        assert_eq!(emoji, ReactionType::new("👍".to_owned()));

        // the derived sorts match the C++ operator< exactly
        let mut types = vec![emoji.clone(), paid.clone(), custom.clone()];
        types.sort();
        assert_eq!(types, vec![paid, custom, emoji]);
    }

    #[test]
    fn store_parse_round_trips() {
        let reaction_type = ReactionType::new("#sQz1KQzuAQA=".to_owned());
        let mut out = Vec::new();
        reaction_type.store(&mut out);
        let mut parser = Parser::new(&out);
        let parsed = ReactionType::parse(&mut parser).unwrap();
        parser.fetch_end().unwrap();
        assert_eq!(parsed, reaction_type);
    }
}
