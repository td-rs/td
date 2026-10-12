//! Port of `td/telegram/KeyboardButtonStyle.{h,cpp,hpp}` — the visual
//! style of a keyboard button: one of the three colored backgrounds (or
//! the default), plus an optional custom-emoji icon.
//!
//! The C++ `buttonStyleLink` td_api constructor is parsed but "never
//! allowed" (KeyboardButtonStyle.cpp:23-25) — it collapses into
//! [`KeyboardButtonStyleType::Default`] exactly like a null style.

use crate::emoji_status::CustomEmojiId;
use crate::td_api;
use tdmtproto::telegram_api;

/// `td::KeyboardButtonStyle::Type` (KeyboardButtonStyle.h:19) — append
/// only; the discriminants ride the store/parse format.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum KeyboardButtonStyleType {
    #[default]
    Default,
    Primary,
    Danger,
    Success,
}

impl KeyboardButtonStyleType {
    fn as_i32(self) -> i32 {
        match self {
            KeyboardButtonStyleType::Default => 0,
            KeyboardButtonStyleType::Primary => 1,
            KeyboardButtonStyleType::Danger => 2,
            KeyboardButtonStyleType::Success => 3,
        }
    }

    fn from_i32(value: i32) -> KeyboardButtonStyleType {
        match value {
            1 => KeyboardButtonStyleType::Primary,
            2 => KeyboardButtonStyleType::Danger,
            3 => KeyboardButtonStyleType::Success,
            // 0 and every unknown value read back as the default, the way
            // the C++ int32-backed enum behaves.
            _ => KeyboardButtonStyleType::Default,
        }
    }
}

/// `td::KeyboardButtonStyle` (KeyboardButtonStyle.h:18).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KeyboardButtonStyle {
    pub(crate) type_: KeyboardButtonStyleType,
    pub(crate) icon_custom_emoji_id: CustomEmojiId,
}

impl KeyboardButtonStyle {
    /// `KeyboardButtonStyle(td_api::object_ptr<td_api::ButtonStyle> &&,
    /// int64)` (KeyboardButtonStyle.cpp:11-32).
    pub fn from_td_api(
        style: Option<td_api::ButtonStyle>,
        icon_custom_emoji_id: i64,
    ) -> KeyboardButtonStyle {
        let mut result = KeyboardButtonStyle {
            icon_custom_emoji_id: CustomEmojiId::new(icon_custom_emoji_id),
            ..KeyboardButtonStyle::default()
        };
        let Some(style) = style else {
            return result;
        };
        result.type_ = match style {
            td_api::ButtonStyle::buttonStyleDefault(_) => KeyboardButtonStyleType::Default,
            td_api::ButtonStyle::buttonStylePrimary(_) => KeyboardButtonStyleType::Primary,
            td_api::ButtonStyle::buttonStyleDanger(_) => KeyboardButtonStyleType::Danger,
            td_api::ButtonStyle::buttonStyleSuccess(_) => KeyboardButtonStyleType::Success,
            // buttonStyleLink is never allowed
            td_api::ButtonStyle::buttonStyleLink(_) => KeyboardButtonStyleType::Default,
        };
        result
    }

    /// `explicit KeyboardButtonStyle(telegram_api::object_ptr<
    /// telegram_api::keyboardButtonStyle> &&)`
    /// (KeyboardButtonStyle.cpp:34-44).
    pub fn from_telegram(style: Option<telegram_api::KeyboardButtonStyle>) -> KeyboardButtonStyle {
        let Some(style) = style else {
            return KeyboardButtonStyle::default();
        };
        let mut result = KeyboardButtonStyle {
            icon_custom_emoji_id: CustomEmojiId::new(style.icon),
            ..KeyboardButtonStyle::default()
        };
        // The raw flags word and the derived bools agree on read; the
        // priority order below is the C++ if/else-if ladder.
        if style.bg_primary {
            result.type_ = KeyboardButtonStyleType::Primary;
        } else if style.bg_danger {
            result.type_ = KeyboardButtonStyleType::Danger;
        } else if style.bg_success {
            result.type_ = KeyboardButtonStyleType::Success;
        }
        result
    }

    /// `is_default` (KeyboardButtonStyle.h:34-36).
    pub fn is_default(&self) -> bool {
        self.type_ == KeyboardButtonStyleType::Default && !self.icon_custom_emoji_id.is_valid()
    }

    /// `get_icon_custom_emoji_id` (KeyboardButtonStyle.h:40-42).
    pub fn get_icon_custom_emoji_id(&self) -> CustomEmojiId {
        self.icon_custom_emoji_id
    }

    /// `get_button_style_object` (KeyboardButtonStyle.cpp:46-59).
    pub fn get_button_style_object(&self) -> td_api::ButtonStyle {
        match self.type_ {
            KeyboardButtonStyleType::Default => {
                td_api::ButtonStyle::buttonStyleDefault(td_api::buttonStyleDefault {})
            }
            KeyboardButtonStyleType::Primary => {
                td_api::ButtonStyle::buttonStylePrimary(td_api::buttonStylePrimary {})
            }
            KeyboardButtonStyleType::Danger => {
                td_api::ButtonStyle::buttonStyleDanger(td_api::buttonStyleDanger {})
            }
            KeyboardButtonStyleType::Success => {
                td_api::ButtonStyle::buttonStyleSuccess(td_api::buttonStyleSuccess {})
            }
        }
    }

    /// `get_input_keyboard_button_style` (KeyboardButtonStyle.cpp:61-71):
    /// a null wire object while the style is the default one (the button
    /// then leaves the `keyboardButton.style` flag unset).
    pub fn get_input_keyboard_button_style(&self) -> Option<telegram_api::KeyboardButtonStyle> {
        if self.is_default() {
            return None;
        }
        let flags = if self.icon_custom_emoji_id.is_valid() {
            1 << 3 // ICON_MASK
        } else {
            0
        };
        Some(telegram_api::KeyboardButtonStyle {
            flags,
            bg_primary: self.type_ == KeyboardButtonStyleType::Primary,
            bg_danger: self.type_ == KeyboardButtonStyleType::Danger,
            bg_success: self.type_ == KeyboardButtonStyleType::Success,
            icon: self.icon_custom_emoji_id.get(),
        })
    }

    /// `KeyboardButtonStyle::store` (KeyboardButtonStyle.hpp:18-38).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_type = self.type_ != KeyboardButtonStyleType::Default;
        let has_icon_custom_emoji_id = self.icon_custom_emoji_id.is_valid();
        crate::store_parse::store_flags(out, [has_type, has_icon_custom_emoji_id]);
        if has_type {
            crate::store_parse::store_i32(out, self.type_.as_i32());
        }
        if has_icon_custom_emoji_id {
            crate::store_parse::store_i64(out, self.icon_custom_emoji_id.get());
        }
    }

    /// `KeyboardButtonStyle::parse` (KeyboardButtonStyle.hpp:40-56).
    pub fn parse(
        parser: &mut crate::store_parse::Parser<'_>,
    ) -> Result<KeyboardButtonStyle, &'static str> {
        let [has_type, has_icon_custom_emoji_id] = parser.parse_flags::<2>()?;
        let type_ = if has_type {
            KeyboardButtonStyleType::from_i32(parser.i32_()?)
        } else {
            KeyboardButtonStyleType::Default
        };
        let icon_custom_emoji_id = if has_icon_custom_emoji_id {
            CustomEmojiId::new(parser.i64_()?)
        } else {
            CustomEmojiId::default()
        };
        Ok(KeyboardButtonStyle {
            type_,
            icon_custom_emoji_id,
        })
    }

    /// `operator<<` (KeyboardButtonStyle.cpp:88-114) — empty for the
    /// default style, otherwise `", <Name>[<icon id>]"`.
    pub fn print(&self, out: &mut String) {
        if self.is_default() {
            return;
        }
        out.push_str(", ");
        out.push_str(match self.type_ {
            KeyboardButtonStyleType::Default => "Default",
            KeyboardButtonStyleType::Primary => "Primary",
            KeyboardButtonStyleType::Danger => "Danger",
            KeyboardButtonStyleType::Success => "Success",
        });
        if self.icon_custom_emoji_id.is_valid() {
            out.push_str(&self.icon_custom_emoji_id.get().to_string());
        }
    }
}

impl std::fmt::Display for KeyboardButtonStyle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut out = String::new();
        self.print(&mut out);
        f.write_str(&out)
    }
}

#[cfg(test)]
mod tests {
    use crate::store_parse::Parser;

    use super::*;

    #[test]
    fn builds_from_td_api_and_back() {
        let style = KeyboardButtonStyle::from_td_api(
            Some(td_api::ButtonStyle::buttonStyleDanger(
                td_api::buttonStyleDanger {},
            )),
            0,
        );
        assert_eq!(style.type_, KeyboardButtonStyleType::Danger);
        assert!(!style.is_default());
        match style.get_button_style_object() {
            td_api::ButtonStyle::buttonStyleDanger(_) => {}
            other => panic!("wrong round trip: {other:?}"),
        }

        // buttonStyleLink is never allowed and collapses into the default.
        let link_style = KeyboardButtonStyle::from_td_api(
            Some(td_api::ButtonStyle::buttonStyleLink(
                td_api::buttonStyleLink {},
            )),
            0,
        );
        assert_eq!(link_style, KeyboardButtonStyle::default());
        assert!(link_style.is_default());
    }

    #[test]
    fn icon_custom_emoji_id_defaults() {
        let style = KeyboardButtonStyle::from_td_api(None, 12345);
        assert!(!style.is_default());
        assert_eq!(style.get_icon_custom_emoji_id().get(), 12345);
        let empty = KeyboardButtonStyle::from_td_api(None, 0);
        assert!(empty.is_default());
        // A null style with a valid icon still carries no background color
        // but keeps the icon flag.
        let icon_only = KeyboardButtonStyle::from_td_api(None, 12345);
        assert_eq!(
            icon_only.get_input_keyboard_button_style(),
            Some(telegram_api::KeyboardButtonStyle {
                flags: 1 << 3,
                bg_primary: false,
                bg_danger: false,
                bg_success: false,
                icon: 12345,
            })
        );
    }

    #[test]
    fn wire_round_trip_and_store_round_trip() {
        let style = KeyboardButtonStyle::from_telegram(Some(telegram_api::KeyboardButtonStyle {
            flags: (1 << 0) | (1 << 3),
            bg_primary: true,
            bg_danger: false,
            bg_success: false,
            icon: 4242,
        }));
        assert_eq!(style.type_, KeyboardButtonStyleType::Primary);
        assert_eq!(style.get_icon_custom_emoji_id().get(), 4242);

        // The wire output carries both the raw flags word and the derived
        // bools; the C++ builder only ever writes the ICON_MASK bit into
        // the flags (the codegen store ORs the bools back in).
        let wire = style
            .get_input_keyboard_button_style()
            .expect("non-default");
        assert_eq!(
            wire,
            telegram_api::KeyboardButtonStyle {
                flags: 1 << 3,
                bg_primary: true,
                bg_danger: false,
                bg_success: false,
                icon: 4242,
            }
        );

        let mut out = Vec::new();
        style.store(&mut out);
        let parsed = KeyboardButtonStyle::parse(&mut Parser::new(&out)).expect("parse");
        assert_eq!(parsed, style);
        // A default style stores as a bare empty-flags word.
        let mut default_out = Vec::new();
        KeyboardButtonStyle::default().store(&mut default_out);
        assert_eq!(default_out, [0, 0, 0, 0]);
        let parsed =
            KeyboardButtonStyle::parse(&mut Parser::new(&default_out)).expect("parse default");
        assert!(parsed.is_default());
    }

    #[test]
    fn display_matches_the_cpp_stream_output() {
        let style = KeyboardButtonStyle::from_td_api(
            Some(td_api::ButtonStyle::buttonStyleSuccess(
                td_api::buttonStyleSuccess {},
            )),
            77,
        );
        assert_eq!(style.to_string(), ", Success77");
        assert_eq!(KeyboardButtonStyle::default().to_string(), "");
    }
}
