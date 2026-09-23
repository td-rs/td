//! Port of `td/telegram/RichButtonStyle.{h,cpp,hpp}` — the rich-text
//! button style (the `textButton`/`pageButton` style plane the WebPage
//! machinery consumes; the message-plane sibling is
//! [`crate::keyboard_button_style`]).
//!
//! Landed as the M.17 WebPageBlock dependency plane inside
//! `inline_keyboard_button.rs`; the M.22 unification moved it to its
//! own module to mirror the C++ file split.

use crate::store_parse::{Parser, store_flags, store_i32};
use crate::td_api;
use tdmtproto::telegram_api;


/// `RichButtonStyle::Type` (RichButtonStyle.h:24).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RichButtonStyleType {
    #[default]
    Default,
    Primary,
    Danger,
    Success,
    Link,
}

/// `td::RichButtonStyle` (RichButtonStyle.h:26).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RichButtonStyle {
    pub(crate) type_: RichButtonStyleType,
}

impl RichButtonStyle {
    /// `RichButtonStyle(td_api::ButtonStyle)` (RichButtonStyle.cpp:11-34).
    pub fn from_td_api(style: Option<td_api::ButtonStyle>) -> RichButtonStyle {
        let Some(style) = style else {
            return RichButtonStyle::default();
        };
        let type_ = match style {
            td_api::ButtonStyle::buttonStyleDefault(_) => RichButtonStyleType::Default,
            td_api::ButtonStyle::buttonStylePrimary(_) => RichButtonStyleType::Primary,
            td_api::ButtonStyle::buttonStyleDanger(_) => RichButtonStyleType::Danger,
            td_api::ButtonStyle::buttonStyleSuccess(_) => RichButtonStyleType::Success,
            td_api::ButtonStyle::buttonStyleLink(_) => RichButtonStyleType::Link,
        };
        RichButtonStyle { type_ }
    }

    /// `RichButtonStyle(telegram_api::richButtonStyle)` (RichButtonStyle.cpp:36-49).
    pub fn from_telegram(style: Option<telegram_api::RichButtonStyle>) -> RichButtonStyle {
        let Some(style) = style else {
            return RichButtonStyle::default();
        };
        let mut result = RichButtonStyle::default();
        if style.bg_primary {
            result.type_ = RichButtonStyleType::Primary;
        } else if style.bg_danger {
            result.type_ = RichButtonStyleType::Danger;
        } else if style.bg_success {
            result.type_ = RichButtonStyleType::Success;
        } else if style.link {
            result.type_ = RichButtonStyleType::Link;
        }
        result
    }

    /// `is_default` (RichButtonStyle.h:44-46).
    pub fn is_default(&self) -> bool {
        self.type_ == RichButtonStyleType::Default
    }

    /// `get_button_style_object` (RichButtonStyle.cpp:51-67).
    pub fn get_button_style_object(&self) -> Option<td_api::ButtonStyle> {
        match self.type_ {
            RichButtonStyleType::Default => Some(td_api::ButtonStyle::buttonStyleDefault(
                td_api::buttonStyleDefault {},
            )),
            RichButtonStyleType::Primary => Some(td_api::ButtonStyle::buttonStylePrimary(
                td_api::buttonStylePrimary {},
            )),
            RichButtonStyleType::Danger => Some(td_api::ButtonStyle::buttonStyleDanger(
                td_api::buttonStyleDanger {},
            )),
            RichButtonStyleType::Success => Some(td_api::ButtonStyle::buttonStyleSuccess(
                td_api::buttonStyleSuccess {},
            )),
            RichButtonStyleType::Link => Some(td_api::ButtonStyle::buttonStyleLink(
                td_api::buttonStyleLink {},
            )),
        }
    }

    /// `get_input_rich_button_style` (RichButtonStyle.cpp:69-75).
    pub fn get_input_rich_button_style(&self) -> Option<telegram_api::RichButtonStyle> {
        if self.is_default() {
            return None;
        }
        Some(telegram_api::RichButtonStyle {
            flags: 0,
            bg_primary: self.type_ == RichButtonStyleType::Primary,
            bg_danger: self.type_ == RichButtonStyleType::Danger,
            bg_success: self.type_ == RichButtonStyleType::Success,
            link: self.type_ == RichButtonStyleType::Link,
        })
    }

    /// `RichButtonStyle::store` (RichButtonStyle.hpp:14-27).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_type = self.type_ != RichButtonStyleType::Default;
        store_flags(out, [has_type]);
        if has_type {
            store_i32(out, self.type_ as i32);
        }
    }

    /// `RichButtonStyle::parse` (RichButtonStyle.hpp:29-42).
    pub fn parse(parser: &mut Parser<'_>) -> Result<RichButtonStyle, &'static str> {
        let [has_type] = parser.parse_flags()?;
        let mut result = RichButtonStyle::default();
        if has_type {
            let type_ = parser.i32_()?;
            result.type_ = match type_ {
                0 => RichButtonStyleType::Default,
                1 => RichButtonStyleType::Primary,
                2 => RichButtonStyleType::Danger,
                3 => RichButtonStyleType::Success,
                4 => RichButtonStyleType::Link,
                _ => return Err("Unknown RichButtonStyle type"),
            };
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn td_api_style_table() {
        assert_eq!(
            RichButtonStyle::from_td_api(Some(td_api::ButtonStyle::buttonStyleDefault(
                td_api::buttonStyleDefault
            )))
            .type_,
            RichButtonStyleType::Default
        );
        assert_eq!(
            RichButtonStyle::from_td_api(Some(td_api::ButtonStyle::buttonStyleLink(
                td_api::buttonStyleLink
            )))
            .type_,
            RichButtonStyleType::Link
        );
        assert!(RichButtonStyle::default().is_default());
    }

    #[test]
    fn store_parse_round_trips() {
        for style in [
            RichButtonStyle::default(),
            RichButtonStyle::from_td_api(Some(td_api::ButtonStyle::buttonStyleDanger(
                td_api::buttonStyleDanger,
            ))),
            RichButtonStyle::from_td_api(Some(td_api::ButtonStyle::buttonStyleLink(
                td_api::buttonStyleLink,
            ))),
        ] {
            let mut out = Vec::new();
            style.store(&mut out);
            let mut parser = Parser::new(&out);
            assert_eq!(RichButtonStyle::parse(&mut parser), Ok(style));
        }
    }
}
