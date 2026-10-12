//! Port of `td/telegram/KeyboardButton.{h,cpp,hpp}` — the private-chat
//! keyboard button model: text, optional Web App URL, poll/phone/
//! location/dialog request types, and the shared
//! [`KeyboardButtonStyle`].
//!
//! The td_api intake's Web App URL validation dereferences `G()` in the
//! C++ (`!G()->is_test_dc()`, KeyboardButton.cpp:140); the port carries
//! it as the documented `is_test_dc` parameter, the established
//! is_bot/unix_time seam convention.

use tdutils::status::Status;

use crate::keyboard_button_style::KeyboardButtonStyle;
use crate::link_manager;
use crate::misc::clean_input_string;
use crate::requested_dialog_type::RequestedDialogType;
use crate::store_parse::{self, Parser};
use crate::td_api;
use tdmtproto::telegram_api;

/// `td::KeyboardButton::Type` (KeyboardButton.h:22) — append only; the
/// discriminants ride the store/parse format.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum KeyboardButtonType {
    #[default]
    Text,
    RequestPhoneNumber,
    RequestLocation,
    RequestPoll,
    RequestPollQuiz,
    RequestPollRegular,
    WebView,
    RequestDialog,
}

impl KeyboardButtonType {
    fn as_i32(self) -> i32 {
        match self {
            KeyboardButtonType::Text => 0,
            KeyboardButtonType::RequestPhoneNumber => 1,
            KeyboardButtonType::RequestLocation => 2,
            KeyboardButtonType::RequestPoll => 3,
            KeyboardButtonType::RequestPollQuiz => 4,
            KeyboardButtonType::RequestPollRegular => 5,
            KeyboardButtonType::WebView => 6,
            KeyboardButtonType::RequestDialog => 7,
        }
    }

    fn from_i32(value: i32) -> KeyboardButtonType {
        match value {
            1 => KeyboardButtonType::RequestPhoneNumber,
            2 => KeyboardButtonType::RequestLocation,
            3 => KeyboardButtonType::RequestPoll,
            4 => KeyboardButtonType::RequestPollQuiz,
            5 => KeyboardButtonType::RequestPollRegular,
            6 => KeyboardButtonType::WebView,
            7 => KeyboardButtonType::RequestDialog,
            _ => KeyboardButtonType::Text,
        }
    }
}

/// `td::KeyboardButton` (KeyboardButton.h:20). The equality is the C++
/// `operator==` (KeyboardButton.cpp:251), hand-written below.
#[derive(Clone, Debug, Default)]
pub struct KeyboardButton {
    pub(crate) type_: KeyboardButtonType,
    pub(crate) style: KeyboardButtonStyle,
    pub(crate) text: String,
    pub(crate) url: String, // WebView only
    pub(crate) requested_dialog_type: Option<Box<RequestedDialogType>>, // RequestDialog only
}

impl KeyboardButton {
    /// `explicit KeyboardButton(telegram_api::object_ptr<
    /// telegram_api::keyboardButton> &&)` (KeyboardButton.cpp:18-68).
    /// An unsupported or failed-URL button keeps its text but degrades to
    /// [`KeyboardButtonType::Text`], exactly like the C++ fall-through.
    pub fn from_telegram(keyboard_button: telegram_api::KeyboardButton) -> KeyboardButton {
        let mut result = KeyboardButton {
            style: KeyboardButtonStyle::from_telegram(keyboard_button.style),
            text: keyboard_button.text,
            ..KeyboardButton::default()
        };
        if let Some(type_) = keyboard_button.r#type {
            match type_ {
                telegram_api::ButtonType::buttonTypeDefault(_) => {
                    result.type_ = KeyboardButtonType::Text;
                }
                telegram_api::ButtonType::buttonTypeRequestPhone(_) => {
                    result.type_ = KeyboardButtonType::RequestPhoneNumber;
                }
                telegram_api::ButtonType::buttonTypeRequestGeoLocation(_) => {
                    result.type_ = KeyboardButtonType::RequestLocation;
                }
                telegram_api::ButtonType::buttonTypeRequestPoll(type_) => {
                    if type_.flags & (1 << 0) != 0 {
                        // QUIZ_MASK
                        if type_.quiz {
                            result.type_ = KeyboardButtonType::RequestPollQuiz;
                        } else {
                            result.type_ = KeyboardButtonType::RequestPollRegular;
                        }
                    } else {
                        result.type_ = KeyboardButtonType::RequestPoll;
                    }
                }
                telegram_api::ButtonType::buttonTypeSimpleWebView(type_) => {
                    match link_manager::check_link(&type_.url, false, false) {
                        Ok(url) => {
                            result.type_ = KeyboardButtonType::WebView;
                            result.url = url;
                        }
                        Err(error) => {
                            tdutils::log_error!("Keyboard Web App {}", error.message());
                        }
                    }
                }
                telegram_api::ButtonType::buttonTypeRequestPeer(type_) => {
                    if let Some(peer_type) = type_.peer_type {
                        result.type_ = KeyboardButtonType::RequestDialog;
                        result.requested_dialog_type =
                            Some(Box::new(RequestedDialogType::from_telegram(
                                peer_type,
                                type_.button_id,
                                type_.max_quantity,
                            )));
                    }
                }
                type_ => {
                    tdutils::log_error!("Unsupported keyboard button {:?}", type_);
                }
            }
        }
        result
    }

    /// `KeyboardButton::get_keyboard_button` (KeyboardButton.cpp:82-179)
    /// — the td_api intake with the private-chat 400 table.
    pub fn from_td_api(
        mut button: td_api::KeyboardButton,
        request_buttons_allowed: bool,
        is_test_dc: bool,
    ) -> Result<KeyboardButton, Status> {
        let Some(text) = clean_input_string(&button.text) else {
            return Err(Status::error(
                400,
                "Keyboard button text must be encoded in UTF-8",
            ));
        };
        if text.is_empty() {
            return Err(Status::error(400, "Keyboard button text must be non-empty"));
        }
        button.text = text;

        let mut current_button = KeyboardButton {
            text: button.text,
            style: KeyboardButtonStyle::from_td_api(
                button.style.take(),
                button.icon_custom_emoji_id,
            ),
            ..KeyboardButton::default()
        };

        // `button->type_ == nullptr` reads as keyboardButtonTypeText.
        let button_type = button.r#type.take();
        match button_type {
            None => {
                current_button.type_ = KeyboardButtonType::Text;
            }
            Some(td_api::KeyboardButtonType::keyboardButtonTypeText(_)) => {
                current_button.type_ = KeyboardButtonType::Text;
            }
            Some(td_api::KeyboardButtonType::keyboardButtonTypeRequestPhoneNumber(_)) => {
                if !request_buttons_allowed {
                    return Err(Status::error(
                        400,
                        "Phone number can be requested in private chats only",
                    ));
                }
                current_button.type_ = KeyboardButtonType::RequestPhoneNumber;
            }
            Some(td_api::KeyboardButtonType::keyboardButtonTypeRequestLocation(_)) => {
                if !request_buttons_allowed {
                    return Err(Status::error(
                        400,
                        "Location can be requested in private chats only",
                    ));
                }
                current_button.type_ = KeyboardButtonType::RequestLocation;
            }
            Some(td_api::KeyboardButtonType::keyboardButtonTypeRequestPoll(request_poll)) => {
                if !request_buttons_allowed {
                    return Err(Status::error(
                        400,
                        "Poll can be requested in private chats only",
                    ));
                }
                if request_poll.force_quiz && request_poll.force_regular {
                    return Err(Status::error(
                        400,
                        "Can't force quiz mode and regular poll simultaneously",
                    ));
                }
                if request_poll.force_quiz {
                    current_button.type_ = KeyboardButtonType::RequestPollQuiz;
                } else if request_poll.force_regular {
                    current_button.type_ = KeyboardButtonType::RequestPollRegular;
                } else {
                    current_button.type_ = KeyboardButtonType::RequestPoll;
                }
            }
            Some(td_api::KeyboardButtonType::keyboardButtonTypeWebApp(button_type)) => {
                if !request_buttons_allowed {
                    return Err(Status::error(
                        400,
                        "Web App buttons can be used in private chats only",
                    ));
                }
                if link_manager::get_link_user_id(&button_type.url).is_valid() {
                    return Err(Status::error(
                        400,
                        "Link to a user can't be used in Web App URL buttons",
                    ));
                }
                // The checked URL only guards the error table; the button
                // keeps the raw url_ (the C++ moves the input through).
                link_manager::check_link(&button_type.url, true, !is_test_dc).map_err(|error| {
                    Status::error(400, &format!("Keyboard button Web App {}", error.message()))
                })?;
                current_button.type_ = KeyboardButtonType::WebView;
                current_button.url = button_type.url;
            }
            Some(td_api::KeyboardButtonType::keyboardButtonTypeRequestUsers(button_type)) => {
                if !request_buttons_allowed {
                    return Err(Status::error(
                        400,
                        "Users can be requested in private chats only",
                    ));
                }
                current_button.type_ = KeyboardButtonType::RequestDialog;
                current_button.requested_dialog_type = Some(Box::new(
                    RequestedDialogType::from_td_api_request_users(button_type),
                ));
            }
            Some(td_api::KeyboardButtonType::keyboardButtonTypeRequestChat(button_type)) => {
                if !request_buttons_allowed {
                    return Err(Status::error(
                        400,
                        "Chats can be requested in private chats only",
                    ));
                }
                current_button.type_ = KeyboardButtonType::RequestDialog;
                current_button.requested_dialog_type = Some(Box::new(
                    RequestedDialogType::from_td_api_request_chat(button_type),
                ));
            }
            Some(td_api::KeyboardButtonType::keyboardButtonTypeRequestManagedBot(button_type)) => {
                if !request_buttons_allowed {
                    return Err(Status::error(
                        400,
                        "Managed bots can be requested in private chats only",
                    ));
                }
                current_button.type_ = KeyboardButtonType::RequestDialog;
                current_button.requested_dialog_type = Some(Box::new(
                    RequestedDialogType::from_td_api_request_managed_bot(button_type),
                ));
            }
        }
        Ok(current_button)
    }

    /// `is_empty` (KeyboardButton.h:50-52).
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
            && (self.type_ == KeyboardButtonType::Text
                || !self.style.get_icon_custom_emoji_id().is_valid())
    }

    /// `KeyboardButton::clone` (KeyboardButton.cpp:70-80).
    pub fn clone_button(&self) -> KeyboardButton {
        self.clone()
    }

    /// `get_input_keyboard_button` (KeyboardButton.cpp:181-214). Both the
    /// raw flags word and the derived style flag bit are set.
    pub fn get_input_keyboard_button(&self) -> telegram_api::KeyboardButton {
        let type_ = match self.type_ {
            KeyboardButtonType::Text => {
                telegram_api::ButtonType::buttonTypeDefault(telegram_api::buttonTypeDefault {})
            }
            KeyboardButtonType::RequestPhoneNumber => {
                telegram_api::ButtonType::buttonTypeRequestPhone(
                    telegram_api::buttonTypeRequestPhone {},
                )
            }
            KeyboardButtonType::RequestLocation => {
                telegram_api::ButtonType::buttonTypeRequestGeoLocation(
                    telegram_api::buttonTypeRequestGeoLocation {},
                )
            }
            KeyboardButtonType::RequestPoll => telegram_api::ButtonType::buttonTypeRequestPoll(
                telegram_api::buttonTypeRequestPoll {
                    flags: 0,
                    quiz: false,
                },
            ),
            KeyboardButtonType::RequestPollQuiz => telegram_api::ButtonType::buttonTypeRequestPoll(
                telegram_api::buttonTypeRequestPoll {
                    flags: 1 << 0, // QUIZ_MASK
                    quiz: true,
                },
            ),
            KeyboardButtonType::RequestPollRegular => {
                telegram_api::ButtonType::buttonTypeRequestPoll(
                    telegram_api::buttonTypeRequestPoll {
                        flags: 1 << 0, // QUIZ_MASK
                        quiz: false,
                    },
                )
            }
            KeyboardButtonType::WebView => telegram_api::ButtonType::buttonTypeSimpleWebView(
                telegram_api::buttonTypeSimpleWebView {
                    url: self.url.clone(),
                },
            ),
            KeyboardButtonType::RequestDialog => {
                let requested_dialog_type = self
                    .requested_dialog_type
                    .as_ref()
                    .expect("the C++ CHECK(requested_dialog_type_ != nullptr) fired");
                // The C++ builds the inputButtonTypeRequestPeer ctor —
                // it is a ButtonType constructor in the scheme.
                telegram_api::ButtonType::inputButtonTypeRequestPeer(
                    requested_dialog_type.get_input_button_type_request_peer(),
                )
            }
        };
        let mut flags = 0;
        let style = self.style.get_input_keyboard_button_style();
        if style.is_some() {
            flags |= 1 << 10;
        }
        telegram_api::KeyboardButton {
            flags,
            style,
            text: self.text.clone(),
            r#type: Some(type_),
        }
    }

    /// `get_keyboard_button_object` (KeyboardButton.cpp:216-249).
    pub fn get_keyboard_button_object(&self) -> td_api::KeyboardButton {
        let type_ = match self.type_ {
            KeyboardButtonType::Text => {
                td_api::KeyboardButtonType::keyboardButtonTypeText(td_api::keyboardButtonTypeText)
            }
            KeyboardButtonType::RequestPhoneNumber => {
                td_api::KeyboardButtonType::keyboardButtonTypeRequestPhoneNumber(
                    td_api::keyboardButtonTypeRequestPhoneNumber,
                )
            }
            KeyboardButtonType::RequestLocation => {
                td_api::KeyboardButtonType::keyboardButtonTypeRequestLocation(
                    td_api::keyboardButtonTypeRequestLocation,
                )
            }
            KeyboardButtonType::RequestPoll => {
                td_api::KeyboardButtonType::keyboardButtonTypeRequestPoll(
                    td_api::keyboardButtonTypeRequestPoll {
                        force_regular: false,
                        force_quiz: false,
                    },
                )
            }
            KeyboardButtonType::RequestPollQuiz => {
                td_api::KeyboardButtonType::keyboardButtonTypeRequestPoll(
                    td_api::keyboardButtonTypeRequestPoll {
                        force_regular: false,
                        force_quiz: true,
                    },
                )
            }
            KeyboardButtonType::RequestPollRegular => {
                td_api::KeyboardButtonType::keyboardButtonTypeRequestPoll(
                    td_api::keyboardButtonTypeRequestPoll {
                        force_regular: true,
                        force_quiz: false,
                    },
                )
            }
            KeyboardButtonType::WebView => td_api::KeyboardButtonType::keyboardButtonTypeWebApp(
                td_api::keyboardButtonTypeWebApp {
                    url: format!("{}#kb", self.url),
                },
            ),
            KeyboardButtonType::RequestDialog => self
                .requested_dialog_type
                .as_ref()
                .expect("RequestDialog without a RequestedDialogType")
                .get_keyboard_button_type_object(),
        };
        td_api::KeyboardButton {
            text: self.text.clone(),
            icon_custom_emoji_id: self.style.get_icon_custom_emoji_id().get(),
            style: if self.style.is_default() {
                None
            } else {
                Some(self.style.get_button_style_object())
            },
            r#type: Some(type_),
        }
    }

    /// `get_requested_dialog_type` (KeyboardButton.h:60-62).
    pub fn get_requested_dialog_type(&self) -> Option<&RequestedDialogType> {
        self.requested_dialog_type.as_deref()
    }

    /// `KeyboardButton::store` (KeyboardButton.hpp:18-40).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_url = !self.url.is_empty();
        let has_requested_dialog_type = self.requested_dialog_type.is_some();
        let has_style = !self.style.is_default();
        store_parse::store_flags(out, [has_url, has_requested_dialog_type, has_style]);
        store_parse::store_i32(out, self.type_.as_i32());
        store_parse::store_string(out, &self.text);
        if has_url {
            store_parse::store_string(out, &self.url);
        }
        if has_requested_dialog_type {
            self.requested_dialog_type
                .as_ref()
                .expect("has_requested_dialog_type checked")
                .store(out);
        }
        if has_style {
            self.style.store(out);
        }
    }

    /// `KeyboardButton::parse` (KeyboardButton.hpp:42-70) — the pre-35
    /// legacy ladder reads no flags word and no optional fields.
    pub fn parse(parser: &mut Parser<'_>) -> Result<KeyboardButton, &'static str> {
        let (has_url, has_requested_dialog_type, has_style) =
            if parser.version() >= store_parse::version::ADD_KEYBOARD_BUTTON_FLAGS {
                let [has_url, has_requested_dialog_type, has_style] = parser.parse_flags::<3>()?;
                (has_url, has_requested_dialog_type, has_style)
            } else {
                (false, false, false)
            };
        let type_ = KeyboardButtonType::from_i32(parser.i32_()?);
        let text = parser.string()?;
        let url = if has_url {
            parser.string()?
        } else {
            String::new()
        };
        let requested_dialog_type = if has_requested_dialog_type {
            Some(Box::new(RequestedDialogType::parse(parser)?))
        } else {
            None
        };
        let style = if has_style {
            KeyboardButtonStyle::parse(parser)?
        } else {
            KeyboardButtonStyle::default()
        };
        Ok(KeyboardButton {
            type_,
            style,
            text,
            url,
            requested_dialog_type,
        })
    }

    /// `operator<<` (KeyboardButton.cpp:256-287).
    pub fn print(&self, out: &mut String) {
        out.push_str("Button[");
        out.push_str(match self.type_ {
            KeyboardButtonType::Text => "Text",
            KeyboardButtonType::RequestPhoneNumber => "RequestPhoneNumber",
            KeyboardButtonType::RequestLocation => "RequestLocation",
            KeyboardButtonType::RequestPoll => "RequestPoll",
            KeyboardButtonType::RequestPollQuiz => "RequestPollQuiz",
            KeyboardButtonType::RequestPollRegular => "RequestPollRegular",
            KeyboardButtonType::WebView => "WebApp",
            KeyboardButtonType::RequestDialog => "RequestChat",
        });
        out.push_str(", ");
        out.push_str(&self.text);
        self.style.print(out);
        out.push(']');
    }
}

impl std::fmt::Display for KeyboardButton {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut out = String::new();
        self.print(&mut out);
        f.write_str(&out)
    }
}

/// `operator==` (KeyboardButton.cpp:251-254).
impl PartialEq for KeyboardButton {
    fn eq(&self, other: &Self) -> bool {
        self.type_ == other.type_
            && self.style == other.style
            && self.text == other.text
            && self.url == other.url
            && self.requested_dialog_type == other.requested_dialog_type
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn td_text_button(text: &str) -> td_api::KeyboardButton {
        td_api::KeyboardButton {
            text: text.to_owned(),
            icon_custom_emoji_id: 0,
            style: None,
            r#type: Some(td_api::KeyboardButtonType::keyboardButtonTypeText(
                td_api::keyboardButtonTypeText,
            )),
        }
    }

    #[test]
    fn td_api_text_intake_table() {
        let button =
            KeyboardButton::from_td_api(td_text_button("Hi"), false, false).expect("valid");
        assert_eq!(button.type_, KeyboardButtonType::Text);
        assert_eq!(button.text, "Hi");
        assert!(!button.is_empty());
        // The td_api object round trip (a null style reads back as None).
        match button.get_keyboard_button_object().r#type {
            Some(td_api::KeyboardButtonType::keyboardButtonTypeText(_)) => {}
            other => panic!("wrong object: {other:?}"),
        }
        assert_eq!(button.get_keyboard_button_object().style, None);

        // The 400 table for the text field.
        let error =
            KeyboardButton::from_td_api(td_text_button(""), true, false).expect_err("empty text");
        assert_eq!(error.code(), 400);
        assert_eq!(error.message(), "Keyboard button text must be non-empty");
    }

    #[test]
    fn td_api_request_type_400_table() {
        let request_phone = td_api::KeyboardButton {
            r#type: Some(
                td_api::KeyboardButtonType::keyboardButtonTypeRequestPhoneNumber(
                    td_api::keyboardButtonTypeRequestPhoneNumber,
                ),
            ),
            ..td_text_button("?")
        };
        let error = KeyboardButton::from_td_api(request_phone.clone(), false, false)
            .expect_err("not a private chat");
        assert_eq!(
            error.message(),
            "Phone number can be requested in private chats only"
        );
        let button = KeyboardButton::from_td_api(request_phone, true, false).expect("allowed");
        assert_eq!(button.type_, KeyboardButtonType::RequestPhoneNumber);

        // A request poll with both force flags is contradictory.
        let bad_poll = td_api::KeyboardButton {
            r#type: Some(td_api::KeyboardButtonType::keyboardButtonTypeRequestPoll(
                td_api::keyboardButtonTypeRequestPoll {
                    force_regular: true,
                    force_quiz: true,
                },
            )),
            ..td_text_button("?")
        };
        let error = KeyboardButton::from_td_api(bad_poll, true, false).expect_err("both forced");
        assert_eq!(
            error.message(),
            "Can't force quiz mode and regular poll simultaneously"
        );
    }

    #[test]
    fn web_app_url_validation() {
        let web_app = td_api::KeyboardButton {
            r#type: Some(td_api::KeyboardButtonType::keyboardButtonTypeWebApp(
                td_api::keyboardButtonTypeWebApp {
                    url: "https://example.com/app".to_owned(),
                },
            )),
            ..td_text_button("App")
        };
        // Production DCs require a public HTTPS suffix; the test-DC flag
        // loosens it.
        let button = KeyboardButton::from_td_api(web_app.clone(), true, true).expect("test dc");
        assert_eq!(button.type_, KeyboardButtonType::WebView);
        assert_eq!(button.url, "https://example.com/app");

        // The td_api object carries the "#kb" suffix.
        match button.get_keyboard_button_object().r#type {
            Some(td_api::KeyboardButtonType::keyboardButtonTypeWebApp(web_app)) => {
                assert_eq!(web_app.url, "https://example.com/app#kb");
            }
            other => panic!("wrong object: {other:?}"),
        }

        // A `tg://user?id=` link is rejected outright
        // (KeyboardButton.cpp:136-139); https t.me links are not user
        // links for `get_link_user_id` (LinkManager.cpp:4385-4392) and
        // stay valid.
        let user_link = td_api::KeyboardButton {
            r#type: Some(td_api::KeyboardButtonType::keyboardButtonTypeWebApp(
                td_api::keyboardButtonTypeWebApp {
                    url: "tg://user?id=9".to_owned(),
                },
            )),
            ..td_text_button("App")
        };
        let error = KeyboardButton::from_td_api(user_link, true, true).expect_err("user link");
        assert_eq!(
            error.message(),
            "Link to a user can't be used in Web App URL buttons"
        );
        let t_me_link = td_api::KeyboardButton {
            r#type: Some(td_api::KeyboardButtonType::keyboardButtonTypeWebApp(
                td_api::keyboardButtonTypeWebApp {
                    url: "https://t.me/username?start=bot".to_owned(),
                },
            )),
            ..td_text_button("App")
        };
        let t_me = KeyboardButton::from_td_api(t_me_link, true, true).expect("t.me link");
        assert_eq!(t_me.url, "https://t.me/username?start=bot");
    }

    #[test]
    fn wire_intake_and_output_round_trip() {
        let wire = telegram_api::KeyboardButton {
            flags: 1 << 10,
            style: Some(telegram_api::KeyboardButtonStyle {
                flags: 1 << 1,
                bg_primary: false,
                bg_danger: true,
                bg_success: false,
                icon: 0,
            }),
            text: "Danger".to_owned(),
            r#type: Some(telegram_api::ButtonType::buttonTypeRequestPhone(
                telegram_api::buttonTypeRequestPhone {},
            )),
        };
        let button = KeyboardButton::from_telegram(wire);
        assert_eq!(button.type_, KeyboardButtonType::RequestPhoneNumber);
        assert!(!button.style.is_default());

        // The wire output re-sets both the flags word (the style flag bit
        // 10) and carries the style object.
        let out = button.get_input_keyboard_button();
        assert_eq!(out.flags, 1 << 10);
        assert!(out.style.is_some());
        assert_eq!(out.text, "Danger");

        // Poll flags round trip: the quiz/regular distinction rides both
        // the QUIZ_MASK flag and the derived quiz bool.
        let quiz = KeyboardButton {
            type_: KeyboardButtonType::RequestPollQuiz,
            ..KeyboardButton::default()
        };
        let quiz_out = quiz.get_input_keyboard_button();
        match quiz_out.r#type {
            Some(telegram_api::ButtonType::buttonTypeRequestPoll(poll)) => {
                assert_eq!(poll.flags, 1 << 0);
                assert!(poll.quiz);
            }
            other => panic!("wrong poll type: {other:?}"),
        }
        let regular = KeyboardButton {
            type_: KeyboardButtonType::RequestPollRegular,
            ..KeyboardButton::default()
        };
        match regular.get_input_keyboard_button().r#type {
            Some(telegram_api::ButtonType::buttonTypeRequestPoll(poll)) => {
                assert_eq!(poll.flags, 1 << 0);
                assert!(!poll.quiz);
            }
            other => panic!("wrong poll type: {other:?}"),
        }
    }

    #[test]
    fn request_dialog_button_round_trip() {
        let button = KeyboardButton::from_td_api(
            td_api::KeyboardButton {
                r#type: Some(td_api::KeyboardButtonType::keyboardButtonTypeRequestUsers(
                    td_api::keyboardButtonTypeRequestUsers {
                        id: 3,
                        restrict_user_is_bot: false,
                        user_is_bot: false,
                        restrict_user_is_premium: false,
                        user_is_premium: false,
                        max_quantity: 5,
                        request_name: true,
                        request_username: false,
                        request_photo: true,
                    },
                )),
                ..td_text_button("Pick")
            },
            true,
            false,
        )
        .expect("valid");
        assert_eq!(button.type_, KeyboardButtonType::RequestDialog);
        let requested = button.get_requested_dialog_type().expect("dialog type");
        assert_eq!(requested.get_button_id(), 3);

        // The wire output wraps the request-peer button type.
        match button.get_input_keyboard_button().r#type {
            Some(telegram_api::ButtonType::inputButtonTypeRequestPeer(peer)) => {
                assert_eq!(peer.button_id, 3);
                assert_eq!(peer.max_quantity, 5);
            }
            other => panic!("wrong wire type: {other:?}"),
        }
        // The td_api object rebuilds the request-users payload.
        match button.get_keyboard_button_object().r#type {
            Some(td_api::KeyboardButtonType::keyboardButtonTypeRequestUsers(users)) => {
                assert_eq!(users.id, 3);
                assert_eq!(users.max_quantity, 5);
            }
            other => panic!("wrong object: {other:?}"),
        }
    }

    #[test]
    fn store_parse_round_trip_and_legacy_ladder() {
        let button = KeyboardButton {
            type_: KeyboardButtonType::WebView,
            style: KeyboardButtonStyle::from_td_api(
                Some(td_api::ButtonStyle::buttonStylePrimary(
                    td_api::buttonStylePrimary {},
                )),
                55,
            ),
            text: "Open".to_owned(),
            url: "https://example.com".to_owned(),
            requested_dialog_type: None,
        };
        let mut out = Vec::new();
        button.store(&mut out);
        let parser = &mut Parser::new(&out);
        parser.set_version(store_parse::version::LOG_EVENT_STORE_VERSION);
        let parsed = KeyboardButton::parse(parser).expect("parse");
        assert_eq!(parsed, button);
        assert!(parser.fetch_end().is_ok());

        // A pre-AddKeyboardButtonFlags row: type + text only, no flags
        // word, no optional fields.
        let mut legacy = Vec::new();
        store_parse::store_i32(&mut legacy, KeyboardButtonType::Text.as_i32());
        store_parse::store_string(&mut legacy, "Old");
        let parser = &mut Parser::new(&legacy);
        parser.set_version(store_parse::version::ADD_KEYBOARD_BUTTON_FLAGS - 1);
        let parsed = KeyboardButton::parse(parser).expect("legacy parse");
        assert_eq!(
            parsed,
            KeyboardButton {
                type_: KeyboardButtonType::Text,
                text: "Old".to_owned(),
                ..KeyboardButton::default()
            }
        );
        assert!(parser.fetch_end().is_ok());
    }

    #[test]
    fn display_and_equality() {
        let button = KeyboardButton::from_td_api(td_text_button("Go"), true, false).expect("valid");
        assert_eq!(button.to_string(), "Button[Text, Go]");
        let styled = KeyboardButton {
            style: KeyboardButtonStyle::from_td_api(
                Some(td_api::ButtonStyle::buttonStyleSuccess(
                    td_api::buttonStyleSuccess {},
                )),
                0,
            ),
            ..button.clone_button()
        };
        assert_eq!(styled.to_string(), "Button[Text, Go, Success]");
        assert_ne!(styled, button);
        // An empty text with the default style is the empty button.
        let empty = KeyboardButton::default();
        assert!(empty.is_empty());
    }
}
