//! The port of `td/telegram/Support.{h,cpp}` — the glue slice behind
//! `getSupportName`/`getUserSupportInfo`/`setUserSupportInfo`.
//!
//! The local halves are real: `get_user_info`/`set_user_info` run the
//! `GetUserInfoQuery`/`EditUserInfoQuery` send halves (the
//! `UserManager::get_input_user` validation, Support.cpp:45-52/:76-85)
//! and the `get_formatted_text` intake over the my-dialog context
//! (Support.cpp:132-134); `get_user_support_info_object` is the shared
//! result rewrap (Support.cpp:21-35) — the answer half of the net
//! routing. The `help.getUserInfo`/`help.editUserInfo`/
//! `help.getSupportName` queries ride the client-side dispatcher under
//! their `SupportNetKind` (the answers drain through
//! `Td::on_support_net_answer`; the promises park at the Td-side lots
//! until then).

use tdutils::promise::Promise;
use tdutils::status::Status;

use crate::message_entity::{self, FormattedText, UserManagerLike};
use crate::td_api;
use crate::user_manager::UserId;

/// `get_user_support_info_object` (Support.cpp:21-35) — the shared
/// `help.getUserInfo`/`help.editUserInfo` result rewrap: the userInfo
/// variant parses the message (the date-keyed server-entity fixups) and
/// carries the author/date, the Empty variant keeps the defaults.
pub(crate) fn get_user_support_info_object<U: UserManagerLike + ?Sized>(
    mut user_manager: Option<&mut U>,
    user_info: tdmtproto::telegram_api::help_UserInfo,
) -> td_api::UserSupportInfo {
    let mut message = FormattedText::default();
    let mut author = String::new();
    let mut date = 0;
    if let tdmtproto::telegram_api::help_UserInfo::help_userInfo(info) = user_info {
        let tdmtproto::telegram_api::help_userInfo {
            message: message_text,
            entities,
            author: info_author,
            date: info_date,
        } = info;
        message = message_entity::get_message_text(
            user_manager.as_deref_mut(),
            message_text,
            entities,
            true,
            true,
            info_date,
            false,
            "get_user_support_info_object",
        );
        author = info_author;
        date = info_date;
    }
    td_api::UserSupportInfo {
        message: Some(message_entity::get_formatted_text_object(
            user_manager,
            &message,
            true,
            -1,
        )),
        author,
        date,
    }
}

/// `get_user_info` (Support.cpp:126-128) — the `GetUserInfoQuery::send`
/// validation runs here; the `help.getUserInfo` query rides the seam.
pub(crate) fn get_user_info(
    env: &mut impl SupportEnv,
    user_id: UserId,
    mut promise: Promise<td_api::UserSupportInfo>,
) {
    let input_user = match env.get_input_user(user_id) {
        Ok(input_user) => input_user,
        Err(status) => return promise.set_error(status),
    };
    env.get_user_info_via_net(input_user, promise);
}

/// `set_user_info` (Support.cpp:130-136) — the `get_formatted_text`
/// intake first (Support.cpp:132-134; `is_bot = false`, `allow_empty =
/// true`, `skip_media_timestamps = true`, `skip_trim = false`, the
/// my-dialog context; the user-manager capability reads are the ()
/// stand-in, the M.4.12 documented shape), then the
/// `EditUserInfoQuery::send` validation and wire build
/// (Support.cpp:76-85); the `help.editUserInfo` query rides the seam.
pub(crate) fn set_user_info(
    env: &mut impl SupportEnv,
    user_id: UserId,
    message: Option<td_api::FormattedText>,
    mut promise: Promise<td_api::UserSupportInfo>,
) {
    let formatted_text = match message_entity::get_formatted_text_of_input::<()>(
        None,
        message_entity::DialogId::User(env.get_my_user_id()),
        message,
        false,
        true,
        true,
        false,
        None,
        env.get_always_parse_markdown(),
        env.get_session_count(),
        None,
        false,
    ) {
        Ok(formatted_text) => formatted_text,
        Err(status) => return promise.set_error(status),
    };
    let input_user = match env.get_input_user(user_id) {
        Ok(input_user) => input_user,
        Err(status) => return promise.set_error(status),
    };
    env.edit_user_info_via_net(
        input_user,
        message_entity::get_input_text_with_entities::<()>(
            None,
            &formatted_text,
            "EditUserInfoQuery",
        ),
        promise,
    );
}

/// `get_support_name` (Support.cpp:138-140) — the `help.getSupportName`
/// query rides the seam.
pub(crate) fn get_support_name(env: &mut impl SupportEnv, promise: Promise<String>) {
    env.get_support_name_via_net(promise);
}

/// The [`crate::support_manager`] seams over the `Td` — the env
/// convention ([`crate::auto_download_settings::AutoDownloadSettingsEnv`]).
pub(crate) trait SupportEnv {
    /// `UserManager::get_input_user` — the query-send validation half
    /// (Support.cpp:46-49/:77-80).
    fn get_input_user(
        &mut self,
        user_id: UserId,
    ) -> Result<tdmtproto::telegram_api::InputUser, Status>;

    /// `DialogManager::get_my_dialog_id`'s user half — the
    /// set_user_info intake context (Support.cpp:134).
    fn get_my_user_id(&self) -> UserId;

    /// `OptionManager::get_option_boolean("always_parse_markdown")` —
    /// the intake's Td-owned flag.
    fn get_always_parse_markdown(&self) -> bool;

    /// `OptionManager::get_option_integer("session_count")` — the
    /// intake's Td-owned flag.
    fn get_session_count(&self) -> i32;

    /// `GetUserInfoQuery::send` (Support.cpp:51) — the
    /// `help.getUserInfo` query over the dispatcher.
    fn get_user_info_via_net(
        &mut self,
        input_user: tdmtproto::telegram_api::InputUser,
        promise: Promise<td_api::UserSupportInfo>,
    );

    /// `EditUserInfoQuery::send` (Support.cpp:82-84) — the
    /// `help.editUserInfo` query over the dispatcher.
    fn edit_user_info_via_net(
        &mut self,
        input_user: tdmtproto::telegram_api::InputUser,
        message: tdmtproto::telegram_api::TextWithEntities,
        promise: Promise<td_api::UserSupportInfo>,
    );

    /// `GetSupportNameQuery::send` (Support.cpp:109) — the
    /// `help.getSupportName` query over the dispatcher.
    fn get_support_name_via_net(&mut self, promise: Promise<String>);
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::{Arc, Mutex};

    use tdutils::promise::PromiseCreator;

    /// A `Promise<td_api::UserSupportInfo>` capturing its `Result` into
    /// a shared slot.
    #[allow(clippy::type_complexity)] // the promise + its capture slot, test-local
    fn support_info_promise() -> (
        Promise<td_api::UserSupportInfo>,
        Arc<Mutex<Option<tdutils::status::Result<td_api::UserSupportInfo>>>>,
    ) {
        let result = Arc::new(Mutex::new(None));
        let result_clone = result.clone();
        (
            PromiseCreator::lambda_result(move |r| *result_clone.lock().unwrap() = Some(r)),
            result,
        )
    }

    /// The env test double: `get_input_user` accepts only user 5 (the
    /// C++ wire shape rides through), the option reads are fixed, and
    /// the three seams record their payloads.
    struct TestEnv {
        users: Vec<i64>,
        always_parse_markdown: bool,
        user_info_queries: Vec<tdmtproto::telegram_api::InputUser>,
        edit_user_info_queries: Vec<(
            tdmtproto::telegram_api::InputUser,
            tdmtproto::telegram_api::TextWithEntities,
        )>,
        support_name_queries: usize,
    }

    impl Default for TestEnv {
        fn default() -> Self {
            TestEnv {
                users: vec![5],
                always_parse_markdown: false,
                user_info_queries: Vec::new(),
                edit_user_info_queries: Vec::new(),
                support_name_queries: 0,
            }
        }
    }

    impl SupportEnv for TestEnv {
        fn get_input_user(
            &mut self,
            user_id: UserId,
        ) -> Result<tdmtproto::telegram_api::InputUser, Status> {
            if self.users.contains(&user_id.get()) {
                Ok(tdmtproto::telegram_api::InputUser::inputUser(
                    tdmtproto::telegram_api::inputUser {
                        user_id: user_id.get(),
                        access_hash: 0,
                    },
                ))
            } else {
                Err(Status::error(400, "User not found"))
            }
        }

        fn get_my_user_id(&self) -> UserId {
            UserId::new(777000)
        }

        fn get_always_parse_markdown(&self) -> bool {
            self.always_parse_markdown
        }

        fn get_session_count(&self) -> i32 {
            1
        }

        fn get_user_info_via_net(
            &mut self,
            input_user: tdmtproto::telegram_api::InputUser,
            _promise: Promise<td_api::UserSupportInfo>,
        ) {
            self.user_info_queries.push(input_user);
        }

        fn edit_user_info_via_net(
            &mut self,
            input_user: tdmtproto::telegram_api::InputUser,
            message: tdmtproto::telegram_api::TextWithEntities,
            _promise: Promise<td_api::UserSupportInfo>,
        ) {
            self.edit_user_info_queries.push((input_user, message));
        }

        fn get_support_name_via_net(&mut self, _promise: Promise<String>) {
            self.support_name_queries += 1;
        }
    }

    /// The result rewrap: the userInfo variant parses the message and
    /// carries the author/date; the Empty variant keeps the defaults.
    #[test]
    fn get_user_support_info_object_rewraps_both_variants() {
        let full = get_user_support_info_object::<()>(
            None,
            tdmtproto::telegram_api::help_UserInfo::help_userInfo(
                tdmtproto::telegram_api::help_userInfo {
                    message: "Support rocks".to_string(),
                    entities: Vec::new(),
                    author: "Durov".to_string(),
                    date: 1700000000,
                },
            ),
        );
        let full_message = full.message.expect("the C++ always sets message_");
        assert_eq!(full_message.text, "Support rocks");
        assert!(full_message.entities.is_empty());
        assert_eq!(full.author, "Durov");
        assert_eq!(full.date, 1700000000);

        let empty = get_user_support_info_object::<()>(
            None,
            tdmtproto::telegram_api::help_UserInfo::help_userInfoEmpty(
                tdmtproto::telegram_api::help_userInfoEmpty {},
            ),
        );
        let empty_message = empty.message.expect("the C++ always sets message_");
        assert_eq!(empty_message.text, "");
        assert!(empty_message.entities.is_empty());
        assert_eq!(empty.author, "");
        assert_eq!(empty.date, 0);
    }

    /// `get_user_info` parks at the seam for a known user and reports
    /// the send-half validation error for an unknown one.
    #[test]
    fn get_user_info_runs_the_send_half_validation() {
        let mut env = TestEnv::default();

        let (promise, result) = support_info_promise();
        get_user_info(&mut env, UserId::new(99), promise);
        assert!(env.user_info_queries.is_empty());
        assert!(
            matches!(*result.lock().unwrap(), Some(tdutils::status::Result::Err(ref status)) if status.code() == 400)
        );

        let (promise, result) = support_info_promise();
        get_user_info(&mut env, UserId::new(5), promise);
        assert_eq!(env.user_info_queries.len(), 1);
        assert!(matches!(
            &env.user_info_queries[0],
            tdmtproto::telegram_api::InputUser::inputUser(user) if user.user_id == 5
        ));
        // the seam drops the promise — the dropped LambdaPromise parks the
        // captured slot with the "Lost promise" error (the recorded
        // parked-arm convention until the net step answers)
        assert!(
            matches!(
                *result.lock().unwrap(),
                Some(tdutils::status::Result::Err(_))
            ),
            "expected the parked promise"
        );
    }

    /// `set_user_info` runs the intake first, then the send-half
    /// validation, and builds the wire message at the seam.
    #[test]
    fn set_user_info_orders_the_bands_and_parks_the_query() {
        let message = || {
            Some(td_api::FormattedText {
                text: "Fix it".to_string(),
                entities: Vec::new(),
            })
        };

        // the send-half validation fires after the (passing) intake
        let mut env = TestEnv::default();
        let (promise, result) = support_info_promise();
        set_user_info(&mut env, UserId::new(99), message(), promise);
        assert!(env.edit_user_info_queries.is_empty());
        assert!(
            matches!(*result.lock().unwrap(), Some(tdutils::status::Result::Err(ref status)) if status.code() == 400)
        );

        // the parked query carries the wire-shaped message
        let mut env = TestEnv::default();
        let (promise, result) = support_info_promise();
        set_user_info(&mut env, UserId::new(5), message(), promise);
        assert_eq!(env.edit_user_info_queries.len(), 1);
        let (input_user, wire) = &env.edit_user_info_queries[0];
        assert!(matches!(
            input_user,
            tdmtproto::telegram_api::InputUser::inputUser(user) if user.user_id == 5
        ));
        assert_eq!(wire.text, "Fix it");
        assert!(wire.entities.is_empty());
        // the seam drops the promise — the dropped LambdaPromise parks the
        // captured slot with the "Lost promise" error (the recorded
        // parked-arm convention until the net step answers)
        assert!(
            matches!(
                *result.lock().unwrap(),
                Some(tdutils::status::Result::Err(_))
            ),
            "expected the parked promise"
        );
    }

    /// The my-dialog context and the always_parse_markdown option ride
    /// the intake: the markdown v3 doubled markers parse into a bold
    /// entity and the entities build into the wire message (single
    /// `*bold*` pairs stay literal in v3 — the v1-only shape).
    #[test]
    fn set_user_info_threads_the_intake_context() {
        let message = Some(td_api::FormattedText {
            text: "**Fix it**".to_string(),
            entities: Vec::new(),
        });

        let mut env = TestEnv {
            always_parse_markdown: true,
            ..TestEnv::default()
        };
        let (promise, _result) = support_info_promise();
        set_user_info(&mut env, UserId::new(5), message, promise);
        assert_eq!(env.edit_user_info_queries.len(), 1);
        let wire = &env.edit_user_info_queries[0].1;
        assert_eq!(wire.text, "Fix it");
        assert!(!wire.entities.is_empty());
    }

    /// `get_support_name` parks its query at the seam.
    #[test]
    fn get_support_name_parks_the_query() {
        let mut env = TestEnv::default();
        get_support_name(&mut env, Promise::default());
        assert_eq!(env.support_name_queries, 1);
    }
}
