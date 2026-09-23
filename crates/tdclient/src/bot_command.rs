//! Port of `td/telegram/BotCommand.{h,cpp}` (Wave 3, M.45 — Bots I): the
//! bot command value plane (`td::BotCommand`/`td::BotCommands` with the
//! byte-exact pmc store/parse) and the `setCommands`/`deleteCommands`/
//! `getCommands` request flows.
//!
//! Seams: the three flows end in the `Td::ResultHandler` sends
//! ([`BotInfoManagerEnv`] query seams, [blocked: net]) — the promises park
//! in [`crate::bot_info_manager::BotInfoManager`], whose
//! `get_bot_commands` answer half
//! (`GetBotCommandsQuery::on_result`, BotCommand.cpp:97-105) reduces to
//! [`BotCommands::from_telegram`] + [`BotCommands::get_bot_commands_object`]
//! once the net step lands.
//!
//! Divergences: the C++ `set_commands` loop checks
//! `command == nullptr` per element (BotCommand.cpp:175-177); the Rust
//! `Vec<td_api::BotCommand>` cannot hold nulls, so the branch is
//! unreachable here.

use crate::bot_command_scope::BotCommandScope;
use crate::bot_info_manager::BotInfoManagerEnv;
use crate::store_parse::{self, Parser};
use crate::td_api;
use crate::user_manager::UserId;
use tdmtproto::telegram_api;
use tdutils::promise::Promise;
use tdutils::status::Status;

/// `td::BotCommand` (BotCommand.h:22-59).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BotCommand {
    command: String,
    description: String,
    is_ephemeral: bool,
}

impl BotCommand {
    /// `BotCommand::BotCommand(string, string, bool)`
    /// (BotCommand.h:31-33).
    pub fn new(command: String, description: String, is_ephemeral: bool) -> BotCommand {
        BotCommand {
            command,
            description,
            is_ephemeral,
        }
    }

    /// `BotCommand::BotCommand(object_ptr<telegram_api::botCommand> &&)`
    /// (BotCommand.cpp:112-117).
    pub fn from_telegram(bot_command: telegram_api::BotCommand) -> BotCommand {
        BotCommand {
            command: bot_command.command,
            description: bot_command.description,
            is_ephemeral: bot_command.ephemeral,
        }
    }

    /// `BotCommand::get_bot_command_object` (BotCommand.cpp:119-121).
    pub fn get_bot_command_object(&self) -> td_api::BotCommand {
        td_api::BotCommand {
            command: self.command.clone(),
            description: self.description.clone(),
            is_ephemeral: self.is_ephemeral,
        }
    }

    /// `BotCommand::get_input_bot_command` (BotCommand.cpp:123-125).
    pub fn get_input_bot_command(&self) -> telegram_api::BotCommand {
        telegram_api::BotCommand {
            flags: 0,
            ephemeral: self.is_ephemeral,
            command: self.command.clone(),
            description: self.description.clone(),
        }
    }

    /// `BotCommand::store` (BotCommand.h:40-47).
    pub fn store(&self, out: &mut Vec<u8>) {
        store_parse::store_flags(out, [self.is_ephemeral]);
        store_parse::store_string(out, &self.command);
        store_parse::store_string(out, &self.description);
    }

    /// `BotCommand::parse` (BotCommand.h:49-58): the ephemeral flag is
    /// version-gated (`Version::SupportEphemeralMessages`).
    pub fn parse(parser: &mut Parser) -> Result<BotCommand, &'static str> {
        let mut result = BotCommand::default();
        if parser.version() >= store_parse::version::SUPPORT_EPHEMERAL_MESSAGES {
            let [is_ephemeral] = parser.parse_flags()?;
            result.is_ephemeral = is_ephemeral;
        }
        result.command = parser.string()?;
        result.description = parser.string()?;
        Result::Ok(result)
    }
}

/// `td::BotCommands` (BotCommand.h:67-103).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BotCommands {
    bot_user_id: UserId,
    commands: Vec<BotCommand>,
}

impl BotCommands {
    /// `BotCommands::BotCommands(UserId, vector<object_ptr<botCommand>> &&)`
    /// (BotCommand.cpp:131-136).
    pub fn from_telegram(
        bot_user_id: UserId,
        bot_commands: Vec<telegram_api::BotCommand>,
    ) -> BotCommands {
        BotCommands {
            bot_user_id,
            commands: bot_commands
                .into_iter()
                .map(BotCommand::from_telegram)
                .collect(),
        }
    }

    /// `BotCommands::get_bot_commands_object` (BotCommand.cpp:138-142): the
    /// C++ resolves the bot user id through the manager; the Rust takes the
    /// env for the same `get_user_id_object(bot_user_id_,
    /// "get_bot_commands_object")` call.
    pub fn get_bot_commands_object(&self, env: &mut impl BotInfoManagerEnv) -> td_api::BotCommands {
        td_api::BotCommands {
            bot_user_id: env.get_user_id_object(self.bot_user_id, "get_bot_commands_object"),
            commands: self
                .commands
                .iter()
                .map(|command| command.get_bot_command_object())
                .collect(),
        }
    }

    /// `BotCommands::update_all_bot_commands`
    /// (BotCommand.cpp:144-162): `true` when the stored list changed.
    pub fn update_all_bot_commands(
        all_bot_commands: &mut Vec<BotCommands>,
        bot_commands: BotCommands,
    ) -> bool {
        let bot_user_id = bot_commands.bot_user_id;
        if bot_commands.commands.is_empty() {
            // td::remove_if over the from-bot entries
            let old_len = all_bot_commands.len();
            all_bot_commands.retain(|commands| commands.bot_user_id != bot_user_id);
            return all_bot_commands.len() != old_len;
        }
        if let Some(existing) = all_bot_commands
            .iter_mut()
            .find(|commands| commands.bot_user_id == bot_user_id)
        {
            if *existing != bot_commands {
                *existing = bot_commands;
                return true;
            }
            return false;
        }
        all_bot_commands.push(bot_commands);
        true
    }

    /// `BotCommands::get_bot_user_id` (BotCommand.h:82-84).
    pub fn get_bot_user_id(&self) -> UserId {
        self.bot_user_id
    }

    /// `BotCommands::store` (BotCommand.h:86-90).
    pub fn store(&self, out: &mut Vec<u8>) {
        store_parse::store_i64(out, self.bot_user_id.get());
        store_parse::store_i32(out, self.commands.len() as i32);
        for command in &self.commands {
            command.store(out);
        }
    }

    /// `BotCommands::parse` (BotCommand.h:92-96).
    pub fn parse(parser: &mut Parser) -> Result<BotCommands, &'static str> {
        let mut result = BotCommands {
            bot_user_id: UserId::new(parser.i64_()?),
            ..Default::default()
        };
        let size = parser.i32_()?;
        for _ in 0..size {
            result.commands.push(BotCommand::parse(parser)?);
        }
        Result::Ok(result)
    }
}

/// `set_commands` (BotCommand.cpp:168-212): the validation and
/// normalization of the command list, then the `bots.setBotCommands` send
/// seam with the promise parked in the manager.
pub fn set_commands(
    manager: &mut crate::bot_info_manager::BotInfoManager,
    env: &mut impl BotInfoManagerEnv,
    scope_ptr: Option<td_api::BotCommandScope>,
    language_code: String,
    commands: Vec<td_api::BotCommand>,
    mut promise: Promise<()>,
) {
    let scope = match BotCommandScope::get_bot_command_scope(env, scope_ptr) {
        tdutils::status::Result::Ok(scope) => scope,
        tdutils::status::Result::Err(status) => {
            promise.set_error(status);
            return;
        }
    };
    if let Err(status) = crate::misc::validate_bot_language_code(&language_code) {
        promise.set_error(status);
        return;
    }

    let mut new_commands = Vec::with_capacity(commands.len());
    for mut command in commands {
        // the C++ null-command 400 is unreachable over a Rust Vec
        if crate::misc::clean_input_string(&command.command).is_none() {
            promise.set_error(Status::error(400, "Command must be encoded in UTF-8"));
            return;
        }
        if crate::misc::clean_input_string(&command.description).is_none() {
            promise.set_error(Status::error(
                400,
                "Command description must be encoded in UTF-8",
            ));
            return;
        }

        const MAX_COMMAND_TEXT_LENGTH: usize = 32;
        command.command = crate::misc::trim(&command.command).to_owned();
        if let Some(stripped) = command.command.strip_prefix('/') {
            command.command = stripped.to_owned();
        }
        if command.command.is_empty() {
            promise.set_error(Status::error(400, "Command must be non-empty"));
            return;
        }
        if tdutils::utf8::utf8_length(&command.command) > MAX_COMMAND_TEXT_LENGTH {
            promise.set_error(Status::error(
                400,
                &format!("Command length must not exceed {MAX_COMMAND_TEXT_LENGTH}"),
            ));
            return;
        }

        const MAX_COMMAND_DESCRIPTION_LENGTH: usize = 256;
        command.description = crate::misc::trim(&command.description).to_owned();
        let description_length = tdutils::utf8::utf8_length(&command.description);
        if command.description.is_empty() {
            promise.set_error(Status::error(400, "Command description must be non-empty"));
            return;
        }
        if description_length > MAX_COMMAND_DESCRIPTION_LENGTH {
            promise.set_error(Status::error(
                400,
                &format!(
                    "Command description length must not exceed {MAX_COMMAND_DESCRIPTION_LENGTH}"
                ),
            ));
            return;
        }

        new_commands.push(BotCommand::new(
            command.command,
            command.description,
            command.is_ephemeral,
        ));
    }

    manager.park_set_bot_commands_query(promise);
    let input_commands: Vec<telegram_api::BotCommand> = new_commands
        .iter()
        .map(|command| command.get_input_bot_command())
        .collect();
    let input_scope = scope.get_input_bot_command_scope(env);
    env.send_set_bot_commands_query(input_scope, &language_code, input_commands);
}

/// `delete_commands` (BotCommand.cpp:214-220).
pub fn delete_commands(
    manager: &mut crate::bot_info_manager::BotInfoManager,
    env: &mut impl BotInfoManagerEnv,
    scope_ptr: Option<td_api::BotCommandScope>,
    language_code: String,
    mut promise: Promise<()>,
) {
    let scope = match BotCommandScope::get_bot_command_scope(env, scope_ptr) {
        tdutils::status::Result::Ok(scope) => scope,
        tdutils::status::Result::Err(status) => {
            promise.set_error(status);
            return;
        }
    };
    if let Err(status) = crate::misc::validate_bot_language_code(&language_code) {
        promise.set_error(status);
        return;
    }

    manager.park_reset_bot_commands_query(promise);
    let input_scope = scope.get_input_bot_command_scope(env);
    env.send_reset_bot_commands_query(input_scope, &language_code);
}

/// `get_commands` (BotCommand.cpp:222-228).
pub fn get_commands(
    manager: &mut crate::bot_info_manager::BotInfoManager,
    env: &mut impl BotInfoManagerEnv,
    scope_ptr: Option<td_api::BotCommandScope>,
    language_code: String,
    mut promise: Promise<td_api::BotCommands>,
) {
    let scope = match BotCommandScope::get_bot_command_scope(env, scope_ptr) {
        tdutils::status::Result::Ok(scope) => scope,
        tdutils::status::Result::Err(status) => {
            promise.set_error(status);
            return;
        }
    };
    if let Err(status) = crate::misc::validate_bot_language_code(&language_code) {
        promise.set_error(status);
        return;
    }

    manager.park_get_bot_commands_query(promise);
    let input_scope = scope.get_input_bot_command_scope(env);
    env.send_get_bot_commands_query(input_scope, &language_code);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot_info_manager::BotInfoManager;
    use crate::bot_info_manager::test_env::{self, TestBotEnv};
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

    fn bot_env<'a>(
        global: &'a mut Global,
        options: &'a mut OptionManager,
        user_manager: &'a mut UserManager,
    ) -> TestBotEnv<'a> {
        test_env::fixture(global, Some(options), user_manager, true, 1)
    }

    #[test]
    fn bot_command_store_parse_round_trip() {
        // the ephemeral flag rides the version gate (BotCommand.h:49-58)
        let command = BotCommand::new("/start@bot".to_owned(), "Start".to_owned(), true);
        let mut out = Vec::new();
        store_parse::store_log_event_version(&mut out);
        command.store(&mut out);
        let mut parser = Parser::new_log_event(&out).expect("framed");
        assert_eq!(BotCommand::parse(&mut parser).expect("parse"), command);

        // a legacy (pre-SupportEphemeralMessages) row was written under an
        // old version prefix and without the flag word: command +
        // description only, parsed as false
        let mut out = Vec::new();
        store_parse::store_i32(
            &mut out,
            store_parse::version::SUPPORT_EPHEMERAL_MESSAGES - 1,
        );
        store_parse::store_string(&mut out, "start");
        store_parse::store_string(&mut out, "Start");
        let mut parser = Parser::new_log_event(&out).expect("framed");
        let parsed = BotCommand::parse(&mut parser).expect("parse");
        assert_eq!(
            parsed,
            BotCommand::new("start".to_owned(), "Start".to_owned(), false)
        );
    }

    #[test]
    fn bot_commands_store_parse_round_trip() {
        let commands = BotCommands::from_telegram(
            UserId::new(7),
            vec![
                telegram_api::BotCommand {
                    flags: 0,
                    ephemeral: false,
                    command: "start".to_owned(),
                    description: "Start".to_owned(),
                },
                telegram_api::BotCommand {
                    flags: 0,
                    ephemeral: true,
                    command: "x".to_owned(),
                    description: "Y".to_owned(),
                },
            ],
        );
        let mut out = Vec::new();
        store_parse::store_log_event_version(&mut out);
        commands.store(&mut out);
        let mut parser = Parser::new_log_event(&out).expect("framed");
        let parsed = BotCommands::parse(&mut parser).expect("parse");
        assert_eq!(parsed, commands);
        assert_eq!(parsed.get_bot_user_id(), UserId::new(7));
    }

    #[test]
    fn update_all_bot_commands_matches_the_cpp() {
        let first = BotCommands::from_telegram(
            UserId::new(1),
            vec![telegram_api::BotCommand {
                flags: 0,
                ephemeral: false,
                command: "a".to_owned(),
                description: "a".to_owned(),
            }],
        );
        let same = first.clone();
        let changed = BotCommands::from_telegram(
            UserId::new(1),
            vec![telegram_api::BotCommand {
                flags: 0,
                ephemeral: false,
                command: "b".to_owned(),
                description: "b".to_owned(),
            }],
        );
        let other = BotCommands::from_telegram(UserId::new(2), vec![]);

        let mut all = vec![first.clone(), other.clone()];
        // an equal update is a no-op
        assert!(!BotCommands::update_all_bot_commands(&mut all, same));
        assert_eq!(all.len(), 2);
        // a changed update replaces
        assert!(BotCommands::update_all_bot_commands(&mut all, changed));
        assert_eq!(all[0].get_bot_user_id(), UserId::new(1));
        // an empty update removes the bot
        assert!(BotCommands::update_all_bot_commands(
            &mut all,
            BotCommands::from_telegram(UserId::new(1), vec![])
        ));
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].get_bot_user_id(), UserId::new(2));
        // removing an absent bot is a no-op
        assert!(!BotCommands::update_all_bot_commands(
            &mut all,
            BotCommands::from_telegram(UserId::new(1), vec![])
        ));
        // an absent bot appends (the empty-command branch is the removal
        // half, so the appended update carries a command)
        let added = BotCommands::from_telegram(
            UserId::new(3),
            vec![telegram_api::BotCommand {
                flags: 0,
                ephemeral: false,
                command: "c".to_owned(),
                description: "c".to_owned(),
            }],
        );
        assert!(BotCommands::update_all_bot_commands(&mut all, added));
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn get_input_bot_command_preserves_the_fields() {
        let command = BotCommand::new("cmd".to_owned(), "desc".to_owned(), true);
        let input = command.get_input_bot_command();
        assert!(input.ephemeral);
        assert_eq!(input.command, "cmd");
        assert_eq!(input.description, "desc");
        assert_eq!(
            command.get_bot_command_object(),
            td_api::BotCommand {
                command: "cmd".to_owned(),
                description: "desc".to_owned(),
                is_ephemeral: true,
            }
        );
    }

    #[test]
    fn set_commands_validates_and_parks() {
        let mut global = Global::new();
        let mut options = OptionManager::new(false);
        let mut user_manager = UserManager::new();
        let mut env = bot_env(&mut global, &mut options, &mut user_manager);
        let mut manager = BotInfoManager::new();

        // the bad language code fails after the scope conversion
        let (promise, recorded) = recording_promise();
        set_commands(
            &mut manager,
            &mut env,
            None,
            "e!".to_owned(),
            vec![],
            promise,
        );
        let recorded = recorded.0.lock().unwrap().clone();
        assert_eq!(
            recorded.unwrap().unwrap_err().message(),
            "Invalid language code specified"
        );

        // the empty command after trim fails
        let (promise, recorded) = recording_promise();
        set_commands(
            &mut manager,
            &mut env,
            None,
            "en".to_owned(),
            vec![td_api::BotCommand {
                command: "  /  ".to_owned(),
                description: "d".to_owned(),
                is_ephemeral: false,
            }],
            promise,
        );
        let recorded = recorded.0.lock().unwrap().clone();
        assert_eq!(
            recorded.unwrap().unwrap_err().message(),
            "Command must be non-empty"
        );

        // the too-long command fails
        let (promise, recorded) = recording_promise();
        set_commands(
            &mut manager,
            &mut env,
            None,
            "en".to_owned(),
            vec![td_api::BotCommand {
                command: "a".repeat(33),
                description: "d".to_owned(),
                is_ephemeral: false,
            }],
            promise,
        );
        let recorded = recorded.0.lock().unwrap().clone();
        assert_eq!(
            recorded.unwrap().unwrap_err().message(),
            "Command length must not exceed 32"
        );

        // the empty description fails
        let (promise, recorded) = recording_promise();
        set_commands(
            &mut manager,
            &mut env,
            None,
            "en".to_owned(),
            vec![td_api::BotCommand {
                command: "cmd".to_owned(),
                description: String::new(),
                is_ephemeral: false,
            }],
            promise,
        );
        let recorded = recorded.0.lock().unwrap().clone();
        assert_eq!(
            recorded.unwrap().unwrap_err().message(),
            "Command description must be non-empty"
        );

        // the valid command normalizes (the leading slash and the trim) and
        // parks
        let (promise, recorded) = recording_promise();
        set_commands(
            &mut manager,
            &mut env,
            None,
            "en".to_owned(),
            vec![td_api::BotCommand {
                command: "  /cmd  ".to_owned(),
                description: "  d  ".to_owned(),
                is_ephemeral: true,
            }],
            promise,
        );
        assert!(recorded.0.lock().unwrap().is_none(), "the promise parks");
        assert_eq!(
            env.queries,
            vec!["Send bots.setBotCommands — [blocked: net]"]
        );
    }

    #[test]
    fn delete_and_get_commands_validate_and_park() {
        let mut global = Global::new();
        let mut options = OptionManager::new(false);
        let mut user_manager = UserManager::new();
        let mut env = bot_env(&mut global, &mut options, &mut user_manager);
        let mut manager = BotInfoManager::new();

        let (promise, recorded) = recording_promise();
        delete_commands(&mut manager, &mut env, None, "e!".to_owned(), promise);
        let recorded = recorded.0.lock().unwrap().clone();
        assert_eq!(
            recorded.unwrap().unwrap_err().message(),
            "Invalid language code specified"
        );

        let (promise, recorded) = recording_promise();
        delete_commands(&mut manager, &mut env, None, "en".to_owned(), promise);
        assert!(recorded.0.lock().unwrap().is_none());
        assert_eq!(
            env.queries,
            vec!["Send bots.resetBotCommands — [blocked: net]"]
        );

        let (promise, recorded) = recording_promise();
        get_commands(&mut manager, &mut env, None, "en".to_owned(), promise);
        assert!(recorded.0.lock().unwrap().is_none());
        assert_eq!(
            env.queries.last().unwrap(),
            "Send bots.getBotCommands — [blocked: net]"
        );
    }

    #[test]
    fn bot_commands_object_builds_the_td_api() {
        let mut global = Global::new();
        let mut options = OptionManager::new(false);
        let mut user_manager = UserManager::new();
        let mut env = bot_env(&mut global, &mut options, &mut user_manager);

        let commands = BotCommands::from_telegram(
            UserId::new(1),
            vec![telegram_api::BotCommand {
                flags: 0,
                ephemeral: true,
                command: "start".to_owned(),
                description: "Start".to_owned(),
            }],
        );
        let object = commands.get_bot_commands_object(&mut env);
        assert_eq!(object.bot_user_id, 1);
        assert_eq!(object.commands.len(), 1);
        assert!(object.commands[0].is_ephemeral);
    }
}
