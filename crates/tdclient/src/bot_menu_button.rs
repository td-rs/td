//! Port of `td/telegram/BotMenuButton.{h,cpp}` (Wave 3, M.45 — Bots I):
//! the bot menu button value plane (with the byte-exact store/parse) and
//! the `setMenuButton`/`getMenuButton` request flows.
//!
//! Seams: the flows end in the `Td::ResultHandler` sends
//! ([`BotInfoManagerEnv`] query seams, [blocked: net]) — the promises park
//! in [`crate::bot_info_manager::BotInfoManager`], whose
//! `get_menu_button` answer half (`GetBotMenuButtonQuery::on_result`,
//! BotMenuButton.cpp:69-80) reduces to [`get_bot_menu_button`] +
//! [`get_bot_menu_button_object`] once the net step lands.
//!
//! Divergence: the C++ `LinkManager::check_link` failure is reported as
//! `400 "Menu button Web App " + message` (BotMenuButton.cpp:148-150).

use crate::bot_info_manager::BotInfoManagerEnv;
use crate::store_parse::{self, Parser};
use crate::td_api;
use crate::user_manager::UserId;
use tdmtproto::telegram_api;
use tdutils::promise::Promise;
use tdutils::status::Status;

/// `td::BotMenuButton` (BotMenuButton.h:21-72).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BotMenuButton {
    text: String,
    url: String,
}

impl BotMenuButton {
    /// `BotMenuButton::BotMenuButton(string &&, string &&)`
    /// (BotMenuButton.h:30-31).
    pub fn new(text: String, url: String) -> BotMenuButton {
        BotMenuButton { text, url }
    }

    /// `BotMenuButton::get_bot_menu_button_object`
    /// (BotMenuButton.cpp:110-113): the non-bot surface prefixes
    /// `menu://`.
    pub fn get_bot_menu_button_object(
        &self,
        env: &impl BotInfoManagerEnv,
    ) -> td_api::BotMenuButton {
        let is_bot = env.is_bot();
        td_api::BotMenuButton {
            text: self.text.clone(),
            url: if is_bot {
                self.url.clone()
            } else {
                format!("menu://{}", self.url)
            },
        }
    }

    /// `BotMenuButton::store` (BotMenuButton.h:35-49): the presence flags
    /// gate the optional strings.
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_text = !self.text.is_empty();
        let has_url = !self.url.is_empty();
        store_parse::store_flags(out, [has_text, has_url]);
        if has_text {
            store_parse::store_string(out, &self.text);
        }
        if has_url {
            store_parse::store_string(out, &self.url);
        }
    }

    /// `BotMenuButton::parse` (BotMenuButton.h:51-65).
    pub fn parse(parser: &mut Parser) -> Result<BotMenuButton, &'static str> {
        let mut result = BotMenuButton::default();
        let [has_text, has_url] = parser.parse_flags()?;
        if has_text {
            result.text = parser.string()?;
        }
        if has_url {
            result.url = parser.string()?;
        }
        Result::Ok(result)
    }
}

/// `get_bot_menu_button` (BotMenuButton.cpp:87-108): `None` is the C++
/// `nullptr` (no custom button; the Commands and invalid variants).
pub fn get_bot_menu_button(
    bot_menu_button: Option<telegram_api::BotMenuButton>,
) -> Option<BotMenuButton> {
    let bot_menu_button = bot_menu_button?;
    match bot_menu_button {
        // the commands list is the "no button" answer
        telegram_api::BotMenuButton::botMenuButtonCommands(_) => None,
        telegram_api::BotMenuButton::botMenuButtonDefault(_) => {
            Some(BotMenuButton::new(String::new(), "default".to_owned()))
        }
        telegram_api::BotMenuButton::botMenuButton(button) => {
            if button.text.is_empty() {
                tdutils::log_error!("Receive bot menu button with empty text: {:?}", button);
                return None;
            }
            Some(BotMenuButton::new(button.text, button.url))
        }
    }
}

/// `get_bot_menu_button_object(Td *, const BotMenuButton *)`
/// (BotMenuButton.cpp:119-124): `None` is the C++ `nullptr`.
pub fn get_bot_menu_button_object(
    env: &impl BotInfoManagerEnv,
    bot_menu_button: Option<&BotMenuButton>,
) -> Option<td_api::BotMenuButton> {
    bot_menu_button.map(|button| button.get_bot_menu_button_object(env))
}

/// `set_menu_button` (BotMenuButton.cpp:126-155): the input button
/// conversion, then the `bots.setBotMenuButton` send seam with the promise
/// parked in the manager.
pub fn set_menu_button(
    manager: &mut crate::bot_info_manager::BotInfoManager,
    env: &mut impl BotInfoManagerEnv,
    user_id: UserId,
    menu_button: Option<td_api::BotMenuButton>,
    mut promise: Promise<()>,
) {
    if !user_id.is_valid() && user_id != UserId::default() {
        promise.set_error(Status::error(400, "User not found"));
        return;
    }

    let input_bot_menu_button = if let Some(menu_button) = menu_button {
        if menu_button.text.is_empty() {
            if menu_button.url != "default" {
                promise.set_error(Status::error(400, "Menu button text must be non-empty"));
                return;
            }
            telegram_api::BotMenuButton::botMenuButtonDefault(telegram_api::botMenuButtonDefault {})
        } else {
            if crate::misc::clean_input_string(&menu_button.text).is_none() {
                promise.set_error(Status::error(
                    400,
                    "Menu button text must be encoded in UTF-8",
                ));
                return;
            }
            if crate::misc::clean_input_string(&menu_button.url).is_none() {
                promise.set_error(Status::error(
                    400,
                    "Menu button URL must be encoded in UTF-8",
                ));
                return;
            }
            let r_url = crate::link_manager::check_link(&menu_button.url, true, !env.is_test_dc());
            if let Err(error) = r_url {
                promise.set_error(Status::error(
                    400,
                    &format!("Menu button Web App {}", error.message()),
                ));
                return;
            }
            telegram_api::BotMenuButton::botMenuButton(telegram_api::botMenuButton {
                text: menu_button.text,
                url: r_url.expect("checked above"),
            })
        }
    } else {
        telegram_api::BotMenuButton::botMenuButtonCommands(telegram_api::botMenuButtonCommands {})
    };

    // SetBotMenuButtonQuery::send (BotMenuButton.cpp:31-36): the invalid
    // user id sends `inputUserEmpty`
    let input_user = if user_id.is_valid() {
        Some(env.get_input_user(user_id).expect("CHECK failed"))
    } else {
        None
    };

    manager.park_set_menu_button_query(promise);
    env.send_set_bot_menu_button_query(input_user, input_bot_menu_button);
}

/// `get_menu_button` (BotMenuButton.cpp:157-163).
pub fn get_menu_button(
    manager: &mut crate::bot_info_manager::BotInfoManager,
    env: &mut impl BotInfoManagerEnv,
    user_id: UserId,
    mut promise: Promise<td_api::BotMenuButton>,
) {
    if !user_id.is_valid() && user_id != UserId::default() {
        promise.set_error(Status::error(400, "User not found"));
        return;
    }

    let input_user = if user_id.is_valid() {
        Some(env.get_input_user(user_id).expect("CHECK failed"))
    } else {
        None
    };

    manager.park_get_menu_button_query(promise);
    env.send_get_bot_menu_button_query(input_user);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot_info_manager::BotInfoManager;
    use crate::bot_info_manager::test_env;
    use crate::global::Global;
    use crate::option_manager::OptionManager;
    use crate::user_manager::UserManager;
    use tdutils::promise::PromiseInterface;

    struct Recorded<T: Send + 'static>(
        std::sync::Arc<std::sync::Mutex<Option<std::result::Result<T, Status>>>>,
    );

    struct RecordingPromise<T: Send + 'static> {
        recorded: Recorded<T>,
    }

    impl<T: Send + 'static> PromiseInterface<T> for RecordingPromise<T> {
        fn set_value(&mut self, value: T) {
            *self.recorded.0.lock().unwrap() = Some(Result::Ok(value));
        }

        fn set_error(&mut self, error: Status) {
            *self.recorded.0.lock().unwrap() = Some(Result::Err(error));
        }
    }

    fn recording_promise<T: Send + 'static>() -> (Promise<T>, Recorded<T>) {
        let recorded = Recorded(std::sync::Arc::new(std::sync::Mutex::new(None)));
        (
            Promise::from_interface(RecordingPromise {
                recorded: Recorded(recorded.0.clone()),
            }),
            recorded,
        )
    }

    fn env_pair() -> (Global, OptionManager, UserManager) {
        (Global::new(), OptionManager::new(false), UserManager::new())
    }

    #[test]
    fn bot_menu_button_store_parse_round_trip() {
        // all four presence-flag combinations (BotMenuButton.h:35-65)
        for (text, url) in [
            (String::new(), String::new()),
            ("text".to_owned(), String::new()),
            (String::new(), "url".to_owned()),
            ("text".to_owned(), "url".to_owned()),
        ] {
            let button = BotMenuButton::new(text, url);
            let mut out = Vec::new();
            store_parse::store_log_event_version(&mut out);
            button.store(&mut out);
            let mut parser = Parser::new_log_event(&out).expect("framed");
            assert_eq!(BotMenuButton::parse(&mut parser).expect("parse"), button);
        }
    }

    #[test]
    fn get_bot_menu_button_variants() {
        // the commands list is no button
        assert!(
            get_bot_menu_button(Some(telegram_api::BotMenuButton::botMenuButtonCommands(
                telegram_api::botMenuButtonCommands {},
            )))
            .is_none()
        );
        // the default button is the empty text + "default" url
        assert_eq!(
            get_bot_menu_button(Some(telegram_api::BotMenuButton::botMenuButtonDefault(
                telegram_api::botMenuButtonDefault {},
            ))),
            Some(BotMenuButton::new(String::new(), "default".to_owned()))
        );
        // the empty text is the logged error
        assert!(
            get_bot_menu_button(Some(telegram_api::BotMenuButton::botMenuButton(
                telegram_api::botMenuButton {
                    text: String::new(),
                    url: "https://t.me".to_owned(),
                },
            )))
            .is_none()
        );
        // the null is the null
        assert!(get_bot_menu_button(None).is_none());
    }

    #[test]
    fn menu_button_object_prefixes_menu_scheme_for_users() {
        let (mut global, mut options, mut user_manager) = env_pair();
        let user_env =
            test_env::fixture(&mut global, Some(&mut options), &mut user_manager, false, 1);
        let button = BotMenuButton::new("Open".to_owned(), "t.me/bot".to_owned());
        assert_eq!(
            button.get_bot_menu_button_object(&user_env),
            td_api::BotMenuButton {
                text: "Open".to_owned(),
                url: "menu://t.me/bot".to_owned(),
            }
        );

        let bot_env =
            test_env::fixture(&mut global, Some(&mut options), &mut user_manager, true, 1);
        assert_eq!(
            button.get_bot_menu_button_object(&bot_env),
            td_api::BotMenuButton {
                text: "Open".to_owned(),
                url: "t.me/bot".to_owned(),
            }
        );
    }

    #[test]
    fn set_menu_button_validates_and_parks() {
        let (mut global, mut options, mut user_manager) = env_pair();
        let mut env =
            test_env::fixture(&mut global, Some(&mut options), &mut user_manager, true, 1);
        let mut manager = BotInfoManager::new();

        // the invalid user id (cpp:128-130)
        let (promise, recorded) = recording_promise();
        set_menu_button(&mut manager, &mut env, UserId::new(-1), None, promise);
        let recorded = recorded.0.lock().unwrap().clone();
        assert_eq!(recorded.unwrap().unwrap_err().message(), "User not found");

        // the empty text with a non-default url (cpp:135-138)
        let (promise, recorded) = recording_promise();
        set_menu_button(
            &mut manager,
            &mut env,
            UserId::default(),
            Some(td_api::BotMenuButton {
                text: String::new(),
                url: "other".to_owned(),
            }),
            promise,
        );
        let recorded = recorded.0.lock().unwrap().clone();
        assert_eq!(
            recorded.unwrap().unwrap_err().message(),
            "Menu button text must be non-empty"
        );

        // the invalid Web App url (cpp:147-150)
        let (promise, recorded) = recording_promise();
        set_menu_button(
            &mut manager,
            &mut env,
            UserId::default(),
            Some(td_api::BotMenuButton {
                text: "Open".to_owned(),
                url: "not a url".to_owned(),
            }),
            promise,
        );
        let recorded = recorded.0.lock().unwrap().clone();
        assert!(
            recorded
                .unwrap()
                .unwrap_err()
                .message()
                .starts_with("Menu button Web App ")
        );

        // the commands variant (null button) parks the promise
        let (promise, recorded) = recording_promise();
        set_menu_button(&mut manager, &mut env, UserId::default(), None, promise);
        assert!(recorded.0.lock().unwrap().is_none());
        assert_eq!(
            env.queries,
            vec!["Send bots.setBotMenuButton — [blocked: net]"]
        );
    }

    #[test]
    fn get_menu_button_validates_and_parks() {
        let (mut global, mut options, mut user_manager) = env_pair();
        let mut env =
            test_env::fixture(&mut global, Some(&mut options), &mut user_manager, true, 1);
        let mut manager = BotInfoManager::new();

        let (promise, recorded) = recording_promise();
        get_menu_button(&mut manager, &mut env, UserId::new(-1), promise);
        let recorded = recorded.0.lock().unwrap().clone();
        assert_eq!(recorded.unwrap().unwrap_err().message(), "User not found");

        let (promise, recorded) = recording_promise();
        get_menu_button(&mut manager, &mut env, UserId::default(), promise);
        assert!(recorded.0.lock().unwrap().is_none());
        assert_eq!(
            env.queries,
            vec!["Send bots.getBotMenuButton — [blocked: net]"]
        );
    }
}
