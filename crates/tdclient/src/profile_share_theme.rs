//! The port of `td/telegram/ProfileShareTheme.{h,cpp,hpp}` — the profile
//! share-theme value carried by the user/channel full records and set by
//! `setProfileShareTheme` (upstream 1.8.70, the #124 profile-themes
//! family: 4a01cdc88 the td_api type (scheme-regen side), 91b786cf6 the
//! class, cef469622 the storer/parser, 9e36a4eb8 add_dependencies,
//! 3563ba8bb the userFullInfo plane, cb74e789c the supergroupFullInfo
//! plane, 7b3922ab3 the `get_profile_share_theme` helper, 2a80e059e the
//! default-theme removal; the request surface lives in requests.rs —
//! 94fa3af8b/555069da3 the two arms, 7feac4bf5 the set refetch tail,
//! 6e7fcda8f the backdrops-for-bots CHECK_IS_USER removal, and the
//! backdrops cache in theme_manager.rs — e8601c748/319f2a116).
//!
//! Divergences (the recorded seams):
//! - The gift variant rides the StarGift conversion seam. The C++
//!   constructor converts the wire gift through
//!   `StarGift(td, stargift, true)` (ProfileShareTheme.cpp:71) and the
//!   object emission through `get_upgraded_gift_object`
//!   (ProfileShareTheme.cpp:115); both need the manager surfaces the
//!   user/channel-full intakes don't carry (the `UserEnv` collapse).
//!   [`ShareThemeEnv::star_gift_for_share_theme`] models the intake
//!   seam ([blocked: the StarGift-in-user-env seam], the sibling
//!   ChatTheme band shape —
//!   [`crate::theme_manager::ThemeManagerEnv::star_gift_object_for_theme`]),
//!   so a gift theme answers Default exactly like the C++ invalid-gift
//!   bail (ProfileShareTheme.cpp:72-75). The object emission answers
//!   None for the gift type for the same reason.
//! - The store/parse keeps the C++ flag layout byte-exact. A valid
//!   gift value can't be constructed while the intake seam parks, so
//!   the `has_star_gift` body never stores; the parse rejects a
//!   C++-written gift row cleanly (the standard repair path) until the
//!   seam lands.
//! - `add_dependencies` (ProfileShareTheme.cpp:121-125) rides the
//!   comment-modeled dependency bands of the user/channel-full loads
//!   (the star-gift dialog dependencies arrive with the Dependencies
//!   plane).

use crate::star_gift_manager::StarGift;
use crate::stickers_manager::CustomEmojiId;
use crate::store_parse::{self, Parser};
use crate::td_api;
use tdmtproto::telegram_api;

/// `ProfileShareTheme::MAX_EMOJI_COUNT` (ProfileShareTheme.h:22).
pub(crate) const MAX_EMOJI_COUNT: usize = 6;

/// `ProfileShareTheme::Type` (ProfileShareTheme.h:24).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Type {
    #[default]
    Default = 0,
    Emoji = 1,
    Gift = 2,
}

impl Type {
    fn as_raw(self) -> i32 {
        self as i32
    }

    /// `td::parse(type_, parser)` reads the raw int32; unknown values
    /// answer Default (the C++ keeps the raw value and trips the
    /// UNREACHABLEs later).
    fn from_raw(raw: i32) -> Type {
        match raw {
            1 => Type::Emoji,
            2 => Type::Gift,
            _ => Type::Default,
        }
    }
}

/// `ProfileShareTheme::ThemeEmoji` (ProfileShareTheme.h:27-40) — the
/// regular-emoji / custom-emoji pair.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ThemeEmoji {
    /// for regular emoji
    emoji: String,
    /// for custom emoji
    custom_emoji_id: CustomEmojiId,
}

impl ThemeEmoji {
    /// `ThemeEmoji::store` (ProfileShareTheme.hpp:18-31).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_emoji = !self.emoji.is_empty();
        let has_custom_emoji_id = self.custom_emoji_id.is_valid();
        store_parse::store_flags(out, [has_emoji, has_custom_emoji_id]);
        if has_emoji {
            store_parse::store_string(out, &self.emoji);
        }
        if has_custom_emoji_id {
            store_parse::store_i64(out, self.custom_emoji_id.get());
        }
    }

    /// `ThemeEmoji::parse` (ProfileShareTheme.hpp:34-47).
    pub fn parse(parser: &mut Parser) -> Result<ThemeEmoji, &'static str> {
        let [has_emoji, has_custom_emoji_id] = parser.parse_flags()?;
        let mut result = ThemeEmoji::default();
        if has_emoji {
            result.emoji = parser.string()?.to_owned();
        }
        if has_custom_emoji_id {
            result.custom_emoji_id = CustomEmojiId::new(parser.i64_()?);
        }
        Result::Ok(result)
    }
}

/// The share-theme intake seam — the narrow slice of
/// `StarGift(td, stargift, true)` the user/channel-full intakes need
/// (see the module docs for the blocked posture).
pub trait ShareThemeEnv {
    /// `StarGift(td, std::move(share_theme->stargift_), true)`
    /// (ProfileShareTheme.cpp:71) — `None` models the C++ invalid /
    /// not-unique bail (the whole theme answers Default).
    fn star_gift_for_share_theme(&mut self, gift: telegram_api::StarGift) -> Option<StarGift> {
        let _ = gift;
        tdutils::log_info!(
            "StarGift for the share theme — [blocked: the StarGift-in-user-env seam]"
        );
        None
    }
}

/// The port of `td::ProfileShareTheme`.
#[derive(Clone, Debug, Default)]
pub struct ProfileShareTheme {
    r#type: Type,
    /// for Emoji
    emojis: Vec<ThemeEmoji>,
    /// for Emoji
    use_default_background: bool,
    /// for Emoji
    backdrop_id: i32,
    /// for Gift — the C++ value member; always invalid while the
    /// intake seam parks (the module docs).
    star_gift: StarGift,
}

impl PartialEq for ProfileShareTheme {
    /// `operator==` (ProfileShareTheme.cpp:127-131).
    fn eq(&self, other: &Self) -> bool {
        self.r#type == other.r#type
            && self.emojis == other.emojis
            && self.use_default_background == other.use_default_background
            && self.backdrop_id == other.backdrop_id
            && self.star_gift == other.star_gift
    }
}

impl ProfileShareTheme {
    /// `ProfileShareTheme::ProfileShareTheme(Td *, telegram_api::ShareTheme)`
    /// (ProfileShareTheme.cpp:16-86).
    pub fn from_telegram(
        env: &mut impl ShareThemeEnv,
        theme: Option<telegram_api::ShareTheme>,
    ) -> ProfileShareTheme {
        let Some(theme) = theme else {
            return ProfileShareTheme::default();
        };
        match theme {
            telegram_api::ShareTheme::shareThemeDefault(_) => ProfileShareTheme::default(),
            telegram_api::ShareTheme::shareThemeEmojiBackdrop(share_theme) => {
                let mut emojis = Vec::new();
                for emoji_ptr in share_theme.emojis {
                    match emoji_ptr {
                        telegram_api::Reaction::reactionEmoji(emoji) => {
                            if emoji.emoticon.is_empty() {
                                tdutils::log_error!("Receive empty emoji in a profile share theme");
                                continue;
                            }
                            emojis.push(ThemeEmoji {
                                emoji: emoji.emoticon,
                                custom_emoji_id: CustomEmojiId::default(),
                            });
                        }
                        telegram_api::Reaction::reactionCustomEmoji(emoji) => {
                            let custom_emoji_id = CustomEmojiId::new(emoji.document_id);
                            if !custom_emoji_id.is_valid() {
                                tdutils::log_error!(
                                    "Receive invalid custom emoji in a profile share theme"
                                );
                                continue;
                            }
                            emojis.push(ThemeEmoji {
                                emoji: String::new(),
                                custom_emoji_id,
                            });
                        }
                        // the C++ default band logs the full wire object
                        // (cpp:48-51); the port logs the band without the dump
                        emoji_ptr => {
                            tdutils::log_error!("Receive an unsupported profile share theme emoji");
                            let _ = emoji_ptr;
                        }
                    }
                }
                if emojis.len() > MAX_EMOJI_COUNT {
                    tdutils::log_error!("Receive {} emoji in a profile share theme", emojis.len());
                    // the C++ `return` keeps the theme Default
                    return ProfileShareTheme::default();
                }
                let use_default_background = (share_theme.flags & 1) == 0;
                if emojis.is_empty() && use_default_background {
                    tdutils::log_error!("Receive empty profile share theme");
                    return ProfileShareTheme::default();
                }
                ProfileShareTheme {
                    r#type: Type::Emoji,
                    emojis,
                    use_default_background,
                    backdrop_id: share_theme.backdrop_id,
                    star_gift: StarGift::default(),
                }
            }
            telegram_api::ShareTheme::shareThemeGift(share_theme) => {
                let Some(star_gift) = share_theme
                    .stargift
                    .and_then(|gift| env.star_gift_for_share_theme(gift))
                else {
                    // the C++ invalid/not-unique bail (cpp:72-75)
                    tdutils::log_error!("Receive profile share theme with an invalid star gift");
                    return ProfileShareTheme::default();
                };
                if !star_gift.is_valid() || !star_gift.is_unique() {
                    tdutils::log_error!("Receive profile share theme with an invalid star gift");
                    return ProfileShareTheme::default();
                }
                ProfileShareTheme {
                    r#type: Type::Gift,
                    star_gift,
                    ..ProfileShareTheme::default()
                }
            }
            // the C++ LOG(ERROR) band (cpp:80-82)
            telegram_api::ShareTheme::inputShareThemeGift(_) => {
                tdutils::log_error!("Receive an input profile share theme from the server");
                ProfileShareTheme::default()
            }
        }
    }

    /// `ProfileShareTheme::get_profile_share_theme`
    /// (ProfileShareTheme.cpp:88-98) — `None` for the null wire theme
    /// and for the Default-typed results (the invalid bails).
    pub fn get_profile_share_theme(
        env: &mut impl ShareThemeEnv,
        theme: Option<telegram_api::ShareTheme>,
    ) -> Option<ProfileShareTheme> {
        let theme = theme?;
        let result = ProfileShareTheme::from_telegram(env, Some(theme));
        if result.is_default() {
            return None;
        }
        Some(result)
    }

    /// `ProfileShareTheme::is_default` (ProfileShareTheme.h:59-61).
    pub fn is_default(&self) -> bool {
        self.r#type == Type::Default
    }

    /// `ProfileShareTheme::get_profile_share_theme_object`
    /// (ProfileShareTheme.cpp:100-119) — `None` for the Default type
    /// (the C++ nullptr). The gift emission is the recorded seam (the
    /// module docs).
    pub fn get_profile_share_theme_object(&self) -> Option<td_api::ProfileShareTheme> {
        match self.r#type {
            Type::Default => None,
            Type::Emoji => {
                let emojis = self
                    .emojis
                    .iter()
                    .map(|emoji| {
                        if emoji.custom_emoji_id.is_valid() {
                            td_api::EmojiType::emojiTypeCustomEmoji(td_api::emojiTypeCustomEmoji {
                                custom_emoji_id: emoji.custom_emoji_id.get(),
                            })
                        } else {
                            td_api::EmojiType::emojiTypeRegular(td_api::emojiTypeRegular {
                                emoji: emoji.emoji.clone(),
                            })
                        }
                    })
                    .collect();
                Some(td_api::ProfileShareTheme::profileShareThemeEmoji(
                    td_api::profileShareThemeEmoji {
                        emojis,
                        use_default_background: self.use_default_background,
                        background_backdrop_id: self.backdrop_id,
                    },
                ))
            }
            Type::Gift => {
                // star_gift_.get_upgraded_gift_object(td)
                // (ProfileShareTheme.cpp:115) — [blocked: the
                // StarGift-in-user-env seam]; unreachable while the
                // intake seam parks the gift conversion.
                tdutils::log_error!("Share theme gift object needs the StarGift seam");
                None
            }
        }
    }

    /// `ProfileShareTheme::store` (ProfileShareTheme.hpp:50-74).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_type = self.r#type != Type::Default;
        let has_emojis = !self.emojis.is_empty();
        let has_backdrop_id = self.backdrop_id != 0;
        // the C++ star_gift_.is_valid() flag — the value is always
        // invalid while the intake seam parks (the module docs), so the
        // gift body never stores.
        let has_star_gift = self.star_gift.is_valid();
        store_parse::store_flags(
            out,
            [
                has_type,
                has_emojis,
                self.use_default_background,
                has_backdrop_id,
                has_star_gift,
            ],
        );
        if has_type {
            store_parse::store_i32(out, self.r#type.as_raw());
        }
        if has_emojis {
            store_parse::store_i32(out, self.emojis.len() as i32);
            for emoji in &self.emojis {
                emoji.store(out);
            }
        }
        if has_backdrop_id {
            store_parse::store_i32(out, self.backdrop_id);
        }
        if has_star_gift {
            // td::store(star_gift_, storer) — [blocked: the StarGift
            // store seam]; unreachable while the intake seam parks.
        }
    }

    /// `ProfileShareTheme::parse` (ProfileShareTheme.hpp:77-101).
    pub fn parse(parser: &mut Parser) -> Result<ProfileShareTheme, &'static str> {
        let [
            has_type,
            has_emojis,
            use_default_background,
            has_backdrop_id,
            has_star_gift,
        ] = parser.parse_flags()?;
        if has_star_gift {
            return Err("share-theme gift needs the StarGift seam (profile-themes family)");
        }
        let mut result = ProfileShareTheme::default();
        if has_type {
            result.r#type = Type::from_raw(parser.i32_()?);
        }
        if has_emojis {
            let size = parser.i32_()?;
            for _ in 0..size {
                result.emojis.push(ThemeEmoji::parse(parser)?);
            }
        }
        result.use_default_background = use_default_background;
        if has_backdrop_id {
            result.backdrop_id = parser.i32_()?;
        }
        Result::Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The empty theme parses and answers Default/None.
    #[test]
    fn profile_share_theme_default_round_trip() {
        let theme = ProfileShareTheme::default();
        assert!(theme.is_default());
        assert!(theme.get_profile_share_theme_object().is_none());

        let mut out = Vec::new();
        theme.store(&mut out);
        let parsed = ProfileShareTheme::parse(&mut Parser::new(&out)).unwrap();
        assert_eq!(theme, parsed);
    }

    /// The emoji theme round-trips through the C++ flag layout
    /// (ProfileShareTheme.hpp:50-74): the flags word, the optional
    /// type/emojis/backdrop bodies.
    #[test]
    fn profile_share_theme_emoji_round_trip() {
        let wire = telegram_api::ShareTheme::shareThemeEmojiBackdrop(
            telegram_api::shareThemeEmojiBackdrop {
                flags: 1, // the backdrop id mask
                backdrop_id: 5,
                emojis: vec![
                    telegram_api::Reaction::reactionEmoji(telegram_api::reactionEmoji {
                        emoticon: "\u{1f600}".to_owned(),
                    }),
                    telegram_api::Reaction::reactionCustomEmoji(
                        telegram_api::reactionCustomEmoji {
                            document_id: 1 << 21,
                        },
                    ),
                ],
            },
        );
        let theme = ProfileShareTheme::get_profile_share_theme(&mut TestEnv, Some(wire))
            .expect("the emoji theme is not default");
        assert!(!theme.use_default_background);
        assert_eq!(theme.backdrop_id, 5);
        assert_eq!(theme.emojis.len(), 2);
        let object = theme.get_profile_share_theme_object().unwrap();
        let td_api::ProfileShareTheme::profileShareThemeEmoji(emoji) = &object else {
            panic!("expected the emoji variant");
        };
        assert!(!emoji.use_default_background);
        assert_eq!(emoji.background_backdrop_id, 5);
        assert_eq!(emoji.emojis.len(), 2);

        let mut out = Vec::new();
        theme.store(&mut out);
        let parsed = ProfileShareTheme::parse(&mut Parser::new(&out)).unwrap();
        assert_eq!(theme, parsed);
    }

    /// More than the C++ MAX_EMOJI_COUNT bails to Default (the
    /// ProfileShareTheme.cpp:53-56 `return`).
    #[test]
    fn profile_share_theme_too_many_emojis_bails() {
        let wire = telegram_api::ShareTheme::shareThemeEmojiBackdrop(
            telegram_api::shareThemeEmojiBackdrop {
                flags: 0,
                backdrop_id: 0,
                emojis: (0..MAX_EMOJI_COUNT + 1)
                    .map(|i| {
                        telegram_api::Reaction::reactionEmoji(telegram_api::reactionEmoji {
                            emoticon: char::from_u32(0x1F600 + i as u32)
                                .unwrap_or('\u{1F600}')
                                .to_string(),
                        })
                    })
                    .collect(),
            },
        );
        let theme = ProfileShareTheme::from_telegram(&mut TestEnv, Some(wire));
        assert!(theme.is_default());
    }

    /// The empty wire gift and the input-from-server gift answer
    /// Default through the seam (the cpp:69-82 bands).
    #[test]
    fn profile_share_theme_gift_seam_bails() {
        let wire = telegram_api::ShareTheme::shareThemeGift(telegram_api::shareThemeGift {
            stargift: None,
        });
        let theme = ProfileShareTheme::from_telegram(&mut TestEnv, Some(wire));
        assert!(theme.is_default());

        let wire =
            telegram_api::ShareTheme::inputShareThemeGift(telegram_api::inputShareThemeGift {
                stargift: None,
            });
        let theme = ProfileShareTheme::from_telegram(&mut TestEnv, Some(wire));
        assert!(theme.is_default());
    }

    struct TestEnv;

    impl ShareThemeEnv for TestEnv {}
}
